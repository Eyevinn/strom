//! AES67 stream discovery via SAP (Session Announcement Protocol).
//!
//! This module provides:
//! - SAP listener to discover Dante/AES67 streams on the network
//! - SAP announcer to advertise Strom's AES67 output streams

pub mod sap;
pub mod types;

use crate::events::EventBroadcaster;
use socket2::{Domain, Protocol, Socket, Type};
use std::collections::HashMap;
use std::net::{IpAddr, Ipv4Addr, SocketAddr, SocketAddrV4};
use std::sync::Arc;
use std::time::{Duration, Instant};
use strom_types::{FlowId, StromEvent};
use tokio::net::UdpSocket;
use tokio::sync::{broadcast, RwLock};
use tracing::{debug, error, info, warn};

pub use types::{
    AnnouncedStream, AudioEncoding, DiscoveredStream, DiscoveredStreamResponse, DiscoverySource,
    SdpStreamInfo, DEFAULT_STREAM_TTL, SAP_ANNOUNCE_INTERVAL, SAP_MULTICAST_ADDR, SAP_PORT,
};

use sap::{SapError, SapPacket};

/// Discovery service for AES67 streams.
#[derive(Clone)]
pub struct DiscoveryService {
    inner: Arc<DiscoveryServiceInner>,
}

struct DiscoveryServiceInner {
    /// Discovered streams from SAP announcements.
    discovered_streams: RwLock<HashMap<String, DiscoveredStream>>,
    /// Streams we're announcing.
    announced_streams: RwLock<HashMap<String, AnnouncedStream>>,
    /// Event broadcaster for real-time updates.
    events: EventBroadcaster,
    /// Shutdown signal sender.
    shutdown_tx: RwLock<Option<broadcast::Sender<()>>>,
    /// Socket for sending SAP announcements.
    send_socket: RwLock<Option<Arc<UdpSocket>>>,
    /// Local IP address for announcements.
    local_ip: RwLock<Option<IpAddr>>,
}

impl DiscoveryService {
    /// Create a new discovery service.
    pub fn new(events: EventBroadcaster) -> Self {
        Self {
            inner: Arc::new(DiscoveryServiceInner {
                discovered_streams: RwLock::new(HashMap::new()),
                announced_streams: RwLock::new(HashMap::new()),
                events,
                shutdown_tx: RwLock::new(None),
                send_socket: RwLock::new(None),
                local_ip: RwLock::new(None),
            }),
        }
    }

    /// Start the discovery service (listener and announcer).
    pub async fn start(&self) -> anyhow::Result<()> {
        info!("Starting SAP discovery service");

        // Check if already running
        {
            let shutdown = self.inner.shutdown_tx.read().await;
            if shutdown.is_some() {
                warn!("Discovery service already running");
                return Ok(());
            }
        }

        // Create shutdown channel
        let (shutdown_tx, _) = broadcast::channel::<()>(1);
        {
            let mut tx = self.inner.shutdown_tx.write().await;
            *tx = Some(shutdown_tx.clone());
        }

        // Get local IP for announcements
        let local_ip = Self::get_local_ip().unwrap_or(IpAddr::V4(Ipv4Addr::new(0, 0, 0, 0)));
        {
            let mut ip = self.inner.local_ip.write().await;
            *ip = Some(local_ip);
        }
        info!("Using local IP for SAP announcements: {}", local_ip);

        // Create multicast socket for receiving
        let recv_socket = match Self::create_multicast_socket() {
            Ok(s) => s,
            Err(e) => {
                error!("Failed to create multicast socket: {}", e);
                return Err(e.into());
            }
        };

        // Create socket for sending
        let send_socket = match Self::create_send_socket() {
            Ok(s) => Arc::new(s),
            Err(e) => {
                error!("Failed to create send socket: {}", e);
                return Err(e.into());
            }
        };

        {
            let mut sock = self.inner.send_socket.write().await;
            *sock = Some(send_socket.clone());
        }

        // Start listener task
        let listener_inner = self.inner.clone();
        let listener_shutdown = shutdown_tx.subscribe();
        tokio::spawn(async move {
            Self::run_listener(recv_socket, listener_inner, listener_shutdown).await;
        });

        // Start announcer task
        let announcer_inner = self.inner.clone();
        let announcer_shutdown = shutdown_tx.subscribe();
        tokio::spawn(async move {
            Self::run_announcer(send_socket, announcer_inner, announcer_shutdown).await;
        });

        // Start cleanup task
        let cleanup_inner = self.inner.clone();
        let cleanup_shutdown = shutdown_tx.subscribe();
        tokio::spawn(async move {
            Self::run_cleanup(cleanup_inner, cleanup_shutdown).await;
        });

        info!("SAP discovery service started");
        Ok(())
    }

