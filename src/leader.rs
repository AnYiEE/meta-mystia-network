use std::collections::HashSet;

use parking_lot::{Mutex, RwLock};
use tokio::sync::{broadcast, mpsc};

use crate::membership::MembershipEvent;
use crate::types::PeerId;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Role {
    Follower,
    Candidate,
    Leader,
}

struct ElectionState {
    role: Role,
    current_term: u64,
    voted_for: Option<PeerId>,
    leader_id: Option<PeerId>,
    votes_received: HashSet<PeerId>,
    auto_election_enabled: bool,
    manual_override: bool,
}

pub struct LeaderElection {
    local_peer_id: PeerId,
    state: RwLock<ElectionState>,
    membership_rx: Mutex<broadcast::Receiver<MembershipEvent>>,
    leader_change_tx: mpsc::Sender<Option<PeerId>>,
}

impl LeaderElection {
    pub fn new(
        local_peer_id: PeerId,
        auto_election_enabled: bool,
        membership_rx: broadcast::Receiver<MembershipEvent>,
        leader_change_tx: mpsc::Sender<Option<PeerId>>,
    ) -> Self {
        Self {
            local_peer_id,
            state: RwLock::new(ElectionState {
                role: Role::Follower,
                current_term: 0,
                voted_for: None,
                leader_id: None,
                votes_received: HashSet::new(),
                auto_election_enabled,
                manual_override: false,
            }),
            membership_rx: Mutex::new(membership_rx),
            leader_change_tx,
        }
    }

    pub fn role(&self) -> Role {
        self.state.read().role
    }

    pub fn current_term(&self) -> u64 {
        self.state.read().current_term
    }

    pub fn leader_id(&self) -> Option<PeerId> {
        self.state.read().leader_id.clone()
    }

    pub fn is_leader(&self) -> bool {
        self.state.read().role == Role::Leader
    }

    pub fn is_auto_election_enabled(&self) -> bool {
        self.state.read().auto_election_enabled
    }

    fn majority_threshold(total_nodes: usize) -> usize {
        if total_nodes <= 2 {
            1
        } else {
            total_nodes / 2 + 1
        }
    }

    pub fn start_election(&self, connected_peer_count: usize) -> Option<(u64, String)> {
        let mut state = self.state.write();
        if !state.auto_election_enabled || state.manual_override {
            return None;
        }

        state.current_term += 1;
        state.role = Role::Candidate;
        // Per Raft protocol: reset voted_for before voting for self
        state.voted_for = Some(self.local_peer_id.clone());
        state.votes_received.clear();
        state.votes_received.insert(self.local_peer_id.clone());

        let term = state.current_term;
        let total_nodes = 1 + connected_peer_count;
        let threshold = Self::majority_threshold(total_nodes);

        tracing::debug!(term, total_nodes, threshold, "starting election");

        if state.votes_received.len() >= threshold {
            self.become_leader_inner(&mut state);
            return None;
        }

        Some((term, self.local_peer_id.as_str().to_owned()))
    }

    pub fn handle_request_vote(&self, term: u64, candidate_id: &str) -> (u64, bool) {
        let mut state = self.state.write();

        if term < state.current_term {
            return (state.current_term, false);
        }

        if term > state.current_term {
            state.current_term = term;
            state.voted_for = None;
            state.role = Role::Follower;
            state.votes_received.clear();
        }

        let granted = match &state.voted_for {
            None => true,
            Some(voted) => voted.as_str() == candidate_id,
        };

        if granted {
            state.voted_for = Some(PeerId::new(candidate_id));
        }

        (state.current_term, granted)
    }

