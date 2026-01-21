//! Network stream discovery via SAP and mDNS.
//!
//! This module provides:
//! - SAP listener to discover Dante/AES67 streams on the network
//! - SAP announcer to advertise Strom's AES67 output streams
//! - mDNS discovery for RAVENNA, NDI, and other network protocols
//! - RTSP client for fetching SDP from RAVENNA sources

pub mod mdns;
pub mod rtsp_client;
pub mod sap;
pub mod types;

use crate::events::EventBroadcaster;
use socket2::{Domain, Protocol, Socket, Type};
use std::collections::HashMap;
use std::net::{IpAddr, Ipv4Addr, SocketAddr, SocketAddrV4};
use std::str::FromStr;
use std::sync::Arc;
use std::time::{Duration, Instant};
use strom_types::{FlowId, StromEvent};
use tokio::net::UdpSocket;
use tokio::sync::{broadcast, RwLock};
use tracing::{debug, error, info, warn};

pub use types::{
    sap_address_for_stream, AnnouncedStream, AudioEncoding, DiscoveredStream,
    DiscoveredStreamResponse, DiscoverySource, SdpStreamInfo, DEFAULT_STREAM_TTL,
    SAP_ANNOUNCE_INTERVAL, SAP_MULTICAST_ADDRS, SAP_MULTICAST_ADDR_AES67,
    SAP_MULTICAST_ADDR_GLOBAL, SAP_PORT,
};

use mdns::MdnsDiscovery;
use mdns_sd::ServiceEvent;
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
    /// Sockets for sending SAP announcements (one per interface).
    send_sockets: RwLock<Vec<(Ipv4Addr, Arc<UdpSocket>)>>,
    /// Local IP address for announcements.
    local_ip: RwLock<Option<IpAddr>>,
    /// mDNS discovery service.
    mdns_discovery: RwLock<Option<Arc<MdnsDiscovery>>>,
    /// Configured SAP multicast addresses to listen on.
    sap_multicast_addresses: Vec<String>,
}

