use crate::thread::spawn_system_core_thread;
use crate::video_stream::BgrFrameView;
use crate::{system_thread, video_stream};
use ctru::prelude::{Apt, Gfx, Hid, KeyPad};
use ctru::services::gfx::{Flush, Screen, Swap};
use ctru::services::ir_user::{CirclePadProInputResponse, ConnectionStatus, IrDeviceId, IrUser};
use ctru::services::svc::HandleExt;
use n3ds_remote_play_common::{CStick, CirclePad, InputState};
use std::net::{Ipv4Addr, TcpStream, UdpSocket};
use std::os::fd::AsRawFd;
use std::time::{Duration, Instant};
use tokio::sync::mpsc::error::TrySendError;
use tokio_util::bytes::BytesMut;

const CPP_CONNECTION_POLLING_PERIOD_MS: u8 = 0x08;
const CPP_POLLING_PERIOD_MS: u8 = 0x32;

pub struct RemotePlayClient<'gfx> {
    apt: Apt,
    gfx: &'gfx Gfx,
    ir_user: IrUser,
    hid: Hid,
    mpeg_decoder: video_stream::Mpeg1Decoder,
    last_cpp_input: CirclePadProInputResponse,
}

impl<'gfx> RemotePlayClient<'gfx> {
    pub fn new(apt: Apt, gfx: &'gfx Gfx, ir_user: IrUser, hid: Hid) -> Self {
        Self {
            apt,
            gfx,
            ir_user,
            hid,
            mpeg_decoder: video_stream::Mpeg1Decoder::new()
                .expect("Failed to create MPEG1 decoder"),
            last_cpp_input: CirclePadProInputResponse::default(),
        }
    }

    pub fn run(&mut self, server_ip: Ipv4Addr) {
        // Set up connections
        let connection =
            TcpStream::connect((server_ip, 3535)).expect("Failed to connect to server");
        let udp_socket = UdpSocket::bind(connection.local_addr().unwrap())
            .expect("Failed to listen for UDP connections");
        set_udp_recv_buffer_size(&udp_socket, 64 * 1024);
        udp_socket
            .connect((server_ip, 3535))
            .expect("Failed to set up UDP connection to server");
        udp_socket.set_nonblocking(true).unwrap();
        log::info!("Connected to remote play server at {server_ip}.");

        self.connect_circle_pad_pro();

        let receive_packet_event = self
            .ir_user
            .get_recv_event()
            .expect("Couldn't get ir:USER recv event");

        // Set up system core thread to read network data in parallel
        let (input_sender, input_receiver) = tokio::sync::mpsc::channel(1);
        let (packet_sender, packet_receiver) = std::sync::mpsc::sync_channel(2);
        let result = unsafe {
            spawn_system_core_thread(&mut self.apt, move || {
                system_thread::run_system_thread(
                    connection,
                    udp_socket,
                    input_receiver,
                    packet_sender,
                )
            })
        };
        let Ok(system_thread) = result else {
            log::error!(
                "Failed to spawn system core thread: {:?}",
                result.unwrap_err()
            );
            return;
        };

        let mut dropped_inputs = 0;

        while self.apt.main_loop() {
            self.hid.scan_input();

            let mut keys_down_or_held = self.hid.keys_down().union(self.hid.keys_held());
            if keys_down_or_held.contains(KeyPad::START)
                && keys_down_or_held.contains(KeyPad::SELECT)
            {
                break;
            }

            self.scan_cpp_input(receive_packet_event);

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
                key_pad: n3ds_remote_play_common::KeyPad::from_bits_truncate(
                    keys_down_or_held.bits(),
                ),
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
            match input_sender.try_send(input_state) {
                Ok(_) => {
                    dropped_inputs = 0;
                }
                Err(TrySendError::Full(_)) => {
                    // This is ok, we can drop some inputs if the system thread is busy
                    dropped_inputs += 1;
                    if dropped_inputs == 10 {
                        log::warn!("Dropped 10 input states in a row, there may be a bug");
                    }
                }
                Err(TrySendError::Closed(_)) => {
                    log::error!("Input sender channel closed");
                    break;
                }
            }

            // Drain/decode any received MPEG packets on the main thread.
            self.process_video_stream(&packet_receiver);

            self.gfx.wait_for_vblank();
        }

        unsafe { libc::pthread_detach(system_thread) };
        log::info!("Main loop stopped");
    }

