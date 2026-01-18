use anyhow::anyhow;
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

pub struct MpegPacketReassembler {
    tcp_buf: Vec<u8>,
}

impl MpegPacketReassembler {
    pub fn new() -> Self {
        Self {
            tcp_buf: Vec::new(),
        }
    }

    /// Feed an arbitrary TCP chunk. Returns a complete MPEG packet payload when available.
    /// Framing: [u32 length BE][payload]. Invalid length => hard error (non-recoverable desync).
    pub fn push_chunk(&mut self, chunk: &[u8]) -> anyhow::Result<Option<Vec<u8>>> {
        self.tcp_buf.extend_from_slice(chunk);

        if self.tcp_buf.len() < 4 {
            return Ok(None);
        }

        let len = u32::from_be_bytes(self.tcp_buf[0..4].try_into().unwrap()) as usize;
        const MAX_PACKET_LEN: usize = 2 * 1024 * 1024;
        if len == 0 || len > MAX_PACKET_LEN {
            return Err(anyhow!("Bad MPEG packet length {len}; TCP stream desynced"));
        }

        if self.tcp_buf.len() < 4 + len {
            return Ok(None);
        }

        let payload = self.tcp_buf[4..4 + len].to_vec();
        self.tcp_buf.drain(0..4 + len);
        Ok(Some(payload))
    }
}

pub struct Mpeg1Decoder {
    decoder: ffmpeg::codec::decoder::Video,
    video_frame: ffmpeg::util::frame::Video,
    /// One-time sanity log to confirm decoder output matches framebuffer expectations.
    logged_format: bool,
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

        Ok(Self {
            decoder,
            video_frame,
            logged_format: false,
        })
    }

    /// Decode a single MPEG packet payload (already framed by the TCP reassembler).
    /// Returns the first decoded frame (BGR24) produced by this packet (if any); otherwise None.
    pub fn decode_mpeg_packet(&mut self, payload: &[u8]) -> anyhow::Result<Option<Vec<u8>>> {
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

                // Return the first decoded frame immediately.
                Ok(Some(yuv420p_to_bgr24(&self.video_frame)?))
            }
            Err(ffmpeg::Error::Other { errno }) if errno == ffmpeg::sys::EAGAIN => Ok(None),
            Err(e) => Err(anyhow!("Decoding error: {}", e)),
        }
    }
}

fn yuv420p_to_bgr24(frame: &ffmpeg::util::frame::Video) -> anyhow::Result<Vec<u8>> {
    use ffmpeg::format::Pixel;

    // Prefer a strict format match, but don't fail hard if the wrapper reports Pixel::None.
    let fmt = frame.format();
    if fmt != Pixel::YUV420P {
        static WARNED_FMT_MISMATCH: std::sync::Once = std::sync::Once::new();
        WARNED_FMT_MISMATCH.call_once(|| {
            log::warn!("Decoder reported pixel format {:?}; assuming YUV420P", fmt);
        });
    }

    let w = frame.width() as usize;
    let h = frame.height() as usize;
    if w == 0 || h == 0 {
        return Err(anyhow!("Decoded frame has invalid size {}x{}", w, h));
    }

    let y_plane = frame.data(0);
    let u_plane = frame.data(1);
    let v_plane = frame.data(2);

    let y_stride = frame.stride(0);
    let u_stride = frame.stride(1);
    let v_stride = frame.stride(2);
    if y_stride == 0 || u_stride == 0 || v_stride == 0 {
        return Err(anyhow!("Decoded frame has invalid strides"));
    }

    if y_plane.is_empty() || u_plane.is_empty() || v_plane.is_empty() {
        return Err(anyhow!("Decoded frame missing plane data"));
    }

    // Output packed BGR
    let mut out = vec![0u8; w * h * 3];

    // BT.601 limited-range conversion (common for MPEG-1/2)
    // Integer approximation to keep it fast on 3DS.
    for y in 0..h {
        let y_row = y * y_stride;
        let uv_row = (y / 2) * u_stride;
        let uv_row_v = (y / 2) * v_stride;

        for x in 0..w {
            let yy = y_plane[y_row + x] as i32;
            let uu = u_plane[uv_row + (x / 2)] as i32;
            let vv = v_plane[uv_row_v + (x / 2)] as i32;

            // Convert to signed/offset domain. (16..235) luma, (16..240) chroma
            let c = yy - 16;
            let d = uu - 128;
            let e = vv - 128;

            // Scale/convert
            let mut r = (298 * c + 409 * e + 128) >> 8;
            let mut g = (298 * c - 100 * d - 208 * e + 128) >> 8;
            let mut b = (298 * c + 516 * d + 128) >> 8;

            // Clamp
            r = r.clamp(0, 255);
            g = g.clamp(0, 255);
            b = b.clamp(0, 255);

            let o = (y * w + x) * 3;
            out[o] = b as u8;
            out[o + 1] = g as u8;
            out[o + 2] = r as u8;
        }
    }

    Ok(out)
}
