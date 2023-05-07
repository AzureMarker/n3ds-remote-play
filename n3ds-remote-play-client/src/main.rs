use bincode::Options;
use ctru::applets::swkbd::{self, Swkbd};
use ctru::console::Console;
use ctru::prelude::Gfx;
use ctru::services::gfx::{Flush, Screen, Swap};
use ctru::services::hid::KeyPad;
use ctru::services::ir_user::{CirclePadProInputResponse, IrDeviceId, IrUser};
use ctru::services::soc::Soc;
use ctru::services::srv::HandleExt;
use ctru::services::{Apt, Hid};
use n3ds_remote_play_common::{CStick, CirclePad, InputState};
use std::io::{Read, Write};
use std::net::{Ipv4Addr, TcpStream};
use std::str::FromStr;
use std::time::{Duration, Instant};

const PACKET_INFO_SIZE: usize = 8;
const MAX_PACKET_SIZE: usize = 32;
const PACKET_COUNT: usize = 1;
const PACKET_BUFFER_SIZE: usize = PACKET_COUNT * (PACKET_INFO_SIZE + MAX_PACKET_SIZE);
const CPP_CONNECTION_POLLING_PERIOD_MS: u8 = 0x08;
const CPP_POLLING_PERIOD_MS: u8 = 0x32;

