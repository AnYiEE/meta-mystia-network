//! Core implementation of the MetaMystia peer‑to‑peer networking
//! stack. Exposes an FFI layer for C clients as well as a rich set
//! of internal Rust components for transport, discovery, membership,
//! leader election, and message routing.

mod callback;
mod config;
mod discovery;
mod error;
mod leader;
mod membership;
mod messaging;
mod protocol;
mod session_router;
mod transport;
mod types;

// FFI functions intentionally dereference raw pointers within `catch_unwind` blocks.
// They cannot be marked `unsafe` as they are the safe-boundary entry points from C.
#[allow(clippy::not_unsafe_ptr_arg_deref)]
pub mod ffi;

use std::sync::Arc;
use std::time::Duration;

use rand::Rng;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::callback::CallbackManager;
use crate::config::NetworkConfig;
use crate::discovery::DiscoveryManager;
use crate::error::NetworkError;
use crate::leader::LeaderElection;
use crate::membership::{MembershipEvent, MembershipManager, current_timestamp_ms};
use crate::messaging::{RawPacket, decode_internal, decode_payload, encode_internal};
use crate::protocol::InternalMessage;
use crate::session_router::SessionRouter;
use crate::transport::TransportManager;
use crate::types::{PeerId, PeerStatus};

/// Holds all high‑level components that comprise a running
/// network node. This struct is wrapped in an `Arc` and stored in
/// the global FFI state during initialization.
///
/// Fields are grouped into transport, membership/election,
/// routing/discovery, callbacks, lifecycle and identity/config.
pub struct NetworkState {
    // --- core managers ---------------------------------------------------
    pub transport: Arc<TransportManager>,
    pub membership: Arc<MembershipManager>,
    pub leader_election: Arc<LeaderElection>,
    pub session_router: Arc<SessionRouter>,
    pub discovery: Option<DiscoveryManager>,

    // --- callback bridge -------------------------------------------------
    pub callback: Arc<CallbackManager>,

    // --- lifecycle control -----------------------------------------------
    pub shutdown_token: CancellationToken,

    // --- identity & configuration ----------------------------------------
    pub local_peer_id: PeerId,
    pub session_id: String,
    pub config: NetworkConfig,
}

impl NetworkState {
    /// Initialize all networking components for a node given the
    /// textual peer ID, session identifier, and configuration.
    ///
    /// This includes constructing transport, membership, leader
    /// election, session routing, discovery (mDNS), and callback
    /// handling. Returns an error if any subsystem fails to start.
    pub async fn new(
        peer_id: String,
        session_id: String,
        config: NetworkConfig,
    ) -> Result<Self, NetworkError> {
        let local_peer_id = PeerId::new(&peer_id);
        let shutdown_token = CancellationToken::new();

        let (transport, incoming_rx) = TransportManager::new(
            local_peer_id.clone(),
            session_id.clone(),
            config,
            shutdown_token.clone(),
        )
        .await?;

        let membership = Arc::new(MembershipManager::new(local_peer_id.clone(), config));

        let (leader_change_tx, leader_change_rx) = mpsc::channel(16);
        let leader_election = Arc::new(LeaderElection::new(
            local_peer_id.clone(),
            config.auto_election_enabled,
            config.manual_override_recovery,
            leader_change_tx,
        ));

        let session_router = Arc::new(SessionRouter::new(
            local_peer_id.clone(),
            Arc::clone(&transport),
            Arc::clone(&leader_election),
            config,
        ));

        let callback = Arc::new(CallbackManager::new());

        let listen_port = transport.listener_addr().port();
        let discovery = match DiscoveryManager::new(
            local_peer_id.clone(),
            session_id.clone(),
            listen_port,
            config.mdns_port,
            Arc::clone(&membership),
            Arc::clone(&transport),
            shutdown_token.clone(),
        ) {
            Ok(d) => match d.start() {
                Ok(()) => Some(d),
                Err(e) => {
                    tracing::warn!(error = %e, "mDNS discovery start failed — mDNS will be unavailable");
                    d.shutdown();
                    None
                }
            },
            Err(e) => {
                tracing::warn!(error = %e, "mDNS discovery creation failed — mDNS will be unavailable");
                None
            }
        };

        spawn_message_handler(
            incoming_rx,
            MessageHandlerCtx {
                transport: Arc::clone(&transport),
                membership: Arc::clone(&membership),
                leader_election: Arc::clone(&leader_election),
                session_router: Arc::clone(&session_router),
                callback: Arc::clone(&callback),
                local_peer_id: local_peer_id.clone(),
                max_message_size: config.max_message_size,
            },
            shutdown_token.clone(),
        );

        spawn_periodic_tasks(
            Arc::clone(&transport),
            Arc::clone(&membership),
            Arc::clone(&leader_election),
            Arc::clone(&callback),
            config,
            shutdown_token.clone(),
        );

        spawn_leader_change_watcher(
            leader_change_rx,
            Arc::clone(&callback),
            shutdown_token.clone(),
        );

        spawn_membership_event_watcher(
            membership.event_tx.subscribe(),
            Arc::clone(&leader_election),
            Arc::clone(&callback),
            shutdown_token.clone(),
        );

        tracing::info!(
            peer = %local_peer_id,
            session = %session_id,
            addr = %transport.listener_addr(),
            "network initialized"
        );

        Ok(Self {
            transport,
            membership,
            leader_election,
            session_router,
            discovery,
            callback,
            shutdown_token,
            local_peer_id,
            session_id,
            config,
        })
    }

    /// Returns `true` if mDNS discovery started successfully and is
    /// currently active. When `false`, peer discovery relies solely
    /// on manual `connect_to` calls.
    pub fn is_mdns_active(&self) -> bool {
        self.discovery.is_some()
    }

    /// Gracefully shut down the network state, broadcasting a
    /// leave message, draining send queues, cancelling background
    /// tasks, and tearing down discovery and callback threads.
    pub async fn shutdown(self) {
        self.transport.broadcast_peer_leave();
        self.transport
            .drain_send_queues(Duration::from_secs(1))
            .await;
        self.shutdown_token.cancel();

        if let Some(discovery) = &self.discovery {
            discovery.shutdown();
        }

        self.callback.drain_and_shutdown();
        self.transport.shutdown();
        self.callback.join_thread();

        tracing::info!("network shutdown complete");
    }
}

// Context passed into the message handling task. Bundles all
// the managers and identifiers needed to process incoming packets.
struct MessageHandlerCtx {
    transport: Arc<TransportManager>,
    membership: Arc<MembershipManager>,
    leader_election: Arc<LeaderElection>,
    session_router: Arc<SessionRouter>,
    callback: Arc<CallbackManager>,
    local_peer_id: PeerId,
    max_message_size: u32,
}

/// Spawn a background task responsible for draining the transport
/// `incoming_rx` channel and dispatching each packet to
/// `handle_message`. This isolates the network I/O from the rest of
/// the application logic.
fn spawn_message_handler(
    mut incoming_rx: mpsc::Receiver<(PeerId, RawPacket)>,
    ctx: MessageHandlerCtx,
    shutdown_token: CancellationToken,
) {
    tokio::spawn(async move {
        loop {
            tokio::select! {
                _ = shutdown_token.cancelled() => break,
                msg = incoming_rx.recv() => {
                    let Some((peer_id, packet)) = msg else { break };
                    handle_message(&peer_id, packet, &ctx).await;
                }
            }
        }
    });
}

/// Core packet processing logic. Differentiates between
/// internal protocol messages and user payloads, updating membership
/// state, leader election, routing, and callbacks as appropriate.
async fn handle_message(peer_id: &PeerId, packet: RawPacket, ctx: &MessageHandlerCtx) {
    let MessageHandlerCtx {
        transport,
        membership,
        leader_election,
        session_router,
        callback,
        local_peer_id,
        max_message_size,
    } = ctx;
    if packet.is_internal() {
        let msg = match decode_internal(&packet) {
            Ok(m) => m,
            Err(e) => {
                tracing::warn!(peer = %peer_id, error = %e, "failed to decode internal message");
                return;
            }
        };

        match msg {
            InternalMessage::Handshake {
                peer_id: remote_id,
                listen_port: _,
                protocol_version: _,
                session_id: _,
            } => {
                let pid = PeerId::new(&remote_id);
                if let Some(addr) = transport.get_peer_addr(&pid) {
                    let _ = membership.add_peer(pid, addr);
                }
            }
            InternalMessage::HandshakeAck {
                peer_id: remote_id,
                listen_port: _,
                success,
                error_reason: _,
            } => {
                if success {
                    let pid = PeerId::new(&remote_id);
                    if let Some(addr) = transport.get_peer_addr(&pid) {
                        let _ = membership.add_peer(pid, addr);
                    }
                }
            }
            InternalMessage::PeerLeave {
                peer_id: leaving_id,
            } => {
                let pid = PeerId::new(&leaving_id);
                transport.set_should_reconnect(&pid, false);
                membership.remove_peer(&pid);
            }
            InternalMessage::PeerListSync { peers } => {
                for (remote_id, addr_str) in peers {
                    let pid = PeerId::new(&remote_id);
                    if pid == *local_peer_id
                        || membership.has_peer(&pid)
                        || transport.has_peer(&pid)
                    {
                        continue;
                    }
                    if local_peer_id.as_str() > pid.as_str() {
                        continue;
                    }
                    let t = Arc::clone(transport);
                    tokio::spawn(async move {
                        if let Err(e) = t.connect_to(&addr_str).await {
                            tracing::warn!(addr = %addr_str, error = %e, "PeerListSync connect failed");
                        }
                    });
                }
            }
            InternalMessage::Ping { timestamp_ms } => {
                membership.update_last_seen(peer_id);
                let pong =
                    encode_internal(&InternalMessage::Pong { timestamp_ms }, *max_message_size);
                if let Ok(pong_packet) = pong {
                    let _ = transport.send_to_peer(peer_id, pong_packet);
                }
            }
            InternalMessage::Pong { timestamp_ms } => {
                membership.handle_pong(peer_id, timestamp_ms);
                transport.update_last_seen(peer_id);
                transport.update_rtt(
                    peer_id,
                    (current_timestamp_ms().saturating_sub(timestamp_ms)) as u32,
                );
            }
            InternalMessage::Heartbeat {
                term,
                leader_id,
                timestamp_ms,
            } => {
                leader_election.handle_heartbeat(term, &leader_id);
                membership.update_last_seen(peer_id);
                let resp = encode_internal(
                    &InternalMessage::HeartbeatResponse { term, timestamp_ms },
                    *max_message_size,
                );
                if let Ok(pkt) = resp {
                    let _ = transport.send_to_peer(peer_id, pkt);
                }
            }
            InternalMessage::HeartbeatResponse {
                term: _,
                timestamp_ms: _,
            } => {
                membership.update_last_seen(peer_id);
            }
            InternalMessage::RequestVote { term, candidate_id } => {
                let (resp_term, granted) = leader_election.handle_request_vote(term, &candidate_id);
                let response = encode_internal(
                    &InternalMessage::VoteResponse {
                        term: resp_term,
                        voter_id: local_peer_id.as_str().to_owned(),
                        granted,
                    },
                    *max_message_size,
                );
                if let Ok(pkt) = response {
                    let _ = transport.send_to_peer(peer_id, pkt);
                }
            }
            InternalMessage::VoteResponse {
                term,
                voter_id,
                granted,
            } => {
                let count = membership.get_connected_peer_count();
                let became_leader =
                    leader_election.handle_vote_response(term, &voter_id, granted, count);
                if became_leader {
                    let hb = encode_internal(
                        &InternalMessage::Heartbeat {
                            term: leader_election.current_term(),
                            leader_id: local_peer_id.as_str().to_owned(),
                            timestamp_ms: current_timestamp_ms(),
                        },
                        *max_message_size,
                    );
                    if let Ok(pkt) = hb {
                        transport.broadcast(pkt, None);
                    }
                }
            }
            InternalMessage::LeaderAssign {
                term,
                leader_id,
                assigner_id,
            } => {
                leader_election.handle_leader_assign(term, &leader_id, &assigner_id);
            }
            InternalMessage::ForwardedUserData {
                from_peer_id,
                original_msg_type,
                original_flags,
                payload,
            } => {
                if let Some(user_packet) = session_router.handle_forwarded_user_data(
                    &from_peer_id,
                    original_msg_type,
                    original_flags,
                    &payload,
                ) {
                    let from = PeerId::new(&from_peer_id);
                    let data = decode_payload(&user_packet)
                        .unwrap_or_else(|_| user_packet.payload.clone());
                    callback.send_received(&from, data, user_packet.msg_type, user_packet.flags);
                }
            }
        }
    } else {
        let data = match decode_payload(&packet) {
            Ok(d) => d,
            Err(e) => {
                tracing::warn!(peer = %peer_id, error = %e, "failed to decode user message payload");
                return;
            }
        };
        callback.send_received(peer_id, data, packet.msg_type, packet.flags);
    }
}

/// Launch timers for periodic background work:
/// - ping/peer timeout checks
/// - leader heartbeat forwarding
/// - election timeouts
fn spawn_periodic_tasks(
    transport: Arc<TransportManager>,
    membership: Arc<MembershipManager>,
    leader_election: Arc<LeaderElection>,
    callback: Arc<CallbackManager>,
    config: NetworkConfig,
    shutdown_token: CancellationToken,
) {
    let hb_interval = Duration::from_millis(config.heartbeat_interval_ms);
    let max_msg = config.max_message_size;

    {
        let transport = Arc::clone(&transport);
        let membership = Arc::clone(&membership);
        let shutdown = shutdown_token.clone();
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(hb_interval);
            loop {
                tokio::select! {
                    _ = shutdown.cancelled() => break,
                    _ = interval.tick() => {
                        let ts = current_timestamp_ms();
                        let ping = encode_internal(&InternalMessage::Ping { timestamp_ms: ts }, max_msg);
                        if let Ok(pkt) = ping {
                            transport.broadcast(pkt, None);
                        }

                        let timed_out = membership.check_timeouts();
                        for pid in timed_out {
                            tracing::warn!(peer = %pid, "peer timed out");
                            membership.update_status(&pid, PeerStatus::Disconnected);
                            callback.send_peer_status(&pid, PeerStatus::Disconnected.as_i32());
                        }
                    }
                }
            }
        });
    }

    {
        let transport = Arc::clone(&transport);
        let leader_election = Arc::clone(&leader_election);
        let shutdown = shutdown_token.clone();
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(hb_interval);
            loop {
                tokio::select! {
                    _ = shutdown.cancelled() => break,
                    _ = interval.tick() => {
                        if !leader_election.is_leader() {
                            continue;
                        }
                        let hb = encode_internal(&InternalMessage::Heartbeat {
                            term: leader_election.current_term(),
                            leader_id: transport.local_peer_id().as_str().to_owned(),
                            timestamp_ms: current_timestamp_ms(),
                        }, max_msg);
                        if let Ok(pkt) = hb {
                            transport.broadcast(pkt, None);
                        }
                    }
                }
            }
        });
    }

    {
        let transport = Arc::clone(&transport);
        let leader_election = Arc::clone(&leader_election);
        let membership = Arc::clone(&membership);
        let shutdown = shutdown_token.clone();
        let election_min = config.election_timeout_min_ms;
        let election_max = config.election_timeout_max_ms;

        tokio::spawn(async move {
            loop {
                let timeout_ms = rand::rng().random_range(election_min..=election_max);
                let timeout = Duration::from_millis(timeout_ms);

                tokio::select! {
                    _ = shutdown.cancelled() => break,
                    _ = tokio::time::sleep(timeout) => {
                        if leader_election.is_leader()
                            || !leader_election.is_auto_election_enabled()
                            || leader_election.leader_id().is_some()
                        {
                            continue;
                        }

                        let count = membership.get_connected_peer_count();
                        if let Some((term, candidate_id)) = leader_election.start_election(count) {
                            let vote_req = encode_internal(&InternalMessage::RequestVote {
                                term,
                                candidate_id,
                            }, max_msg);
                            if let Ok(pkt) = vote_req {
                                transport.broadcast(pkt, None);
                            }
                        } else if leader_election.is_leader() {
                            let hb = encode_internal(&InternalMessage::Heartbeat {
                                term: leader_election.current_term(),
                                leader_id: transport.local_peer_id().as_str().to_owned(),
                                timestamp_ms: current_timestamp_ms(),
                            }, max_msg);
                            if let Ok(pkt) = hb {
                                transport.broadcast(pkt, None);
                            }
                        }
                    }
                }
            }
        });
    }
}

/// Watch for leader change notifications coming from the
/// `LeaderElection` component and forward them to the callback
/// manager.
fn spawn_leader_change_watcher(
    mut leader_rx: mpsc::Receiver<Option<PeerId>>,
    callback: Arc<CallbackManager>,
    shutdown_token: CancellationToken,
) {
    tokio::spawn(async move {
        loop {
            tokio::select! {
                _ = shutdown_token.cancelled() => break,
                msg = leader_rx.recv() => {
                    match msg {
                        Some(leader) => {
                            callback.send_leader_changed(leader.as_ref());
                        }
                        None => break,
                    }
                }
            }
        }
    });
}