    pub fn handle_vote_response(
        &self,
        term: u64,
        voter_id: &str,
        granted: bool,
        connected_peer_count: usize,
    ) -> bool {
        let mut state = self.state.write();

        if term > state.current_term {
            state.current_term = term;
            state.voted_for = None;
            state.role = Role::Follower;
            state.votes_received.clear();
            return false;
        }

        if state.role != Role::Candidate || term != state.current_term {
            return false;
        }

        if granted {
            state.votes_received.insert(PeerId::new(voter_id));
        }

        let total_nodes = 1 + connected_peer_count;
        let threshold = Self::majority_threshold(total_nodes);

        if state.votes_received.len() >= threshold {
            self.become_leader_inner(&mut state);
            return true;
        }

        false
    }

    pub fn handle_heartbeat(&self, term: u64, leader_id: &str) -> bool {
        let mut state = self.state.write();

        if term < state.current_term {
            return false;
        }

        let higher_term = term > state.current_term;
        if higher_term {
            state.current_term = term;
            state.voted_for = None;
        }

        if state.role == Role::Leader {
            if higher_term || self.local_peer_id.as_str() < leader_id {
                self.step_down_inner(&mut state);
            } else {
                return false;
            }
        }

        if state.role != Role::Follower {
            state.role = Role::Follower;
            state.votes_received.clear();
        }

        let old_leader = state.leader_id.clone();
        let new_leader = PeerId::new(leader_id);
        state.leader_id = Some(new_leader.clone());

        if old_leader.as_ref() != Some(&new_leader) {
            drop(state);
            self.notify_leader_change(Some(new_leader));
        }

        true
    }

    pub fn handle_leader_assign(&self, term: u64, leader_id: &str, _assigner_id: &str) {
        let mut state = self.state.write();

        if term > state.current_term {
            state.current_term = term;
        }

        let new_leader = PeerId::new(leader_id);
        let old_leader = state.leader_id.clone();

        state.leader_id = Some(new_leader.clone());
        state.manual_override = true;

        if new_leader == self.local_peer_id {
            state.role = Role::Leader;
        } else {
            state.role = Role::Follower;
        }

        state.voted_for = None;
        state.votes_received.clear();

        if old_leader.as_ref() != Some(&new_leader) {
            drop(state);
            self.notify_leader_change(Some(new_leader));
        }
    }

    pub fn set_leader(&self, leader_id: &PeerId) -> (u64, String, String) {
        let mut state = self.state.write();

        state.current_term += 1;
        state.manual_override = true;

        let old_leader = state.leader_id.clone();
        state.leader_id = Some(leader_id.clone());

        if *leader_id == self.local_peer_id {
            state.role = Role::Leader;
        } else {
            state.role = Role::Follower;
        }

        state.voted_for = None;
        state.votes_received.clear();

        let term = state.current_term;
        let lid = leader_id.as_str().to_owned();
        let aid = self.local_peer_id.as_str().to_owned();

        if old_leader.as_ref() != Some(leader_id) {
            drop(state);
            self.notify_leader_change(Some(leader_id.clone()));
        }

        (term, lid, aid)
    }

    pub fn enable_auto_election(&self, enable: bool) {
        let mut state = self.state.write();
        state.auto_election_enabled = enable;
        if enable && state.leader_id.is_none() {
            state.manual_override = false;
        }
    }

    pub fn handle_peer_left(&self, peer_id: &PeerId) {
        let state = self.state.read();
        if state.leader_id.as_ref() == Some(peer_id) {
            drop(state);
            let mut state = self.state.write();
            state.leader_id = None;
            if state.role == Role::Follower && !state.manual_override {
                // Will trigger election timeout naturally
            }
            drop(state);
            self.notify_leader_change(None);
        }
    }

    pub fn drain_membership_events(&self, connected_peer_count: &mut usize) {
        let mut rx = self.membership_rx.lock();
        loop {
            match rx.try_recv() {
                Ok(MembershipEvent::Joined(_)) => {
                    *connected_peer_count += 1;
                }
                Ok(MembershipEvent::Left(ref pid)) => {
                    *connected_peer_count = connected_peer_count.saturating_sub(1);
                    self.handle_peer_left(pid);
                }
                Ok(MembershipEvent::StatusChanged { .. }) => {}
                Err(broadcast::error::TryRecvError::Lagged(n)) => {
                    tracing::warn!(n, "leader election broadcast lagged, need full sync");
                    break;
                }
                Err(_) => break,
            }
        }
    }

