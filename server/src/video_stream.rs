use crate::mpeg_encoder::Mpeg1Encoder;
use crate::video_capture::VideoCapture;
use anyhow::Context;
use ffmpeg_next as ffmpeg;
use n3ds_remote_play_common::rtp_mpeg::RTP_MPEG_PAYLOAD_TYPE;
use rtp_types::RtpPacketBuilder;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::net::UdpSocket;
use tokio::select;
use tokio::time::MissedTickBehavior;
use tracing::{debug, error, info, warn};

/// Maximum Transmission Unit size.
/// The 3DS has an MTU of 1400 bytes, but using a larger size seems to improve performance.
const MTU: usize = 2500;

/// This struct is used to run the video streaming task. It captures frames from the monitor using
/// [`VideoCapture`], encodes them using [`Mpeg1Encoder`], and sends them to the client over UDP
/// using RTP packets.
pub struct VideoStreamTask {
    pub peer_addr: SocketAddr,
    pub udp_socket: Arc<UdpSocket>,
    /// The number of frames sent per second. This must be equal or lower than the encoder FPS.
    pub effective_fps: usize,
    /// The numerator of the configured encoder FPS. This must be a supported MPEG-1 framerate.
    pub encoder_fps: usize,
}

impl VideoStreamTask {
    #[tracing::instrument(name = "video_stream_task", skip_all, fields(peer_addr = %self.peer_addr))]
    pub async fn run(
        self,
        mut exit_receiver: tokio::sync::oneshot::Receiver<()>,
    ) -> anyhow::Result<()> {
        // Start the video capture so we know which monitor to stream
        let mut video_capture = VideoCapture::start()?;
        let monitor_width = video_capture
            .monitor
            .width()
            .context("Failed to get monitor width")?;
        let monitor_height = video_capture
            .monitor
            .height()
            .context("Failed to get monitor height")?;

        // Set up the video encoder
        let mut video_encoder = Mpeg1Encoder::new(
            self.effective_fps,
            self.encoder_fps,
            monitor_width,
            monitor_height,
        )?;

        let mut frame_sleep_interval =
            tokio::time::interval(Duration::from_millis(1000 / self.effective_fps as u64));
        let mut sequence_number: u16 = 0;
        let mut stopped_receiving_frames = false;
        frame_sleep_interval.set_missed_tick_behavior(MissedTickBehavior::Delay);

        info!("Starting to send frames");
        loop {
            select! {
                biased;
                _ = &mut exit_receiver => {
                    // Regardless of the result (Ok or Err) we should exit
                    break;
                }
                // If we are waiting for the capture to start up again,
                // stop waiting immediately after a new frame is captured.
                _ = video_capture.frame_receiver.changed(), if stopped_receiving_frames => {
                    video_capture.frame_receiver.mark_changed();
                }
                // Sleep to limit the frame rate
                _ = frame_sleep_interval.tick() => {}
            }

            // Check for a frame
            let frame = match video_capture.frame_receiver.has_changed() {
                Ok(true) => {
                    // We have a new frame, continue to encode and send it
                    stopped_receiving_frames = false;
                    video_capture.frame_receiver.borrow_and_update()
                }
                Ok(false) => {
                    if !stopped_receiving_frames {
                        debug!("Stopped receiving frames from video stream");
                        stopped_receiving_frames = true;
                    }
                    continue;
                }
                Err(_) => {
                    warn!("Video stream ended unexpectedly");
                    break;
                }
            };

            // Encode the frame into MPEG-1 video packets
            let frame_processing_start = Instant::now();
            let mpeg_packets = video_encoder.encode_frame(&frame)?;
            let frame_processing_end = Instant::now();

            let frame_processing_duration =
                frame_processing_end.duration_since(frame_processing_start);
            let mpeg_data_size: usize = mpeg_packets.iter().map(ffmpeg::Packet::size).sum();

            // Send encoded MPEG packets as RTP over UDP back to the client.
            for packet in mpeg_packets {
                let Some(payload) = packet.data() else {
                    warn!("Failed to get MPEG packet data, skipping packet");
                    continue;
                };
                let start_time = Instant::now();

                match self
                    .send_mpeg_rtp_packet(payload, &mut sequence_number)
                    .await
                {
                    Err(e) => {
                        error!("Error while sending MPEG RTP packet: {e}");
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

        debug!("Video stream task exiting");
        video_capture
            .video_recorder
            .stop()
            .context("Failed to stop video recorder")?;
        Ok(())
    }

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
        &self,
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
        let frag_count = total_len.div_ceil(max_payload);
        let frag_count_u16: u16 = frag_count.try_into().map_err(|_| {
            anyhow::anyhow!("MPEG packet too large to fragment (frag_count overflow)")
        })?;

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
            self.udp_socket.send_to(&bytes, self.peer_addr).await?;
        }

        // Advance to the next MPEG packet id.
        *sequence_number = sequence_number.wrapping_add(1);

        Ok(frag_count)
    }
}
