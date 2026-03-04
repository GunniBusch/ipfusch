use crate::error::{IpfuschError, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::io::{Read, Write};

pub const PROTOCOL_VERSION: u16 = 1;
pub const DATA_HEADER_SIZE: usize = 40;
pub const DATA_MAGIC: u32 = 0x4950_4653; // IPFS(H)

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum ControlMessage {
    Hello {
        version: u16,
        client_name: String,
        token_hash: Option<[u8; 32]>,
        capabilities: Vec<String>,
    },
    StartSession {
        session_id: u64,
        transport: String,
        streams: u16,
        duration_ms: u64,
        payload_bytes: usize,
        interval_ms: u64,
        rate_bps: Option<u64>,
    },
    Ack {
        session_id: u64,
        accepted: bool,
        reason: Option<String>,
    },
    StopSession {
        session_id: u64,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DataHeader {
    pub magic: u32,
    pub version: u16,
    pub flags: u16,
    pub session_id: u64,
    pub stream_id: u32,
    pub seq: u64,
    pub send_mono_ns: u64,
    pub payload_len: u32,
}

impl DataHeader {
    pub fn new(
        session_id: u64,
        stream_id: u32,
        seq: u64,
        send_mono_ns: u64,
        payload_len: u32,
    ) -> Self {
        Self {
            magic: DATA_MAGIC,
            version: PROTOCOL_VERSION,
            flags: 0,
            session_id,
            stream_id,
            seq,
            send_mono_ns,
            payload_len,
        }
    }

    pub fn encode(self) -> [u8; DATA_HEADER_SIZE] {
        let mut buf = [0u8; DATA_HEADER_SIZE];
        buf[0..4].copy_from_slice(&self.magic.to_le_bytes());
        buf[4..6].copy_from_slice(&self.version.to_le_bytes());
        buf[6..8].copy_from_slice(&self.flags.to_le_bytes());
        buf[8..16].copy_from_slice(&self.session_id.to_le_bytes());
        buf[16..20].copy_from_slice(&self.stream_id.to_le_bytes());
        buf[20..28].copy_from_slice(&self.seq.to_le_bytes());
        buf[28..36].copy_from_slice(&self.send_mono_ns.to_le_bytes());
        buf[36..40].copy_from_slice(&self.payload_len.to_le_bytes());
        buf
    }

    pub fn decode(buf: &[u8]) -> Result<Self> {
        if buf.len() != DATA_HEADER_SIZE {
            return Err(IpfuschError::InvalidPacketHeader);
        }

        let magic = u32::from_le_bytes(buf[0..4].try_into().expect("slice size checked"));
        let version = u16::from_le_bytes(buf[4..6].try_into().expect("slice size checked"));
        if magic != DATA_MAGIC {
            return Err(IpfuschError::InvalidPacketHeader);
        }
        if version != PROTOCOL_VERSION {
            return Err(IpfuschError::ProtocolVersionMismatch {
                expected: PROTOCOL_VERSION,
                got: version,
            });
        }

        Ok(Self {
            magic,
            version,
            flags: u16::from_le_bytes(buf[6..8].try_into().expect("slice size checked")),
            session_id: u64::from_le_bytes(buf[8..16].try_into().expect("slice size checked")),
            stream_id: u32::from_le_bytes(buf[16..20].try_into().expect("slice size checked")),
            seq: u64::from_le_bytes(buf[20..28].try_into().expect("slice size checked")),
            send_mono_ns: u64::from_le_bytes(buf[28..36].try_into().expect("slice size checked")),
            payload_len: u32::from_le_bytes(buf[36..40].try_into().expect("slice size checked")),
        })
    }
}

pub fn hash_token(token: &str) -> [u8; 32] {
    let mut hasher = Sha256::new();
    hasher.update(token.as_bytes());
    hasher.finalize().into()
}

pub fn write_control_frame<W: Write>(mut writer: W, msg: &ControlMessage) -> Result<()> {
    let body = bincode::serialize(msg)?;
    let len = u32::try_from(body.len())
        .map_err(|_| IpfuschError::Serialization("control frame too large".into()))?;
    writer.write_all(&len.to_le_bytes())?;
    writer.write_all(&body)?;
    Ok(())
}

pub fn read_control_frame<R: Read>(mut reader: R) -> Result<ControlMessage> {
    let mut len_buf = [0u8; 4];
    reader.read_exact(&mut len_buf)?;
    let len = u32::from_le_bytes(len_buf) as usize;
    let mut payload = vec![0u8; len];
    reader.read_exact(&mut payload)?;
    Ok(bincode::deserialize(&payload)?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn data_header_roundtrip() {
        let header = DataHeader::new(7, 2, 99, 123_456, 1400);
        let encoded = header.encode();
        let decoded = DataHeader::decode(&encoded).expect("decode must succeed");
        assert_eq!(header, decoded);
    }

    #[test]
    fn control_roundtrip() {
        let msg = ControlMessage::Hello {
            version: PROTOCOL_VERSION,
            client_name: "ipfusch-test".to_string(),
            token_hash: Some(hash_token("abc")),
            capabilities: vec!["tcp".to_string(), "udp".to_string()],
        };
        let mut bytes = Vec::new();
        write_control_frame(&mut bytes, &msg).expect("serialize");
        let got = read_control_frame(bytes.as_slice()).expect("deserialize");
        match got {
            ControlMessage::Hello { client_name, .. } => assert_eq!(client_name, "ipfusch-test"),
            _ => panic!("unexpected message type"),
        }
    }
}
