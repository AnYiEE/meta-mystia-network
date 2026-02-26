//! Configuration constants and runtime parameters for the network
//! library.

use crate::error::NetworkError;

/// Protocol version number baked into the handshake.
pub const PROTOCOL_VERSION: u16 = 1;

/// Maximum size (bytes) of a single user message payload.
pub const MAX_MESSAGE_SIZE: u32 = 1024 * 1024;

/// Maximum number of simultaneous TCP connections allowed.
pub const MAX_CONNECTIONS: usize = 64;

/// Timeout (ms) to complete the TCP handshake exchange.
pub const HANDSHAKE_TIMEOUT_MS: u64 = 5000;

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

    // --- feature toggles -------------------------------------------------
    /// if `true`, leader will automatically forward incoming
    /// broadcast messages when operating in centralized mode
    pub centralized_auto_forward: bool,
    /// enable automatic leader election when current leader
    /// becomes unresponsive
    pub auto_election_enabled: bool,
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
            send_queue_capacity: 1024,
            centralized_auto_forward: true,
            auto_election_enabled: true,
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
    fn test_config_validation() {
        let mut config = NetworkConfig::default();

        config.heartbeat_interval_ms = 0;
        assert!(config.validate().is_err());
        config.heartbeat_interval_ms = 500;

        config.election_timeout_min_ms = 500;
        assert!(config.validate().is_err());
        config.election_timeout_min_ms = 1500;

        config.election_timeout_max_ms = 1000;
        assert!(config.validate().is_err());
        config.election_timeout_max_ms = 3000;

        config.heartbeat_timeout_multiplier = 0;
        assert!(config.validate().is_err());
        config.heartbeat_timeout_multiplier = 3;

        config.reconnect_initial_ms = 0;
        assert!(config.validate().is_err());
        config.reconnect_initial_ms = 1000;

        config.reconnect_max_ms = 500;
        assert!(config.validate().is_err());
        config.reconnect_max_ms = 30000;

        config.send_queue_capacity = 0;
        assert!(config.validate().is_err());
    }
}