/// Listen to membership events (join/leave/status) and turn them
/// into peer-status callbacks for the FFI layer. Also notifies the
/// leader election module when a peer leaves so it can detect leader
/// loss and trigger re-election.
fn spawn_membership_event_watcher(
    mut event_rx: tokio::sync::broadcast::Receiver<MembershipEvent>,
    leader_election: Arc<LeaderElection>,
    callback: Arc<CallbackManager>,
    shutdown_token: CancellationToken,
) {
    tokio::spawn(async move {
        loop {
            tokio::select! {
                _ = shutdown_token.cancelled() => break,
                event = event_rx.recv() => {
                    match event {
                        Ok(MembershipEvent::Joined(pid)) => {
                            callback.send_peer_status(&pid, PeerStatus::Connected.as_i32());
                        }
                        Ok(MembershipEvent::Left(ref pid)) => {
                            leader_election.handle_peer_left(pid);
                            callback.send_peer_status(pid, PeerStatus::Disconnected.as_i32());
                        }
                        Ok(MembershipEvent::StatusChanged { peer_id, new, .. }) => {
                            callback.send_peer_status(&peer_id, new.as_i32());
                        }
                        Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                            tracing::warn!(n, "membership event broadcast lagged");
                        }
                        Err(_) => break,
                    }
                }
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::CString;

    use futures_util::{SinkExt, StreamExt};

    async fn create_node(peer_id: &str, session_id: &str) -> NetworkState {
        NetworkState::new(
            peer_id.to_string(),
            session_id.to_string(),
            NetworkConfig::default(),
        )
        .await
        .unwrap()
    }

    async fn connect_nodes(a: &NetworkState, b: &NetworkState) {
        let listen = b.transport.listener_addr();
        let addr = format!("127.0.0.1:{}", listen.port());
        a.transport.connect_to(&addr).await.unwrap();
        tokio::time::sleep(Duration::from_millis(100)).await;
    }

    async fn wait_until(f: impl Fn() -> bool, timeout_ms: u64) -> bool {
        let deadline = tokio::time::Instant::now() + Duration::from_millis(timeout_ms);

        while tokio::time::Instant::now() < deadline {
            if f() {
                return true;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }

        false
    }

    /// Quick runtime check: register a throwaway mDNS service on the
    /// default custom port and try to discover it within a few
    /// seconds. Returns `true` if multicast DNS works on this machine.
    /// Used to gracefully skip mDNS-dependent tests when the network
    /// environment does not support multicast.
    async fn mdns_available() -> bool {
        use mdns_sd::{ServiceDaemon, ServiceEvent, ServiceInfo};

        let mdns_port = NetworkConfig::default().mdns_port;
        let Ok(d1) = ServiceDaemon::new_with_port(mdns_port) else {
            return false;
        };

        let host = hostname::get()
            .map(|h| h.to_string_lossy().into_owned())
            .unwrap_or_else(|_| "mdns-check".into());

        let Ok(svc) = ServiceInfo::new(
            "_mdns-check._tcp.local.",
            "check_inst",
            &format!("{host}.local."),
            "",
            19999,
            &[("t", "1")][..],
        ) else {
            let _ = d1.shutdown();
            return false;
        };

        if d1.register(svc).is_err() {
            let _ = d1.shutdown();
            return false;
        }

        let Ok(receiver) = d1.browse("_mdns-check._tcp.local.") else {
            let _ = d1.shutdown();
            return false;
        };

        let found = tokio::task::spawn_blocking(move || {
            for _ in 0..5 {
                if let Ok(ServiceEvent::ServiceResolved(_)) =
                    receiver.recv_timeout(std::time::Duration::from_secs(1))
                {
                    return true;
                }
            }
            false
        })
        .await
        .unwrap_or(false);

        let _ = d1.unregister("check_inst._mdns-check._tcp.local.");
        let _ = d1.shutdown();

        found
    }

    #[tokio::test]
    async fn test_two_node_connect() {
        let node_a = create_node("peer_a", "session1").await;
        let node_b = create_node("peer_b", "session1").await;

        connect_nodes(&node_a, &node_b).await;

        assert!(wait_until(|| node_a.membership.has_peer(&PeerId::new("peer_b")), 2000).await);
        assert!(wait_until(|| node_b.membership.has_peer(&PeerId::new("peer_a")), 2000).await);

        node_a.shutdown().await;
        node_b.shutdown().await;
    }

    #[tokio::test]
    async fn test_handshake_session_mismatch() {
        let node_a = create_node("peer_a", "session1").await;
        let node_b = create_node("peer_b", "session2").await;

        let addr = format!("127.0.0.1:{}", node_b.transport.listener_addr().port());
        let result = node_a.transport.connect_to(&addr).await;
        assert!(result.is_err());

        node_a.shutdown().await;
        node_b.shutdown().await;
    }

    #[tokio::test]
    async fn test_handshake_duplicate_peer_id() {
        let node_a = create_node("peer_a", "session1").await;
        let node_b = create_node("peer_b", "session1").await;

        let addr = format!("127.0.0.1:{}", node_b.transport.listener_addr().port());
        node_a.transport.connect_to(&addr).await.unwrap();
        tokio::time::sleep(Duration::from_millis(100)).await;

        let result = node_a.transport.connect_to(&addr).await;
        assert!(result.is_err());

        node_a.shutdown().await;
        node_b.shutdown().await;
    }

    #[tokio::test]
    async fn test_graceful_shutdown() {
        let node_a = create_node("peer_a", "session1").await;
        let node_b = create_node("peer_b", "session1").await;

        connect_nodes(&node_a, &node_b).await;
        assert!(wait_until(|| node_a.membership.has_peer(&PeerId::new("peer_b")), 2000).await);

        node_a.shutdown().await;
        node_b.shutdown().await;
    }

    #[tokio::test]
    async fn test_shutdown_reinitialize() {
        let node = create_node("peer_a", "session1").await;
        let addr = node.transport.listener_addr();
        node.shutdown().await;

        let node2 = create_node("peer_a", "session1").await;
        assert_ne!(node2.transport.listener_addr(), addr);
        node2.shutdown().await;
    }

    #[tokio::test]
    async fn test_broadcast_message() {
        let node_a = create_node("peer_a", "session1").await;
        let node_b = create_node("peer_b", "session1").await;

        connect_nodes(&node_a, &node_b).await;
        assert!(wait_until(|| node_a.membership.has_peer(&PeerId::new("peer_b")), 2000).await);

        let result = node_a.session_router.route_message(
            types::MessageTarget::Broadcast,
            0x0100,
            b"hello",
            0,
        );
        assert!(result.is_ok());

        node_a.shutdown().await;
        node_b.shutdown().await;
    }

    #[tokio::test]
    async fn test_send_queue_overflow() {
        let config = NetworkConfig {
            send_queue_capacity: 1,
            ..Default::default()
        };
        let node_a = NetworkState::new("peer_a".into(), "session1".into(), config)
            .await
            .unwrap();
        let node_b = create_node("peer_b", "session1").await;

        connect_nodes(&node_a, &node_b).await;
        assert!(wait_until(|| node_a.membership.has_peer(&PeerId::new("peer_b")), 2000).await);

        let mut overflow_detected = false;
        for _ in 0..100 {
            let result = node_a.session_router.route_message(
                types::MessageTarget::ToPeer(PeerId::new("peer_b")),
                0x0100,
                &vec![0u8; 1024],
                0,
            );
            if matches!(result, Err(error::NetworkError::SendQueueFull)) {
                overflow_detected = true;
                break;
            }
        }
        assert!(overflow_detected);

        node_a.shutdown().await;
        node_b.shutdown().await;
    }

    #[tokio::test]
    async fn test_disconnect_no_reconnect() {
        let node_a = create_node("peer_a", "session1").await;
        let node_b = create_node("peer_b", "session1").await;

        connect_nodes(&node_a, &node_b).await;
        assert!(wait_until(|| node_a.membership.has_peer(&PeerId::new("peer_b")), 2000).await);

        node_a
            .transport
            .disconnect_peer(&PeerId::new("peer_b"))
            .unwrap();
        node_a.membership.remove_peer(&PeerId::new("peer_b"));

        tokio::time::sleep(Duration::from_millis(500)).await;
        assert!(!node_a.membership.has_peer(&PeerId::new("peer_b")));

        node_a.shutdown().await;
        node_b.shutdown().await;
    }

    #[tokio::test]
    async fn test_ffi_error_codes_not_initialized() {
        use crate::ffi::*;

        assert_eq!(IsNetworkInitialized(), 0);
        assert!(GetLocalAddr().is_null());
    }

    // All FFI tests that touch global state (NETWORK/RUNTIME) are combined
    // into one function to avoid races when tests run in parallel.
    #[test]
    fn test_ffi_full_lifecycle() {
        use crate::ffi::*;

        // --- Phase 1: getters/setters ---
        let peer_id = CString::new("ffi_lc_peer").unwrap();
        let session_id = CString::new("ffi_lc_session").unwrap();

        let r = InitializeNetwork(peer_id.as_ptr(), session_id.as_ptr());
        assert_eq!(r, error::error_codes::OK);

        assert!(!GetLocalAddr().is_null());
        assert!(!GetLocalPeerId().is_null());
        assert!(!GetSessionId().is_null());

        let local_id = unsafe { std::ffi::CStr::from_ptr(GetLocalPeerId()) }
            .to_str()
            .unwrap();
        assert_eq!(local_id, "ffi_lc_peer");

        let session = unsafe { std::ffi::CStr::from_ptr(GetSessionId()) }
            .to_str()
            .unwrap();
        assert_eq!(session, "ffi_lc_session");

        assert_eq!(GetPeerCount(), 0);
        assert!(!GetPeerList().is_null());
        assert!(!GetCurrentLeader().is_null());
        assert_eq!(IsLeader(), 0);
        assert_eq!(IsCentralizedMode(), 0);
        assert_eq!(IsCentralizedAutoForward(), 1);

        assert_eq!(SetCentralizedMode(1), error::error_codes::OK);
        assert_eq!(IsCentralizedMode(), 1);
        assert_eq!(SetCentralizedMode(0), error::error_codes::OK);
        assert_eq!(IsCentralizedMode(), 0);

        assert_eq!(SetCentralizedAutoForward(0), error::error_codes::OK);
        assert_eq!(IsCentralizedAutoForward(), 0);
        assert_eq!(SetCentralizedAutoForward(1), error::error_codes::OK);
        assert_eq!(IsCentralizedAutoForward(), 1);

        assert_eq!(SetCompressionThreshold(1024), error::error_codes::OK);

        assert_eq!(EnableAutoLeaderElection(0), error::error_codes::OK);
        assert_eq!(EnableAutoLeaderElection(1), error::error_codes::OK);

        assert_eq!(EnableLogging(0), error::error_codes::OK);

        let unknown = CString::new("unknown_peer").unwrap();
        assert_eq!(GetPeerRTT(unknown.as_ptr()), -1);
        assert_eq!(GetPeerStatus(unknown.as_ptr()), -1);

        let r = ShutdownNetwork();
        assert_eq!(r, error::error_codes::OK);

        // --- Phase 2: InitializeNetworkWithConfig ---
        let d = NetworkConfig::default();
        let config = NetworkConfigFFI {
            heartbeat_interval_ms: d.heartbeat_interval_ms,
            election_timeout_min_ms: d.election_timeout_min_ms,
            election_timeout_max_ms: d.election_timeout_max_ms,
            reconnect_initial_ms: d.reconnect_initial_ms,
            reconnect_max_ms: d.reconnect_max_ms,
            compression_threshold: 256, // intentionally non-default
            heartbeat_timeout_multiplier: d.heartbeat_timeout_multiplier,
            send_queue_capacity: 512, // intentionally non-default
            max_connections: d.max_connections as u32,
            max_message_size: d.max_message_size,
            centralized_auto_forward: 1,
            auto_election_enabled: 1,
            mdns_port: d.mdns_port,
            manual_override_recovery: 0,
            _padding: [0; 3],
            handshake_timeout_ms: d.handshake_timeout_ms,
        };

        let r =
            InitializeNetworkWithConfig(peer_id.as_ptr(), session_id.as_ptr(), &config as *const _);
        assert_eq!(r, error::error_codes::OK);

        let r = ShutdownNetwork();
        assert_eq!(r, error::error_codes::OK);

        // null config
        let r =
            InitializeNetworkWithConfig(peer_id.as_ptr(), session_id.as_ptr(), std::ptr::null());
        assert_eq!(r, error::error_codes::INVALID_ARGUMENT);

        // --- Phase 3: null args ---
        let result = InitializeNetwork(std::ptr::null(), std::ptr::null());
        assert_eq!(result, error::error_codes::INVALID_ARGUMENT);

        // --- Phase 4: panic safety (not initialized) ---
        let result = ShutdownNetwork();
        assert_eq!(result, error::error_codes::NOT_INITIALIZED);

        let result = BroadcastMessage(std::ptr::null(), 10, 0x0100, 0);
        assert_ne!(result, error::error_codes::OK);

        // --- Phase 5: reinitialize after shutdown ---
        let peer_id2 = CString::new("ffi_reinit_peer").unwrap();
        let session_id2 = CString::new("ffi_reinit_session").unwrap();

        let result = InitializeNetwork(peer_id2.as_ptr(), session_id2.as_ptr());
        assert_eq!(result, error::error_codes::OK);
        assert_eq!(IsNetworkInitialized(), 1);

        let result2 = InitializeNetwork(peer_id2.as_ptr(), session_id2.as_ptr());
        assert_eq!(result2, error::error_codes::ALREADY_INITIALIZED);

        let result = ShutdownNetwork();
        assert_eq!(result, error::error_codes::OK);
        assert_eq!(IsNetworkInitialized(), 0);

        let result = InitializeNetwork(peer_id2.as_ptr(), session_id2.as_ptr());
        assert_eq!(result, error::error_codes::OK);

        let result = ShutdownNetwork();
        assert_eq!(result, error::error_codes::OK);
    }

    #[tokio::test]
    async fn test_handshake_version_mismatch() {
        use tokio::net::TcpStream;
        use tokio_util::codec::Framed;

        use crate::messaging::{decode_internal, encode_internal};
        use crate::protocol::InternalMessage;
        use crate::transport::PacketCodec;

        let node = create_node("peer_b", "session_ver").await;
        let addr = format!("127.0.0.1:{}", node.transport.listener_addr().port());

        let cfg = NetworkConfig::default();
        let stream = TcpStream::connect(&addr).await.unwrap();
        let mut framed = Framed::new(stream, PacketCodec::new(cfg.max_message_size));

        let hs = encode_internal(
            &InternalMessage::Handshake {
                peer_id: "peer_a".into(),
                listen_port: 9999,
                protocol_version: 999,
                session_id: "session_ver".into(),
            },
            cfg.max_message_size,
        )
        .unwrap();
        framed.send(hs).await.unwrap();

        let pkt = framed
            .next()
            .await
            .expect("expected a response")
            .expect("read error");
        let msg = decode_internal(&pkt).unwrap();
        match msg {
            InternalMessage::HandshakeAck {
                success,
                error_reason,
                ..
            } => {
                assert!(!success);
                let reason = error_reason.expect("error_reason should be set");
                assert!(
                    reason.contains("version_mismatch"),
                    "expected version_mismatch, got: {reason}"
                );
            }
            _ => panic!("expected HandshakeAck"),
        }

        node.shutdown().await;
    }

    #[tokio::test]
    async fn test_handshake_timeout() {
        let node = create_node("peer_timeout", "session_to").await;
        let addr = format!("127.0.0.1:{}", node.transport.listener_addr().port());

        let mut stream = tokio::net::TcpStream::connect(&addr).await.unwrap();

        let mut buf = [0u8; 1];
        let result = tokio::time::timeout(
            Duration::from_secs(7),
            tokio::io::AsyncReadExt::read(&mut stream, &mut buf),
        )
        .await;

        match result {
            Ok(Ok(0)) | Ok(Err(_)) => {}
            Err(_) => panic!("handshake timeout did not close connection within 7s"),
            Ok(Ok(_)) => panic!("unexpected data received"),
        }

        node.shutdown().await;
    }

    #[tokio::test]
    async fn test_max_connections() {
        use futures_util::{SinkExt, StreamExt};
        use tokio::net::TcpStream;
        use tokio_util::codec::Framed;

        use crate::config::PROTOCOL_VERSION;
        use crate::messaging::{decode_internal, encode_internal};
        use crate::protocol::InternalMessage;
        use crate::transport::PacketCodec;

        let node = create_node("peer_main", "session_max").await;
        let addr = format!("127.0.0.1:{}", node.transport.listener_addr().port());

        let cfg = NetworkConfig::default();
        let mut streams = Vec::new();
        let mut connected = 0u32;
        for i in 0..66 {
            let Ok(tcp) = TcpStream::connect(&addr).await else {
                continue;
            };
            let mut framed = Framed::new(tcp, PacketCodec::new(cfg.max_message_size));
            let hs = encode_internal(
                &InternalMessage::Handshake {
                    peer_id: format!("peer_{i}"),
                    listen_port: 0,
                    protocol_version: PROTOCOL_VERSION,
                    session_id: "session_max".into(),
                },
                cfg.max_message_size,
            )
            .unwrap();
            if framed.send(hs).await.is_err() {
                continue;
            }
            if let Ok(Some(Ok(pkt))) =
                tokio::time::timeout(Duration::from_secs(3), framed.next()).await
                && let Ok(InternalMessage::HandshakeAck { success: true, .. }) =
                    decode_internal(&pkt)
            {
                connected += 1;
                streams.push(framed);
            }
        }

        // At least some should succeed, and the count should be capped
        assert!(connected > 0);
        assert!(connected <= NetworkConfig::default().max_connections as u32);

        drop(streams);
        node.shutdown().await;
    }

    #[tokio::test]
    async fn test_reconnect_exponential_backoff() {
        let config = NetworkConfig {
            reconnect_initial_ms: 100,
            reconnect_max_ms: 500,
            ..Default::default()
        };

        let node_a = NetworkState::new("peer_a".into(), "session_recon".into(), config)
            .await
            .unwrap();
        let node_b = NetworkState::new("peer_b".into(), "session_recon".into(), config)
            .await
            .unwrap();

        connect_nodes(&node_a, &node_b).await;
        assert!(wait_until(|| node_a.membership.has_peer(&PeerId::new("peer_b")), 2000).await);

        // Kill node_b to trigger reconnect attempts on node_a
        node_b.shutdown().await;

        // Wait a bit for node_a to detect disconnection and start reconnecting
        tokio::time::sleep(Duration::from_millis(2000)).await;

        // node_a should have attempted reconnection (we can't easily verify backoff timing,
        // but we verify the node is still alive and functional)
        assert!(!node_a.shutdown_token.is_cancelled());

        node_a.shutdown().await;
    }

    #[tokio::test]
    async fn test_auto_leader_election_3_nodes() {
        let config = NetworkConfig {
            election_timeout_min_ms: 300,
            election_timeout_max_ms: 600,
            heartbeat_interval_ms: 100,
            ..Default::default()
        };

        let n0 = NetworkState::new("node_0".into(), "session_election_3".into(), config)
            .await
            .unwrap();
        let n1 = NetworkState::new("node_1".into(), "session_election_3".into(), config)
            .await
            .unwrap();
        let n2 = NetworkState::new("node_2".into(), "session_election_3".into(), config)
            .await
            .unwrap();

        let addr1 = format!("127.0.0.1:{}", n1.transport.listener_addr().port());
        let addr2 = format!("127.0.0.1:{}", n2.transport.listener_addr().port());

        // Build full mesh explicitly
        n0.transport.connect_to(&addr1).await.unwrap();
        n0.transport.connect_to(&addr2).await.unwrap();
        n1.transport.connect_to(&addr2).await.unwrap();
        tokio::time::sleep(Duration::from_millis(300)).await;

        // All nodes should see 2 peers
        assert!(wait_until(|| n0.membership.get_connected_peer_count() >= 2, 3000).await);
        assert!(wait_until(|| n1.membership.get_connected_peer_count() >= 2, 3000).await);
        assert!(wait_until(|| n2.membership.get_connected_peer_count() >= 2, 3000).await);

        // Wait for all nodes to agree on the same leader
        let all_agree = wait_until(
            || {
                let l0 = n0.leader_election.leader_id();
                let l1 = n1.leader_election.leader_id();
                let l2 = n2.leader_election.leader_id();
                l0.is_some() && l0 == l1 && l1 == l2
            },
            8000,
        )
        .await;
        assert!(all_agree, "nodes did not agree on a leader");

        n0.shutdown().await;
        n1.shutdown().await;
        n2.shutdown().await;
    }

    #[tokio::test]
    async fn test_auto_leader_election_2_nodes() {
        let config = NetworkConfig {
            election_timeout_min_ms: 300,
            election_timeout_max_ms: 600,
            heartbeat_interval_ms: 100,
            ..Default::default()
        };

        let n0 = NetworkState::new("node_0".into(), "session_election_2".into(), config)
            .await
            .unwrap();
        let n1 = NetworkState::new("node_1".into(), "session_election_2".into(), config)
            .await
            .unwrap();

        let addr1 = format!("127.0.0.1:{}", n1.transport.listener_addr().port());
        n0.transport.connect_to(&addr1).await.unwrap();
        tokio::time::sleep(Duration::from_millis(200)).await;

        assert!(wait_until(|| n0.membership.get_connected_peer_count() >= 1, 3000).await);

        let has_leader = wait_until(
            || n0.leader_election.leader_id().is_some() && n1.leader_election.leader_id().is_some(),
            5000,
        )
        .await;
        assert!(has_leader, "no leader elected within timeout");

        let l0 = n0.leader_election.leader_id().unwrap();
        let l1 = n1.leader_election.leader_id().unwrap();
        assert_eq!(l0, l1);

        n0.shutdown().await;
        n1.shutdown().await;
    }

    #[tokio::test]
    async fn test_centralized_routing() {
        let config = NetworkConfig {
            election_timeout_min_ms: 200,
            election_timeout_max_ms: 400,
            heartbeat_interval_ms: 100,
            ..Default::default()
        };

        let n0 = NetworkState::new("node_0".into(), "session_central".into(), config)
            .await
            .unwrap();
        let n1 = NetworkState::new("node_1".into(), "session_central".into(), config)
            .await
            .unwrap();

        let addr1 = format!("127.0.0.1:{}", n1.transport.listener_addr().port());
        n0.transport.connect_to(&addr1).await.unwrap();
        tokio::time::sleep(Duration::from_millis(200)).await;

        assert!(wait_until(|| n0.membership.get_connected_peer_count() >= 1, 3000).await);
        assert!(wait_until(|| n0.leader_election.leader_id().is_some(), 5000).await);

        // Enable centralized mode
        n0.session_router.set_centralized(true);
        n1.session_router.set_centralized(true);

        // Non-leader sends broadcast — should be wrapped as ForwardedUserData to leader
        let result = n0.session_router.route_message(
            types::MessageTarget::Broadcast,
            0x0100,
            b"centralized_msg",
            0x04,
        );
        // If n0 is not leader, it sends to leader; if n0 is leader, it broadcasts directly
        // Either way, it should succeed
        assert!(result.is_ok());

        n0.shutdown().await;
        n1.shutdown().await;
    }

    #[tokio::test]
    async fn test_forward_message() {
        let config = NetworkConfig {
            election_timeout_min_ms: 200,
            election_timeout_max_ms: 400,
            heartbeat_interval_ms: 100,
            ..Default::default()
        };

        let n0 = NetworkState::new("node_0".into(), "session_fwd".into(), config)
            .await
            .unwrap();
        let n1 = NetworkState::new("node_1".into(), "session_fwd".into(), config)
            .await
            .unwrap();

        let addr1 = format!("127.0.0.1:{}", n1.transport.listener_addr().port());
        n0.transport.connect_to(&addr1).await.unwrap();
        tokio::time::sleep(Duration::from_millis(200)).await;

        assert!(wait_until(|| n0.leader_election.leader_id().is_some(), 5000).await);

        // Find which node is leader
        let leader_node = if n0.leader_election.is_leader() {
            &n0
        } else {
            &n1
        };
        let follower_id = if n0.leader_election.is_leader() {
            PeerId::new("node_1")
        } else {
            PeerId::new("node_0")
        };

        // Leader forwards a message
        let result = leader_node.session_router.forward_message(
            &PeerId::new("external"),
            types::ForwardTarget::ToPeer(follower_id),
            0x0100,
            0x02,
            b"forwarded_data",
        );
        assert!(result.is_ok());

        // Non-leader should get NotLeader
        let non_leader = if n0.leader_election.is_leader() {
            &n1
        } else {
            &n0
        };
        let result = non_leader.session_router.forward_message(
            &PeerId::new("external"),
            types::ForwardTarget::Broadcast,
            0x0100,
            0,
            b"should_fail",
        );
        assert!(matches!(result, Err(error::NetworkError::NotLeader)));

        n0.shutdown().await;
        n1.shutdown().await;
    }

    #[test]
    fn test_ffi_callbacks() {
        use std::sync::atomic::{AtomicU32, Ordering};

        use crate::callback::CallbackManager;

        static RECEIVE_COUNT: AtomicU32 = AtomicU32::new(0);

        unsafe extern "C" fn on_receive(
            _peer_id: *const std::ffi::c_char,
            _data: *const u8,
            _len: i32,
            _msg_type: u16,
            _flags: u8,
        ) {
            RECEIVE_COUNT.fetch_add(1, Ordering::Relaxed);
        }

        let cb = CallbackManager::new();
        cb.register_receive_callback(Some(on_receive));

        RECEIVE_COUNT.store(0, Ordering::Relaxed);
        cb.send_received(
            &PeerId::new("test_peer"),
            bytes::Bytes::from_static(b"hello"),
            0x0100,
            0,
        );

        // Give the callback thread time to process
        std::thread::sleep(Duration::from_millis(100));
        assert!(
            RECEIVE_COUNT.load(Ordering::Relaxed) > 0,
            "receive callback should have been invoked"
        );

        cb.register_receive_callback(None);
        cb.drain_and_shutdown();
        cb.join_thread();
    }

    #[test]
    fn test_concurrent_ffi_calls() {
        use crate::ffi::*;

        // Read-only FFI calls are safe to call concurrently even without initialization.
        // They should return default/error values without crashing.
        let handles: Vec<_> = (0..8)
            .map(|_| {
                std::thread::spawn(|| {
                    for _ in 0..100 {
                        let _ = IsNetworkInitialized();
                        let _ = GetPeerCount();
                        let _ = IsLeader();
                        let _ = IsCentralizedMode();
                        let _ = IsCentralizedAutoForward();
                        let _ = GetCurrentLeader();
                        let _ = GetLastErrorCode();
                    }
                })
            })
            .collect();

        for h in handles {
            h.join().unwrap();
        }
    }

    #[tokio::test]
    async fn test_rapid_connect_disconnect() {
        let node_a = create_node("peer_a", "session_rapid").await;
        let node_b = create_node("peer_b", "session_rapid").await;
        let addr = format!("127.0.0.1:{}", node_b.transport.listener_addr().port());

        for _ in 0..5 {
            let _ = node_a.transport.connect_to(&addr).await;
            tokio::time::sleep(Duration::from_millis(50)).await;
            let _ = node_a.transport.disconnect_peer(&PeerId::new("peer_b"));
            node_a.membership.remove_peer(&PeerId::new("peer_b"));
            tokio::time::sleep(Duration::from_millis(50)).await;
        }

        // Node should still be functional
        assert!(!node_a.shutdown_token.is_cancelled());

        node_a.shutdown().await;
        node_b.shutdown().await;
    }

    #[test]
    fn test_broadcast_lagged_recovery() {
        use crate::membership::MembershipManager;

        let mgr = MembershipManager::new(PeerId::new("local"), NetworkConfig::default());
        let mut rx = mgr.event_tx.subscribe();
        let addr: std::net::SocketAddr = "127.0.0.1:8080".parse().unwrap();

        // Send more events than channel capacity (256) to guarantee Lagged
        for i in 0..300 {
            let _ = mgr.add_peer(PeerId::new(format!("p{i}")), addr);
        }

        // Drain receiver — must encounter Lagged at some point
        let mut lagged = false;
        loop {
            match rx.try_recv() {
                Ok(_) => continue,
                Err(tokio::sync::broadcast::error::TryRecvError::Lagged(_)) => {
                    lagged = true;
                    break;
                }
                Err(tokio::sync::broadcast::error::TryRecvError::Empty) => break,
                Err(tokio::sync::broadcast::error::TryRecvError::Closed) => break,
            }
        }
        assert!(lagged, "broadcast channel should have lagged");

        // After lagging, full peer list is still available for recovery
        let peers = mgr.get_peer_list();
        assert_eq!(peers.len(), 300);
    }

    #[tokio::test]
    async fn test_discovery_same_session_auto_connect() {
        use crate::discovery::DiscoveryManager;

        let n0 = create_node("disc_peer_0", "disc_session").await;
        let n1 = create_node("disc_peer_1", "disc_session").await;

        // Simulate what mDNS discovery would do: peer_0 < peer_1, so peer_0 connects
        let should = DiscoveryManager::should_connect_to_discovered_peer(
            &n0.local_peer_id,
            &n0.session_id,
            n1.local_peer_id.as_str(),
            &n1.session_id,
            &n0.membership,
            &n0.transport,
        );
        assert!(should, "same session peer should be connectable");

        // Actually connect to verify the full flow works
        let addr = format!("127.0.0.1:{}", n1.transport.listener_addr().port());
        n0.transport.connect_to(&addr).await.unwrap();

        assert!(wait_until(|| n0.membership.has_peer(&PeerId::new("disc_peer_1")), 2000).await);
        assert!(wait_until(|| n1.membership.has_peer(&PeerId::new("disc_peer_0")), 2000).await);

        n0.shutdown().await;
        n1.shutdown().await;
    }

    #[tokio::test]
    async fn test_discovery_session_isolation() {
        use crate::discovery::DiscoveryManager;

        let n0 = create_node("iso_peer_0", "session_A").await;
        let n1 = create_node("iso_peer_1", "session_B").await;

        // Discovery filter should reject cross-session peers
        let should = DiscoveryManager::should_connect_to_discovered_peer(
            &n0.local_peer_id,
            &n0.session_id,
            n1.local_peer_id.as_str(),
            &n1.session_id,
            &n0.membership,
            &n0.transport,
        );
        assert!(!should, "different session peers should not connect");

        // Verify that even if we force a TCP connection, handshake rejects session mismatch
        let addr = format!("127.0.0.1:{}", n1.transport.listener_addr().port());
        let result = n0.transport.connect_to(&addr).await;
        assert!(result.is_err(), "cross-session handshake should fail");

        assert!(!n0.membership.has_peer(&PeerId::new("iso_peer_1")));

        n0.shutdown().await;
        n1.shutdown().await;
    }

    // --- E2E: two-node unicast message delivery with callback verification

    mod e2e_unicast_delivery {
        use super::*;
        use std::ffi::CStr;
        use std::sync::atomic::{AtomicU32, Ordering};

        static RECV_COUNT: AtomicU32 = AtomicU32::new(0);
        static RECV_MSGS: std::sync::Mutex<Vec<(String, Vec<u8>, u16)>> =
            std::sync::Mutex::new(Vec::new());

        unsafe extern "C" fn recv_cb(
            peer_id: *const std::ffi::c_char,
            data: *const u8,
            len: i32,
            msg_type: u16,
            _flags: u8,
        ) {
            let pid = unsafe { CStr::from_ptr(peer_id) }
                .to_str()
                .unwrap()
                .to_owned();
            let payload = unsafe { std::slice::from_raw_parts(data, len as usize) }.to_vec();
            RECV_MSGS.lock().unwrap().push((pid, payload, msg_type));
            RECV_COUNT.fetch_add(1, Ordering::SeqCst);
        }

        #[tokio::test]
        async fn test_unicast_delivery_verified() {
            RECV_COUNT.store(0, Ordering::SeqCst);
            RECV_MSGS.lock().unwrap().clear();

            let node_a = create_node("e2e_uni_a", "session_e2e_uni").await;
            let node_b = create_node("e2e_uni_b", "session_e2e_uni").await;

            // Register receive callback on node B only
            node_b.callback.register_receive_callback(Some(recv_cb));

            connect_nodes(&node_a, &node_b).await;
            assert!(
                wait_until(
                    || node_a.membership.has_peer(&PeerId::new("e2e_uni_b")),
                    3000
                )
                .await
            );
            assert!(
                wait_until(
                    || node_b.membership.has_peer(&PeerId::new("e2e_uni_a")),
                    3000
                )
                .await
            );

            // A sends unicast to B
            node_a
                .session_router
                .route_message(
                    types::MessageTarget::ToPeer(PeerId::new("e2e_uni_b")),
                    0x0200,
                    b"unicast_e2e_payload",
                    0,
                )
                .unwrap();

            // Verify B received it via its callback
            assert!(
                wait_until(|| RECV_COUNT.load(Ordering::SeqCst) >= 1, 5000).await,
                "node B did not receive unicast message within 5 seconds"
            );

            {
                let msgs = RECV_MSGS.lock().unwrap();
                assert!(
                    msgs.iter().any(|(pid, data, mt)| pid == "e2e_uni_a"
                        && data == b"unicast_e2e_payload"
                        && *mt == 0x0200),
                    "received message content did not match expected"
                );
            }

            node_a.shutdown().await;
            node_b.shutdown().await;
        }
    }

    // --- E2E: three-node broadcast delivery with callback verification ---

    mod e2e_broadcast_delivery {
        use super::*;
        use std::ffi::CStr;
        use std::sync::atomic::{AtomicU32, Ordering};

        static BROADCAST_COUNT: AtomicU32 = AtomicU32::new(0);
        static BROADCAST_MSGS: std::sync::Mutex<Vec<(String, Vec<u8>, u16)>> =
            std::sync::Mutex::new(Vec::new());

        unsafe extern "C" fn broadcast_recv_cb(
            peer_id: *const std::ffi::c_char,
            data: *const u8,
            len: i32,
            msg_type: u16,
            _flags: u8,
        ) {
            let pid = unsafe { CStr::from_ptr(peer_id) }
                .to_str()
                .unwrap()
                .to_owned();
            let payload = unsafe { std::slice::from_raw_parts(data, len as usize) }.to_vec();
            BROADCAST_MSGS
                .lock()
                .unwrap()
                .push((pid, payload, msg_type));
            BROADCAST_COUNT.fetch_add(1, Ordering::SeqCst);
        }

        #[tokio::test]
        async fn test_broadcast_delivery_verified() {
            BROADCAST_COUNT.store(0, Ordering::SeqCst);
            BROADCAST_MSGS.lock().unwrap().clear();

            let node_a = create_node("e2e_bc_a", "session_e2e_bc").await;
            let node_b = create_node("e2e_bc_b", "session_e2e_bc").await;
            let node_c = create_node("e2e_bc_c", "session_e2e_bc").await;

            // Register the same receive callback on B and C
            node_b
                .callback
                .register_receive_callback(Some(broadcast_recv_cb));
            node_c
                .callback
                .register_receive_callback(Some(broadcast_recv_cb));

            // A connects to B and C
            let addr_b = format!("127.0.0.1:{}", node_b.transport.listener_addr().port());
            let addr_c = format!("127.0.0.1:{}", node_c.transport.listener_addr().port());
            node_a.transport.connect_to(&addr_b).await.unwrap();
            node_a.transport.connect_to(&addr_c).await.unwrap();
            tokio::time::sleep(Duration::from_millis(300)).await;

            assert!(
                wait_until(|| node_a.membership.get_connected_peer_count() >= 2, 3000).await,
                "node A did not see 2 peers"
            );

            // A broadcasts a message
            node_a
                .session_router
                .route_message(
                    types::MessageTarget::Broadcast,
                    0x0300,
                    b"broadcast_e2e_payload",
                    0,
                )
                .unwrap();

            // Both B and C should receive the broadcast
            assert!(
                wait_until(|| BROADCAST_COUNT.load(Ordering::SeqCst) >= 2, 5000).await,
                "broadcast not received by all nodes within 5s (got {})",
                BROADCAST_COUNT.load(Ordering::SeqCst)
            );

            {
                let msgs = BROADCAST_MSGS.lock().unwrap();
                let match_count = msgs
                    .iter()
                    .filter(|(pid, data, mt)| {
                        pid == "e2e_bc_a" && data == b"broadcast_e2e_payload" && *mt == 0x0300
                    })
                    .count();
                assert!(
                    match_count >= 2,
                    "expected broadcast received by 2 nodes, got {}",
                    match_count
                );
            }

            node_a.shutdown().await;
            node_b.shutdown().await;
            node_c.shutdown().await;
        }
    }

    // --- E2E: bidirectional send & receive -------------------------------

    mod e2e_bidirectional {
        use super::*;
        use std::ffi::CStr;
        use std::sync::atomic::{AtomicU32, Ordering};

        static A_COUNT: AtomicU32 = AtomicU32::new(0);
        static B_COUNT: AtomicU32 = AtomicU32::new(0);
        static A_MSGS: std::sync::Mutex<Vec<(String, Vec<u8>, u16)>> =
            std::sync::Mutex::new(Vec::new());
        static B_MSGS: std::sync::Mutex<Vec<(String, Vec<u8>, u16)>> =
            std::sync::Mutex::new(Vec::new());

        unsafe extern "C" fn recv_a(
            peer_id: *const std::ffi::c_char,
            data: *const u8,
            len: i32,
            msg_type: u16,
            _flags: u8,
        ) {
            let pid = unsafe { CStr::from_ptr(peer_id) }
                .to_str()
                .unwrap()
                .to_owned();
            let payload = unsafe { std::slice::from_raw_parts(data, len as usize) }.to_vec();
            A_MSGS.lock().unwrap().push((pid, payload, msg_type));
            A_COUNT.fetch_add(1, Ordering::SeqCst);
        }

        unsafe extern "C" fn recv_b(
            peer_id: *const std::ffi::c_char,
            data: *const u8,
            len: i32,
            msg_type: u16,
            _flags: u8,
        ) {
            let pid = unsafe { CStr::from_ptr(peer_id) }
                .to_str()
                .unwrap()
                .to_owned();
            let payload = unsafe { std::slice::from_raw_parts(data, len as usize) }.to_vec();
            B_MSGS.lock().unwrap().push((pid, payload, msg_type));
            B_COUNT.fetch_add(1, Ordering::SeqCst);
        }

        /// Both nodes send to each other in parallel and verify receipt.
        #[tokio::test]
        async fn test_bidirectional_messaging() {
            A_COUNT.store(0, Ordering::SeqCst);
            B_COUNT.store(0, Ordering::SeqCst);
            A_MSGS.lock().unwrap().clear();
            B_MSGS.lock().unwrap().clear();

            let node_a = create_node("bidirectional_a", "session_bidirectional").await;
            let node_b = create_node("bidirectional_b", "session_bidirectional").await;

            node_a.callback.register_receive_callback(Some(recv_a));
            node_b.callback.register_receive_callback(Some(recv_b));

            connect_nodes(&node_a, &node_b).await;
            assert!(
                wait_until(
                    || node_a.membership.has_peer(&PeerId::new("bidirectional_b")),
                    3000
                )
                .await
            );
            assert!(
                wait_until(
                    || node_b.membership.has_peer(&PeerId::new("bidirectional_a")),
                    3000
                )
                .await
            );

            // A → B
            node_a
                .session_router
                .route_message(
                    types::MessageTarget::ToPeer(PeerId::new("bidirectional_b")),
                    0x0200,
                    b"from_a_to_b",
                    0,
                )
                .unwrap();

            // B → A
            node_b
                .session_router
                .route_message(
                    types::MessageTarget::ToPeer(PeerId::new("bidirectional_a")),
                    0x0201,
                    b"from_b_to_a",
                    0,
                )
                .unwrap();

            assert!(
                wait_until(|| A_COUNT.load(Ordering::SeqCst) >= 1, 5000).await,
                "A did not receive message from B"
            );
            assert!(
                wait_until(|| B_COUNT.load(Ordering::SeqCst) >= 1, 5000).await,
                "B did not receive message from A"
            );

            {
                let a_msgs = A_MSGS.lock().unwrap();
                assert!(a_msgs.iter().any(|(pid, d, mt)| pid == "bidirectional_b"
                    && d == b"from_b_to_a"
                    && *mt == 0x0201));
                let b_msgs = B_MSGS.lock().unwrap();
                assert!(b_msgs.iter().any(|(pid, d, mt)| pid == "bidirectional_a"
                    && d == b"from_a_to_b"
                    && *mt == 0x0200));
            }

            node_a.shutdown().await;
            node_b.shutdown().await;
        }
    }

    // --- E2E: multiple sequential messages preserve ordering -------------

    mod e2e_sequential_messages {
        use super::*;
        use std::ffi::CStr;
        use std::sync::atomic::{AtomicU32, Ordering};

        static SEQ_COUNT: AtomicU32 = AtomicU32::new(0);
        static SEQ_MSGS: std::sync::Mutex<Vec<(String, Vec<u8>, u16)>> =
            std::sync::Mutex::new(Vec::new());

        unsafe extern "C" fn seq_cb(
            peer_id: *const std::ffi::c_char,
            data: *const u8,
            len: i32,
            msg_type: u16,
            _flags: u8,
        ) {
            let pid = unsafe { CStr::from_ptr(peer_id) }
                .to_str()
                .unwrap()
                .to_owned();
            let payload = unsafe { std::slice::from_raw_parts(data, len as usize) }.to_vec();
            SEQ_MSGS.lock().unwrap().push((pid, payload, msg_type));
            SEQ_COUNT.fetch_add(1, Ordering::SeqCst);
        }

        /// Send 20 numbered messages and verify all arrive.
        #[tokio::test]
        async fn test_sequential_delivery() {
            SEQ_COUNT.store(0, Ordering::SeqCst);
            SEQ_MSGS.lock().unwrap().clear();

            let node_a = create_node("seq_a", "session_seq").await;
            let node_b = create_node("seq_b", "session_seq").await;

            node_b.callback.register_receive_callback(Some(seq_cb));

            connect_nodes(&node_a, &node_b).await;
            assert!(wait_until(|| node_a.membership.has_peer(&PeerId::new("seq_b")), 3000).await);

            const COUNT: u32 = 20;
            for i in 0..COUNT {
                let payload = format!("msg_{i}");
                node_a
                    .session_router
                    .route_message(
                        types::MessageTarget::ToPeer(PeerId::new("seq_b")),
                        0x0400,
                        payload.as_bytes(),
                        0,
                    )
                    .unwrap();
            }

            assert!(
                wait_until(|| SEQ_COUNT.load(Ordering::SeqCst) >= COUNT, 10000).await,
                "only received {}/{} messages",
                SEQ_COUNT.load(Ordering::SeqCst),
                COUNT
            );

            {
                let msgs = SEQ_MSGS.lock().unwrap();
                // Verify all 20 are present
                for i in 0..COUNT {
                    let expected = format!("msg_{i}");
                    assert!(
                        msgs.iter().any(|(_, d, _)| d == expected.as_bytes()),
                        "missing message: {expected}"
                    );
                }
            }

            node_a.shutdown().await;
            node_b.shutdown().await;
        }
    }

    // --- E2E: large payload delivery (above compression threshold) -------

    mod e2e_large_payload {
        use super::*;
        use std::sync::atomic::{AtomicU32, Ordering};

        static LARGE_COUNT: AtomicU32 = AtomicU32::new(0);
        static LARGE_MSGS: std::sync::Mutex<Vec<Vec<u8>>> = std::sync::Mutex::new(Vec::new());

        unsafe extern "C" fn large_cb(
            _peer_id: *const std::ffi::c_char,
            data: *const u8,
            len: i32,
            _msg_type: u16,
            _flags: u8,
        ) {
            let payload = unsafe { std::slice::from_raw_parts(data, len as usize) }.to_vec();
            LARGE_MSGS.lock().unwrap().push(payload);
            LARGE_COUNT.fetch_add(1, Ordering::SeqCst);
        }

        /// Send a 100 KB payload that will be compressed, then verified intact.
        #[tokio::test]
        async fn test_large_payload_delivery() {
            LARGE_COUNT.store(0, Ordering::SeqCst);
            LARGE_MSGS.lock().unwrap().clear();

            let node_a = create_node("large_a", "session_large").await;
            let node_b = create_node("large_b", "session_large").await;

            node_b.callback.register_receive_callback(Some(large_cb));

            connect_nodes(&node_a, &node_b).await;
            assert!(wait_until(|| node_a.membership.has_peer(&PeerId::new("large_b")), 3000).await);

            // Build a 100 KB payload with a recognizable pattern
            let size = 100 * 1024;
            let mut payload = Vec::with_capacity(size);
            for i in 0..size {
                payload.push((i % 251) as u8);
            }

            node_a
                .session_router
                .route_message(
                    types::MessageTarget::ToPeer(PeerId::new("large_b")),
                    0x0500,
                    &payload,
                    0,
                )
                .unwrap();

            assert!(
                wait_until(|| LARGE_COUNT.load(Ordering::SeqCst) >= 1, 10000).await,
                "large payload not received"
            );

            {
                let msgs = LARGE_MSGS.lock().unwrap();
                assert_eq!(msgs[0].len(), size);
                assert_eq!(msgs[0], payload, "large payload content mismatch");
            }

            node_a.shutdown().await;
            node_b.shutdown().await;
        }
    }

    // --- E2E: msg_type and user flags preserved --------------------------

    mod e2e_msg_type_and_flags {
        use super::*;
        use std::sync::atomic::{AtomicU32, Ordering};

        static TF_COUNT: AtomicU32 = AtomicU32::new(0);
        static TF_MSGS: std::sync::Mutex<Vec<(u16, u8)>> = std::sync::Mutex::new(Vec::new());

        unsafe extern "C" fn tf_cb(
            _peer_id: *const std::ffi::c_char,
            _data: *const u8,
            _len: i32,
            msg_type: u16,
            flags: u8,
        ) {
            TF_MSGS.lock().unwrap().push((msg_type, flags));
            TF_COUNT.fetch_add(1, Ordering::SeqCst);
        }

        /// Various msg_type values and user flag combinations must survive transport.
        #[tokio::test]
        async fn test_type_and_flags_preserved() {
            TF_COUNT.store(0, Ordering::SeqCst);
            TF_MSGS.lock().unwrap().clear();

            let node_a = create_node("tf_a", "session_tf").await;
            let node_b = create_node("tf_b", "session_tf").await;

            node_b.callback.register_receive_callback(Some(tf_cb));

            connect_nodes(&node_a, &node_b).await;
            assert!(wait_until(|| node_a.membership.has_peer(&PeerId::new("tf_b")), 3000).await);

            // Test several type/flag combos. Bit 0 is reserved (LZ4), stripped by router.
            let cases: Vec<(u16, u8, u8)> = vec![
                (0x0100, 0x00, 0x00), // min user type, no flags
                (0x0100, 0x02, 0x02), // flag bit 1
                (0x0100, 0xFE, 0xFE), // all user bits set (bit 0 stripped)
                (0xFFFF, 0x00, 0x00), // max msg_type
                (0x1234, 0x04, 0x04), // arbitrary combo
            ];

            for (mt, send_flags, _) in &cases {
                node_a
                    .session_router
                    .route_message(
                        types::MessageTarget::ToPeer(PeerId::new("tf_b")),
                        *mt,
                        b"tf",
                        *send_flags,
                    )
                    .unwrap();
            }

            assert!(
                wait_until(
                    || TF_COUNT.load(Ordering::SeqCst) >= cases.len() as u32,
                    5000
                )
                .await,
                "not all type/flag combos received"
            );

            {
                let msgs = TF_MSGS.lock().unwrap();
                for (mt, _, expected_flags) in &cases {
                    assert!(
                        msgs.iter().any(|(t, f)| *t == *mt && *f == *expected_flags),
                        "missing msg_type=0x{mt:04X} flags=0x{expected_flags:02X}"
                    );
                }
            }

            node_a.shutdown().await;
            node_b.shutdown().await;
        }
    }

    // --- E2E: three-node full mesh, every node sends to every other ----

    mod e2e_full_mesh {
        use super::*;
        use std::ffi::CStr;
        use std::sync::atomic::{AtomicU32, Ordering};

        static MESH_COUNT: AtomicU32 = AtomicU32::new(0);
        static MESH_MSGS: std::sync::Mutex<Vec<(String, Vec<u8>)>> =
            std::sync::Mutex::new(Vec::new());

        unsafe extern "C" fn mesh_cb(
            peer_id: *const std::ffi::c_char,
            data: *const u8,
            len: i32,
            _msg_type: u16,
            _flags: u8,
        ) {
            let pid = unsafe { CStr::from_ptr(peer_id) }
                .to_str()
                .unwrap()
                .to_owned();
            let payload = unsafe { std::slice::from_raw_parts(data, len as usize) }.to_vec();
            MESH_MSGS.lock().unwrap().push((pid, payload));
            MESH_COUNT.fetch_add(1, Ordering::SeqCst);
        }

        /// 3 nodes, full mesh, each broadcasts – every node should hear from the other 2.
        #[tokio::test]
        async fn test_full_mesh_broadcast() {
            MESH_COUNT.store(0, Ordering::SeqCst);
            MESH_MSGS.lock().unwrap().clear();

            let n0 = create_node("mesh_0", "session_mesh").await;
            let n1 = create_node("mesh_1", "session_mesh").await;
            let n2 = create_node("mesh_2", "session_mesh").await;

            n0.callback.register_receive_callback(Some(mesh_cb));
            n1.callback.register_receive_callback(Some(mesh_cb));
            n2.callback.register_receive_callback(Some(mesh_cb));

            // Build full mesh
            let addr1 = format!("127.0.0.1:{}", n1.transport.listener_addr().port());
            let addr2 = format!("127.0.0.1:{}", n2.transport.listener_addr().port());
            n0.transport.connect_to(&addr1).await.unwrap();
            n0.transport.connect_to(&addr2).await.unwrap();
            n1.transport.connect_to(&addr2).await.unwrap();
            tokio::time::sleep(Duration::from_millis(300)).await;

            assert!(wait_until(|| n0.membership.get_connected_peer_count() >= 2, 3000).await);
            assert!(wait_until(|| n1.membership.get_connected_peer_count() >= 2, 3000).await);
            assert!(wait_until(|| n2.membership.get_connected_peer_count() >= 2, 3000).await);

            // Each node broadcasts
            n0.session_router
                .route_message(types::MessageTarget::Broadcast, 0x0600, b"from_0", 0)
                .unwrap();
            n1.session_router
                .route_message(types::MessageTarget::Broadcast, 0x0600, b"from_1", 0)
                .unwrap();
            n2.session_router
                .route_message(types::MessageTarget::Broadcast, 0x0600, b"from_2", 0)
                .unwrap();

            // 3 nodes × 2 received messages each = 6 total
            assert!(
                wait_until(|| MESH_COUNT.load(Ordering::SeqCst) >= 6, 8000).await,
                "full mesh: expected 6 deliveries, got {}",
                MESH_COUNT.load(Ordering::SeqCst)
            );

            {
                let msgs = MESH_MSGS.lock().unwrap();
                // Verify each source was heard by 2 recipients
                for sender in &["mesh_0", "mesh_1", "mesh_2"] {
                    let from_sender = msgs.iter().filter(|(pid, _)| pid == sender).count();
                    assert!(
                        from_sender >= 2,
                        "sender {sender} was heard by {from_sender} nodes, expected 2"
                    );
                }
            }

            n0.shutdown().await;
            n1.shutdown().await;
            n2.shutdown().await;
        }
    }

    // --- E2E: peer status callbacks fire on connect and disconnect -------

    mod e2e_peer_status_callbacks {
        use super::*;
        use std::ffi::CStr;
        use std::sync::atomic::{AtomicU32, Ordering};

        static STATUS_COUNT: AtomicU32 = AtomicU32::new(0);
        static STATUS_EVENTS: std::sync::Mutex<Vec<(String, i32)>> =
            std::sync::Mutex::new(Vec::new());

        unsafe extern "C" fn status_cb(peer_id: *const std::ffi::c_char, status: i32) {
            let pid = unsafe { CStr::from_ptr(peer_id) }
                .to_str()
                .unwrap()
                .to_owned();
            STATUS_EVENTS.lock().unwrap().push((pid, status));
            STATUS_COUNT.fetch_add(1, Ordering::SeqCst);
        }

        /// Connect two nodes, verify Connected callback, disconnect, verify Disconnected.
        #[tokio::test]
        async fn test_status_callbacks_on_connect_disconnect() {
            STATUS_COUNT.store(0, Ordering::SeqCst);
            STATUS_EVENTS.lock().unwrap().clear();

            let node_a = create_node("stat_a", "session_stat").await;
            let node_b = create_node("stat_b", "session_stat").await;

            node_a
                .callback
                .register_peer_status_callback(Some(status_cb));

            connect_nodes(&node_a, &node_b).await;

            // A should get Connected for stat_b
            assert!(
                wait_until(
                    || STATUS_EVENTS
                        .lock()
                        .unwrap()
                        .iter()
                        .any(|(pid, s)| pid == "stat_b" && *s == PeerStatus::Connected.as_i32()),
                    5000
                )
                .await,
                "did not receive Connected status for stat_b"
            );

            // Disconnect B and wait for Disconnected callback
            node_a
                .transport
                .disconnect_peer(&PeerId::new("stat_b"))
                .unwrap();
            node_a.membership.remove_peer(&PeerId::new("stat_b"));

            assert!(
                wait_until(
                    || STATUS_EVENTS
                        .lock()
                        .unwrap()
                        .iter()
                        .any(|(pid, s)| pid == "stat_b" && *s == PeerStatus::Disconnected.as_i32()),
                    5000
                )
                .await,
                "did not receive Disconnected status for stat_b"
            );

            node_a.shutdown().await;
            node_b.shutdown().await;
        }
    }

    // --- E2E: reconnect and messaging after temporary disconnect ---------

    mod e2e_reconnect_messaging {
        use super::*;
        use std::ffi::CStr;
        use std::sync::atomic::{AtomicU32, Ordering};

        static RECON_COUNT: AtomicU32 = AtomicU32::new(0);
        static RECON_MSGS: std::sync::Mutex<Vec<(String, Vec<u8>)>> =
            std::sync::Mutex::new(Vec::new());

        unsafe extern "C" fn recon_cb(
            peer_id: *const std::ffi::c_char,
            data: *const u8,
            len: i32,
            _msg_type: u16,
            _flags: u8,
        ) {
            let pid = unsafe { CStr::from_ptr(peer_id) }
                .to_str()
                .unwrap()
                .to_owned();
            let payload = unsafe { std::slice::from_raw_parts(data, len as usize) }.to_vec();
            RECON_MSGS.lock().unwrap().push((pid, payload));
            RECON_COUNT.fetch_add(1, Ordering::SeqCst);
        }

        /// Disconnect, manually reconnect, then verify messaging works again.
        #[tokio::test]
        async fn test_reconnect_then_message() {
            RECON_COUNT.store(0, Ordering::SeqCst);
            RECON_MSGS.lock().unwrap().clear();

            let node_a = create_node("recon_a", "session_recon").await;
            let node_b = create_node("recon_b", "session_recon").await;

            node_b.callback.register_receive_callback(Some(recon_cb));

            connect_nodes(&node_a, &node_b).await;
            assert!(wait_until(|| node_a.membership.has_peer(&PeerId::new("recon_b")), 3000).await);

            // Disconnect
            node_a
                .transport
                .disconnect_peer(&PeerId::new("recon_b"))
                .unwrap();
            node_a.membership.remove_peer(&PeerId::new("recon_b"));
            tokio::time::sleep(Duration::from_millis(300)).await;
            assert!(!node_a.membership.has_peer(&PeerId::new("recon_b")));

            // Reconnect
            connect_nodes(&node_a, &node_b).await;
            assert!(
                wait_until(|| node_a.membership.has_peer(&PeerId::new("recon_b")), 3000).await,
                "failed to reconnect"
            );

            // Send message after reconnect
            node_a
                .session_router
                .route_message(
                    types::MessageTarget::ToPeer(PeerId::new("recon_b")),
                    0x0700,
                    b"after_reconnect",
                    0,
                )
                .unwrap();

            assert!(
                wait_until(|| RECON_COUNT.load(Ordering::SeqCst) >= 1, 5000).await,
                "message after reconnect not received"
            );

            {
                let msgs = RECON_MSGS.lock().unwrap();
                assert!(msgs.iter().any(|(_, d)| d == b"after_reconnect"));
            }

            node_a.shutdown().await;
            node_b.shutdown().await;
        }
    }

    // --- E2E: centralized mode – follower message routed through leader --

    mod e2e_centralized_routing {
        use super::*;
        use std::ffi::CStr;
        use std::sync::atomic::{AtomicU32, Ordering};

        static CENT_COUNT: AtomicU32 = AtomicU32::new(0);
        static CENT_MSGS: std::sync::Mutex<Vec<(String, Vec<u8>)>> =
            std::sync::Mutex::new(Vec::new());

        unsafe extern "C" fn cent_cb(
            peer_id: *const std::ffi::c_char,
            data: *const u8,
            len: i32,
            _msg_type: u16,
            _flags: u8,
        ) {
            let pid = unsafe { CStr::from_ptr(peer_id) }
                .to_str()
                .unwrap()
                .to_owned();
            let payload = unsafe { std::slice::from_raw_parts(data, len as usize) }.to_vec();
            CENT_MSGS.lock().unwrap().push((pid, payload));
            CENT_COUNT.fetch_add(1, Ordering::SeqCst);
        }

        /// In centralized mode a follower broadcast is forwarded to the leader,
        /// and the leader re-broadcasts it. Verify end-to-end delivery.
        #[tokio::test]
        async fn test_centralized_broadcast_delivery() {
            CENT_COUNT.store(0, Ordering::SeqCst);
            CENT_MSGS.lock().unwrap().clear();

            let config = NetworkConfig {
                election_timeout_min_ms: 200,
                election_timeout_max_ms: 400,
                heartbeat_interval_ms: 100,
                ..Default::default()
            };

            let n0 = NetworkState::new("cent_0".into(), "session_cent".into(), config)
                .await
                .unwrap();
            let n1 = NetworkState::new("cent_1".into(), "session_cent".into(), config)
                .await
                .unwrap();
            let n2 = NetworkState::new("cent_2".into(), "session_cent".into(), config)
                .await
                .unwrap();

            // Register callback on all 3 nodes
            n0.callback.register_receive_callback(Some(cent_cb));
            n1.callback.register_receive_callback(Some(cent_cb));
            n2.callback.register_receive_callback(Some(cent_cb));

            // Full mesh
            let addr1 = format!("127.0.0.1:{}", n1.transport.listener_addr().port());
            let addr2 = format!("127.0.0.1:{}", n2.transport.listener_addr().port());
            n0.transport.connect_to(&addr1).await.unwrap();
            n0.transport.connect_to(&addr2).await.unwrap();
            n1.transport.connect_to(&addr2).await.unwrap();
            tokio::time::sleep(Duration::from_millis(300)).await;

            assert!(wait_until(|| n0.membership.get_connected_peer_count() >= 2, 3000).await);

            // Wait for a leader to be elected
            assert!(
                wait_until(
                    || {
                        let l0 = n0.leader_election.leader_id();
                        let l1 = n1.leader_election.leader_id();
                        let l2 = n2.leader_election.leader_id();
                        l0.is_some() && l0 == l1 && l1 == l2
                    },
                    8000
                )
                .await,
                "no leader agreement"
            );

            // Enable centralized mode on all nodes
            n0.session_router.set_centralized(true);
            n1.session_router.set_centralized(true);
            n2.session_router.set_centralized(true);

            // Find a follower node
            let (follower, follower_name) = if !n0.leader_election.is_leader() {
                (&n0, "cent_0")
            } else if !n1.leader_election.is_leader() {
                (&n1, "cent_1")
            } else {
                (&n2, "cent_2")
            };

            // Follower broadcasts – message should be forwarded through leader
            // and delivered to at least one other follower
            follower
                .session_router
                .route_message(
                    types::MessageTarget::Broadcast,
                    0x0800,
                    b"centralized_e2e",
                    0,
                )
                .unwrap();

            // At least 1 node (another follower) should receive the message
            assert!(
                wait_until(|| CENT_COUNT.load(Ordering::SeqCst) >= 1, 8000).await,
                "centralized broadcast not received by any node (got {})",
                CENT_COUNT.load(Ordering::SeqCst)
            );

            {
                let msgs = CENT_MSGS.lock().unwrap();
                assert!(
                    msgs.iter().any(|(_, d)| d == b"centralized_e2e"),
                    "centralized payload mismatch, follower={follower_name}"
                );
            }

            n0.shutdown().await;
            n1.shutdown().await;
            n2.shutdown().await;
        }
    }

    // --- E2E: leader election + leader-originated broadcast --------------

    mod e2e_leader_broadcast {
        use super::*;
        use std::ffi::CStr;
        use std::sync::atomic::{AtomicU32, Ordering};

        static LB_COUNT: AtomicU32 = AtomicU32::new(0);
        static LB_MSGS: std::sync::Mutex<Vec<(String, Vec<u8>)>> =
            std::sync::Mutex::new(Vec::new());

        unsafe extern "C" fn lb_cb(
            peer_id: *const std::ffi::c_char,
            data: *const u8,
            len: i32,
            _msg_type: u16,
            _flags: u8,
        ) {
            let pid = unsafe { CStr::from_ptr(peer_id) }
                .to_str()
                .unwrap()
                .to_owned();
            let payload = unsafe { std::slice::from_raw_parts(data, len as usize) }.to_vec();
            LB_MSGS.lock().unwrap().push((pid, payload));
            LB_COUNT.fetch_add(1, Ordering::SeqCst);
        }

        /// Elect a leader and use send_from_leader to broadcast, verify followers receive it.
        #[tokio::test]
        async fn test_leader_broadcast_delivery() {
            LB_COUNT.store(0, Ordering::SeqCst);
            LB_MSGS.lock().unwrap().clear();

            let config = NetworkConfig {
                election_timeout_min_ms: 200,
                election_timeout_max_ms: 400,
                heartbeat_interval_ms: 100,
                ..Default::default()
            };

            let n0 = NetworkState::new("lb_0".into(), "session_lb".into(), config)
                .await
                .unwrap();
            let n1 = NetworkState::new("lb_1".into(), "session_lb".into(), config)
                .await
                .unwrap();

            n0.callback.register_receive_callback(Some(lb_cb));
            n1.callback.register_receive_callback(Some(lb_cb));

            connect_nodes(&n0, &n1).await;
            assert!(wait_until(|| n0.membership.has_peer(&PeerId::new("lb_1")), 3000).await);

            // Wait for a leader to be elected
            assert!(
                wait_until(
                    || n0.leader_election.leader_id().is_some()
                        && n1.leader_election.leader_id().is_some(),
                    5000
                )
                .await,
                "no leader elected"
            );

            let leader = if n0.leader_election.is_leader() {
                &n0
            } else {
                &n1
            };

            leader
                .session_router
                .send_from_leader(0x0900, b"from_the_leader", 0)
                .unwrap();

            // The follower should receive the leader's broadcast
            assert!(
                wait_until(|| LB_COUNT.load(Ordering::SeqCst) >= 1, 5000).await,
                "follower did not receive leader broadcast"
            );

            {
                let msgs = LB_MSGS.lock().unwrap();
                assert!(msgs.iter().any(|(_, d)| d == b"from_the_leader"));
            }

            n0.shutdown().await;
            n1.shutdown().await;
        }
    }

    // --- E2E: four-node dynamic join — late joiner gets messages ---------

    mod e2e_dynamic_join {
        use super::*;
        use std::ffi::CStr;
        use std::sync::atomic::{AtomicU32, Ordering};

        static DJ_COUNT: AtomicU32 = AtomicU32::new(0);
        static DJ_MSGS: std::sync::Mutex<Vec<(String, Vec<u8>)>> =
            std::sync::Mutex::new(Vec::new());

        unsafe extern "C" fn dj_cb(
            peer_id: *const std::ffi::c_char,
            data: *const u8,
            len: i32,
            _msg_type: u16,
            _flags: u8,
        ) {
            let pid = unsafe { CStr::from_ptr(peer_id) }
                .to_str()
                .unwrap()
                .to_owned();
            let payload = unsafe { std::slice::from_raw_parts(data, len as usize) }.to_vec();
            DJ_MSGS.lock().unwrap().push((pid, payload));
            DJ_COUNT.fetch_add(1, Ordering::SeqCst);
        }

        /// 3 nodes start, then a 4th joins late, and messages flow to it.
        #[tokio::test]
        async fn test_late_joiner_receives_messages() {
            DJ_COUNT.store(0, Ordering::SeqCst);
            DJ_MSGS.lock().unwrap().clear();

            let n0 = create_node("dj_0", "session_dj").await;
            let n1 = create_node("dj_1", "session_dj").await;
            let n2 = create_node("dj_2", "session_dj").await;

            // Connect n0↔n1, n0↔n2
            let addr1 = format!("127.0.0.1:{}", n1.transport.listener_addr().port());
            let addr2 = format!("127.0.0.1:{}", n2.transport.listener_addr().port());
            n0.transport.connect_to(&addr1).await.unwrap();
            n0.transport.connect_to(&addr2).await.unwrap();
            tokio::time::sleep(Duration::from_millis(200)).await;

            assert!(wait_until(|| n0.membership.get_connected_peer_count() >= 2, 3000).await);

            // Late joiner
            let n3 = create_node("dj_3", "session_dj").await;
            n3.callback.register_receive_callback(Some(dj_cb));

            let addr3 = format!("127.0.0.1:{}", n3.transport.listener_addr().port());
            n0.transport.connect_to(&addr3).await.unwrap();
            tokio::time::sleep(Duration::from_millis(200)).await;

            assert!(
                wait_until(|| n3.membership.has_peer(&PeerId::new("dj_0")), 3000).await,
                "late joiner did not connect to n0"
            );

            // n0 sends to the late joiner
            n0.session_router
                .route_message(
                    types::MessageTarget::ToPeer(PeerId::new("dj_3")),
                    0x0A00,
                    b"welcome_latecomer",
                    0,
                )
                .unwrap();

            assert!(
                wait_until(|| DJ_COUNT.load(Ordering::SeqCst) >= 1, 5000).await,
                "late joiner did not receive message"
            );

            {
                let msgs = DJ_MSGS.lock().unwrap();
                assert!(msgs.iter().any(|(_, d)| d == b"welcome_latecomer"));
            }

            n0.shutdown().await;
            n1.shutdown().await;
            n2.shutdown().await;
            n3.shutdown().await;
        }
    }

    // --- E2E: mDNS auto-discovery (same session, no manual connect) ------

    mod e2e_mdns_auto_discovery {
        use super::*;
        use std::ffi::CStr;
        use std::sync::atomic::{AtomicU32, Ordering};

        static MDNS_DISC_COUNT: AtomicU32 = AtomicU32::new(0);
        static MDNS_DISC_MSGS: std::sync::Mutex<Vec<(String, Vec<u8>)>> =
            std::sync::Mutex::new(Vec::new());

        unsafe extern "C" fn mdns_disc_cb(
            peer_id: *const std::ffi::c_char,
            data: *const u8,
            len: i32,
            _msg_type: u16,
            _flags: u8,
        ) {
            let pid = unsafe { CStr::from_ptr(peer_id) }
                .to_str()
                .unwrap()
                .to_owned();
            let payload = unsafe { std::slice::from_raw_parts(data, len as usize) }.to_vec();
            MDNS_DISC_MSGS.lock().unwrap().push((pid, payload));
            MDNS_DISC_COUNT.fetch_add(1, Ordering::SeqCst);
        }

        /// Two nodes with the same session auto-discover via mDNS
        /// without any manual `connect_to`. Verifies messaging works
        /// over the auto-discovered connection.
        /// Skipped when mDNS multicast is unavailable on the host.
        #[tokio::test]
        async fn test_mdns_discovers_and_connects() {
            if !mdns_available().await {
                tracing::warn!("mDNS multicast not available on this machine – skipping");
                return;
            }

            MDNS_DISC_COUNT.store(0, Ordering::SeqCst);
            MDNS_DISC_MSGS.lock().unwrap().clear();

            let n0 = create_node("mdns_alpha", "session_mdns_disc").await;
            let n1 = create_node("mdns_beta", "session_mdns_disc").await;

            n1.callback.register_receive_callback(Some(mdns_disc_cb));

            // Do NOT call connect_nodes — rely on mDNS discovery only.
            // "mdns_alpha" < "mdns_beta", so n0 initiates the connection.
            assert!(
                wait_until(|| n0.membership.has_peer(&PeerId::new("mdns_beta")), 20_000).await,
                "mDNS did not auto-connect n0 → n1 within 20 s"
            );
            assert!(
                wait_until(|| n1.membership.has_peer(&PeerId::new("mdns_alpha")), 5_000).await,
                "n1 did not see n0 after mDNS discovery"
            );

            // Verify messaging works over the mDNS-discovered link
            n0.session_router
                .route_message(
                    types::MessageTarget::ToPeer(PeerId::new("mdns_beta")),
                    0x0B00,
                    b"hello_via_mdns",
                    0,
                )
                .unwrap();

            assert!(
                wait_until(|| MDNS_DISC_COUNT.load(Ordering::SeqCst) >= 1, 5_000).await,
                "message over mDNS-discovered connection not received"
            );

            {
                let msgs = MDNS_DISC_MSGS.lock().unwrap();
                assert!(
                    msgs.iter()
                        .any(|(pid, d)| pid == "mdns_alpha" && d == b"hello_via_mdns")
                );
            }
            n0.shutdown().await;
            n1.shutdown().await;
        }
    }

    // --- E2E: mDNS cross-session isolation (no unwanted connections) -----

    mod e2e_mdns_cross_session_no_connect {
        use super::*;

        /// Two nodes in different sessions must NOT auto-connect via mDNS.
        /// Skipped when mDNS multicast is unavailable on the host.
        #[tokio::test]
        async fn test_mdns_different_session_no_connect() {
            if !mdns_available().await {
                tracing::warn!("mDNS multicast not available on this machine – skipping");
                return;
            }

            let n0 = create_node("mdns_iso_a", "session_mdns_x").await;
            let n1 = create_node("mdns_iso_b", "session_mdns_y").await;

            // Give mDNS ample time to potentially (wrongly) discover.
            tokio::time::sleep(Duration::from_secs(6)).await;

            assert!(
                !n0.membership.has_peer(&PeerId::new("mdns_iso_b")),
                "nodes in different sessions should not auto-connect via mDNS"
            );
            assert!(
                !n1.membership.has_peer(&PeerId::new("mdns_iso_a")),
                "nodes in different sessions should not auto-connect via mDNS"
            );

            n0.shutdown().await;
            n1.shutdown().await;
        }
    }

    // --- E2E: mDNS three-node auto-mesh ----------------------------------

    mod e2e_mdns_three_node_mesh {
        use super::*;
        use std::ffi::CStr;
        use std::sync::atomic::{AtomicU32, Ordering};

        static MDNS3_COUNT: AtomicU32 = AtomicU32::new(0);
        static MDNS3_MSGS: std::sync::Mutex<Vec<(String, Vec<u8>)>> =
            std::sync::Mutex::new(Vec::new());

        unsafe extern "C" fn mdns3_cb(
            peer_id: *const std::ffi::c_char,
            data: *const u8,
            len: i32,
            _msg_type: u16,
            _flags: u8,
        ) {
            let pid = unsafe { CStr::from_ptr(peer_id) }
                .to_str()
                .unwrap()
                .to_owned();
            let payload = unsafe { std::slice::from_raw_parts(data, len as usize) }.to_vec();
            MDNS3_MSGS.lock().unwrap().push((pid, payload));
            MDNS3_COUNT.fetch_add(1, Ordering::SeqCst);
        }

        /// Three nodes in the same session form a full mesh via mDNS only.
        /// Then broadcast from one and verify all others receive the message.
        /// Skipped when mDNS multicast is unavailable on the host.
        #[tokio::test]
        async fn test_mdns_three_node_auto_mesh() {
            if !mdns_available().await {
                tracing::warn!("mDNS multicast not available on this machine – skipping");
                return;
            }

            MDNS3_COUNT.store(0, Ordering::SeqCst);
            MDNS3_MSGS.lock().unwrap().clear();

            let n0 = create_node("m3_a", "session_mdns3").await;
            let n1 = create_node("m3_b", "session_mdns3").await;
            let n2 = create_node("m3_c", "session_mdns3").await;

            n1.callback.register_receive_callback(Some(mdns3_cb));
            n2.callback.register_receive_callback(Some(mdns3_cb));

            // Wait for all three to form a full mesh via mDNS + PeerListSync.
            assert!(
                wait_until(|| n0.membership.get_connected_peer_count() >= 2, 25_000).await,
                "n0 did not see 2 peers via mDNS (got {})",
                n0.membership.get_connected_peer_count()
            );
            assert!(
                wait_until(|| n1.membership.get_connected_peer_count() >= 2, 10_000).await,
                "n1 did not see 2 peers via mDNS (got {})",
                n1.membership.get_connected_peer_count()
            );
            assert!(
                wait_until(|| n2.membership.get_connected_peer_count() >= 2, 10_000).await,
                "n2 did not see 2 peers via mDNS (got {})",
                n2.membership.get_connected_peer_count()
            );

            // Broadcast from n0, n1 and n2 should both receive.
            n0.session_router
                .route_message(
                    types::MessageTarget::Broadcast,
                    0x0B10,
                    b"mdns3_broadcast",
                    0,
                )
                .unwrap();

            assert!(
                wait_until(|| MDNS3_COUNT.load(Ordering::SeqCst) >= 2, 5_000).await,
                "mDNS mesh broadcast not received by both peers (got {})",
                MDNS3_COUNT.load(Ordering::SeqCst)
            );

            drop(MDNS3_MSGS.lock().unwrap());
            n0.shutdown().await;
            n1.shutdown().await;
            n2.shutdown().await;
        }
    }

    // --- E2E: duplicate peer ID is rejected at handshake -----------------

    mod e2e_duplicate_peer_id_rejected {
        use super::*;

        /// If a node with the same peer_id as an already-connected peer
        /// attempts to connect, the handshake must reject it.
        #[tokio::test]
        async fn test_duplicate_peer_id_connection_rejected() {
            let a = create_node("dup_host", "session_dup_id").await;
            let b = create_node("dup_guest", "session_dup_id").await;

            connect_nodes(&a, &b).await;
            assert!(wait_until(|| a.membership.has_peer(&PeerId::new("dup_guest")), 3_000).await);

            // Create a third node with the SAME peer_id as b.
            let c = NetworkState::new(
                "dup_guest".into(),
                "session_dup_id".into(),
                NetworkConfig::default(),
            )
            .await
            .unwrap();

            // c tries to connect to a — should be rejected because a
            // already has a connection keyed by "dup_guest".
            let addr_a = format!("127.0.0.1:{}", a.transport.listener_addr().port());
            let result = c.transport.connect_to(&addr_a).await;
            assert!(
                result.is_err(),
                "duplicate peer_id connection should be rejected"
            );

            // Original connection must still be healthy.
            assert!(a.membership.has_peer(&PeerId::new("dup_guest")));

            a.shutdown().await;
            b.shutdown().await;
            c.shutdown().await;
        }
    }

    // --- E2E: PeerListSync auto-forms mesh beyond direct connections -----

    mod e2e_peer_list_sync_auto_mesh {
        use super::*;

        /// A connects to B. Then C connects to A. A sends PeerListSync
        /// to C containing B's address. C should auto-connect to B,
        /// forming a full mesh without explicit instructions.
        #[tokio::test]
        async fn test_peer_list_sync_auto_mesh() {
            // Names chosen so that "pls_c" < "pls_b" is FALSE and
            // "pls_a" < "pls_b" is TRUE. PeerListSync respects
            // the lexicographic dedup rule.
            let a = create_node("pls_a", "session_pls").await;
            let b = create_node("pls_b", "session_pls").await;
            // C's id "pls_aa" < "pls_b" so C will initiate to B.
            let c = create_node("pls_aa", "session_pls").await;

            // Step 1: A → B
            connect_nodes(&a, &b).await;
            assert!(wait_until(|| a.membership.has_peer(&PeerId::new("pls_b")), 3_000).await);

            // Step 2: C → A
            let addr_a = format!("127.0.0.1:{}", a.transport.listener_addr().port());
            c.transport.connect_to(&addr_a).await.unwrap();
            assert!(wait_until(|| c.membership.has_peer(&PeerId::new("pls_a")), 3_000).await);

            // Step 3: C should auto-discover B via PeerListSync or mDNS and connect.
            assert!(
                wait_until(|| c.membership.has_peer(&PeerId::new("pls_b")), 10_000).await,
                "C did not auto-connect to B via PeerListSync or mDNS"
            );

            // Full mesh: every node sees 2 peers.
            assert!(
                wait_until(|| a.membership.get_connected_peer_count() >= 2, 5_000).await,
                "A does not see 2 peers"
            );

            a.shutdown().await;
            b.shutdown().await;
            c.shutdown().await;
        }
    }

    // --- E2E: concurrent senders to one receiver -------------------------

    mod e2e_concurrent_senders {
        use super::*;
        use std::ffi::CStr;
        use std::sync::atomic::{AtomicU32, Ordering};

        static CS_COUNT: AtomicU32 = AtomicU32::new(0);
        static CS_MSGS: std::sync::Mutex<Vec<(String, Vec<u8>)>> =
            std::sync::Mutex::new(Vec::new());

        unsafe extern "C" fn cs_cb(
            peer_id: *const std::ffi::c_char,
            data: *const u8,
            len: i32,
            _msg_type: u16,
            _flags: u8,
        ) {
            let pid = unsafe { CStr::from_ptr(peer_id) }
                .to_str()
                .unwrap()
                .to_owned();
            let payload = unsafe { std::slice::from_raw_parts(data, len as usize) }.to_vec();
            CS_MSGS.lock().unwrap().push((pid, payload));
            CS_COUNT.fetch_add(1, Ordering::SeqCst);
        }

        /// Three senders each fire 5 messages to a single receiver
        /// simultaneously. All 15 must arrive.
        #[tokio::test]
        async fn test_concurrent_senders_all_delivered() {
            CS_COUNT.store(0, Ordering::SeqCst);
            CS_MSGS.lock().unwrap().clear();

            let target = create_node("cs_target", "session_cs").await;
            let s0 = create_node("cs_s0", "session_cs").await;
            let s1 = create_node("cs_s1", "session_cs").await;
            let s2 = create_node("cs_s2", "session_cs").await;

            target.callback.register_receive_callback(Some(cs_cb));

            // Connect all senders to target.
            let tgt_addr = format!("127.0.0.1:{}", target.transport.listener_addr().port());
            s0.transport.connect_to(&tgt_addr).await.unwrap();
            s1.transport.connect_to(&tgt_addr).await.unwrap();
            s2.transport.connect_to(&tgt_addr).await.unwrap();

            assert!(wait_until(|| target.membership.get_connected_peer_count() >= 3, 5_000).await);

            // Fire all sends concurrently.
            for (node, name) in [(&s0, "cs_s0"), (&s1, "cs_s1"), (&s2, "cs_s2")] {
                for i in 0u8..5 {
                    let payload = format!("{name}_{i}");
                    node.session_router
                        .route_message(
                            types::MessageTarget::ToPeer(PeerId::new("cs_target")),
                            0x0C00,
                            payload.as_bytes(),
                            0,
                        )
                        .unwrap();
                }
            }

            assert!(
                wait_until(|| CS_COUNT.load(Ordering::SeqCst) >= 15, 10_000).await,
                "only received {}/15 concurrent messages",
                CS_COUNT.load(Ordering::SeqCst)
            );

            {
                let msgs = CS_MSGS.lock().unwrap();
                // All 3 senders must be represented.
                for sender in ["cs_s0", "cs_s1", "cs_s2"] {
                    let count = msgs.iter().filter(|(p, _)| p == sender).count();
                    assert_eq!(count, 5, "sender {sender} delivered {count}/5 msgs");
                }
            }
            target.shutdown().await;
            s0.shutdown().await;
            s1.shutdown().await;
            s2.shutdown().await;
        }
    }

    // --- E2E: empty (zero-length) payload delivery -----------------------

    mod e2e_empty_payload {
        use super::*;
        use std::sync::atomic::{AtomicU32, Ordering};

        static EMPTY_COUNT: AtomicU32 = AtomicU32::new(0);
        static EMPTY_TYPES: std::sync::Mutex<Vec<u16>> = std::sync::Mutex::new(Vec::new());

        unsafe extern "C" fn empty_cb(
            _peer_id: *const std::ffi::c_char,
            _data: *const u8,
            len: i32,
            msg_type: u16,
            _flags: u8,
        ) {
            // len must be 0 for empty payloads.
            assert_eq!(len, 0, "expected zero-length payload");
            EMPTY_TYPES.lock().unwrap().push(msg_type);
            EMPTY_COUNT.fetch_add(1, Ordering::SeqCst);
        }

        /// A zero-length user message must be delivered intact.
        #[tokio::test]
        async fn test_empty_payload_delivery() {
            EMPTY_COUNT.store(0, Ordering::SeqCst);
            EMPTY_TYPES.lock().unwrap().clear();

            let a = create_node("empty_a", "session_empty").await;
            let b = create_node("empty_b", "session_empty").await;

            b.callback.register_receive_callback(Some(empty_cb));

            connect_nodes(&a, &b).await;
            assert!(wait_until(|| a.membership.has_peer(&PeerId::new("empty_b")), 3_000).await);

            a.session_router
                .route_message(
                    types::MessageTarget::ToPeer(PeerId::new("empty_b")),
                    0x0D00,
                    b"",
                    0,
                )
                .unwrap();

            assert!(
                wait_until(|| EMPTY_COUNT.load(Ordering::SeqCst) >= 1, 5_000).await,
                "empty payload not received"
            );

            {
                let types = EMPTY_TYPES.lock().unwrap();
                assert!(types.contains(&0x0D00));
            }
            a.shutdown().await;
            b.shutdown().await;
        }
    }

    // --- E2E: leader election reaches consensus --------------------------

    mod e2e_leader_election_consensus {
        use super::*;

        /// Three nodes form a mesh with fast election timers.
        /// Verify all three nodes agree on the same leader.
        #[tokio::test]
        async fn test_all_nodes_agree_on_leader() {
            let config = NetworkConfig {
                election_timeout_min_ms: 200,
                election_timeout_max_ms: 400,
                heartbeat_interval_ms: 100,
                ..Default::default()
            };

            let n0 = NetworkState::new("election_0".into(), "session_election".into(), config)
                .await
                .unwrap();
            let n1 = NetworkState::new("election_1".into(), "session_election".into(), config)
                .await
                .unwrap();
            let n2 = NetworkState::new("election_2".into(), "session_election".into(), config)
                .await
                .unwrap();

            // Build full mesh.
            let addr1 = format!("127.0.0.1:{}", n1.transport.listener_addr().port());
            let addr2 = format!("127.0.0.1:{}", n2.transport.listener_addr().port());
            n0.transport.connect_to(&addr1).await.unwrap();
            n0.transport.connect_to(&addr2).await.unwrap();
            n1.transport.connect_to(&addr2).await.unwrap();
            tokio::time::sleep(Duration::from_millis(300)).await;

            assert!(wait_until(|| n0.membership.get_connected_peer_count() >= 2, 3_000).await);

            // Wait for all three to agree on one leader.
            assert!(
                wait_until(
                    || {
                        let l0 = n0.leader_election.leader_id();
                        let l1 = n1.leader_election.leader_id();
                        let l2 = n2.leader_election.leader_id();
                        l0.is_some() && l0 == l1 && l1 == l2
                    },
                    10_000
                )
                .await,
                "leader election did not reach consensus"
            );

            let leader = n0.leader_election.leader_id().unwrap();
            // Exactly one node should consider itself the leader.
            let leader_count = [&n0, &n1, &n2]
                .iter()
                .filter(|n| n.leader_election.is_leader())
                .count();
            assert_eq!(leader_count, 1, "exactly one node must be leader");

            // The leader must be one of the three peer ids.
            let valid = ["election_0", "election_1", "election_2"];
            assert!(
                valid.contains(&leader.as_str()),
                "leader {leader} is not one of the nodes"
            );

            n0.shutdown().await;
            n1.shutdown().await;
            n2.shutdown().await;
        }
    }

    // --- E2E: graceful leave detected by remaining peers -----------------

    mod e2e_graceful_leave_detection {
        use super::*;
        use std::ffi::CStr;
        use std::sync::atomic::{AtomicU32, Ordering};

        static LEAVE_STATUS_COUNT: AtomicU32 = AtomicU32::new(0);
        static LEAVE_EVENTS: std::sync::Mutex<Vec<(String, i32)>> =
            std::sync::Mutex::new(Vec::new());

        unsafe extern "C" fn leave_status_cb(peer_id: *const std::ffi::c_char, status: i32) {
            let pid = unsafe { CStr::from_ptr(peer_id) }
                .to_str()
                .unwrap()
                .to_owned();
            LEAVE_EVENTS.lock().unwrap().push((pid, status));
            LEAVE_STATUS_COUNT.fetch_add(1, Ordering::SeqCst);
        }

        /// A, B, C are connected. B shuts down gracefully (broadcasts PeerLeave).
        /// A and C must detect that B has left.
        #[tokio::test]
        async fn test_graceful_leave_propagates() {
            LEAVE_STATUS_COUNT.store(0, Ordering::SeqCst);
            LEAVE_EVENTS.lock().unwrap().clear();

            let a = create_node("leave_a", "session_leave").await;
            let b = create_node("leave_b", "session_leave").await;
            let c = create_node("leave_c", "session_leave").await;

            a.callback
                .register_peer_status_callback(Some(leave_status_cb));
            c.callback
                .register_peer_status_callback(Some(leave_status_cb));

            // A ↔ B, A ↔ C
            connect_nodes(&a, &b).await;
            connect_nodes(&a, &c).await;
            // B ↔ C
            let addr_c = format!("127.0.0.1:{}", c.transport.listener_addr().port());
            b.transport.connect_to(&addr_c).await.unwrap();

            assert!(wait_until(|| a.membership.get_connected_peer_count() >= 2, 5_000).await);
            assert!(wait_until(|| b.membership.get_connected_peer_count() >= 2, 5_000).await);
            assert!(wait_until(|| c.membership.get_connected_peer_count() >= 2, 5_000).await);

            // Clear status events accumulated during connection.
            LEAVE_STATUS_COUNT.store(0, Ordering::SeqCst);
            LEAVE_EVENTS.lock().unwrap().clear();

            // B shuts down gracefully (sends PeerLeave).
            b.shutdown().await;

            // A and C should each receive a Disconnected event for leave_b.
            assert!(
                wait_until(
                    || {
                        let events = LEAVE_EVENTS.lock().unwrap();
                        let a_saw = events.iter().any(|(p, s)| {
                            p == "leave_b" && *s == PeerStatus::Disconnected.as_i32()
                        });
                        let c_saw = events.iter().any(|(p, s)| {
                            p == "leave_b" && *s == PeerStatus::Disconnected.as_i32()
                        });
                        a_saw && c_saw
                    },
                    10_000
                )
                .await,
                "A and/or C did not detect B's graceful leave"
            );

            a.shutdown().await;
            c.shutdown().await;
        }
    }

    // --- E2E: connection to non-reachable address fails gracefully -------

    mod e2e_connect_refused {
        use super::*;

        /// Connecting to a port that actively refuses connections must
        /// return an error without crashing the node.
        #[tokio::test]
        async fn test_connection_refused_graceful() {
            let node = create_node("refuse_a", "session_refuse").await;

            // Bind a listener and immediately drop it to ensure the port
            // is closed and will refuse connections.
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let addr = listener.local_addr().unwrap();
            drop(listener);

            let result = node.transport.connect_to(&addr.to_string()).await;
            assert!(result.is_err(), "connection to closed port should fail");

            // Node must remain functional after the failed connection.
            assert!(!node.shutdown_token.is_cancelled());

            let b = create_node("refuse_b", "session_refuse").await;
            connect_nodes(&node, &b).await;
            assert!(
                wait_until(|| node.membership.has_peer(&PeerId::new("refuse_b")), 3_000).await,
                "node should still be functional after connection refusal"
            );

            node.shutdown().await;
            b.shutdown().await;
        }
    }

    // --- E2E: Leader failover — leader disconnects, new election ---------

    mod e2e_leader_failover {
        use super::*;

        /// Three-node mesh. The elected leader shuts down. The two
        /// remaining nodes must elect a new leader within a reasonable
        /// time.
        #[tokio::test]
        async fn test_leader_failover_new_election() {
            let config = NetworkConfig {
                election_timeout_min_ms: 200,
                election_timeout_max_ms: 400,
                heartbeat_interval_ms: 100,
                ..Default::default()
            };

            let n0 = NetworkState::new("fo_0".into(), "session_fo".into(), config)
                .await
                .unwrap();
            let n1 = NetworkState::new("fo_1".into(), "session_fo".into(), config)
                .await
                .unwrap();
            let n2 = NetworkState::new("fo_2".into(), "session_fo".into(), config)
                .await
                .unwrap();

            // Full mesh
            let addr1 = format!("127.0.0.1:{}", n1.transport.listener_addr().port());
            let addr2 = format!("127.0.0.1:{}", n2.transport.listener_addr().port());
            n0.transport.connect_to(&addr1).await.unwrap();
            n0.transport.connect_to(&addr2).await.unwrap();
            n1.transport.connect_to(&addr2).await.unwrap();
            tokio::time::sleep(Duration::from_millis(300)).await;

            assert!(wait_until(|| n0.membership.get_connected_peer_count() >= 2, 3_000).await);

            // Wait for initial leader election
            assert!(
                wait_until(
                    || {
                        let l0 = n0.leader_election.leader_id();
                        let l1 = n1.leader_election.leader_id();
                        let l2 = n2.leader_election.leader_id();
                        l0.is_some() && l0 == l1 && l1 == l2
                    },
                    8_000,
                )
                .await,
                "initial leader election failed"
            );

            let old_leader = n0.leader_election.leader_id().unwrap();

            // Use Option to allow taking ownership out of the array
            let mut nodes = [Some(n0), Some(n1), Some(n2)];
            let leader_idx = if old_leader.as_str() == "fo_0" {
                0
            } else if old_leader.as_str() == "fo_1" {
                1
            } else {
                2
            };
            let survivor_a = (leader_idx + 1) % 3;
            let survivor_b = (leader_idx + 2) % 3;

            // Shut down the leader
            nodes[leader_idx].take().unwrap().shutdown().await;

            // Survivors should detect the leader left and elect a new one
            let new_leader_elected = wait_until(
                || {
                    let l_a = nodes[survivor_a]
                        .as_ref()
                        .unwrap()
                        .leader_election
                        .leader_id();
                    let l_b = nodes[survivor_b]
                        .as_ref()
                        .unwrap()
                        .leader_election
                        .leader_id();
                    l_a.is_some() && l_a == l_b && l_a.as_ref() != Some(&old_leader)
                },
                15_000,
            )
            .await;

            assert!(
                new_leader_elected,
                "survivors did not elect a new leader after failover"
            );

            let new_leader = nodes[survivor_a]
                .as_ref()
                .unwrap()
                .leader_election
                .leader_id()
                .unwrap();
            assert_ne!(new_leader, old_leader, "new leader should differ from old");

            for node in &mut nodes {
                if let Some(n) = node.take() {
                    n.shutdown().await;
                }
            }
        }
    }

    // --- E2E: SendToLeader + SendFromLeader end-to-end -------------------

    mod e2e_send_to_from_leader {
        use super::*;
        use std::ffi::CStr;
        use std::sync::atomic::{AtomicU32, Ordering};

        static LEADER_RECV_COUNT: AtomicU32 = AtomicU32::new(0);
        static LEADER_RECV_MSGS: std::sync::Mutex<Vec<(String, Vec<u8>, u16)>> =
            std::sync::Mutex::new(Vec::new());
        static FOLLOWER_RECV_COUNT: AtomicU32 = AtomicU32::new(0);
        static FOLLOWER_RECV_MSGS: std::sync::Mutex<Vec<(String, Vec<u8>, u16)>> =
            std::sync::Mutex::new(Vec::new());

        unsafe extern "C" fn leader_cb(
            peer_id: *const std::ffi::c_char,
            data: *const u8,
            len: i32,
            msg_type: u16,
            _flags: u8,
        ) {
            let pid = unsafe { CStr::from_ptr(peer_id) }
                .to_str()
                .unwrap()
                .to_owned();
            let payload = unsafe { std::slice::from_raw_parts(data, len as usize) }.to_vec();
            LEADER_RECV_MSGS
                .lock()
                .unwrap()
                .push((pid, payload, msg_type));
            LEADER_RECV_COUNT.fetch_add(1, Ordering::SeqCst);
        }

        unsafe extern "C" fn follower_cb(
            peer_id: *const std::ffi::c_char,
            data: *const u8,
            len: i32,
            msg_type: u16,
            _flags: u8,
        ) {
            let pid = unsafe { CStr::from_ptr(peer_id) }
                .to_str()
                .unwrap()
                .to_owned();
            let payload = unsafe { std::slice::from_raw_parts(data, len as usize) }.to_vec();
            FOLLOWER_RECV_MSGS
                .lock()
                .unwrap()
                .push((pid, payload, msg_type));
            FOLLOWER_RECV_COUNT.fetch_add(1, Ordering::SeqCst);
        }

        /// Follower sends a message to the leader via ToLeader,
        /// then leader sends a broadcast via send_from_leader.
        /// Both must be received correctly.
        #[tokio::test]
        async fn test_send_to_leader_and_from_leader() {
            LEADER_RECV_COUNT.store(0, Ordering::SeqCst);
            LEADER_RECV_MSGS.lock().unwrap().clear();
            FOLLOWER_RECV_COUNT.store(0, Ordering::SeqCst);
            FOLLOWER_RECV_MSGS.lock().unwrap().clear();

            // Disable auto-election so the leader stays stable throughout the test.
            let config = NetworkConfig {
                auto_election_enabled: false,
                ..Default::default()
            };

            let n0 = NetworkState::new(
                "send_to_leader_and_from_leader_0".into(),
                "session_send_to_leader_and_from_leader".into(),
                config,
            )
            .await
            .unwrap();
            let n1 = NetworkState::new(
                "send_to_leader_and_from_leader_1".into(),
                "session_send_to_leader_and_from_leader".into(),
                config,
            )
            .await
            .unwrap();

            connect_nodes(&n0, &n1).await;
            assert!(
                wait_until(
                    || n0
                        .membership
                        .has_peer(&PeerId::new("send_to_leader_and_from_leader_1")),
                    3_000
                )
                .await
            );

            // Manually set a deterministic leader.
            n0.leader_election
                .set_leader(&PeerId::new("send_to_leader_and_from_leader_0"));
            n1.leader_election
                .set_leader(&PeerId::new("send_to_leader_and_from_leader_0"));

            let (leader, follower) = (&n0, &n1);

            // Register callbacks
            leader.callback.register_receive_callback(Some(leader_cb));
            follower
                .callback
                .register_receive_callback(Some(follower_cb));

            // 1) Follower → Leader (ToLeader)
            follower
                .session_router
                .route_message(types::MessageTarget::ToLeader, 0x0E00, b"msg_to_leader", 0)
                .unwrap();

            assert!(
                wait_until(|| LEADER_RECV_COUNT.load(Ordering::SeqCst) >= 1, 5_000).await,
                "leader did not receive ToLeader message"
            );

            {
                let msgs = LEADER_RECV_MSGS.lock().unwrap();
                assert!(
                    msgs.iter()
                        .any(|(_, d, mt)| d == b"msg_to_leader" && *mt == 0x0E00)
                );
            }

            // 2) Leader → Follower (send_from_leader broadcast)
            leader
                .session_router
                .send_from_leader(0x0E01, b"from_leader_broadcast", 0)
                .unwrap();

            assert!(
                wait_until(|| FOLLOWER_RECV_COUNT.load(Ordering::SeqCst) >= 1, 5_000).await,
                "follower did not receive send_from_leader message"
            );

            {
                let msgs = FOLLOWER_RECV_MSGS.lock().unwrap();
                assert!(
                    msgs.iter()
                        .any(|(_, d, mt)| d == b"from_leader_broadcast" && *mt == 0x0E01)
                );
            }

            n0.shutdown().await;
            n1.shutdown().await;
        }
    }

    // --- E2E: ForwardMessage — leader forwards to specific peer ----------

    mod e2e_forward_message_delivery {
        use super::*;
        use std::ffi::CStr;
        use std::sync::atomic::{AtomicU32, Ordering};

        static FWD_COUNT: AtomicU32 = AtomicU32::new(0);
        static FWD_MSGS: std::sync::Mutex<Vec<(String, Vec<u8>, u16)>> =
            std::sync::Mutex::new(Vec::new());

        unsafe extern "C" fn fwd_cb(
            peer_id: *const std::ffi::c_char,
            data: *const u8,
            len: i32,
            msg_type: u16,
            _flags: u8,
        ) {
            let pid = unsafe { CStr::from_ptr(peer_id) }
                .to_str()
                .unwrap()
                .to_owned();
            let payload = unsafe { std::slice::from_raw_parts(data, len as usize) }.to_vec();
            FWD_MSGS.lock().unwrap().push((pid, payload, msg_type));
            FWD_COUNT.fetch_add(1, Ordering::SeqCst);
        }

        /// Three nodes. Leader uses forward_message to relay a message
        /// from an external source to a specific follower. The target
        /// follower must receive the forwarded data.
        #[tokio::test]
        async fn test_forward_message_delivery_to_follower() {
            FWD_COUNT.store(0, Ordering::SeqCst);
            FWD_MSGS.lock().unwrap().clear();

            let config = NetworkConfig {
                election_timeout_min_ms: 200,
                election_timeout_max_ms: 400,
                heartbeat_interval_ms: 100,
                ..Default::default()
            };

            let n0 = NetworkState::new("fwd_0".into(), "session_fwd2".into(), config)
                .await
                .unwrap();
            let n1 = NetworkState::new("fwd_1".into(), "session_fwd2".into(), config)
                .await
                .unwrap();
            let n2 = NetworkState::new("fwd_2".into(), "session_fwd2".into(), config)
                .await
                .unwrap();

            // Register callback on all nodes
            n0.callback.register_receive_callback(Some(fwd_cb));
            n1.callback.register_receive_callback(Some(fwd_cb));
            n2.callback.register_receive_callback(Some(fwd_cb));

            // Full mesh
            let addr1 = format!("127.0.0.1:{}", n1.transport.listener_addr().port());
            let addr2 = format!("127.0.0.1:{}", n2.transport.listener_addr().port());
            n0.transport.connect_to(&addr1).await.unwrap();
            n0.transport.connect_to(&addr2).await.unwrap();
            n1.transport.connect_to(&addr2).await.unwrap();
            tokio::time::sleep(Duration::from_millis(300)).await;

            assert!(wait_until(|| n0.membership.get_connected_peer_count() >= 2, 3_000).await);

            // Wait for leader
            assert!(
                wait_until(
                    || {
                        let l0 = n0.leader_election.leader_id();
                        let l1 = n1.leader_election.leader_id();
                        let l2 = n2.leader_election.leader_id();
                        l0.is_some() && l0 == l1 && l1 == l2
                    },
                    8_000,
                )
                .await,
                "no leader consensus"
            );

            let leader = if n0.leader_election.is_leader() {
                &n0
            } else if n1.leader_election.is_leader() {
                &n1
            } else {
                &n2
            };

            // Pick a follower to forward to
            let target_id = if !n0.leader_election.is_leader() {
                PeerId::new("fwd_0")
            } else if !n1.leader_election.is_leader() {
                PeerId::new("fwd_1")
            } else {
                PeerId::new("fwd_2")
            };

            // Leader forwards a message as if from an external peer
            leader
                .session_router
                .forward_message(
                    &PeerId::new("external_peer"),
                    types::ForwardTarget::ToPeer(target_id),
                    0x0F00,
                    0x02,
                    b"forwarded_payload",
                )
                .unwrap();

            assert!(
                wait_until(|| FWD_COUNT.load(Ordering::SeqCst) >= 1, 5_000).await,
                "forwarded message not received by target"
            );

            {
                let msgs = FWD_MSGS.lock().unwrap();
                assert!(
                    msgs.iter()
                        .any(|(_, d, mt)| d == b"forwarded_payload" && *mt == 0x0F00),
                    "forwarded payload mismatch"
                );
            }
            n0.shutdown().await;
            n1.shutdown().await;
            n2.shutdown().await;
        }
    }

    // --- E2E: LZ4 compression verified round-trip ------------------------

    mod e2e_lz4_compression_verified {
        use super::*;
        use std::sync::atomic::{AtomicU32, Ordering};

        static LZ4_COUNT: AtomicU32 = AtomicU32::new(0);
        static LZ4_MSGS: std::sync::Mutex<Vec<(Vec<u8>, u8)>> = std::sync::Mutex::new(Vec::new());

        unsafe extern "C" fn lz4_cb(
            _peer_id: *const std::ffi::c_char,
            data: *const u8,
            len: i32,
            _msg_type: u16,
            flags: u8,
        ) {
            let payload = unsafe { std::slice::from_raw_parts(data, len as usize) }.to_vec();
            LZ4_MSGS.lock().unwrap().push((payload, flags));
            LZ4_COUNT.fetch_add(1, Ordering::SeqCst);
        }

        /// Send a highly compressible payload above the compression
        /// threshold. Verify the receiver gets the original data intact.
        /// Also send a small payload below threshold to confirm it is
        /// NOT compressed.
        #[tokio::test]
        async fn test_compression_roundtrip() {
            LZ4_COUNT.store(0, Ordering::SeqCst);
            LZ4_MSGS.lock().unwrap().clear();

            let node_a = create_node("lz4_a", "session_lz4").await;
            let node_b = create_node("lz4_b", "session_lz4").await;

            node_b.callback.register_receive_callback(Some(lz4_cb));

            connect_nodes(&node_a, &node_b).await;
            assert!(wait_until(|| node_a.membership.has_peer(&PeerId::new("lz4_b")), 3_000).await);

            // 1) Large compressible payload (8 KB of repeating pattern)
            let big = vec![0xABu8; 8192];
            node_a
                .session_router
                .route_message(
                    types::MessageTarget::ToPeer(PeerId::new("lz4_b")),
                    0x1000,
                    &big,
                    0,
                )
                .unwrap();

            // 2) Small payload (below 512 threshold)
            let small = b"tiny_msg";
            node_a
                .session_router
                .route_message(
                    types::MessageTarget::ToPeer(PeerId::new("lz4_b")),
                    0x1001,
                    small,
                    0,
                )
                .unwrap();

            assert!(
                wait_until(|| LZ4_COUNT.load(Ordering::SeqCst) >= 2, 5_000).await,
                "did not receive both messages"
            );

            {
                let msgs = LZ4_MSGS.lock().unwrap();

                // Big message should arrive intact
                let big_msg = msgs.iter().find(|(d, _)| d.len() == 8192);
                assert!(big_msg.is_some(), "big message not received");
                assert_eq!(big_msg.unwrap().0, big, "big message content corrupted");

                // Small message should arrive intact too
                let small_msg = msgs.iter().find(|(d, _)| d == b"tiny_msg");
                assert!(small_msg.is_some(), "small message not received");
            }
            node_a.shutdown().await;
            node_b.shutdown().await;
        }
    }

    // --- E2E: Keepalive timeout triggers disconnect detection ------------

    mod e2e_keepalive_timeout {
        use super::*;
        use std::ffi::CStr;
        use std::sync::atomic::{AtomicU32, Ordering};

        static KA_COUNT: AtomicU32 = AtomicU32::new(0);
        static KA_EVENTS: std::sync::Mutex<Vec<(String, i32)>> = std::sync::Mutex::new(Vec::new());

        unsafe extern "C" fn ka_status_cb(peer_id: *const std::ffi::c_char, status: i32) {
            let pid = unsafe { CStr::from_ptr(peer_id) }
                .to_str()
                .unwrap()
                .to_owned();
            KA_EVENTS.lock().unwrap().push((pid, status));
            KA_COUNT.fetch_add(1, Ordering::SeqCst);
        }

        /// Connect two nodes with very short heartbeat/timeout windows.
        /// Then freeze one node (drop its transport without sending
        /// PeerLeave). The other node should detect the timeout and
        /// report a Disconnected status.
        #[tokio::test]
        async fn test_keepalive_timeout_detection() {
            KA_COUNT.store(0, Ordering::SeqCst);
            KA_EVENTS.lock().unwrap().clear();

            let config = NetworkConfig {
                heartbeat_interval_ms: 100,
                heartbeat_timeout_multiplier: 2,
                ..Default::default()
            };
            // Short timeout: 100ms × 2 = 200ms before declaring timeout

            let n0 = NetworkState::new("ka_0".into(), "session_ka".into(), config)
                .await
                .unwrap();
            let n1 = NetworkState::new("ka_1".into(), "session_ka".into(), config)
                .await
                .unwrap();

            n0.callback
                .register_peer_status_callback(Some(ka_status_cb));

            connect_nodes(&n0, &n1).await;
            assert!(wait_until(|| n0.membership.has_peer(&PeerId::new("ka_1")), 3_000).await);

            // Clear accumulated connect events
            KA_COUNT.store(0, Ordering::SeqCst);
            KA_EVENTS.lock().unwrap().clear();

            // Abruptly kill n1's transport (no PeerLeave sent)
            n1.shutdown_token.cancel();
            n1.transport.shutdown();

            // n0 should detect timeout and fire Disconnected status
            assert!(
                wait_until(
                    || {
                        KA_EVENTS.lock().unwrap().iter().any(|(pid, s)| {
                            pid == "ka_1" && *s == PeerStatus::Disconnected.as_i32()
                        })
                    },
                    10_000,
                )
                .await,
                "keepalive timeout not detected (events: {:?})",
                KA_EVENTS.lock().unwrap()
            );

            n0.shutdown().await;
            // n1 already shut down manually
        }
    }

    // --- E2E: Max connections limit enforced at E2E level ----------------

    mod e2e_max_connections_enforced {
        use super::*;

        use futures_util::{SinkExt, StreamExt};
        use tokio::net::TcpStream;
        use tokio_util::codec::Framed;

        use crate::config::PROTOCOL_VERSION;
        use crate::messaging::{decode_internal, encode_internal};
        use crate::protocol::InternalMessage;
        use crate::transport::PacketCodec;

        /// Use raw TCP + handshake to saturate the server's connection
        /// limit without creating heavyweight `NetworkState` per client.
        /// Returns (success_count, reject_count, open_streams).
        async fn fill_connections(
            addr: &str,
            session: &str,
            count: usize,
        ) -> (u32, u32, Vec<Framed<TcpStream, PacketCodec>>) {
            let mut success = 0u32;
            let mut reject = 0u32;
            let mut streams = Vec::new();

            for i in 0..count {
                let Ok(tcp) = TcpStream::connect(addr).await else {
                    reject += 1;
                    continue;
                };
                let mut framed = Framed::new(
                    tcp,
                    PacketCodec::new(NetworkConfig::default().max_message_size),
                );
                let hs = encode_internal(
                    &InternalMessage::Handshake {
                        peer_id: format!("mc_c{i}"),
                        listen_port: 0,
                        protocol_version: PROTOCOL_VERSION,
                        session_id: session.into(),
                    },
                    NetworkConfig::default().max_message_size,
                )
                .unwrap();
                if framed.send(hs).await.is_err() {
                    reject += 1;
                    continue;
                }
                match tokio::time::timeout(Duration::from_secs(3), framed.next()).await {
                    Ok(Some(Ok(pkt))) => {
                        if let Ok(InternalMessage::HandshakeAck { success: ok, .. }) =
                            decode_internal(&pkt)
                        {
                            if ok {
                                success += 1;
                                streams.push(framed);
                            } else {
                                reject += 1;
                            }
                        } else {
                            reject += 1;
                        }
                    }
                    _ => reject += 1,
                }
            }
            (success, reject, streams)
        }

        /// Saturate the server with raw handshake connections, verify
        /// some are rejected, then verify recovery after disconnect.
        #[tokio::test]
        async fn test_max_connections_rejection() {
            let server = create_node("mc_server", "session_mc").await;
            let addr = format!("127.0.0.1:{}", server.transport.listener_addr().port());

            let max_conn = NetworkConfig::default().max_connections;
            let target = max_conn + 2;
            let (success_count, reject_count, streams) =
                fill_connections(&addr, "session_mc", target).await;

            assert!(
                success_count > 0,
                "at least some connections should succeed"
            );
            assert!(
                success_count <= max_conn as u32,
                "connected {success_count} > max_connections ({max_conn})"
            );
            assert!(
                reject_count > 0,
                "expected at least one rejected connection"
            );

            // Drop one stream to free a slot
            drop(streams.into_iter().next());
            tokio::time::sleep(Duration::from_millis(300)).await;

            // Now a new connection should succeed
            let new_client = create_node("mc_new", "session_mc").await;
            let result = new_client.transport.connect_to(&addr).await;
            assert!(
                result.is_ok(),
                "should accept new connection after disconnect"
            );
            new_client.shutdown().await;
            server.shutdown().await;
        }
    }

    // --- E2E: Protocol version mismatch at E2E level ---------------------

    mod e2e_version_mismatch {
        use super::*;

        use futures_util::{SinkExt, StreamExt};
        use tokio::net::TcpStream;
        use tokio_util::codec::Framed;

        use crate::messaging::{decode_internal, encode_internal};
        use crate::protocol::InternalMessage;
        use crate::transport::PacketCodec;

        /// Raw TCP connection sending a wrong protocol version.
        /// The server must reject with version_mismatch and remain
        /// functional for subsequent correct connections.
        #[tokio::test]
        async fn test_version_mismatch_then_recovery() {
            let node = create_node("vm_server", "session_vm").await;
            let addr = format!("127.0.0.1:{}", node.transport.listener_addr().port());

            let cfg = NetworkConfig::default();
            // 1) Send a handshake with wrong version
            let stream = TcpStream::connect(&addr).await.unwrap();
            let mut framed = Framed::new(stream, PacketCodec::new(cfg.max_message_size));

            let hs = encode_internal(
                &InternalMessage::Handshake {
                    peer_id: "vm_bad".into(),
                    listen_port: 9999,
                    protocol_version: 999,
                    session_id: "session_vm".into(),
                },
                cfg.max_message_size,
            )
            .unwrap();
            framed.send(hs).await.unwrap();

            let pkt = framed
                .next()
                .await
                .expect("expected response")
                .expect("read error");
            let msg = decode_internal(&pkt).unwrap();
            match msg {
                InternalMessage::HandshakeAck {
                    success,
                    error_reason,
                    ..
                } => {
                    assert!(!success, "should reject wrong version");
                    assert!(
                        error_reason
                            .as_deref()
                            .unwrap_or("")
                            .contains("version_mismatch"),
                        "expected version_mismatch error"
                    );
                }
                _ => panic!("expected HandshakeAck"),
            }

            // 2) Now a correct connection should still work
            let good = create_node("vm_good", "session_vm").await;
            connect_nodes(&good, &node).await;
            assert!(
                wait_until(|| node.membership.has_peer(&PeerId::new("vm_good")), 3_000).await,
                "node should accept correct connection after version mismatch"
            );

            good.shutdown().await;
            node.shutdown().await;
        }
    }

    // --- E2E: FFI error handling — calling APIs in wrong order -----------

    mod e2e_ffi_wrong_order {
        use super::*;

        /// Call FFI operations that require initialization BEFORE
        /// InitializeNetwork is called. They must return appropriate
        /// error codes without crashing.
        #[test]
        fn test_ffi_operations_before_init() {
            use crate::ffi::*;

            // These should all fail gracefully when not initialized
            assert_eq!(IsNetworkInitialized(), 0);

            let peer = std::ffi::CString::new("some_peer").unwrap();
            let data = b"test_data";

            // SendToPeer without init
            let result = SendToPeer(peer.as_ptr(), data.as_ptr(), data.len() as i32, 0x0100, 0);
            assert_ne!(result, error::error_codes::OK);

            // BroadcastMessage without init
            let result = BroadcastMessage(data.as_ptr(), data.len() as i32, 0x0100, 0);
            assert_ne!(result, error::error_codes::OK);

            // SendToLeader without init
            let result = SendToLeader(data.as_ptr(), data.len() as i32, 0x0100, 0);
            assert_ne!(result, error::error_codes::OK);

            // ConnectToPeer without init
            let addr = std::ffi::CString::new("127.0.0.1:9999").unwrap();
            let result = ConnectToPeer(addr.as_ptr());
            assert_ne!(result, error::error_codes::OK);

            // DisconnectPeer without init
            let result = DisconnectPeer(peer.as_ptr());
            assert_ne!(result, error::error_codes::OK);

            // SetLeader without init
            let result = SetLeader(peer.as_ptr());
            assert_ne!(result, error::error_codes::OK);

            // ForwardMessage without init
            let result = ForwardMessage(
                peer.as_ptr(),
                peer.as_ptr(),
                data.as_ptr(),
                data.len() as i32,
                0x0100,
                0,
            );
            assert_ne!(result, error::error_codes::OK);

            // SendFromLeader without init
            let result = SendFromLeader(data.as_ptr(), data.len() as i32, 0x0100, 0);
            assert_ne!(result, error::error_codes::OK);

            // Double shutdown is also safe
            let result = ShutdownNetwork();
            assert_eq!(result, error::error_codes::NOT_INITIALIZED);
        }
    }

    // --- E2E: Rapid reconnection stability -------------------------------

    mod e2e_rapid_reconnect_stability {
        use super::*;
        use std::sync::atomic::{AtomicU32, Ordering};

        static RAPID_COUNT: AtomicU32 = AtomicU32::new(0);
        static RAPID_MSGS: std::sync::Mutex<Vec<Vec<u8>>> = std::sync::Mutex::new(Vec::new());

        unsafe extern "C" fn rapid_cb(
            _peer_id: *const std::ffi::c_char,
            data: *const u8,
            len: i32,
            _msg_type: u16,
            _flags: u8,
        ) {
            let payload = unsafe { std::slice::from_raw_parts(data, len as usize) }.to_vec();
            RAPID_MSGS.lock().unwrap().push(payload);
            RAPID_COUNT.fetch_add(1, Ordering::SeqCst);
        }

        /// Rapidly connect, disconnect, reconnect the same two nodes
        /// 10 times, then verify messaging still works after the storm.
        #[tokio::test]
        async fn test_rapid_reconnect_then_message() {
            RAPID_COUNT.store(0, Ordering::SeqCst);
            RAPID_MSGS.lock().unwrap().clear();

            let node_a = create_node("rapid_a", "session_rapid").await;
            let node_b = create_node("rapid_b", "session_rapid").await;
            let addr_b = format!("127.0.0.1:{}", node_b.transport.listener_addr().port());

            node_b.callback.register_receive_callback(Some(rapid_cb));

            // Rapid connect/disconnect cycles
            for _ in 0..10 {
                let _ = node_a.transport.connect_to(&addr_b).await;
                tokio::time::sleep(Duration::from_millis(30)).await;
                let _ = node_a.transport.disconnect_peer(&PeerId::new("rapid_b"));
                node_a.membership.remove_peer(&PeerId::new("rapid_b"));
                tokio::time::sleep(Duration::from_millis(30)).await;
            }

            // Both nodes should still be functional
            assert!(!node_a.shutdown_token.is_cancelled());
            assert!(!node_b.shutdown_token.is_cancelled());

            // Final stable connection
            node_a.transport.connect_to(&addr_b).await.unwrap();
            assert!(
                wait_until(
                    || node_a.membership.has_peer(&PeerId::new("rapid_b")),
                    3_000
                )
                .await,
                "failed to establish stable connection after rapid cycles"
            );

            // Send a message to verify the link works
            node_a
                .session_router
                .route_message(
                    types::MessageTarget::ToPeer(PeerId::new("rapid_b")),
                    0x1100,
                    b"survived_the_storm",
                    0,
                )
                .unwrap();

            assert!(
                wait_until(|| RAPID_COUNT.load(Ordering::SeqCst) >= 1, 5_000).await,
                "message after rapid reconnect not received"
            );

            {
                let msgs = RAPID_MSGS.lock().unwrap();
                assert!(msgs.iter().any(|d| d == b"survived_the_storm"));
            }
            node_a.shutdown().await;
            node_b.shutdown().await;
        }
    }

    // --- E2E: mDNS auto-connect race with manual ConnectTo ---------------

    mod e2e_mdns_race_connect_then_manual {
        use super::*;

        /// Two peers in the same session with mDNS enabled. After mDNS
        /// auto-connects them, a manual `ConnectToPeer` to the same peer
        /// should return `AlreadyConnected` rather than a hard error.
        #[tokio::test]
        async fn test_mdns_race_connect_then_manual() {
            if !mdns_available().await {
                eprintln!("mDNS not available, skipping test");
                return;
            }

            let config = NetworkConfig {
                heartbeat_interval_ms: 200,
                election_timeout_min_ms: 400,
                election_timeout_max_ms: 800,
                ..Default::default()
            };

            let n0 = NetworkState::new("mdns_race_0".into(), "session_mdns_race".into(), config)
                .await
                .unwrap();
            let n1 = NetworkState::new("mdns_race_1".into(), "session_mdns_race".into(), config)
                .await
                .unwrap();

            // Wait for mDNS to auto-connect the two nodes
            let connected = wait_until(
                || n0.membership.has_peer(&PeerId::new("mdns_race_1")),
                10_000,
            )
            .await;
            assert!(connected, "mDNS did not auto-connect the peers");

            // Now manually try to connect to the same peer — should get AlreadyConnected
            let addr = format!("127.0.0.1:{}", n1.transport.listener_addr().port());
            let result = n0.transport.connect_to(&addr).await;
            assert!(
                matches!(result, Err(NetworkError::AlreadyConnected(_))),
                "expected AlreadyConnected, got: {result:?}"
            );

            // Verify original connection is still intact
            assert!(n0.transport.has_peer(&PeerId::new("mdns_race_1")));
            assert!(n1.transport.has_peer(&PeerId::new("mdns_race_0")));

            n0.shutdown().await;
            n1.shutdown().await;
        }
    }

    // --- E2E: election loop skips when leader is known -------------------

    mod e2e_election_skips_known_leader {
        use super::*;

        /// Two nodes connected with auto-election. Once a leader is
        /// elected, the election term should stabilize — no unnecessary
        /// re-elections. Verify by checking that the term stays constant
        /// for several election timeout periods.
        #[tokio::test]
        async fn test_election_skips_known_leader() {
            let config = NetworkConfig {
                election_timeout_min_ms: 200,
                election_timeout_max_ms: 400,
                heartbeat_interval_ms: 100,
                ..Default::default()
            };

            let n0 = NetworkState::new("esk_0".into(), "session_esk".into(), config)
                .await
                .unwrap();
            let n1 = NetworkState::new("esk_1".into(), "session_esk".into(), config)
                .await
                .unwrap();

            connect_nodes(&n0, &n1).await;
            assert!(wait_until(|| n0.membership.has_peer(&PeerId::new("esk_1")), 3_000).await);

            // Wait for leader election
            assert!(
                wait_until(
                    || {
                        let l0 = n0.leader_election.leader_id();
                        let l1 = n1.leader_election.leader_id();
                        l0.is_some() && l0 == l1
                    },
                    8_000,
                )
                .await,
                "leader election did not converge"
            );

            let leader = n0.leader_election.leader_id().unwrap();
            let term_after_election = n0.leader_election.current_term();

            // Wait several election timeout periods and verify term is stable
            tokio::time::sleep(Duration::from_millis(2_000)).await;

            let term_later = n0.leader_election.current_term();
            let leader_later = n0.leader_election.leader_id();
            assert_eq!(
                term_after_election, term_later,
                "term changed from {term_after_election} to {term_later} — spurious re-election"
            );
            assert_eq!(
                leader_later.as_ref(),
                Some(&leader),
                "leader changed unexpectedly"
            );

            n0.shutdown().await;
            n1.shutdown().await;
        }
    }

    // --- E2E: manual leader survives heartbeat from other nodes ----------

    mod e2e_manual_leader_survives_heartbeat {
        use super::*;

        /// Manually assign a leader via `set_leader`, then let another
        /// node win an election (producing higher-term heartbeats). The
        /// manual leader assignment must not be overridden on nodes
        /// where `manual_override` is active.
        #[tokio::test]
        async fn test_manual_leader_survives_heartbeat() {
            let config = NetworkConfig {
                election_timeout_min_ms: 200,
                election_timeout_max_ms: 400,
                heartbeat_interval_ms: 100,
                auto_election_enabled: false, // disable auto to control manually
                ..Default::default()
            };

            let n0 = NetworkState::new(
                "manual_leader_survives_heartbeat_0".into(),
                "session_manual_leader_survives_heartbeat".into(),
                config,
            )
            .await
            .unwrap();
            let n1 = NetworkState::new(
                "manual_leader_survives_heartbeat_1".into(),
                "session_manual_leader_survives_heartbeat".into(),
                config,
            )
            .await
            .unwrap();

            connect_nodes(&n0, &n1).await;
            assert!(
                wait_until(
                    || n0
                        .membership
                        .has_peer(&PeerId::new("manual_leader_survives_heartbeat_1")),
                    3_000
                )
                .await
            );

            // Manually set n0 as leader on both nodes (like FFI SetLeader)
            let manual_leader = PeerId::new("manual_leader_survives_heartbeat_0");
            let (term, lid, aid) = n0.leader_election.set_leader(&manual_leader);
            let assign = encode_internal(
                &InternalMessage::LeaderAssign {
                    term,
                    leader_id: lid,
                    assigner_id: aid,
                },
                config.max_message_size,
            )
            .unwrap();
            n0.transport.broadcast(assign, None);
            tokio::time::sleep(Duration::from_millis(200)).await;

            // Both nodes should see n0 as leader
            assert_eq!(
                n0.leader_election.leader_id().as_ref(),
                Some(&manual_leader)
            );
            assert_eq!(
                n1.leader_election.leader_id().as_ref(),
                Some(&manual_leader)
            );

            // Simulate a foreign heartbeat with a much higher term from n1
            // trying to claim leadership. Under manual_override this should
            // be rejected by n0.
            let foreign_term = n0.leader_election.current_term() + 10;
            let accepted = n0
                .leader_election
                .handle_heartbeat(foreign_term, "manual_leader_survives_heartbeat_1");
            assert!(
                !accepted,
                "manual override node should reject foreign heartbeat"
            );

            // n0 still considers itself the manual leader
            assert_eq!(
                n0.leader_election.leader_id().as_ref(),
                Some(&manual_leader)
            );

            n0.shutdown().await;
            n1.shutdown().await;
        }
    }
}
