use n3ds_controller_common::InputMessage;
use std::error::Error;
use tokio::io::BufReader;
use tokio::net::{TcpListener, TcpStream};
use tokio_serde::formats::SymmetricalBincode;
use tokio_serde::SymmetricallyFramed;
use tokio_stream::wrappers::TcpListenerStream;
use tokio_stream::StreamExt;
use tokio_util::codec::{FramedRead, LengthDelimitedCodec};

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

    // let device = match create_device().await {
    //     Ok(device) => device,
    //     Err(e) => {
    //         eprintln!("Closing connection with [{peer_addr}] due to error:\n{e}");
    //         return;
    //     }
    // };
    // println!("Created uinput device");

    let mut message_stream = SymmetricallyFramed::new(
        FramedRead::new(connection, LengthDelimitedCodec::new()),
        SymmetricalBincode::<InputMessage>::default(),
    );

    while let Some(message) = message_stream.try_next().await.transpose() {
        match message {
            Ok(message) => {
                println!("[{peer_addr}] {message:?}");
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

async fn create_device() -> Result<uinput_tokio::Device, Box<dyn Error>> {
    uinput_tokio::default()
        .map_err(|e| anyhow::Error::msg(e.to_string()))?
        .name("test")
        .map_err(|e| anyhow::Error::msg(e.to_string()))?
        .event(uinput_tokio::event::Controller::All)?
        .create()
        .await
}
