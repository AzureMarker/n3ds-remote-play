// We don't directly use tracing, but some dependencies do.
// We depend on it directly in order to enable the `log-always` feature,
// which ensures that events are sent to the `log` crate as well.
extern crate tracing;

mod virtual_device;

use crate::virtual_device::{VirtualDevice, VirtualDeviceFactory};
use bincode::Options;
use futures::StreamExt;
use image::buffer::ConvertBuffer;
use n3ds_remote_play_common::InputState;
use rtp_types::RtpPacketBuilder;
use std::collections::HashMap;
use std::net::SocketAddr;
use std::num::NonZeroU32;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::io::AsyncWriteExt;
use tokio::net::{TcpListener, TcpStream, UdpSocket};
use tokio::sync::RwLock;
use tokio::task::{LocalSet, spawn_blocking, spawn_local};
use tokio::time::sleep;
use tokio::{select, spawn};
use tokio_stream::wrappers::TcpListenerStream;
use tracing::level_filters::LevelFilter;
use tracing::{debug, error, info, trace, warn};
use tracing_subscriber::FmtSubscriber;
use ffmpeg_next as ffmpeg;

const CLIENT_CONNECTION_TIMEOUT: Duration = Duration::from_secs(30);
const N3DS_TOP_WIDTH: u32 = 400;
const N3DS_TOP_HEIGHT: u32 = 240;

fn main() {
    FmtSubscriber::builder()
        .with_max_level(LevelFilter::DEBUG)
        .init();

    // Initialize ffmpeg
    ffmpeg::init().expect("Failed to initialize ffmpeg");

    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let local_set = LocalSet::new();

    local_set.block_on(&runtime, async_main());
}

async fn async_main() {
    info!("Starting n3ds-remote-play server on 0.0.0.0:3535");
    let tcp_listener = TcpListener::bind(("0.0.0.0", 3535))
        .await
        .expect("Failed to bind address");
    let mut tcp_stream = TcpListenerStream::new(tcp_listener);
    let udp_socket = UdpSocket::bind(("0.0.0.0", 3535))
        .await
        .expect("Failed to bind UDP socket");
    let device_factory = Arc::new(
        virtual_device::new_device_factory().expect("Failed to create virtual device factory"),
    );

    let input_map = Arc::new(RwLock::new(HashMap::new()));
    let (exit_sender, exit_receiver) = tokio::sync::oneshot::channel::<()>();
    let udp_input_mapper = spawn(input_mapper(
        udp_socket,
        Arc::clone(&input_map),
        exit_receiver,
    ));

    info!("Server started, waiting for connections");
    while let Some(connection) = tcp_stream.next().await {
        match connection {
            Ok(connection) => {
                spawn_local(handle_connection(
                    connection,
                    Arc::clone(&input_map),
                    Arc::clone(&device_factory),
                ));
            }
            Err(e) => {
                error!("New connection error: {e}");
                continue;
            }
        }
    }

    info!("Server shutting down");
    exit_sender.send(()).ok();
    udp_input_mapper.await.ok();
    info!("Server shut down");
}

async fn input_mapper(
    udp_socket: UdpSocket,
    input_map: Arc<RwLock<HashMap<SocketAddr, tokio::sync::mpsc::UnboundedSender<InputState>>>>,
    mut exit_receiver: tokio::sync::oneshot::Receiver<()>,
) {
    let mut buffer = vec![0; 1024];
    loop {
        let packet = select! {
            _ = &mut exit_receiver => {
                debug!("UDP input mapper exiting");
                break;
            }
            packet = udp_socket.recv_from(&mut buffer) => packet
        };

        let Ok((size, src_addr)) = packet.inspect_err(|e| {
            error!("Error while receiving UDP packet: {e}");
        }) else {
            continue;
        };
        trace!("Received UDP input packet of size {size} from [{src_addr}]");

        // Look up the input channel for this client
        let input_map = input_map.read().await;
        let Some(sender) = input_map.get(&src_addr) else {
            warn!("Received UDP input packet from unknown address [{src_addr}]");
            continue;
        };

        // Deserialize the input state
        let bincode_options = bincode::DefaultOptions::new();
        let Ok(input_state) = bincode_options
            .deserialize::<InputState>(&buffer[..size])
            .inspect_err(|e| {
                error!("Error while deserializing input state from [{src_addr}]: {e}");
            })
        else {
            continue;
        };

        // Send the input state to the corresponding client handler
        if let Err(e) = sender.send(input_state) {
            error!("Error while sending input state to TCP handler for [{src_addr}]: {e}");
        }
    }
}

