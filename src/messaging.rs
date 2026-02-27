//! Packet definitions and helpers for encoding/decoding user and
//! internal messages, including optional compression.

use bytes::Bytes;

use crate::error::NetworkError;
use crate::protocol::{InternalMessage, msg_types};

/// In‑memory representation of a network packet. This is the
/// wire format used by `TransportManager` after framing/de-framing.
///
/// - `msg_type` is either an internal message code (<0x0100) or a
///   user message identifier.
/// - `flags` contains a compression bit plus seven user‑definable
///   flag bits.
/// - `payload` holds either raw bytes or the serialized
///   `InternalMessage` when `msg_type` is internal.
#[derive(Clone, Debug)]
pub struct RawPacket {
    pub msg_type: u16,
    pub flags: u8,
    pub payload: Bytes,
}

impl RawPacket {
    /// Was the payload compressed by `encode_packet`? Checks the low
    /// bit of `flags` which is reserved for compression.
    pub fn is_compressed(&self) -> bool {
        self.flags & 0x01 != 0
    }

    /// Does the packet carry an internal protocol message rather
    /// than a user payload? Internal messages have type codes below
    /// `USER_MESSAGE_START`.
    pub fn is_internal(&self) -> bool {
        self.msg_type < msg_types::USER_MESSAGE_START
    }
}

/// Build a user packet, optionally compressing the payload if it
/// exceeds `compression_threshold`. The caller provides `user_flags`;
/// the low bit is reserved and will be cleared.
pub fn encode_packet(
    msg_type: u16,
    data: &[u8],
    user_flags: u8,
    compression_threshold: u32,
    max_message_size: u32,
) -> Result<RawPacket, NetworkError> {
    // enforce user message range; internal messages use
    // `encode_internal`.
    if msg_type < msg_types::USER_MESSAGE_START {
        return Err(NetworkError::InvalidArgument(
            "msg_type must be >= 0x0100 for user messages".into(),
        ));
    }

    if data.len() as u32 > max_message_size {
        return Err(NetworkError::MessageTooLarge(data.len() as u32));
    }

    let clean_flags = user_flags & 0xFE; // drop compression bit

    if data.len() as u32 > compression_threshold {
        let compressed = lz4_flex::block::compress_prepend_size(data);
        if compressed.len() < data.len() {
            return Ok(RawPacket {
                msg_type,
                flags: clean_flags | 0x01, // set compression bit
                payload: Bytes::from(compressed),
            });
        }
    }

    Ok(RawPacket {
        msg_type,
        flags: clean_flags,
        payload: Bytes::copy_from_slice(data),
    })
}

/// Return the uncompressed payload bytes. If the packet has the
/// compression flag set, decompress using LZ4; otherwise clone the
/// existing buffer.
pub fn decode_payload(packet: &RawPacket) -> Result<Bytes, NetworkError> {
    if packet.is_compressed() {
        lz4_flex::block::decompress_size_prepended(&packet.payload)
            .map(Bytes::from)
            .map_err(|e| NetworkError::Internal(format!("LZ4 decompression failed: {e}")))
    } else {
        Ok(packet.payload.clone())
    }
}

