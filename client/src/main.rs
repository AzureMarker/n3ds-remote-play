mod client;
mod input_handler;
mod mpeg_decoder;
mod system_thread;
mod thread_spawning;
mod udp_async_reader;

use client::RemotePlayClient;
use ctru::applets::swkbd;
use ctru::applets::swkbd::SoftwareKeyboard;
use ctru::console::Console;
use ctru::prelude::{Apt, Gfx, Hid};
use ctru::services::ir_user::IrUser;
use ctru::services::soc::Soc;
use std::io::Write;
use std::net::Ipv4Addr;
use std::panic;
use std::str::FromStr;
use std::thread::sleep;
use std::time::Duration;

const PRESET_SERVER_IP: Option<&str> = option_env!("N3DS_REMOTE_PLAY_SERVER_IP");

// ir:USER service constants. These are used to initialize the shared memory buffer for receiving IR data.
const PACKET_COUNT: usize = 1;
const PACKET_INFO_SIZE: usize = 8;
const MAX_PACKET_SIZE: usize = 32;
const PACKET_BUFFER_SIZE: usize = PACKET_COUNT * (PACKET_INFO_SIZE + MAX_PACKET_SIZE);

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

    let Some(server_ip) = get_server_ip(&apt, &gfx) else {
        log::info!("Cancel was clicked, exiting in 5 seconds...");
        sleep(Duration::from_secs(5));
        return;
    };

    // Run the main client logic, which will block until the user exits the app or an error occurs
    RemotePlayClient::new(apt, &gfx).run(server_ip, hid, ir_user);

    log::info!("Exiting in 10 seconds...");
    sleep(Duration::from_secs(10));
    log::info!("Main thread exiting");
}

/// The 3DS bottom screen is very small, so this log format is optimized to keep messages short.
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

/// Prompts the user to enter the server IP address using the software keyboard.
/// Returns `None` if the user cancels the input.
///
/// If `PRESET_SERVER_IP` is set, it will be used instead of prompting the user.
fn get_server_ip(apt: &Apt, gfx: &Gfx) -> Option<Ipv4Addr> {
    // Check for a preset server IP address
    if let Some(server_ip) = PRESET_SERVER_IP
        && !server_ip.trim().is_empty()
    {
        log::info!("Using preset server IP: {server_ip}");
        match server_ip.parse() {
            Ok(ip_addr) => return Some(ip_addr),
            Err(_) => log::warn!("Invalid preset server IP, falling back to user input"),
        }
    }

    log::info!("Enter the n3ds-remote-play server IP");
    let mut keyboard = SoftwareKeyboard::default();
    loop {
        match keyboard.launch(apt, gfx) {
            Ok((input, swkbd::Button::Right)) => {
                // Clicked "OK"
                match Ipv4Addr::from_str(&input) {
                    Ok(ip_addr) => {
                        log::info!("Using server IP: {ip_addr}");
                        return Some(ip_addr);
                    }
                    Err(_) => {
                        log::warn!("Invalid IP address, try again");
                        continue;
                    }
                };
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
