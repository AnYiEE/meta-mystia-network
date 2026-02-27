//! Configuration constants and runtime parameters for the network
//! library.

use crate::error::NetworkError;

/// Protocol version number baked into the handshake.
pub const PROTOCOL_VERSION: u16 = 1;

/// Controls what happens when a manually-assigned leader goes
/// offline. Used only when `manual_override` is active in the
/// election module.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ManualOverrideRecovery {
    /// Keep `manual_override` active, do **not** start an automatic
    /// election. The upper layer is expected to call `SetLeader`
    /// again or take other recovery action. A leader-change
    /// notification with `None` is emitted so the application can
    /// react.
    Hold = 0,
    /// Clear `manual_override` and let the normal Raft-like
    /// election take over, choosing a new leader automatically.
    AutoElect = 1,
}

impl ManualOverrideRecovery {
    /// Convert from an FFI-friendly integer (0 = Hold, 1 = AutoElect).
    /// Returns `Hold` for any unrecognized value.
    pub fn from_u8(v: u8) -> Self {
        match v {
            1 => Self::AutoElect,
            _ => Self::Hold,
        }
    }
}

/// User‑tunable parameters that control timing, buffering,
/// and behavior of the peer‑to‑peer network.
///
/// Fields are grouped by purpose to make it easier to
/// configure and validate.
#[derive(Clone, Copy, Debug)]
pub struct NetworkConfig {
    // --- compression -----------------------------------------------------
    /// minimum payload size (bytes) before attempting LZ4
    /// compression. smaller packets are sent uncompressed.
    pub compression_threshold: u32,

    // --- heartbeats & election -------------------------------------------
    /// interval (ms) between automatic heartbeat broadcasts
    pub heartbeat_interval_ms: u64,
    /// lower bound for randomized election timeout (ms)
    pub election_timeout_min_ms: u64,
    /// upper bound for randomized election timeout (ms)
    pub election_timeout_max_ms: u64,
    /// multiplier applied to heartbeat interval when
    /// calculating member timeout detection
    pub heartbeat_timeout_multiplier: u32,

    // --- reconnection/backoff --------------------------------------------
    /// initial delay (ms) before attempting to reconnect
    pub reconnect_initial_ms: u64,
    /// maximum backoff delay (ms) for reconnection attempts
    pub reconnect_max_ms: u64,

    // --- buffers & capacities --------------------------------------------
    /// capacity of each peer's outgoing send queue
    pub send_queue_capacity: usize,
    /// maximum number of simultaneous TCP connections
    pub max_connections: usize,
    /// maximum size (bytes) of a single message payload.
    /// both user and internal messages are subject to this limit.
    pub max_message_size: u32,

    // --- timeouts --------------------------------------------------------
    /// timeout (ms) to complete the TCP handshake exchange
    pub handshake_timeout_ms: u64,

    // --- discovery -------------------------------------------------------
    /// UDP port for mDNS service discovery. Both publisher and
    /// browser must use the same port to communicate.
    pub mdns_port: u16,

    // --- feature toggles -------------------------------------------------
    /// if `true`, leader will automatically forward incoming
    /// broadcast messages when operating in centralized mode
    pub centralized_auto_forward: bool,
    /// enable automatic leader election when current leader
    /// becomes unresponsive
    pub auto_election_enabled: bool,
    /// behavior when a manually-assigned leader goes offline.
    /// only relevant when `manual_override` is active.
    pub manual_override_recovery: ManualOverrideRecovery,
}

impl Default for NetworkConfig {
    fn default() -> Self {
        Self {
            compression_threshold: 512,
            heartbeat_interval_ms: 500,
            election_timeout_min_ms: 1500,
            election_timeout_max_ms: 3000,
            heartbeat_timeout_multiplier: 3,
            reconnect_initial_ms: 1000,
            reconnect_max_ms: 30000,
            send_queue_capacity: 128,
            max_connections: 64,
            max_message_size: 256 * 1024,
            handshake_timeout_ms: 5000,
            mdns_port: 15353,
            centralized_auto_forward: true,
            auto_election_enabled: true,
            manual_override_recovery: ManualOverrideRecovery::Hold,
        }
    }
}

