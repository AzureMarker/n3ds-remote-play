use crate::virtual_device::{VirtualDevice, VirtualDeviceFactory};
use futures::{SinkExt, StreamExt};
use image::buffer::ConvertBuffer;
use n3ds_remote_play_common::InputState;
use std::num::NonZeroU32;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::net::{TcpListener, TcpStream, UdpSocket};
use tokio::select;
use tokio::sync::oneshot::error::TryRecvError;
use tokio::task::{spawn_local, LocalSet};
use tokio_serde::formats::SymmetricalBincode;
use tokio_serde::SymmetricallyFramed;
use tokio_stream::wrappers::TcpListenerStream;
use tokio_util::codec::{FramedRead, LengthDelimitedCodec};
use tokio_util::udp::UdpFramed;

mod virtual_device;

const N3DS_TOP_WIDTH: u32 = 400;
const N3DS_TOP_HEIGHT: u32 = 240;

fn main() {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let local_set = LocalSet::new();

    local_set.block_on(&runtime, async_main());
}

async fn async_main() {
    println!("Starting n3ds-remote-play server on 0.0.0.0:3535");
    let tcp_listener = TcpListener::bind(("0.0.0.0", 3535))
        .await
        .expect("Failed to bind address");
    let mut tcp_stream = TcpListenerStream::new(tcp_listener);
    let udp_socket = UdpSocket::bind(("0.0.0.0", 3535))
        .await
        .expect("Failed to bind UDP socket");
    let udp_socket = Arc::new(udp_socket);
    let device_factory = Arc::new(
        virtual_device::new_device_factory().expect("Failed to create virtual device factory"),
    );

    while let Some(connection) = tcp_stream.next().await {
        match connection {
            Ok(connection) => {
                spawn_local(handle_connection(
                    connection,
                    Arc::clone(&udp_socket),
                    Arc::clone(&device_factory),
                ));
            }
            Err(e) => {
                eprintln!("New connection error: {e}");
                continue;
            }
        }
    }
}

async fn handle_connection(
    tcp_stream: TcpStream,
    udp_socket: Arc<UdpSocket>,
    device_factory: Arc<impl VirtualDeviceFactory>,
) {
    let peer_addr = match tcp_stream.peer_addr() {
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
        FramedRead::new(tcp_stream, LengthDelimitedCodec::new()),
        SymmetricalBincode::<InputState>::default(),
    );
    let mut frame_sink = UdpFramed::new(udp_socket, LengthDelimitedCodec::new());
    println!("Created input stream and frame sink");

    let display = scrap::Display::primary().unwrap();
    let mut display_capturer = scrap::Capturer::new(display).unwrap();
    let (start_sender, start_receiver) = tokio::sync::oneshot::channel();
    let mut start_sender = Some(start_sender);
    let (exit_sender, mut exit_receiver) = tokio::sync::oneshot::channel();
    let display_task_handle = spawn_local(async move {
        let width = NonZeroU32::new(display_capturer.width() as u32).unwrap();
        let height = NonZeroU32::new(display_capturer.height() as u32).unwrap();
        let mut resizer = fast_image_resize::Resizer::new(fast_image_resize::ResizeAlg::Nearest);

        // Wait for the 3DS to connect
        if let Err(e) = start_receiver.await {
            eprintln!("Got an error in display task while waiting for 3DS to start: {e}");
            return;
        }
        println!("Starting to send frames");

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
                    let frame_processing_start = Instant::now();
                    // let original_frame = image::RgbaImage::from_vec(width.get(), height.get(), frame.to_vec()).unwrap();
                    // let mut tmp_original_file = BufWriter::new(File::create("test-original.png").unwrap());
                    // original_frame.write_to(&mut tmp_original_file, image::ImageOutputFormat::Png).unwrap();

                    // First resize the image to fit the 3DS screen
                    let frame_view = fast_image_resize::DynamicImageView::U8x4(
                        fast_image_resize::ImageView::from_buffer(width, height, &frame).unwrap(),
                    );
                    let mut resized_frame = fast_image_resize::Image::new(
                        NonZeroU32::new(N3DS_TOP_WIDTH).unwrap(),
                        NonZeroU32::new(N3DS_TOP_HEIGHT).unwrap(),
                        fast_image_resize::PixelType::U8x4,
                    );

                    resizer
                        .resize(&frame_view, &mut resized_frame.view_mut())
                        .unwrap();

                    // Convert from BGRA to BGR and rotate since the 3DS top screen is actually in portrait.
                    let resized_frame = image::RgbaImage::from_vec(
                        N3DS_TOP_WIDTH,
                        N3DS_TOP_HEIGHT,
                        resized_frame.into_vec(),
                    )
                    .unwrap();
                    let resized_frame_bgr: image::RgbImage = resized_frame.convert();
                    let rotated_frame = image::imageops::rotate90(&resized_frame_bgr);
                    // let mut tmp_file = BufWriter::new(File::create("test.jpeg").unwrap());
                    // rotated_frame.write_to(&mut tmp_file, image::ImageOutputFormat::Jpeg).unwrap();

                    let mut jpeg_frame = Vec::new();
                    let mut jpeg_encoder = image::codecs::jpeg::JpegEncoder::new(&mut jpeg_frame);
                    jpeg_encoder.encode_image(&rotated_frame).unwrap();

                    let frame_processing_end = Instant::now();
                    println!(
                        "Encoded image with size {} to JPEG with size {}",
                        rotated_frame.len(),
                        jpeg_frame.len()
                    );

                    // Send frame to the 3DS
                    if let Err(e) = frame_sink.send((jpeg_frame.into(), peer_addr)).await {
                        eprintln!("Got error in display task while sending frame: {e}");
                        if e.kind() == std::io::ErrorKind::ConnectionReset {
                            break;
                        }
                    }
                    let frame_send_end = Instant::now();
                    let frame_processing_duration =
                        frame_processing_end.duration_since(frame_processing_start);
                    let frame_send_duration = frame_send_end.duration_since(frame_processing_end);
                    println!("Sent frame. Processing: {frame_processing_duration:?}, Sending: {frame_send_duration:?}");

                    // Sleep to limit the frame rate (10 fps for now)
                    select! {
                        _ = tokio::time::sleep(Duration::from_millis(100)) => {}
                        _ = &mut exit_receiver => {
                            // Regardless of the result (Ok or Err) we should exit
                            break;
                        }
                    }
                }
                Err(e) => {
                    if e.kind() == std::io::ErrorKind::WouldBlock {
                        tokio::task::yield_now().await;
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
        // Start sending frames
        if let Some(sender) = start_sender.take() {
            let _ = sender.send(());
        }

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
    let _ = exit_sender.send(());
    display_task_handle.await.unwrap();
}
