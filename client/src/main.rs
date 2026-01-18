mod thread;
mod video_stream;

use crate::thread::spawn_system_core_thread;
use crate::video_stream::BgrFrameView;
use bincode::Options;
use ctru::applets::swkbd;
use ctru::applets::swkbd::SoftwareKeyboard;
use ctru::console::Console;
use ctru::prelude::{Apt, Gfx, Hid};
use ctru::services::gfx::{Flush, Screen, Swap};
use ctru::services::hid::KeyPad;
use ctru::services::ir_user::{CirclePadProInputResponse, ConnectionStatus, IrDeviceId, IrUser};
use ctru::services::soc::Soc;
use ctru::services::svc::HandleExt;
use n3ds_remote_play_common::{CStick, CirclePad, InputState};
use std::io::{Read, Write};
use std::net::{Ipv4Addr, TcpStream, UdpSocket};
use std::panic;
use std::str::FromStr;
use std::thread::sleep;
use std::time::{Duration, Instant};

const PACKET_INFO_SIZE: usize = 8;
const MAX_PACKET_SIZE: usize = 32;
const PACKET_COUNT: usize = 1;
const PACKET_BUFFER_SIZE: usize = PACKET_COUNT * (PACKET_INFO_SIZE + MAX_PACKET_SIZE);
const CPP_CONNECTION_POLLING_PERIOD_MS: u8 = 0x08;
const CPP_POLLING_PERIOD_MS: u8 = 0x32;

fn main() {
    ctru::set_panic_hook(true);
    env_logger::builder()
        .filter_level(log::LevelFilter::Info)
        .format(log_format)
        .init();

    let gfx = Gfx::new().expect("Couldn't obtain GFX controller");
    let apt = Apt::new().expect("Couldn't obtain APT controller");
    let _soc = Soc::new().expect("Couldn't initialize networking");
    let _console = Console::new(gfx.bottom_screen.borrow_mut());

    let ir_user = IrUser::init(
        PACKET_BUFFER_SIZE,
        PACKET_COUNT,
        PACKET_BUFFER_SIZE,
        PACKET_COUNT,
    )
    .expect("Couldn't initialize ir:USER service");

    // Initialize HID after ir:USER because libctru also initializes ir:rst,
    // which is mutually exclusive with ir:USER. Initializing HID before ir:USER
    // on New 3DS causes ir:USER to not work.
    let hid = Hid::new().expect("Couldn't obtain HID controller");

    // log::info!("Enter the n3ds-remote-play server IP");
    // let server_ip = match get_server_ip(&apt, &gfx) {
    //     Some(server_ip) => server_ip,
    //     None => {
    //         log::info!("Cancel was clicked, exiting in 5 seconds...");
    //         std::thread::sleep(Duration::from_secs(5));
    //         return;
    //     }
    // };
    let server_ip = Ipv4Addr::new(10, 0, 0, 132);

    let mut remote_play_client = RemotePlayClient::new(apt, &gfx, ir_user, hid);

    remote_play_client.run(server_ip);
    log::info!("Exiting in 10 seconds...");
    sleep(Duration::from_secs(10));
}

fn log_format(f: &mut env_logger::fmt::Formatter, record: &log::Record<'_>) -> std::io::Result<()> {
    let level = match record.level() {
        log::Level::Error => "E",
        log::Level::Warn => "W",
        log::Level::Info => "I",
        log::Level::Debug => "D",
        log::Level::Trace => "T",
    };
    let color_start = match record.level() {
        log::Level::Error => "\x1b[31;1m",
        log::Level::Warn => "\x1b[33;1m",
        log::Level::Info => "\x1b[0m",
        log::Level::Debug => "\x1b[2m",
        log::Level::Trace => "\x1b[2m",
    };
    let color_end = "\x1b[0m";

    writeln!(f, "{color_start}{level}: {}{color_end}", record.args())
}

struct RemotePlayClient<'gfx> {
    apt: Apt,
    gfx: &'gfx Gfx,
    ir_user: IrUser,
    hid: Hid,
    // connection: TcpStream,
    // udp_connection: UdpSocket,
    last_cpp_input: CirclePadProInputResponse,
}

