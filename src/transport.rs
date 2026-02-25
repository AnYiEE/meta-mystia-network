use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use bytes::{BufMut, BytesMut};
use futures_util::{SinkExt, StreamExt};
use parking_lot::RwLock;
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::mpsc;
use tokio_util::codec::{Decoder, Encoder, Framed};
use tokio_util::sync::CancellationToken;

use crate::config::{
    HANDSHAKE_TIMEOUT_MS, MAX_CONNECTIONS, MAX_MESSAGE_SIZE, NetworkConfig, PROTOCOL_VERSION,
};
use crate::error::NetworkError;
use crate::messaging::{RawPacket, decode_internal, encode_internal};
use crate::protocol::InternalMessage;
use crate::types::{PeerId, PeerInfo, PeerStatus};

pub struct PacketCodec {
    max_message_size: u32,
}

impl PacketCodec {
    pub fn new() -> Self {
        Self {
            max_message_size: MAX_MESSAGE_SIZE,
        }
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
        let length = item.payload.len() as u32;
        dst.reserve(7 + item.payload.len());
        dst.put_u32(length);
        dst.put_u16(item.msg_type);
        dst.put_u8(item.flags);
        dst.put_slice(&item.payload);
        Ok(())
    }
}

struct PeerConnection {
    send_tx: mpsc::Sender<RawPacket>,
    cancel_token: CancellationToken,
    info: PeerInfo,
}

pub struct TransportManager {
    local_peer_id: PeerId,
    session_id: String,
    config: NetworkConfig,
    connections: Arc<RwLock<HashMap<PeerId, PeerConnection>>>,
    listener_addr: SocketAddr,
    incoming_tx: mpsc::Sender<(PeerId, RawPacket)>,
    shutdown_token: CancellationToken,
}

impl TransportManager {
    pub async fn new(
        local_peer_id: PeerId,
        session_id: String,
        config: NetworkConfig,
        shutdown_token: CancellationToken,
    ) -> Result<(Arc<Self>, mpsc::Receiver<(PeerId, RawPacket)>), NetworkError> {
        let listener = TcpListener::bind("0.0.0.0:0").await?;
        let listener_addr = listener.local_addr()?;
        tracing::info!(addr = %listener_addr, "TCP listener started");

        let (incoming_tx, incoming_rx) = mpsc::channel(256);

        let manager = Arc::new(Self {
            local_peer_id,
            session_id,
            config,
            connections: Arc::new(RwLock::new(HashMap::new())),
            listener_addr,
            incoming_tx,
            shutdown_token: shutdown_token.clone(),
        });

        let mgr = Arc::clone(&manager);
        tokio::spawn(async move {
            mgr.accept_loop(listener).await;
        });

        Ok((manager, incoming_rx))
    }

    pub fn listener_addr(&self) -> SocketAddr {
        self.listener_addr
    }

    pub fn local_peer_id(&self) -> &PeerId {
        &self.local_peer_id
    }

    pub fn session_id(&self) -> &str {
        &self.session_id
    }

    pub fn connection_count(&self) -> usize {
        self.connections.read().len()
    }

