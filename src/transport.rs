//! Low-level TCP transport manager. Handles connection
//! establishment, framing/de-framing of packets, keepalive,
//! and reconnection/backoff logic.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use bytes::{BufMut, BytesMut};
use futures_util::{SinkExt, StreamExt};
use parking_lot::RwLock;
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::mpsc;
use tokio_util::codec::{Decoder, Encoder, Framed};
use tokio_util::sync::CancellationToken;

use crate::config::{NetworkConfig, PROTOCOL_VERSION};
use crate::error::NetworkError;
use crate::messaging::{RawPacket, decode_internal, encode_internal};
use crate::protocol::{InternalMessage, msg_types};
use crate::types::{PeerId, PeerInfo, PeerStatus};

/// Duration for `SO_LINGER` on TCP sockets. A non-zero linger
/// causes the kernel to send RST on close instead of entering
/// TIME_WAIT, preventing ghost connections through tunneled networks.
const SO_LINGER_DURATION: Duration = Duration::from_secs(2);

/// Delay before initiating the data channel after control-channel
/// registration. Gives both sides time to finish the control
/// handshake before the data-channel TCP connection arrives.
const DATA_CHANNEL_OPEN_DELAY: Duration = Duration::from_millis(50);

/// Codec used by `tokio_util::codec::Framed` to frame and
/// de-frame `RawPacket`s. Each encoded packet consists of a 7-byte
/// header (length, msg_type, flags) followed by the payload bytes.
/// The codec also enforces a maximum size.
pub struct PacketCodec {
    max_message_size: u32,
}

impl PacketCodec {
    pub fn new(max_message_size: u32) -> Self {
        Self { max_message_size }
    }
}

impl Decoder for PacketCodec {
    type Item = RawPacket;
    type Error = NetworkError;

    fn decode(&mut self, src: &mut BytesMut) -> Result<Option<Self::Item>, Self::Error> {
        if src.len() < 7 {
            return Ok(None);
        }

        let length = u32::from_be_bytes([src[0], src[1], src[2], src[3]]);
        if length > self.max_message_size {
            return Err(NetworkError::MessageTooLarge(length));
        }

        let total = 7 + length as usize;
        if src.len() < total {
            src.reserve(total - src.len());
            return Ok(None);
        }

        let header = src.split_to(7);
        let payload = src.split_to(length as usize);

        Ok(Some(RawPacket {
            msg_type: u16::from_be_bytes([header[4], header[5]]),
            flags: header[6],
            payload: payload.freeze(),
        }))
    }
}

impl Encoder<RawPacket> for PacketCodec {
    type Error = NetworkError;

    fn encode(&mut self, item: RawPacket, dst: &mut BytesMut) -> Result<(), Self::Error> {
        if item.payload.len() > self.max_message_size as usize {
            return Err(NetworkError::MessageTooLarge(item.payload.len() as u32));
        }
        let length = item.payload.len() as u32;
        dst.reserve(7 + item.payload.len());
        dst.put_u32(length);
        dst.put_u16(item.msg_type);
        dst.put_u8(item.flags);
        dst.put_slice(&item.payload);
        Ok(())
    }
}

/// Internal state for an active connection to a peer. Stored in
/// the `TransportManager::connections` map.
///
/// Each peer has a **control channel** (always present) that carries
/// internal protocol messages (heartbeat, election, handshake, ...)
/// and an optional **data channel** dedicated to user payloads.
/// When the data channel is not yet established or has broken, user
/// messages fall back transparently to the control channel.
struct PeerConnection {
    /// Sender for the control channel (internal messages).
    control_tx: mpsc::Sender<RawPacket>,
    /// Sender for the data channel (user messages). `None` until
    /// the data-channel handshake completes, or after it drops.
    data_tx: Option<mpsc::Sender<RawPacket>>,
    /// Token to shut down control-channel read/write tasks.
    control_cancel: CancellationToken,
    /// Token to shut down data-channel read/write tasks (if active).
    data_cancel: Option<CancellationToken>,
    /// Metadata about the peer.
    info: PeerInfo,
}

/// Manages TCP connections to peers. Responsible for accepting
/// incoming peers, initiating outbound connections, and shuttling
/// packets to/from the rest of the system.
///
/// Fields are grouped for clarity: identity, configuration,
/// connection state, endpoints, and shutdown control.
pub struct TransportManager {
    // --- identity --------------------------------------------------------
    local_peer_id: PeerId,
    session_id: String,

    // --- configuration ---------------------------------------------------
    config: NetworkConfig,

    // --- active connections map ------------------------------------------
    connections: Arc<RwLock<HashMap<PeerId, PeerConnection>>>,

    // --- network endpoints -----------------------------------------------
    listener_addr: SocketAddr,
    incoming_tx: mpsc::Sender<(PeerId, RawPacket)>,

    // --- reconnection ----------------------------------------------------
    reconnect_event_tx: mpsc::Sender<(PeerId, u8)>,
    auto_reconnect_enabled: AtomicBool,

    // --- lifecycle control -----------------------------------------------
    shutdown_token: CancellationToken,
}

impl TransportManager {
    /// Create a new transport manager and begin listening on a
    /// randomly assigned TCP port. Returns the manager and a receiver
    /// for incoming `(PeerId, RawPacket)` tuples.
    pub async fn new(
        local_peer_id: PeerId,
        session_id: String,
        config: NetworkConfig,
        reconnect_event_tx: mpsc::Sender<(PeerId, u8)>,
        shutdown_token: CancellationToken,
    ) -> Result<(Arc<Self>, mpsc::Receiver<(PeerId, RawPacket)>), NetworkError> {
        let listener = TcpListener::bind("0.0.0.0:0").await?;
        let listener_addr = listener.local_addr()?;
        tracing::info!(addr = %listener_addr, "TCP listener started");

        let (incoming_tx, incoming_rx) = mpsc::channel(config.incoming_queue_capacity);

        let manager = Arc::new(Self {
            local_peer_id,
            session_id,
            config,
            connections: Arc::new(RwLock::new(HashMap::new())),
            listener_addr,
            incoming_tx,
            reconnect_event_tx,
            auto_reconnect_enabled: AtomicBool::new(config.auto_reconnect_enabled),
            shutdown_token: shutdown_token.clone(),
        });

        let mgr = Arc::clone(&manager);
        tokio::spawn(async move {
            mgr.accept_loop(listener).await;
        });

        Ok((manager, incoming_rx))
    }

    /// Address the TCP listener is bound to.
    pub fn listener_addr(&self) -> SocketAddr {
        self.listener_addr
    }

    /// Local identifier of this node.
    pub fn local_peer_id(&self) -> &PeerId {
        &self.local_peer_id
    }

    /// Session identifier shared by all peers in the network.
    pub fn session_id(&self) -> &str {
        &self.session_id
    }

    /// Number of currently tracked connections.
    pub fn connection_count(&self) -> usize {
        self.connections.read().len()
    }