async fn handle_connection(
    mut tcp_stream: TcpStream,
    input_map: Arc<RwLock<HashMap<SocketAddr, tokio::sync::mpsc::UnboundedSender<InputState>>>>,
    device_factory: Arc<impl VirtualDeviceFactory>,
) {
    let peer_addr = match tcp_stream.peer_addr() {
        Ok(peer_addr) => peer_addr,
        Err(e) => {
            error!("Error while getting peer address: {e}");
            return;
        }
    };
    info!("New connection from {peer_addr}");

    let mut device = match device_factory.new_device().await {
        Ok(device) => device,
        Err(e) => {
            error!("Closing connection with [{peer_addr}] due to error:\n{e:?}");
            return;
        }
    };
    debug!("Created uinput device");

    let (input_sender, mut input_receiver) = tokio::sync::mpsc::unbounded_channel::<InputState>();
    let mut input_map_guard = input_map.write().await;
    input_map_guard.insert(peer_addr, input_sender);
    drop(input_map_guard);
    debug!("Created input stream");

    let (_tcp_reader, mut tcp_writer) = tcp_stream.split();

    // Set up display capture
    let displays = xcap::Monitor::all().unwrap();
    let display = displays.into_iter().next().unwrap();
    let (display_video_recorder, video_stream) = display.video_recorder().unwrap();
    display_video_recorder.start().unwrap();
    let (frame_sender, mut frame_receiver) = tokio::sync::mpsc::channel(1);

    let (start_sender, start_receiver) = tokio::sync::oneshot::channel();
    let mut start_sender = Some(start_sender);
    let (exit_sender, mut exit_receiver) = tokio::sync::oneshot::channel();
    let display_task_handle = spawn_local(async move {
        let width = NonZeroU32::new(display.width().unwrap()).unwrap();
        let height = NonZeroU32::new(display.height().unwrap()).unwrap();
        let mut resizer = fast_image_resize::Resizer::new(fast_image_resize::ResizeAlg::Nearest);

        // Wait for the 3DS to connect
        if let Err(e) = start_receiver.await {
            error!("Got an error in display task while waiting for 3DS to start: {e}");
            return;
        }
        info!("Starting to send frames");

        // Convert the video stream into a Tokio-compatible stream
        let (raw_video_sender, mut raw_video_receiver) = tokio::sync::mpsc::channel(1);
        let video_future = spawn_blocking(move || {
            for frame in video_stream {
                if let Err(e) = raw_video_sender.blocking_send(frame) {
                    error!("Error while sending captured frame to Tokio task: {e}");
                    break;
                }
            }
        });

        // Create MPEG-1 encoder using ffmpeg (rotated dimensions since 3DS top screen is portrait)
        let codec = ffmpeg::encoder::find(ffmpeg::codec::Id::MPEG1VIDEO)
            .expect("Failed to find MPEG1 codec");

        let mut encoder = ffmpeg::codec::context::Context::new_with_codec(codec)
            .encoder()
            .video()
            .expect("Failed to create video encoder");

        encoder.set_width(N3DS_TOP_HEIGHT);
        encoder.set_height(N3DS_TOP_WIDTH);
        encoder.set_format(ffmpeg::format::Pixel::RGB24);
        encoder.set_frame_rate(Some((5, 1)));
        encoder.set_time_base((1, 5));
        encoder.set_bit_rate(500000); // 500 kbps
        encoder.set_gop(12); // Group of pictures size

        let mut encoder = encoder.open_as(codec).expect("Failed to open encoder");

        let mut frame_index: i64 = 0;

        let mut next_frame_time = Instant::now();
        let mut sequence_number: u16 = 0;
        let mut got_frame_last_time = false;

        'outer: loop {
            let mut last_frame = None;
            loop {
                // Sleep to limit the frame rate (5 fps for now)
                select! {
                    biased;
                    _ = &mut exit_receiver => {
                        // Regardless of the result (Ok or Err) we should exit
                        break 'outer;
                    }
                    _ = tokio::time::sleep_until(tokio::time::Instant::from_std(next_frame_time)) => {
                        next_frame_time += Duration::from_millis(200);
                        break;
                    }
                    frame = raw_video_receiver.recv() => {
                        // Drain the video stream to avoid buffering
                        if frame.is_none() {
                            warn!("Video stream ended");
                            break 'outer;
                        }
                        last_frame = frame;
                        got_frame_last_time = true;
                    }
                }
            }

            match last_frame {
                Some(frame) => {
                    let frame_processing_start = Instant::now();
                    // let original_frame =
                    //     image::RgbaImage::from_vec(width.get(), height.get(), frame.raw.clone())
                    //         .unwrap();
                    // let mut tmp_original_file =
                    //     BufWriter::new(File::create("test-original.png").unwrap());
                    // original_frame
                    //     .write_to(&mut tmp_original_file, image::ImageOutputFormat::Png)
                    //     .unwrap();

                    // First resize the image to fit the 3DS screen
                    let frame_view = fast_image_resize::DynamicImageView::U8x4(
                        fast_image_resize::ImageView::from_buffer(width, height, &frame.raw)
                            .unwrap(),
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

                    // let mut jpeg_frame = Vec::new();
                    // let mut jpeg_encoder =
                    //     image::codecs::jpeg::JpegEncoder::new_with_quality(&mut jpeg_frame, 80);
                    // jpeg_encoder.encode_image(&rotated_frame).unwrap();

                    // Encode frame to MPEG-1 using ffmpeg
                    let mut video_frame = ffmpeg::util::frame::Video::new(
                        ffmpeg::format::Pixel::BGR24,
                        N3DS_TOP_HEIGHT,
                        N3DS_TOP_WIDTH,
                    );

                    // Copy RGB data to ffmpeg frame
                    let frame_data = rotated_frame.as_raw();
                    video_frame.data_mut(0)[..frame_data.len()].copy_from_slice(frame_data);
                    video_frame.set_pts(Some(frame_index));
                    frame_index += 1;

                    // Encode the frame
                    let mut mpeg_packets = Vec::new();

                    if encoder.send_frame(&video_frame).is_ok() {
                        loop  {
                            let mut encoded_packet = ffmpeg::Packet::empty();
                            if encoder.receive_packet(&mut encoded_packet).is_ok() {
                                mpeg_packets.push(encoded_packet);
                            } else {
                                break;
                            }
                        }
                    }

                    let frame_processing_end = Instant::now();
                    let mpeg_packet_count = mpeg_packets.len();
                    let mpeg_data_size: usize = mpeg_packets.iter().map(ffmpeg::Packet::size).sum();

                    let frame_processing_duration =
                        frame_processing_end.duration_since(frame_processing_start);
                    debug!(
                        "Processed frame with packet count {mpeg_packet_count} and total size {mpeg_data_size:5}. Processing: {frame_processing_duration:.1?}"
                    );

                    // Send frame to the 3DS
                    // if let Err(e) = send_mjpeg_frame(
                    //     &udp_socket,
                    //     peer_addr,
                    //     &jpeg_frame,
                    //     &mut sequence_number,
                    //     0,
                    // )
                    // .await
                    // if let Err(e) = tcp_writer.write_all(&jpeg_frame).await
                    // {
                    //     error!("Got error in display task while sending frame: {e}");
                    //     // if let Some(e) = e.downcast_ref::<std::io::Error>()
                    //     //     && e.kind() == std::io::ErrorKind::ConnectionReset
                    //     if e.kind() == std::io::ErrorKind::ConnectionReset
                    //     {
                    //         break;
                    //     }
                    // }
                    for packet in mpeg_packets {
                        if let Err(e) = frame_sender.send(packet.data().unwrap().to_vec()).await {
                            error!("Error while sending frame to main task: {e}");
                            break;
                        }
                    }
                    // let frame_send_end = Instant::now();
                    // let frame_processing_duration = frame_processing_end
                    //     .duration_since(frame_processing_start)
                    //     .as_millis();
                    // let frame_send_duration = frame_send_end.duration_since(frame_processing_end);
                    // info!(
                    //     "Sent frame with size {:5}. Processing: {frame_processing_duration:3?}ms, Sending: {frame_send_duration:9?}",
                    //     jpeg_frame_size
                    // );
                }
                None => {
                    // if e.kind() == std::io::ErrorKind::WouldBlock {
                    //     tokio::task::yield_now().await;
                    //     continue;
                    // }
                    // if e == TryRecvError::Empty {
                    //     tokio::task::yield_now().await;
                    //     continue;
                    // }
                    //
                    // error!("Error while capturing frame: {e}");
                    if got_frame_last_time {
                        debug!("Stopped receiving frames from video stream");
                    }
                    got_frame_last_time = false;
                }
            }
        }

        debug!("Display task exiting");
        display_video_recorder
            .stop()
            .inspect_err(|e| {
                error!("Error while stopping display video recorder: {e}");
            })
            .ok();
        let mut input_map_guard = input_map.write().await;
        input_map_guard.remove(&peer_addr);
    });
    debug!("Spawned display capture task");

    loop {
        select! {
            stream_item = input_receiver.recv() => {
                let Some(input_state) = stream_item else {
                    info!("Input receiver channel closed for [{peer_addr}]");
                    break;
                };

                if let Some(sender) = start_sender.take() {
                    let _ = sender.send(());
                }

                // info!("[{peer_addr}] {input_state:?}");
                device.emit_input(input_state).unwrap();
            }
            frame = frame_receiver.recv() => {
                match frame {
                    Some(frame) => {
                        let frame_send_start = Instant::now();

                        // Prepend the frame size as a big-endian u32
                        let mpeg_frame_size = frame.len() as u32;
                        let mut full_frame = Vec::new();
                        full_frame.extend_from_slice(&mpeg_frame_size.to_be_bytes());
                        full_frame.extend_from_slice(&frame);

                        // Send the frame over TCP
                        if let Err(e) = tcp_writer.write_all(&full_frame).await {
                            error!("Got error in display task while sending frame: {e}");
                            if e.kind() == std::io::ErrorKind::ConnectionReset {
                                break;
                            }
                        }

                        let frame_send_end = Instant::now();
                        let frame_send_duration = frame_send_end.duration_since(frame_send_start);
                        debug!("Sent frame with size {mpeg_frame_size:5}. Sending: {frame_send_duration:.1?}");
                    }
                    None => {
                        error!("Frame receiver channel closed");
                        break;
                    }
                }
            }
            _ = tokio::time::sleep(CLIENT_CONNECTION_TIMEOUT) => {
                error!("Timed out while waiting for [{peer_addr}] to send an input packet ({CLIENT_CONNECTION_TIMEOUT:?})");
                break;
            }
        }
    }

    info!("Closing connection with [{peer_addr}]");
    let _ = exit_sender.send(());
    frame_receiver.close();
    display_task_handle.await.unwrap();
    info!("Closed connection with [{peer_addr}]");
}

