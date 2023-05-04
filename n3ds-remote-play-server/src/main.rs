use crate::virtual_device::{VirtualDevice, VirtualDeviceFactory};
use n3ds_remote_play_common::InputState;
use std::sync::Arc;
use tokio::io::BufReader;
use tokio::net::{TcpListener, TcpStream};
use tokio_serde::formats::SymmetricalBincode;
use tokio_serde::SymmetricallyFramed;
use tokio_stream::wrappers::TcpListenerStream;
use tokio_stream::StreamExt;
use tokio_util::codec::{FramedRead, LengthDelimitedCodec};

mod virtual_device;

#[tokio::main(flavor = "current_thread")]
async fn main() {
    println!("Starting n3ds-remote-play server on 0.0.0.0:3535");
    let tcp_listener = TcpListener::bind(("0.0.0.0", 3535))
        .await
        .expect("Failed to bind address");
    let mut tcp_stream = TcpListenerStream::new(tcp_listener);
    let device_factory = Arc::new(
        virtual_device::new_device_factory().expect("Failed to create virtual device factory"),
    );

    while let Some(connection) = tcp_stream.next().await {
        match connection {
            Ok(connection) => {
                tokio::spawn(handle_connection(connection, Arc::clone(&device_factory)));
            }
            Err(e) => {
                eprintln!("New connection error: {e}");
                continue;
            }
        }
    }
}

async fn handle_connection(tcp_stream: TcpStream, device_factory: Arc<impl VirtualDeviceFactory>) {
    let connection = BufReader::new(tcp_stream);
    let peer_addr = match connection.get_ref().peer_addr() {
        Ok(peer_addr) => peer_addr,
        Err(e) => {
            eprintln!("Error while getting peer address: {e}");
            return;
        }
    };

    println!("New connection from {peer_addr}");

    let mut device = match device_factory.new_device().await {
        Ok(device) => device,
        Err(e) => {
            eprintln!("Closing connection with [{peer_addr}] due to error:\n{e:?}");
            return;
        }
    };
    println!("Created uinput device");

    let mut input_stream = SymmetricallyFramed::new(
        FramedRead::new(connection, LengthDelimitedCodec::new()),
        SymmetricalBincode::<InputState>::default(),
    );

    while let Some(input_result) = input_stream.try_next().await.transpose() {
        match input_result {
            Ok(input_state) => {
                // println!("[{peer_addr}] {input_state:?}");
                device.emit_input(input_state).unwrap();
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