    /// Task run by `new()` that accepts inbound TCP sockets and
    /// spawns a handler for each. Respects the shutdown token.
    ///
    /// The `max_connections` limit is **not** enforced here because
    /// at accept time we cannot distinguish a new-peer handshake from
    /// a data-channel handshake. Rejecting early would prevent
    /// existing peers from establishing their data channels when the
    /// peer count has already reached the limit.
    ///
    /// Instead, `handle_incoming_connection` checks the limit after
    /// reading the first framed packet and determining the connection
    /// type.
    async fn accept_loop(self: &Arc<Self>, listener: TcpListener) {
        loop {
            tokio::select! {
                _ = self.shutdown_token.cancelled() => break,
                result = listener.accept() => {
                    match result {
                        Ok((stream, addr)) => {
                            let mgr = Arc::clone(self);
                            tokio::spawn(async move {
                                if let Err(e) = mgr.handle_incoming_connection(stream, addr).await {
                                    tracing::warn!(%addr, error = %e, "incoming connection failed");
                                }
                            });
                        }
                        Err(e) => {
                            tracing::error!(error = %e, "accept error");
                        }
                    }
                }
            }
        }
    }

    /// Apply platform-specific TCP keepalive, `TCP_NODELAY`, and
    /// `SO_LINGER` settings to a stream. Used for both incoming and
    /// outgoing connections to detect dead peers and eliminate ghost
    /// connections (stale TIME_WAIT entries through frp/tailscale
    /// tunnels).
    fn configure_socket(stream: &TcpStream, config: &NetworkConfig) -> Result<(), NetworkError> {
        use socket2::SockRef;

        let sock = SockRef::from(stream);
        let keepalive = socket2::TcpKeepalive::new()
            .with_time(Duration::from_secs(config.keepalive_time_secs.into()))
            .with_interval(Duration::from_secs(config.keepalive_interval_secs.into()));

        #[cfg(not(target_os = "windows"))]
        let keepalive = keepalive.with_retries(config.keepalive_retries.into());

        sock.set_tcp_keepalive(&keepalive)?;
        sock.set_linger(Some(SO_LINGER_DURATION))?;
        stream.set_nodelay(config.tcp_nodelay)?;

        Ok(())
    }

    /// Perform handshake on a newly accepted connection, then
    /// register it and start reader/writer tasks. Returns an error if
    /// handshake fails (including version/session mismatch).
    ///
    /// If the first message is a [`DataChannelHandshake`] instead of
    /// a normal [`Handshake`], delegates to
    /// [`handle_incoming_data_channel_framed`].
    async fn handle_incoming_connection(
        self: &Arc<Self>,
        stream: TcpStream,
        addr: SocketAddr,
    ) -> Result<(), NetworkError> {
        Self::configure_socket(&stream, &self.config)?;

        let mut framed = Framed::new(stream, PacketCodec::new(self.config.max_message_size));

        // Read first packet with a timeout to determine connection type.
        let first_packet = tokio::time::timeout(
            Duration::from_millis(self.config.handshake_timeout_ms.into()),
            framed.next(),
        )
        .await;

        let packet = match first_packet {
            Ok(Some(Ok(pkt))) => pkt,
            Ok(Some(Err(e))) => return Err(e),
            Ok(None) => {
                return Err(NetworkError::HandshakeFailed("connection closed".into()));
            }
            Err(_) => {
                tracing::warn!(%addr, "handshake timeout");
                return Err(NetworkError::HandshakeTimeout);
            }
        };

        // Route to data-channel handler if this is a DataChannelHandshake.
        // Data channels are for *existing* peers, so the
        // max_connections limit does not apply.
        if packet.msg_type == msg_types::DATA_CHANNEL_HANDSHAKE {
            return self
                .handle_incoming_data_channel_framed(framed, addr, packet)
                .await;
        }

        // For new peer handshakes, enforce the max_connections limit.
        if self.connections.read().len() >= self.config.max_connections {
            tracing::warn!(%addr, "max connections reached, rejecting handshake");
            return Err(NetworkError::MaxConnectionsReached);
        }

        // Proceed with normal control-channel handshake.
        let msg = decode_internal(&packet)?;

        let handshake_result = self.process_handshake_message(msg, &mut framed).await;

        match handshake_result {
            Ok((peer_id, listen_port, notify_packet)) => {
                let peer_addr = SocketAddr::new(addr.ip(), listen_port);
                match self.register_connection(peer_id.clone(), peer_addr, framed) {
                    Ok(()) => {
                        let _ = self.incoming_tx.send((peer_id, notify_packet)).await;
                        Ok(())
                    }
                    Err(NetworkError::DuplicatePeerId(id)) => {
                        tracing::debug!(peer = %id, "inbound TOCTOU duplicate, keeping existing");
                        Ok(())
                    }
                    Err(e) => Err(e),
                }
            }
            Err(NetworkError::AlreadyConnected(ref pid)) => {
                tracing::debug!(%addr, peer = %pid, "inbound connection from already-connected peer, keeping existing");
                Ok(())
            }
            Err(e) => Err(e),
        }
    }

    /// Process a normal `Handshake` message that was already read
    /// from the framed stream. On success returns the remote
    /// `PeerId`, its listen port, and a notification packet.
    async fn process_handshake_message(
        &self,
        msg: InternalMessage,
        framed: &mut Framed<TcpStream, PacketCodec>,
    ) -> Result<(PeerId, u16, RawPacket), NetworkError> {
        match msg {
            InternalMessage::Handshake {
                peer_id,
                listen_port,
                protocol_version,
                session_id,
            } => {
                if protocol_version != PROTOCOL_VERSION {
                    let ack = encode_internal(
                        &InternalMessage::HandshakeAck {
                            peer_id: self.local_peer_id.as_str().to_owned(),
                            listen_port: self.listener_addr.port(),
                            success: false,
                            error_reason: Some(format!(
                                "version_mismatch: expected {PROTOCOL_VERSION}, got {protocol_version}"
                            )),
                        },
                        self.config.max_message_size,
                    )?;
                    let _ = framed.send(ack).await;
                    return Err(NetworkError::VersionMismatch {
                        expected: PROTOCOL_VERSION,
                        got: protocol_version,
                    });
                }

                if session_id != self.session_id {
                    let ack = encode_internal(
                        &InternalMessage::HandshakeAck {
                            peer_id: self.local_peer_id.as_str().to_owned(),
                            listen_port: self.listener_addr.port(),
                            success: false,
                            error_reason: Some("session_mismatch".into()),
                        },
                        self.config.max_message_size,
                    )?;
                    let _ = framed.send(ack).await;
                    return Err(NetworkError::SessionMismatch {
                        expected: self.session_id.clone(),
                        got: session_id,
                    });
                }

                let pid = PeerId::new(&peer_id);
                if self.connections.read().contains_key(&pid) {
                    let ack = encode_internal(
                        &InternalMessage::HandshakeAck {
                            peer_id: self.local_peer_id.as_str().to_owned(),
                            listen_port: self.listener_addr.port(),
                            success: true,
                            error_reason: Some("already_connected".into()),
                        },
                        self.config.max_message_size,
                    )?;
                    let _ = framed.send(ack).await;
                    return Err(NetworkError::AlreadyConnected(peer_id));
                }

                let ack = encode_internal(
                    &InternalMessage::HandshakeAck {
                        peer_id: self.local_peer_id.as_str().to_owned(),
                        listen_port: self.listener_addr.port(),
                        success: true,
                        error_reason: None,
                    },
                    self.config.max_message_size,
                )?;
                framed.send(ack).await.map_err(|e| {
                    NetworkError::ConnectionFailed(format!("failed to send handshake ack: {e}"))
                })?;

                let peer_list = self.get_peer_list_for_sync();
                if !peer_list.is_empty() {
                    let sync_msg = encode_internal(
                        &InternalMessage::PeerListSync { peers: peer_list },
                        self.config.max_message_size,
                    )?;
                    let _ = framed.send(sync_msg).await;
                }

                let notify_packet = encode_internal(
                    &InternalMessage::Handshake {
                        peer_id: peer_id.clone(),
                        listen_port,
                        protocol_version,
                        session_id,
                    },
                    self.config.max_message_size,
                )?;

                Ok((pid, listen_port, notify_packet))
            }
            _ => Err(NetworkError::HandshakeFailed(
                "expected Handshake message".into(),
            )),
        }
    }

