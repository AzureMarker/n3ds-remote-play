use ctru::prelude::{Apt, Gfx, Hid, KeyPad};
use ctru::services::ir_user::{CirclePadProInputResponse, ConnectionStatus, IrDeviceId, IrUser};
use ctru::services::svc::HandleExt;
use n3ds_remote_play_common::{CStick, CirclePad, InputState};
use std::time::Duration;

const CPP_CONNECTION_POLLING_PERIOD_MS: u8 = 0x08;
const CPP_POLLING_PERIOD_MS: u8 = 0x32;

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
    pub fn send_inputs_to_server(&mut self) -> Result<(), &'static str> {
        self.hid.scan_input();
        let mut keys_down_or_held = self.hid.keys_down().union(self.hid.keys_held());

        if keys_down_or_held.contains(KeyPad::START) && keys_down_or_held.contains(KeyPad::SELECT) {
            log::info!("Start + Select pressed, exiting");
            return Err("Exit requested");
        }

        self.scan_cpp_input();

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
            return Err("Input sender channel closed");
        }

        Ok(())
    }

    /// Check for a new Circle Pad Pro input packet and store in `self.last_cpp_input` if found.
    fn scan_cpp_input(&mut self) {
        // Check if we've received a packet from the circle pad pro
        let packet_received = self
            .receive_packet_event
            .wait_for_event(Duration::ZERO)
            .is_ok();
        if !packet_received {
            return;
        }

        let packets = self.ir_user.get_packets().expect("Failed to get packets");
        let packet_count = packets.len();
        let Some(packet) = packets.last() else {
            panic!("No packets found")
        };
        let cpp_input = CirclePadProInputResponse::try_from(packet)
            .expect("Failed to parse CPP response from IR packet");
        self.last_cpp_input = cpp_input;

        // Done handling the packets, release them
        self.ir_user
            .release_received_data(packet_count as u32)
            .expect("Failed to release ir:USER packets");

        // Remind the CPP that we're still listening
        self.ir_user
            .request_input_polling(CPP_POLLING_PERIOD_MS)
            .expect("Failed to request input polling from CPP");
    }

    /// Detect and enable 3DS Circle Pad Pro (or continue without it if user skips)
    pub fn connect_circle_pad_pro(&mut self, apt: &Apt, gfx: &Gfx) {
        // Get event handles
        let connection_status_event = self
            .ir_user
            .get_connection_status_event()
            .expect("Couldn't get ir:USER connection status event");
        let receive_packet_event = self
            .ir_user
            .get_recv_event()
            .expect("Couldn't get ir:USER recv event");

        log::info!(
            "If you have a New 3DS or Circle Pad Pro, press A to connect extra inputs. Otherwise press B."
        );
        'cpp_connect_loop: while apt.main_loop() {
            self.hid.scan_input();
            let keys_down_or_held = self.hid.keys_down().union(self.hid.keys_held());

            if keys_down_or_held.contains(KeyPad::B) {
                log::info!("Canceling New 3DS / Circle Pad Pro detection");
                break;
            }

            if keys_down_or_held.contains(KeyPad::A) {
                log::info!("Connecting, press Start to cancel.");

                // Connection loop
                loop {
                    self.hid.scan_input();
                    if self.hid.keys_held().contains(KeyPad::START) {
                        break 'cpp_connect_loop;
                    }

                    // Start the connection process
                    self.ir_user
                        .require_connection(IrDeviceId::CirclePadPro)
                        .expect("Couldn't initialize circle pad pro connection");

                    // Wait for the connection to establish
                    if let Err(e) =
                        connection_status_event.wait_for_event(Duration::from_millis(100))
                        && !e.is_timeout()
                    {
                        panic!("Couldn't initialize circle pad pro connection: {e}");
                    }

                    if self.ir_user.get_status_info().connection_status
                        == ConnectionStatus::Connected
                    {
                        log::info!("Connected!");
                        break;
                    }

                    // If not connected (ex. timeout), disconnect so we can retry
                    self.ir_user
                        .disconnect()
                        .expect("Failed to disconnect circle pad pro connection");

                    // Wait for the disconnect to go through
                    if let Err(e) =
                        connection_status_event.wait_for_event(Duration::from_millis(100))
                        && !e.is_timeout()
                    {
                        panic!("Couldn't initialize circle pad pro connection: {e}");
                    }
                }

                // Sending first packet retry loop
                loop {
                    self.hid.scan_input();
                    if self.hid.keys_held().contains(KeyPad::START) {
                        break 'cpp_connect_loop;
                    }

                    // Send a request for input to the CPP
                    if let Err(e) = self
                        .ir_user
                        .request_input_polling(CPP_CONNECTION_POLLING_PERIOD_MS)
                    {
                        log::error!("{e:?}");
                    }

                    // Wait for the response
                    let recv_event_result =
                        receive_packet_event.wait_for_event(Duration::from_millis(100));

                    if recv_event_result.is_ok() {
                        log::info!("Got first packet from CPP");
                        let packets = self.ir_user.get_packets().expect("Failed to get packets");
                        let packet_count = packets.len();
                        let Some(packet) = packets.last() else {
                            panic!("No packets found")
                        };
                        let cpp_input = CirclePadProInputResponse::try_from(packet)
                            .expect("Failed to parse CPP response from IR packet");
                        self.last_cpp_input = cpp_input;

                        // Done handling the packets, release them
                        self.ir_user
                            .release_received_data(packet_count as u32)
                            .expect("Failed to release ir:USER packets");

                        // Remind the CPP that we're still listening
                        self.ir_user
                            .request_input_polling(CPP_POLLING_PERIOD_MS)
                            .expect("Failed to request input polling from CPP");
                        break;
                    }

                    // We didn't get a response in time, so loop and retry
                }

                // Finished connecting
                break;
            }

            gfx.wait_for_vblank();
        }
    }
}
