use serde::{Deserialize, Serialize};
use std::net::SocketAddr;
use std::str::FromStr;
use std::time::Duration;

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Hash)]
#[serde(rename_all = "lowercase")]
pub enum Transport {
    Tcp,
    Udp,
    Quic,
}

impl std::fmt::Display for Transport {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Tcp => f.write_str("tcp"),
            Self::Udp => f.write_str("udp"),
            Self::Quic => f.write_str("quic"),
        }
    }
}

impl FromStr for Transport {
    type Err = String;

    fn from_str(s: &str) -> std::result::Result<Self, Self::Err> {
        match s.to_ascii_lowercase().as_str() {
            "tcp" => Ok(Self::Tcp),
            "udp" => Ok(Self::Udp),
            "quic" => Ok(Self::Quic),
            _ => Err(format!("unsupported transport: {s}")),
        }
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ServerTransport {
    Tcp,
    Udp,
    Quic,
    All,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum SweepObjective {
    Throughput,
    MaxEfficiency,
    Balanced,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "mode", content = "bps", rename_all = "snake_case")]
pub enum RateMode {
    Auto,
    Bps(u64),
}

impl RateMode {
    pub fn as_bps(self) -> Option<u64> {
        match self {
            Self::Auto => None,
            Self::Bps(v) => Some(v),
        }
    }

    pub fn is_auto(self) -> bool {
        matches!(self, Self::Auto)
    }
}

#[derive(Debug, Clone)]
pub struct RunConfig {
    pub host: SocketAddr,
    pub transport: Transport,
    pub streams: u16,
    pub duration: Duration,
    pub warmup: Duration,
    pub payload_bytes: usize,
    pub rate: RateMode,
    pub token: Option<String>,
    pub interval: Duration,
}

#[derive(Debug, Clone)]
pub struct SweepConfig {
    pub host: SocketAddr,
    pub transport: Transport,
    pub streams: Vec<u16>,
    pub rates: Vec<RateMode>,
    pub duration: Duration,
    pub warmup: Duration,
    pub payload_bytes: usize,
    pub token: Option<String>,
    pub interval: Duration,
    pub objective: SweepObjective,
    pub top: usize,
}

#[derive(Debug, Clone)]
pub struct ServerConfig {
    pub bind: SocketAddr,
    pub transport: ServerTransport,
    pub token: Option<String>,
    pub max_clients: usize,
    pub idle_timeout: Duration,
}