/// Serialize one of the internal protocol messages into a
/// `RawPacket` with an appropriate `msg_type` code. Internal
/// packets are never compressed and always use flag=0.
pub fn encode_internal(
    msg: &InternalMessage,
    max_message_size: u32,
) -> Result<RawPacket, NetworkError> {
    let payload = postcard::to_allocvec(msg)?;

    if payload.len() as u32 > max_message_size {
        return Err(NetworkError::MessageTooLarge(payload.len() as u32));
    }

    let msg_type = match msg {
        InternalMessage::Handshake { .. } => msg_types::HANDSHAKE,
        InternalMessage::HandshakeAck { .. } => msg_types::HANDSHAKE_ACK,
        InternalMessage::PeerLeave { .. } => msg_types::PEER_LEAVE,
        InternalMessage::PeerListSync { .. } => msg_types::PEER_LIST_SYNC,
        InternalMessage::Heartbeat { .. } => msg_types::HEARTBEAT,
        InternalMessage::HeartbeatResponse { .. } => msg_types::HEARTBEAT_RESPONSE,
        InternalMessage::Ping { .. } => msg_types::PING,
        InternalMessage::Pong { .. } => msg_types::PONG,
        InternalMessage::RequestVote { .. } => msg_types::REQUEST_VOTE,
        InternalMessage::VoteResponse { .. } => msg_types::VOTE_RESPONSE,
        InternalMessage::LeaderAssign { .. } => msg_types::LEADER_ASSIGN,
        InternalMessage::ForwardedUserData { .. } => msg_types::FORWARDED_USER_DATA,
    };

    Ok(RawPacket {
        msg_type,
        flags: 0,
        payload: Bytes::from(payload),
    })
}

