use crate::input_mapper::InputMapperHandle;
use crate::video_stream;
use crate::virtual_device::{VirtualDevice, VirtualDeviceFactory};
use anyhow::Context;
use std::sync::Arc;
use std::time::Duration;
use tokio::io::AsyncWriteExt;
use tokio::net::{TcpStream, UdpSocket};
use tokio::select;
use tokio_util::sync::CancellationToken;
use tokio_util::task::LocalPoolHandle;
use tracing::{debug, error, info, trace};

const CLIENT_CONNECTION_TIMEOUT: Duration = Duration::from_secs(30);
const EFFECTIVE_FRAME_RATE: usize = 20;
const ENCODER_FRAME_RATE: usize = 25;

/// This function is the connection task entrypoint, which handles a single client connection.
#[tracing::instrument(name = "handle_connection", skip_all, fields(peer_addr))]
pub async fn handle_connection(
    tcp_stream: TcpStream,
    udp_socket: Arc<UdpSocket>,
    device_factory: impl VirtualDeviceFactory,
    input_mapper_handle: InputMapperHandle,
    local_pool_handle: LocalPoolHandle,
    cancel_token: CancellationToken,
) {
    // Handle the connection in a separate function so we can use the `?` operator for error handling.
    // We will log the errors in this function since nothing is watching the result of this task.
    if let Err(e) = handle_connection_impl(
        tcp_stream,
        udp_socket,
        device_factory,
        input_mapper_handle,
        local_pool_handle,
        cancel_token,
    )
    .await
    {
        error!("{e:#}");
    }
}

async fn handle_connection_impl(
    mut tcp_stream: TcpStream,
    udp_socket: Arc<UdpSocket>,
    device_factory: impl VirtualDeviceFactory,
    input_mapper_handle: InputMapperHandle,
    local_pool_handle: LocalPoolHandle,
    cancel_token: CancellationToken,
) -> anyhow::Result<()> {
    let peer_addr = tcp_stream
        .peer_addr()
        .context("Failed to get peer address")?;
    tracing::Span::current().record("peer_addr", tracing::field::display(peer_addr));
    info!("New connection from {peer_addr}");

    let mut device = device_factory
        .new_device()
        .await
        .context("Failed to create virtual device")?;
    debug!("Created virtual device");

    // Register to receive input packets
    let (mut input_receiver, _input_guard) = input_mapper_handle
        .register_input(peer_addr)
        .await
        .context("Failed to register for input packets")?;
    debug!("Registered input mapping");

    // Start video stream task. It must be spawned on the local pool since it's !Send due to FFMPEG.
    let video_stream_task = video_stream::VideoStreamTask {
        peer_addr,
        udp_socket,
        effective_fps: EFFECTIVE_FRAME_RATE,
        encoder_fps: ENCODER_FRAME_RATE,
    };
    let video_cancel_token = cancel_token.child_token();
    let video_cancel_token_clone = video_cancel_token.clone();
    let mut video_stream_handle =
        local_pool_handle.spawn_pinned(|| video_stream_task.run(video_cancel_token_clone));
    debug!("Spawned video stream task");

    // Handle input events in the main task.
    // If the client stops sending input events, we will close the connection.
    let mut joined_video_stream_task = false;
    loop {
        select! {
            biased;

            _ = cancel_token.cancelled() => {
                debug!("Connection task was cancelled");
                break;
            }

            result = &mut video_stream_handle => {
                joined_video_stream_task = true;
                match result {
                    Ok(Ok(())) => {
                        error!("Video stream task ended unexpectedly");
                    }
                    Ok(Err(e)) => {
                        error!("Video stream task returned an error: {e:#}");
                    }
                    Err(e) => {
                        error!("Video stream task ended unexpectedly: {e}");
                    }
                }
                break;
            }

            changed_result = input_receiver.changed() => {
                if changed_result.is_err() {
                    info!("Input receiver channel closed");
                    break;
                };

                // Copy the input state from the watch channel
                let input_state = *input_receiver.borrow_and_update();

                // Send the input state to the virtual device
                trace!("{input_state:?}");
                if let Err(e) = device.emit_input(input_state) {
                    error!("Error while emitting input state: {e:#}");
                    break;
                }
            }

            _ = tokio::time::sleep(CLIENT_CONNECTION_TIMEOUT) => {
                error!("Timed out while waiting for client to send an input packet ({CLIENT_CONNECTION_TIMEOUT:?})");
                break;
            }
        }
    }

    info!("Closing connection");
    video_cancel_token.cancel();
    if !joined_video_stream_task {
        video_stream_handle.await.ok();
    }
    tcp_stream
        .shutdown()
        .await
        .context("Failed to shut down TCP stream")?;
    info!("Closed connection");

    Ok(())
}