    /// Handle a data-channel handshake on a framed stream whose first
    /// packet has already been read and determined to be a
    /// `DataChannelHandshake`.
    async fn handle_incoming_data_channel_framed(
        self: &Arc<Self>,
        mut framed: Framed<TcpStream, PacketCodec>,
        addr: SocketAddr,
        packet: RawPacket,
    ) -> Result<(), NetworkError> {
        let msg = decode_internal(&packet)?;
        match msg {
            InternalMessage::DataChannelHandshake { peer_id } => {
                let pid = PeerId::new(peer_id);
                let known = self.connections.read().contains_key(&pid);
                let ack = encode_internal(
                    &InternalMessage::DataChannelHandshakeAck {
                        peer_id: self.local_peer_id.as_str().to_owned(),
                        success: known,
                    },
                    self.config.max_message_size,
                )?;
                framed.send(ack).await.map_err(|e| {
                    NetworkError::ConnectionFailed(format!(
                        "failed to send DataChannelHandshakeAck: {e}"
                    ))
                })?;
                if !known {
                    return Err(NetworkError::HandshakeFailed(format!(
                        "data channel from unknown peer {pid}"
                    )));
                }
                self.register_data_channel(&pid, framed);
                tracing::info!(peer = %pid, addr = %addr, "data channel established (inbound)");
                Ok(())
            }
            _ => Err(NetworkError::HandshakeFailed(
                "expected DataChannelHandshake".into(),
            )),
        }
    }

    /// Establish an outbound TCP connection to the given address and
    /// perform the handshake protocol. On success the new peer's
    /// `PeerId` is returned and the connection is registered.
    pub async fn connect_to(self: &Arc<Self>, addr: &str) -> Result<PeerId, NetworkError> {
        if self.connections.read().len() >= self.config.max_connections {
            return Err(NetworkError::MaxConnectionsReached);
        }

        let socket_addr: SocketAddr = addr
            .parse()
            .map_err(|e| NetworkError::InvalidArgument(format!("invalid address: {e}")))?;

        let stream = tokio::time::timeout(
            Duration::from_millis(self.config.handshake_timeout_ms.into()),
            TcpStream::connect(socket_addr),
        )
        .await
        .map_err(|_| NetworkError::HandshakeTimeout)?
        .map_err(|e| NetworkError::ConnectionFailed(format!("TCP connect failed: {e}")))?;

        Self::configure_socket(&stream, &self.config)?;

        let mut framed = Framed::new(stream, PacketCodec::new(self.config.max_message_size));

        let handshake = encode_internal(
            &InternalMessage::Handshake {
                peer_id: self.local_peer_id.as_str().to_owned(),
                listen_port: self.listener_addr.port(),
                protocol_version: PROTOCOL_VERSION,
                session_id: self.session_id.clone(),
            },
            self.config.max_message_size,
        )?;

        framed.send(handshake).await.map_err(|e| {
            NetworkError::ConnectionFailed(format!("failed to send handshake: {e}"))
        })?;

        let ack_result = tokio::time::timeout(
            Duration::from_millis(self.config.handshake_timeout_ms.into()),
            self.receive_handshake_ack(&mut framed),
        )
        .await;

        match ack_result {
            Ok(Ok((peer_id, listen_port, notify_packet))) => {
                let peer_addr = SocketAddr::new(socket_addr.ip(), listen_port);
                match self.register_connection(peer_id.clone(), peer_addr, framed) {
                    Ok(()) => {
                        let _ = self
                            .incoming_tx
                            .send((peer_id.clone(), notify_packet))
                            .await;
                        Ok(peer_id)
                    }
                    Err(NetworkError::DuplicatePeerId(id)) => {
                        // TOCTOU: another path registered this peer between
                        // handshake and register_connection. Treat as already connected.
                        Err(NetworkError::AlreadyConnected(id))
                    }
                    Err(e) => Err(e),
                }
            }
            Ok(Err(NetworkError::AlreadyConnected(pid))) => {
                // Remote says we are already connected — return the existing peer_id.
                tracing::debug!(peer = %pid, "connect_to: peer already connected via another path");
                Err(NetworkError::AlreadyConnected(pid))
            }
            Ok(Err(e)) => Err(e),
            Err(_) => Err(NetworkError::HandshakeTimeout),
        }
    }

    /// Wait for and process a handshake acknowledgement from a peer.
    /// Returns the remote `PeerId`, its reported listen port and a
    /// packet mirroring the ack for notification purposes.
    async fn receive_handshake_ack(
        &self,
        framed: &mut Framed<TcpStream, PacketCodec>,
    ) -> Result<(PeerId, u16, RawPacket), NetworkError> {
        let packet = framed
            .next()
            .await
            .ok_or_else(|| NetworkError::HandshakeFailed("connection closed".into()))??;

        let msg = decode_internal(&packet)?;

        match msg {
            InternalMessage::HandshakeAck {
                peer_id,
                listen_port,
                success,
                error_reason,
            } => {
                if !success {
                    let reason = error_reason.unwrap_or_else(|| "unknown".into());
                    if reason == "duplicate_peer_id" {
                        return Err(NetworkError::AlreadyConnected(peer_id));
                    }
                    return Err(NetworkError::HandshakeFailed(reason));
                }
                // success=true with "already_connected" hint means the
                // remote already has a connection from us (e.g. via mDNS).
                if error_reason.as_deref() == Some("already_connected") {
                    return Err(NetworkError::AlreadyConnected(peer_id));
                }
                let pid = PeerId::new(&peer_id);

                let notify_packet = encode_internal(
                    &InternalMessage::HandshakeAck {
                        peer_id,
                        listen_port,
                        success: true,
                        error_reason: None,
                    },
                    self.config.max_message_size,
                )?;

                Ok((pid, listen_port, notify_packet))
            }
            _ => Err(NetworkError::HandshakeFailed(
                "expected HandshakeAck message".into(),
            )),
        }
    }

