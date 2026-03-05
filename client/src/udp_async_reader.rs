use std::net::UdpSocket;
use std::pin::Pin;
use std::sync::Arc;
use std::task::Poll;
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
        cx: &mut std::task::Context<'_>,
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
