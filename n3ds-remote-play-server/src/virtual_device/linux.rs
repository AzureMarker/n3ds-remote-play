use crate::virtual_device::{VirtualDevice, VirtualDeviceFactory};
use async_trait::async_trait;
use input_linux::{
    AbsoluteAxis, AbsoluteEvent, AbsoluteInfo, AbsoluteInfoSetup, EventKind, EventTime, InputId,
    Key, KeyEvent, KeyState, SynchronizeEvent, SynchronizeKind, UInputHandle,
};
use n3ds_remote_play_common::{InputState, KeyPad};
use tokio::fs::{File, OpenOptions};

const PRO_CONTROLLER_AXES_LIMIT: i32 = 32767;
const N3DS_CPAD_AXES_LIMIT: i32 = 156;

pub struct UInputDeviceFactory;

#[async_trait]
impl VirtualDeviceFactory for UInputDeviceFactory {
    type Device = UInputDevice;

    async fn new_device(&self) -> anyhow::Result<Self::Device> {
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
            minimum: -PRO_CONTROLLER_AXES_LIMIT,
            maximum: PRO_CONTROLLER_AXES_LIMIT,
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

        Ok(UInputDevice {
            handle,
            last_input_state: InputState::default(),
            // Initial guesses
            c_stick_range_x: (1500, 2500),
            c_stick_range_y: (1500, 2500),
        })
    }
}

pub struct UInputDevice {
    handle: UInputHandle<File>,
    last_input_state: InputState,
    c_stick_range_x: (u16, u16),
    c_stick_range_y: (u16, u16),
}

impl VirtualDevice for UInputDevice {
    fn emit_input(&mut self, input_state: InputState) -> anyhow::Result<()> {
        let event_time = EventTime::default();
        let key_event =
            |key: Key, state: KeyState| *KeyEvent::new(event_time, key, state).as_event().as_raw();
        let absolute_event = |axis: AbsoluteAxis, value: i32| {
            *AbsoluteEvent::new(event_time, axis, value)
                .as_event()
                .as_raw()
        };

        // Calculate the new pressed and released keys
        let new_keys_pressed = input_state
            .key_pad
            .difference(self.last_input_state.key_pad);
        let new_keys_released = self
            .last_input_state
            .key_pad
            .difference(input_state.key_pad);

        // Create the events list
        let mut events = Vec::with_capacity(
            new_keys_pressed.bits().count_ones() as usize
                + new_keys_released.bits().count_ones() as usize
                // 2 for each axis, 1 for synchronization
                + 5,
        );

        // Add button events
        let mut add_events_for_key = |input_key: KeyPad, uinput_key: Key| {
            if new_keys_pressed.contains(input_key) {
                events.push(key_event(uinput_key, KeyState::PRESSED));
            } else if new_keys_released.contains(input_key) {
                events.push(key_event(uinput_key, KeyState::RELEASED));
            }
        };
        add_events_for_key(KeyPad::KEY_A, Key::ButtonEast);
        add_events_for_key(KeyPad::KEY_B, Key::ButtonSouth);
        add_events_for_key(KeyPad::KEY_X, Key::ButtonNorth);
        add_events_for_key(KeyPad::KEY_Y, Key::ButtonWest);
        add_events_for_key(KeyPad::KEY_L, Key::ButtonTL);
        add_events_for_key(KeyPad::KEY_R, Key::ButtonTR);
        add_events_for_key(KeyPad::KEY_ZL, Key::ButtonTL2);
        add_events_for_key(KeyPad::KEY_ZR, Key::ButtonTR2);
        add_events_for_key(KeyPad::KEY_START, Key::ButtonStart);
        add_events_for_key(KeyPad::KEY_SELECT, Key::ButtonSelect);

        // Add D-pad events
        let mut add_events_for_dpad = |input_key: KeyPad, axis: AbsoluteAxis, value: i32| {
            if new_keys_pressed.contains(input_key) {
                events.push(absolute_event(axis, value));
            } else if new_keys_released.contains(input_key) {
                events.push(absolute_event(axis, 0));
            }
        };
        add_events_for_dpad(KeyPad::KEY_DUP, AbsoluteAxis::Hat0Y, -1);
        add_events_for_dpad(KeyPad::KEY_DDOWN, AbsoluteAxis::Hat0Y, 1);
        add_events_for_dpad(KeyPad::KEY_DLEFT, AbsoluteAxis::Hat0X, -1);
        add_events_for_dpad(KeyPad::KEY_DRIGHT, AbsoluteAxis::Hat0X, 1);

        // Add circle pad events
        let mut add_events_for_axis = |axis: AbsoluteAxis, value: i32| {
            events.push(
                *AbsoluteEvent::new(event_time, axis, value)
                    .as_event()
                    .as_raw(),
            );
        };

        if input_state.circle_pad != self.last_input_state.circle_pad {
            let pro_x = (input_state.circle_pad.x as f32 / N3DS_CPAD_AXES_LIMIT as f32
                * PRO_CONTROLLER_AXES_LIMIT as f32) as i32;
            // Note: the Y dimension is negated because the 3DS has an inverted C-pad Y dimension.
            let pro_y = -(input_state.circle_pad.y as f32 / N3DS_CPAD_AXES_LIMIT as f32
                * PRO_CONTROLLER_AXES_LIMIT as f32) as i32;

            add_events_for_axis(AbsoluteAxis::X, pro_x);
            add_events_for_axis(AbsoluteAxis::Y, pro_y);
        }

        // Add C stick events
        if input_state.c_stick != self.last_input_state.c_stick {
            // Auto-adjust C stick calibration
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

            let x_range = self.c_stick_range_x.1 - self.c_stick_range_x.0;
            let y_range = self.c_stick_range_y.1 - self.c_stick_range_y.0;

            let x_percent =
                (input_state.c_stick.x - self.c_stick_range_x.0) as f32 / x_range as f32;
            let pro_x = ((x_percent - 0.5) * 2.0 * PRO_CONTROLLER_AXES_LIMIT as f32) as i32;
            // Note: the Y dimension is negated because the 3DS has an inverted C-stick Y dimension.
            let y_percent =
                (input_state.c_stick.y - self.c_stick_range_y.0) as f32 / y_range as f32;
            let pro_y = -((y_percent - 0.5) * 2.0 * PRO_CONTROLLER_AXES_LIMIT as f32) as i32;

            add_events_for_axis(AbsoluteAxis::RX, pro_x);
            add_events_for_axis(AbsoluteAxis::RY, pro_y);
        }

        // Publish the events
        events.push(
            *SynchronizeEvent::new(EventTime::default(), SynchronizeKind::Report, 0)
                .as_event()
                .as_raw(),
        );

        self.handle.write(&events)?;
        self.last_input_state = input_state;

        Ok(())
    }
}
