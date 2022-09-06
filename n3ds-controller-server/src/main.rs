use input_linux::{
    AbsoluteAxis, AbsoluteEvent, AbsoluteInfo, AbsoluteInfoSetup, EventKind, EventTime, InputId,
    Key, KeyEvent, KeyState, SynchronizeEvent, SynchronizeKind, UInputHandle,
};
use n3ds_controller_common::{Button, ButtonAction, InputMessage};
use std::error::Error;
use tokio::fs::{File, OpenOptions};
use tokio::io::BufReader;
use tokio::net::{TcpListener, TcpStream};
use tokio_serde::formats::SymmetricalBincode;
use tokio_serde::SymmetricallyFramed;
use tokio_stream::wrappers::TcpListenerStream;
use tokio_stream::StreamExt;
use tokio_util::codec::{FramedRead, LengthDelimitedCodec};

#[tokio::main(flavor = "current_thread")]
async fn main() {
    println!("Starting n3ds-controller server on 0.0.0.0:3535");
    let tcp_listener = TcpListener::bind(("0.0.0.0", 3535))
        .await
        .expect("Failed to bind address");
    let mut tcp_stream = TcpListenerStream::new(tcp_listener);

    while let Some(connection) = tcp_stream.next().await {
        match connection {
            Ok(connection) => {
                tokio::spawn(handle_connection(connection));
            }
            Err(e) => {
                eprintln!("New connection error: {e}");
                continue;
            }
        }
    }
}

async fn handle_connection(tcp_stream: TcpStream) {
    let connection = BufReader::new(tcp_stream);
    let peer_addr = match connection.get_ref().peer_addr() {
        Ok(peer_addr) => peer_addr,
        Err(e) => {
            eprintln!("Error while getting peer address: {e}");
            return;
        }
    };

    println!("New connection from {peer_addr}");

    let mut device = match create_device().await {
        Ok(device) => device,
        Err(e) => {
            eprintln!("Closing connection with [{peer_addr}] due to error:\n{e:?}");
            return;
        }
    };
    println!("Created uinput device");

    let mut message_stream = SymmetricallyFramed::new(
        FramedRead::new(connection, LengthDelimitedCodec::new()),
        SymmetricalBincode::<InputMessage>::default(),
    );

    while let Some(message) = message_stream.try_next().await.transpose() {
        match message {
            Ok(message) => {
                println!("[{peer_addr}] {message:?}");
                emit_input_action(message, &mut device).await.unwrap();
            }
            Err(e) => {
                eprintln!("Error while reading stream: {e}");
                if e.kind() == std::io::ErrorKind::ConnectionReset {
                    break;
                }
            }
        }
    }

    println!("Closing connection with [{peer_addr}]");
}

async fn create_device() -> anyhow::Result<UInputHandle<File>> {
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
    handle.set_keybit(Key::ButtonTL2)?;
    handle.set_keybit(Key::ButtonTR2)?;
    handle.set_keybit(Key::ButtonThumbl)?;
    handle.set_keybit(Key::ButtonThumbr)?;
    handle.set_keybit(Key::ButtonMode)?;
    handle.set_absbit(AbsoluteAxis::RX)?;
    handle.set_absbit(AbsoluteAxis::RY)?;

    let axis_absolute_info = AbsoluteInfo {
        minimum: -32767,
        maximum: 32767,
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

    Ok(handle)
}

async fn emit_input_action(
    message: InputMessage,
    device: &mut UInputHandle<File>,
) -> Result<(), Box<dyn Error>> {
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
                Button::Start => key_event(Key::ButtonStart, key_state),
                Button::Select => key_event(Key::ButtonSelect, key_state),
                Button::Up => absolute_event(AbsoluteAxis::Hat0Y, -absolute_value),
                Button::Down => absolute_event(AbsoluteAxis::Hat0Y, absolute_value),
                Button::Left => absolute_event(AbsoluteAxis::Hat0X, -absolute_value),
                Button::Right => absolute_event(AbsoluteAxis::Hat0X, absolute_value),
            };

            device.write(&[
                event,
                *SynchronizeEvent::new(EventTime::default(), SynchronizeKind::Report, 0)
                    .as_event()
                    .as_raw(),
            ])?;
        }
    }

    Ok(())
}