    fn notify_leader_change(&self, leader: Option<PeerId>) {
        if self.leader_change_tx.try_send(leader).is_err() {
            tracing::warn!("leader change notification channel full, event dropped");
        }
    }

    fn become_leader_inner(&self, state: &mut ElectionState) {
        state.role = Role::Leader;
        state.leader_id = Some(self.local_peer_id.clone());
        state.votes_received.clear();

        tracing::info!(
            term = state.current_term,
            peer = %self.local_peer_id,
            "became leader"
        );

        self.notify_leader_change(Some(self.local_peer_id.clone()));
    }

    fn step_down_inner(&self, state: &mut ElectionState) {
        state.role = Role::Follower;
        state.votes_received.clear();
        tracing::info!(
            term = state.current_term,
            peer = %self.local_peer_id,
            "stepped down from leader"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use tokio::sync::broadcast;

    fn create_election(peer_id: &str) -> (LeaderElection, mpsc::Receiver<Option<PeerId>>) {
        let (event_tx, event_rx) = broadcast::channel(256);
        let _ = event_tx;
        let (leader_tx, leader_rx) = mpsc::channel(16);
        let election = LeaderElection::new(PeerId::new(peer_id), true, event_rx, leader_tx);
        (election, leader_rx)
    }

    #[test]
    fn test_initial_state() {
        let (election, _rx) = create_election("peer1");
        assert_eq!(election.role(), Role::Follower);
        assert_eq!(election.current_term(), 0);
        assert!(election.leader_id().is_none());
        assert!(!election.is_leader());
    }

    #[test]
    fn test_start_election_single_node() {
        let (election, _rx) = create_election("peer1");
        let result = election.start_election(0);
        assert!(result.is_none());
        assert_eq!(election.role(), Role::Leader);
        assert!(election.is_leader());
    }

    #[test]
    fn test_start_election_two_nodes() {
        let (election, _rx) = create_election("peer1");
        let result = election.start_election(1);
        assert!(result.is_none());
        assert_eq!(election.role(), Role::Leader);
    }

    #[test]
    fn test_start_election_three_nodes() {
        let (election, _rx) = create_election("peer1");
        let result = election.start_election(2);
        assert!(result.is_some());
        assert_eq!(election.role(), Role::Candidate);

        let (term, candidate) = result.unwrap();
        assert_eq!(term, 1);
        assert_eq!(candidate, "peer1");
    }

    #[test]
    fn test_vote_response_majority() {
        let (election, _rx) = create_election("peer1");
        election.start_election(2);

        let became_leader = election.handle_vote_response(1, "peer2", true, 2);
        assert!(became_leader);
        assert_eq!(election.role(), Role::Leader);
    }

    #[test]
    fn test_handle_heartbeat() {
        let (election, _rx) = create_election("peer1");
        let accepted = election.handle_heartbeat(1, "peer2");
        assert!(accepted);
        assert_eq!(election.role(), Role::Follower);
        assert_eq!(election.leader_id().unwrap().as_str(), "peer2");
    }

    #[test]
    fn test_higher_term_steps_down() {
        let (election, _rx) = create_election("peer1");
        election.start_election(0);
        assert_eq!(election.role(), Role::Leader);

        let accepted = election.handle_heartbeat(2, "peer2");
        assert!(accepted);
        assert_eq!(election.role(), Role::Follower);
    }

    #[test]
    fn test_manual_set_leader() {
        let (election, _rx) = create_election("peer1");
        let (term, lid, aid) = election.set_leader(&PeerId::new("peer2"));
        assert_eq!(term, 1);
        assert_eq!(lid, "peer2");
        assert_eq!(aid, "peer1");
        assert_eq!(election.leader_id().unwrap().as_str(), "peer2");
        assert_eq!(election.role(), Role::Follower);
    }

    #[test]
    fn test_manual_set_leader_self() {
        let (election, _rx) = create_election("peer1");
        election.set_leader(&PeerId::new("peer1"));
        assert_eq!(election.role(), Role::Leader);
        assert!(election.is_leader());
    }

    #[test]
    fn test_majority_threshold() {
        assert_eq!(LeaderElection::majority_threshold(1), 1);
        assert_eq!(LeaderElection::majority_threshold(2), 1);
        assert_eq!(LeaderElection::majority_threshold(3), 2);
        assert_eq!(LeaderElection::majority_threshold(5), 3);
        assert_eq!(LeaderElection::majority_threshold(7), 4);
    }

    #[test]
    fn test_leader_failover() {
        let (election, _rx) = create_election("peer1");
        election.handle_heartbeat(1, "peer2");
        assert_eq!(election.leader_id().unwrap().as_str(), "peer2");

        election.handle_peer_left(&PeerId::new("peer2"));
        assert!(election.leader_id().is_none());
    }

    #[test]
    fn test_split_brain_recovery() {
        let (e1, _rx1) = create_election("peer_a");
        e1.start_election(0);
        assert!(e1.is_leader());
        let term_a = e1.current_term();

        let (e2, _rx2) = create_election("peer_b");
        e2.start_election(0);
        assert!(e2.is_leader());
        let term_b = e2.current_term();

        assert_eq!(term_a, term_b);

        let accepted = e1.handle_heartbeat(term_b, "peer_b");
        assert!(accepted);
        assert_eq!(e1.role(), Role::Follower);
        assert_eq!(e1.leader_id().unwrap().as_str(), "peer_b");
    }

    #[test]
    fn test_vote_rejected_lower_term() {
        let (election, _rx) = create_election("peer1");
        election.handle_heartbeat(5, "peer2");
        let (resp_term, granted) = election.handle_request_vote(3, "peer3");
        assert_eq!(resp_term, 5);
        assert!(!granted);
    }

    #[test]
    fn test_vote_rejected_already_voted() {
        let (election, _rx) = create_election("peer1");
        let (_, granted1) = election.handle_request_vote(1, "peer2");
        assert!(granted1);
        let (_, granted2) = election.handle_request_vote(1, "peer3");
        assert!(!granted2);
    }

    #[test]
    fn test_manual_override_blocks_election() {
        let (election, _rx) = create_election("peer1");
        election.set_leader(&PeerId::new("peer2"));
        assert!(election.state.read().manual_override);

        let result = election.start_election(2);
        assert!(result.is_none());
        assert_ne!(election.role(), Role::Candidate);
    }

    #[test]
    fn test_enable_auto_election_preserves_override_with_leader() {
        let (election, _rx) = create_election("peer1");
        election.set_leader(&PeerId::new("peer2"));

        election.enable_auto_election(true);
        assert!(election.is_auto_election_enabled());
        assert!(election.state.read().manual_override);
    }

    #[test]
    fn test_enable_auto_election_after_leader_gone() {
        let (election, _rx) = create_election("peer1");
        election.set_leader(&PeerId::new("peer2"));
        election.handle_peer_left(&PeerId::new("peer2"));

        election.enable_auto_election(true);
        assert!(election.is_auto_election_enabled());
        assert!(!election.state.read().manual_override);
    }

    #[test]
    fn test_leader_assign() {
        let (election, _rx) = create_election("peer1");
        election.handle_leader_assign(5, "peer3", "peer2");
        assert_eq!(election.leader_id().unwrap().as_str(), "peer3");
        assert_eq!(election.current_term(), 5);
        assert_eq!(election.role(), Role::Follower);
        assert!(election.state.read().manual_override);
    }
}
