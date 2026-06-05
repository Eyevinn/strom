//! Hardening for the HTTP(S) accept path.
//!
//! Protections that axum-server does not provide by default, added after a
//! production outage: internet scanners left half-open TLS connections behind,
//! each pinning a file descriptor forever, until the process hit its fd limit
//! and `accept()` failed with EMFILE — silently, because axum-server swallows
//! accept errors. The server then looked healthy while refusing all traffic.
//!
//! Layers of defense:
//! - [`KeepaliveAcceptor`] enables TCP keepalive on accepted connections, so
//!   the kernel reclaims connections whose peer vanished without a FIN.
//! - [`FirstByteTimeoutAcceptor`] closes connections that never send a single
//!   byte after being accepted (post-TLS). hyper's `header_read_timeout`
//!   cannot cover this: axum-server serves connections through hyper-util's
//!   auto builder, whose h1/h2 version sniffing reads without a timeout, so
//!   an idle connection parks there forever. The timer disarms on the first
//!   byte, so it never affects established WebSockets or in-flight requests.
//! - [`HEADER_READ_TIMEOUT`] (applied in `main.rs`) reaps idle keep-alive
//!   connections between requests. Upgraded connections (WebSockets) are not
//!   affected.
//! - [`spawn_fd_watchdog`] logs loudly while fd usage approaches the limit,
//!   so fd exhaustion is visible before the server becomes unreachable.

use std::future::{Future, Ready};
use std::io;
use std::pin::Pin;
use std::task::{Context, Poll};
use std::time::Duration;

use axum_server::accept::Accept;
use socket2::{SockRef, TcpKeepalive};
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::net::TcpStream;
use tracing::debug;

/// Idle time before the kernel sends the first TCP keepalive probe.
const KEEPALIVE_TIME: Duration = Duration::from_secs(60);

/// Interval between TCP keepalive probes once probing has started.
const KEEPALIVE_INTERVAL: Duration = Duration::from_secs(15);

/// How long hyper waits for a request head before closing the connection.
/// Also reaps idle keep-alive connections between requests; clients
/// transparently reconnect.
pub const HEADER_READ_TIMEOUT: Duration = Duration::from_secs(30);

/// How long the TLS handshake may take before the connection is dropped.
/// Matches the axum-server default, but set explicitly so the protection
/// does not silently change with upstream defaults.
pub const TLS_HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);

/// How long an accepted (post-TLS) connection may stay silent before it is
/// closed. Disarmed permanently once the connection sends its first byte.
const FIRST_BYTE_TIMEOUT: Duration = Duration::from_secs(30);

/// An acceptor that enables TCP keepalive on every accepted connection.
///
/// Without keepalive, a connection whose peer disappeared without a FIN
/// (crashed scanner, dropped NAT mapping) stays ESTABLISHED forever and pins
/// one file descriptor. With keepalive the kernel detects the dead peer and
/// closes the socket after roughly `KEEPALIVE_TIME` plus a few probe
/// intervals.
#[derive(Clone, Copy, Debug, Default)]
pub struct KeepaliveAcceptor;

impl<S> Accept<TcpStream, S> for KeepaliveAcceptor {
    type Stream = TcpStream;
    type Service = S;
    type Future = Ready<io::Result<(TcpStream, S)>>;

    fn accept(&self, stream: TcpStream, service: S) -> Self::Future {
        let keepalive = TcpKeepalive::new()
            .with_time(KEEPALIVE_TIME)
            .with_interval(KEEPALIVE_INTERVAL);
        if let Err(e) = SockRef::from(&stream).set_tcp_keepalive(&keepalive) {
            // Not fatal: the connection still works, it just loses the
            // dead-peer protection.
            debug!("Failed to set TCP keepalive on accepted connection: {}", e);
        }
        std::future::ready(Ok((stream, service)))
    }
}

/// An acceptor that closes connections that never send a byte.
///
/// Wraps the stream produced by the inner acceptor (e.g. the TLS stream from
/// `RustlsAcceptor`) so that the first read carries a deadline. Scanners that
/// complete the TLS handshake and then go silent are closed after
/// `FIRST_BYTE_TIMEOUT` instead of pinning a file descriptor forever.
#[derive(Clone, Copy, Debug)]
pub struct FirstByteTimeoutAcceptor<A> {
    inner: A,
    timeout: Duration,
}

