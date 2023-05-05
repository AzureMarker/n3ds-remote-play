use crate::virtual_device::{VirtualDevice, VirtualDeviceFactory};
use futures::{SinkExt, StreamExt};
use n3ds_remote_play_common::InputState;
use std::num::NonZeroU32;
use std::sync::Arc;
use std::time::Duration;
use tokio::io::BufReader;
use tokio::net::{TcpListener, TcpStream};
use tokio::select;
use tokio::sync::oneshot::error::TryRecvError;
use tokio::task::spawn_local;
use tokio_serde::formats::SymmetricalBincode;
use tokio_serde::SymmetricallyFramed;
use tokio_stream::wrappers::TcpListenerStream;
use tokio_util::codec::{FramedRead, FramedWrite, LengthDelimitedCodec};

mod virtual_device;

// fn main() {
//     tokio::runtime::Builder::new_current_thread()
//         .enable_all()
//         .build()
//         .unwrap()
//         .block_on()
// }

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
                spawn_local(handle_connection(connection, Arc::clone(&device_factory)));
            }
            Err(e) => {
                eprintln!("New connection error: {e}");
                continue;
            }
        }
    }
}

async fn handle_connection(tcp_stream: TcpStream, device_factory: Arc<impl VirtualDeviceFactory>) {
    let peer_addr = match tcp_stream.peer_addr() {
        Ok(peer_addr) => peer_addr,
        Err(e) => {
            eprintln!("Error while getting peer address: {e}");
            return;
        }
    };
    println!("New connection from {peer_addr}");

    let (tcp_reader, tcp_writer) = tcp_stream.into_split();
    let tcp_reader = BufReader::new(tcp_reader);

    let mut device = match device_factory.new_device().await {
        Ok(device) => device,
        Err(e) => {
            eprintln!("Closing connection with [{peer_addr}] due to error:\n{e:?}");
            return;
        }
    };
    println!("Created uinput device");

    let mut input_stream = SymmetricallyFramed::new(
        FramedRead::new(tcp_reader, LengthDelimitedCodec::new()),
        SymmetricalBincode::<InputState>::default(),
    );
    let mut output_stream = SymmetricallyFramed::new(
        FramedWrite::new(tcp_writer, LengthDelimitedCodec::new()),
        SymmetricalBincode::<Vec<u8>>::default(),
    );
    println!("Created input and output streams");

    let display = scrap::Display::primary().unwrap();
    let mut display_capturer = scrap::Capturer::new(display).unwrap();
    let (exit_sender, mut exit_receiver) = tokio::sync::oneshot::channel();
    let display_task_handle = spawn_local(async move {
        let width = NonZeroU32::new(display_capturer.width() as u32).unwrap();
        let height = NonZeroU32::new(display_capturer.height() as u32).unwrap();
        let mut resizer = fast_image_resize::Resizer::new(fast_image_resize::ResizeAlg::Nearest);

        loop {
            match exit_receiver.try_recv() {
                Ok(()) | Err(TryRecvError::Closed) => {
                    println!("Exiting display task");
                    break;
                }
                _ => {}
            }

            match display_capturer.frame() {
                Ok(frame) => {
                    let frame_view = fast_image_resize::DynamicImageView::U8x4(
                        fast_image_resize::ImageView::from_buffer(width, height, &frame).unwrap(),
                    );
                    let mut resized_frame = fast_image_resize::Image::new(
                        NonZeroU32::new(400).unwrap(),
                        NonZeroU32::new(240).unwrap(),
                        fast_image_resize::PixelType::U8x4,
                    );

                    resizer
                        .resize(&frame_view, &mut resized_frame.view_mut())
                        .unwrap();

                    output_stream.send(resized_frame.into_vec()).await.unwrap();

                    // Sleep to limit the frame rate (10 fps for now)
                    select! {
                        _ = tokio::time::sleep(Duration::from_millis(100)) => {}
                        _ = &mut exit_receiver => {
                            // Regardless of the result (Ok or Err) we should exit
                            break
                        }
                    }
                }
                Err(e) => {
                    if e.kind() == std::io::ErrorKind::WouldBlock {
                        eprintln!("Got a WouldBlock in display thread (unexpected but OK)");
                        continue;
                    }

                    eprintln!("Error while capturing frame: {e}");
                    break;
                }
            }
        }
    });
    println!("Spawned display capture task");

    while let Some(input_result) = input_stream.next().await {
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
    exit_sender.send(()).unwrap();
    display_task_handle.await.unwrap();
}
