use anyhow::Context;
use ffmpeg_next as ffmpeg;
use tracing::error;

const N3DS_TOP_WIDTH: u32 = 400;
const N3DS_TOP_HEIGHT: u32 = 240;

/// This struct is responsible for taking raw monitor frames and encoding them into MPEG-1 video
/// frames. It uses FFMPEG to perform the encoding, including other steps such as downscaling the
/// frames to the lower resolution of the 3DS.
pub struct Mpeg1Encoder {
    encoder: ffmpeg::encoder::video::Encoder,
    filter_graph: FfmpegFilterGraph,
    filtered_frame: ffmpeg::util::frame::video::Video,
    monitor_width: u32,
    monitor_height: u32,
    /// The number of frames sent per second. This must be equal or lower than the encoder FPS.
    effective_fps: usize,
    /// The numerator of the configured encoder FPS. This must be a supported MPEG-1 framerate.
    encoder_fps: usize,
    /// Fractional PTS accumulator numerator (ticks * EFFECTIVE_FPS).
    /// Each output frame advances pts_numerator by ENCODER_FPS, and pts is computed as pts_numerator / EFFECTIVE_FPS.
    pts_numerator: usize,
}

struct FfmpegFilterGraph {
    _graph: ffmpeg::filter::Graph,
    input: ffmpeg::filter::Context,
    output: ffmpeg::filter::Context,
}

impl Mpeg1Encoder {
    pub fn new(
        effective_fps: usize,
        encoder_fps: usize,
        monitor_width: u32,
        monitor_height: u32,
    ) -> anyhow::Result<Self> {
        let encoder = Self::create_encoder(encoder_fps)?;
        let filter_graph = Self::create_filter_graph(monitor_width, monitor_height, effective_fps)?;
        let filtered_frame = ffmpeg::util::frame::Video::new(
            ffmpeg::format::Pixel::YUV420P,
            N3DS_TOP_HEIGHT,
            N3DS_TOP_WIDTH,
        );

        Ok(Self {
            encoder,
            filter_graph,
            filtered_frame,
            monitor_width,
            monitor_height,
            effective_fps,
            encoder_fps,
            pts_numerator: 0,
        })
    }

    /// Create MPEG-1 encoder using ffmpeg (rotated dimensions since 3DS top screen is portrait)
    fn create_encoder(encoder_fps: usize) -> anyhow::Result<ffmpeg::encoder::video::Encoder> {
        // Find the MPEG-1 video encoder
        let codec = ffmpeg::encoder::find(ffmpeg::codec::Id::MPEG1VIDEO)
            .context("Failed to find MPEG-1 codec")?;
        let mut encoder = ffmpeg::codec::context::Context::new_with_codec(codec)
            .encoder()
            .video()
            .context("Failed to create video encoder")?;

        // Set encoder parameters (rotated dimensions since 3DS top screen is portrait)
        encoder.set_width(N3DS_TOP_HEIGHT);
        encoder.set_height(N3DS_TOP_WIDTH);
        encoder.set_format(ffmpeg::format::Pixel::YUV420P);

        // Set the encoder framerate (must be a supported MPEG-1 framerate)
        encoder.set_frame_rate(Some((encoder_fps as i32, 1)));
        encoder.set_time_base((1, encoder_fps as i32));

        // Set quality parameters
        encoder.set_bit_rate(1_000_000); // 1 Mbps bitrate
        encoder.set_gop(20); // How many frames between keyframes (I-frames).
        encoder.set_max_b_frames(0); // B-frames introduce latency due to frame reordering.

        let encoder = encoder.open().context("Failed to open encoder")?;
        Ok(encoder)
    }

