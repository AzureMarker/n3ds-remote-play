use bincode::Options;
use n3ds_remote_play_common::InputState;
use std::net::{TcpStream, UdpSocket};
use std::sync::Arc;
use std::time::Duration;
use tokio_stream::StreamExt;
use tokio_util::bytes::BytesMut;
use tokio_util::codec::FramedRead;

/// The system core thread runs its own Tokio runtime to handle async tasks.
/// It handles sending input states over UDP and reading packets from TCP.
pub fn run_system_thread(
    connection: TcpStream,
    udp_socket: UdpSocket,
    input_reader: tokio::sync::mpsc::Receiver<InputState>,
    packet_writer: std::sync::mpsc::SyncSender<(BytesMut, Duration)>,
) {
    let async_runtime = tokio::runtime::Builder::new_current_thread()
        .enable_time()
        .build()
        .expect("Failed to create Tokio runtime");

    async_runtime.block_on(async move {
        // Spawn tasks and wait for them to complete
        let mut join_set = tokio::task::JoinSet::new();

        // Reuse the same UDP socket for both directions:
        // - send input states to server
        // - receive RTP/MPEG packets from server
        let udp_socket = Arc::new(udp_socket);

        join_set.spawn(send_inputs(Arc::clone(&udp_socket), input_reader));
        join_set.spawn(receive_video_packets(connection, udp_socket, packet_writer));

        while let Some(join_result) = join_set.join_next().await {
            if let Err(e) = join_result {
                log::error!("A task failed: {e}");
            }
        }
    });

    log::info!("System thread exiting");
}

/// Task to send input states over UDP
async fn send_inputs(
    udp_socket: Arc<UdpSocket>,
    mut input_reader: tokio::sync::mpsc::Receiver<InputState>,
) {
    loop {
        // Read input state from channel
        match input_reader.recv().await {
            Some(state) => {
                // Send input state to server
                if let Err(e) = send_input_state(&udp_socket, state) {
                    log::error!("Failed to send input state: {e}");
                    break;
                }
            }
            None => {
                log::info!("Input reader channel disconnected");
                break;
            }
        }
    }
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
    connection: TcpStream,
    udp_socket: Arc<UdpSocket>,
    packet_writer: std::sync::mpsc::SyncSender<(BytesMut, Duration)>,
) {
    // Keep TCP around for bootstrap/future use.
    let _connection = connection;

    // 1 MB buffer size to allow for holding multiple RTP packets as they are decoded
    const BUFFER_SIZE: usize = 1024 * 1024;
    let mut packet_reader = FramedRead::with_capacity(
        crate::video_stream::UdpSocketAsyncReader::new(udp_socket),
        n3ds_remote_play_common::rtp_mpeg::RtpMpegPacketCodec::new(),
        BUFFER_SIZE,
    );

    while let Some(item) = packet_reader.next().await {
        match item {
            Ok((packet, recv_duration)) => match packet_writer.try_send((packet, recv_duration)) {
                Ok(()) => {}
                Err(std::sync::mpsc::TrySendError::Full(_)) => {
                    log::warn!("Packet writer channel full, dropping packet");
                }
                Err(std::sync::mpsc::TrySendError::Disconnected(_)) => {
                    log::info!("Packet writer channel closed");
                    break;
                }
            },
            Err(e) => {
                log::error!("Error while reading RTP MPEG packet: {e}");
                break;
            }
        }
    }
}
