use ffmpeg_next as ffmpeg;
use rtp_types::RtpPacket;
use std::cmp::min;
use anyhow::anyhow;

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

pub struct Mpeg1FrameReassembler {
    decoder: ffmpeg::codec::decoder::Video,
    video_frame: ffmpeg::util::frame::Video,
}

impl Mpeg1FrameReassembler {
    pub fn new() -> anyhow::Result<Self> {
        // Initialize FFmpeg libraries
        ffmpeg::init()?;

        // Find the MPEG1 decoder
        let codec = ffmpeg::decoder::find(ffmpeg::codec::Id::MPEG1VIDEO)
            .ok_or_else(|| anyhow!("MPEG1 decoder not found"))?;

        let context = ffmpeg::codec::context::Context::new_with_codec(codec);
        let decoder = context.decoder().video()?;
        let video_frame = ffmpeg::util::frame::Video::empty();

        Ok(Self { decoder, video_frame })
    }
}

impl FrameReassembler for Mpeg1FrameReassembler {
    fn process_packet(&mut self, packet_buf: &[u8]) -> anyhow::Result<Option<Vec<u8>>> {
        let packet = ffmpeg::codec::packet::Packet::borrow(packet_buf);

        // Send the packet to the decoder
        self.decoder.send_packet(&packet)?;

        // Try to receive a decoded frame
        // Note: Decoders may require multiple packets before returning a frame (B-frames)
        // or one packet might contain multiple frames.
        match self.decoder.receive_frame(&mut self.video_frame) {
            Ok(()) => {
                // For demonstration, we convert the YUV data to a simple Vec<u8>.
                // In a real app, you might want to convert to RGB or use the planes directly.
                let data = self.extract_raw_frame_data();
                Ok(Some(data))
            }
            Err(ffmpeg::Error::Other { errno }) if errno == ffmpeg::sys::EAGAIN => {
                // Decoder needs more data to produce a frame
                Ok(None)
            }
            Err(e) => Err(anyhow!("Decoding error: {}", e)),
        }
    }
}

impl Mpeg1FrameReassembler {
    fn extract_raw_frame_data(&self) -> Vec<u8> {
        // Simple extraction of YUV420P data (most common for MPEG-1)
        let mut buffer = Vec::new();
        for i in 0..3 {
            let data = self.video_frame.data(i);
            let stride = self.video_frame.stride(i);
            let width = if i == 0 { self.video_frame.width() } else { self.video_frame.width() / 2 };
            let height = if i == 0 { self.video_frame.height() } else { self.video_frame.height() / 2 };

            for y in 0..height as usize {
                let start = y * stride;
                let end = start + width as usize;
                buffer.extend_from_slice(&data[start..end]);
            }
        }
        buffer
    }
}
