//! Multicast DNS (mDNS) based peer discovery. Advertises the
//! local node on the LAN and attempts to connect to other peers that
//! match the same session.

use std::sync::Arc;

use mdns_sd::{ServiceDaemon, ServiceEvent, ServiceInfo};
use tokio_util::sync::CancellationToken;

use crate::config::PROTOCOL_VERSION;
use crate::error::NetworkError;
use crate::membership::MembershipManager;
use crate::transport::TransportManager;
use crate::types::PeerId;

const SERVICE_TYPE: &str = "_meta-mystia._tcp.local.";

/// Responsible for advertising the local node via mDNS and
/// discovering other peers on the same LAN session. Uses
/// `mdns_sd` crate to register a service and browse for peers.
///
/// Fields are grouped: mDNS state, identity, network helpers,
/// and shutdown control.
pub struct DiscoveryManager {
    // --- mDNS internal state ---------------------------------------------
    daemon: ServiceDaemon,

    // --- identity information --------------------------------------------
    local_peer_id: PeerId,
    session_id: String,
    instance_name: String, // derived from session + peer
    listen_port: u16,

    // --- references to other subsystems ----------------------------------
    membership: Arc<MembershipManager>,
    transport: Arc<TransportManager>,

    // --- shutdown control ------------------------------------------------
    shutdown_token: CancellationToken,
}

impl DiscoveryManager {
    pub fn new(
        local_peer_id: PeerId,
        session_id: String,
        listen_port: u16,
        membership: Arc<MembershipManager>,
        transport: Arc<TransportManager>,
        shutdown_token: CancellationToken,
    ) -> Result<Self, NetworkError> {
        let daemon = ServiceDaemon::new()
            .map_err(|e| NetworkError::Internal(format!("failed to create mDNS daemon: {e}")))?;

        let instance_name = format!("{}_{}", session_id, local_peer_id);

        Ok(Self {
            daemon,
            local_peer_id,
            session_id,
            instance_name,
            listen_port,
            membership,
            transport,
            shutdown_token,
        })
    }

    /// Begin mDNS service advertisement and start browsing for peers.
    /// Returns an error if registering the local service fails.
    pub fn start(&self) -> Result<(), NetworkError> {
        self.register_service()?;
        self.start_browse();
        Ok(())
    }

    /// Create and register the local mDNS service using our peer
    /// ID, session, and protocol version as properties.
    fn register_service(&self) -> Result<(), NetworkError> {
        let hostname = hostname::get()
            .map(|h| h.to_string_lossy().into_owned())
            .unwrap_or_else(|_| "unknown".into());

        let protocol_version = PROTOCOL_VERSION.to_string();
        let properties = [
            ("peer_id", self.local_peer_id.as_str()),
            ("session_id", self.session_id.as_str()),
            ("protocol_version", protocol_version.as_str()),
        ];

        let service = ServiceInfo::new(
            SERVICE_TYPE,
            &self.instance_name,
            &format!("{hostname}.local."),
            "",
            self.listen_port,
            &properties[..],
        )
        .map_err(|e| NetworkError::Internal(format!("failed to create service info: {e}")))?;

        self.daemon
            .register(service)
            .map_err(|e| NetworkError::Internal(format!("failed to register mDNS service: {e}")))?;

        tracing::info!(
            session = %self.session_id,
            peer = %self.local_peer_id,
            port = self.listen_port,
            "mDNS service registered"
        );

        Ok(())
    }

    /// Spawn a task that listens for incoming mDNS service events and
    /// initiates TCP connections to discovered peers that pass the
    /// filtering rules.
    fn start_browse(&self) {
        let receiver = match self.daemon.browse(SERVICE_TYPE) {
            Ok(r) => r,
            Err(e) => {
                tracing::error!(error = %e, "failed to start mDNS browse");
                return;
            }
        };

        let local_peer_id = self.local_peer_id.clone();
        let session_id = self.session_id.clone();
        let membership = Arc::clone(&self.membership);
        let transport = Arc::clone(&self.transport);
        let shutdown = self.shutdown_token.clone();

        tokio::spawn(async move {
            loop {
                tokio::select! {
                    _ = shutdown.cancelled() => break,
                    event = tokio::task::spawn_blocking({
                        let receiver = receiver.clone();
                        move || receiver.recv()
                    }) => {
                        let event = match event {
                            Ok(Ok(e)) => e,
                            _ => break,
                        };

                        match event {
                            ServiceEvent::ServiceResolved(info) => {
                                let discovered_session = info.get_property_val_str("session_id");
                                let discovered_peer = info.get_property_val_str("peer_id");

                                let (Some(session), Some(peer_id)) = (discovered_session, discovered_peer) else {
                                    continue;
                                };

                                if !Self::should_connect_to_discovered_peer(
                                    &local_peer_id,
                                    &session_id,
                                    peer_id,
                                    session,
                                    &membership,
                                ) {
                                    continue;
                                }

                                if let Some(addr) = info.get_addresses().iter().next() {
                                    let target = format!("{}:{}", addr, info.get_port());
                                    tracing::info!(
                                        peer = peer_id,
                                        addr = %target,
                                        "mDNS discovered peer, connecting"
                                    );
                                    let transport = Arc::clone(&transport);
                                    tokio::spawn(async move {
                                        if let Err(e) = transport.connect_to(&target).await {
                                            tracing::warn!(addr = %target, error = %e, "mDNS auto-connect failed");
                                        }
                                    });
                                }
                            }
                            ServiceEvent::ServiceRemoved(_, _) => {
                                // Rely on Ping/Pong timeout detection
                            }
                            _ => {}
                        }
                    }
                }
            }
        });
    }

