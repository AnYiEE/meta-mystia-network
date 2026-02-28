//! Membership manager maintains peer state and broadcasts events
//! when the set of known peers changes.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Instant;

use parking_lot::RwLock;
use tokio::sync::broadcast;

use crate::config::NetworkConfig;
use crate::error::NetworkError;
use crate::types::{PeerId, PeerInfo, PeerStatus};

/// Events emitted by `MembershipManager` whenever the peer list
/// changes. Other components (leader election, callbacks) subscribe
/// to the broadcast channel.
#[derive(Clone, Debug)]
pub enum MembershipEvent {
    Joined(PeerId),
    Left(PeerId),
    StatusChanged {
        peer_id: PeerId,
        old: PeerStatus,
        new: PeerStatus,
    },
}

/// Maintains the list of known peers, their connection status,
/// and associated metadata. Thread-safe APIs allow other tasks to
/// query or update information without blocking.
pub struct MembershipManager {
    /// identity of this node
    local_peer_id: PeerId,
    /// protected map from PeerId -> PeerInfo
    peers: Arc<RwLock<HashMap<PeerId, PeerInfo>>>,
    /// configuration parameters used during timeouts and other checks
    config: NetworkConfig,
    /// broadcast sender for membership events
    pub event_tx: broadcast::Sender<MembershipEvent>,
}

impl MembershipManager {
    /// Create a fresh `MembershipManager` for the given local peer
    /// and configuration. The internal peer list starts empty, and
    /// a broadcast channel is created for membership events.
    pub fn new(local_peer_id: PeerId, config: NetworkConfig) -> Self {
        let (event_tx, _) = broadcast::channel(256);
        Self {
            local_peer_id,
            peers: Arc::new(RwLock::new(HashMap::new())),
            config,
            event_tx,
        }
    }

    /// Return the identifier assigned to this node.
    pub fn local_peer_id(&self) -> &PeerId {
        &self.local_peer_id
    }

    /// Add a new peer to the membership table. Returns an error if a
    /// peer with the same ID already exists.
    pub fn add_peer(&self, peer_id: PeerId, addr: SocketAddr) -> Result<(), NetworkError> {
        let mut peers = self.peers.write();
        if peers.contains_key(&peer_id) {
            return Err(NetworkError::DuplicatePeerId(peer_id.to_string()));
        }
        let info = PeerInfo::new(peer_id.clone(), addr);
        peers.insert(peer_id.clone(), info);
        drop(peers);

        let _ = self.event_tx.send(MembershipEvent::Joined(peer_id));
        Ok(())
    }

    /// Remove a peer from the table, signalling a `Left` event if the
    /// peer was present.
    pub fn remove_peer(&self, peer_id: &PeerId) {
        let removed = self.peers.write().remove(peer_id).is_some();
        if removed {
            let _ = self.event_tx.send(MembershipEvent::Left(peer_id.clone()));
        }
    }

    /// Update the status of an existing peer, emitting a
    /// `StatusChanged` event if the value actually changed.
    pub fn update_status(&self, peer_id: &PeerId, new_status: PeerStatus) {
        let mut peers = self.peers.write();
        if let Some(info) = peers.get_mut(peer_id) {
            let old = info.status;
            if old != new_status {
                info.status = new_status;
                drop(peers);
                let _ = self.event_tx.send(MembershipEvent::StatusChanged {
                    peer_id: peer_id.clone(),
                    old,
                    new: new_status,
                });
            }
        }
    }

    /// Check whether the given peer ID is known to the manager.
    pub fn has_peer(&self, peer_id: &PeerId) -> bool {
        self.peers.read().contains_key(peer_id)
    }

    /// Return a snapshot list of all peers currently tracked.
    pub fn get_peer_list(&self) -> Vec<PeerInfo> {
        self.peers.read().values().cloned().collect()
    }

