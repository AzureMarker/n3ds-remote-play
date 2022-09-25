use crate::virtual_device::VirtualDevice;
use async_trait::async_trait;
use input_linux::{
    AbsoluteAxis, AbsoluteEvent, AbsoluteInfo, AbsoluteInfoSetup, EventKind, EventTime, InputId,
    Key, KeyEvent, KeyState, SynchronizeEvent, SynchronizeKind, UInputHandle,
};
use n3ds_controller_common::{Button, ButtonAction, InputMessage};
use tokio::fs::{File, OpenOptions};

const PRO_CONTROLLER_LEFT_AXES_LIMIT: i32 = 32767;
const N3DS_CPAD_AXES_LIMIT: i32 = 156;

pub struct UInputDevice {
    handle: UInputHandle<File>,
}

#[async_trait]
impl VirtualDevice for UInputDevice {
    async fn new() -> anyhow::Result<Self> {
        let uinput_file = OpenOptions::new()
            .read(true)
            .write(true)
            .custom_flags(libc::O_NONBLOCK)
            .open("/dev/uinput")
            .await?;

        let handle = UInputHandle::new(uinput_file);

        // Register inputs
        handle.set_evbit(EventKind::Key)?;
        handle.set_keybit(Key::ButtonNorth)?;
        handle.set_keybit(Key::ButtonSouth)?;
        handle.set_keybit(Key::ButtonEast)?;
        handle.set_keybit(Key::ButtonWest)?;
        handle.set_keybit(Key::ButtonTL)?;
        handle.set_keybit(Key::ButtonTR)?;
        handle.set_keybit(Key::ButtonTL2)?;
        handle.set_keybit(Key::ButtonTR2)?;
        handle.set_keybit(Key::ButtonStart)?;
        handle.set_keybit(Key::ButtonSelect)?;

        handle.set_evbit(EventKind::Absolute)?;
        handle.set_absbit(AbsoluteAxis::X)?;
        handle.set_absbit(AbsoluteAxis::Y)?;
        handle.set_absbit(AbsoluteAxis::Hat0X)?;
        handle.set_absbit(AbsoluteAxis::Hat0Y)?;

        // These buttons/axes can't be triggered on the 3DS, but we still register
        // them for compatibility with the Switch Pro Controller (helps games and
        // browsers remap them).
        handle.set_keybit(Key::ButtonZ)?;
        handle.set_keybit(Key::ButtonThumbl)?;
        handle.set_keybit(Key::ButtonThumbr)?;
        handle.set_keybit(Key::ButtonMode)?;
        handle.set_absbit(AbsoluteAxis::RX)?;
        handle.set_absbit(AbsoluteAxis::RY)?;

        let axis_absolute_info = AbsoluteInfo {
            minimum: -PRO_CONTROLLER_LEFT_AXES_LIMIT,
            maximum: PRO_CONTROLLER_LEFT_AXES_LIMIT,
            fuzz: 250,
            flat: 500,
            value: 0,
            ..AbsoluteInfo::default()
        };
        let hat_absolute_info = AbsoluteInfo {
            minimum: -1,
            maximum: 1,
            value: 0,
            ..AbsoluteInfo::default()
        };

        handle.create(
            &InputId {
                bustype: input_linux::sys::BUS_USB,
                vendor: 0x057e,
                product: 0x2009,
                version: 0x8111,
            },
            b"Nintendo Switch Pro Controller",
            0,
            &[
                AbsoluteInfoSetup {
                    axis: AbsoluteAxis::X,
                    info: axis_absolute_info,
                },
                AbsoluteInfoSetup {
                    axis: AbsoluteAxis::Y,
                    info: axis_absolute_info,
                },
                AbsoluteInfoSetup {
                    axis: AbsoluteAxis::RX,
                    info: axis_absolute_info,
                },
                AbsoluteInfoSetup {
                    axis: AbsoluteAxis::RY,
                    info: axis_absolute_info,
                },
                AbsoluteInfoSetup {
                    axis: AbsoluteAxis::Hat0X,
                    info: hat_absolute_info,
                },
                AbsoluteInfoSetup {
                    axis: AbsoluteAxis::Hat0Y,
                    info: hat_absolute_info,
                },
            ],
        )?;

        Ok(Self { handle })
    }

    fn emit_input(&self, message: InputMessage) -> anyhow::Result<()> {
        match message {
            InputMessage::Button { action, button } => {
                let key_event = |key: Key, state: KeyState| {
                    *KeyEvent::new(EventTime::default(), key, state)
                        .as_event()
                        .as_raw()
                };
                let absolute_event = |axis: AbsoluteAxis, value: i32| {
                    *AbsoluteEvent::new(EventTime::default(), axis, value)
                        .as_event()
                        .as_raw()
                };

                let key_state = match action {
                    ButtonAction::Pressed => KeyState::PRESSED,
                    ButtonAction::Released => KeyState::RELEASED,
                };
                let absolute_value = match action {
                    ButtonAction::Pressed => 1,
                    ButtonAction::Released => 0,
                };

                let event = match button {
                    Button::A => key_event(Key::ButtonEast, key_state),
                    Button::B => key_event(Key::ButtonSouth, key_state),
                    Button::X => key_event(Key::ButtonNorth, key_state),
                    Button::Y => key_event(Key::ButtonWest, key_state),
                    Button::L => key_event(Key::ButtonTL, key_state),
                    Button::R => key_event(Key::ButtonTR, key_state),
                    Button::ZL => key_event(Key::ButtonTL2, key_state),
                    Button::ZR => key_event(Key::ButtonTR2, key_state),
                    Button::Start => key_event(Key::ButtonStart, key_state),
                    Button::Select => key_event(Key::ButtonSelect, key_state),
                    Button::Up => absolute_event(AbsoluteAxis::Hat0Y, -absolute_value),
                    Button::Down => absolute_event(AbsoluteAxis::Hat0Y, absolute_value),
                    Button::Left => absolute_event(AbsoluteAxis::Hat0X, -absolute_value),
                    Button::Right => absolute_event(AbsoluteAxis::Hat0X, absolute_value),
                };

                self.handle.write(&[
                    event,
                    *SynchronizeEvent::new(EventTime::default(), SynchronizeKind::Report, 0)
                        .as_event()
                        .as_raw(),
                ])?;
            }
            InputMessage::CirclePadPosition(x, y) => {
                let pro_x = (x as f32 / N3DS_CPAD_AXES_LIMIT as f32
                    * PRO_CONTROLLER_LEFT_AXES_LIMIT as f32) as i32;
                // Note: the Y dimension is negated because the 3DS has an inverted CPAD Y dimension.
                let pro_y = -(y as f32 / N3DS_CPAD_AXES_LIMIT as f32
                    * PRO_CONTROLLER_LEFT_AXES_LIMIT as f32) as i32;

                self.handle.write(&[
                    *AbsoluteEvent::new(EventTime::default(), AbsoluteAxis::X, pro_x)
                        .as_event()
                        .as_raw(),
                    *AbsoluteEvent::new(EventTime::default(), AbsoluteAxis::Y, pro_y)
                        .as_event()
                        .as_raw(),
                    *SynchronizeEvent::new(EventTime::default(), SynchronizeKind::Report, 0)
                        .as_event()
                        .as_raw(),
                ])?;
            }
        }

        Ok(())
    }
}
