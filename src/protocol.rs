//! Definitions of internal protocol messages and their numeric
//! type codes used during handshakes, heartbeats, elections, etc.

use serde::{Deserialize, Serialize};

/// Messages exchanged between peers to manage discovery, leader
/// election, heartbeats and to carry forwarded user payloads. All
/// internal traffic is serialized with `postcard` and transported
/// using the `RawPacket` wrapper.
#[derive(Clone, Debug, Deserialize, Serialize)]
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

    // user messages (>= 0x0100) are sent via RawPacket without
    // internal encoding
    pub const USER_MESSAGE_START: u16 = 0x0100;
}