    /// Return a list of peer IDs currently marked `Connected`.
    pub fn get_connected_peers(&self) -> Vec<PeerId> {
        self.peers
            .read()
            .iter()
            .filter(|(_, info)| info.status == PeerStatus::Connected)
            .map(|(pid, _)| pid.clone())
            .collect()
    }

    /// Get the last measured round‑trip time for a peer, if any.
    pub fn get_peer_rtt(&self, peer_id: &PeerId) -> Option<u32> {
        self.peers.read().get(peer_id).and_then(|info| info.rtt_ms)
    }

    /// Retrieve the current status of a peer, if known.
    pub fn get_peer_status(&self, peer_id: &PeerId) -> Option<PeerStatus> {
        self.peers.read().get(peer_id).map(|info| info.status)
    }

    /// Return number of peers currently marked `Connected`.
    pub fn get_connected_peer_count(&self) -> usize {
        self.peers
            .read()
            .values()
            .filter(|info| info.status == PeerStatus::Connected)
            .count()
    }

    /// Update RTT and last-seen timestamp when a Pong arrives.
    pub fn handle_pong(&self, peer_id: &PeerId, sent_timestamp_ms: u64) {
        let now = current_timestamp_ms();
        let rtt = now.saturating_sub(sent_timestamp_ms) as u32;
        let mut peers = self.peers.write();
        if let Some(info) = peers.get_mut(peer_id) {
            info.rtt_ms = Some(rtt);
            info.last_seen = Instant::now();
        }
    }

    /// Refresh only the last-seen timestamp for the given peer.
    pub fn update_last_seen(&self, peer_id: &PeerId) {
        let mut peers = self.peers.write();
        if let Some(info) = peers.get_mut(peer_id) {
            info.last_seen = Instant::now();
        }
    }

    /// Return list of peers whose last-seen exceeded the calculated
    /// timeout (heartbeat interval * multiplier). Useful for cleanup.
    pub fn check_timeouts(&self) -> Vec<PeerId> {
        let timeout_ms =
            self.config.heartbeat_interval_ms * self.config.heartbeat_timeout_multiplier as u64;
        let timeout = std::time::Duration::from_millis(timeout_ms);
        let now = Instant::now();

        let peers = self.peers.read();

        peers
            .iter()
            .filter(|(_, info)| {
                info.status == PeerStatus::Connected && now.duration_since(info.last_seen) > timeout
            })
            .map(|(pid, _)| pid.clone())
            .collect()
    }
}

