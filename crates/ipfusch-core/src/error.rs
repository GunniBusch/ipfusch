use thiserror::Error;

pub type Result<T> = std::result::Result<T, IpfuschError>;

#[derive(Debug, Error)]
pub enum IpfuschError {
    #[error("invalid packet header")]
    InvalidPacketHeader,
    #[error("protocol mismatch: expected {expected}, got {got}")]
    ProtocolVersionMismatch { expected: u16, got: u16 },
    #[error("auth token rejected")]
    AuthRejected,
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("serialization error: {0}")]
    Serialization(String),
    #[error("timeout waiting for packet response")]
    Timeout,
}

impl From<bincode::Error> for IpfuschError {
    fn from(value: bincode::Error) -> Self {
        Self::Serialization(value.to_string())
    }
}
