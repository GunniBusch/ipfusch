mod client;
mod server;

use ipfusch_core::Transport;
use std::net::SocketAddr;
use std::time::Duration;

pub use client::run_stream;
pub use server::run_server;

#[derive(Debug, Clone)]
pub struct StreamRunSpec {
    pub session_id: u64,
    pub stream_id: u32,
    pub host: SocketAddr,
    pub transport: Transport,
    pub duration: Duration,
    pub warmup: Duration,
    pub payload_bytes: usize,
    pub rate_bps: Option<u64>,
    pub token: Option<String>,
    pub timeout: Duration,
}

#[derive(Debug, Clone)]
pub struct PacketEvent {
    pub stream_id: u32,
    pub seq: u64,
    pub bytes: u64,
    pub send_mono_ns: u64,
    pub recv_mono_ns: Option<u64>,
    pub measured: bool,
}

fn pacing_delay(payload_bytes: usize, rate_bps: Option<u64>) -> Option<Duration> {
    let bps = rate_bps?;
    if bps == 0 {
        return None;
    }
    let bits = (payload_bytes as f64) * 8.0;
    let nanos = (bits * 1_000_000_000f64 / bps as f64).max(1.0);
    Some(Duration::from_nanos(nanos as u64))
}

fn monotonic_ns() -> u64 {
    use std::sync::OnceLock;
    use std::time::Instant;

    static START: OnceLock<Instant> = OnceLock::new();
    START.get_or_init(Instant::now).elapsed().as_nanos() as u64
}

fn token_matches(expected_token: Option<&str>, got_hash: Option<[u8; 32]>) -> bool {
    let Some(expected_token) = expected_token else {
        return true;
    };
    let Some(got_hash) = got_hash else {
        return false;
    };
    let expected_hash = ipfusch_core::hash_token(expected_token);
    let mut diff: u8 = 0;
    for (a, b) in expected_hash.iter().zip(got_hash.iter()) {
        diff |= a ^ b;
    }
    diff == 0
}