/// Decode a packet that was known to be internal back into the
/// `InternalMessage` enum.
pub fn decode_internal(packet: &RawPacket) -> Result<InternalMessage, NetworkError> {
    postcard::from_bytes(&packet.payload).map_err(NetworkError::Serialization)
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::config::NetworkConfig;

    #[test]
    fn test_packet_encode_decode() {
        let cfg = NetworkConfig::default();
        let data = b"hello world";
        let packet = encode_packet(
            0x0100,
            data,
            0,
            cfg.compression_threshold,
            cfg.max_message_size,
        )
        .unwrap();
        assert_eq!(packet.msg_type, 0x0100);
        assert!(!packet.is_compressed());
        let decoded = decode_payload(&packet).unwrap();
        assert_eq!(&decoded[..], data);
    }

    #[test]
    fn test_packet_compression() {
        let cfg = NetworkConfig::default();
        let data = vec![0x42; 2048];
        let packet = encode_packet(
            0x0100,
            &data,
            0,
            cfg.compression_threshold,
            cfg.max_message_size,
        )
        .unwrap();
        assert!(packet.is_compressed());
        assert!(packet.payload.len() < data.len());
        let decoded = decode_payload(&packet).unwrap();
        assert_eq!(&decoded[..], data);
    }

    #[test]
    fn test_compression_not_beneficial() {
        let cfg = NetworkConfig::default();
        // Random-like data that LZ4 cannot compress effectively
        let data: Vec<u8> = (0u32..600)
            .map(|i| (i.wrapping_mul(2654435761) >> 24) as u8)
            .collect();
        let packet = encode_packet(
            0x0100,
            &data,
            0,
            cfg.compression_threshold,
            cfg.max_message_size,
        )
        .unwrap();
        // LZ4 compressed output is larger than input for random data, so no compression
        assert!(
            !packet.is_compressed(),
            "compression should not be used when result is larger"
        );
        let decoded = decode_payload(&packet).unwrap();
        assert_eq!(&decoded[..], &data[..]);
    }

    #[test]
    fn test_user_msg_type_validation() {
        let cfg = NetworkConfig::default();
        let result = encode_packet(
            0x0001,
            b"test",
            0,
            cfg.compression_threshold,
            cfg.max_message_size,
        );
        assert!(result.is_err());
        let result = encode_packet(
            0x00FF,
            b"test",
            0,
            cfg.compression_threshold,
            cfg.max_message_size,
        );
        assert!(result.is_err());
        let result = encode_packet(
            0x0100,
            b"test",
            0,
            cfg.compression_threshold,
            cfg.max_message_size,
        );
        assert!(result.is_ok());
    }

    #[test]
    fn test_flags_compression_bit() {
        let cfg = NetworkConfig::default();
        let packet = encode_packet(
            0x0100,
            b"small",
            0b1111_1111,
            cfg.compression_threshold,
            cfg.max_message_size,
        )
        .unwrap();
        assert_eq!(packet.flags & 0x01, 0);
        assert_eq!(packet.flags & 0xFE, 0xFE);

        let data = vec![0x42; 2048];
        let packet = encode_packet(
            0x0100,
            &data,
            0b0000_0110,
            cfg.compression_threshold,
            cfg.max_message_size,
        )
        .unwrap();
        assert!(packet.is_compressed(), "2048 bytes of 0x42 should compress");
        assert_eq!(packet.flags & 0x01, 0x01);
        assert_eq!(packet.flags & 0xFE, 0b0000_0110);
    }

    #[test]
    fn test_message_too_large() {
        let cfg = NetworkConfig::default();
        let max = cfg.max_message_size;
        let data = vec![0; max as usize + 1];
        let result = encode_packet(0x0100, &data, 0, cfg.compression_threshold, max);
        assert!(matches!(result, Err(NetworkError::MessageTooLarge(_))));
    }

    #[test]
    fn test_large_message_near_limit() {
        let cfg = NetworkConfig::default();
        let max = cfg.max_message_size;
        let data = vec![0x42; max as usize];
        let packet = encode_packet(0x0100, &data, 0, max + 1, max).unwrap();
        let decoded = decode_payload(&packet).unwrap();
        assert_eq!(&decoded[..], &data[..]);
    }

    #[test]
    fn test_compression_threshold_boundary() {
        let cfg = NetworkConfig::default();
        let thresh = cfg.compression_threshold;
        let max = cfg.max_message_size;
        let data = vec![0xAA; thresh as usize];
        let packet_at = encode_packet(0x0100, &data, 0, thresh, max).unwrap();
        assert!(!packet_at.is_compressed());

        let data = vec![0xAA; thresh as usize + 1];
        let packet_above = encode_packet(0x0100, &data, 0, thresh, max).unwrap();
        assert!(
            packet_above.is_compressed(),
            "data above threshold should be compressed when beneficial"
        );
        let decoded = decode_payload(&packet_above).unwrap();
        assert_eq!(&decoded[..], &data[..]);
    }

    #[test]
    fn test_internal_message_serde() {
        let max = NetworkConfig::default().max_message_size;
        let messages = vec![
            InternalMessage::Handshake {
                peer_id: "peer1".into(),
                listen_port: 8080,
                protocol_version: 1,
                session_id: "session1".into(),
            },
            InternalMessage::HandshakeAck {
                peer_id: "peer2".into(),
                listen_port: 9090,
                success: true,
                error_reason: None,
            },
            InternalMessage::PeerLeave {
                peer_id: "peer1".into(),
            },
            InternalMessage::PeerListSync {
                peers: vec![("peer1".into(), "127.0.0.1:8080".into())],
            },
            InternalMessage::Heartbeat {
                term: 1,
                leader_id: "peer1".into(),
                timestamp_ms: 12345,
            },
            InternalMessage::HeartbeatResponse {
                term: 1,
                timestamp_ms: 12345,
            },
            InternalMessage::Ping {
                timestamp_ms: 12345,
            },
            InternalMessage::Pong {
                timestamp_ms: 12345,
            },
            InternalMessage::RequestVote {
                term: 2,
                candidate_id: "peer1".into(),
            },
            InternalMessage::VoteResponse {
                term: 2,
                voter_id: "peer2".into(),
                granted: true,
            },
            InternalMessage::LeaderAssign {
                term: 3,
                leader_id: "peer1".into(),
                assigner_id: "peer2".into(),
            },
            InternalMessage::ForwardedUserData {
                from_peer_id: "peer1".into(),
                original_msg_type: 0x0100,
                original_flags: 0x02,
                payload: vec![1, 2, 3],
            },
        ];

        for msg in &messages {
            let packet = encode_internal(msg, max).unwrap();
            assert!(packet.is_internal());
            let decoded = decode_internal(&packet).unwrap();
            let re_encoded = encode_internal(&decoded, max).unwrap();
            assert_eq!(packet.payload, re_encoded.payload);
            assert_eq!(packet.msg_type, re_encoded.msg_type);
        }
    }
}
