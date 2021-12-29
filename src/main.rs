use ctru::services::soc::Soc;
use ctru::{
    console::Console,
    services::{hid::KeyPad, Apt, Hid},
    Gfx,
};
use std::io::Write;
use std::net::{TcpListener, TcpStream};
use std::time::Duration;

fn main() {
    let gfx = Gfx::default();
    let hid = Hid::init().expect("Couldn't obtain HID controller");
    let apt = Apt::init().expect("Couldn't obtain APT controller");
    let _soc = Soc::init().expect("Couldn't initialize networking");
    let _console = Console::default();

    println!("Hello, world!");

    let tcp_listener = match TcpListener::bind("0.0.0.0:80") {
        Ok(socket) => socket,
        Err(e) => {
            println!("Error while binding: {}", e);
            std::thread::sleep(Duration::from_secs(2));
            return;
        }
    };
    println!("Started server socket");
    tcp_listener
        .set_nonblocking(true)
        .expect("Couldn't make socket nonblocking");

    // Main loop
    while apt.main_loop() {
        // Scan all the inputs. This should be done once for each frame
        hid.scan_input();

        if hid.keys_down().contains(KeyPad::KEY_START) {
            break;
        }

        match tcp_listener.accept() {
            Ok((stream, socket_addr)) => {
                println!("Got connection from {}", socket_addr);
                if let Err(e) = write_hello_world(stream) {
                    println!("Error writing response: {}", e);
                }
            }
            Err(e) => match e.kind() {
                std::io::ErrorKind::WouldBlock => {}
                _ => {
                    println!("Error accepting connection: {}", e)
                }
            },
        }

        // Flush and swap frame buffers
        gfx.flush_buffers();
        gfx.swap_buffers();

        // Wait for VBlank
        gfx.wait_for_vblank();
    }
}

fn write_hello_world(mut stream: TcpStream) -> std::io::Result<()> {
    stream.set_nonblocking(false)?;
    stream.write_all(b"Hello world!\n")?;
    stream.flush()
}
