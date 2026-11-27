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
#[cfg(target_os = "linux")]
const FD_WATCHDOG_INTERVAL: Duration = Duration::from_secs(60);

/// Upper bound for the raised soft `RLIMIT_NOFILE`. Where the hard limit is
/// effectively unlimited (`LimitNOFILE=infinity` gives about 2^30), children
/// inherit the soft limit, and tools that size tables from it or `close()` every
/// fd up to it get slow or allocate gigabytes. 2^20 is far above what Strom
/// needs.
#[cfg(unix)]
const FD_LIMIT_CAP: libc::rlim_t = 1 << 20;

/// Raise the soft `RLIMIT_NOFILE` towards the hard limit, capped at
/// [`FD_LIMIT_CAP`] (and on macOS at `kern.maxfilesperproc`, since setrlimit
/// rejects anything above it).
///
/// Every WebRTC viewer holds file descriptors (its ICE sockets, the event fds
/// of webrtcbin's and the ICE agent's main contexts), and so does every
/// connection and pipeline. Containers commonly start with a soft limit of
/// 1024, macOS with 256. When it runs out, a new webrtcbin's thread cannot
/// create its main context and GLib ends the process with `g_error`
/// ("Creating pipes for GWakeup"): a `trap int3` in libglib, without a Rust
/// panic or a log line from Strom.
#[cfg(unix)]
pub fn raise_fd_limit() {
    use tracing::{info, warn};

    let mut limit = libc::rlimit {
        rlim_cur: 0,
        rlim_max: 0,
    };
    // SAFETY: getrlimit writes into the struct it is given.
    if unsafe { libc::getrlimit(libc::RLIMIT_NOFILE, &mut limit) } != 0 {
        warn!(
            "Could not read the file descriptor limit: {}",
            io::Error::last_os_error()
        );
        return;
    }
    let target = fd_limit_target(limit.rlim_max);
    if limit.rlim_cur >= target {
        info!("File descriptor limit: {}", limit.rlim_cur);
        return;
    }
    let previous = limit.rlim_cur;
    limit.rlim_cur = target;
    // SAFETY: setrlimit only reads the struct.
    if unsafe { libc::setrlimit(libc::RLIMIT_NOFILE, &limit) } == 0 {
        info!(
            "File descriptor limit raised from {} to {}",
            previous, limit.rlim_cur
        );
    } else {
        warn!(
            "Could not raise the file descriptor limit from {} to {}: {}",
            previous,
            target,
            io::Error::last_os_error()
        );
    }
}

/// The soft limit to raise to: the hard limit, capped.
#[cfg(unix)]
fn fd_limit_target(hard: libc::rlim_t) -> libc::rlim_t {
    hard.min(FD_LIMIT_CAP).min(platform_fd_max())
}

/// The most this platform lets setrlimit set, below the hard limit.
#[cfg(target_os = "macos")]
fn platform_fd_max() -> libc::rlim_t {
    max_files_per_proc().unwrap_or(MACOS_OPEN_MAX)
}

#[cfg(all(unix, not(target_os = "macos")))]
fn platform_fd_max() -> libc::rlim_t {
    libc::RLIM_INFINITY
}

/// `OPEN_MAX` from `<sys/syslimits.h>`, the fallback when
/// `kern.maxfilesperproc` cannot be read (the libc crate does not export it).
#[cfg(target_os = "macos")]
const MACOS_OPEN_MAX: libc::rlim_t = 10240;

/// `kern.maxfilesperproc`, the most macOS lets setrlimit set.
#[cfg(target_os = "macos")]
fn max_files_per_proc() -> Option<libc::rlim_t> {
    let mut value: libc::c_int = 0;
    let mut size = std::mem::size_of::<libc::c_int>();
    // SAFETY: the name is NUL-terminated, and `value` and `size` describe a
    // c_int buffer that sysctlbyname writes into.
    let ret = unsafe {
        libc::sysctlbyname(
            c"kern.maxfilesperproc".as_ptr(),
            (&mut value as *mut libc::c_int).cast(),
            &mut size,
            std::ptr::null_mut(),
            0,
        )
    };
    (ret == 0 && value > 0).then_some(value as libc::rlim_t)
}

/// No-op outside Unix.
#[cfg(not(unix))]
pub fn raise_fd_limit() {}

/// Usage ratio above which a warning is logged (once per excursion).
#[cfg(target_os = "linux")]
const FD_WARN_RATIO: f64 = 0.80;

/// Usage ratio above which an error is logged on every sample.
#[cfg(target_os = "linux")]
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

    #[cfg(unix)]
    #[test]
    fn fd_limit_target_is_capped() {
        assert_eq!(fd_limit_target(4096), 4096);
        assert!(fd_limit_target(libc::RLIM_INFINITY) <= FD_LIMIT_CAP);
        assert!(fd_limit_target(libc::RLIM_INFINITY) >= 256);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn fd_usage_returns_plausible_values() {
        let (used, limit) = fd_usage().expect("fd_usage should work on Linux");
        assert!(used > 0);
        assert!(limit >= used);
    }
}