impl DiscoveryService {
    /// Create a new discovery service.
    ///
    /// `sap_multicast_addresses` specifies which SAP multicast groups to join.
    /// Default is both AES67 (239.255.255.255) and global scope (224.2.127.254).
    pub fn new(events: EventBroadcaster, sap_multicast_addresses: Vec<String>) -> Self {
        // Use defaults if empty
        let sap_addrs = if sap_multicast_addresses.is_empty() {
            SAP_MULTICAST_ADDRS.iter().map(|s| s.to_string()).collect()
        } else {
            sap_multicast_addresses
        };

        Self {
            inner: Arc::new(DiscoveryServiceInner {
                discovered_streams: RwLock::new(HashMap::new()),
                announced_streams: RwLock::new(HashMap::new()),
                events,
                shutdown_tx: RwLock::new(None),
                send_sockets: RwLock::new(Vec::new()),
                local_ip: RwLock::new(None),
                mdns_discovery: RwLock::new(None),
                sap_multicast_addresses: sap_addrs,
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
        let sap_addresses = self.inner.sap_multicast_addresses.clone();
        info!("SAP multicast addresses configured: {:?}", sap_addresses);
        let recv_socket = match Self::create_multicast_socket(&sap_addresses) {
            Ok(s) => s,
            Err(e) => {
                error!("Failed to create multicast socket: {}", e);
                return Err(e.into());
            }
        };

        // Create send sockets for ALL interfaces
        let interface_ips = Self::get_all_interface_ips();
        let mut send_sockets = Vec::new();

        for ip in &interface_ips {
            match Self::create_send_socket_for_interface(*ip) {
                Ok(s) => {
                    info!("Created SAP send socket for interface {}", ip);
                    send_sockets.push((*ip, Arc::new(s)));
                }
                Err(e) => {
                    warn!("Failed to create SAP send socket for {}: {}", ip, e);
                }
            }
        }

        if send_sockets.is_empty() {
            // Fallback: create a default socket
            match Self::create_send_socket_for_interface(Ipv4Addr::UNSPECIFIED) {
                Ok(s) => {
                    warn!("No interface sockets, using default SAP send socket");
                    send_sockets.push((Ipv4Addr::UNSPECIFIED, Arc::new(s)));
                }
                Err(e) => {
                    error!("Failed to create default send socket: {}", e);
                    return Err(e.into());
                }
            }
        }

        {
            let mut socks = self.inner.send_sockets.write().await;
            *socks = send_sockets;
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
            Self::run_announcer(announcer_inner, announcer_shutdown).await;
        });

        // Start cleanup task
        let cleanup_inner = self.inner.clone();
        let cleanup_shutdown = shutdown_tx.subscribe();
        tokio::spawn(async move {
            Self::run_cleanup(cleanup_inner, cleanup_shutdown).await;
        });

        // Start mDNS discovery
        if let Err(e) = self.start_mdns_discovery(shutdown_tx.clone()).await {
            warn!("Failed to start mDNS discovery: {}", e);
        }

        // Start RTSP server for mDNS/RAVENNA announcements
        if let Err(e) = self.start_rtsp_server(shutdown_tx.clone()).await {
            warn!("Failed to start RTSP server: {}", e);
        }

        info!("Discovery service started (SAP + mDNS + RTSP)");
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
            let mut socks = self.inner.send_sockets.write().await;
            socks.clear();
        }

        // Shutdown mDNS
        {
            let mdns_lock = self.inner.mdns_discovery.write().await;
            if let Some(mdns) = mdns_lock.as_ref() {
                if let Err(e) = mdns.shutdown().await {
                    warn!("Failed to shutdown mDNS: {}", e);
                }
            }
        }

        info!("Discovery service stopped (SAP + mDNS)");
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

    /// Get the raw SDP for a discovered or announced stream.
    pub async fn get_stream_sdp(&self, id: &str) -> Option<String> {
        // First check discovered streams
        {
            let streams = self.inner.discovered_streams.read().await;
            if let Some(stream) = streams.get(id) {
                return Some(stream.sdp.clone());
            }
        }

        // Then check announced streams (for RTSP server serving our own streams)
        {
            let announced = self.inner.announced_streams.read().await;
            if let Some(stream) = announced.get(id) {
                return Some(stream.sdp.clone());
            }
        }

        None
    }

    /// Register a stream for SAP and mDNS announcement.
    ///
    /// If `interface` is Some, SAP announcements will only be sent on the
    /// interface with a matching IP address. If None, SAP is sent on all interfaces.
    pub async fn announce_stream(
        &self,
        flow_id: FlowId,
        block_id: &str,
        sdp: &str,
        interface: Option<&str>,
    ) {
        let key = AnnouncedStream::key(&flow_id, block_id);
        let msg_id_hash = types::generate_msg_id_hash(&flow_id, block_id);

        let local_ip = {
            let ip = self.inner.local_ip.read().await;
            ip.unwrap_or(IpAddr::V4(Ipv4Addr::new(0, 0, 0, 0)))
        };

        // Parse SDP to get stream name and multicast address
        let sdp_info = SdpStreamInfo::parse(sdp);
        let stream_name = sdp_info
            .as_ref()
            .map(|info| info.name.clone())
            .unwrap_or_else(|| format!("strom-{}-{}", flow_id, block_id));
        let multicast_address = sdp_info
            .as_ref()
            .and_then(|info| info.connection_address)
            .unwrap_or(IpAddr::V4(Ipv4Addr::new(239, 255, 255, 255)));

        // Resolve interface name to IP address for filtering
        let announce_interface = interface.map(|s| s.to_string());

        let mut stream = AnnouncedStream {
            flow_id,
            block_id: block_id.to_string(),
            msg_id_hash,
            sdp: sdp.to_string(),
            origin_ip: local_ip,
            multicast_address,
            last_announced: Instant::now() - SAP_ANNOUNCE_INTERVAL, // Force immediate announcement
            mdns_fullname: None,
            announce_interface: announce_interface.clone(),
        };

        info!(
            "Registering stream for SAP announcement: {} (hash: {:04x})",
            key, msg_id_hash
        );

        // Send initial SAP announcement immediately
        if let Err(e) = self.send_announcement(&stream).await {
            warn!("Failed to send initial SAP announcement: {}", e);
        }

        // Register with mDNS if available
        if let Some(mdns) = self.inner.mdns_discovery.read().await.as_ref() {
            match self
                .register_mdns_service(&key, &stream_name, local_ip, mdns)
                .await
            {
                Ok(fullname) => {
                    stream.mdns_fullname = Some(fullname.clone());
                    info!("Registered mDNS service: {}", fullname);
                }
                Err(e) => {
                    warn!("Failed to register mDNS service: {}", e);
                }
            }
        }

        let mut announced = self.inner.announced_streams.write().await;
        announced.insert(key, stream);
    }

    /// Remove a stream from SAP and mDNS announcements.
    pub async fn remove_announcement(&self, flow_id: FlowId, block_id: &str) {
        let key = AnnouncedStream::key(&flow_id, block_id);

        let stream = {
            let mut announced = self.inner.announced_streams.write().await;
            announced.remove(&key)
        };

        if let Some(stream) = stream {
            info!("Removing announcement: {}", key);

            // Send SAP deletion message
            if let Err(e) = self.send_deletion(&stream).await {
                warn!("Failed to send SAP deletion: {}", e);
            }

            // Unregister mDNS service if it was registered
            if let Some(fullname) = &stream.mdns_fullname {
                if let Some(mdns) = self.inner.mdns_discovery.read().await.as_ref() {
                    if let Err(e) = mdns.unregister(fullname).await {
                        warn!("Failed to unregister mDNS service {}: {}", fullname, e);
                    } else {
                        info!("Unregistered mDNS service: {}", fullname);
                    }
                }
            }
        }
    }

    /// Get all announced streams.
    pub async fn get_announced_streams(&self) -> Vec<AnnouncedStream> {
        let announced = self.inner.announced_streams.read().await;
        announced.values().cloned().collect()
    }

    // --- Internal methods ---

    /// Get all IPv4 interface addresses (excluding loopback and link-local).
    fn get_all_interface_ips() -> Vec<Ipv4Addr> {
        use network_interface::{NetworkInterface, NetworkInterfaceConfig};

        let mut ips = Vec::new();

        if let Ok(interfaces) = NetworkInterface::show() {
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
                            debug!("Found interface {} with IP {}", iface.name, ip);
                            ips.push(ip);
                        }
                    }
                }
            }
        }

        ips
    }

    /// Get the IPv4 address for a specific network interface.
    fn get_interface_ip(interface_name: &str) -> Option<Ipv4Addr> {
        use network_interface::{NetworkInterface, NetworkInterfaceConfig};

        if let Ok(interfaces) = NetworkInterface::show() {
            for iface in interfaces {
                if iface.name == interface_name {
                    for addr in iface.addr {
                        if let network_interface::Addr::V4(v4) = addr {
                            let ip = v4.ip;
                            if !ip.is_loopback() && !ip.is_link_local() {
                                return Some(ip);
                            }
                        }
                    }
                }
            }
        }

        None
    }

    /// Create a multicast socket for receiving SAP announcements.
    /// Joins configured SAP multicast groups on ALL interfaces.
    fn create_multicast_socket(sap_addresses: &[String]) -> std::io::Result<UdpSocket> {
        let socket = Socket::new(Domain::IPV4, Type::DGRAM, Some(Protocol::UDP))?;

        // Allow address reuse
        socket.set_reuse_address(true)?;

        // Bind to SAP port
        let bind_addr = SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, SAP_PORT);
        socket.bind(&bind_addr.into())?;

        // Join configured SAP multicast groups on ALL interfaces
        let interface_ips = Self::get_all_interface_ips();

        for sap_addr_str in sap_addresses {
            let multicast_addr: Ipv4Addr = match sap_addr_str.parse() {
                Ok(addr) => addr,
                Err(e) => {
                    warn!("Invalid SAP multicast address '{}': {}", sap_addr_str, e);
                    continue;
                }
            };

            if interface_ips.is_empty() {
                // Fallback to UNSPECIFIED if no interfaces found
                warn!(
                    "No interfaces found, joining SAP multicast {} on default interface",
                    sap_addr_str
                );
                socket.join_multicast_v4(&multicast_addr, &Ipv4Addr::UNSPECIFIED)?;
            } else {
                for ip in &interface_ips {
                    match socket.join_multicast_v4(&multicast_addr, ip) {
                        Ok(_) => info!(
                            "Joined SAP multicast group {} on interface {}",
                            sap_addr_str, ip
                        ),
                        Err(e) => warn!(
                            "Failed to join SAP multicast {} on {}: {}",
                            sap_addr_str, ip, e
                        ),
                    }
                }
            }
        }

        // Set non-blocking for tokio
        socket.set_nonblocking(true)?;

        // Convert to tokio UdpSocket
        let std_socket: std::net::UdpSocket = socket.into();
        UdpSocket::from_std(std_socket)
    }

    /// Create a socket for sending SAP announcements on a specific interface.
    fn create_send_socket_for_interface(interface_ip: Ipv4Addr) -> std::io::Result<UdpSocket> {
        let socket = Socket::new(Domain::IPV4, Type::DGRAM, Some(Protocol::UDP))?;

        // Set multicast TTL
        socket.set_multicast_ttl_v4(32)?;

        // Set the outgoing multicast interface
        if interface_ip != Ipv4Addr::UNSPECIFIED {
            socket.set_multicast_if_v4(&interface_ip)?;
        }

        // Bind to any port on this interface
        let bind_addr = SocketAddrV4::new(interface_ip, 0);
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

        // Try to determine which interface the packet was received on
        // by matching the source address to local interface subnets
        let received_interface = Self::find_interface_for_address(&addr);

        if packet.is_deletion() {
            // Handle deletion
            Self::handle_deletion(&session_id, inner).await;
        } else {
            // Handle announcement
            Self::handle_announcement(packet, received_interface, inner).await;
        }
    }

    /// Find which local interface would be used to reach a given address.
    /// Returns the interface name if found.
    fn find_interface_for_address(addr: &SocketAddr) -> Option<String> {
        use network_interface::{NetworkInterface, NetworkInterfaceConfig};

        let target_ip = match addr.ip() {
            IpAddr::V4(ip) => ip,
            IpAddr::V6(_) => return None, // IPv6 not supported yet
        };

        if let Ok(interfaces) = NetworkInterface::show() {
            for iface in interfaces {
                // Skip loopback
                if iface.name.starts_with("lo") {
                    continue;
                }

                for net_addr in &iface.addr {
                    if let network_interface::Addr::V4(v4) = net_addr {
                        let ip = v4.ip;
                        let netmask = v4.netmask.unwrap_or(Ipv4Addr::new(255, 255, 255, 0));

                        // Check if target IP is in the same subnet as this interface
                        if Self::is_same_subnet(&target_ip, &ip, &netmask) {
                            debug!(
                                "SAP packet from {} matched to interface {} ({})",
                                addr, iface.name, ip
                            );
                            return Some(iface.name.clone());
                        }
                    }
                }
            }
        }

        None
    }

    /// Check if two IPs are in the same subnet given a netmask.
    fn is_same_subnet(ip1: &Ipv4Addr, ip2: &Ipv4Addr, netmask: &Ipv4Addr) -> bool {
        let ip1_octets = ip1.octets();
        let ip2_octets = ip2.octets();
        let mask_octets = netmask.octets();

        for i in 0..4 {
            if (ip1_octets[i] & mask_octets[i]) != (ip2_octets[i] & mask_octets[i]) {
                return false;
            }
        }
        true
    }

    /// Handle a SAP announcement.
    async fn handle_announcement(
        packet: SapPacket,
        received_interface: Option<String>,
        inner: &Arc<DiscoveryServiceInner>,
    ) {
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

        // Preserve existing interface if updating, or use newly detected one
        let interface = received_interface.or_else(|| {
            streams
                .get(&stream_id)
                .and_then(|s| s.received_on_interface.clone())
        });

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
            received_on_interface: interface,
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
                    Self::send_pending_announcements(&inner).await;
                }
            }
        }
    }

    /// Send announcements for streams that are due.
    /// If a stream has announce_interface set, only sends on that interface.
    /// Otherwise sends on all interfaces.
    /// SAP destination address is chosen based on the stream's multicast scope.
    async fn send_pending_announcements(inner: &Arc<DiscoveryServiceInner>) {
        let sockets = inner.send_sockets.read().await;
        if sockets.is_empty() {
            return;
        }

        let mut announced = inner.announced_streams.write().await;

        for stream in announced.values_mut() {
            // Determine SAP address based on stream's multicast scope
            let sap_addr = sap_address_for_stream(&stream.multicast_address);
            let dest = SocketAddr::new(IpAddr::V4(sap_addr.parse().unwrap()), SAP_PORT);
            if stream.last_announced.elapsed() >= SAP_ANNOUNCE_INTERVAL {
                let packet =
                    SapPacket::build(stream.origin_ip, stream.msg_id_hash, &stream.sdp, false);

                // Resolve interface name to IP if specified
                let target_ip = stream
                    .announce_interface
                    .as_ref()
                    .and_then(|iface| Self::get_interface_ip(iface));

                // Send on matching interface(s)
                let mut any_success = false;
                for (ip, socket) in sockets.iter() {
                    // Skip if interface is specified and doesn't match
                    if let Some(target) = target_ip {
                        if *ip != target {
                            continue;
                        }
                    }

                    match socket.send_to(&packet, dest).await {
                        Ok(_) => {
                            debug!(
                                "Sent SAP announcement for {}:{} on {} to {} ({} bytes)",
                                stream.flow_id,
                                stream.block_id,
                                ip,
                                sap_addr,
                                packet.len()
                            );
                            any_success = true;
                        }
                        Err(e) => {
                            warn!("Failed to send SAP on {}: {}", ip, e);
                        }
                    }
                }

                if any_success {
                    stream.last_announced = Instant::now();
                } else if target_ip.is_some() {
                    warn!(
                        "No matching socket found for interface {:?} when announcing {}:{}",
                        stream.announce_interface, stream.flow_id, stream.block_id
                    );
                }
            }
        }
    }

    /// Send a single announcement.
    /// If stream has announce_interface set, only sends on that interface.
    /// Otherwise sends on all interfaces.
    /// SAP destination address is chosen based on the stream's multicast scope.
    async fn send_announcement(&self, stream: &AnnouncedStream) -> Result<(), SapError> {
        let sockets = self.inner.send_sockets.read().await;

        if sockets.is_empty() {
            return Err(SapError::InvalidPayload);
        }

        // Determine SAP address based on stream's multicast scope
        let sap_addr = sap_address_for_stream(&stream.multicast_address);
        let dest = SocketAddr::new(IpAddr::V4(sap_addr.parse().unwrap()), SAP_PORT);
        let packet = SapPacket::build(stream.origin_ip, stream.msg_id_hash, &stream.sdp, false);

        // Resolve interface name to IP if specified
        let target_ip = stream
            .announce_interface
            .as_ref()
            .and_then(|iface| Self::get_interface_ip(iface));

        if let Some(iface) = &stream.announce_interface {
            info!(
                "SAP announcement for {}:{} will be sent on interface {} (IP: {:?}) to {}",
                stream.flow_id, stream.block_id, iface, target_ip, sap_addr
            );
        }

        let mut any_success = false;
        for (ip, socket) in sockets.iter() {
            // Skip if interface is specified and doesn't match
            if let Some(target) = target_ip {
                if *ip != target {
                    continue;
                }
            }

            match socket.send_to(&packet, dest).await {
                Ok(_) => {
                    debug!(
                        "Sent SAP announcement for {}:{} on {} to {} ({} bytes)",
                        stream.flow_id,
                        stream.block_id,
                        ip,
                        sap_addr,
                        packet.len()
                    );
                    any_success = true;
                }
                Err(e) => {
                    warn!("Failed to send SAP announcement on {}: {}", ip, e);
                }
            }
        }

        if any_success {
            Ok(())
        } else {
            if target_ip.is_some() {
                warn!(
                    "No matching socket found for interface {:?}",
                    stream.announce_interface
                );
            }
            Err(SapError::InvalidPayload)
        }
    }

    /// Send a deletion message for a stream.
    /// If stream has announce_interface set, only sends on that interface.
    /// Otherwise sends on all interfaces.
    /// SAP destination address is chosen based on the stream's multicast scope.
    async fn send_deletion(&self, stream: &AnnouncedStream) -> Result<(), SapError> {
        let sockets = self.inner.send_sockets.read().await;

        if sockets.is_empty() {
            return Err(SapError::InvalidPayload);
        }

        // Determine SAP address based on stream's multicast scope
        let sap_addr = sap_address_for_stream(&stream.multicast_address);
        let dest = SocketAddr::new(IpAddr::V4(sap_addr.parse().unwrap()), SAP_PORT);
        let packet = SapPacket::build(stream.origin_ip, stream.msg_id_hash, &stream.sdp, true);

        // Resolve interface name to IP if specified
        let target_ip = stream
            .announce_interface
            .as_ref()
            .and_then(|iface| Self::get_interface_ip(iface));

        let mut any_success = false;
        for (ip, socket) in sockets.iter() {
            // Skip if interface is specified and doesn't match
            if let Some(target) = target_ip {
                if *ip != target {
                    continue;
                }
            }

            match socket.send_to(&packet, dest).await {
                Ok(_) => {
                    info!(
                        "Sent SAP deletion for {}:{} on {} to {}",
                        stream.flow_id, stream.block_id, ip, sap_addr
                    );
                    any_success = true;
                }
                Err(e) => {
                    warn!("Failed to send SAP deletion on {}: {}", ip, e);
                }
            }
        }

        if any_success {
            Ok(())
        } else {
            Err(SapError::InvalidPayload)
        }
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

    // --- mDNS methods ---

    /// Start mDNS discovery for RAVENNA streams.
    async fn start_mdns_discovery(&self, shutdown_tx: broadcast::Sender<()>) -> anyhow::Result<()> {
        info!("Starting mDNS discovery for RAVENNA streams");

        // Create mDNS discovery instance
        let mdns = match MdnsDiscovery::new() {
            Ok(m) => Arc::new(m),
            Err(e) => {
                error!("Failed to create mDNS discovery: {}", e);
                return Err(e);
            }
        };

        // Store for later use
        {
            let mut mdns_lock = self.inner.mdns_discovery.write().await;
            *mdns_lock = Some(mdns.clone());
        }

        // Start browsing for RAVENNA streams (_rtsp._tcp.local)
        let receiver = mdns.browse("_rtsp._tcp.local.")?;

        // Spawn task to handle mDNS events
        let inner = self.inner.clone();
        let shutdown_rx = shutdown_tx.subscribe();
        tokio::spawn(async move {
            Self::run_mdns_listener(receiver, inner, shutdown_rx).await;
        });

        info!("mDNS discovery started");
        Ok(())
    }

    /// Run the mDNS listener loop.
    async fn run_mdns_listener(
        receiver: mdns_sd::Receiver<ServiceEvent>,
        inner: Arc<DiscoveryServiceInner>,
        mut shutdown: broadcast::Receiver<()>,
    ) {
        loop {
            tokio::select! {
                _ = shutdown.recv() => {
                    debug!("mDNS listener shutting down");
                    break;
                }
                event = async { receiver.recv_async().await } => {
                    match event {
                        Ok(ServiceEvent::ServiceResolved(info)) => {
                            // ResolvedService has public fields
                            let service_type = info.ty_domain.clone();
                            let instance_name = info.fullname.clone();
                            let hostname = info.host.clone();
                            let port = info.port;
                            let addresses: Vec<_> = info.addresses.iter()
                                .filter_map(|a| IpAddr::from_str(&a.to_string()).ok())
                                .collect();
                            let path = info.txt_properties.get("path")
                                .map(|s| {
                                    let s = s.to_string();
                                    // Strip "path=" prefix if present (some mDNS libs include it)
                                    s.strip_prefix("path=").unwrap_or(&s).to_string()
                                })
                                .unwrap_or_else(|| "/".to_string());
                            debug!("mDNS TXT property 'path': {:?}", path);

                            // Spawn a task to handle it (async RTSP fetch)
                            let inner_clone = inner.clone();
                            tokio::spawn(async move {
                                Self::handle_mdns_service_data(
                                    service_type,
                                    instance_name,
                                    hostname,
                                    port,
                                    addresses,
                                    path,
                                    inner_clone,
                                ).await;
                                debug!("mDNS service data handling completed");
                            });
                        }
                        Ok(ServiceEvent::ServiceRemoved(service_type, fullname)) => {
                            debug!("mDNS service removed: {} ({})", fullname, service_type);
                            // Handle service removal if needed
                        }
                        Ok(_) => {
                            // Other events (SearchStarted, SearchStopped) - ignore
                        }
                        Err(e) => {
                            warn!("mDNS receiver error: {}", e);
                            break;
                        }
                    }
                }
            }
        }
    }

    /// Handle a resolved mDNS service (RAVENNA stream).
    async fn handle_mdns_service_data(
        service_type: String,
        instance_name: String,
        hostname: String,
        port: u16,
        addresses: Vec<IpAddr>,
        path: String,
        inner: Arc<DiscoveryServiceInner>,
    ) {
        info!(
            "Discovered mDNS service: {} at {}:{}",
            instance_name, hostname, port
        );

        // For RAVENNA, fetch SDP via RTSP DESCRIBE
        if service_type.starts_with("_rtsp._tcp.") {
            info!("Processing RTSP service type: {}", service_type);

            // Get first IP address
            let ip = match addresses.first() {
                Some(addr) => *addr,
                None => {
                    warn!("No IP address for mDNS service: {}", instance_name);
                    return;
                }
            };
            info!("Using IP address: {}", ip);

            // Build RTSP URL - ensure path starts with /
            let path = if path.starts_with('/') {
                path
            } else {
                format!("/{}", path)
            };
            let rtsp_url = format!("rtsp://{}:{}{}", ip, port, path);

            info!("Fetching SDP from RTSP URL: {}", rtsp_url);

            // Fetch SDP
            match rtsp_client::rtsp_describe(&rtsp_url).await {
                Ok(sdp) => {
                    info!(
                        "Successfully fetched SDP ({} bytes) from {}",
                        sdp.len(),
                        rtsp_url
                    );
                    // Parse SDP to extract stream info
                    if let Some(sdp_info) = SdpStreamInfo::parse(&sdp) {
                        // Generate stream ID with mDNS suffix to differentiate from SAP
                        let base_id = sdp_info.generate_id(&ip);
                        let stream_id = format!("{}-mdns", base_id);
                        info!(
                            "Parsed SDP, stream_id: {}, name: {}",
                            stream_id, sdp_info.name
                        );
                        let now = Instant::now();

                        let mut streams = inner.discovered_streams.write().await;
                        let is_new = !streams.contains_key(&stream_id);
                        info!(
                            "Stream is_new: {}, total_streams: {}",
                            is_new,
                            streams.len()
                        );

                        // Try to determine interface from source IP
                        let received_interface =
                            Self::find_interface_for_address(&SocketAddr::new(ip, port));

                        let stream = DiscoveredStream {
                            id: stream_id.clone(),
                            name: sdp_info.name.clone(),
                            source: DiscoverySource::Mdns {
                                service_type,
                                instance_name: instance_name.clone(),
                                hostname,
                                port,
                            },
                            sdp: sdp.clone(),
                            multicast_address: sdp_info
                                .connection_address
                                .unwrap_or(IpAddr::V4(Ipv4Addr::UNSPECIFIED)),
                            port: sdp_info.port.unwrap_or(5004),
                            channels: sdp_info.channels.unwrap_or(2),
                            sample_rate: sdp_info.sample_rate.unwrap_or(48000),
                            encoding: sdp_info.encoding,
                            origin_host: sdp_info.origin_address.clone(),
                            first_seen: streams
                                .get(&stream_id)
                                .map(|s| s.first_seen)
                                .unwrap_or(now),
                            last_seen: now,
                            ttl: DEFAULT_STREAM_TTL,
                            received_on_interface: received_interface,
                        };

                        streams.insert(stream_id.clone(), stream);

                        if is_new {
                            info!(
                                "Discovered new RAVENNA stream via mDNS: {} ({})",
                                sdp_info.name, stream_id
                            );
                            inner.events.broadcast(StromEvent::StreamDiscovered {
                                stream_id: stream_id.clone(),
                                name: sdp_info.name,
                                source: "mDNS (RAVENNA)".to_string(),
                            });
                        } else {
                            debug!("Updated existing mDNS stream: {}", stream_id);
                            inner.events.broadcast(StromEvent::StreamUpdated {
                                stream_id: stream_id.clone(),
                            });
                        }
                    } else {
                        warn!(
                            "Failed to parse SDP from RTSP URL: {}, SDP content: {}",
                            rtsp_url, sdp
                        );
                    }
                }
                Err(e) => {
                    warn!("Failed to fetch SDP from {}: {}", rtsp_url, e);
                }
            }
        } else {
            info!("Ignoring non-RTSP mDNS service type: {}", service_type);
        }
    }

    /// Register a stream as an mDNS service.
    async fn register_mdns_service(
        &self,
        stream_key: &str,
        instance_name: &str,
        local_ip: IpAddr,
        mdns: &MdnsDiscovery,
    ) -> anyhow::Result<String> {
        use mdns_sd::ServiceInfo;

        let service_type = "_rtsp._tcp.local.";

        // Get system hostname (cross-platform), fallback to "strom"
        let hostname = hostname::get()
            .ok()
            .and_then(|h| h.into_string().ok())
            .unwrap_or_else(|| "strom".to_string());
        let hostname = if hostname.ends_with(".local.") {
            hostname
        } else if hostname.ends_with(".local") {
            format!("{}.", hostname)
        } else {
            format!("{}.local.", hostname)
        };

        let ip_str = local_ip.to_string();
        let port = types::RTSP_PORT;

        // TXT record with path to stream
        let path = format!("/{}", stream_key);
        let properties = [("path", path.as_str())];

        debug!(
            "Creating mDNS service: type={}, instance={}, host={}, ip={}, port={}",
            service_type, instance_name, hostname, ip_str, port
        );

        let service_info = ServiceInfo::new(
            service_type,
            instance_name,
            &hostname,
            &ip_str,
            port,
            &properties[..],
        )?;

        let fullname = service_info.get_fullname().to_string();
        mdns.register(service_info)?;

        Ok(fullname)
    }

    /// Start the RTSP server for serving SDP to announced streams.
    async fn start_rtsp_server(&self, _shutdown_tx: broadcast::Sender<()>) -> anyhow::Result<()> {
        use crate::rtsp_server::{run_rtsp_server, RtspServerConfig};

        info!("Starting RTSP server on port {}", types::RTSP_PORT);

        let config = RtspServerConfig {
            bind_addr: format!("0.0.0.0:{}", types::RTSP_PORT),
        };

        let discovery = self.clone();

        // Spawn RTSP server task
        tokio::spawn(async move {
            if let Err(e) = run_rtsp_server(config, discovery).await {
                error!("RTSP server error: {}", e);
            }
        });

        Ok(())
    }
}

impl Default for DiscoveryService {
    fn default() -> Self {
        Self::new(
            EventBroadcaster::default(),
            SAP_MULTICAST_ADDRS.iter().map(|s| s.to_string()).collect(),
        )
    }
}
