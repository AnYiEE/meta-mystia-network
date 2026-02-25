use bytes::Bytes;

use crate::config::MAX_MESSAGE_SIZE;
use crate::error::NetworkError;
use crate::protocol::{InternalMessage, msg_types};

#[derive(Clone, Debug)]
pub struct RawPacket {
    pub msg_type: u16,
    pub flags: u8,
    pub payload: Bytes,
}

impl RawPacket {
    pub fn is_compressed(&self) -> bool {
        self.flags & 0x01 != 0
    }

    pub fn is_internal(&self) -> bool {
        self.msg_type < msg_types::USER_MESSAGE_START
    }
}

pub fn encode_packet(
    msg_type: u16,
    data: &[u8],
    user_flags: u8,
    compression_threshold: u32,
) -> Result<RawPacket, NetworkError> {
    if msg_type < msg_types::USER_MESSAGE_START {
        return Err(NetworkError::InvalidArgument(
            "msg_type must be >= 0x0100 for user messages".into(),
        ));
    }

    if data.len() as u32 > MAX_MESSAGE_SIZE {
        return Err(NetworkError::MessageTooLarge(data.len() as u32));
    }

    let clean_flags = user_flags & 0xFE;

    if data.len() as u32 > compression_threshold {
        let compressed = lz4_flex::block::compress_prepend_size(data);
        if compressed.len() < data.len() {
            return Ok(RawPacket {
                msg_type,
                flags: clean_flags | 0x01,
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

pub fn decode_payload(packet: &RawPacket) -> Result<Bytes, NetworkError> {
    if packet.is_compressed() {
        lz4_flex::block::decompress_size_prepended(&packet.payload)
            .map(Bytes::from)
            .map_err(|e| NetworkError::Internal(format!("LZ4 decompression failed: {e}")))
    } else {
        Ok(packet.payload.clone())
    }
}

pub fn encode_internal(msg: &InternalMessage) -> Result<RawPacket, NetworkError> {
    let payload = postcard::to_allocvec(msg)?;

    if payload.len() as u32 > MAX_MESSAGE_SIZE {
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

pub fn decode_internal(packet: &RawPacket) -> Result<InternalMessage, NetworkError> {
    postcard::from_bytes(&packet.payload).map_err(NetworkError::Serialization)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_packet_encode_decode() {
        let data = b"hello world";
        let packet = encode_packet(0x0100, data, 0, 512).unwrap();
        assert_eq!(packet.msg_type, 0x0100);
        assert!(!packet.is_compressed());
        let decoded = decode_payload(&packet).unwrap();
        assert_eq!(&decoded[..], data);
    }

    #[test]
    fn test_packet_compression() {
        let data = vec![0x42; 2048];
        let packet = encode_packet(0x0100, &data, 0, 512).unwrap();
        assert!(packet.is_compressed());
        assert!(packet.payload.len() < data.len());
        let decoded = decode_payload(&packet).unwrap();
        assert_eq!(&decoded[..], data);
    }

    #[test]
    fn test_compression_not_beneficial() {
        // Random-like data that LZ4 cannot compress effectively
        let data: Vec<u8> = (0u32..600)
            .map(|i| (i.wrapping_mul(2654435761) >> 24) as u8)
            .collect();
        let packet = encode_packet(0x0100, &data, 0, 512).unwrap();
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
        let result = encode_packet(0x0001, b"test", 0, 512);
        assert!(result.is_err());
        let result = encode_packet(0x00FF, b"test", 0, 512);
        assert!(result.is_err());
        let result = encode_packet(0x0100, b"test", 0, 512);
        assert!(result.is_ok());
    }

    #[test]
    fn test_flags_compression_bit() {
        let packet = encode_packet(0x0100, b"small", 0b1111_1111, 512).unwrap();
        assert_eq!(packet.flags & 0x01, 0);
        assert_eq!(packet.flags & 0xFE, 0xFE);

        let data = vec![0x42; 2048];
        let packet = encode_packet(0x0100, &data, 0b0000_0110, 512).unwrap();
        assert!(packet.is_compressed(), "2048 bytes of 0x42 should compress");
        assert_eq!(packet.flags & 0x01, 0x01);
        assert_eq!(packet.flags & 0xFE, 0b0000_0110);
    }

    #[test]
    fn test_message_too_large() {
        let data = vec![0; MAX_MESSAGE_SIZE as usize + 1];
        let result = encode_packet(0x0100, &data, 0, 512);
        assert!(matches!(result, Err(NetworkError::MessageTooLarge(_))));
    }

    #[test]
    fn test_large_message_near_limit() {
        let data = vec![0x42; MAX_MESSAGE_SIZE as usize];
        let packet = encode_packet(0x0100, &data, 0, MAX_MESSAGE_SIZE + 1).unwrap();
        let decoded = decode_payload(&packet).unwrap();
        assert_eq!(&decoded[..], &data[..]);
    }

    #[test]
    fn test_compression_threshold_boundary() {
        let data = vec![0xAA; 512];
        let packet_at = encode_packet(0x0100, &data, 0, 512).unwrap();
        assert!(!packet_at.is_compressed());

        let data = vec![0xAA; 513];
        let packet_above = encode_packet(0x0100, &data, 0, 512).unwrap();
        assert!(
            packet_above.is_compressed(),
            "data above threshold should be compressed when beneficial"
        );
        let decoded = decode_payload(&packet_above).unwrap();
        assert_eq!(&decoded[..], &data[..]);
    }

    #[test]
    fn test_internal_message_serde() {
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
            let packet = encode_internal(msg).unwrap();
            assert!(packet.is_internal());
            let decoded = decode_internal(&packet).unwrap();
            let re_encoded = encode_internal(&decoded).unwrap();
            assert_eq!(packet.payload, re_encoded.payload);
            assert_eq!(packet.msg_type, re_encoded.msg_type);
        }
    }
}
