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
use std::io::Write;
use std::net::{Ipv4Addr, TcpStream, UdpSocket};
use std::str::FromStr;
use std::time::{Duration, Instant};

const PACKET_INFO_SIZE: usize = 8;
const MAX_PACKET_SIZE: usize = 32;
const PACKET_COUNT: usize = 1;
const PACKET_BUFFER_SIZE: usize = PACKET_COUNT * (PACKET_INFO_SIZE + MAX_PACKET_SIZE);
const CPP_CONNECTION_POLLING_PERIOD_MS: u8 = 0x08;
const CPP_POLLING_PERIOD_MS: u8 = 0x32;

fn main() {
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

    println!("Enter the n3ds-remote-play server IP");
    let server_ip = match get_server_ip(&apt, &gfx) {
        Some(server_ip) => server_ip,
        None => {
            println!("Cancel was clicked, exiting in 5 seconds...");
            std::thread::sleep(Duration::from_secs(5));
            return;
        }
    };
    // let server_ip = Ipv4Addr::new(192, 168, 1, 141);

    let mut remote_play_client = RemotePlayClient::new(apt, &gfx, ir_user, hid, server_ip)
        .expect("User has canceled client creation");
    println!("Connected to remote play server at {server_ip}.");

    remote_play_client.run();
}

struct RemotePlayClient<'gfx> {
    apt: Apt,
    gfx: &'gfx Gfx,
    ir_user: IrUser,
    hid: Hid,
    connection: TcpStream,
    udp_connection: UdpSocket,
    last_cpp_input: CirclePadProInputResponse,
}

impl<'gfx> RemotePlayClient<'gfx> {
    fn new(
        apt: Apt,
        gfx: &'gfx Gfx,
        ir_user: IrUser,
        hid: Hid,
        server_ip: Ipv4Addr,
    ) -> Option<Self> {
        let connection =
            TcpStream::connect((server_ip, 3535)).expect("Failed to connect to server");
        connection.set_nonblocking(true).unwrap();
        let local_address = connection.local_addr().unwrap();

        let udp_connection =
            UdpSocket::bind(local_address).expect("Failed to listen for UDP connections");
        // udp_connection.set_nonblocking(true).unwrap();
        udp_connection
            .connect((server_ip, 3535))
            .expect("Failed to set up UDP connection to server");

        Some(Self {
            apt,
            gfx,
            ir_user,
            hid,
            connection,
            udp_connection,
            last_cpp_input: CirclePadProInputResponse::default(),
        })
    }

    fn send_input_state(&mut self, state: InputState) -> anyhow::Result<()> {
        println!("Sending {state:?}");

        let bincode_options = bincode::DefaultOptions::new();
        let state_size: u32 = bincode_options.serialized_size(&state)?.try_into()?;

        self.connection.write_all(&state_size.to_be_bytes())?;
        bincode_options.serialize_into(&mut self.connection, &state)?;
        Ok(())
    }

    fn run(&mut self) {
        self.gfx.top_screen.borrow_mut().set_double_buffering(false);
        self.gfx.top_screen.borrow_mut().swap_buffers();
        // gfx.top_screen.borrow_mut().set_wide_mode(true);

        self.connect_circle_pad_pro();

        let receive_packet_event = self
            .ir_user
            .get_recv_event()
            .expect("Couldn't get ir:USER recv event");

        let mut udp_recv_buffer = vec![0u8; 70_000];
        let mut frame_read_start;

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
            self.send_input_state(input_state).unwrap();

            // Check if we've received a frame from the PC
            frame_read_start = Instant::now();
            let udp_result = self.udp_connection.recv(&mut udp_recv_buffer);
            if let Ok(datagram_size) = udp_result {
                let frame_decode_start = Instant::now();
                // println!(
                //     "Got datagram of size {datagram_size} bytes, duration: {:?}",
                //     frame_decode_start.duration_since(frame_read_start)
                // );
                let jpeg_frame = &udp_recv_buffer[..datagram_size];
                let frame = jpeg_decoder::Decoder::new(jpeg_frame).decode().unwrap();
                // println!(
                //     "Decoded frame with size: {}, duration: {:?}",
                //     frame.len(),
                //     frame_decode_start.elapsed()
                // );

                // Write the frame
                let frame_write_start = Instant::now();
                unsafe {
                    self.gfx
                        .top_screen
                        .borrow_mut()
                        .raw_framebuffer()
                        .ptr
                        .copy_from(frame.as_ptr(), frame.len());
                }
                let frame_flush_start = Instant::now();
                let mut top_screen = self.gfx.top_screen.borrow_mut();
                top_screen.flush_buffers();
                // top_screen.swap_buffers();

                let frame_flush_duration = frame_flush_start.elapsed();
                let frame_write_duration = frame_flush_start.duration_since(frame_write_start);
                // println!("Write: {frame_write_duration:?}, Flush: {frame_flush_duration:?}");
                println!(
                    "Datagram size: {datagram_size:5}, Recv: {:3?}ms, Proc: {:3?}ms",
                    frame_decode_start
                        .duration_since(frame_read_start)
                        .as_millis(),
                    frame_decode_start.elapsed().as_millis()
                );
            } else if let Err(e) = udp_result {
                if e.kind() != std::io::ErrorKind::WouldBlock {
                    eprintln!(
                        "Error while checking for UDP packet: {e}, kind = {}",
                        e.kind()
                    );
                }
            }

            self.gfx.wait_for_vblank();
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

        println!(
            "If you have a New 3DS or Circle Pad Pro, press A to connect extra inputs. Otherwise press B."
        );
        'cpp_connect_loop: while self.apt.main_loop() {
            self.hid.scan_input();
            let keys_down_or_held = self.hid.keys_down().union(self.hid.keys_held());

            if keys_down_or_held.contains(KeyPad::B) {
                println!("Canceling New 3DS / Circle Pad Pro detection");
                break;
            }

            if keys_down_or_held.contains(KeyPad::A) {
                println!("Trying to connect. Press Start to cancel.");

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
                    {
                        if !e.is_timeout() {
                            panic!("Couldn't initialize circle pad pro connection: {e}");
                        }
                    }

                    if self.ir_user.get_status_info().connection_status
                        == ConnectionStatus::Connected
                    {
                        println!("Connected!");
                        break;
                    }

                    // If not connected (ex. timeout), disconnect so we can retry
                    self.ir_user
                        .disconnect()
                        .expect("Failed to disconnect circle pad pro connection");

                    // Wait for the disconnect to go through
                    if let Err(e) =
                        connection_status_event.wait_for_event(Duration::from_millis(100))
                    {
                        if !e.is_timeout() {
                            panic!("Couldn't initialize circle pad pro connection: {e}");
                        }
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
                        println!("Error: {e:?}");
                    }

                    // Wait for the response
                    let recv_event_result =
                        receive_packet_event.wait_for_event(Duration::from_millis(100));

                    if recv_event_result.is_ok() {
                        println!("Got first packet from CPP");
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
                        println!("Invalid IP address, try again");
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