    /// Tear down the mDNS advertisement and stop the daemon.
    pub fn shutdown(&self) {
        if let Err(e) = self
            .daemon
            .unregister(&format!("{}.{}", self.instance_name, SERVICE_TYPE))
        {
            tracing::warn!(error = %e, "mDNS unregister failed");
        }
        if let Err(e) = self.daemon.shutdown() {
            tracing::warn!(error = %e, "mDNS daemon shutdown failed");
        }
        tracing::info!("mDNS discovery shutdown");
    }

    /// Placeholder for future support of centralized discovery
    /// servers. Currently unimplemented.
    pub fn connect_to_discovery_server(_server_addr: &str) -> Result<(), NetworkError> {
        Err(NetworkError::NotImplemented)
    }

    /// Determines whether to initiate a connection to a discovered peer.
    /// Returns `true` if all conditions are met:
    /// - `discovered_session` matches our session
    /// - `discovered_peer` is not ourselves
    /// - `discovered_peer` is not already connected
    /// - Lexicographic dedup: we only connect if `local_peer_id < discovered_peer`.
    pub fn should_connect_to_discovered_peer(
        local_peer_id: &PeerId,
        local_session_id: &str,
        discovered_peer_id: &str,
        discovered_session_id: &str,
        membership: &MembershipManager,
    ) -> bool {
        if discovered_session_id != local_session_id {
            return false;
        }
        if discovered_peer_id == local_peer_id.as_str() {
            return false;
        }
        if membership.has_peer(&PeerId::from(discovered_peer_id)) {
            return false;
        }
        if local_peer_id.as_str() > discovered_peer_id {
            return false;
        }
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::config::NetworkConfig;

    fn make_membership(peer_id: &str) -> MembershipManager {
        MembershipManager::new(PeerId::new(peer_id), NetworkConfig::default())
    }

    #[test]
    fn test_discovery_same_session_connects() {
        let membership = make_membership("peer_a");
        assert!(DiscoveryManager::should_connect_to_discovered_peer(
            &PeerId::new("peer_a"),
            "session1",
            "peer_b",
            "session1",
            &membership,
        ));
    }

    #[test]
    fn test_discovery_different_session_rejects() {
        let membership = make_membership("peer_a");
        assert!(!DiscoveryManager::should_connect_to_discovered_peer(
            &PeerId::new("peer_a"),
            "session1",
            "peer_b",
            "session2",
            &membership,
        ));
    }

    #[test]
    fn test_discovery_self_excluded() {
        let membership = make_membership("peer_a");
        assert!(!DiscoveryManager::should_connect_to_discovered_peer(
            &PeerId::new("peer_a"),
            "session1",
            "peer_a",
            "session1",
            &membership,
        ));
    }

    #[test]
    fn test_discovery_already_connected_skipped() {
        let membership = make_membership("peer_a");
        let addr: std::net::SocketAddr = "127.0.0.1:8080".parse().unwrap();
        membership.add_peer(PeerId::new("peer_b"), addr).unwrap();

        assert!(!DiscoveryManager::should_connect_to_discovered_peer(
            &PeerId::new("peer_a"),
            "session1",
            "peer_b",
            "session1",
            &membership,
        ));
    }

    #[test]
    fn test_discovery_lexicographic_dedup() {
        let membership = make_membership("peer_b");
        // peer_b > peer_a → peer_b should NOT initiate (peer_a should)
        assert!(!DiscoveryManager::should_connect_to_discovered_peer(
            &PeerId::new("peer_b"),
            "session1",
            "peer_a",
            "session1",
            &membership,
        ));

        // peer_a < peer_b → peer_a SHOULD initiate
        let membership2 = make_membership("peer_a");
        assert!(DiscoveryManager::should_connect_to_discovered_peer(
            &PeerId::new("peer_a"),
            "session1",
            "peer_b",
            "session1",
            &membership2,
        ));
    }

    #[test]
    fn test_connect_to_discovery_server_not_implemented() {
        let result = DiscoveryManager::connect_to_discovery_server("1.2.3.4:5000");
        assert!(matches!(result, Err(NetworkError::NotImplemented)));
    }
}
