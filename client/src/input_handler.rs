use anyhow::Context;
use ctru::prelude::{Apt, Gfx, Hid, KeyPad};
use ctru::services::ir_user::{CirclePadProInputResponse, ConnectionStatus, IrDeviceId, IrUser};
use ctru::services::svc::HandleExt;
use n3ds_remote_play_common::{CStick, CirclePad, InputState};
use std::time::Duration;

const CPP_CONNECTION_POLLING_PERIOD_MS: u8 = 8;
const CPP_POLLING_PERIOD_MS: u8 = 50;

/// Handles user input from the 3DS and sends it to the system core thread to be
/// forwarded to the server.
///
/// The input handle also sets up the Circle Pad Pro connection and polls it for input.
pub struct InputHandler {
    hid: Hid,
    ir_user: IrUser,
    receive_packet_event: ctru_sys::Handle,
    last_cpp_input: CirclePadProInputResponse,
    input_sender: tokio::sync::watch::Sender<InputState>,
}

impl InputHandler {
    pub fn new(
        hid: Hid,
        ir_user: IrUser,
        input_sender: tokio::sync::watch::Sender<InputState>,
    ) -> Self {
        let receive_packet_event = ir_user
            .get_recv_event()
            .expect("Couldn't get ir:USER recv event");

        Self {
            hid,
            ir_user,
            receive_packet_event,
            last_cpp_input: CirclePadProInputResponse::default(),
            input_sender,
        }
    }

    /// Scan inputs and send them to the system thread, which will forward them to the server.
    /// If the user decides to close the app, an Err will be returned to signal the main loop to exit.
    /// Other errors are propagated as well.
    pub fn send_inputs_to_server(&mut self) -> anyhow::Result<()> {
        self.hid.scan_input();
        let mut keys_down_or_held = self.hid.keys_down().union(self.hid.keys_held());

        if keys_down_or_held.contains(KeyPad::START) && keys_down_or_held.contains(KeyPad::SELECT) {
            log::info!("Start + Select pressed, exiting");
            anyhow::bail!("Exit requested");
        }

        self.scan_cpp_input()?;

        // Add in the buttons from CPP
        if self.last_cpp_input.r_pressed {
            keys_down_or_held |= KeyPad::R;
        }
        if self.last_cpp_input.zl_pressed {
            keys_down_or_held |= KeyPad::ZL;
        }
        if self.last_cpp_input.zr_pressed {
            keys_down_or_held |= KeyPad::ZR;
        }

        let circle_pad_pos = self.hid.circlepad_position();
        let input_state = InputState {
            key_pad: n3ds_remote_play_common::KeyPad::from_bits_truncate(keys_down_or_held.bits()),
            circle_pad: CirclePad {
                x: circle_pad_pos.0,
                y: circle_pad_pos.1,
            },
            c_stick: CStick {
                x: self.last_cpp_input.c_stick_x,
                y: self.last_cpp_input.c_stick_y,
            },
        };

        // Send input state to system thread
        if self.input_sender.send(input_state).is_err() {
            log::error!("Input sender channel closed");
            anyhow::bail!("Input sender channel closed");
        }

        Ok(())
    }

    /// Check for a new Circle Pad Pro input packet and store in `self.last_cpp_input` if found.
    fn scan_cpp_input(&mut self) -> anyhow::Result<()> {
        // Check if we've received a packet from the circle pad pro
        let packet_received = self
            .receive_packet_event
            .wait_for_event(Duration::ZERO)
            .is_ok();
        if !packet_received {
            return Ok(());
        }

        self.scan_cpp_input_unchecked()
    }

    /// A version of [`Self::scan_cpp_input`] that doesn't check if a packet has been received
    /// before trying to read it.
    fn scan_cpp_input_unchecked(&mut self) -> anyhow::Result<()> {
        let packets = self
            .ir_user
            .get_packets()
            .map_err(|e| anyhow::anyhow!("Failed to get packets: {e}"))?;
        let packet_count = packets.len();
        let Some(packet) = packets.last() else {
            anyhow::bail!("No packets found")
        };
        let cpp_input = CirclePadProInputResponse::try_from(packet)
            .map_err(|e| anyhow::anyhow!("Failed to parse CPP response from IR packet: {e}"))?;
        self.last_cpp_input = cpp_input;

        // Done handling the packets, release them
        self.ir_user
            .release_received_data(packet_count as u32)
            .context("Failed to release ir:USER packets")?;

        // Remind the CPP that we're still listening
        self.ir_user
            .request_input_polling(CPP_POLLING_PERIOD_MS)
            .context("Failed to request input polling from CPP")?;

        Ok(())
    }

