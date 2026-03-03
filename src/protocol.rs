//! Definitions of internal protocol messages and their numeric
//! type codes used during handshakes, heartbeats, elections, etc.

use serde::{Deserialize, Serialize};

/// Messages exchanged between peers to manage discovery, leader
/// election, heartbeats and to carry forwarded user payloads. All
/// internal traffic is serialized with `postcard` and transported
/// using the `RawPacket` wrapper.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub enum InternalMessage {
    // --- connection handshake --------------------------------------------
    Handshake {
        peer_id: String,
        listen_port: u16,
        protocol_version: u16,
        session_id: String,
    },
    HandshakeAck {
        peer_id: String,
        listen_port: u16,
        success: bool,
        error_reason: Option<String>,
    },

    // --- membership notifications ----------------------------------------
    PeerLeave {
        peer_id: String,
    },
    PeerListSync {
        peers: Vec<(String, String)>, // (peer_id, addr)
    },

    // --- heartbeat/ping for liveness --------------------------------------
    Heartbeat {
        term: u64,
        leader_id: String,
        timestamp_ms: u64,
    },
    HeartbeatResponse {
        term: u64,
        timestamp_ms: u64,
    },
    Ping {
        timestamp_ms: u64,
    },
    Pong {
        timestamp_ms: u64,
    },

    // --- leader election messages ---------------------------------------
    RequestVote {
        term: u64,
        candidate_id: String,
    },
    VoteResponse {
        term: u64,
        voter_id: String,
        granted: bool,
    },
    LeaderAssign {
        term: u64,
        leader_id: String,
        assigner_id: String,
    },

    // --- forwarded user data (used by centralized routing) --------------
    ForwardedUserData {
        from_peer_id: String,
        original_msg_type: u16,
        original_flags: u8,
        payload: Vec<u8>,
    },

    // --- dual-channel handshake (data channel establishment) ------------
    // Sent on a newly opened TCP connection to attach it as the
    // **data channel** for an already-connected peer. The control
    // channel must have been established first via the normal
    // `Handshake` / `HandshakeAck` flow.
    DataChannelHandshake {
        peer_id: String,
    },
    DataChannelHandshakeAck {
        peer_id: String,
        success: bool,
    },
}

impl InternalMessage {
    /// Return the wire-level `msg_type` code for this message.
    ///
    /// This is the canonical mapping between enum variants and the
    /// numeric constants in [`msg_types`]. Used by [`encode_internal`]
    /// to populate the `RawPacket` header.
    ///
    /// [`encode_internal`]: crate::messaging::encode_internal
    pub fn msg_type(&self) -> u16 {
        match self {
            Self::Handshake { .. } => msg_types::HANDSHAKE,
            Self::HandshakeAck { .. } => msg_types::HANDSHAKE_ACK,
            Self::PeerLeave { .. } => msg_types::PEER_LEAVE,
            Self::PeerListSync { .. } => msg_types::PEER_LIST_SYNC,
            Self::Heartbeat { .. } => msg_types::HEARTBEAT,
            Self::HeartbeatResponse { .. } => msg_types::HEARTBEAT_RESPONSE,
            Self::Ping { .. } => msg_types::PING,
            Self::Pong { .. } => msg_types::PONG,
            Self::RequestVote { .. } => msg_types::REQUEST_VOTE,
            Self::VoteResponse { .. } => msg_types::VOTE_RESPONSE,
            Self::LeaderAssign { .. } => msg_types::LEADER_ASSIGN,
            Self::ForwardedUserData { .. } => msg_types::FORWARDED_USER_DATA,
            Self::DataChannelHandshake { .. } => msg_types::DATA_CHANNEL_HANDSHAKE,
            Self::DataChannelHandshakeAck { .. } => msg_types::DATA_CHANNEL_HANDSHAKE_ACK,
        }
    }
}

pub mod msg_types {
    // handshake
    pub const HANDSHAKE: u16 = 0x0001;
    pub const HANDSHAKE_ACK: u16 = 0x0002;

    // membership
    pub const PEER_LEAVE: u16 = 0x0003;
    pub const PEER_LIST_SYNC: u16 = 0x0004;

    // heartbeat / ping
    pub const HEARTBEAT: u16 = 0x0010;
    pub const HEARTBEAT_RESPONSE: u16 = 0x0011;
    pub const PING: u16 = 0x0012;
    pub const PONG: u16 = 0x0013;

    // election
    pub const REQUEST_VOTE: u16 = 0x0020;
    pub const VOTE_RESPONSE: u16 = 0x0021;
    pub const LEADER_ASSIGN: u16 = 0x0022;