    /// Add a newly established, handshake connection to the
    /// internal map and spawn the read/write tasks that keep the
    /// socket alive and feed incoming packets back to the network
    /// state. Also handles cleanup and reconnection logic when the
    /// socket closes.
    ///
    /// This registers the **control channel** for the peer. The data
    /// channel is established separately via [`open_data_channel`].
    fn register_connection(
        self: &Arc<Self>,
        peer_id: PeerId,
        peer_addr: SocketAddr,
        framed: Framed<TcpStream, PacketCodec>,
    ) -> Result<(), NetworkError> {
        let (mut sink, mut stream) = framed.split();
        let (send_tx, mut send_rx) = mpsc::channel::<RawPacket>(self.config.send_queue_capacity);
        let cancel_token = self.shutdown_token.child_token();

        let info = PeerInfo::new(peer_id.clone(), peer_addr);

        {
            let mut conns = self.connections.write();
            if conns.contains_key(&peer_id) {
                return Err(NetworkError::DuplicatePeerId(peer_id.to_string()));
            }
            conns.insert(
                peer_id.clone(),
                PeerConnection {
                    control_tx: send_tx,
                    data_tx: None,
                    control_cancel: cancel_token.clone(),
                    data_cancel: None,
                    info,
                },
            );
        }

        tracing::info!(peer = %peer_id, addr = %peer_addr, "peer connected (control channel)");

        let write_cancel = cancel_token.clone();
        let write_timeout_ms = self.config.write_timeout_ms;
        tokio::spawn(async move {
            loop {
                tokio::select! {
                    _ = write_cancel.cancelled() => break,
                    msg = send_rx.recv() => {
                        match msg {
                            Some(packet) => {
                                match tokio::time::timeout(
                                    std::time::Duration::from_millis(u64::from(write_timeout_ms)),
                                    sink.send(packet),
                                ).await {
                                    Ok(Ok(())) => {}
                                    Ok(Err(e)) => {
                                        tracing::warn!(error = %e, "control write task send error");
                                        break;
                                    }
                                    Err(_) => {
                                        tracing::warn!("control write timeout, disconnecting stalled peer");
                                        break;
                                    }
                                }
                            }
                            None => break,
                        }
                    }
                }
            }
        });

        let read_cancel = cancel_token.clone();
        let read_peer_id = peer_id.clone();
        let incoming_tx = self.incoming_tx.clone();
        let connections = Arc::clone(&self.connections);
        let mgr = Arc::clone(self);
        tokio::spawn(async move {
            loop {
                tokio::select! {
                    _ = read_cancel.cancelled() => break,
                    frame = stream.next() => {
                        match frame {
                            Some(Ok(packet)) => {
                                // Control packets (heartbeat, election, membership)
                                // must never be dropped — use blocking send.
                                if incoming_tx.send((read_peer_id.clone(), packet)).await.is_err() {
                                    break; // receiver dropped
                                }
                            }
                            Some(Err(e)) => {
                                tracing::warn!(peer = %read_peer_id, error = %e, "control read error");
                                break;
                            }
                            None => {
                                tracing::info!(peer = %read_peer_id, "control channel closed by remote");
                                break;
                            }
                        }
                    }
                }
            }

            let reconnect_addr = {
                let mut conns = connections.write();
                if let Some(conn) = conns.get_mut(&read_peer_id) {
                    conn.info.status = PeerStatus::Disconnected;
                    conn.control_cancel.cancel();
                    if let Some(dc) = conn.data_cancel.take() {
                        dc.cancel();
                    }
                    let addr = if conn.info.should_reconnect
                        && !mgr.shutdown_token.is_cancelled()
                        && mgr.auto_reconnect_enabled.load(Ordering::Relaxed)
                    {
                        Some(conn.info.addr.to_string())
                    } else {
                        None
                    };
                    conns.remove(&read_peer_id);
                    addr
                } else {
                    None
                }
            };

            if let Some(addr) = reconnect_addr {
                let _ = mgr.reconnect_event_tx.try_send((read_peer_id.clone(), 0));
                mgr.spawn_reconnect(read_peer_id, addr);
            }
        });

        // After successful control-channel registration, attempt to
        // open the data channel asynchronously. To avoid races where
        // both sides simultaneously open data channels (and the stale
        // cleanup wipes the surviving one), only the peer with the
        // lexicographically smaller ID initiates the data channel.
        if self.local_peer_id.as_str() < peer_id.as_str() {
            let mgr = Arc::clone(self);
            let data_peer_id = peer_id.clone();
            let data_peer_addr = peer_addr;
            tokio::spawn(async move {
                tokio::time::sleep(DATA_CHANNEL_OPEN_DELAY).await;
                if let Err(e) = mgr.open_data_channel(&data_peer_id, data_peer_addr).await {
                    tracing::debug!(
                        peer = %data_peer_id,
                        error = %e,
                        "data channel open failed, using control channel as fallback"
                    );
                }
            });
        }

        Ok(())
    }

    /// When a connection is lost but flagged for reconnection, this
    /// method kicks off a background task that attempts to re-establish
    /// the connection using exponential backoff until success, shutdown,
    /// or the configured maximum retry count is reached.
    ///
    /// Reconnection lifecycle events are sent through `reconnect_event_tx`:
    /// `0` = disconnected (sent by caller before this method),
    /// `1` = reconnect succeeded, `2` = retries exhausted.
    fn spawn_reconnect(self: &Arc<Self>, peer_id: PeerId, addr: String) {
        let mgr = Arc::clone(self);
        let initial_ms = u32::from(self.config.reconnect_initial_ms);
        let max_ms = self.config.reconnect_max_ms;
        let max_retries = self.config.reconnect_max_retries;
        let shutdown = self.shutdown_token.clone();

        tokio::spawn(async move {
            let mut delay_ms = initial_ms;
            let mut attempts: u8 = 0;
            loop {
                tracing::info!(peer = %peer_id, delay_ms, "scheduling reconnect");

                tokio::select! {
                    _ = shutdown.cancelled() => break,
                    _ = tokio::time::sleep(Duration::from_millis(delay_ms.into())) => {}
                }

                if !mgr.auto_reconnect_enabled.load(Ordering::Relaxed) {
                    tracing::info!(peer = %peer_id, "auto-reconnect disabled at runtime, stopping");
                    let _ = mgr.reconnect_event_tx.try_send((peer_id.clone(), 2));
                    break;
                }

                let result = tokio::select! {
                    _ = shutdown.cancelled() => break,
                    r = mgr.connect_to(&addr) => r,
                };

                match result {
                    Ok(_) => {
                        tracing::info!(peer = %peer_id, "reconnected successfully");
                        let _ = mgr.reconnect_event_tx.try_send((peer_id.clone(), 1));
                        break;
                    }
                    Err(e) => {
                        tracing::warn!(peer = %peer_id, error = %e, "reconnect failed");
                        attempts = attempts.saturating_add(1);
                        if max_retries > 0 && attempts >= max_retries {
                            tracing::warn!(
                                peer = %peer_id, attempts,
                                "reconnect attempts exhausted"
                            );
                            let _ = mgr.reconnect_event_tx.try_send((peer_id.clone(), 2));
                            break;
                        }
                        delay_ms = delay_ms.saturating_mul(2).min(max_ms);
                    }
                }
            }
        });
    }