    fn scan_cpp_input(&mut self, receive_packet_event: ctru_sys::Handle) {
        // Check if we've received a packet from the circle pad pro
        let packet_received = receive_packet_event.wait_for_event(Duration::ZERO).is_ok();
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

    fn process_video_stream(
        &mut self,
        packet_receiver: &std::sync::mpsc::Receiver<(BytesMut, Duration)>,
    ) {
        // Drain queued MPEG packets, decode them, and display the most recent decoded frame.
        let frame_decode_start = Instant::now();
        let frame: BgrFrameView<'_>;
        let packet_size: usize;
        let frame_recv_duration: Duration;

        if let Ok((packet, recv_duration)) = packet_receiver.try_recv() {
            packet_size = packet.len();
            frame_recv_duration = recv_duration;

            match self.mpeg_decoder.decode_mpeg_packet(&packet) {
                Ok(Some(frame_view)) => {
                    frame = frame_view;
                }
                Ok(None) => {
                    // Decoder needs more input before it can output a frame.
                    return;
                }
                Err(e) => {
                    log::error!("MPEG decode error: {e}");
                    return;
                }
            }
        } else {
            // No packets available.
            return;
        }

        // Write the frame (row-by-row because FFmpeg may pad stride for alignment).
        let frame_write_start = Instant::now();
        let mut top_screen = self.gfx.top_screen.borrow_mut();
        unsafe {
            let fb = top_screen.raw_framebuffer().ptr;

            let row_bytes = frame.width * 3;
            for y in 0..frame.height {
                let src_off = y * frame.stride;
                let dst_off = y * row_bytes;
                fb.add(dst_off)
                    .copy_from(frame.data.as_ptr().add(src_off), row_bytes);
            }
        }
        let frame_flush_start = Instant::now();
        top_screen.flush_buffers();
        top_screen.swap_buffers();

        let frame_flush_duration = frame_flush_start.elapsed();
        let frame_write_duration = frame_flush_start.duration_since(frame_write_start);
        log::debug!("Write: {frame_write_duration:.1?}, Flush: {frame_flush_duration:.1?}");
        log::info!(
            "MPEG: {:5} Rx: {:3?}ms De: {:2?}ms",
            packet_size,
            frame_recv_duration.as_millis(),
            frame_decode_start.elapsed().as_millis()
        );
        log::debug!("");
    }

    /// Detect and enable 3DS Circle Pad Pro (or continue without it if user skips)
    fn connect_circle_pad_pro(&mut self) {
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
        'cpp_connect_loop: while self.apt.main_loop() {
            self.hid.scan_input();
            let keys_down_or_held = self.hid.keys_down().union(self.hid.keys_held());

            if keys_down_or_held.contains(KeyPad::B) {
                log::info!("Canceling New 3DS / Circle Pad Pro detection");
                break;
            }

            if keys_down_or_held.contains(KeyPad::A) {
                log::info!("Trying to connect. Press Start to cancel.");

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

            self.gfx.wait_for_vblank();
        }
    }
}

fn set_udp_recv_buffer_size(udp_socket: &UdpSocket, size: usize) {
    unsafe {
        let socket_fd = udp_socket.as_raw_fd();
        if libc::setsockopt(
            socket_fd,
            libc::SOL_SOCKET,
            libc::SO_RCVBUF,
            &size as *const _ as *const libc::c_void,
            size_of::<libc::c_int>() as libc::socklen_t,
        ) != 0
        {
            panic!("setsockopt failed");
        }
    }
}