    /// Detect and enable 3DS Circle Pad Pro (or continue without it if user skips)
    /// This function will prompt the user to connect the extra inputs, so it will block until
    /// the user responds.
    pub fn connect_circle_pad_pro(&mut self, apt: &Apt, gfx: &Gfx) -> anyhow::Result<()> {
        // Get event handles
        let connection_status_event = self
            .ir_user
            .get_connection_status_event()
            .context("Couldn't get ir:USER connection status event")?;
        let receive_packet_event = self
            .ir_user
            .get_recv_event()
            .context("Couldn't get ir:USER recv event")?;

        log::info!(
            "If you have a New 3DS or Circle Pad Pro, press A to connect extra inputs. Otherwise press B."
        );
        while apt.main_loop() {
            self.hid.scan_input();
            let keys_down_or_held = self.hid.keys_down().union(self.hid.keys_held());

            if keys_down_or_held.contains(KeyPad::B) {
                log::info!("Canceling New 3DS / Circle Pad Pro detection");
                break;
            }

            if keys_down_or_held.contains(KeyPad::A) {
                self.setup_circle_pad_pro(connection_status_event, receive_packet_event)?;

                // Regardless of whether we connected successfully, we can move on now
                break;
            }

            gfx.wait_for_vblank();
        }

        Ok(())
    }

    /// Internal function to set up the Circle Pad Pro connection and input polling.
    fn setup_circle_pad_pro(
        &mut self,
        connection_status_event: ctru_sys::Handle,
        receive_packet_event: ctru_sys::Handle,
    ) -> anyhow::Result<()> {
        log::info!("Connecting, press Start to cancel.");

        // Connection loop
        loop {
            self.hid.scan_input();
            if self.hid.keys_held().contains(KeyPad::START) {
                // User requested to cancel connection
                return Ok(());
            }

            // Start the connection process
            self.ir_user
                .require_connection(IrDeviceId::CirclePadPro)
                .context("Couldn't initialize circle pad pro connection")?;

            // Wait for the connection to establish
            if let Err(e) = connection_status_event.wait_for_event(Duration::from_millis(100))
                && !e.is_timeout()
            {
                anyhow::bail!("Couldn't initialize circle pad pro connection: {e}");
            }

            if self.ir_user.get_status_info().connection_status == ConnectionStatus::Connected {
                log::info!("Connected!");
                break;
            }

            // If not connected (ex. timeout), disconnect so we can retry
            self.ir_user
                .disconnect()
                .context("Failed to disconnect circle pad pro connection")?;

            // Wait for the disconnect to go through
            if let Err(e) = connection_status_event.wait_for_event(Duration::from_millis(100))
                && !e.is_timeout()
            {
                anyhow::bail!("Couldn't initialize circle pad pro connection: {e}");
            }
        }

        // Sending first packet retry loop
        loop {
            self.hid.scan_input();
            if self.hid.keys_held().contains(KeyPad::START) {
                // User requested to cancel connection
                return Ok(());
            }

            // Send a request for input to the CPP
            if let Err(e) = self
                .ir_user
                .request_input_polling(CPP_CONNECTION_POLLING_PERIOD_MS)
            {
                anyhow::bail!("Failed to request input polling from CPP: {e}");
            }

            // Wait for the response
            if let Err(e) = receive_packet_event.wait_for_event(Duration::from_millis(100)) {
                if e.is_timeout() {
                    // No response yet, retry
                    continue;
                } else {
                    anyhow::bail!("Failed to wait for CPP response: {e}");
                }
            }

            // We got a response, parse it and store the input state
            log::info!("Got first packet from CPP");
            self.scan_cpp_input_unchecked()?;
            break;
        }

        Ok(())
    }
}
