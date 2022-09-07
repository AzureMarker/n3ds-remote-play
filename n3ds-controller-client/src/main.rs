use bincode::Options;
use ctru::applets::swkbd::{self, Swkbd};
use ctru::services::hid::CirclePosition;
use ctru::services::soc::Soc;
use ctru::{
    console::Console,
    services::{hid::KeyPad, Apt, Hid},
    Gfx,
};
use n3ds_controller_common::{Button, ButtonAction, InputMessage};
use std::io::Write;
use std::net::{Ipv4Addr, TcpStream};
use std::str::FromStr;
use std::time::Duration;

fn main() {
    ctru::init();
    let gfx = Gfx::init().expect("Couldn't obtain GFX controller");
    gfx.top_screen.borrow_mut().set_wide_mode(true);
    let hid = Hid::init().expect("Couldn't obtain HID controller");
    let apt = Apt::init().expect("Couldn't obtain APT controller");
    let _soc = Soc::init().expect("Couldn't initialize networking");
    let _console = Console::init(gfx.top_screen.borrow_mut());

    println!("Enter the n3ds-controller server IP");
    let server_ip = match get_server_ip() {
        Some(server_ip) => server_ip,
        None => return,
    };

    let mut connection =
        TcpStream::connect((server_ip, 3535)).expect("Failed to connect to server");
    connection.set_nonblocking(true).unwrap();

    println!(
        "Connected to server. Button presses will be sent to the server. \
        Press START + SELECT to exit."
    );

    // Main loop
    let mut circle_position = CirclePosition::new();
    let mut last_circle_position = (0, 0);
    while apt.main_loop() {
        hid.scan_input();

        let keys_down = hid.keys_down();
        let keys_up = hid.keys_up();
        let keys_down_or_held = keys_down.union(hid.keys_held());

        if keys_down_or_held.contains(KeyPad::KEY_START)
            && keys_down_or_held.contains(KeyPad::KEY_SELECT)
        {
            break;
        }

        // Send button press updates
        send_keys(keys_down, ButtonAction::Pressed, &mut connection).unwrap();
        send_keys(keys_up, ButtonAction::Released, &mut connection).unwrap();

        // Send circle pad update if it changed
        let new_circle_position = circle_position.get();
        if new_circle_position != last_circle_position {
            send_message(
                InputMessage::CirclePadPosition(new_circle_position.0, new_circle_position.1),
                &mut connection,
            )
            .unwrap();
        }
        last_circle_position = new_circle_position;

        gfx.flush_buffers();
        gfx.swap_buffers();
        gfx.wait_for_vblank();
    }
}

fn send_keys(
    keypad: KeyPad,
    action: ButtonAction,
    connection: &mut TcpStream,
) -> anyhow::Result<()> {
    macro_rules! handle_keys {
        ($($keypad_key:ident => $button:ident),*) => {
            $(
            if keypad.contains(KeyPad::$keypad_key) {
                send_message(
                    InputMessage::Button {
                        action,
                        button: Button::$button,
                    },
                    connection,
                )?;
            }
            )*
        };
    }

    handle_keys!(
        KEY_A => A,
        KEY_B => B,
        KEY_X => X,
        KEY_Y => Y,
        KEY_L => L,
        KEY_R => R,
        KEY_DUP => Up,
        KEY_DDOWN => Down,
        KEY_DLEFT => Left,
        KEY_DRIGHT => Right,
        KEY_START => Start,
        KEY_SELECT => Select
    );

    Ok(())
}

fn send_message(message: InputMessage, connection: &mut TcpStream) -> anyhow::Result<()> {
    let bincode_options = bincode::DefaultOptions::new();
    let message_size: u32 = bincode_options.serialized_size(&message)?.try_into()?;
    let mut buffer = Vec::new();

    buffer.extend_from_slice(&message_size.to_be_bytes());
    bincode_options.serialize_into(&mut buffer, &message)?;

    println!("Sending {message:?}");

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
                panic!("Error: {:?}", e)
            }
        }
    }
}