impl<'gfx> RemotePlayClient<'gfx> {
    fn new(apt: Apt, gfx: &'gfx Gfx, ir_user: IrUser, hid: Hid) -> Self {
        // // Set up control connection
        // let connection =
        //     TcpStream::connect((server_ip, 3535)).expect("Failed to connect to server");
        // connection.set_nonblocking(true).unwrap();
        // let local_address = connection.local_addr().unwrap();

        // Set up packet listener
        // let udp_connection =
        //     UdpSocket::bind(local_address).expect("Failed to listen for UDP connections");
        // udp_connection.set_nonblocking(true).unwrap();
        // udp_connection
        //     .connect((server_ip, 3535))
        //     .expect("Failed to set up UDP connection to server");

        Self {
            apt,
            gfx,
            ir_user,
            hid,
            // connection,
            // udp_connection,
            // frame_reassembler: Box::new(video_stream::RtpFrameReassembler::new()),
            last_cpp_input: CirclePadProInputResponse::default(),
        }
    }

    fn send_input_state(connection: &mut UdpSocket, state: InputState) -> anyhow::Result<()> {
        log::trace!("Sending {state:?}");

        let bincode_options = bincode::DefaultOptions::new();
        let data = bincode_options.serialize(&state)?;
        connection.send(&data)?;
        Ok(())
    }

    fn run(&mut self, server_ip: Ipv4Addr) {
        // Set up connections
        let connection =
            TcpStream::connect((server_ip, 3535)).expect("Failed to connect to server");
        connection.set_nonblocking(true).unwrap();
        let udp_connection = UdpSocket::bind(connection.local_addr().unwrap())
            .expect("Failed to listen for UDP connections");
        udp_connection
            .connect((server_ip, 3535))
            .expect("Failed to set up UDP connection to server");
        udp_connection.set_nonblocking(true).unwrap();
        log::info!("Connected to remote play server at {server_ip}.");

        // Main thread owns the MPEG decoder; system thread only reassembles length-prefixed packets.
        let mut mpeg_decoder =
            video_stream::Mpeg1Decoder::new().expect("Failed to create MPEG1 decoder");

        self.gfx.top_screen.borrow_mut().set_double_buffering(false);
        self.gfx.top_screen.borrow_mut().swap_buffers();
        // gfx.top_screen.borrow_mut().set_wide_mode(true);

        self.connect_circle_pad_pro();

        let receive_packet_event = self
            .ir_user
            .get_recv_event()
            .expect("Couldn't get ir:USER recv event");

        // Set up system core thread to read network data in parallel
        let (input_sender, input_receiver) = std::sync::mpsc::channel();
        let (packet_sender, packet_receiver) = std::sync::mpsc::sync_channel(2);
        let result = unsafe {
            spawn_system_core_thread(&mut self.apt, move || {
                Self::run_system_thread(connection, udp_connection, input_receiver, packet_sender)
            })
        };
        let Ok(system_thread) = result else {
            log::error!(
                "Failed to spawn system core thread: {:?}",
                result.unwrap_err()
            );
            return;
        };

        // let mut udp_recv_buffer = vec![0u8; 70_000];

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
            if let Err(e) = input_sender.send(input_state) {
                log::error!("Input sender channel closed: {e}");
                break;
            }

            // Drain/decode any received MPEG packets on the main thread.
            self.process_video_stream(&mut mpeg_decoder, &packet_receiver);

            self.gfx.wait_for_vblank();
        }