    /// Create a filter graph to crop and rotate the captured frames to match the 3DS top screen.
    /// It also converts the pixel format to YUV420P which is required by the MPEG-1 encoder.
    fn create_filter_graph(
        monitor_width: u32,
        monitor_height: u32,
        effective_fps: usize,
    ) -> anyhow::Result<FfmpegFilterGraph> {
        let mut graph = ffmpeg::filter::Graph::new();
        let args = format!(
            "video_size={}x{}:pix_fmt=rgba:time_base=1/{}:pixel_aspect=1/1",
            monitor_width, monitor_height, effective_fps,
        );

        let input = graph
            .add(&ffmpeg::filter::find("buffer").unwrap(), "in", &args)
            .context("Failed to create buffer source")?;
        let mut output = graph
            .add(&ffmpeg::filter::find("buffersink").unwrap(), "out", "")
            .context("Failed to create buffer sink")?;
        output.set_pixel_format(ffmpeg::format::Pixel::YUV420P);

        let chain = format!(
            "scale={}:{}:flags=bilinear,transpose=1,format=yuv420p",
            N3DS_TOP_WIDTH, N3DS_TOP_HEIGHT
        );

        graph
            .output("in", 0)?
            .input("out", 0)?
            .parse(&chain)
            .context("Failed to parse filter graph")?;
        graph
            .validate()
            .context("Failed to validate filter graph")?;

        Ok(FfmpegFilterGraph {
            _graph: graph,
            input,
            output,
        })
    }

    /// Encode a raw frame into an MPEG-1 video packet. This involves pushing the raw frame through
    /// the filter graph to scale/rotate/convert it, then sending the filtered frame to the encoder
    /// and retrieving the encoded packet.
    ///
    /// Note: The first frame sent to the encoder may return zero packets due to internal buffering,
    /// but subsequent frames should return one packet each.
    pub fn encode_frame(
        &mut self,
        monitor_frame: &xcap::Frame,
    ) -> anyhow::Result<Vec<ffmpeg::Packet>> {
        // PTS is in encoder time_base units (1/ENCODER_FPS_NUM). We advance it fractionally
        // to match EFFECTIVE_FPS.
        let pts = self.pts_numerator / self.effective_fps;
        self.pts_numerator = self.pts_numerator.saturating_add(self.encoder_fps);

        // Capture is RGBA. Allocate a fresh frame, fill it, then submit it.
        // The filter graph takes ownership of refcounted frame buffers.
        let mut src_rgba = ffmpeg::util::frame::Video::new(
            ffmpeg::format::Pixel::RGBA,
            self.monitor_width,
            self.monitor_height,
        );
        self.copy_packed_into_frame(&mut src_rgba, 0, 4, &monitor_frame.raw);
        src_rgba.set_pts(Some(pts as i64));

        // Push through filter graph (scale+transpose+format)
        self.filter_graph
            .input
            .source()
            .add(&src_rgba)
            .context("Failed to push frame into filter graph")?;

        // Drain exactly one output frame for this input.
        self.filter_graph
            .output
            .sink()
            .frame(&mut self.filtered_frame)
            .context("Failed to pull frame from filter graph")?;

        // Encode the frame
        let mut mpeg_packets = Vec::new();

        self.encoder
            .send_frame(&self.filtered_frame)
            .context("Failed to send frame to encoder")?;
        loop {
            let mut encoded_packet = ffmpeg::Packet::empty();
            match self.encoder.receive_packet(&mut encoded_packet) {
                Ok(()) => {
                    mpeg_packets.push(encoded_packet);
                }
                Err(ffmpeg::Error::Other { errno }) if errno == ffmpeg::sys::EAGAIN => {
                    // No more packets available for now
                    break;
                }
                Err(e) => {
                    error!("Encoding error: {}", e);
                    break;
                }
            }
        }

        Ok(mpeg_packets)
    }

    /// Copy pixel data from the captured RGBA frame into the FFMPEG frame buffer.
    fn copy_packed_into_frame(
        &self,
        dst: &mut ffmpeg::util::frame::Video,
        plane: usize,
        bytes_per_pixel: usize,
        src: &[u8],
    ) {
        let dst_stride = dst.stride(plane);
        let row_bytes = self.monitor_width as usize * bytes_per_pixel;
        debug_assert!(src.len() >= row_bytes * self.monitor_height as usize);

        for y in 0..self.monitor_height as usize {
            let src_off = y * row_bytes;
            let dst_off = y * dst_stride;
            dst.data_mut(plane)[dst_off..dst_off + row_bytes]
                .copy_from_slice(&src[src_off..src_off + row_bytes]);
        }
    }
}
