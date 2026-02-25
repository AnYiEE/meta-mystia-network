use std::fmt;
use std::net::SocketAddr;
use std::time::Instant;

use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
pub struct PeerId(String);

impl PeerId {
    pub fn new(id: impl Into<String>) -> Self {
        Self(id.into())
    }

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

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub enum PeerStatus {
    Connected,
    Disconnected,
    Reconnecting,
    Handshaking,
}

impl PeerStatus {
    pub fn as_i32(self) -> i32 {
        match self {
            Self::Connected => 0,
            Self::Disconnected => 1,
            Self::Reconnecting => 2,
            Self::Handshaking => 3,
        }
    }
}

#[derive(Clone, Debug)]
pub struct PeerInfo {
    pub peer_id: PeerId,
    pub addr: SocketAddr,
    pub status: PeerStatus,
    pub last_seen: Instant,
    pub rtt_ms: Option<u32>,
    pub connected_at: Instant,
    pub should_reconnect: bool,
}

impl PeerInfo {
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

#[derive(Clone, Debug)]
pub enum MessageTarget {
    Broadcast,
    ToPeer(PeerId),
    ToLeader,
}

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