impl NetworkConfig {
    /// Ensure that the configuration values make sense; returns an
    /// error describing the first invalid field encountered.
    pub fn validate(&self) -> Result<(), NetworkError> {
        let e = |msg: &str| Err(NetworkError::InvalidArgument(msg.into()));

        if self.heartbeat_interval_ms == 0 {
            return e("heartbeat_interval_ms must be > 0");
        }
        if self.election_timeout_min_ms <= self.heartbeat_interval_ms {
            return e("election_timeout_min_ms must be > heartbeat_interval_ms");
        }
        if self.election_timeout_min_ms > self.election_timeout_max_ms {
            return e("election_timeout_min_ms must be <= election_timeout_max_ms");
        }
        if self.heartbeat_timeout_multiplier == 0 {
            return e("heartbeat_timeout_multiplier must be > 0");
        }
        if self.reconnect_initial_ms == 0 {
            return e("reconnect_initial_ms must be > 0");
        }
        if self.reconnect_initial_ms > self.reconnect_max_ms {
            return e("reconnect_initial_ms must be <= reconnect_max_ms");
        }
        if self.send_queue_capacity == 0 {
            return e("send_queue_capacity must be > 0");
        }
        if self.max_connections == 0 {
            return e("max_connections must be > 0");
        }
        if self.max_message_size == 0 {
            return e("max_message_size must be > 0");
        }
        if self.handshake_timeout_ms == 0 {
            return e("handshake_timeout_ms must be > 0");
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_config_default_valid() {
        assert!(NetworkConfig::default().validate().is_ok());
    }

    #[test]
    #[allow(clippy::field_reassign_with_default)]
    fn test_config_validation() {
        let d = NetworkConfig::default();
        let mut config = NetworkConfig::default();

        config.heartbeat_interval_ms = 0;
        assert!(config.validate().is_err());
        config.heartbeat_interval_ms = d.heartbeat_interval_ms;

        config.election_timeout_min_ms = 500;
        assert!(config.validate().is_err());
        config.election_timeout_min_ms = d.election_timeout_min_ms;

        config.election_timeout_max_ms = 1000;
        assert!(config.validate().is_err());
        config.election_timeout_max_ms = d.election_timeout_max_ms;

        config.heartbeat_timeout_multiplier = 0;
        assert!(config.validate().is_err());
        config.heartbeat_timeout_multiplier = d.heartbeat_timeout_multiplier;

        config.reconnect_initial_ms = 0;
        assert!(config.validate().is_err());
        config.reconnect_initial_ms = d.reconnect_initial_ms;

        config.reconnect_max_ms = 500;
        assert!(config.validate().is_err());
        config.reconnect_max_ms = d.reconnect_max_ms;

        config.send_queue_capacity = 0;
        assert!(config.validate().is_err());
    }

    #[test]
    fn test_manual_override_recovery_from_u8() {
        assert_eq!(
            ManualOverrideRecovery::from_u8(0),
            ManualOverrideRecovery::Hold
        );
        assert_eq!(
            ManualOverrideRecovery::from_u8(1),
            ManualOverrideRecovery::AutoElect
        );
        // Unrecognised values fall back to Hold
        assert_eq!(
            ManualOverrideRecovery::from_u8(2),
            ManualOverrideRecovery::Hold
        );
        assert_eq!(
            ManualOverrideRecovery::from_u8(255),
            ManualOverrideRecovery::Hold
        );
    }

    #[test]
    fn test_manual_override_recovery_default_in_config() {
        let cfg = NetworkConfig::default();
        assert_eq!(cfg.manual_override_recovery, ManualOverrideRecovery::Hold);
    }
}
