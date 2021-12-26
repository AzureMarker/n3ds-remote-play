use ctru::{
    console::Console,
    services::{hid::KeyPad, Apt, Hid},
    Gfx,
};

fn main() {
    let gfx = Gfx::default();
    let hid = Hid::init().expect("Couldn't obtain HID controller");
    let apt = Apt::init().expect("Couldn't obtain APT controller");
    let _console = Console::default();

    println!("Hello, world!");

    // Main loop
    while apt.main_loop() {
        // Scan all the inputs. This should be done once for each frame
        hid.scan_input();

        if hid.keys_down().contains(KeyPad::KEY_START) {
            break;
        }

        // Flush and swap frame buffers
        gfx.flush_buffers();
        gfx.swap_buffers();

        // Wait for VBlank
        gfx.wait_for_vblank();
    }
}