    // forwarded payload
    pub const FORWARDED_USER_DATA: u16 = 0x0030;

    // dual-channel handshake
    pub const DATA_CHANNEL_HANDSHAKE: u16 = 0x0040;
    pub const DATA_CHANNEL_HANDSHAKE_ACK: u16 = 0x0041;

    // user messages (>= 0x0100) are sent via RawPacket without
    // internal encoding
    pub const USER_MESSAGE_START: u16 = 0x0100;

    /// Return all internal message type constants in definition order.
    /// Used by tests to verify uniqueness and range invariants.
    #[cfg(test)]
    pub const fn all() -> [u16; 14] {
        [
            HANDSHAKE,
            HANDSHAKE_ACK,
            PEER_LEAVE,
            PEER_LIST_SYNC,
            HEARTBEAT,
            HEARTBEAT_RESPONSE,
            PING,
            PONG,
            REQUEST_VOTE,
            VOTE_RESPONSE,
            LEADER_ASSIGN,
            FORWARDED_USER_DATA,
            DATA_CHANNEL_HANDSHAKE,
            DATA_CHANNEL_HANDSHAKE_ACK,
        ]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_msg_type_returns_internal_range() {
        let messages: Vec<InternalMessage> = vec![
            InternalMessage::Handshake {
                peer_id: "p".into(),
                listen_port: 0,
                protocol_version: 1,
                session_id: "s".into(),
            },
            InternalMessage::HandshakeAck {
                peer_id: "p".into(),
                listen_port: 0,
                success: true,
                error_reason: None,
            },
            InternalMessage::PeerLeave {
                peer_id: "p".into(),
            },
            InternalMessage::PeerListSync { peers: vec![] },
            InternalMessage::Heartbeat {
                term: 0,
                leader_id: "p".into(),
                timestamp_ms: 0,
            },
            InternalMessage::HeartbeatResponse {
                term: 0,
                timestamp_ms: 0,
            },
            InternalMessage::Ping { timestamp_ms: 0 },
            InternalMessage::Pong { timestamp_ms: 0 },
            InternalMessage::RequestVote {
                term: 0,
                candidate_id: "p".into(),
            },
            InternalMessage::VoteResponse {
                term: 0,
                voter_id: "p".into(),
                granted: false,
            },
            InternalMessage::LeaderAssign {
                term: 0,
                leader_id: "p".into(),
                assigner_id: "p".into(),
            },
            InternalMessage::ForwardedUserData {
                from_peer_id: "p".into(),
                original_msg_type: 0x0100,
                original_flags: 0,
                payload: vec![],
            },
            InternalMessage::DataChannelHandshake {
                peer_id: "p".into(),
            },
            InternalMessage::DataChannelHandshakeAck {
                peer_id: "p".into(),
                success: true,
            },
        ];

        for msg in &messages {
            assert!(
                msg.msg_type() < msg_types::USER_MESSAGE_START,
                "{msg:?} has msg_type {:#06x} which is >= USER_MESSAGE_START",
                msg.msg_type()
            );
        }
    }

    #[test]
    fn test_msg_type_constants_unique() {
        let codes = msg_types::all();
        for (i, &a) in codes.iter().enumerate() {
            for &b in &codes[i + 1..] {
                assert_ne!(a, b, "duplicate msg_type constant: {a:#06x}");
            }
        }
    }

    #[test]
    fn test_msg_type_all_covers_every_constant() {
        // 14 InternalMessage variants → 14 msg_type constants.
        assert_eq!(msg_types::all().len(), 14);
    }

    #[test]
    fn test_msg_type_expected_values() {
        assert_eq!(
            InternalMessage::Handshake {
                peer_id: String::new(),
                listen_port: 0,
                protocol_version: 0,
                session_id: String::new(),
            }
            .msg_type(),
            msg_types::HANDSHAKE
        );
        assert_eq!(
            InternalMessage::Ping { timestamp_ms: 0 }.msg_type(),
            msg_types::PING
        );
        assert_eq!(
            InternalMessage::DataChannelHandshake {
                peer_id: String::new()
            }
            .msg_type(),
            msg_types::DATA_CHANNEL_HANDSHAKE
        );
    }

    #[test]
    fn test_internal_message_partial_eq() {
        let a = InternalMessage::Ping { timestamp_ms: 42 };
        let b = InternalMessage::Ping { timestamp_ms: 42 };
        let c = InternalMessage::Ping { timestamp_ms: 99 };
        assert_eq!(a, b);
        assert_ne!(a, c);

        let d = InternalMessage::Pong { timestamp_ms: 42 };
        assert_ne!(a, d, "different variants with same field value");
    }
}