    /// Open a dedicated data channel to a peer that already has
    /// a control channel registered. The initiator opens a new TCP
    /// connection, sends [`DataChannelHandshake`], waits for an ack,
    /// then registers the stream.
    async fn open_data_channel(
        self: &Arc<Self>,
        peer_id: &PeerId,
        peer_addr: SocketAddr,
    ) -> Result<(), NetworkError> {
        // Only open once
        {
            let conns = self.connections.read();
            match conns.get(peer_id) {
                Some(c) if c.data_tx.is_some() => return Ok(()),
                None => return Err(NetworkError::PeerNotFound(peer_id.to_string())),
                _ => {}
            }
        }

        let stream = tokio::time::timeout(
            Duration::from_millis(self.config.handshake_timeout_ms.into()),
            TcpStream::connect(peer_addr),
        )
        .await
        .map_err(|_| NetworkError::HandshakeTimeout)?
        .map_err(|e| {
            NetworkError::ConnectionFailed(format!("data channel TCP connect failed: {e}"))
        })?;

        Self::configure_socket(&stream, &self.config)?;

        let mut framed = Framed::new(stream, PacketCodec::new(self.config.max_message_size));

        let hs = encode_internal(
            &InternalMessage::DataChannelHandshake {
                peer_id: self.local_peer_id.as_str().to_owned(),
            },
            self.config.max_message_size,
        )?;
        framed.send(hs).await.map_err(|e| {
            NetworkError::ConnectionFailed(format!("failed to send DataChannelHandshake: {e}"))
        })?;

        let ack = tokio::time::timeout(
            Duration::from_millis(self.config.handshake_timeout_ms.into()),
            framed.next(),
        )
        .await
        .map_err(|_| NetworkError::HandshakeTimeout)?
        .ok_or_else(|| NetworkError::HandshakeFailed("data channel closed".into()))??;

        let msg = decode_internal(&ack)?;
        match msg {
            InternalMessage::DataChannelHandshakeAck { success, .. } if success => {}
            _ => {
                return Err(NetworkError::HandshakeFailed(
                    "data channel handshake rejected".into(),
                ));
            }
        }

        self.register_data_channel(peer_id, framed);

        tracing::info!(peer = %peer_id, "data channel established (outbound)");
        Ok(())
    }

    /// Attach a framed TCP stream as the data channel for an
    /// already-registered peer.
    fn register_data_channel(
        self: &Arc<Self>,
        peer_id: &PeerId,
        framed: Framed<TcpStream, PacketCodec>,
    ) {
        let (mut sink, mut stream) = framed.split();
        let (send_tx, mut send_rx) = mpsc::channel::<RawPacket>(self.config.send_queue_capacity);
        let cancel_token = self.shutdown_token.child_token();

        {
            let mut conns = self.connections.write();
            if let Some(conn) = conns.get_mut(peer_id) {
                // Cancel any previous data channel
                if let Some(dc) = conn.data_cancel.take() {
                    dc.cancel();
                }
                conn.data_tx = Some(send_tx);
                conn.data_cancel = Some(cancel_token.clone());
            } else {
                tracing::debug!(peer = %peer_id, "data channel setup aborted: peer removed from connections");
                return;
            }
        }

        // Write task for data channel
        let write_cancel = cancel_token.clone();
        let write_timeout_ms = self.config.write_timeout_ms;
        tokio::spawn(async move {
            loop {
                tokio::select! {
                    _ = write_cancel.cancelled() => break,
                    msg = send_rx.recv() => {
                        match msg {
                            Some(packet) => {
                                match tokio::time::timeout(
                                    std::time::Duration::from_millis(u64::from(write_timeout_ms)),
                                    sink.send(packet),
                                ).await {
                                    Ok(Ok(())) => {}
                                    Ok(Err(e)) => {
                                        tracing::warn!(error = %e, "data write task send error");
                                        break;
                                    }
                                    Err(_) => {
                                        tracing::warn!("data write timeout, disconnecting stalled peer");
                                        break;
                                    }
                                }
                            }
                            None => break,
                        }
                    }
                }
            }
        });

        // Read task for data channel — feeds packets into the same
        // `incoming_tx` as the control channel.
        let read_cancel = cancel_token.clone();
        let read_peer_id = peer_id.clone();
        let incoming_tx = self.incoming_tx.clone();
        let connections = Arc::clone(&self.connections);
        let cleanup_token = cancel_token.clone();
        let data_read_timeout_ms = self.config.write_timeout_ms;
        tokio::spawn(async move {
            loop {
                tokio::select! {
                    _ = read_cancel.cancelled() => break,
                    frame = stream.next() => {
                        match frame {
                            Some(Ok(packet)) => {
                                // Data packets: if the incoming queue is full for
                                // longer than write_timeout_ms, disconnect. This
                                // avoids silent packet loss in P2P scenarios and
                                // lets the reconnection mechanism trigger state resync.
                                match tokio::time::timeout(
                                    Duration::from_millis(u64::from(data_read_timeout_ms)),
                                    incoming_tx.send((read_peer_id.clone(), packet)),
                                ).await {
                                    Ok(Ok(())) => {}
                                    Ok(Err(_)) => break, // receiver dropped
                                    Err(_) => {
                                        tracing::warn!(peer = %read_peer_id, "incoming queue full for data channel, disconnecting");
                                        break;
                                    }
                                }
                            }
                            Some(Err(e)) => {
                                tracing::warn!(peer = %read_peer_id, error = %e, "data channel read error");
                                break;
                            }
                            None => {
                                tracing::debug!(peer = %read_peer_id, "data channel closed by remote");
                                break;
                            }
                        }
                    }
                }
            }
            // Data channel broke — clear it so future sends fall back
            // to control. Only clear if OUR cancel token was NOT
            // externally cancelled (which would mean a newer data
            // channel replaced us via register_data_channel).
            if !cleanup_token.is_cancelled() {
                let mut conns = connections.write();
                if let Some(conn) = conns.get_mut(&read_peer_id) {
                    conn.data_tx = None;
                    conn.data_cancel = None;
                }
            }
        });
    }

    /// Enqueue a packet to be sent to a specific peer. Internal
    /// protocol messages are routed to the **control channel**;
    /// user messages prefer the **data channel** and fall back to
    /// the control channel when the data channel is unavailable.
    pub fn send_to_peer(&self, peer_id: &PeerId, packet: RawPacket) -> Result<(), NetworkError> {
        let conns = self.connections.read();
        let conn = conns
            .get(peer_id)
            .ok_or_else(|| NetworkError::PeerNotFound(peer_id.to_string()))?;

        let tx = if packet.is_internal() {
            &conn.control_tx
        } else {
            conn.data_tx.as_ref().unwrap_or(&conn.control_tx)
        };

        tx.try_send(packet).map_err(|e| match e {
            mpsc::error::TrySendError::Full(_) => NetworkError::SendQueueFull,
            mpsc::error::TrySendError::Closed(_) => {
                NetworkError::ConnectionFailed("send channel closed".into())
            }
        })
    }

    /// Send the same packet to all connected peers, optionally
    /// excluding a list. Internal messages are sent on the control
    /// channel; user messages prefer the data channel. Returns a
    /// list of peers that failed to receive the packet along with
    /// the error.
    pub fn broadcast(
        &self,
        packet: RawPacket,
        exclude: Option<&[PeerId]>,
    ) -> Vec<(PeerId, NetworkError)> {
        let use_data = !packet.is_internal();
        let targets: Vec<(PeerId, mpsc::Sender<RawPacket>)> = {
            let conns = self.connections.read();
            conns
                .iter()
                .filter(|(pid, conn)| {
                    conn.info.status == PeerStatus::Connected
                        && !exclude.is_some_and(|excl| excl.contains(pid))
                })
                .map(|(pid, conn)| {
                    let tx = if use_data {
                        conn.data_tx.as_ref().unwrap_or(&conn.control_tx).clone()
                    } else {
                        conn.control_tx.clone()
                    };
                    (pid.clone(), tx)
                })
                .collect()
        };

        let mut errors = Vec::new();
        for (pid, tx) in &targets {
            if let Err(e) = tx.try_send(packet.clone()) {
                let err = match e {
                    mpsc::error::TrySendError::Full(_) => NetworkError::SendQueueFull,
                    mpsc::error::TrySendError::Closed(_) => {
                        NetworkError::ConnectionFailed("send channel closed".into())
                    }
                };
                errors.push((pid.clone(), err));
            }
        }

        errors
    }

