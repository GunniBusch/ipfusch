use crate::config::Transport;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RunMeta {
    pub session_id: u64,
    pub version: u16,
    pub host: String,
    pub transport: Transport,
    pub started_unix_ms: i128,
    pub finished_unix_ms: i128,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RunConfigSnapshot {
    pub streams: u16,
    pub duration_ms: u64,
    pub warmup_ms: u64,
    pub payload_bytes: usize,
    pub rate_bps: Option<u64>,
    pub interval_ms: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct IntervalStats {
    pub index: u64,
    pub start_ms: u64,
    pub end_ms: u64,
    pub bytes: u64,
    pub packets_sent: u64,
    pub packets_acked: u64,
    pub packets_lost: u64,
    pub throughput_bps: f64,
    pub packet_rate_pps: f64,
    pub loss_pct: f64,
    pub jitter_ms: f64,
    pub p50_ms: f64,
    pub p95_ms: f64,
    pub p99_ms: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct StreamStats {
    pub stream_id: u32,
    pub packets_sent: u64,
    pub packets_acked: u64,
    pub packets_lost: u64,
    pub packets_reordered: u64,
    pub bytes: u64,
    pub throughput_bps: f64,
    pub jitter_ms: f64,
    pub p50_ms: f64,
    pub p95_ms: f64,
    pub p99_ms: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct AggregateStats {
    pub packets_sent: u64,
    pub packets_acked: u64,
    pub packets_lost: u64,
    pub packets_reordered: u64,
    pub bytes: u64,
    pub throughput_bps: f64,
    pub packet_rate_pps: f64,
    pub loss_pct: f64,
    pub jitter_ms: f64,
    pub p50_ms: f64,
    pub p95_ms: f64,
    pub p99_ms: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LatencyBucket {
    pub le_ms: u64,
    pub count: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct LatencyHistogram {
    pub buckets: Vec<LatencyBucket>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Recommendation {
    pub label: String,
    pub score: f64,
    pub reason: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RunReport {
    pub run_meta: RunMeta,
    pub config: RunConfigSnapshot,
    pub intervals: Vec<IntervalStats>,
    pub streams: Vec<StreamStats>,
    pub aggregate: AggregateStats,
    pub latency_histogram: LatencyHistogram,
    pub recommendations: Vec<Recommendation>,
}