const MTU: usize = 2500; // Maximum Transmission Unit size
const RTP_MJPEG_PAYLOAD_TYPE: u8 = 26; // Standard payload type for JPEG

async fn send_mjpeg_frame(
    socket: &UdpSocket,
    destination: SocketAddr,
    frame_data: &[u8],
    sequence_number: &mut u16,
    timestamp: u32,
) -> anyhow::Result<()> {
    let mut cursor = 0;
    let data_len = frame_data.len();

    while cursor < data_len {
        let chunk_size = std::cmp::min(MTU, data_len - cursor);
        let end = cursor + chunk_size;
        let payload = &frame_data[cursor..end];

        // The marker bit is set to 1 for the last packet of a frame
        let marker = end == data_len;

        // *** CRITICAL STEP: RFC 2435 JPEG Header ***
        // You must manually add a 4-byte JPEG header as the first part of the payload.
        // This includes "Type", "Q", "Width", and "Height" info.
        // For simplicity here, we assume standard parameters and attach the raw JPEG slice after it.
        // For a full implementation, you need to parse the JPEG header to populate these fields correctly.

        let jpeg_header = [0u8; 4]; // Placeholder: replace with actual RFC 2435 compliant header bytes

        let packet_builder = RtpPacketBuilder::new()
            .payload_type(RTP_MJPEG_PAYLOAD_TYPE)
            .sequence_number(*sequence_number)
            .timestamp(timestamp)
            .marker_bit(marker)
            .payload(jpeg_header.as_slice())
            .payload(payload);
        debug!(
            "Sending RTP packet seq={}, marker={}",
            *sequence_number, marker
        );

        // Send the packet
        let bytes = packet_builder.write_vec()?;
        socket.send_to(&bytes, destination).await?;

        // Add a small sleep to avoid overwhelming the 3DS
        sleep(Duration::from_millis(10)).await;

        cursor = end;
        *sequence_number += 1;
    }

    Ok(())
}