    /// Gracefully disconnect from a peer, sending a `PeerLeave`
    /// message and cancelling its tasks. The peer is removed from
    /// the internal map but may reconnect if allowed.
    pub fn disconnect_peer(&self, peer_id: &PeerId) -> Result<(), NetworkError> {
        let leave_msg = encode_internal(
            &InternalMessage::PeerLeave {
                peer_id: self.local_peer_id.as_str().to_owned(),
            },
            self.config.max_message_size,
        )?;
        if let Err(e) = self.send_to_peer(peer_id, leave_msg) {
            tracing::debug!(peer = %peer_id, error = %e, "failed to send PeerLeave during disconnect");
        }

        let mut conns = self.connections.write();
        if let Some(conn) = conns.get_mut(peer_id) {
            conn.info.should_reconnect = false;
            conn.control_cancel.cancel();
            if let Some(dc) = conn.data_cancel.take() {
                dc.cancel();
            }
        }
        conns.remove(peer_id);

        tracing::info!(peer = %peer_id, "peer disconnected (manual)");
        Ok(())
    }

    /// Broadcast a `PeerLeave` internal message to all connected
    /// peers, used during shutdown.
    pub fn broadcast_peer_leave(&self) {
        match encode_internal(
            &InternalMessage::PeerLeave {
                peer_id: self.local_peer_id.as_str().to_owned(),
            },
            self.config.max_message_size,
        ) {
            Ok(packet) => {
                self.broadcast(packet, None);
            }
            Err(e) => {
                tracing::error!(error = %e, "failed to encode PeerLeave for broadcast");
            }
        }
    }

