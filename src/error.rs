use std::fmt;
use std::io;

pub(crate) mod error_codes {
    pub const OK: i32 = 0;
    pub const NOT_INITIALIZED: i32 = -1;
    pub const ALREADY_INITIALIZED: i32 = -2;
    pub const INVALID_ARGUMENT: i32 = -3;
    pub const CONNECTION_FAILED: i32 = -4;
    pub const PEER_NOT_FOUND: i32 = -5;
    pub const NOT_LEADER: i32 = -6;
    pub const SEND_QUEUE_FULL: i32 = -7;
    pub const MESSAGE_TOO_LARGE: i32 = -8;
    pub const SERIALIZATION_ERROR: i32 = -9;
    pub const SESSION_MISMATCH: i32 = -10;
    pub const DUPLICATE_PEER_ID: i32 = -11;
    pub const VERSION_MISMATCH: i32 = -12;
    pub const MAX_CONNECTIONS_REACHED: i32 = -13;
    pub const INTERNAL_ERROR: i32 = -99;
}

#[derive(Debug)]
pub enum NetworkError {
    NotInitialized,
    AlreadyInitialized,
    InvalidArgument(String),
    Io(io::Error),
    ConnectionFailed(String),
    PeerNotFound(String),
    NotLeader,
    SendQueueFull,
    MessageTooLarge(u32),
    Serialization(postcard::Error),
    SessionMismatch { expected: String, got: String },
    DuplicatePeerId(String),
    VersionMismatch { expected: u16, got: u16 },
    MaxConnectionsReached,
    HandshakeFailed(String),
    HandshakeTimeout,
    NotImplemented,
    Internal(String),
}

impl NetworkError {
    pub fn error_code(&self) -> i32 {
        match self {
            Self::NotInitialized => error_codes::NOT_INITIALIZED,
            Self::AlreadyInitialized => error_codes::ALREADY_INITIALIZED,
            Self::InvalidArgument(_) => error_codes::INVALID_ARGUMENT,
            Self::Io(_)
            | Self::ConnectionFailed(_)
            | Self::HandshakeFailed(_)
            | Self::HandshakeTimeout => error_codes::CONNECTION_FAILED,
            Self::PeerNotFound(_) => error_codes::PEER_NOT_FOUND,
            Self::NotLeader => error_codes::NOT_LEADER,
            Self::SendQueueFull => error_codes::SEND_QUEUE_FULL,
            Self::MessageTooLarge(_) => error_codes::MESSAGE_TOO_LARGE,
            Self::Serialization(_) => error_codes::SERIALIZATION_ERROR,
            Self::SessionMismatch { .. } => error_codes::SESSION_MISMATCH,
            Self::DuplicatePeerId(_) => error_codes::DUPLICATE_PEER_ID,
            Self::VersionMismatch { .. } => error_codes::VERSION_MISMATCH,
            Self::MaxConnectionsReached => error_codes::MAX_CONNECTIONS_REACHED,
            Self::NotImplemented | Self::Internal(_) => error_codes::INTERNAL_ERROR,
        }
    }
}

impl fmt::Display for NetworkError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotInitialized => write!(f, "network not initialized"),
            Self::AlreadyInitialized => write!(f, "network already initialized"),
            Self::InvalidArgument(msg) => write!(f, "invalid argument: {msg}"),
            Self::Io(err) => write!(f, "IO error: {err}"),
            Self::ConnectionFailed(msg) => write!(f, "connection failed: {msg}"),
            Self::PeerNotFound(id) => write!(f, "peer not found: {id}"),
            Self::NotLeader => write!(f, "not leader"),
            Self::SendQueueFull => write!(f, "send queue full"),
            Self::MessageTooLarge(size) => write!(f, "message too large: {size} bytes"),
            Self::Serialization(err) => write!(f, "serialization error: {err}"),
            Self::SessionMismatch { expected, got } => {
                write!(f, "session mismatch: expected {expected}, got {got}")
            }
            Self::DuplicatePeerId(id) => write!(f, "duplicate peer id: {id}"),
            Self::VersionMismatch { expected, got } => {
                write!(f, "version mismatch: expected {expected}, got {got}")
            }
            Self::MaxConnectionsReached => write!(f, "max connections reached"),
            Self::HandshakeFailed(msg) => write!(f, "handshake failed: {msg}"),
            Self::HandshakeTimeout => write!(f, "handshake timeout"),
            Self::NotImplemented => write!(f, "not implemented"),
            Self::Internal(msg) => write!(f, "internal error: {msg}"),
        }
    }
}

impl std::error::Error for NetworkError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(err) => Some(err),
            Self::Serialization(err) => Some(err),
            _ => None,
        }
    }
}

impl From<io::Error> for NetworkError {
    fn from(err: io::Error) -> Self {
        Self::Io(err)
    }
}

