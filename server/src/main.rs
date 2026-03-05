#![warn(clippy::unused_async)]

mod connection_task;
mod input_mapper;
mod mpeg_encoder;
mod video_capture;
mod video_stream;
mod virtual_device;

use crate::connection_task::handle_connection;
use ffmpeg_next as ffmpeg;
use futures::StreamExt;
use input_mapper::InputMapper;
use std::sync::Arc;
use tokio::net::{TcpListener, UdpSocket};
use tokio_stream::wrappers::TcpListenerStream;
use tokio_util::future::FutureExt;
use tokio_util::sync::CancellationToken;
use tokio_util::task::{LocalPoolHandle, TaskTracker};
use tracing::level_filters::LevelFilter;
use tracing::{error, info, warn};
use tracing_subscriber::FmtSubscriber;

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

    // Set up task lifecycle management and signal handling
    let task_tracker = TaskTracker::new();
    let cancel_token = CancellationToken::new();
    let _cancel_guard = cancel_token.clone().drop_guard();
    task_tracker.spawn(handle_signals(cancel_token.clone()));

    // We need a local pool to run the video streaming tasks, since they are !Send due to FFMPEG.
    // Currently, it's not expected to have more than one client, so one worker is enough.
    let local_pool_handle = LocalPoolHandle::new(1);

    // Set up the input mapper to receive input packets from the client and send them to the correct
    // connection task.
    let input_mapper = InputMapper::new(Arc::clone(&udp_socket));
    let input_mapper_handle = input_mapper.handle();
    task_tracker.spawn(input_mapper.run(cancel_token.child_token()));

    info!("Server started, waiting for connections");
    while let Some(connection) = tcp_stream
        .next()
        .with_cancellation_token(&cancel_token)
        .await
    {
        match connection {
            Some(Ok(connection)) => {
                task_tracker.spawn(handle_connection(
                    connection,
                    Arc::clone(&udp_socket),
                    device_factory.clone(),
                    input_mapper_handle.clone(),
                    local_pool_handle.clone(),
                    cancel_token.child_token(),
                ));
            }
            Some(Err(e)) => {
                error!("New connection error: {e}");
                continue;
            }
            None => {
                // This can only happen if the cancellation token is triggered
                // by a shutdown signal, so we can just exit the loop and shut down the server.
                break;
            }
        }
    }

    info!("Server shutting down");
    task_tracker.close();
    cancel_token.cancel();
    task_tracker.wait().await;
    info!("Server shut down");
}

/// If a Ctrl-C is received, cancel the token to shut down the server.
async fn handle_signals(cancel_token: CancellationToken) {
    tokio::signal::ctrl_c()
        .await
        .expect("Failed to listen for Ctrl-C");

    warn!("Ctrl-C received, shutting down server");
    cancel_token.cancel();
}
