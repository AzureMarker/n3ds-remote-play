use bincode::Options;
use ctru::applets::swkbd::{self, Swkbd};
use ctru::console::Console;
use ctru::gfx::Gfx;
use ctru::services::hid::{CirclePosition, KeyPad};
use ctru::services::ir_user::{CirclePadProInputResponse, IrDeviceId, IrUser};
use ctru::services::soc::Soc;
use ctru::services::srv::HandleExt;
use ctru::services::{Apt, Hid};
use n3ds_controller_common::{CStick, CirclePad, InputState};
use std::io::Write;
use std::net::{Ipv4Addr, TcpStream};
use std::str::FromStr;
use std::time::Duration;

const PACKET_INFO_SIZE: usize = 8;
const MAX_PACKET_SIZE: usize = 32;
const PACKET_COUNT: usize = 1;
const PACKET_BUFFER_SIZE: usize = PACKET_COUNT * (PACKET_INFO_SIZE + MAX_PACKET_SIZE);
const CPP_CONNECTION_POLLING_PERIOD_MS: u8 = 0x08;
const CPP_POLLING_PERIOD_MS: u8 = 0x32;

fn main() {
    ctru::init();
    let gfx = Gfx::init().expect("Couldn't obtain GFX controller");
    gfx.top_screen.borrow_mut().set_wide_mode(true);
    let apt = Apt::init().expect("Couldn't obtain APT controller");
    let _soc = Soc::init().expect("Couldn't initialize networking");
    let _console = Console::init(gfx.top_screen.borrow_mut());

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
    let hid = Hid::init().expect("Couldn't obtain HID controller");

    println!("Enter the n3ds-controller server IP");
    // let server_ip = match get_server_ip() {
    //     Some(server_ip) => server_ip,
    //     None => return,
    // };
    let server_ip = "192.168.1.3";

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

        if keys_down_or_held.contains(KeyPad::KEY_A) {
            println!("Trying to connect. Press Start to cancel.");

            // Connection loop
            loop {
                hid.scan_input();
                if hid.keys_held().contains(KeyPad::KEY_START) {
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
                if hid.keys_held().contains(KeyPad::KEY_START) {
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
        } else if keys_down_or_held.contains(KeyPad::KEY_B) {
            break;
        }

        gfx.flush_buffers();
        gfx.swap_buffers();
        gfx.wait_for_vblank();
    }

    // Main loop
    let mut circle_position = CirclePosition::new();
    while apt.main_loop() {
        hid.scan_input();

        let mut keys_down_or_held = hid.keys_down().union(hid.keys_held());
        if keys_down_or_held.contains(KeyPad::KEY_START)
            && keys_down_or_held.contains(KeyPad::KEY_SELECT)
        {
            break;
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
            keys_down_or_held |= KeyPad::KEY_R;
        }
        if last_cpp_input.zl_pressed {
            keys_down_or_held |= KeyPad::KEY_ZL;
        }
        if last_cpp_input.zr_pressed {
            keys_down_or_held |= KeyPad::KEY_ZR;
        }

        let circle_pad_pos = circle_position.get();
        let input_state = InputState {
            key_pad: n3ds_controller_common::KeyPad::from_bits_truncate(keys_down_or_held.bits()),
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

        gfx.flush_buffers();
        gfx.swap_buffers();
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
    let mut input = String::new();

    loop {
        match keyboard.get_utf8(&mut input) {
            Ok(swkbd::Button::Right) => {
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
            Ok(swkbd::Button::Left) => {
                // Clicked "Cancel"
                println!("Cancel was clicked, exiting in 5 seconds...");
                std::thread::sleep(Duration::from_secs(5));
                return None;
            }
            Ok(swkbd::Button::Middle) => {
                // This button wasn't shown
                unreachable!()
            }
            Err(e) => {
                panic!("Error: {e:?}")
            }
        }
    }
}
