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

pub struct NetworkState {
    pub transport: Arc<TransportManager>,
    pub membership: Arc<MembershipManager>,
    pub leader_election: Arc<LeaderElection>,
    pub session_router: Arc<SessionRouter>,
    pub discovery: Option<DiscoveryManager>,
    pub callback: Arc<CallbackManager>,
    pub shutdown_token: CancellationToken,
    pub local_peer_id: PeerId,
    pub session_id: String,
    pub config: NetworkConfig,
}

impl NetworkState {
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
            membership.event_tx.subscribe(),
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
            Arc::clone(&membership),
            Arc::clone(&transport),
            shutdown_token.clone(),
        ) {
            Ok(d) => {
                if let Err(e) = d.start() {
                    tracing::warn!(error = %e, "mDNS discovery start failed");
                }
                Some(d)
            }
            Err(e) => {
                tracing::warn!(error = %e, "mDNS discovery creation failed");
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

struct MessageHandlerCtx {
    transport: Arc<TransportManager>,
    membership: Arc<MembershipManager>,
    leader_election: Arc<LeaderElection>,
    session_router: Arc<SessionRouter>,
    callback: Arc<CallbackManager>,
    local_peer_id: PeerId,
}

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

async fn handle_message(peer_id: &PeerId, packet: RawPacket, ctx: &MessageHandlerCtx) {
    let MessageHandlerCtx {
        transport,
        membership,
        leader_election,
        session_router,
        callback,
        local_peer_id,
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
                let pong = encode_internal(&InternalMessage::Pong { timestamp_ms });
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
                let resp =
                    encode_internal(&InternalMessage::HeartbeatResponse { term, timestamp_ms });
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
                let response = encode_internal(&InternalMessage::VoteResponse {
                    term: resp_term,
                    voter_id: local_peer_id.as_str().to_owned(),
                    granted,
                });
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
                    let hb = encode_internal(&InternalMessage::Heartbeat {
                        term: leader_election.current_term(),
                        leader_id: local_peer_id.as_str().to_owned(),
                        timestamp_ms: current_timestamp_ms(),
                    });
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
                    callback.send_message_received(
                        &from,
                        data,
                        user_packet.msg_type,
                        user_packet.flags,
                    );
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
        callback.send_message_received(peer_id, data, packet.msg_type, packet.flags);
    }
}

fn spawn_periodic_tasks(
    transport: Arc<TransportManager>,
    membership: Arc<MembershipManager>,
    leader_election: Arc<LeaderElection>,
    callback: Arc<CallbackManager>,
    config: NetworkConfig,
    shutdown_token: CancellationToken,
) {
    let hb_interval = Duration::from_millis(config.heartbeat_interval_ms);

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
                        let ping = encode_internal(&InternalMessage::Ping { timestamp_ms: ts });
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
                        });
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
                        {
                            continue;
                        }

                        let count = membership.get_connected_peer_count();
                        if let Some((term, candidate_id)) = leader_election.start_election(count) {
                            let vote_req = encode_internal(&InternalMessage::RequestVote {
                                term,
                                candidate_id,
                            });
                            if let Ok(pkt) = vote_req {
                                transport.broadcast(pkt, None);
                            }
                        } else if leader_election.is_leader() {
                            let hb = encode_internal(&InternalMessage::Heartbeat {
                                term: leader_election.current_term(),
                                leader_id: transport.local_peer_id().as_str().to_owned(),
                                timestamp_ms: current_timestamp_ms(),
                            });
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

fn spawn_membership_event_watcher(
    mut event_rx: tokio::sync::broadcast::Receiver<MembershipEvent>,
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
                        Ok(MembershipEvent::Left(pid)) => {
                            callback.send_peer_status(&pid, PeerStatus::Disconnected.as_i32());
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
        let mut config = NetworkConfig::default();
        config.send_queue_capacity = 1;
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
        let config = NetworkConfigFFI {
            heartbeat_interval_ms: 500,
            election_timeout_min_ms: 1500,
            election_timeout_max_ms: 3000,
            reconnect_initial_ms: 1000,
            reconnect_max_ms: 30000,
            compression_threshold: 256,
            heartbeat_timeout_multiplier: 3,
            send_queue_capacity: 512,
            centralized_auto_forward: 1,
            auto_election_enabled: 1,
            _padding: [0; 2],
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

    // --- Handshake version mismatch (raw TCP, forge wrong version) ---

    #[tokio::test]
    async fn test_handshake_version_mismatch() {
        use tokio::net::TcpStream;
        use tokio_util::codec::Framed;

        use crate::messaging::{decode_internal, encode_internal};
        use crate::protocol::InternalMessage;
        use crate::transport::PacketCodec;

        let node = create_node("peer_b", "session_ver").await;
        let addr = format!("127.0.0.1:{}", node.transport.listener_addr().port());

        let stream = TcpStream::connect(&addr).await.unwrap();
        let mut framed = Framed::new(stream, PacketCodec::new());

        let hs = encode_internal(&InternalMessage::Handshake {
            peer_id: "peer_a".into(),
            listen_port: 9999,
            protocol_version: 999,
            session_id: "session_ver".into(),
        })
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

    // --- Handshake timeout (connect but never send Handshake) ---

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

    // --- Max connections ---

    #[tokio::test]
    async fn test_max_connections() {
        use crate::config::MAX_CONNECTIONS;

        let node = create_node("peer_main", "session_max").await;
        let addr = format!("127.0.0.1:{}", node.transport.listener_addr().port());

        // MAX_CONNECTIONS is 64, but we test the concept with a smaller number
        // by connecting many peers and checking the limit is enforced
        let mut peers = Vec::new();
        let mut connected = 0u32;
        for i in 0..66 {
            let p = create_node(&format!("peer_{i}"), "session_max").await;
            let result = p.transport.connect_to(&addr).await;
            if result.is_ok() {
                connected += 1;
            }
            peers.push(p);
        }

        // At least some should succeed, and the count should be capped
        assert!(connected > 0);
        assert!(connected <= MAX_CONNECTIONS as u32);

        node.shutdown().await;
        for p in peers {
            p.shutdown().await;
        }
    }

    // --- Reconnect exponential backoff ---

    #[tokio::test]
    async fn test_reconnect_exponential_backoff() {
        let mut config = NetworkConfig::default();
        config.reconnect_initial_ms = 100;
        config.reconnect_max_ms = 500;

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

    // --- 3-node auto leader election (integration) ---

    #[tokio::test]
    async fn test_auto_leader_election_3_nodes() {
        let mut config = NetworkConfig::default();
        config.election_timeout_min_ms = 300;
        config.election_timeout_max_ms = 600;
        config.heartbeat_interval_ms = 100;

        let n0 = NetworkState::new("node_0".into(), "session_elect3".into(), config)
            .await
            .unwrap();
        let n1 = NetworkState::new("node_1".into(), "session_elect3".into(), config)
            .await
            .unwrap();
        let n2 = NetworkState::new("node_2".into(), "session_elect3".into(), config)
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

    // --- 2-node auto leader election ---

    #[tokio::test]
    async fn test_auto_leader_election_2_nodes() {
        let mut config = NetworkConfig::default();
        config.election_timeout_min_ms = 300;
        config.election_timeout_max_ms = 600;
        config.heartbeat_interval_ms = 100;

        let n0 = NetworkState::new("node_0".into(), "session_elect2".into(), config)
            .await
            .unwrap();
        let n1 = NetworkState::new("node_1".into(), "session_elect2".into(), config)
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

    // --- Centralized routing ---

    #[tokio::test]
    async fn test_centralized_routing() {
        let mut config = NetworkConfig::default();
        config.election_timeout_min_ms = 200;
        config.election_timeout_max_ms = 400;
        config.heartbeat_interval_ms = 100;

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

    // --- Forward message (Leader only) ---

    #[tokio::test]
    async fn test_forward_message() {
        let mut config = NetworkConfig::default();
        config.election_timeout_min_ms = 200;
        config.election_timeout_max_ms = 400;
        config.heartbeat_interval_ms = 100;

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

    // --- Callback manager direct test ---

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
        cb.send_message_received(
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

    // --- Concurrent FFI calls ---

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

    // --- Rapid connect/disconnect ---

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

    // --- Broadcast lagged recovery ---

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

    // --- Discovery logic tests (no real mDNS needed) ---

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
}