    async fn accept_loop(self: &Arc<Self>, listener: TcpListener) {
        loop {
            tokio::select! {
                _ = self.shutdown_token.cancelled() => break,
                result = listener.accept() => {
                    match result {
                        Ok((stream, addr)) => {
                            if self.connections.read().len() >= MAX_CONNECTIONS {
                                tracing::warn!(%addr, "max connections reached, rejecting");
                                continue;
                            }
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

    fn configure_keepalive(stream: &TcpStream) -> Result<(), NetworkError> {
        use socket2::SockRef;

        let sock = SockRef::from(stream);
        let keepalive = socket2::TcpKeepalive::new()
            .with_time(Duration::from_secs(60))
            .with_interval(Duration::from_secs(10));

        #[cfg(not(target_os = "windows"))]
        let keepalive = keepalive.with_retries(3);

        sock.set_tcp_keepalive(&keepalive)?;

        Ok(())
    }

    async fn handle_incoming_connection(
        self: &Arc<Self>,
        stream: TcpStream,
        addr: SocketAddr,
    ) -> Result<(), NetworkError> {
        Self::configure_keepalive(&stream)?;

        let mut framed = Framed::new(stream, PacketCodec::new());

        let handshake_result = tokio::time::timeout(
            Duration::from_millis(HANDSHAKE_TIMEOUT_MS),
            self.receive_handshake(&mut framed),
        )
        .await;

        match handshake_result {
            Ok(Ok((peer_id, listen_port, notify_packet))) => {
                let peer_addr = SocketAddr::new(addr.ip(), listen_port);
                self.register_connection(peer_id.clone(), peer_addr, framed)?;
                let _ = self.incoming_tx.send((peer_id, notify_packet)).await;
                Ok(())
            }
            Ok(Err(e)) => Err(e),
            Err(_) => {
                tracing::warn!(%addr, "handshake timeout");
                Err(NetworkError::HandshakeTimeout)
            }
        }
    }

    async fn receive_handshake(
        &self,
        framed: &mut Framed<TcpStream, PacketCodec>,
    ) -> Result<(PeerId, u16, RawPacket), NetworkError> {
        let packet = framed
            .next()
            .await
            .ok_or_else(|| NetworkError::HandshakeFailed("connection closed".into()))??;

        let msg = decode_internal(&packet)?;

        match msg {
            InternalMessage::Handshake {
                peer_id,
                listen_port,
                protocol_version,
                session_id,
            } => {
                if protocol_version != PROTOCOL_VERSION {
                    let ack = encode_internal(&InternalMessage::HandshakeAck {
                        peer_id: self.local_peer_id.as_str().to_owned(),
                        listen_port: self.listener_addr.port(),
                        success: false,
                        error_reason: Some(format!(
                            "version_mismatch: expected {PROTOCOL_VERSION}, got {protocol_version}"
                        )),
                    })?;
                    let _ = framed.send(ack).await;
                    return Err(NetworkError::VersionMismatch {
                        expected: PROTOCOL_VERSION,
                        got: protocol_version,
                    });
                }

                if session_id != self.session_id {
                    let ack = encode_internal(&InternalMessage::HandshakeAck {
                        peer_id: self.local_peer_id.as_str().to_owned(),
                        listen_port: self.listener_addr.port(),
                        success: false,
                        error_reason: Some("session_mismatch".into()),
                    })?;
                    let _ = framed.send(ack).await;
                    return Err(NetworkError::SessionMismatch {
                        expected: self.session_id.clone(),
                        got: session_id,
                    });
                }

                let pid = PeerId::new(&peer_id);
                if self.connections.read().contains_key(&pid) {
                    let ack = encode_internal(&InternalMessage::HandshakeAck {
                        peer_id: self.local_peer_id.as_str().to_owned(),
                        listen_port: self.listener_addr.port(),
                        success: false,
                        error_reason: Some("duplicate_peer_id".into()),
                    })?;
                    let _ = framed.send(ack).await;
                    return Err(NetworkError::DuplicatePeerId(peer_id));
                }

                let ack = encode_internal(&InternalMessage::HandshakeAck {
                    peer_id: self.local_peer_id.as_str().to_owned(),
                    listen_port: self.listener_addr.port(),
                    success: true,
                    error_reason: None,
                })?;
                framed.send(ack).await.map_err(|e| {
                    NetworkError::ConnectionFailed(format!("failed to send handshake ack: {e}"))
                })?;

                let peer_list = self.get_peer_list_for_sync();
                if !peer_list.is_empty() {
                    let sync_msg =
                        encode_internal(&InternalMessage::PeerListSync { peers: peer_list })?;
                    let _ = framed.send(sync_msg).await;
                }

                let notify_packet = encode_internal(&InternalMessage::Handshake {
                    peer_id: peer_id.clone(),
                    listen_port,
                    protocol_version,
                    session_id,
                })?;

                Ok((pid, listen_port, notify_packet))
            }
            _ => Err(NetworkError::HandshakeFailed(
                "expected Handshake message".into(),
            )),
        }
    }

    pub async fn connect_to(self: &Arc<Self>, addr: &str) -> Result<PeerId, NetworkError> {
        if self.connections.read().len() >= MAX_CONNECTIONS {
            return Err(NetworkError::MaxConnectionsReached);
        }

        let socket_addr: SocketAddr = addr
            .parse()
            .map_err(|e| NetworkError::InvalidArgument(format!("invalid address: {e}")))?;

        let stream = tokio::time::timeout(
            Duration::from_millis(HANDSHAKE_TIMEOUT_MS),
            TcpStream::connect(socket_addr),
        )
        .await
        .map_err(|_| NetworkError::HandshakeTimeout)?
        .map_err(|e| NetworkError::ConnectionFailed(format!("TCP connect failed: {e}")))?;

        Self::configure_keepalive(&stream)?;

        let mut framed = Framed::new(stream, PacketCodec::new());

        let handshake = encode_internal(&InternalMessage::Handshake {
            peer_id: self.local_peer_id.as_str().to_owned(),
            listen_port: self.listener_addr.port(),
            protocol_version: PROTOCOL_VERSION,
            session_id: self.session_id.clone(),
        })?;

        framed.send(handshake).await.map_err(|e| {
            NetworkError::ConnectionFailed(format!("failed to send handshake: {e}"))
        })?;

        let ack_result = tokio::time::timeout(
            Duration::from_millis(HANDSHAKE_TIMEOUT_MS),
            self.receive_handshake_ack(&mut framed),
        )
        .await;

        match ack_result {
            Ok(Ok((peer_id, listen_port, notify_packet))) => {
                let peer_addr = SocketAddr::new(socket_addr.ip(), listen_port);
                self.register_connection(peer_id.clone(), peer_addr, framed)?;
                let _ = self
                    .incoming_tx
                    .send((peer_id.clone(), notify_packet))
                    .await;
                Ok(peer_id)
            }
            Ok(Err(e)) => Err(e),
            Err(_) => Err(NetworkError::HandshakeTimeout),
        }
    }

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
                    return Err(NetworkError::HandshakeFailed(reason));
                }
                let pid = PeerId::new(&peer_id);

                let notify_packet = encode_internal(&InternalMessage::HandshakeAck {
                    peer_id,
                    listen_port,
                    success: true,
                    error_reason: None,
                })?;

                Ok((pid, listen_port, notify_packet))
            }
            _ => Err(NetworkError::HandshakeFailed(
                "expected HandshakeAck message".into(),
            )),
        }
    }

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
                    send_tx,
                    cancel_token: cancel_token.clone(),
                    info,
                },
            );
        }

        tracing::info!(peer = %peer_id, addr = %peer_addr, "peer connected");

        let write_cancel = cancel_token.clone();
        tokio::spawn(async move {
            loop {
                tokio::select! {
                    _ = write_cancel.cancelled() => break,
                    msg = send_rx.recv() => {
                        match msg {
                            Some(packet) => {
                                if let Err(e) = sink.send(packet).await {
                                    tracing::warn!(error = %e, "write task send error");
                                    break;
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
                                if incoming_tx.send((read_peer_id.clone(), packet)).await.is_err() {
                                    break;
                                }
                            }
                            Some(Err(e)) => {
                                tracing::warn!(peer = %read_peer_id, error = %e, "read error");
                                break;
                            }
                            None => {
                                tracing::info!(peer = %read_peer_id, "connection closed by remote");
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
                    conn.cancel_token.cancel();
                    let addr = if conn.info.should_reconnect && !mgr.shutdown_token.is_cancelled() {
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
                mgr.spawn_reconnect(read_peer_id, addr);
            }
        });

        Ok(())
    }

    fn spawn_reconnect(self: &Arc<Self>, peer_id: PeerId, addr: String) {
        let mgr = Arc::clone(self);
        let initial_ms = self.config.reconnect_initial_ms;
        let max_ms = self.config.reconnect_max_ms;
        let shutdown = self.shutdown_token.clone();

        tokio::spawn(async move {
            let mut delay_ms = initial_ms;
            loop {
                tracing::info!(peer = %peer_id, delay_ms, "scheduling reconnect");

                tokio::select! {
                    _ = shutdown.cancelled() => break,
                    _ = tokio::time::sleep(Duration::from_millis(delay_ms)) => {}
                }

                let result = tokio::select! {
                    _ = shutdown.cancelled() => break,
                    r = mgr.connect_to(&addr) => r,
                };

                match result {
                    Ok(_) => {
                        tracing::info!(peer = %peer_id, "reconnected successfully");
                        break;
                    }
                    Err(e) => {
                        tracing::warn!(peer = %peer_id, error = %e, "reconnect failed");
                        delay_ms = (delay_ms * 2).min(max_ms);
                    }
                }
            }
        });
    }

    pub fn send_to_peer(&self, peer_id: &PeerId, packet: RawPacket) -> Result<(), NetworkError> {
        let conns = self.connections.read();
        let conn = conns
            .get(peer_id)
            .ok_or_else(|| NetworkError::PeerNotFound(peer_id.to_string()))?;

        conn.send_tx.try_send(packet).map_err(|e| match e {
            mpsc::error::TrySendError::Full(_) => NetworkError::SendQueueFull,
            mpsc::error::TrySendError::Closed(_) => {
                NetworkError::ConnectionFailed("send channel closed".into())
            }
        })
    }

    pub fn broadcast(
        &self,
        packet: RawPacket,
        exclude: Option<&[PeerId]>,
    ) -> Vec<(PeerId, NetworkError)> {
        let targets: Vec<(PeerId, mpsc::Sender<RawPacket>)> = {
            let conns = self.connections.read();
            conns
                .iter()
                .filter(|(pid, conn)| {
                    conn.info.status == PeerStatus::Connected
                        && !exclude.is_some_and(|excl| excl.contains(pid))
                })
                .map(|(pid, conn)| (pid.clone(), conn.send_tx.clone()))
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

    pub fn disconnect_peer(&self, peer_id: &PeerId) -> Result<(), NetworkError> {
        let leave_msg = encode_internal(&InternalMessage::PeerLeave {
            peer_id: self.local_peer_id.as_str().to_owned(),
        })?;
        if let Err(e) = self.send_to_peer(peer_id, leave_msg) {
            tracing::debug!(peer = %peer_id, error = %e, "failed to send PeerLeave during disconnect");
        }

        let mut conns = self.connections.write();
        if let Some(conn) = conns.get_mut(peer_id) {
            conn.info.should_reconnect = false;
            conn.cancel_token.cancel();
        }
        conns.remove(peer_id);

        tracing::info!(peer = %peer_id, "peer disconnected (manual)");
        Ok(())
    }

    pub fn broadcast_peer_leave(&self) {
        match encode_internal(&InternalMessage::PeerLeave {
            peer_id: self.local_peer_id.as_str().to_owned(),
        }) {
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
                conns
                    .values()
                    .all(|c| c.send_tx.capacity() == c.send_tx.max_capacity())
            };
            if all_empty || tokio::time::Instant::now() >= deadline {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }

    pub fn shutdown(&self) {
        let mut conns = self.connections.write();
        for conn in conns.values_mut() {
            conn.info.should_reconnect = false;
            conn.cancel_token.cancel();
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

    pub fn get_connected_peer_ids(&self) -> Vec<PeerId> {
        self.connections
            .read()
            .iter()
            .filter(|(_, c)| c.info.status == PeerStatus::Connected)
            .map(|(pid, _)| pid.clone())
            .collect()
    }

    pub fn has_peer(&self, peer_id: &PeerId) -> bool {
        self.connections.read().contains_key(peer_id)
    }

    pub fn set_should_reconnect(&self, peer_id: &PeerId, should_reconnect: bool) {
        if let Some(conn) = self.connections.write().get_mut(peer_id) {
            conn.info.should_reconnect = should_reconnect;
        }
    }

    pub fn update_peer_status(&self, peer_id: &PeerId, status: PeerStatus) {
        if let Some(conn) = self.connections.write().get_mut(peer_id) {
            conn.info.status = status;
        }
    }

    pub fn update_last_seen(&self, peer_id: &PeerId) {
        if let Some(conn) = self.connections.write().get_mut(peer_id) {
            conn.info.last_seen = std::time::Instant::now();
        }
    }

    pub fn update_rtt(&self, peer_id: &PeerId, rtt_ms: u32) {
        if let Some(conn) = self.connections.write().get_mut(peer_id) {
            conn.info.rtt_ms = Some(rtt_ms);
            conn.info.last_seen = std::time::Instant::now();
        }
    }

    pub fn get_peer_rtt(&self, peer_id: &PeerId) -> Option<u32> {
        self.connections
            .read()
            .get(peer_id)
            .and_then(|c| c.info.rtt_ms)
    }

    pub fn get_peer_status(&self, peer_id: &PeerId) -> Option<PeerStatus> {
        self.connections.read().get(peer_id).map(|c| c.info.status)
    }

    pub fn get_peer_addr(&self, peer_id: &PeerId) -> Option<SocketAddr> {
        self.connections.read().get(peer_id).map(|c| c.info.addr)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use bytes::Bytes;

    #[test]
    fn test_packet_codec_framing() {
        let mut codec = PacketCodec::new();
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
        let mut codec = PacketCodec::new();
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
        let mut codec2 = PacketCodec::new();
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
        let mut codec = PacketCodec::new();
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
        let mut codec = PacketCodec::new();
        let mut buf = BytesMut::new();
        buf.put_u32(MAX_MESSAGE_SIZE + 1);
        buf.put_u16(0x0100);
        buf.put_u8(0);

        let result = codec.decode(&mut buf);
        assert!(matches!(result, Err(NetworkError::MessageTooLarge(_))));
    }
}