impl From<postcard::Error> for NetworkError {
    fn from(err: postcard::Error) -> Self {
        Self::Serialization(err)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_error_code_mapping() {
        assert_eq!(
            NetworkError::NotInitialized.error_code(),
            error_codes::NOT_INITIALIZED
        );
        assert_eq!(
            NetworkError::AlreadyInitialized.error_code(),
            error_codes::ALREADY_INITIALIZED
        );
        assert_eq!(
            NetworkError::InvalidArgument("test".into()).error_code(),
            error_codes::INVALID_ARGUMENT
        );
        assert_eq!(
            NetworkError::Io(io::Error::new(io::ErrorKind::Other, "test")).error_code(),
            error_codes::CONNECTION_FAILED
        );
        assert_eq!(
            NetworkError::ConnectionFailed("test".into()).error_code(),
            error_codes::CONNECTION_FAILED
        );
        assert_eq!(
            NetworkError::PeerNotFound("test".into()).error_code(),
            error_codes::PEER_NOT_FOUND
        );
        assert_eq!(
            NetworkError::NotLeader.error_code(),
            error_codes::NOT_LEADER
        );
        assert_eq!(
            NetworkError::SendQueueFull.error_code(),
            error_codes::SEND_QUEUE_FULL
        );
        assert_eq!(
            NetworkError::MessageTooLarge(0).error_code(),
            error_codes::MESSAGE_TOO_LARGE
        );
        assert_eq!(
            NetworkError::SessionMismatch {
                expected: "a".into(),
                got: "b".into()
            }
            .error_code(),
            error_codes::SESSION_MISMATCH
        );
        assert_eq!(
            NetworkError::DuplicatePeerId("test".into()).error_code(),
            error_codes::DUPLICATE_PEER_ID
        );
        assert_eq!(
            NetworkError::VersionMismatch {
                expected: 1,
                got: 2
            }
            .error_code(),
            error_codes::VERSION_MISMATCH
        );
        assert_eq!(
            NetworkError::MaxConnectionsReached.error_code(),
            error_codes::MAX_CONNECTIONS_REACHED
        );
        assert_eq!(
            NetworkError::HandshakeFailed("test".into()).error_code(),
            error_codes::CONNECTION_FAILED
        );
        assert_eq!(
            NetworkError::HandshakeTimeout.error_code(),
            error_codes::CONNECTION_FAILED
        );
        assert_eq!(
            NetworkError::Serialization(postcard::Error::DeserializeBadVarint).error_code(),
            error_codes::SERIALIZATION_ERROR
        );
        assert_eq!(
            NetworkError::NotImplemented.error_code(),
            error_codes::INTERNAL_ERROR
        );
        assert_eq!(
            NetworkError::Internal("test".into()).error_code(),
            error_codes::INTERNAL_ERROR
        );
    }

    #[test]
    fn test_error_display() {
        assert_eq!(
            NetworkError::NotInitialized.to_string(),
            "network not initialized"
        );
        assert_eq!(NetworkError::NotLeader.to_string(), "not leader");
        assert_eq!(NetworkError::SendQueueFull.to_string(), "send queue full");
        assert_eq!(
            NetworkError::HandshakeTimeout.to_string(),
            "handshake timeout"
        );
        assert_eq!(
            NetworkError::MaxConnectionsReached.to_string(),
            "max connections reached"
        );
        assert_eq!(NetworkError::NotImplemented.to_string(), "not implemented");

        let err = NetworkError::InvalidArgument("bad".into());
        assert!(err.to_string().contains("bad"));

        let err = NetworkError::MessageTooLarge(9999);
        assert!(err.to_string().contains("9999"));

        let err = NetworkError::SessionMismatch {
            expected: "a".into(),
            got: "b".into(),
        };
        let s = err.to_string();
        assert!(s.contains("a") && s.contains("b"));
    }

    #[test]
    fn test_error_from_io() {
        let io_err = io::Error::new(io::ErrorKind::ConnectionRefused, "refused");
        let err: NetworkError = io_err.into();
        assert!(matches!(err, NetworkError::Io(_)));
        assert_eq!(err.error_code(), error_codes::CONNECTION_FAILED);
    }

    #[test]
    fn test_error_from_postcard() {
        let pc_err = postcard::Error::DeserializeBadVarint;
        let err: NetworkError = pc_err.into();
        assert!(matches!(err, NetworkError::Serialization(_)));
        assert_eq!(err.error_code(), error_codes::SERIALIZATION_ERROR);
    }

    #[test]
    fn test_error_source() {
        use std::error::Error;

        let io_err = NetworkError::Io(io::Error::new(io::ErrorKind::Other, "test"));
        assert!(io_err.source().is_some());

        let ser_err = NetworkError::Serialization(postcard::Error::DeserializeBadVarint);
        assert!(ser_err.source().is_some());

        assert!(NetworkError::NotLeader.source().is_none());
    }
}
