use anyhow::anyhow;
use ffmpeg::software::scaling as sws;
use ffmpeg_next as ffmpeg;
use rtp_types::RtpPacket;
use std::cmp::min;

pub trait FrameReassembler {
    fn process_packet(&mut self, packet_buf: &[u8]) -> anyhow::Result<Option<Vec<u8>>>;
}

// A simple representation of a decoded frame
// pub type Frame<'a> = &'a [u8];

// FrameReassembler handles the reassembly of RTP packets into complete frames.
pub struct RtpFrameReassembler {
    data: Vec<u8>,
    state: RtpFrameReassemblyState,
}

#[derive(PartialEq, Eq, Debug)]
enum RtpFrameReassemblyState {
    NotStarted,
    InProgress { expected_seq: u16 },
    WaitingForMarker,
    Complete,
}

impl RtpFrameReassembler {
    pub fn new() -> Self {
        RtpFrameReassembler {
            data: Vec::new(),
            state: RtpFrameReassemblyState::NotStarted,
        }
    }
}

impl FrameReassembler for RtpFrameReassembler {
    // Process an incoming RTP packet and attempt to reassemble frames.
    // Returns Some(Frame) if a complete frame is reassembled, None otherwise.
    fn process_packet(&mut self, packet_buf: &[u8]) -> anyhow::Result<Option<Vec<u8>>> {
        log::debug!("Starting state: {:?}", self.state);

        if self.state == RtpFrameReassemblyState::Complete {
            // Previous frame is complete, clear for new frame
            self.data.clear();
            self.state = RtpFrameReassemblyState::NotStarted;
        }

        let packet = RtpPacket::parse(packet_buf)?;
        log::debug!(
            "Processing packet: seq={}, marker={}",
            packet.sequence_number(),
            packet.marker_bit(),
        );

        // Handle waiting for marker bit to start a new frame
        if self.state == RtpFrameReassemblyState::WaitingForMarker {
            if !packet.marker_bit() {
                // Still waiting for the next frame start
                return Ok(None);
            } else {
                // Found the start of a new frame
                self.state = RtpFrameReassemblyState::NotStarted;
                log::debug!("Starting new frame reassembly");
            }
        }

        // Check for sequence number continuity
        if let RtpFrameReassemblyState::InProgress { expected_seq } = self.state
            && packet.sequence_number() != expected_seq
        {
            log::warn!("Packet loss detected, discarding current frame data.");
            self.data.clear();
            // Wait for the next marker bit to know when the next frame starts
            self.state = RtpFrameReassemblyState::WaitingForMarker;
            return Ok(None);
        }

        // Append payload (skipping the 4-byte RFC 2435 header that was added)
        self.data.extend_from_slice(&packet.payload()[4..]);
        log::debug!("Current frame size: {} bytes", self.data.len());

        if packet.marker_bit() {
            // Frame is complete
            self.state = RtpFrameReassemblyState::Complete;
            Ok(Some(self.data.clone()))
        } else {
            // Frame is incomplete
            self.state = RtpFrameReassemblyState::InProgress {
                expected_seq: packet.sequence_number().wrapping_add(1),
            };
            Ok(None)
        }
    }
}

pub struct TcpFrameReassembler {
    data: Vec<u8>,
    state: TcpFrameReassemblyState,
}

#[derive(PartialEq, Eq, Debug)]
enum TcpFrameReassemblyState {
    NotStarted,
    InProgress { expected_length: u32 },
    Complete,
}

impl TcpFrameReassembler {
    pub fn new() -> Self {
        TcpFrameReassembler {
            data: Vec::new(),
            state: TcpFrameReassemblyState::NotStarted,
        }
    }
}

