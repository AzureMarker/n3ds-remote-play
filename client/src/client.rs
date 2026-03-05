use crate::input_handler::InputHandler;
use crate::thread_spawning::spawn_system_core_thread;
use crate::video_stream::BgrFrameView;
use crate::{system_thread, video_stream};
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
    mpeg_decoder: video_stream::Mpeg1Decoder,
}

impl<'gfx> RemotePlayClient<'gfx> {
    pub fn new(apt: Apt, gfx: &'gfx Gfx) -> Self {
        Self {
            apt,
            gfx,
            mpeg_decoder: video_stream::Mpeg1Decoder::new()
                .expect("Failed to create MPEG1 decoder"),
        }
    }

    pub fn run(&mut self, server_ip: Ipv4Addr, hid: Hid, ir_user: IrUser) {
        // Set up connections
        let tcp_connection =
            TcpStream::connect((server_ip, 3535)).expect("Failed to connect to server");
        let udp_socket = UdpSocket::bind(tcp_connection.local_addr().unwrap())
            .expect("Failed to listen for UDP connections");
        set_udp_recv_buffer_size(&udp_socket, 64 * 1024);
        udp_socket
            .connect((server_ip, 3535))
            .expect("Failed to set up UDP connection to server");
        udp_socket.set_nonblocking(true).unwrap();
        log::info!("Connected to remote play server at {server_ip}.");

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
        let spawn_result = unsafe {
            spawn_system_core_thread(&mut self.apt, || {
                system_thread::run_system_thread(
                    udp_socket,
                    input_receiver,
                    packet_sender,
                    cancel_token,
                )
            })
        };
        let Ok(system_thread) = spawn_result else {
            log::error!(
                "Failed to spawn system core thread: {:?}",
                spawn_result.unwrap_err()
            );
            return;
        };

        while self.apt.main_loop() {
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

    fn process_video_stream(
        &mut self,
        packet_receiver: &std::sync::mpsc::Receiver<(BytesMut, Duration)>,
    ) {
        // Drain queued MPEG packets, decode them, and display the most recent decoded frame.
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