fn main() {
    ctru::use_panic_handler();
    let gfx = Gfx::new().expect("Couldn't obtain GFX controller");
    gfx.top_screen.borrow_mut().set_double_buffering(false);
    gfx.top_screen.borrow_mut().swap_buffers();
    // gfx.top_screen.borrow_mut().set_wide_mode(true);
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
    let mut hid = Hid::new().expect("Couldn't obtain HID controller");

    // println!("Enter the n3ds-remote-play server IP");
    // let server_ip = match get_server_ip() {
    //     Some(server_ip) => server_ip,
    //     None => return,
    // };
    let server_ip = "192.168.81.82";

    let mut connection =
        TcpStream::connect((server_ip, 3535)).expect("Failed to connect to server");
    connection.set_nonblocking(true).unwrap();

    println!(
        "Connected to server. Button presses will be sent to the server. \
        Press START + SELECT to exit."
    );

    // Detect and enable 3DS Circle Pad Pro
    // Get event handles
    let connection_status_event = ir_user
        .get_connection_status_event()
        .expect("Couldn't get ir:USER connection status event");
    let receive_packet_event = ir_user
        .get_recv_event()
        .expect("Couldn't get ir:USER recv event");

    println!(
        "If you have a New 3DS or Circle Pad Pro, press A to connect extra inputs. Otherwise press B."
    );
    let mut last_cpp_input = CirclePadProInputResponse::default();
    'cpp_connect_loop: while apt.main_loop() {
        hid.scan_input();
        let keys_down_or_held = hid.keys_down().union(hid.keys_held());

        if keys_down_or_held.contains(KeyPad::B) {
            break;
        }

        if keys_down_or_held.contains(KeyPad::A) {
            println!("Trying to connect. Press Start to cancel.");

            // Connection loop
            loop {
                hid.scan_input();
                if hid.keys_held().contains(KeyPad::START) {
                    break 'cpp_connect_loop;
                }

                // Start the connection process
                ir_user
                    .require_connection(IrDeviceId::CirclePadPro)
                    .expect("Couldn't initialize circle pad pro connection");

                // Wait for the connection to establish
                if let Err(e) = connection_status_event.wait_for_event(Duration::from_millis(100)) {
                    if !e.is_timeout() {
                        panic!("Couldn't initialize circle pad pro connection: {e}");
                    }
                }

                if ir_user.get_status_info().connection_status == 2 {
                    println!("Connected!");
                    break;
                }

                // If not connected (ex. timeout), disconnect so we can retry
                ir_user
                    .disconnect()
                    .expect("Failed to disconnect circle pad pro connection");

                // Wait for the disconnect to go through
                if let Err(e) = connection_status_event.wait_for_event(Duration::from_millis(100)) {
                    if !e.is_timeout() {
                        panic!("Couldn't initialize circle pad pro connection: {e}");
                    }
                }
            }

            // Sending first packet retry loop
            loop {
                hid.scan_input();
                if hid.keys_held().contains(KeyPad::START) {
                    break 'cpp_connect_loop;
                }

                // Send a request for input to the CPP
                if let Err(e) = ir_user.request_input_polling(CPP_CONNECTION_POLLING_PERIOD_MS) {
                    println!("Error: {e:?}");
                }

                // Wait for the response
                let recv_event_result =
                    receive_packet_event.wait_for_event(Duration::from_millis(100));

                if recv_event_result.is_ok() {
                    println!("Got first packet from CPP");
                    let packets = ir_user.get_packets();
                    let packet_count = packets.len();
                    let Some(packet) = packets.last() else { panic!("No packets found") };
                    let cpp_input = CirclePadProInputResponse::try_from(packet)
                        .expect("Failed to parse CPP response from IR packet");
                    last_cpp_input = cpp_input;

                    // Done handling the packets, release them
                    ir_user
                        .release_received_data(packet_count as u32)
                        .expect("Failed to release ir:USER packets");

                    // Remind the CPP that we're still listening
                    ir_user
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

    // Main loop
    let mut frame = None;
    let mut frame_filled_bytes = 0;
    let mut frame_read_start = Instant::now();
    while apt.main_loop() {
        hid.scan_input();

        let mut keys_down_or_held = hid.keys_down().union(hid.keys_held());
        if keys_down_or_held.contains(KeyPad::START) && keys_down_or_held.contains(KeyPad::SELECT) {
            break;
        }

        // Check if we've received a frame from the PC
        let mut packet_size_buf = [0; 4];
        if frame.is_none() && connection.read_exact(&mut packet_size_buf).is_ok() {
            // Read the frame size (should be the same but haven't tested)
            let frame_size = u32::from_be_bytes(packet_size_buf);
            println!("Got frame size: {frame_size}");
            frame = Some(vec![0; frame_size as usize]);
            frame_read_start = Instant::now();
        }

        // Read the frame
        if let Some(frame_bytes) = &mut frame {
            let mut sleep_count = 0;
            while frame_filled_bytes != frame_bytes.len() {
                match connection.read(&mut frame_bytes[frame_filled_bytes..]) {
                    Ok(bytes_read) => {
                        frame_filled_bytes += bytes_read;
                        // println!("Added {bytes_read} bytes to frame. Total: {frame_filled_bytes}");
                    }
                    Err(e) => {
                        if e.kind() == std::io::ErrorKind::WouldBlock {
                            // println!("Pausing frame read to wait for network");
                            // break;
                            sleep_count += 1;
                            std::thread::sleep(Duration::from_millis(1));
                            continue;
                        }
                        panic!("{e}");
                    }
                }
            }
            println!("Slept {sleep_count} times");

            if frame_filled_bytes == frame_bytes.len() {
                let frame_read_duration = frame_read_start.elapsed();
                println!("Finished reading frame. Duration: {frame_read_duration:?}");

                // Write the frame
                let frame_write_start = Instant::now();
                unsafe {
                    gfx.top_screen
                        .borrow_mut()
                        .raw_framebuffer()
                        .ptr
                        .copy_from(frame_bytes.as_ptr(), frame_bytes.len());
                }
                let frame_flush_start = Instant::now();
                let mut top_screen = gfx.top_screen.borrow_mut();
                top_screen.flush_buffers();
                // top_screen.swap_buffers();

                let frame_flush_duration =  frame_flush_start.elapsed();
                let frame_write_duration =  frame_flush_start.duration_since(frame_write_start);
                println!("Write: {frame_write_duration:?}, Flush: {frame_flush_duration:?}");

                frame = None;
                frame_filled_bytes = 0;
            }
        }

        // Check if we've received a packet from the circle pad pro
        let packet_received = receive_packet_event.wait_for_event(Duration::ZERO).is_ok();
        if packet_received {
            let packets = ir_user.get_packets();
            let packet_count = packets.len();
            let Some(packet) = packets.last() else { panic!("No packets found") };
            let cpp_input = CirclePadProInputResponse::try_from(packet)
                .expect("Failed to parse CPP response from IR packet");
            last_cpp_input = cpp_input;

            // Done handling the packets, release them
            ir_user
                .release_received_data(packet_count as u32)
                .expect("Failed to release ir:USER packets");

            // Remind the CPP that we're still listening
            ir_user
                .request_input_polling(CPP_POLLING_PERIOD_MS)
                .expect("Failed to request input polling from CPP");
        }

        // Add in the buttons from CPP
        if last_cpp_input.r_pressed {
            keys_down_or_held |= KeyPad::R;
        }
        if last_cpp_input.zl_pressed {
            keys_down_or_held |= KeyPad::ZL;
        }
        if last_cpp_input.zr_pressed {
            keys_down_or_held |= KeyPad::ZR;
        }

        let circle_pad_pos = hid.circlepad_position();
        let input_state = InputState {
            key_pad: n3ds_remote_play_common::KeyPad::from_bits_truncate(keys_down_or_held.bits()),
            circle_pad: CirclePad {
                x: circle_pad_pos.0,
                y: circle_pad_pos.1,
            },
            c_stick: CStick {
                x: last_cpp_input.c_stick_x,
                y: last_cpp_input.c_stick_y,
            },
        };
        send_input_state(input_state, &mut connection).unwrap();

        gfx.wait_for_vblank();
    }
}

fn send_input_state(state: InputState, connection: &mut TcpStream) -> anyhow::Result<()> {
    let bincode_options = bincode::DefaultOptions::new();
    let state_size: u32 = bincode_options.serialized_size(&state)?.try_into()?;
    let mut buffer = Vec::new();

    buffer.extend_from_slice(&state_size.to_be_bytes());
    bincode_options.serialize_into(&mut buffer, &state)?;

    // println!("Sending {state:?}");

    Ok(connection.write_all(&buffer)?)
}

fn get_server_ip() -> Option<Ipv4Addr> {
    let mut keyboard = Swkbd::default();

    loop {
        match keyboard.get_string(20) {
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
                println!("Cancel was clicked, exiting in 5 seconds...");
                std::thread::sleep(Duration::from_secs(5));
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
