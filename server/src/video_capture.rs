use anyhow::Context;
use tokio::task::spawn_blocking;
use tracing::{debug, error};

/// This struct is responsible for capturing raw frames from the monitor.
/// It spawns a separate thread to perform the capture. Read the frames using `frame_receiver`.
pub struct VideoCapture {
    pub monitor: xcap::Monitor,
    pub video_recorder: xcap::VideoRecorder,
    pub frame_receiver: tokio::sync::watch::Receiver<xcap::Frame>,
}

impl VideoCapture {
    pub fn start() -> anyhow::Result<Self> {
        // Set up display capture
        let monitors = xcap::Monitor::all().context("Failed to enumerate displays for capture")?;
        let monitor = monitors
            .into_iter()
            .next()
            .context("No displays found for capture")?;
        let (video_recorder, video_stream) = monitor
            .video_recorder()
            .context("Failed to create display video recorder")?;
        video_recorder
            .start()
            .context("Failed to start display video recorder")?;

        // Convert the video stream to an async channel
        let empty_frame = xcap::Frame::new(0, 0, Vec::new());
        let (frame_sender, frame_receiver) = tokio::sync::watch::channel(empty_frame);
        spawn_blocking(move || {
            for frame in video_stream {
                if frame_sender.send(frame).is_err() {
                    error!("Failed to send frame to async channel, receiver was closed");
                    break;
                }
            }
            debug!("Video capture thread exiting since video stream ended");
        });

        Ok(Self {
            monitor,
            video_recorder,
            frame_receiver,
        })
    }
}
