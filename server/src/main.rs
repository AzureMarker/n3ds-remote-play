#![warn(clippy::unused_async)]

mod input_mapper;
mod mpeg_encoder;
mod video_capture;
mod video_stream;
mod virtual_device;

use crate::virtual_device::{VirtualDevice, VirtualDeviceFactory};
use anyhow::Context;
use ffmpeg_next as ffmpeg;
use futures::StreamExt;
use input_mapper::{InputMapper, InputMapperHandle};
use std::sync::Arc;
use std::time::Duration;
use tokio::io::AsyncWriteExt;
use tokio::net::{TcpListener, TcpStream, UdpSocket};
use tokio::select;
use tokio_stream::wrappers::TcpListenerStream;
use tokio_util::task::LocalPoolHandle;
use tracing::level_filters::LevelFilter;
use tracing::{debug, error, info, trace, warn};
use tracing_subscriber::FmtSubscriber;

const CLIENT_CONNECTION_TIMEOUT: Duration = Duration::from_secs(30);

fn main() {
    FmtSubscriber::builder()
        .with_max_level(LevelFilter::DEBUG)
        .init();

    ffmpeg::init().expect("Failed to initialize ffmpeg");

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .unwrap();

    runtime.block_on(async_main());
}

async fn async_main() {
    info!("Starting n3ds-remote-play server on 0.0.0.0:3535");

    // Set up networking
    let tcp_listener = TcpListener::bind(("0.0.0.0", 3535))
        .await
        .expect("Failed to bind address");
    let mut tcp_stream = TcpListenerStream::new(tcp_listener);
    let udp_socket = UdpSocket::bind(("0.0.0.0", 3535))
        .await
        .expect("Failed to bind UDP socket");
    let udp_socket = Arc::new(udp_socket);

    // The virtual device factory is used to create virtual input devices for each client.
    let device_factory =
        virtual_device::new_device_factory().expect("Failed to create virtual device factory");

    // Set up the input mapper to receive input packets from the client and send them to the correct
    // connection task.
    let input_mapper = InputMapper::new(Arc::clone(&udp_socket));
    let input_mapper_handle = input_mapper.handle();
    let input_mapper_task = tokio::spawn(input_mapper.run());

    // We need a local pool to run the video streaming tasks, since they are !Send due to FFMPEG.
    // Currently, it's not expected to have more than one client, so one worker is enough.
    let local_pool_handle = LocalPoolHandle::new(1);

    info!("Server started, waiting for connections");
    while let Some(connection) = tcp_stream.next().await {
        match connection {
            Ok(connection) => {
                tokio::spawn(handle_connection(
                    connection,
                    Arc::clone(&udp_socket),
                    device_factory.clone(),
                    input_mapper_handle.clone(),
                    local_pool_handle.clone(),
                ));
            }
            Err(e) => {
                error!("New connection error: {e}");
                continue;
            }
        }
    }

    info!("Server shutting down");
    input_mapper_handle.start_shutdown().await;
    input_mapper_task.await.ok();
    info!("Server shut down");
}

async fn handle_connection(
    tcp_stream: TcpStream,
    udp_socket: Arc<UdpSocket>,
    device_factory: impl VirtualDeviceFactory,
    input_mapper_handle: InputMapperHandle,
    local_pool_handle: LocalPoolHandle,
) {
    // Handle the connection in a separate function so we can use the `?` operator for error handling.
    // We will log the errors in this function since nothing is watching the result of this task.
    if let Err(e) = handle_connection_impl(
        tcp_stream,
        udp_socket,
        device_factory,
        input_mapper_handle,
        local_pool_handle,
    )
    .await
    {
        error!("{e:#}");
    }
}

#[tracing::instrument(name = "handle_connection", skip_all, fields(peer_addr))]
async fn handle_connection_impl(
    mut tcp_stream: TcpStream,
    udp_socket: Arc<UdpSocket>,
    device_factory: impl VirtualDeviceFactory,
    input_mapper_handle: InputMapperHandle,
    local_pool_handle: LocalPoolHandle,
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
        effective_fps: 20,
        encoder_fps: 25,
    };
    let (exit_sender, exit_receiver) = tokio::sync::oneshot::channel();
    let mut video_stream_handle =
        local_pool_handle.spawn_pinned(|| video_stream_task.run(exit_receiver));
    debug!("Spawned video stream task");

    // Handle input events in the main task.
    // If the client stops sending input events, we will close the connection.
    let mut joined_video_stream_task = false;
    loop {
        select! {
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
        }
    }

    info!("Closing connection");
    let _ = exit_sender.send(());
    if !joined_video_stream_task {
        video_stream_handle
            .await
            .context("Failed to wait for video stream task to finish")?
            .context("Video stream task returned an error")?;
    }
    tcp_stream
        .shutdown()
        .await
        .context("Failed to shut down TCP stream")?;
    info!("Closed connection");

    Ok(())
}