impl FrameReassembler for TcpFrameReassembler {
    fn process_packet(&'_ mut self, packet_buf: &[u8]) -> anyhow::Result<Option<Vec<u8>>> {
        log::debug!("Starting TCP state: {:?}", self.state);

        if self.state == TcpFrameReassemblyState::Complete {
            // Previous frame is complete, clear for new frame
            self.data.clear();
            self.state = TcpFrameReassemblyState::NotStarted;
        }

        log::debug!("Processing TCP packet: length={}", packet_buf.len(),);

        match &self.state {
            TcpFrameReassemblyState::NotStarted => {
                // Start of a new frame
                let expected_length = u32::from_be_bytes(*packet_buf.first_chunk().unwrap());
                self.data.extend_from_slice(&packet_buf[4..]);
                self.state = TcpFrameReassemblyState::InProgress { expected_length };
                Ok(None)
            }
            TcpFrameReassemblyState::InProgress { expected_length } => {
                let remaining_length = min(
                    *expected_length as usize - self.data.len(),
                    packet_buf.len(),
                );

                if remaining_length != packet_buf.len() {
                    log::warn!(
                        "Received more data than expected for TCP frame, trying to compensate."
                    );
                    self.data.extend_from_slice(&packet_buf[..remaining_length]);
                    let full_frame = self.data.clone();
                    self.state = TcpFrameReassemblyState::Complete;

                    let inner_result = self.process_packet(&packet_buf[remaining_length..])?;
                    if inner_result.is_some() {
                        log::debug!(
                            "Also completed next frame while compensating. Only returning first frame."
                        );
                    }

                    return Ok(Some(full_frame));
                }

                self.data.extend_from_slice(&packet_buf[..remaining_length]);

                if self.data.len() == *expected_length as usize {
                    // Frame is complete
                    self.state = TcpFrameReassemblyState::Complete;
                    Ok(Some(self.data.clone()))
                } else if self.data.len() > *expected_length as usize {
                    // This should not happen
                    log::warn!("Received more data than expected for TCP frame, resetting.");
                    self.data.clear();
                    self.state = TcpFrameReassemblyState::NotStarted;
                    Ok(None)
                } else {
                    // Frame is still incomplete
                    log::debug!("Current frame size: {} bytes", self.data.len());
                    Ok(None)
                }
            }
            TcpFrameReassemblyState::Complete => {
                // Should not reach here due to the initial check
                unreachable!();
            }
        }
    }
}

/// Reassembles MPEG packets from a TCP byte stream.
/// MPEG packets are framed with a 4-byte big-endian length prefix.
pub struct MpegPacketReassembler {
    tcp_buf: Vec<u8>,
}

impl MpegPacketReassembler {
    // 1 MB max packet size to avoid OOM on malformed streams
    const MAX_PACKET_LEN: usize = 1024 * 1024;

    pub fn new() -> Self {
        Self {
            tcp_buf: Vec::with_capacity(Self::MAX_PACKET_LEN),
        }
    }

    /// Feed the next chunk read from the TCP stream.
    /// Returns a complete MPEG packet payload when available.
    /// Framing: (u32 length BE)(payload). Invalid length is a hard error (non-recoverable desync).
    pub fn push_chunk(&mut self, chunk: &[u8]) -> anyhow::Result<Option<Vec<u8>>> {
        self.tcp_buf.extend_from_slice(chunk);

        let Some((length_bytes, packet_bytes)) = self.tcp_buf.split_first_chunk::<4>() else {
            // Not enough data for length prefix yet
            return Ok(None);
        };

        let len = u32::from_be_bytes(*length_bytes) as usize;
        if len == 0 || len > Self::MAX_PACKET_LEN {
            return Err(anyhow!("Bad MPEG packet length {len}; TCP stream desynced"));
        }

        if packet_bytes.len() < len {
            // Not enough data for full packet yet
            return Ok(None);
        }

        let packet = packet_bytes.to_vec();
        // The drain should not do any memory copies in the common case because there should not be
        // any extra data after the completed packet yet.
        self.tcp_buf.drain(0..4 + len);

        Ok(Some(packet))
    }
}

pub struct Mpeg1Decoder {
    decoder: ffmpeg::codec::decoder::Video,
    video_frame: ffmpeg::util::frame::Video,
    /// Software scaler to convert decoded YUV420P into packed BGR for the framebuffer.
    scaler: sws::context::Context,
    bgr_frame: ffmpeg::util::frame::Video,
    /// One-time sanity log to confirm decoder output matches framebuffer expectations.
    logged_format: bool,
}

