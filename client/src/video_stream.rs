use anyhow::anyhow;
use ffmpeg_next as ffmpeg;
use ffmpeg_next::software::scaling as sws;
use std::net::UdpSocket;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::Duration;
use tokio::io::{AsyncRead, ReadBuf};

/// Wrapper to adapt a UDP socket into an AsyncRead for FramedRead.
///
/// Each `poll_read` yields exactly one UDP datagram (truncated to the ReadBuf capacity).
pub struct UdpSocketAsyncReader {
    inner: Arc<UdpSocket>,
    interval: tokio::time::Interval,
}

impl UdpSocketAsyncReader {
    pub fn new(socket: Arc<UdpSocket>) -> Self {
        socket.set_nonblocking(true).ok();
        Self {
            inner: socket,
            interval: tokio::time::interval(Duration::from_millis(1)),
        }
    }
}

impl AsyncRead for UdpSocketAsyncReader {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        let this = self.get_mut();

        // Wait for the interval to elapse
        if this.interval.poll_tick(cx).is_pending() {
            return Poll::Pending;
        }

        let unfilled = buf.initialize_unfilled();
        match this.inner.recv(unfilled) {
            Ok(n) => {
                if n > unfilled.len() {
                    log::warn!("UDP truncated: {} > {}", n, unfilled.len());
                }
                let n = n.min(unfilled.len());
                buf.advance(n);

                // Request another poll after yielding data
                this.interval.reset_immediately();
                Poll::Ready(Ok(()))
            }
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                this.interval.reset();
                Poll::Pending
            }
            Err(e) => Poll::Ready(Err(e)),
        }
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
