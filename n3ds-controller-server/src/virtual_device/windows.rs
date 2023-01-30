use crate::virtual_device::VirtualDeviceFactory;
use crate::VirtualDevice;
use async_trait::async_trait;
use n3ds_controller_common::{InputState, KeyPad};
use std::ops::Not;
use std::sync::Arc;
use vigem_client::{TargetId, XButtons, XGamepad, Xbox360Wired};

const N3DS_CPAD_AXES_LIMIT: i32 = 156;

pub struct ViGEmDeviceFactory {
    client: Arc<vigem_client::Client>,
}

impl ViGEmDeviceFactory {
    pub fn new() -> anyhow::Result<Self> {
        Ok(Self {
            client: Arc::new(vigem_client::Client::connect()?),
        })
    }
}

#[async_trait]
impl VirtualDeviceFactory for ViGEmDeviceFactory {
    type Device = ViGEmDevice;

    async fn new_device(&self) -> anyhow::Result<Self::Device> {
        let mut target = Xbox360Wired::new(Arc::clone(&self.client), TargetId::XBOX360_WIRED);
        target.plugin()?;

        Ok(ViGEmDevice {
            target,
            gamepad: XGamepad::default(),
            // Initial guesses
            c_stick_range_x: (1500, 2500),
            c_stick_range_y: (1500, 2500),
        })
    }
}

pub struct ViGEmDevice {
    target: Xbox360Wired<Arc<vigem_client::Client>>,
    gamepad: XGamepad,
    c_stick_range_x: (u16, u16),
    c_stick_range_y: (u16, u16),
}

impl VirtualDevice for ViGEmDevice {
    fn emit_input(&mut self, input_state: InputState) -> anyhow::Result<()> {
        self.target.wait_ready()?;
        let mut gamepad = XGamepad::default();

        // Update buttons
        let update_button = |input_key: KeyPad, xbox_button: u16| {
            if input_state.key_pad.contains(input_key) {
                gamepad.buttons.raw |= xbox_button;
            }
        };

        update_button(KeyPad::KEY_A, XButtons::A);
        update_button(KeyPad::KEY_B, XButtons::B);
        update_button(KeyPad::KEY_X, XButtons::X);
        update_button(KeyPad::KEY_Y, XButtons::Y);
        update_button(KeyPad::KEY_L, XButtons::LB);
        update_button(KeyPad::KEY_R, XButtons::RB);
        update_button(KeyPad::KEY_DUP, XButtons::UP);
        update_button(KeyPad::KEY_DDOWN, XButtons::DOWN);
        update_button(KeyPad::KEY_DLEFT, XButtons::LEFT);
        update_button(KeyPad::KEY_DRIGHT, XButtons::RIGHT);
        update_button(KeyPad::KEY_START, XButtons::START);
        update_button(KeyPad::KEY_SELECT, XButtons::BACK);

        // Update trigger buttons (ZL/ZR)
        if input_state.key_pad.contains(KeyPad::KEY_ZL) {
            gamepad.left_trigger = u8::MAX;
        }
        if input_state.key_pad.contains(KeyPad::KEY_ZL) {
            gamepad.right_trigger = u8::MAX;
        }

        // Update left stick (C-pad)
        let cpad_x_percent = (input_state.circle_pad.x as f32 / N3DS_CPAD_AXES_LIMIT as f32);
        self.gamepad.thumb_lx = (cpad_x_percent * i16::MAX as f32) as i16;
        let cpad_y_percent = (input_state.circle_pad.y as f32 / N3DS_CPAD_AXES_LIMIT as f32);
        self.gamepad.thumb_ly = (cpad_y_percent * i16::MAX as f32) as i16;

        // Auto-adjust C-stick calibration
        if input_state.c_stick.x < self.c_stick_range_x.0 {
            self.c_stick_range_x.0 = input_state.c_stick.x;
        }
        if input_state.c_stick.x > self.c_stick_range_x.1 {
            self.c_stick_range_x.1 = input_state.c_stick.x;
        }
        if input_state.c_stick.y < self.c_stick_range_y.0 {
            self.c_stick_range_y.0 = input_state.c_stick.y;
        }
        if input_state.c_stick.y > self.c_stick_range_y.1 {
            self.c_stick_range_y.1 = input_state.c_stick.y;
        }

        // Update right stick (C-stick)
        let cstick_x_range = self.c_stick_range_x.1 - self.c_stick_range_x.0;
        let cstick_y_range = self.c_stick_range_y.1 - self.c_stick_range_y.0;

        let cstick_x_percent =
            (input_state.c_stick.x - self.c_stick_range_x.0) as f32 / cstick_x_range as f32;
        gamepad.thumb_rx = ((cstick_x_percent - 0.5) * 2.0 * i16::MAX as f32) as i16;
        let cstick_y_percent =
            (input_state.c_stick.y - self.c_stick_range_y.0) as f32 / cstick_y_range as f32;
        gamepad.thumb_ry = ((cstick_y_percent - 0.5) * 2.0 * i16::MAX as f32) as i16;

        self.target.update(&self.gamepad)?;

        Ok(())
    }
}
