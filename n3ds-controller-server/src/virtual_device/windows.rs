use crate::virtual_device::VirtualDeviceFactory;
use crate::VirtualDevice;
use async_trait::async_trait;
use n3ds_controller_common::{Button, ButtonAction, InputMessage};
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
        })
    }
}

pub struct ViGEmDevice {
    target: Xbox360Wired<Arc<vigem_client::Client>>,
    gamepad: XGamepad,
}

impl VirtualDevice for ViGEmDevice {
    fn emit_input(&mut self, message: InputMessage) -> anyhow::Result<()> {
        self.target.wait_ready()?;

        match message {
            InputMessage::Button { action, button } => {
                let xinput_button = match button {
                    Button::A => XButtons::A,
                    Button::B => XButtons::B,
                    Button::X => XButtons::X,
                    Button::Y => XButtons::Y,
                    Button::L => XButtons::LB,
                    Button::R => XButtons::RB,
                    Button::ZL => XButtons::LTHUMB,
                    Button::ZR => XButtons::RTHUMB,
                    Button::Up => XButtons::UP,
                    Button::Down => XButtons::DOWN,
                    Button::Left => XButtons::LEFT,
                    Button::Right => XButtons::RIGHT,
                    Button::Start => XButtons::START,
                    Button::Select => XButtons::GUIDE,
                };

                match action {
                    ButtonAction::Pressed => {
                        self.gamepad.buttons |= xinput_button;
                    }
                    ButtonAction::Released => {
                        self.gamepad.buttons &= xinput_button.not();
                    }
                }
            }
            InputMessage::CirclePadPosition(x, y) => {
                self.gamepad.thumb_lx = (x as f32 / N3DS_CPAD_AXES_LIMIT as f32) * 100;
                self.gamepad.thumb_ly = (y as f32 / N3DS_CPAD_AXES_LIMIT as f32) * 100;
            }
        }

        self.target.update(&self.gamepad)?;

        Ok(())
    }
}
