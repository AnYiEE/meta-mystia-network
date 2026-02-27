//! Routes user data messages according to the current topology and
//! centralized/leader mode. Provides operations for broadcasting,
//! targeted sends, and leader-forwarding.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};

use crate::config::NetworkConfig;
use crate::error::NetworkError;
use crate::leader::LeaderElection;
use crate::messaging::{RawPacket, encode_internal, encode_packet};
use crate::protocol::{InternalMessage, msg_types};
use crate::transport::TransportManager;
use crate::types::{ForwardTarget, MessageTarget, PeerId};

/// Routes user messages through the network. Handles
/// different delivery targets (broadcast, peer, leader) and
/// automatically forwards traffic in centralized mode.
///
/// Fields are ordered: identity, dependencies, configuration
/// state (atomics) used at runtime.
pub struct SessionRouter {
    // identity of this node
    local_peer_id: PeerId,

    // dependencies on other managers
    transport: Arc<TransportManager>,
    leader_election: Arc<LeaderElection>,

    // runtime configuration/state stored atomically for
    // lock‑free modification by other threads
    centralized_mode: AtomicBool, // true when operating under leader
    centralized_auto_forward: AtomicBool, // leader behavior toggle
    compression_threshold: AtomicU32, // size at which packets compress
    max_message_size: u32,        // maximum payload size in bytes
}

impl SessionRouter {
    /// Construct a new router. Initial behavior (auto-forward
    /// and compression threshold) is taken from the provided
    /// `NetworkConfig`.
    pub fn new(
        local_peer_id: PeerId,
        transport: Arc<TransportManager>,
        leader_election: Arc<LeaderElection>,
        config: NetworkConfig,
    ) -> Self {
        let auto_forward = config.centralized_auto_forward;
        let threshold = config.compression_threshold;
        Self {
            local_peer_id,
            transport,
            leader_election,
            centralized_mode: AtomicBool::new(false),
            centralized_auto_forward: AtomicBool::new(auto_forward),
            compression_threshold: AtomicU32::new(threshold),
            max_message_size: config.max_message_size,
        }
    }

    // --- configuration accessors -----------------------------------------

    /// Is the network currently operating in centralized (leader‑based)
    /// mode? Only the leader may send broadcast messages directly.
    pub fn is_centralized(&self) -> bool {
        self.centralized_mode.load(Ordering::Relaxed)
    }

    /// Enable or disable centralized (leader‑based) operation mode.
    /// Only leaders should set this to `true` when elected.
    pub fn set_centralized(&self, enable: bool) {
        self.centralized_mode.store(enable, Ordering::Relaxed);
    }

    /// Leader-specific setting: when true, a leader receiving a
    /// broadcast will re-broadcast it to all followers.
    pub fn is_auto_forward(&self) -> bool {
        self.centralized_auto_forward.load(Ordering::Relaxed)
    }

    /// Toggle the leader behavior that automatically re‑broadcasts
    /// a received broadcast message to all followers.
    pub fn set_auto_forward(&self, enable: bool) {
        self.centralized_auto_forward
            .store(enable, Ordering::Relaxed);
    }

    /// Packet size beyond which LZ4 compression will be attempted.
    pub fn set_compression_threshold(&self, threshold: u32) {
        self.compression_threshold
            .store(threshold, Ordering::Relaxed);
    }

    pub fn compression_threshold(&self) -> u32 {
        self.compression_threshold.load(Ordering::Relaxed)
    }

    // --- convenience helpers ---------------------------------------------

    fn is_leader(&self) -> bool {
        self.leader_election.is_leader()
    }

    fn leader_id(&self) -> Option<PeerId> {
        self.leader_election.leader_id()
    }

    pub fn route_message(
        &self,
        target: MessageTarget,
        msg_type: u16,
        data: &[u8],
        user_flags: u8,
    ) -> Result<(), NetworkError> {
        if msg_type < msg_types::USER_MESSAGE_START {
            return Err(NetworkError::InvalidArgument(
                "msg_type must be >= 0x0100".into(),
            ));
        }

        let clean_flags = user_flags & 0xFE;

        match target {
            MessageTarget::Broadcast => {
                if self.is_centralized() && !self.is_leader() {
                    let leader = self.leader_id().ok_or(NetworkError::NotLeader)?;
                    let fwd = encode_internal(
                        &InternalMessage::ForwardedUserData {
                            from_peer_id: self.local_peer_id.as_str().to_owned(),
                            original_msg_type: msg_type,
                            original_flags: clean_flags,
                            payload: data.to_vec(),
                        },
                        self.max_message_size,
                    )?;
                    self.transport.send_to_peer(&leader, fwd)?;
                } else {
                    let packet = encode_packet(
                        msg_type,
                        data,
                        clean_flags,
                        self.compression_threshold(),
                        self.max_message_size,
                    )?;
                    self.transport.broadcast(packet, None);
                }
            }
            MessageTarget::ToPeer(ref peer_id) => {
                let packet = encode_packet(
                    msg_type,
                    data,
                    clean_flags,
                    self.compression_threshold(),
                    self.max_message_size,
                )?;
                self.transport.send_to_peer(peer_id, packet)?;
            }
            MessageTarget::ToLeader => {
                if self.is_leader() {
                    return Ok(());
                }
                let leader = self.leader_id().ok_or(NetworkError::NotLeader)?;
                let packet = encode_packet(
                    msg_type,
                    data,
                    clean_flags,
                    self.compression_threshold(),
                    self.max_message_size,
                )?;
                self.transport.send_to_peer(&leader, packet)?;
            }
        }

        Ok(())
    }

