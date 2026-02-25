use crate::error::NetworkError;

pub const PROTOCOL_VERSION: u16 = 1;
pub const MAX_MESSAGE_SIZE: u32 = 1024 * 1024;
pub const MAX_CONNECTIONS: usize = 64;
pub const HANDSHAKE_TIMEOUT_MS: u64 = 5000;

#[derive(Clone, Copy, Debug)]
pub struct NetworkConfig {
    pub compression_threshold: u32,
    pub heartbeat_interval_ms: u64,
    pub election_timeout_min_ms: u64,
    pub election_timeout_max_ms: u64,
    pub heartbeat_timeout_multiplier: u32,
    pub reconnect_initial_ms: u64,
    pub reconnect_max_ms: u64,
    pub send_queue_capacity: usize,
    pub centralized_auto_forward: bool,
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
    pub fn validate(&self) -> Result<(), NetworkError> {
        let err = |msg: &str| Err(NetworkError::InvalidArgument(msg.into()));

        if self.heartbeat_interval_ms == 0 {
            return err("heartbeat_interval_ms must be > 0");
        }
        if self.election_timeout_min_ms <= self.heartbeat_interval_ms {
            return err("election_timeout_min_ms must be > heartbeat_interval_ms");
        }
        if self.election_timeout_min_ms > self.election_timeout_max_ms {
            return err("election_timeout_min_ms must be <= election_timeout_max_ms");
        }
        if self.heartbeat_timeout_multiplier == 0 {
            return err("heartbeat_timeout_multiplier must be > 0");
        }
        if self.reconnect_initial_ms == 0 {
            return err("reconnect_initial_ms must be > 0");
        }
        if self.reconnect_initial_ms > self.reconnect_max_ms {
            return err("reconnect_initial_ms must be <= reconnect_max_ms");
        }
        if self.send_queue_capacity == 0 {
            return err("send_queue_capacity must be > 0");
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
