mod virtual_device;

use crate::virtual_device::{VirtualDevice, VirtualDeviceFactory};
use bincode::Options;
use ffmpeg_next as ffmpeg;
use futures::StreamExt;
use n3ds_remote_play_common::InputState;
use n3ds_remote_play_common::rtp_mpeg::RTP_MPEG_PAYLOAD_TYPE;
use rtp_types::RtpPacketBuilder;
use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::net::{TcpListener, TcpStream, UdpSocket};
use tokio::sync::RwLock;
use tokio::task::{LocalSet, spawn_blocking, spawn_local};
use tokio::{select, spawn};
use tokio_stream::wrappers::TcpListenerStream;
use tracing::level_filters::LevelFilter;
use tracing::{debug, error, info, trace, warn};
use tracing_subscriber::FmtSubscriber;

const CLIENT_CONNECTION_TIMEOUT: Duration = Duration::from_secs(30);
const N3DS_TOP_WIDTH: u32 = 400;
const N3DS_TOP_HEIGHT: u32 = 240;

fn main() {
    FmtSubscriber::builder()
        .with_max_level(LevelFilter::DEBUG)
        .init();

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
    let udp_socket = Arc::new(udp_socket);

    let device_factory =
        virtual_device::new_device_factory().expect("Failed to create virtual device factory");

    let input_map = Arc::new(RwLock::new(HashMap::new()));
    let (exit_sender, exit_receiver) = tokio::sync::oneshot::channel::<()>();

    let udp_input_mapper = spawn(input_mapper(
        Arc::clone(&udp_socket),
        Arc::clone(&input_map),
        exit_receiver,
    ));

    info!("Server started, waiting for connections");
    while let Some(connection) = tcp_stream.next().await {
        match connection {
            Ok(connection) => {
                spawn_local(handle_connection(
                    connection,
                    Arc::clone(&udp_socket),
                    Arc::clone(&input_map),
                    device_factory.clone(),
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
    udp_socket: Arc<UdpSocket>,
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
    tcp_stream: TcpStream,
    udp_socket: Arc<UdpSocket>,
    input_map: Arc<RwLock<HashMap<SocketAddr, tokio::sync::mpsc::UnboundedSender<InputState>>>>,
    device_factory: impl VirtualDeviceFactory,
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

    // Set up display capture
    let displays = xcap::Monitor::all().unwrap();
    let display = displays.into_iter().next().unwrap();
    let (display_video_recorder, video_stream) = display.video_recorder().unwrap();
    display_video_recorder.start().unwrap();
    let (start_sender, start_receiver) = tokio::sync::oneshot::channel::<()>();
    let mut start_sender = Some(start_sender);
    let (exit_sender, mut exit_receiver) = tokio::sync::oneshot::channel();
    let display_task_handle = spawn_local(async move {
        let cap_w = display.width().unwrap();
        let cap_h = display.height().unwrap();

        // Wait for the 3DS to connect
        if let Err(e) = start_receiver.await {
            error!("Got an error in display task while waiting for 3DS to start: {e}");
            return;
        }
        info!("Starting to send frames");

        // Convert the video stream into a Tokio-compatible stream
        let (raw_video_sender, mut raw_video_receiver) = tokio::sync::mpsc::channel(1);
        let _video_future = spawn_blocking(move || {
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
        encoder.set_format(ffmpeg::format::Pixel::YUV420P);
        // MPEG-1 has a restricted set of supported framerates; For example, 5 fps = 5/1 is rejected.
        // We still *send* frames at EFFECTIVE_FPS by pacing the loop, but configure the encoder
        // to a supported rate (ENCODER_FPS_NUM) and set PTS in multi-frame steps.
        const ENCODER_FPS_NUM: i32 = 25;
        // Effective real send rate. Higher FPS reduces baseline 'frame interval' latency.
        // We still configure MPEG-1 to ENCODER_FPS_NUM (a supported rate) and step PTS accordingly.
        const EFFECTIVE_FPS: i64 = 25;
        encoder.set_frame_rate(Some((ENCODER_FPS_NUM, 1)));
        encoder.set_time_base((1, ENCODER_FPS_NUM));
        encoder.set_bit_rate(1_000_000); // 1000 kbps
        // Lower-latency settings:
        // - Smaller GOP => more frequent I-frames => recover faster from scene changes
        // - No B-frames => no frame reordering delay
        encoder.set_gop(20);
        encoder.set_max_b_frames(0);

        let mut encoder = encoder.open().expect("Failed to open encoder");

        // FFmpeg filtergraph pipeline:
        // RGBA capture -> scale to 400x240 -> transpose (90° clockwise) -> format=yuv420p
        // Resulting frame is 240x400 yuv420p, exactly what the MPEG-1 encoder supports.
        // NOTE: We intentionally allocate the *input* frame per iteration. `buffersrc.add()`
        // uses `av_buffersrc_add_frame()` which takes ownership of reference-counted buffers
        // and resets the input frame on success.
        let mut yuv420p = ffmpeg::util::frame::Video::new(
            ffmpeg::format::Pixel::YUV420P,
            N3DS_TOP_HEIGHT,
            N3DS_TOP_WIDTH,
        );

        let mut graph = ffmpeg::filter::Graph::new();
        let args = format!(
            "video_size={}x{}:pix_fmt=rgba:time_base=1/{}:pixel_aspect=1/1",
            cap_w, cap_h, EFFECTIVE_FPS,
        );

        let mut buffersrc = graph
            .add(&ffmpeg::filter::find("buffer").unwrap(), "in", &args)
            .expect("Failed to create buffer source");
        let mut buffersink = graph
            .add(&ffmpeg::filter::find("buffersink").unwrap(), "out", "")
            .expect("Failed to create buffer sink");
        buffersink.set_pixel_format(ffmpeg::format::Pixel::YUV420P);

        let chain = format!(
            "scale={}:{}:flags=bilinear,transpose=1,format=yuv420p",
            N3DS_TOP_WIDTH, N3DS_TOP_HEIGHT
        );

        graph
            .output("in", 0)
            .unwrap()
            .input("out", 0)
            .unwrap()
            .parse(&chain)
            .expect("Failed to parse filter graph");
        graph.validate().expect("Failed to validate filter graph");

        // Fractional PTS accumulator numerator (ticks * EFFECTIVE_FPS).
        // Each output frame advances pts_numer by ENCODER_FPS_I64, and pts is computed as pts_numer / EFFECTIVE_FPS.
        let mut pts_numerator: i64 = 0;

        let mut next_frame_time = Instant::now();
        let mut sequence_number: u16 = 0;
        let mut got_frame_last_time = false;

        'outer: loop {
            let mut last_frame = None;
            loop {
                // Sleep to limit the frame rate
                select! {
                    biased;
                    _ = &mut exit_receiver => {
                        // Regardless of the result (Ok or Err) we should exit
                        break 'outer;
                    }
                    _ = tokio::time::sleep_until(tokio::time::Instant::from_std(next_frame_time)) => {
                        next_frame_time += Duration::from_millis(1000 / EFFECTIVE_FPS as u64);
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

                    // PTS is in encoder time_base units (1/ENCODER_FPS_NUM). We advance it fractionally
                    // to match EFFECTIVE_FPS.
                    let pts = pts_numerator / EFFECTIVE_FPS;
                    pts_numerator = pts_numerator.saturating_add(ENCODER_FPS_NUM as i64);

                    // Capture is RGBA. Allocate a fresh frame, fill it, then submit it.
                    // The filter graph takes ownership of refcounted frame buffers.
                    let mut src_rgba =
                        ffmpeg::util::frame::Video::new(ffmpeg::format::Pixel::RGBA, cap_w, cap_h);
                    copy_packed_into_frame(&mut src_rgba, 0, cap_w, cap_h, 4, &frame.raw);
                    src_rgba.set_pts(Some(pts));

                    // Push through filter graph (scale+transpose+format)
                    buffersrc
                        .source()
                        .add(&src_rgba)
                        .expect("Failed to push frame into filter graph");

                    // Drain exactly one output frame for this input.
                    buffersink
                        .sink()
                        .frame(&mut yuv420p)
                        .expect("Failed to pull frame from filter graph");

                    // Keep PTS consistent through the graph and encoder.
                    yuv420p.set_pts(Some(pts));

                    // Encode the frame
                    let mut mpeg_packets = Vec::new();

                    encoder
                        .send_frame(&yuv420p)
                        .expect("Failed to send frame to encoder");
                    loop {
                        let mut encoded_packet = ffmpeg::Packet::empty();
                        match encoder.receive_packet(&mut encoded_packet) {
                            Ok(()) => {
                                mpeg_packets.push(encoded_packet);
                            }
                            Err(ffmpeg::Error::Other { errno }) if errno == ffmpeg::sys::EAGAIN => {
                                // No more packets available for now
                                break;
                            }
                            Err(e) => {
                                error!("Encoding error: {}", e);
                                break;
                            }
                        }
                    }

                    let frame_processing_end = Instant::now();
                    let mpeg_data_size: usize = mpeg_packets.iter().map(ffmpeg::Packet::size).sum();

                    let frame_processing_duration =
                        frame_processing_end.duration_since(frame_processing_start);

                    // Send encoded MPEG packets as RTP over UDP back to the client.
                    for packet in mpeg_packets {
                        let Some(payload) = packet.data() else {
                            continue;
                        };
                        let start_time = Instant::now();
                        match send_mpeg_rtp_packet(
                            &udp_socket,
                            peer_addr,
                            payload,
                            &mut sequence_number,
                        )
                        .await
                        {
                            Err(e) => {
                                error!(
                                    "Error while sending MPEG RTP packets to [{peer_addr}]: {e}"
                                );
                                break;
                            }
                            Ok(fragment_count) => {
                                let send_duration = start_time.elapsed();
                                debug!(
                                    "Sent {fragment_count} fragments for MPEG packet of size {mpeg_data_size} bytes in {send_duration:.1?}. Processing: {frame_processing_duration:.1?}",
                                );
                            }
                        }
                    }
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

                trace!("[{peer_addr}] {input_state:?}");
                device.emit_input(input_state).unwrap();
            }
            _ = tokio::time::sleep(CLIENT_CONNECTION_TIMEOUT) => {
                error!("Timed out while waiting for [{peer_addr}] to send an input packet ({CLIENT_CONNECTION_TIMEOUT:?})");
                break;
            }
        }
    }

    info!("Closing connection with [{peer_addr}]");
    let _ = exit_sender.send(());
    display_task_handle.await.unwrap();
    info!("Closed connection with [{peer_addr}]");
}

fn copy_packed_into_frame(
    dst: &mut ffmpeg::util::frame::Video,
    plane: usize,
    width: u32,
    height: u32,
    bytes_per_pixel: usize,
    src: &[u8],
) {
    let dst_stride = dst.stride(plane);
    let row_bytes = width as usize * bytes_per_pixel;
    debug_assert!(src.len() >= row_bytes * height as usize);

    for y in 0..height as usize {
        let src_off = y * row_bytes;
        let dst_off = y * dst_stride;
        dst.data_mut(plane)[dst_off..dst_off + row_bytes]
            .copy_from_slice(&src[src_off..src_off + row_bytes]);
    }
}

/// Maximum Transmission Unit size.
/// The 3DS has an MTU of 1400 bytes, but using a larger size seems to improve performance.
const MTU: usize = 2500;

/// RTP packetization for a single encoded MPEG packet.
///
/// We include a small fragment header so the client can reassemble the original MPEG packet:
/// - mpeg_len (u32 BE)
/// - frag_index (u16 BE)
/// - frag_count (u16 BE)
/// - frag_offset (u32 BE)
///
/// Returns the number of fragments sent.
async fn send_mpeg_rtp_packet(
    socket: &UdpSocket,
    destination: SocketAddr,
    mpeg_packet: &[u8],
    sequence_number: &mut u16,
) -> anyhow::Result<usize> {
    // Send to the client's UDP port equal to its TCP peer port.
    // (Client binds UDP to connection.local_addr(), so UDP uses the same port.)

    // Conservative max payload per RTP packet. We don't know the exact RTP header size here,
    // so reserve a little headroom.
    const RTP_HEADER_HEADROOM: usize = 64;
    const FRAG_HDR_LEN: usize = 12;
    let max_payload = MTU
        .saturating_sub(RTP_HEADER_HEADROOM)
        .saturating_sub(FRAG_HDR_LEN);
    let total_len = mpeg_packet.len();
    let frag_count = (total_len + max_payload - 1) / max_payload;
    let frag_count_u16: u16 = frag_count
        .try_into()
        .map_err(|_| anyhow::anyhow!("MPEG packet too large to fragment (frag_count overflow)"))?;

    // Use a single RTP sequence number to identify this MPEG packet.
    // All fragments for this MPEG packet share the same RTP sequence number.
    let mpeg_packet_seq = *sequence_number;

    for frag_index in 0..frag_count {
        let start = frag_index * max_payload;
        let end = std::cmp::min(start + max_payload, total_len);
        let frag_bytes = &mpeg_packet[start..end];

        let marker = frag_index + 1 == frag_count;

        let mpeg_len_be = (total_len as u32).to_be_bytes();
        let frag_index_be = (frag_index as u16).to_be_bytes();
        let frag_count_be = frag_count_u16.to_be_bytes();
        let frag_offset_be = (start as u32).to_be_bytes();

        let mut payload = Vec::with_capacity(FRAG_HDR_LEN + frag_bytes.len());
        payload.extend_from_slice(&mpeg_len_be);
        payload.extend_from_slice(&frag_index_be);
        payload.extend_from_slice(&frag_count_be);
        payload.extend_from_slice(&frag_offset_be);
        payload.extend_from_slice(frag_bytes);

        let packet_builder = RtpPacketBuilder::new()
            .payload_type(RTP_MPEG_PAYLOAD_TYPE)
            .sequence_number(mpeg_packet_seq)
            .marker_bit(marker)
            .payload(payload.as_slice());

        let bytes = packet_builder.write_vec()?;
        socket.send_to(&bytes, destination).await?;
    }

    // Advance to the next MPEG packet id.
    *sequence_number = sequence_number.wrapping_add(1);

    Ok(frag_count)
}