        unsafe { libc::pthread_detach(system_thread) };
    }

    fn run_system_thread(
        mut connection: TcpStream,
        mut udp_connection: UdpSocket,
        input_reader: std::sync::mpsc::Receiver<InputState>,
        packet_writer: std::sync::mpsc::SyncSender<(Vec<u8>, Duration)>,
    ) {
        let mut packet_reassembler = video_stream::MpegPacketReassembler::new();
        let mut tcp_recv_buffer = vec![0u8; 70_000];
        let mut frame_recv_start: Option<Instant> = None;
        loop {
            // Sleep briefly to avoid busy-looping
            sleep(Duration::from_millis(1));

            // Read input state from channel
            match input_reader.try_recv() {
                Ok(state) => {
                    // Send input state to server
                    if let Err(e) = Self::send_input_state(&mut udp_connection, state) {
                        log::error!("Failed to send input state: {e}");
                        break;
                    }
                }
                Err(std::sync::mpsc::TryRecvError::Empty) => {
                    // No new input state, continue
                }
                Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                    log::info!("Input reader channel disconnected");
                    break;
                }
            };

            // Read frame data from TCP connection
            let possible_frame_recv_start = Instant::now();
            let bytes_read = match connection.read(&mut tcp_recv_buffer) {
                Ok(size) => {
                    if frame_recv_start.is_none() {
                        frame_recv_start = Some(possible_frame_recv_start);
                    }
                    size
                }
                Err(e) => {
                    if e.kind() != std::io::ErrorKind::WouldBlock {
                        log::error!(
                            "Error while checking for UDP packet: {e}, kind = {}",
                            e.kind()
                        );
                    }
                    continue;
                }
            };

            if bytes_read == 0 {
                log::info!("Server closed the connection");
                break;
            }

            let packet_data = &tcp_recv_buffer[..bytes_read];
            match packet_reassembler.push_chunk(packet_data) {
                Ok(Some(mpeg_packet)) => {
                    let frame_recv_duration = frame_recv_start.unwrap().elapsed();
                    frame_recv_start = None;

                    match packet_writer.try_send((mpeg_packet, frame_recv_duration)) {
                        Ok(()) => {
                            // Successfully sent packet
                        }
                        Err(std::sync::mpsc::TrySendError::Full(_)) => {
                            log::warn!("Packet writer channel full, dropping packet");
                        }
                        Err(std::sync::mpsc::TrySendError::Disconnected(_)) => {
                            log::error!("Packet writer channel closed");
                            break;
                        }
                    }
                }
                Ok(None) => {
                    // Need more TCP data.
                }
                Err(e) => {
                    log::error!("Error while reassembling MPEG packet: {e}");
                    break;
                }
            }
        }
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
        mpeg_decoder: &mut video_stream::Mpeg1Decoder,
        packet_receiver: &std::sync::mpsc::Receiver<(Vec<u8>, Duration)>,
    ) {
        // Drain queued MPEG packets, decode them, and display the most recent decoded frame.
        let frame_decode_start = Instant::now();
        let frame: BgrFrameView<'_>;
        let packet_size: usize;
        let frame_recv_duration: Duration;

        if let Ok((packet, recv_duration)) = packet_receiver.try_recv() {
            packet_size = packet.len();
            frame_recv_duration = recv_duration;

            match mpeg_decoder.decode_mpeg_packet(&packet) {
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
        unsafe {
            let fb = self.gfx.top_screen.borrow_mut().raw_framebuffer().ptr;

            let row_bytes = frame.width * 3;
            for y in 0..frame.height {
                let src_off = y * frame.stride;
                let dst_off = y * row_bytes;
                fb.add(dst_off)
                    .copy_from(frame.data.as_ptr().add(src_off), row_bytes);
            }
        }
        let frame_flush_start = Instant::now();
        let mut top_screen = self.gfx.top_screen.borrow_mut();
        top_screen.flush_buffers();
        // top_screen.swap_buffers();

        let frame_flush_duration = frame_flush_start.elapsed();
        let frame_write_duration = frame_flush_start.duration_since(frame_write_start);
        log::debug!("Write: {frame_write_duration:?}, Flush: {frame_flush_duration:?}");
        log::info!(
            "MPEG: {:5} Rx: {:3?}ms De: {:3?}ms",
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

fn get_server_ip(apt: &Apt, gfx: &Gfx) -> Option<Ipv4Addr> {
    let mut keyboard = SoftwareKeyboard::default();

    loop {
        match keyboard.launch(apt, gfx) {
            Ok((input, swkbd::Button::Right)) => {
                // Clicked "OK"
                let ip_addr = match Ipv4Addr::from_str(&input) {
                    Ok(ip_addr) => ip_addr,
                    Err(_) => {
                        log::warn!("Invalid IP address, try again");
                        continue;
                    }
                };
                return Some(ip_addr);
            }
            Ok((_, swkbd::Button::Left)) => {
                // Clicked "Cancel"
                return None;
            }
            Ok((_, swkbd::Button::Middle)) => {
                // This button wasn't shown
                unreachable!()
            }
            Err(e) => {
                panic!("Error: {e:?}")
            }
        }
    }
}