    /// Stop the discovery service.
    pub async fn stop(&self) {
        info!("Stopping SAP discovery service");

        // Send deletion messages for all announced streams
        self.send_all_deletions().await;

        // Signal shutdown
        let tx = {
            let mut shutdown = self.inner.shutdown_tx.write().await;
            shutdown.take()
        };

        if let Some(tx) = tx {
            let _ = tx.send(());
        }

        // Clear state
        {
            let mut sock = self.inner.send_socket.write().await;
            *sock = None;
        }

        info!("SAP discovery service stopped");
    }

    /// Get all discovered streams.
    pub async fn get_streams(&self) -> Vec<DiscoveredStream> {
        let streams = self.inner.discovered_streams.read().await;
        streams.values().cloned().collect()
    }

    /// Get a specific discovered stream by ID.
    pub async fn get_stream(&self, id: &str) -> Option<DiscoveredStream> {
        let streams = self.inner.discovered_streams.read().await;
        streams.get(id).cloned()
    }

    /// Get the raw SDP for a discovered stream.
    pub async fn get_stream_sdp(&self, id: &str) -> Option<String> {
        let streams = self.inner.discovered_streams.read().await;
        streams.get(id).map(|s| s.sdp.clone())
    }

    /// Register a stream for SAP announcement.
    pub async fn announce_stream(&self, flow_id: FlowId, block_id: &str, sdp: &str) {
        let key = AnnouncedStream::key(&flow_id, block_id);
        let msg_id_hash = types::generate_msg_id_hash(&flow_id, block_id);

        let local_ip = {
            let ip = self.inner.local_ip.read().await;
            ip.unwrap_or(IpAddr::V4(Ipv4Addr::new(0, 0, 0, 0)))
        };

        let stream = AnnouncedStream {
            flow_id,
            block_id: block_id.to_string(),
            msg_id_hash,
            sdp: sdp.to_string(),
            origin_ip: local_ip,
            last_announced: Instant::now() - SAP_ANNOUNCE_INTERVAL, // Force immediate announcement
        };

        info!(
            "Registering stream for SAP announcement: {} (hash: {:04x})",
            key, msg_id_hash
        );

        // Send initial announcement immediately
        if let Err(e) = self.send_announcement(&stream).await {
            warn!("Failed to send initial SAP announcement: {}", e);
        }

        let mut announced = self.inner.announced_streams.write().await;
        announced.insert(key, stream);
    }

    /// Remove a stream from SAP announcements.
    pub async fn remove_announcement(&self, flow_id: FlowId, block_id: &str) {
        let key = AnnouncedStream::key(&flow_id, block_id);

        let stream = {
            let mut announced = self.inner.announced_streams.write().await;
            announced.remove(&key)
        };

        if let Some(stream) = stream {
            info!("Removing SAP announcement: {}", key);

            // Send deletion message
            if let Err(e) = self.send_deletion(&stream).await {
                warn!("Failed to send SAP deletion: {}", e);
            }
        }
    }

    /// Get all announced streams.
    pub async fn get_announced_streams(&self) -> Vec<AnnouncedStream> {
        let announced = self.inner.announced_streams.read().await;
        announced.values().cloned().collect()
    }