impl<A> FirstByteTimeoutAcceptor<A> {
    pub fn new(inner: A) -> Self {
        Self {
            inner,
            timeout: FIRST_BYTE_TIMEOUT,
        }
    }

    #[cfg(test)]
    fn with_timeout(inner: A, timeout: Duration) -> Self {
        Self { inner, timeout }
    }
}

impl<I, S, A> Accept<I, S> for FirstByteTimeoutAcceptor<A>
where
    A: Accept<I, S>,
    A::Future: Send + 'static,
    A::Stream: Send + 'static,
    A::Service: Send + 'static,
{
    type Stream = FirstByteTimeoutStream<A::Stream>;
    type Service = A::Service;
    type Future = Pin<Box<dyn Future<Output = io::Result<(Self::Stream, Self::Service)>> + Send>>;

    fn accept(&self, stream: I, service: S) -> Self::Future {
        let inner_future = self.inner.accept(stream, service);
        let timeout = self.timeout;
        Box::pin(async move {
            let (stream, service) = inner_future.await?;
            Ok((FirstByteTimeoutStream::new(stream, timeout), service))
        })
    }
}

/// Stream wrapper that fails reads with `TimedOut` if no byte has arrived
/// within the deadline. The deadline is disarmed by the first received byte,
/// after which the wrapper is a transparent passthrough.
pub struct FirstByteTimeoutStream<S> {
    inner: S,
    deadline: Option<Pin<Box<tokio::time::Sleep>>>,
}

impl<S> FirstByteTimeoutStream<S> {
    fn new(inner: S, timeout: Duration) -> Self {
        Self {
            inner,
            deadline: Some(Box::pin(tokio::time::sleep(timeout))),
        }
    }
}

impl<S: AsyncRead + Unpin> AsyncRead for FirstByteTimeoutStream<S> {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        if let Some(deadline) = this.deadline.as_mut() {
            if deadline.as_mut().poll(cx).is_ready() {
                return Poll::Ready(Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "connection sent no data within the first-byte timeout",
                )));
            }
        }
        let filled_before = buf.filled().len();
        let result = Pin::new(&mut this.inner).poll_read(cx, buf);
        if matches!(result, Poll::Ready(Ok(()))) && buf.filled().len() > filled_before {
            this.deadline = None;
        }
        result
    }
}

impl<S: AsyncWrite + Unpin> AsyncWrite for FirstByteTimeoutStream<S> {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        Pin::new(&mut self.get_mut().inner).poll_write(cx, buf)
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.get_mut().inner).poll_flush(cx)
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.get_mut().inner).poll_shutdown(cx)
    }

    fn poll_write_vectored(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        bufs: &[io::IoSlice<'_>],
    ) -> Poll<io::Result<usize>> {
        Pin::new(&mut self.get_mut().inner).poll_write_vectored(cx, bufs)
    }

    fn is_write_vectored(&self) -> bool {
        self.inner.is_write_vectored()
    }
}

/// How often the fd watchdog samples usage.
const FD_WATCHDOG_INTERVAL: Duration = Duration::from_secs(60);

/// Usage ratio above which a warning is logged (once per excursion).
const FD_WARN_RATIO: f64 = 0.80;

/// Usage ratio above which an error is logged on every sample.
const FD_ERROR_RATIO: f64 = 0.95;

/// Current open fd count and the soft `RLIMIT_NOFILE` limit.
/// Returns `None` if either cannot be determined.
#[cfg(target_os = "linux")]
fn fd_usage() -> Option<(usize, usize)> {
    let used = std::fs::read_dir("/proc/self/fd").ok()?.count();
    let limits = std::fs::read_to_string("/proc/self/limits").ok()?;
    let soft_limit = limits
        .lines()
        .find(|line| line.starts_with("Max open files"))?
        .split_whitespace()
        .nth(3)?
        .parse::<usize>()
        .ok()?;
    Some((used, soft_limit))
}

