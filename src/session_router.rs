use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};

use crate::config::NetworkConfig;
use crate::error::NetworkError;
use crate::leader::LeaderElection;
use crate::messaging::{RawPacket, encode_internal, encode_packet};
use crate::protocol::{InternalMessage, msg_types};
use crate::transport::TransportManager;
use crate::types::{ForwardTarget, MessageTarget, PeerId};

pub struct SessionRouter {
    local_peer_id: PeerId,
    transport: Arc<TransportManager>,
    leader_election: Arc<LeaderElection>,
    centralized_mode: AtomicBool,
    centralized_auto_forward: AtomicBool,
    compression_threshold: AtomicU32,
}

impl SessionRouter {
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
        }
    }

    pub fn is_centralized(&self) -> bool {
        self.centralized_mode.load(Ordering::Relaxed)
    }

    pub fn set_centralized(&self, enable: bool) {
        self.centralized_mode.store(enable, Ordering::Relaxed);
    }

    pub fn is_auto_forward(&self) -> bool {
        self.centralized_auto_forward.load(Ordering::Relaxed)
    }

    pub fn set_auto_forward(&self, enable: bool) {
        self.centralized_auto_forward
            .store(enable, Ordering::Relaxed);
    }

    pub fn set_compression_threshold(&self, threshold: u32) {
        self.compression_threshold
            .store(threshold, Ordering::Relaxed);
    }

    pub fn compression_threshold(&self) -> u32 {
        self.compression_threshold.load(Ordering::Relaxed)
    }

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
                    let fwd = encode_internal(&InternalMessage::ForwardedUserData {
                        from_peer_id: self.local_peer_id.as_str().to_owned(),
                        original_msg_type: msg_type,
                        original_flags: clean_flags,
                        payload: data.to_vec(),
                    })?;
                    self.transport.send_to_peer(&leader, fwd)?;
                } else {
                    let packet =
                        encode_packet(msg_type, data, clean_flags, self.compression_threshold())?;
                    self.transport.broadcast(packet, None);
                }
            }
            MessageTarget::ToPeer(ref peer_id) => {
                let packet =
                    encode_packet(msg_type, data, clean_flags, self.compression_threshold())?;
                self.transport.send_to_peer(peer_id, packet)?;
            }
            MessageTarget::ToLeader => {
                if self.is_leader() {
                    return Ok(());
                }
                let leader = self.leader_id().ok_or(NetworkError::NotLeader)?;
                let packet =
                    encode_packet(msg_type, data, clean_flags, self.compression_threshold())?;
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
        let packet = encode_packet(msg_type, data, clean_flags, self.compression_threshold())?;
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
        let packet = encode_packet(msg_type, payload, clean_flags, self.compression_threshold())?;

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