    // --- Internal methods ---

    /// Create a multicast socket for receiving SAP announcements.
    fn create_multicast_socket() -> std::io::Result<UdpSocket> {
        let socket = Socket::new(Domain::IPV4, Type::DGRAM, Some(Protocol::UDP))?;

        // Allow address reuse
        socket.set_reuse_address(true)?;

        // Bind to SAP port
        let bind_addr = SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, SAP_PORT);
        socket.bind(&bind_addr.into())?;

        // Join multicast group
        let multicast_addr: Ipv4Addr = SAP_MULTICAST_ADDR.parse().unwrap();
        socket.join_multicast_v4(&multicast_addr, &Ipv4Addr::UNSPECIFIED)?;

        // Set non-blocking for tokio
        socket.set_nonblocking(true)?;

        // Convert to tokio UdpSocket
        let std_socket: std::net::UdpSocket = socket.into();
        UdpSocket::from_std(std_socket)
    }

    /// Create a socket for sending SAP announcements.
    fn create_send_socket() -> std::io::Result<UdpSocket> {
        let socket = Socket::new(Domain::IPV4, Type::DGRAM, Some(Protocol::UDP))?;

        // Set multicast TTL
        socket.set_multicast_ttl_v4(32)?;

        // Bind to any port
        let bind_addr = SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, 0);
        socket.bind(&bind_addr.into())?;

        // Set non-blocking
        socket.set_nonblocking(true)?;

        let std_socket: std::net::UdpSocket = socket.into();
        UdpSocket::from_std(std_socket)
    }

    /// Get the local IP address for announcements.
    fn get_local_ip() -> Option<IpAddr> {
        use network_interface::{NetworkInterface, NetworkInterfaceConfig};

        let interfaces = NetworkInterface::show().ok()?;

        for iface in interfaces {
            // Skip loopback
            if iface.name.starts_with("lo") {
                continue;
            }

            for addr in iface.addr {
                if let network_interface::Addr::V4(v4) = addr {
                    let ip = v4.ip;
                    // Skip loopback and link-local addresses
                    if !ip.is_loopback() && !ip.is_link_local() {
                        return Some(IpAddr::V4(ip));
                    }
                }
            }
        }

        None
    }

    /// Run the SAP listener loop.
    async fn run_listener(
        socket: UdpSocket,
        inner: Arc<DiscoveryServiceInner>,
        mut shutdown: broadcast::Receiver<()>,
    ) {
        let mut buf = [0u8; 4096];

        loop {
            tokio::select! {
                _ = shutdown.recv() => {
                    debug!("SAP listener shutting down");
                    break;
                }
                result = socket.recv_from(&mut buf) => {
                    match result {
                        Ok((len, addr)) => {
                            Self::handle_sap_packet(&buf[..len], addr, &inner).await;
                        }
                        Err(e) => {
                            warn!("Error receiving SAP packet: {}", e);
                        }
                    }
                }
            }
        }
    }

    /// Handle a received SAP packet.
    async fn handle_sap_packet(data: &[u8], addr: SocketAddr, inner: &Arc<DiscoveryServiceInner>) {
        let packet = match SapPacket::parse(data) {
            Ok(p) => p,
            Err(e) => {
                debug!("Failed to parse SAP packet from {}: {}", addr, e);
                return;
            }
        };

        let session_id = packet.session_id();

        if packet.is_deletion() {
            // Handle deletion
            Self::handle_deletion(&session_id, inner).await;
        } else {
            // Handle announcement
            Self::handle_announcement(packet, inner).await;
        }
    }

    /// Handle a SAP announcement.
    async fn handle_announcement(packet: SapPacket, inner: &Arc<DiscoveryServiceInner>) {
        // Parse SDP
        let sdp_info = match SdpStreamInfo::parse(&packet.payload) {
            Some(info) => info,
            None => {
                debug!("Failed to parse SDP from SAP packet");
                return;
            }
        };

        let stream_id = sdp_info.generate_id(&packet.origin);
        let now = Instant::now();

        let mut streams = inner.discovered_streams.write().await;

        let is_new = !streams.contains_key(&stream_id);

        let stream = DiscoveredStream {
            id: stream_id.clone(),
            name: sdp_info.name.clone(),
            source: DiscoverySource::Sap {
                origin_ip: packet.origin,
                msg_id_hash: packet.msg_id_hash,
            },
            sdp: packet.payload.clone(),
            multicast_address: sdp_info
                .connection_address
                .unwrap_or(IpAddr::V4(Ipv4Addr::UNSPECIFIED)),
            port: sdp_info.port.unwrap_or(5004),
            channels: sdp_info.channels.unwrap_or(2),
            sample_rate: sdp_info.sample_rate.unwrap_or(48000),
            encoding: sdp_info.encoding,
            origin_host: sdp_info.origin_address.clone(),
            first_seen: streams.get(&stream_id).map(|s| s.first_seen).unwrap_or(now),
            last_seen: now,
            ttl: DEFAULT_STREAM_TTL,
        };

        streams.insert(stream_id.clone(), stream);

        // Broadcast event
        if is_new {
            info!(
                "Discovered new AES67 stream: {} ({}) from {}",
                sdp_info.name, stream_id, packet.origin
            );
            inner.events.broadcast(StromEvent::StreamDiscovered {
                stream_id: stream_id.clone(),
                name: sdp_info.name,
                source: "SAP".to_string(),
            });
        } else {
            debug!("Updated existing stream: {}", stream_id);
            inner.events.broadcast(StromEvent::StreamUpdated {
                stream_id: stream_id.clone(),
            });
        }
    }

    /// Handle a SAP deletion.
    async fn handle_deletion(session_id: &str, inner: &Arc<DiscoveryServiceInner>) {
        let mut streams = inner.discovered_streams.write().await;

        // Find stream by session ID (origin:hash)
        let to_remove: Vec<String> = streams
            .iter()
            .filter(|(_, stream)| {
                if let DiscoverySource::Sap {
                    origin_ip,
                    msg_id_hash,
                } = &stream.source
                {
                    let stream_session_id = format!("{}:{}", origin_ip, msg_id_hash);
                    stream_session_id == session_id
                } else {
                    false
                }
            })
            .map(|(id, _)| id.clone())
            .collect();

        for stream_id in to_remove {
            if let Some(stream) = streams.remove(&stream_id) {
                info!("Stream deleted via SAP: {} ({})", stream.name, stream_id);
                inner.events.broadcast(StromEvent::StreamRemoved {
                    stream_id: stream_id.clone(),
                });
            }
        }
    }

    /// Run the SAP announcer loop.
    async fn run_announcer(
        socket: Arc<UdpSocket>,
        inner: Arc<DiscoveryServiceInner>,
        mut shutdown: broadcast::Receiver<()>,
    ) {
        let mut interval = tokio::time::interval(Duration::from_secs(5));

        loop {
            tokio::select! {
                _ = shutdown.recv() => {
                    debug!("SAP announcer shutting down");
                    break;
                }
                _ = interval.tick() => {
                    Self::send_pending_announcements(&socket, &inner).await;
                }
            }
        }
    }

    /// Send announcements for streams that are due.
    async fn send_pending_announcements(socket: &UdpSocket, inner: &Arc<DiscoveryServiceInner>) {
        let mut announced = inner.announced_streams.write().await;

        let dest = SocketAddr::new(IpAddr::V4(SAP_MULTICAST_ADDR.parse().unwrap()), SAP_PORT);

        for stream in announced.values_mut() {
            if stream.last_announced.elapsed() >= SAP_ANNOUNCE_INTERVAL {
                let packet =
                    SapPacket::build(stream.origin_ip, stream.msg_id_hash, &stream.sdp, false);

                match socket.send_to(&packet, dest).await {
                    Ok(_) => {
                        debug!(
                            "Sent SAP announcement for {}:{} ({} bytes)",
                            stream.flow_id,
                            stream.block_id,
                            packet.len()
                        );
                        stream.last_announced = Instant::now();
                    }
                    Err(e) => {
                        warn!("Failed to send SAP announcement: {}", e);
                    }
                }
            }
        }
    }

    /// Send a single announcement.
    async fn send_announcement(&self, stream: &AnnouncedStream) -> Result<(), SapError> {
        let socket = {
            let sock = self.inner.send_socket.read().await;
            sock.clone()
        };

        let Some(socket) = socket else {
            return Err(SapError::InvalidPayload);
        };

        let dest = SocketAddr::new(IpAddr::V4(SAP_MULTICAST_ADDR.parse().unwrap()), SAP_PORT);

        let packet = SapPacket::build(stream.origin_ip, stream.msg_id_hash, &stream.sdp, false);

        socket
            .send_to(&packet, dest)
            .await
            .map_err(|_| SapError::InvalidPayload)?;

        debug!(
            "Sent SAP announcement for {}:{} ({} bytes)",
            stream.flow_id,
            stream.block_id,
            packet.len()
        );

        Ok(())
    }

    /// Send a deletion message for a stream.
    async fn send_deletion(&self, stream: &AnnouncedStream) -> Result<(), SapError> {
        let socket = {
            let sock = self.inner.send_socket.read().await;
            sock.clone()
        };

        let Some(socket) = socket else {
            return Err(SapError::InvalidPayload);
        };

        let dest = SocketAddr::new(IpAddr::V4(SAP_MULTICAST_ADDR.parse().unwrap()), SAP_PORT);

        let packet = SapPacket::build(stream.origin_ip, stream.msg_id_hash, &stream.sdp, true);

        socket
            .send_to(&packet, dest)
            .await
            .map_err(|_| SapError::InvalidPayload)?;

        info!(
            "Sent SAP deletion for {}:{}",
            stream.flow_id, stream.block_id
        );

        Ok(())
    }

    /// Send deletion messages for all announced streams.
    async fn send_all_deletions(&self) {
        let streams: Vec<AnnouncedStream> = {
            let announced = self.inner.announced_streams.read().await;
            announced.values().cloned().collect()
        };

        for stream in streams {
            if let Err(e) = self.send_deletion(&stream).await {
                warn!("Failed to send deletion for {}: {}", stream.block_id, e);
            }
        }
    }

    /// Run the cleanup loop for expired streams.
    async fn run_cleanup(inner: Arc<DiscoveryServiceInner>, mut shutdown: broadcast::Receiver<()>) {
        let mut interval = tokio::time::interval(Duration::from_secs(30));

        loop {
            tokio::select! {
                _ = shutdown.recv() => {
                    debug!("SAP cleanup task shutting down");
                    break;
                }
                _ = interval.tick() => {
                    Self::cleanup_expired(&inner).await;
                }
            }
        }
    }

    /// Remove expired streams.
    async fn cleanup_expired(inner: &Arc<DiscoveryServiceInner>) {
        let mut streams = inner.discovered_streams.write().await;

        let expired: Vec<String> = streams
            .iter()
            .filter(|(_, s)| s.is_expired())
            .map(|(id, _)| id.clone())
            .collect();

        for stream_id in expired {
            if let Some(stream) = streams.remove(&stream_id) {
                info!(
                    "Stream expired: {} ({}) - last seen {}s ago",
                    stream.name,
                    stream_id,
                    stream.last_seen.elapsed().as_secs()
                );
                inner.events.broadcast(StromEvent::StreamRemoved {
                    stream_id: stream_id.clone(),
                });
            }
        }
    }
}

impl Default for DiscoveryService {
    fn default() -> Self {
        Self::new(EventBroadcaster::default())
    }
}