pub struct BgrFrameView<'a> {
    pub data: &'a [u8],
    pub width: usize,
    pub height: usize,
    pub stride: usize,
}

impl Mpeg1Decoder {
    pub fn new() -> anyhow::Result<Self> {
        // Initialize FFmpeg libraries
        ffmpeg::init()?;

        // Find the MPEG1 decoder
        let codec = ffmpeg::decoder::find(ffmpeg::codec::Id::MPEG1VIDEO)
            .ok_or_else(|| anyhow!("MPEG1 decoder not found"))?;

        let context = ffmpeg::codec::context::Context::new_with_codec(codec);
        let decoder = context.decoder().video()?;
        let video_frame = ffmpeg::util::frame::Video::empty();

        // The server always sends frames scaled/rotated to the 3DS top screen.
        // Decoded MPEG frames are 240x400 YUV420P.
        const FRAME_W: u32 = 240;
        const FRAME_H: u32 = 400;
        let scaler = sws::context::Context::get(
            ffmpeg::format::Pixel::YUV420P,
            FRAME_W,
            FRAME_H,
            ffmpeg::format::Pixel::BGR24,
            FRAME_W,
            FRAME_H,
            sws::flag::Flags::FAST_BILINEAR,
        )
        .map_err(|e| anyhow!("Failed to create swscale context: {e}"))?;

        let bgr_frame =
            ffmpeg::util::frame::Video::new(ffmpeg::format::Pixel::BGR24, FRAME_W, FRAME_H);

        Ok(Self {
            decoder,
            video_frame,
            scaler,
            bgr_frame,
            logged_format: false,
        })
    }

    /// Decode a single MPEG packet payload (already framed by the TCP reassembler).
    ///
    /// Returns:
    /// - Ok(Some(frame)) when a decoded+converted frame is available
    /// - Ok(None) when the decoder needs more input (transient: EAGAIN/EOF/etc)
    pub fn decode_mpeg_packet(
        &mut self,
        payload: &[u8],
    ) -> anyhow::Result<Option<BgrFrameView<'_>>> {
        let packet = ffmpeg::codec::packet::Packet::borrow(payload);
        self.decoder.send_packet(&packet)?;

        match self.decoder.receive_frame(&mut self.video_frame) {
            Ok(()) => {
                if !self.logged_format {
                    let w = self.video_frame.width();
                    let h = self.video_frame.height();
                    let fmt = self.video_frame.format();
                    log::info!(
                        "MPEG decode output: {}x{} {:?} (expected BGR bytes = {})",
                        w,
                        h,
                        fmt,
                        (w as usize).saturating_mul(h as usize).saturating_mul(3)
                    );
                    self.logged_format = true;
                }

                // Convert decoded YUV420P to BGR24 using swscale.
                self.scaler
                    .run(&self.video_frame, &mut self.bgr_frame)
                    .map_err(|e| anyhow!("swscale conversion failed: {e}"))?;

                let width = self.bgr_frame.width() as usize;
                let height = self.bgr_frame.height() as usize;
                let stride = self.bgr_frame.stride(0);
                let data = self.bgr_frame.data(0);
                if width == 0 || height == 0 || stride == 0 || data.is_empty() {
                    return Err(anyhow!("swscale produced empty/invalid BGR frame"));
                }

                // Ensure the buffer is large enough for row-by-row access.
                let min_needed = stride.saturating_mul(height);
                if data.len() < min_needed {
                    return Err(anyhow!(
                        "swscale produced short buffer: got {}, need at least {}",
                        data.len(),
                        min_needed
                    ));
                }

                Ok(Some(BgrFrameView {
                    data,
                    width,
                    height,
                    stride,
                }))
            }
            Err(ffmpeg::Error::Other { errno })
                if errno == ffmpeg::sys::EAGAIN || errno == ffmpeg::sys::EOF =>
            {
                Ok(None)
            }
            // Some embedded/libc errno tables are odd; treat the common devkitARM value we see
            // as transient rather than fatal.
            Err(ffmpeg::Error::Other { errno }) if errno == libc::ESRCH => Ok(None),
            Err(e) => Err(anyhow!("Decoding error: {}", e)),
        }
    }
}