/// Helper returning the current system time in milliseconds since
/// the UNIX epoch. Used for RTT calculations and heartbeat timestamps.
pub fn current_timestamp_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_add_remove_peer() {
        let mgr = MembershipManager::new(PeerId::new("local"), NetworkConfig::default());
        let addr: SocketAddr = "127.0.0.1:8080".parse().unwrap();

        assert!(mgr.add_peer(PeerId::new("peer1"), addr).is_ok());
        assert!(mgr.has_peer(&PeerId::new("peer1")));
        assert_eq!(mgr.get_connected_peer_count(), 1);

        assert!(mgr.add_peer(PeerId::new("peer1"), addr).is_err());

        mgr.remove_peer(&PeerId::new("peer1"));
        assert!(!mgr.has_peer(&PeerId::new("peer1")));
        assert_eq!(mgr.get_connected_peer_count(), 0);
    }

    #[test]
    fn test_status_update() {
        let mgr = MembershipManager::new(PeerId::new("local"), NetworkConfig::default());
        let addr: SocketAddr = "127.0.0.1:8080".parse().unwrap();

        mgr.add_peer(PeerId::new("peer1"), addr).unwrap();
        assert_eq!(
            mgr.get_peer_status(&PeerId::new("peer1")),
            Some(PeerStatus::Connected)
        );

        mgr.update_status(&PeerId::new("peer1"), PeerStatus::Disconnected);
        assert_eq!(
            mgr.get_peer_status(&PeerId::new("peer1")),
            Some(PeerStatus::Disconnected)
        );
        assert_eq!(mgr.get_connected_peer_count(), 0);
    }

    #[test]
    fn test_rtt_measurement() {
        let mgr = MembershipManager::new(PeerId::new("local"), NetworkConfig::default());
        let addr: SocketAddr = "127.0.0.1:8080".parse().unwrap();

        mgr.add_peer(PeerId::new("peer1"), addr).unwrap();
        assert_eq!(mgr.get_peer_rtt(&PeerId::new("peer1")), None);

        let ts = current_timestamp_ms().saturating_sub(50);
        mgr.handle_pong(&PeerId::new("peer1"), ts);

        let rtt = mgr.get_peer_rtt(&PeerId::new("peer1")).unwrap();
        assert!((40..=200).contains(&rtt));
    }

    #[test]
    fn test_heartbeat_timeout_detection() {
        let config = NetworkConfig {
            heartbeat_interval_ms: 10,
            heartbeat_timeout_multiplier: 1,
            ..Default::default()
        };
        let mgr = MembershipManager::new(PeerId::new("local"), config);
        let addr: SocketAddr = "127.0.0.1:8080".parse().unwrap();

        mgr.add_peer(PeerId::new("peer1"), addr).unwrap();
        assert!(mgr.check_timeouts().is_empty());

        std::thread::sleep(std::time::Duration::from_millis(20));
        let timed_out = mgr.check_timeouts();
        assert_eq!(timed_out.len(), 1);
        assert_eq!(timed_out[0].as_str(), "peer1");
    }

    #[test]
    fn test_update_last_seen_prevents_timeout() {
        let config = NetworkConfig {
            heartbeat_interval_ms: 100,
            heartbeat_timeout_multiplier: 1,
            ..Default::default()
        };
        let mgr = MembershipManager::new(PeerId::new("local"), config);
        let addr: SocketAddr = "127.0.0.1:8080".parse().unwrap();

        mgr.add_peer(PeerId::new("peer1"), addr).unwrap();
        std::thread::sleep(std::time::Duration::from_millis(60));
        mgr.update_last_seen(&PeerId::new("peer1"));
        std::thread::sleep(std::time::Duration::from_millis(60));

        assert!(mgr.check_timeouts().is_empty());
    }

    #[test]
    fn test_membership_events() {
        let mgr = MembershipManager::new(PeerId::new("local"), NetworkConfig::default());
        let mut rx = mgr.event_tx.subscribe();
        let addr: SocketAddr = "127.0.0.1:8080".parse().unwrap();

        mgr.add_peer(PeerId::new("peer1"), addr).unwrap();
        let event = rx.try_recv().unwrap();
        assert!(matches!(event, MembershipEvent::Joined(ref p) if p.as_str() == "peer1"));

        mgr.update_status(&PeerId::new("peer1"), PeerStatus::Disconnected);
        let event = rx.try_recv().unwrap();
        assert!(
            matches!(event, MembershipEvent::StatusChanged { ref peer_id, old: PeerStatus::Connected, new: PeerStatus::Disconnected } if peer_id.as_str() == "peer1")
        );

        mgr.remove_peer(&PeerId::new("peer1"));
        let event = rx.try_recv().unwrap();
        assert!(matches!(event, MembershipEvent::Left(ref p) if p.as_str() == "peer1"));
    }

    #[test]
    fn test_get_connected_peers() {
        let mgr = MembershipManager::new(PeerId::new("local"), NetworkConfig::default());
        let addr: SocketAddr = "127.0.0.1:8080".parse().unwrap();

        mgr.add_peer(PeerId::new("peer1"), addr).unwrap();
        mgr.add_peer(PeerId::new("peer2"), addr).unwrap();
        mgr.update_status(&PeerId::new("peer1"), PeerStatus::Disconnected);

        let connected = mgr.get_connected_peers();
        assert_eq!(connected.len(), 1);
        assert_eq!(connected[0].as_str(), "peer2");
    }
}
