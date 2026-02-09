//! Optional TLS support for the Strom server.
//!
//! Provides a [`TlsListener`] that implements [`axum::serve::Listener`],
//! wrapping a TCP listener with a rustls TLS acceptor.

use std::io;
use std::net::SocketAddr;
use std::path::Path;
use std::sync::Arc;

use rustls::pki_types::{pem::PemObject, CertificateDer, PrivateKeyDer};
use rustls::ServerConfig;
use tokio::net::{TcpListener, TcpStream};
use tokio_rustls::server::TlsStream;
use tokio_rustls::TlsAcceptor;
use tracing::{debug, error, info};

/// A TLS-wrapping listener that implements [`axum::serve::Listener`].
pub struct TlsListener {
    tcp: TcpListener,
    acceptor: TlsAcceptor,
}

impl TlsListener {
    pub fn new(tcp: TcpListener, acceptor: TlsAcceptor) -> Self {
        Self { tcp, acceptor }
    }
}

impl axum::serve::Listener for TlsListener {
    type Io = TlsStream<TcpStream>;
    type Addr = SocketAddr;

    async fn accept(&mut self) -> (Self::Io, Self::Addr) {
        loop {
            let (stream, addr) = loop {
                match self.tcp.accept().await {
                    Ok(conn) => break conn,
                    Err(e) => {
                        if is_connection_error(&e) {
                            continue;
                        }
                        error!("TCP accept error: {}", e);
                        tokio::time::sleep(std::time::Duration::from_secs(1)).await;
                    }
                }
            };

            match self.acceptor.accept(stream).await {
                Ok(tls) => return (tls, addr),
                Err(e) => {
                    debug!("TLS handshake failed from {}: {}", addr, e);
                    continue;
                }
            }
        }
    }

    fn local_addr(&self) -> io::Result<Self::Addr> {
        self.tcp.local_addr()
    }
}

fn is_connection_error(e: &io::Error) -> bool {
    matches!(
        e.kind(),
        io::ErrorKind::ConnectionRefused
            | io::ErrorKind::ConnectionAborted
            | io::ErrorKind::ConnectionReset
    )
}

/// Load TLS certificate and key from PEM files, returning a [`TlsAcceptor`].
pub fn load_tls_config(cert_path: &Path, key_path: &Path) -> anyhow::Result<TlsAcceptor> {
    info!("Loading TLS certificate from {}", cert_path.display());
    info!("Loading TLS private key from {}", key_path.display());

    let certs: Vec<CertificateDer<'static>> = CertificateDer::pem_file_iter(cert_path)
        .map_err(|e| anyhow::anyhow!("Failed to read TLS cert '{}': {}", cert_path.display(), e))?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| anyhow::anyhow!("Failed to parse TLS cert: {}", e))?;

    if certs.is_empty() {
        anyhow::bail!("No certificates found in '{}'", cert_path.display());
    }
    info!("Loaded {} certificate(s)", certs.len());

    let key = PrivateKeyDer::from_pem_file(key_path)
        .map_err(|e| anyhow::anyhow!("Failed to read TLS key '{}': {}", key_path.display(), e))?;

    // Ensure ring crypto provider is installed (rustls 0.23 requires explicit selection)
    let _ = rustls::crypto::ring::default_provider().install_default();

    let config = ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(certs, key)
        .map_err(|e| anyhow::anyhow!("Invalid TLS configuration: {}", e))?;

    Ok(TlsAcceptor::from(Arc::new(config)))
}
