use crate::input_handler::InputHandler;
use crate::mpeg_decoder::BgrFrameView;
use crate::thread_spawning::spawn_system_core_thread;
use crate::{mpeg_decoder, system_thread};
use ctru::prelude::{Apt, Gfx, Hid};
use ctru::services::gfx::{Flush, Screen, Swap};
use ctru::services::ir_user::IrUser;
use n3ds_remote_play_common::InputState;
use std::net::{Ipv4Addr, TcpStream, UdpSocket};
use std::os::fd::AsRawFd;
use std::time::{Duration, Instant};
use tokio_util::bytes::BytesMut;
use tokio_util::sync::CancellationToken;

pub struct RemotePlayClient<'gfx> {
    apt: Apt,
    gfx: &'gfx Gfx,
    mpeg_decoder: mpeg_decoder::Mpeg1Decoder,
}

impl<'gfx> RemotePlayClient<'gfx> {
    pub fn new(apt: Apt, gfx: &'gfx Gfx) -> Self {
        Self {
            apt,
            gfx,
            mpeg_decoder: mpeg_decoder::Mpeg1Decoder::new()
                .expect("Failed to create MPEG1 decoder"),
        }
    }

    pub fn run(mut self, server_ip: Ipv4Addr, hid: Hid, ir_user: IrUser) {
        // Set up connections
        let (tcp_connection, udp_socket) = setup_networking(server_ip);
        log::info!("Connected to remote play server at {server_ip}");

        // Set up input handler and check for Circle Pad Pro
        let (input_sender, input_receiver) = tokio::sync::watch::channel(InputState::default());
        let mut input_handler = InputHandler::new(hid, ir_user, input_sender);

        input_handler
            .connect_circle_pad_pro(&self.apt, self.gfx)
            .expect("Failed to connect Circle Pad Pro");

        // Set up system core thread to send/receive network data in parallel
        let (packet_sender, packet_receiver) = std::sync::mpsc::sync_channel(2);
        let cancel_token = CancellationToken::new();
        let cancel_guard = cancel_token.clone().drop_guard();
        let system_thread = unsafe {
            spawn_system_core_thread(&mut self.apt, || {
                system_thread::run_system_thread(
                    udp_socket,
                    input_receiver,
                    packet_sender,
                    cancel_token,
                )
            })
            .expect("Failed to spawn system core thread")
        };

        // Main loop to send inputs and render video frames
        while self.apt.main_loop() {
            // Check if the TCP connection was closed. If so, shut down the client.
            if is_tcp_disconnected(&tcp_connection) {
                log::warn!("Connection closed by server, shutting down client");
                break;
            }

            // Scan inputs and send to the server
            if let Err(e) = input_handler.send_inputs_to_server() {
                // This could be an error or just the user choosing to exit
                log::info!("Exiting main loop: {e:#}");
                break;
            }

            // Drain/decode any received MPEG packets on the main thread.
            self.process_video_stream(&packet_receiver);

            self.gfx.wait_for_vblank();
        }

        log::debug!("Main loop stopped, shutting down system thread");
        drop(cancel_guard);
        let join_result = unsafe { libc::pthread_join(system_thread, std::ptr::null_mut()) };
        if join_result != 0 {
            log::error!(
                "Failed to join system thread: {}",
                std::io::Error::from_raw_os_error(join_result)
            );
        }
        log::debug!("System thread shut down, exiting client");
    }

    /// Check for an MPEG packet from the system thread, decode it, and display the frame.
    fn process_video_stream(
        &mut self,
        packet_receiver: &std::sync::mpsc::Receiver<(BytesMut, Duration)>,
    ) {
        let frame_decode_start = Instant::now();
        let frame: BgrFrameView<'_>;
        let packet_size: usize;
        let frame_recv_duration: Duration;

        if let Ok((packet, recv_duration)) = packet_receiver.try_recv() {
            packet_size = packet.len();
            frame_recv_duration = recv_duration;

            match self.mpeg_decoder.decode_mpeg_packet(&packet) {
                Ok(Some(frame_view)) => {
                    frame = frame_view;
                }
                Ok(None) => {
                    // Decoder needs more input before it can output a frame.
                    return;
                }
                Err(e) => {
                    log::error!("MPEG decode error: {e:#}");
                    return;
                }
            }
        } else {
            // No packets available.
            return;
        }

        // Write the frame (row-by-row because FFmpeg may pad stride for alignment).
        let frame_write_start = Instant::now();
        let mut top_screen = self.gfx.top_screen.borrow_mut();
        unsafe {
            let fb = top_screen.raw_framebuffer().ptr;

            let row_bytes = frame.width * 3;
            for y in 0..frame.height {
                let src_off = y * frame.stride;
                let dst_off = y * row_bytes;
                fb.add(dst_off)
                    .copy_from(frame.data.as_ptr().add(src_off), row_bytes);
            }
        }
        let frame_flush_start = Instant::now();
        top_screen.flush_buffers();
        top_screen.swap_buffers();

        let frame_flush_duration = frame_flush_start.elapsed();
        let frame_write_duration = frame_flush_start.duration_since(frame_write_start);
        log::debug!("Write: {frame_write_duration:.1?}, Flush: {frame_flush_duration:.1?}");
        log::info!(
            "MPEG: {:5} Rx: {:3?}ms De: {:2?}ms",
            packet_size,
            frame_recv_duration.as_millis(),
            frame_decode_start.elapsed().as_millis()
        );
        log::debug!("");
    }
}

/// Set up TCP and UDP connections to the server.
fn setup_networking(server_ip: Ipv4Addr) -> (TcpStream, UdpSocket) {
    // Connect to the server over TCP to establish the connection.
    let tcp_connection =
        TcpStream::connect((server_ip, 3535)).expect("Failed to connect to server");
    tcp_connection
        .set_nonblocking(true)
        .expect("Failed to set TCP stream to non-blocking");

    // Set up a UDP socket for the data streams
    let udp_socket = UdpSocket::bind(tcp_connection.local_addr().unwrap())
        .expect("Failed to listen for UDP connections");

    set_udp_recv_buffer_size(&udp_socket, 64 * 1024);
    udp_socket
        .connect((server_ip, 3535))
        .expect("Failed to set up UDP connection to server");
    udp_socket
        .set_nonblocking(true)
        .expect("Failed to set UDP socket to non-blocking mode");

    (tcp_connection, udp_socket)
}

/// Check if the TCP connection has been closed by the server.
fn is_tcp_disconnected(tcp_connection: &TcpStream) -> bool {
    // Check if the TCP connection is still alive by peeking with a one byte buffer.
    // If recv returns 0, the connection has been closed by the server.
    let mut buf = [0];
    match tcp_connection.peek(&mut buf) {
        Ok(0) => true,  // Connection closed
        Ok(_) => false, // Data available, connection alive
        // No data but still connected
        Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => false,
        Err(_) => true, // Other errors treated as disconnection
    }
}

/// Set the UDP receive buffer size. The default is very small and causes packet loss.
fn set_udp_recv_buffer_size(udp_socket: &UdpSocket, size: usize) {
    unsafe {
        let socket_fd = udp_socket.as_raw_fd();
        if libc::setsockopt(
            socket_fd,
            libc::SOL_SOCKET,
            libc::SO_RCVBUF,
            &size as *const _ as *const libc::c_void,
            size_of::<libc::c_int>() as libc::socklen_t,
        ) != 0
        {
            panic!("setsockopt failed");
        }
    }
}
