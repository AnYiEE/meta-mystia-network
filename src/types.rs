//! Basic type definitions used throughout the network stack,
//! including peer identifiers, status enums, and routing targets.

use std::fmt;
use std::net::SocketAddr;
use std::time::Instant;

use serde::{Deserialize, Serialize};

/// Unique identifier for a peer. Wrapping a `String` allows
/// strong typing rather than using raw strings everywhere.
///
/// Provides convenience conversions and display formatting.
#[derive(Clone, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
pub struct PeerId(String);

impl PeerId {
    /// Create a new `PeerId` from any type convertible to `String`.
    pub fn new(id: impl Into<String>) -> Self {
        Self(id.into())
    }

    /// Borrow the underlying string slice.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for PeerId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl From<&str> for PeerId {
    fn from(s: &str) -> Self {
        Self(s.to_owned())
    }
}

impl From<String> for PeerId {
    fn from(s: String) -> Self {
        Self(s)
    }
}

impl AsRef<str> for PeerId {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

/// Current state of a peer connection used by membership manager
/// and exported through callbacks.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub enum PeerStatus {
    Connected,
    Disconnected,
    Reconnecting,
    Handshaking,
}

impl PeerStatus {
    /// Convert the status to an integer suitable for FFI callbacks
    /// or external consumers.
    pub fn as_i32(self) -> i32 {
        match self {
            Self::Connected => 0,
            Self::Disconnected => 1,
            Self::Reconnecting => 2,
            Self::Handshaking => 3,
        }
    }
}

/// Detailed information we keep about each known peer in the
/// membership table.
#[derive(Clone, Debug)]
pub struct PeerInfo {
    // identity
    pub peer_id: PeerId,
    pub addr: SocketAddr,

    // runtime state
    pub status: PeerStatus,
    pub last_seen: Instant,
    pub rtt_ms: Option<u32>,
    pub connected_at: Instant,

    // configuration
    pub should_reconnect: bool,
}

impl PeerInfo {
    /// Construct a `PeerInfo` record for a freshly connected peer.
    /// Fields such as `last_seen` and `connected_at` are initialized
    /// to the current instant, and the default status is `Connected`.
    pub fn new(peer_id: PeerId, addr: SocketAddr) -> Self {
        let now = Instant::now();
        Self {
            peer_id,
            addr,
            status: PeerStatus::Connected,
            last_seen: now,
            rtt_ms: None,
            connected_at: now,
            should_reconnect: true,
        }
    }
}

/// Destination of a user message when routing through the session
/// layer; used by `SessionRouter`.
#[derive(Clone, Debug)]
pub enum MessageTarget {
    Broadcast,
    ToPeer(PeerId),
    ToLeader,
}

/// Where a forwarded message should be sent by the leader.
#[derive(Clone, Debug)]
pub enum ForwardTarget {
    ToPeer(PeerId),
    Broadcast,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_peer_id_new_and_as_str() {
        let id = PeerId::new("test_peer");
        assert_eq!(id.as_str(), "test_peer");
    }

    #[test]
    fn test_peer_id_display() {
        let id = PeerId::new("display_peer");
        assert_eq!(format!("{id}"), "display_peer");
    }

    #[test]
    fn test_peer_id_from_str() {
        let id: PeerId = "from_str".into();
        assert_eq!(id.as_str(), "from_str");
    }

    #[test]
    fn test_peer_id_from_string() {
        let id: PeerId = String::from("from_string").into();
        assert_eq!(id.as_str(), "from_string");
    }

    #[test]
    fn test_peer_id_as_ref() {
        let id = PeerId::new("ref_test");
        let s: &str = id.as_ref();
        assert_eq!(s, "ref_test");
    }

    #[test]
    fn test_peer_id_eq_hash() {
        use std::collections::HashSet;

        let a = PeerId::new("same");
        let b = PeerId::new("same");
        assert_eq!(a, b);

        let mut set = HashSet::new();
        set.insert(a);
        assert!(set.contains(&b));
    }

    #[test]
    fn test_peer_status_as_i32() {
        assert_eq!(PeerStatus::Connected.as_i32(), 0);
        assert_eq!(PeerStatus::Disconnected.as_i32(), 1);
        assert_eq!(PeerStatus::Reconnecting.as_i32(), 2);
        assert_eq!(PeerStatus::Handshaking.as_i32(), 3);
    }

    #[test]
    fn test_peer_info_new_defaults() {
        let addr: SocketAddr = "127.0.0.1:8080".parse().unwrap();
        let info = PeerInfo::new(PeerId::new("p1"), addr);
        assert_eq!(info.peer_id.as_str(), "p1");
        assert_eq!(info.addr, addr);
        assert_eq!(info.status, PeerStatus::Connected);
        assert!(info.rtt_ms.is_none());
        assert!(info.should_reconnect);
    }
}
