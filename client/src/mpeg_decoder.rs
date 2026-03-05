use anyhow::{Context, anyhow};
use ffmpeg_next as ffmpeg;
use ffmpeg_next::software::scaling as sws;

/// MPEG1 video decoder using FFMPEG.
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
        .context("Failed to create swscale context")?;

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

        // Wait for the decoder to produce a frame
        match self.decoder.receive_frame(&mut self.video_frame) {
            Ok(()) => {}
            Err(ffmpeg::Error::Other { errno })
                if errno == ffmpeg::sys::EAGAIN
                    || errno == ffmpeg::sys::EOF
                    || errno == libc::ESRCH =>
            {
                // Decoder needs more input before it can output a frame.
                return Ok(None);
            }
            Err(e) => anyhow::bail!("Decoding error: {e}"),
        }

        // At this point, we have a decoded frame in self.video_frame
        if !self.logged_format {
            // For the first frame, log the format to confirm it matches our expectations
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
            .context("swscale conversion failed")?;

        let width = self.bgr_frame.width() as usize;
        let height = self.bgr_frame.height() as usize;
        let stride = self.bgr_frame.stride(0);
        let data = self.bgr_frame.data(0);
        if width == 0 || height == 0 || stride == 0 || data.is_empty() {
            anyhow::bail!("swscale produced empty/invalid BGR frame");
        }

        // Ensure the buffer is large enough for row-by-row access.
        let min_needed = stride.saturating_mul(height);
        if data.len() < min_needed {
            anyhow::bail!(
                "swscale produced short buffer: got {}, need at least {}",
                data.len(),
                min_needed
            );
        }

        Ok(Some(BgrFrameView {
            data,
            width,
            height,
            stride,
        }))
    }
}