/// Spawn a background task that monitors file descriptor usage.
///
/// axum-server retries `accept()` errors (including EMFILE) silently, so an
/// fd-exhausted server keeps running but never answers another connection.
/// This watchdog makes the approach to that state visible: a warning when
/// usage crosses 80% of the soft limit, an error every minute at 95%+.
#[cfg(target_os = "linux")]
pub fn spawn_fd_watchdog() {
    use tracing::{error, info, warn};

    tokio::spawn(async move {
        let mut above_warn = false;
        loop {
            if let Some((used, limit)) = fd_usage() {
                let ratio = used as f64 / limit as f64;
                if ratio >= FD_ERROR_RATIO {
                    error!(
                        "File descriptor usage critical: {}/{} ({:.0}%) — when exhausted, accept() fails with EMFILE and the server becomes unreachable without crashing",
                        used,
                        limit,
                        ratio * 100.0
                    );
                    above_warn = true;
                } else if ratio >= FD_WARN_RATIO {
                    if !above_warn {
                        warn!(
                            "File descriptor usage high: {}/{} ({:.0}%) — possible fd leak, check for accumulating connections",
                            used,
                            limit,
                            ratio * 100.0
                        );
                        above_warn = true;
                    }
                } else if above_warn {
                    info!("File descriptor usage back to normal: {}/{}", used, limit);
                    above_warn = false;
                }
            }
            tokio::time::sleep(FD_WATCHDOG_INTERVAL).await;
        }
    });
}

/// No-op on platforms without `/proc`.
#[cfg(not(target_os = "linux"))]
pub fn spawn_fd_watchdog() {}

#[cfg(test)]
mod tests {
    use super::*;
    use axum_server::accept::DefaultAcceptor;

    #[tokio::test]
    async fn keepalive_acceptor_enables_so_keepalive() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let _client = tokio::net::TcpStream::connect(addr).await.unwrap();
        let (accepted, _) = listener.accept().await.unwrap();

        assert!(!SockRef::from(&accepted).keepalive().unwrap());
        let (stream, ()) = KeepaliveAcceptor.accept(accepted, ()).await.unwrap();
        assert!(SockRef::from(&stream).keepalive().unwrap());
    }

    #[tokio::test(start_paused = true)]
    async fn first_byte_timeout_closes_silent_connection() {
        use tokio::io::AsyncReadExt;

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let _client = tokio::net::TcpStream::connect(addr).await.unwrap();
        let (accepted, _) = listener.accept().await.unwrap();

        let acceptor =
            FirstByteTimeoutAcceptor::with_timeout(DefaultAcceptor, Duration::from_secs(30));
        let (mut stream, ()) = acceptor.accept(accepted, ()).await.unwrap();

        // The client never sends anything; paused time auto-advances past the
        // deadline and the read must fail with TimedOut.
        let err = stream.read_u8().await.unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::TimedOut);
    }

    #[tokio::test(start_paused = true)]
    async fn first_byte_disarms_timeout() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        // In-memory stream: byte delivery is synchronous with the scheduler,
        // so paused-time auto-advance cannot outrun it (unlike real TCP).
        let (mut client, server) = tokio::io::duplex(64);

        let acceptor =
            FirstByteTimeoutAcceptor::with_timeout(DefaultAcceptor, Duration::from_secs(30));
        let (mut stream, ()) = acceptor.accept(server, ()).await.unwrap();

        client.write_u8(42).await.unwrap();
        assert_eq!(stream.read_u8().await.unwrap(), 42);

        // Deadline is disarmed: a subsequent read waits for data instead of
        // failing, even far past the original deadline.
        tokio::time::advance(Duration::from_secs(3600)).await;
        let pending = tokio::time::timeout(Duration::from_secs(1), stream.read_u8()).await;
        assert!(
            pending.is_err(),
            "read should still be pending, not TimedOut"
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn fd_usage_returns_plausible_values() {
        let (used, limit) = fd_usage().expect("fd_usage should work on Linux");
        assert!(used > 0);
        assert!(limit >= used);
    }
}