    pub async fn drain_send_queues(&self, timeout: Duration) {
        let deadline = tokio::time::Instant::now() + timeout.min(Duration::from_secs(1));
        loop {
            let all_empty = {
                let conns = self.connections.read();
                conns.values().all(|c| {
                    let ctrl_empty = c.control_tx.capacity() == c.control_tx.max_capacity();
                    let data_empty = c
                        .data_tx
                        .as_ref()
                        .is_none_or(|d| d.capacity() == d.max_capacity());
                    ctrl_empty && data_empty
                })
            };
            if all_empty || tokio::time::Instant::now() >= deadline {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }

    /// Immediately terminate all connections and prevent
    /// reconnection. Called during network shutdown.
    pub fn shutdown(&self) {
        let mut conns = self.connections.write();
        for conn in conns.values_mut() {
            conn.info.should_reconnect = false;
            conn.control_cancel.cancel();
            if let Some(dc) = conn.data_cancel.take() {
                dc.cancel();
            }
        }
        conns.clear();
        tracing::info!("transport shutdown complete");
    }

    fn get_peer_list_for_sync(&self) -> Vec<(String, String)> {
        self.connections
            .read()
            .values()
            .filter(|c| c.info.status == PeerStatus::Connected)
            .map(|c| (c.info.peer_id.as_str().to_owned(), c.info.addr.to_string()))
            .collect()
    }

    /// Return a list of peer ids whose connections are currently
    /// marked as connected.
    pub fn get_connected_peer_ids(&self) -> Vec<PeerId> {
        self.connections
            .read()
            .iter()
            .filter(|(_, c)| c.info.status == PeerStatus::Connected)
            .map(|(pid, _)| pid.clone())
            .collect()
    }

    /// Check whether a connection entry exists for the given peer.
    pub fn has_peer(&self, peer_id: &PeerId) -> bool {
        self.connections.read().contains_key(peer_id)
    }

    /// Control whether the manager should attempt to reconnect to a
    /// peer after its connection is lost.
    pub fn set_should_reconnect(&self, peer_id: &PeerId, should_reconnect: bool) {
        if let Some(conn) = self.connections.write().get_mut(peer_id) {
            conn.info.should_reconnect = should_reconnect;
        }
    }

    /// Enable or disable automatic reconnection globally at runtime.
    pub fn set_auto_reconnect(&self, enabled: bool) {
        self.auto_reconnect_enabled
            .store(enabled, Ordering::Relaxed);
    }

    /// Check whether automatic reconnection is currently enabled.
    pub fn is_auto_reconnect_enabled(&self) -> bool {
        self.auto_reconnect_enabled.load(Ordering::Relaxed)
    }

    /// Update the stored status of a peer connection (e.g.
    /// Connected/Disconnected).
    pub fn update_peer_status(&self, peer_id: &PeerId, status: PeerStatus) {
        if let Some(conn) = self.connections.write().get_mut(peer_id) {
            conn.info.status = status;
        }
    }

    /// Touch the last-seen timestamp for a peer, used by
    /// membership/timeout logic.
    pub fn update_last_seen(&self, peer_id: &PeerId) {
        if let Some(conn) = self.connections.write().get_mut(peer_id) {
            conn.info.last_seen = std::time::Instant::now();
        }
    }

    /// Record a measured round-trip time for a peer and update last
    /// seen time.
    pub fn update_rtt(&self, peer_id: &PeerId, rtt_ms: u32) {
        if let Some(conn) = self.connections.write().get_mut(peer_id) {
            conn.info.rtt_ms = Some(rtt_ms);
            conn.info.last_seen = std::time::Instant::now();
        }
    }

    /// Retrieve the last known RTT for the specified peer, if any.
    pub fn get_peer_rtt(&self, peer_id: &PeerId) -> Option<u32> {
        self.connections
            .read()
            .get(peer_id)
            .and_then(|c| c.info.rtt_ms)
    }

    /// Get the stored connection status of a peer.
    pub fn get_peer_status(&self, peer_id: &PeerId) -> Option<PeerStatus> {
        self.connections.read().get(peer_id).map(|c| c.info.status)
    }

    /// Return the last-known socket address for the peer.
    pub fn get_peer_addr(&self, peer_id: &PeerId) -> Option<SocketAddr> {
        self.connections.read().get(peer_id).map(|c| c.info.addr)
    }

    #[cfg(test)]
    pub(crate) fn has_data_channel(&self, peer_id: &PeerId) -> bool {
        self.connections
            .read()
            .get(peer_id)
            .is_some_and(|c| c.data_tx.is_some())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use bytes::Bytes;

    use crate::config::NetworkConfig;

    /// Create a TransportManager for testing.
    async fn make_transport(
        peer_id: &str,
        session_id: &str,
    ) -> (Arc<TransportManager>, mpsc::Receiver<(PeerId, RawPacket)>) {
        let config = NetworkConfig::default();
        let token = CancellationToken::new();
        let (reconnect_event_tx, _reconnect_event_rx) = mpsc::channel(16);
        TransportManager::new(
            PeerId::new(peer_id),
            session_id.into(),
            config,
            reconnect_event_tx,
            token,
        )
        .await
        .unwrap()
    }

    #[test]
    fn test_packet_codec_framing() {
        let cfg = NetworkConfig::default();
        let mut codec = PacketCodec::new(cfg.max_message_size);
        let mut buf = BytesMut::new();

        let packet = RawPacket {
            msg_type: 0x0100,
            flags: 0x02,
            payload: vec![1, 2, 3, 4, 5].into(),
        };
        codec.encode(packet.clone(), &mut buf).unwrap();

        let decoded = codec.decode(&mut buf).unwrap().unwrap();
        assert_eq!(decoded.msg_type, 0x0100);
        assert_eq!(decoded.flags, 0x02);
        assert_eq!(decoded.payload, vec![1, 2, 3, 4, 5]);
    }

    #[test]
    fn test_packet_codec_partial() {
        let cfg = NetworkConfig::default();
        let mut codec = PacketCodec::new(cfg.max_message_size);
        let mut buf = BytesMut::new();

        let packet = RawPacket {
            msg_type: 0x0100,
            flags: 0,
            payload: vec![10, 20, 30].into(),
        };
        codec.encode(packet, &mut buf).unwrap();

        let full = buf.split();

        // Case 1: header incomplete (5 of 7 bytes)
        let mut partial = BytesMut::new();
        partial.extend_from_slice(&full[..5]);
        assert!(codec.decode(&mut partial).unwrap().is_none());

        partial.extend_from_slice(&full[5..]);
        let decoded = codec.decode(&mut partial).unwrap().unwrap();
        assert_eq!(decoded.payload, vec![10, 20, 30]);

        // Case 2: header complete but payload incomplete
        let mut codec2 = PacketCodec::new(cfg.max_message_size);
        let mut buf2 = BytesMut::new();
        let packet2 = RawPacket {
            msg_type: 0x0200,
            flags: 0,
            payload: vec![1, 2, 3, 4, 5].into(),
        };
        codec2.encode(packet2, &mut buf2).unwrap();
        let full2 = buf2.split();

        let mut partial2 = BytesMut::new();
        partial2.extend_from_slice(&full2[..9]); // 7 header + 2 of 5 payload
        assert!(codec2.decode(&mut partial2).unwrap().is_none());

        partial2.extend_from_slice(&full2[9..]);
        let decoded2 = codec2.decode(&mut partial2).unwrap().unwrap();
        assert_eq!(decoded2.msg_type, 0x0200);
        assert_eq!(decoded2.payload, vec![1, 2, 3, 4, 5]);
    }

    #[test]
    fn test_packet_codec_multiple() {
        let mut codec = PacketCodec::new(NetworkConfig::default().max_message_size);
        let mut buf = BytesMut::new();

        for i in 0u8..3 {
            let packet = RawPacket {
                msg_type: 0x0100 + i as u16,
                flags: i,
                payload: Bytes::from(vec![i; (i + 1) as usize]),
            };
            codec.encode(packet, &mut buf).unwrap();
        }

        for i in 0u8..3 {
            let decoded = codec.decode(&mut buf).unwrap().unwrap();
            assert_eq!(decoded.msg_type, 0x0100 + i as u16);
            assert_eq!(decoded.flags, i);
            assert_eq!(decoded.payload, vec![i; (i + 1) as usize]);
        }

        assert!(codec.decode(&mut buf).unwrap().is_none());
    }

    #[test]
    fn test_packet_codec_too_large() {
        let max = NetworkConfig::default().max_message_size;
        let mut codec = PacketCodec::new(max);
        let mut buf = BytesMut::new();
        buf.put_u32(max + 1);
        buf.put_u16(0x0100);
        buf.put_u8(0);

        let result = codec.decode(&mut buf);
        assert!(matches!(result, Err(NetworkError::MessageTooLarge(_))));
    }

    #[tokio::test]
    async fn test_already_connected_inbound() {
        let (tm_a, _rx_a) = make_transport("peer_a", "session").await;
        let (tm_b, _rx_b) = make_transport("peer_b", "session").await;

        // First connection: b → a (normal success)
        let addr_a = format!("127.0.0.1:{}", tm_a.listener_addr().port());
        tm_b.connect_to(&addr_a).await.unwrap();
        tokio::time::sleep(Duration::from_millis(100)).await;

        // Both sides should see each other
        assert!(tm_a.has_peer(&PeerId::new("peer_b")));
        assert!(tm_b.has_peer(&PeerId::new("peer_a")));

        // Second connection: b → a again — should get AlreadyConnected
        let result = tm_b.connect_to(&addr_a).await;
        assert!(
            matches!(result, Err(NetworkError::AlreadyConnected(ref id)) if id == "peer_a"),
            "expected AlreadyConnected, got: {result:?}"
        );

        // Original connection still alive
        assert!(tm_a.has_peer(&PeerId::new("peer_b")));
        assert!(tm_b.has_peer(&PeerId::new("peer_a")));
    }

    #[tokio::test]
    async fn test_already_connected_outbound() {
        let (tm_a, _rx_a) = make_transport("peer_a", "session").await;
        let (tm_b, _rx_b) = make_transport("peer_b", "session").await;

        // First connection: a → b
        let addr_b = format!("127.0.0.1:{}", tm_b.listener_addr().port());
        tm_a.connect_to(&addr_b).await.unwrap();
        tokio::time::sleep(Duration::from_millis(100)).await;

        assert!(tm_a.has_peer(&PeerId::new("peer_b")));
        assert!(tm_b.has_peer(&PeerId::new("peer_a")));

        // Now peer_a opens a second outbound connection to peer_b.
        // peer_b already has peer_a registered, so it sends
        // success=true + "already_connected" in HandshakeAck.
        let result = tm_a.connect_to(&addr_b).await;
        assert!(
            matches!(result, Err(NetworkError::AlreadyConnected(ref id)) if id == "peer_b"),
            "expected AlreadyConnected, got: {result:?}"
        );

        // Existing connection unaffected
        assert_eq!(tm_a.connection_count(), 1);
        assert_eq!(tm_b.connection_count(), 1);
    }

    #[tokio::test]
    async fn test_data_channel_established_after_connect() {
        let (tm_a, _rx_a) = make_transport("peer_a", "session").await;
        let (tm_b, _rx_b) = make_transport("peer_b", "session").await;

        let addr_a = format!("127.0.0.1:{}", tm_a.listener_addr().port());
        tm_b.connect_to(&addr_a).await.unwrap();

        // Control channel should be up quickly
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert!(tm_a.has_peer(&PeerId::new("peer_b")));
        assert!(tm_b.has_peer(&PeerId::new("peer_a")));

        // Data channel opens asynchronously — give it ample time for
        // both sides to complete the handshake.
        tokio::time::sleep(Duration::from_millis(500)).await;

        let a_has = tm_a.has_data_channel(&PeerId::new("peer_b"));
        let b_has = tm_b.has_data_channel(&PeerId::new("peer_a"));
        assert!(
            a_has || b_has,
            "expected at least one side to have a data channel (a={a_has}, b={b_has})"
        );
    }

    #[tokio::test]
    async fn test_dual_channel_message_routing() {
        let (tm_a, mut rx_a) = make_transport("peer_a", "session").await;
        let (tm_b, _rx_b) = make_transport("peer_b", "session").await;

        let addr_a = format!("127.0.0.1:{}", tm_a.listener_addr().port());
        tm_b.connect_to(&addr_a).await.unwrap();

        // Wait for data channel to be established
        tokio::time::sleep(Duration::from_millis(400)).await;

        // Drain handshake notification(s) produced during connection
        // setup so they don't interfere with the assertions below.
        while tokio::time::timeout(Duration::from_millis(100), rx_a.recv())
            .await
            .is_ok()
        {}

        // Send a user message (msg_type >= USER_MESSAGE_START) via the
        // data channel.
        let user_packet = RawPacket {
            msg_type: msg_types::USER_MESSAGE_START,
            flags: 0,
            payload: Bytes::from_static(b"hello via data channel"),
        };
        tm_b.send_to_peer(&PeerId::new("peer_a"), user_packet)
            .unwrap();

        // Send an internal message (msg_type < 0x0100) via the control
        // channel. Use PING to avoid colliding with the HANDSHAKE
        // notification already drained above.
        let internal_packet = RawPacket {
            msg_type: msg_types::PING,
            flags: 0,
            payload: Bytes::from_static(b"internal"),
        };
        tm_b.send_to_peer(&PeerId::new("peer_a"), internal_packet)
            .unwrap();

        // Both messages should arrive at tm_a
        let mut received_user = false;
        let mut received_internal = false;

        for _ in 0..20 {
            match tokio::time::timeout(Duration::from_millis(200), rx_a.recv()).await {
                Ok(Some((_pid, pkt))) => {
                    if pkt.msg_type == msg_types::USER_MESSAGE_START {
                        assert_eq!(pkt.payload.as_ref(), b"hello via data channel");
                        received_user = true;
                    } else if pkt.msg_type == msg_types::PING {
                        received_internal = true;
                    }
                    if received_user && received_internal {
                        break;
                    }
                }
                _ => break,
            }
        }

        assert!(received_user, "user message should be received");
        assert!(received_internal, "internal message should be received");
    }

    #[tokio::test]
    async fn test_socket_linger_configured() {
        use socket2::SockRef;

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let stream = TcpStream::connect(addr).await.unwrap();

        let config = NetworkConfig::default();
        TransportManager::configure_socket(&stream, &config).unwrap();

        let sock = SockRef::from(&stream);
        let linger = sock.linger().unwrap();
        assert!(
            linger.is_some(),
            "SO_LINGER should be set after configure_socket"
        );
        assert_eq!(linger.unwrap(), SO_LINGER_DURATION);
    }

    #[tokio::test]
    async fn test_data_channel_works_at_max_connections() {
        let config = NetworkConfig {
            max_connections: 2,
            ..NetworkConfig::default()
        };

        // Create 3 nodes with the low limit.
        let token_a = CancellationToken::new();
        let (reconnect_event_tx_a, _reconnect_event_rx_a) = mpsc::channel(16);
        let (tm_a, mut rx_a) = TransportManager::new(
            PeerId::new("peer_a"),
            "session_dc".into(),
            config,
            reconnect_event_tx_a,
            token_a.clone(),
        )
        .await
        .unwrap();

        let token_b = CancellationToken::new();
        let (reconnect_event_tx_b, _reconnect_event_rx_b) = mpsc::channel(16);
        let (tm_b, _rx_b) = TransportManager::new(
            PeerId::new("peer_b"),
            "session_dc".into(),
            config,
            reconnect_event_tx_b,
            token_b.clone(),
        )
        .await
        .unwrap();

        let token_c = CancellationToken::new();
        let (reconnect_event_tx_c, _reconnect_event_rx_c) = mpsc::channel(16);
        let (tm_c, _rx_c) = TransportManager::new(
            PeerId::new("peer_c"),
            "session_dc".into(),
            config,
            reconnect_event_tx_c,
            token_c.clone(),
        )
        .await
        .unwrap();

        // Fill peer_a to max_connections = 2
        let addr_a = format!("127.0.0.1:{}", tm_a.listener_addr().port());
        tm_b.connect_to(&addr_a).await.unwrap();
        tm_c.connect_to(&addr_a).await.unwrap();

        // peer_a now has 2 peers — exactly at max_connections.
        assert_eq!(tm_a.connection_count(), 2);

        // Wait for data channels to establish.
        tokio::time::sleep(Duration::from_millis(500)).await;

        // At least one side of each pair should have a data channel.
        let b_has_dc = tm_a.has_data_channel(&PeerId::new("peer_b"))
            || tm_b.has_data_channel(&PeerId::new("peer_a"));
        let c_has_dc = tm_a.has_data_channel(&PeerId::new("peer_c"))
            || tm_c.has_data_channel(&PeerId::new("peer_a"));

        assert!(
            b_has_dc,
            "data channel for peer_b should work at max_connections"
        );
        assert!(
            c_has_dc,
            "data channel for peer_c should work at max_connections"
        );

        // Drain any pending handshake notifications.
        while tokio::time::timeout(Duration::from_millis(50), rx_a.recv())
            .await
            .is_ok()
        {}

        // Verify user messages actually flow over the data channel.
        let user_pkt = RawPacket {
            msg_type: msg_types::USER_MESSAGE_START,
            flags: 0,
            payload: Bytes::from_static(b"at-limit"),
        };
        tm_b.send_to_peer(&PeerId::new("peer_a"), user_pkt).unwrap();

        let mut got = false;
        for _ in 0..10 {
            match tokio::time::timeout(Duration::from_millis(200), rx_a.recv()).await {
                Ok(Some((_pid, pkt))) if pkt.msg_type == msg_types::USER_MESSAGE_START => {
                    assert_eq!(pkt.payload.as_ref(), b"at-limit");
                    got = true;
                    break;
                }
                _ => {}
            }
        }
        assert!(
            got,
            "user message should arrive via data channel at max_connections"
        );

        token_a.cancel();
        token_b.cancel();
        token_c.cancel();
    }

    #[tokio::test]
    async fn test_auto_reconnect_toggle() {
        let (tm, _rx) = make_transport("toggle_peer", "session").await;

        // Default: enabled
        assert!(tm.is_auto_reconnect_enabled());

        tm.set_auto_reconnect(false);
        assert!(!tm.is_auto_reconnect_enabled());

        tm.set_auto_reconnect(true);
        assert!(tm.is_auto_reconnect_enabled());
    }

    #[tokio::test]
    async fn test_auto_reconnect_config_disabled() {
        let config = NetworkConfig {
            auto_reconnect_enabled: false,
            ..NetworkConfig::default()
        };
        let token = CancellationToken::new();
        let (reconnect_event_tx, _reconnect_event_rx) = mpsc::channel(16);
        let (tm, _rx) = TransportManager::new(
            PeerId::new("no_reconnect"),
            "session".into(),
            config,
            reconnect_event_tx,
            token,
        )
        .await
        .unwrap();

        assert!(!tm.is_auto_reconnect_enabled());
    }

    #[tokio::test]
    async fn test_reconnect_max_retries_config() {
        let config = NetworkConfig {
            reconnect_max_retries: 5,
            ..NetworkConfig::default()
        };
        // Verify the config value is accepted and the transport initializes
        let token = CancellationToken::new();
        let (reconnect_event_tx, _reconnect_event_rx) = mpsc::channel(16);
        let (tm, _rx) = TransportManager::new(
            PeerId::new("retry_peer"),
            "session".into(),
            config,
            reconnect_event_tx,
            token,
        )
        .await
        .unwrap();

        // Auto-reconnect should still be enabled by default
        assert!(tm.is_auto_reconnect_enabled());
    }
}