    pub fn send_from_leader(
        &self,
        msg_type: u16,
        data: &[u8],
        user_flags: u8,
    ) -> Result<(), NetworkError> {
        if msg_type < msg_types::USER_MESSAGE_START {
            return Err(NetworkError::InvalidArgument(
                "msg_type must be >= 0x0100".into(),
            ));
        }
        if !self.is_leader() {
            return Err(NetworkError::NotLeader);
        }

        let clean_flags = user_flags & 0xFE;
        let packet = encode_packet(
            msg_type,
            data,
            clean_flags,
            self.compression_threshold(),
            self.max_message_size,
        )?;
        self.transport.broadcast(packet, None);

        Ok(())
    }

    pub fn forward_message(
        &self,
        from_peer_id: &PeerId,
        target: ForwardTarget,
        msg_type: u16,
        flags: u8,
        payload: &[u8],
    ) -> Result<(), NetworkError> {
        if msg_type < msg_types::USER_MESSAGE_START {
            return Err(NetworkError::InvalidArgument(
                "msg_type must be >= 0x0100".into(),
            ));
        }
        if !self.is_leader() {
            return Err(NetworkError::NotLeader);
        }

        let clean_flags = flags & 0xFE;
        let packet = encode_packet(
            msg_type,
            payload,
            clean_flags,
            self.compression_threshold(),
            self.max_message_size,
        )?;

        match target {
            ForwardTarget::ToPeer(ref peer_id) => {
                self.transport.send_to_peer(peer_id, packet)?;
            }
            ForwardTarget::Broadcast => {
                self.transport
                    .broadcast(packet, Some(std::slice::from_ref(from_peer_id)));
            }
        }

        Ok(())
    }

    pub fn handle_forwarded_user_data(
        &self,
        from_peer_id: &str,
        original_msg_type: u16,
        original_flags: u8,
        payload: &[u8],
    ) -> Option<RawPacket> {
        if self.is_leader() && self.is_auto_forward() {
            let from = PeerId::new(from_peer_id);
            if let Err(e) = self.forward_message(
                &from,
                ForwardTarget::Broadcast,
                original_msg_type,
                original_flags,
                payload,
            ) {
                tracing::warn!(error = %e, "auto-forward of user data failed");
            }
        }

        Some(RawPacket {
            msg_type: original_msg_type,
            flags: original_flags,
            payload: bytes::Bytes::copy_from_slice(payload),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    use tokio::sync::mpsc;
    use tokio_util::sync::CancellationToken;

    use crate::config::NetworkConfig;
    use crate::leader::LeaderElection;
    use crate::transport::TransportManager;
    use crate::types::PeerId;

    async fn make_router() -> SessionRouter {
        let local = PeerId::new("local");
        let config = NetworkConfig::default();
        let (transport, _incoming) = TransportManager::new(
            local.clone(),
            "session".into(),
            config,
            CancellationToken::new(),
        )
        .await
        .unwrap();
        let (tx, _rx) = mpsc::channel(16);
        let leader = Arc::new(LeaderElection::new(
            local.clone(),
            false,
            crate::config::ManualOverrideRecovery::Hold,
            tx,
        ));
        SessionRouter::new(local, Arc::clone(&transport), Arc::clone(&leader), config)
    }

    #[tokio::test]
    async fn invalid_msg_type_errors() {
        let router = make_router().await;
        let e = router
            .route_message(MessageTarget::Broadcast, 0x0001, b"", 0)
            .unwrap_err();
        assert!(matches!(e, NetworkError::InvalidArgument(_)));
    }

    #[tokio::test]
    async fn not_leader_errors() {
        let router = make_router().await;
        assert!(matches!(
            router.send_from_leader(0x0100, b"x", 0),
            Err(NetworkError::NotLeader)
        ));
        let local = PeerId::new("local");
        assert!(matches!(
            router.forward_message(&local, ForwardTarget::Broadcast, 0x0100, 0, b""),
            Err(NetworkError::NotLeader)
        ));

        // centralized broadcast when not leader should also fail
        router.set_centralized(true);
        let e = router
            .route_message(MessageTarget::Broadcast, 0x0100, b"", 0)
            .unwrap_err();
        assert!(matches!(e, NetworkError::NotLeader));
    }

    #[tokio::test]
    async fn handle_forwarded_user_data_preserves_packet() {
        let router = make_router().await;
        let raw = router.handle_forwarded_user_data("peer", 0x0100, 5, b"abc");
        let pkt = raw.unwrap();
        assert_eq!(pkt.msg_type, 0x0100);
        assert_eq!(pkt.flags, 5);
        assert_eq!(&pkt.payload[..], b"abc");
    }
}
