use bincode::Options;
use n3ds_remote_play_common::InputState;
use std::net::UdpSocket;
use std::sync::Arc;
use std::time::Duration;
use tokio::select;
use tokio_stream::StreamExt;
use tokio_util::bytes::BytesMut;
use tokio_util::codec::FramedRead;
use tokio_util::sync::CancellationToken;

/// The system core thread runs its own Tokio runtime to handle async tasks.
/// It handles sending input states over UDP and reading packets from TCP.
pub fn run_system_thread(
    udp_socket: UdpSocket,
    input_reader: tokio::sync::watch::Receiver<InputState>,
    packet_writer: std::sync::mpsc::SyncSender<(BytesMut, Duration)>,
    cancel_token: CancellationToken,
) {
    let async_runtime = tokio::runtime::Builder::new_current_thread()
        .enable_time()
        .build()
        .expect("Failed to create Tokio runtime");

    async_runtime.block_on(async {
        // Spawn tasks and wait for them to complete
        let mut join_set = tokio::task::JoinSet::new();
        let udp_socket = Arc::new(udp_socket);

        join_set.spawn(send_inputs(
            Arc::clone(&udp_socket),
            input_reader,
            cancel_token.clone(),
        ));
        join_set.spawn(receive_video_packets(
            udp_socket,
            packet_writer,
            cancel_token,
        ));

        while let Some(join_result) = join_set.join_next().await {
            if let Err(e) = join_result {
                log::error!("A task failed: {e}");
            }
        }
    });

    log::debug!("System thread exited");
}

/// Task to send input states over UDP
async fn send_inputs(
    udp_socket: Arc<UdpSocket>,
    mut input_reader: tokio::sync::watch::Receiver<InputState>,
    cancel_token: CancellationToken,
) {
    loop {
        // Use select to wait for either a new input state or cancellation
        let input_result = select! {
            biased;
            _ = cancel_token.cancelled() => break,
            input_result = input_reader.changed() => input_result,
        };

        match input_result {
            Ok(()) => {
                // Send input state to server
                let state = input_reader.borrow_and_update();
                if let Err(e) = send_input_state(&udp_socket, *state) {
                    log::error!("Failed to send input state: {e:#}");
                    break;
                }
            }
            Err(_) => {
                log::warn!("Input channel disconnected unexpectedly");
                break;
            }
        }
    }

    log::debug!("send_inputs task exited");
}

fn send_input_state(udp_socket: &UdpSocket, state: InputState) -> anyhow::Result<()> {
    log::trace!("Sending {state:?}");

    let bincode_options = bincode::DefaultOptions::new();
    let data = bincode_options.serialize(&state)?;
    udp_socket.send(&data)?;
    Ok(())
}

/// Task to receive video packets (RTP over UDP) using FramedRead + a custom Decoder.
async fn receive_video_packets(
    udp_socket: Arc<UdpSocket>,
    packet_writer: std::sync::mpsc::SyncSender<(BytesMut, Duration)>,
    cancel_token: CancellationToken,
) {
    // 1 MB buffer size to allow for holding multiple RTP packets as they are decoded
    const BUFFER_SIZE: usize = 1024 * 1024;
    let mut packet_reader = FramedRead::with_capacity(
        crate::video_stream::UdpSocketAsyncReader::new(udp_socket),
        n3ds_remote_play_common::rtp_mpeg::RtpMpegPacketDecoder::new(),
        BUFFER_SIZE,
    );

    loop {
        // Fetch packets until cancellation or an error occurs
        let packet_result = select! {
            biased;
            _ = cancel_token.cancelled() => break,
            packet_result = packet_reader.next() => packet_result,
        };

        let (packet, recv_duration) = match packet_result {
            Some(Ok(item)) => item,
            Some(Err(e)) => {
                log::error!("Error reading video packet: {e:#}");
                break;
            }
            None => {
                log::warn!("Video packet stream ended unexpectedly");
                break;
            }
        };

        match packet_writer.try_send((packet, recv_duration)) {
            Ok(()) => {}
            Err(std::sync::mpsc::TrySendError::Full(_)) => {
                log::warn!("Packet writer channel full, dropping packet");
            }
            Err(std::sync::mpsc::TrySendError::Disconnected(_)) => {
                log::warn!("Packet writer channel closed unexpectedly");
                break;
            }
        }
    }

    log::debug!("receive_video_packets task exited");
}
