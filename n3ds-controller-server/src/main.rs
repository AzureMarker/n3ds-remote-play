use anyhow::Context;
use n3ds_controller_common::{Button, ButtonAction, InputMessage};
use std::error::Error;
use tokio::io::BufReader;
use tokio::net::{TcpListener, TcpStream};
use tokio_serde::formats::SymmetricalBincode;
use tokio_serde::SymmetricallyFramed;
use tokio_stream::wrappers::TcpListenerStream;
use tokio_stream::StreamExt;
use tokio_util::codec::{FramedRead, LengthDelimitedCodec};
use uinput_tokio::event::controller::{DPad, GamePad};
use uinput_tokio::event::Controller;

#[tokio::main]
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

async fn create_device() -> anyhow::Result<uinput_tokio::Device> {
    uinput_tokio::default()
        .context("Failed to create uinput device builder")?
        .name("test")
        .context("Failed to set device name")?
        .event(Controller::All)
        .context("Failed to enable controller events for the device")?
        .create()
        .await
        .context("Failed to create uinput device")
}

async fn emit_input_action(
    message: InputMessage,
    device: &mut uinput_tokio::Device,
) -> Result<(), Box<dyn Error>> {
    match message {
        InputMessage::Button { action, button } => {
            let device_button = match button {
                Button::A => Controller::GamePad(GamePad::East),
                Button::B => Controller::GamePad(GamePad::South),
                Button::X => Controller::GamePad(GamePad::North),
                Button::Y => Controller::GamePad(GamePad::West),
                Button::L => Controller::GamePad(GamePad::TL),
                Button::R => Controller::GamePad(GamePad::TR),
                Button::Up => Controller::DPad(DPad::Up),
                Button::Down => Controller::DPad(DPad::Down),
                Button::Left => Controller::DPad(DPad::Left),
                Button::Right => Controller::DPad(DPad::Right),
                Button::Start => Controller::GamePad(GamePad::Start),
                Button::Select => Controller::GamePad(GamePad::Select),
            };

            match action {
                ButtonAction::Pressed => device.press(&device_button).await?,
                ButtonAction::Released => device.release(&device_button).await?,
            }
        }
    }

    Ok(())
}
