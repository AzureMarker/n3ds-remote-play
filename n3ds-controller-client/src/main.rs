use ctru::applets::swkbd::{Button, Swkbd};
use ctru::services::soc::Soc;
use ctru::{
    console::Console,
    services::{hid::KeyPad, Apt, Hid},
    Gfx,
};
use std::io::Write;
use std::net::{Ipv4Addr, TcpListener, TcpStream};
use std::str::FromStr;
use std::time::Duration;

fn main() {
    ctru::init();
    let gfx = Gfx::default();
    gfx.top_screen.borrow_mut().set_wide_mode(true);
    let hid = Hid::init().expect("Couldn't obtain HID controller");
    let apt = Apt::init().expect("Couldn't obtain APT controller");
    let soc = Soc::init().expect("Couldn't initialize networking");
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
        "Connected to server. Button presses will be sent to the server. Press START to exit."
    );

    // Main loop
    while apt.main_loop() {
        hid.scan_input();

        let keys_down = hid.keys_down();
        let keys_up = hid.keys_up();

        if keys_down.contains(KeyPad::KEY_START) {
            break;
        }

        if keys_down.contains(KeyPad::KEY_A) {
            connection.write_all(b"DOWN: A\n").unwrap();
        }

        if keys_up.contains(KeyPad::KEY_A) {
            connection.write_all(b"UP: A\n").unwrap();
        }

        gfx.flush_buffers();
        gfx.swap_buffers();
        gfx.wait_for_vblank();
    }
}

fn get_server_ip() -> Option<Ipv4Addr> {
    let mut keyboard = Swkbd::default();
    let mut input = String::new();

    loop {
        match keyboard.get_utf8(&mut input) {
            Ok(Button::Right) => {
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
            Ok(Button::Left) => {
                // Clicked "Cancel"
                println!("Cancel was clicked, exiting in 5 seconds...");
                std::thread::sleep(Duration::from_secs(5));
                return None;
            }
            Ok(Button::Middle) => {
                // This button wasn't shown
                unreachable!()
            }
            Err(e) => {
                panic!("Error: {:?}", e)
            }
        }
    }
}
