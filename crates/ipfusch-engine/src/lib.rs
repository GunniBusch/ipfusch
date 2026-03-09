use anyhow::{Context, Result};
use hdrhistogram::Histogram;
use ipfusch_core::config::{RateMode, RunConfig, SweepConfig, SweepObjective, Transport};
use ipfusch_core::report::{
    AggregateStats, IntervalStats, LatencyBucket, LatencyHistogram, Recommendation,
    RunConfigSnapshot, RunMeta, RunReport, StreamStats,
};
use ipfusch_net::{PacketEvent, StreamRunSpec, run_stream};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tokio::sync::mpsc;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SweepResult {
    pub runs: Vec<RunReport>,
    pub top_recommendations: Vec<Recommendation>,
}

#[derive(Debug, Default, Clone)]
struct StreamAccumulator {
    packets_sent: u64,
    packets_acked: u64,
    packets_lost: u64,
    packets_reordered: u64,
    bytes: u64,
    last_seq: u64,
    latencies_ms: Vec<f64>,
    jitters_ms: Vec<f64>,
    last_latency_ms: Option<f64>,
}

#[derive(Debug, Default, Clone)]
struct Totals {
    packets_sent: u64,
    packets_acked: u64,
    packets_lost: u64,
    bytes: u64,
}

struct IntervalBuildInput<'a> {
    idx: u64,
    interval: Duration,
    totals: &'a Totals,
    prev: &'a Totals,
    interval_latencies: &'a [f64],
    interval_jitters: &'a [f64],
    measurement_start: Instant,
    now: Instant,
}

pub async fn run_benchmark(
    cfg: RunConfig,
    progress_tx: Option<mpsc::UnboundedSender<IntervalStats>>,
) -> Result<RunReport> {
    let session_id = random_session_id();
    let started_ms = unix_ms();
    let started = Instant::now();

    let (resolved_rate, mut recommendations) = resolve_rate(cfg.rate, cfg.transport);

    let (event_tx, mut event_rx) = mpsc::unbounded_channel::<PacketEvent>();
    let per_stream_rate = resolved_rate.map(|v| (v / cfg.streams as u64).max(1));

    let mut handles = Vec::with_capacity(cfg.streams as usize);
    for stream_id in 0..cfg.streams {
        let spec = StreamRunSpec {
            session_id,
            stream_id: stream_id as u32,
            host: cfg.host,
            transport: cfg.transport,
            duration: cfg.duration,
            warmup: cfg.warmup,
            payload_bytes: cfg.payload_bytes,
            rate_bps: per_stream_rate,
            token: cfg.token.clone(),
            timeout: Duration::from_millis(750),
        };
        let tx = event_tx.clone();
        handles.push(tokio::spawn(async move { run_stream(spec, tx).await }));
    }
    drop(event_tx);

    let measurement_start = started + cfg.warmup;
    let mut interval_timer = tokio::time::interval(cfg.interval);
    interval_timer.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    let mut streams = BTreeMap::<u32, StreamAccumulator>::new();
    let mut totals = Totals::default();
    let mut totals_prev = Totals::default();
    let mut intervals = Vec::new();
    let mut interval_idx = 0u64;
    let mut last_interval_tick: Option<Instant> = None;
    let mut interval_latencies = Vec::<f64>::new();
    let mut interval_jitters = Vec::<f64>::new();
    let mut global_latencies = Vec::<f64>::new();
    let mut histogram =
        Histogram::<u64>::new_with_bounds(1, 120_000_000, 3).context("create latency histogram")?;

    loop {
        tokio::select! {
            maybe_event = event_rx.recv() => {
                let Some(event) = maybe_event else {
                    break;
                };

                if !event.measured {
                    continue;
                }

                apply_event(
                    &mut streams,
                    &mut totals,
                    &mut interval_latencies,
                    &mut interval_jitters,
                    &mut global_latencies,
                    &mut histogram,
                    event,
                );
            }
            _ = interval_timer.tick() => {
                let now = Instant::now();
                if now < measurement_start {
                    continue;
                }

                let interval_span = if let Some(last_tick) = last_interval_tick {
                    now.saturating_duration_since(last_tick)
                } else {
                    now.saturating_duration_since(measurement_start)
                };

                let effective_span = if interval_span.is_zero() {
                    cfg.interval
                } else {
                    interval_span
                };

                let interval = build_interval(IntervalBuildInput {
                    idx: interval_idx,
                    interval: effective_span,
                    totals: &totals,
                    prev: &totals_prev,
                    interval_latencies: &interval_latencies,
                    interval_jitters: &interval_jitters,
                    measurement_start,
                    now,
                });
                totals_prev = totals.clone();
                last_interval_tick = Some(now);
                interval_latencies.clear();
                interval_jitters.clear();

                if let Some(tx) = &progress_tx {
                    tx.send(interval.clone()).ok();
                }
                intervals.push(interval);
                interval_idx += 1;
            }
        }
    }

    for handle in handles {
        handle
            .await
            .context("stream join error")?
            .context("stream execution failed")?;
    }

    let finished_ms = unix_ms();
    let elapsed_secs = cfg.duration.as_secs_f64().max(0.000_001);
    let stream_stats = stream_stats_from_map(&streams, elapsed_secs);
    let aggregate = build_aggregate(&streams, &totals, elapsed_secs);

    if aggregate.loss_pct > 1.0 {
        recommendations.push(Recommendation {
            label: "reduce target rate".to_string(),
            score: (100.0 - aggregate.loss_pct).max(0.0),
            reason: "packet loss above 1% indicates saturation".to_string(),
        });
    } else {
        recommendations.push(Recommendation {
            label: "increase stream count".to_string(),
            score: aggregate.throughput_bps / 1_000_000_000.0,
            reason: "loss remains low; additional streams may raise throughput".to_string(),
        });
    }

    Ok(RunReport {
        run_meta: RunMeta {
            session_id,
            version: ipfusch_core::PROTOCOL_VERSION,
            host: cfg.host.to_string(),
            transport: cfg.transport,
            started_unix_ms: started_ms,
            finished_unix_ms: finished_ms,
        },
        config: RunConfigSnapshot {
            streams: cfg.streams,
            duration_ms: cfg.duration.as_millis() as u64,
            warmup_ms: cfg.warmup.as_millis() as u64,
            payload_bytes: cfg.payload_bytes,
            rate_bps: resolved_rate,
            interval_ms: cfg.interval.as_millis() as u64,
        },
        intervals,
        streams: stream_stats,
        aggregate,
        latency_histogram: build_histogram(&global_latencies, &mut histogram),
        recommendations,
    })
}

pub async fn run_sweep(cfg: SweepConfig) -> Result<SweepResult> {
    let mut runs = Vec::new();
    for streams in &cfg.streams {
        for rate in &cfg.rates {
            let run_cfg = RunConfig {
                host: cfg.host,
                transport: cfg.transport,
                streams: *streams,
                duration: cfg.duration,
                warmup: cfg.warmup,
                payload_bytes: cfg.payload_bytes,
                rate: *rate,
                token: cfg.token.clone(),
                interval: cfg.interval,
            };

            let mut report = run_benchmark(run_cfg, None).await?;
            let score = score_run(cfg.objective, &report.aggregate);
            report.recommendations.push(Recommendation {
                label: format!("streams={}, rate={}", streams, rate_label(*rate)),
                score,
                reason: format!("score for {:?} objective", cfg.objective),
            });
            runs.push(report);
        }
    }

    runs.sort_by(|a, b| {
        let ascore = score_run(cfg.objective, &a.aggregate);
        let bscore = score_run(cfg.objective, &b.aggregate);
        bscore.total_cmp(&ascore)
    });

    let top_recommendations = runs
        .iter()
        .take(cfg.top)
        .map(|r| Recommendation {
            label: format!(
                "streams={},rate={}bps",
                r.config.streams,
                r.config.rate_bps.unwrap_or_default()
            ),
            score: score_run(cfg.objective, &r.aggregate),
            reason: "top-ranked sweep result".to_string(),
        })
        .collect();

    Ok(SweepResult {
        runs,
        top_recommendations,
    })
}

fn apply_event(
    streams: &mut BTreeMap<u32, StreamAccumulator>,
    totals: &mut Totals,
    interval_latencies: &mut Vec<f64>,
    interval_jitters: &mut Vec<f64>,
    global_latencies: &mut Vec<f64>,
    histogram: &mut Histogram<u64>,
    event: PacketEvent,
) {
    let stream = streams.entry(event.stream_id).or_default();
    stream.packets_sent += 1;
    stream.bytes += event.bytes;

    totals.packets_sent += 1;
    totals.bytes += event.bytes;

    if let Some(recv_ns) = event.recv_mono_ns {
        stream.packets_acked += 1;
        totals.packets_acked += 1;

        let latency_ms = (recv_ns.saturating_sub(event.send_mono_ns)) as f64 / 1_000_000.0;
        stream.latencies_ms.push(latency_ms);
        interval_latencies.push(latency_ms);
        global_latencies.push(latency_ms);
        histogram.record((latency_ms * 1000.0).max(1.0) as u64).ok();

        if let Some(prev) = stream.last_latency_ms {
            let jitter = (latency_ms - prev).abs();
            stream.jitters_ms.push(jitter);
            interval_jitters.push(jitter);
        }
        stream.last_latency_ms = Some(latency_ms);

        if stream.last_seq > event.seq {
            stream.packets_reordered += 1;
        }
        stream.last_seq = stream.last_seq.max(event.seq);
    } else {
        stream.packets_lost += 1;
        totals.packets_lost += 1;
    }
}

fn build_interval(input: IntervalBuildInput<'_>) -> IntervalStats {
    let bytes = input.totals.bytes.saturating_sub(input.prev.bytes);
    let packets_sent = input
        .totals
        .packets_sent
        .saturating_sub(input.prev.packets_sent);
    let packets_acked = input
        .totals
        .packets_acked
        .saturating_sub(input.prev.packets_acked);
    let packets_lost = input
        .totals
        .packets_lost
        .saturating_sub(input.prev.packets_lost);

    let secs = input.interval.as_secs_f64().max(0.000_001);
    let throughput_bps = (bytes as f64 * 8.0) / secs;
    let packet_rate_pps = packets_sent as f64 / secs;
    let loss_pct = if packets_sent == 0 {
        0.0
    } else {
        (packets_lost as f64 * 100.0) / packets_sent as f64
    };

    IntervalStats {
        index: input.idx,
        start_ms: input
            .now
            .saturating_duration_since(input.measurement_start)
            .saturating_sub(input.interval)
            .as_millis() as u64,
        end_ms: input
            .now
            .saturating_duration_since(input.measurement_start)
            .as_millis() as u64,
        bytes,
        packets_sent,
        packets_acked,
        packets_lost,
        throughput_bps,
        packet_rate_pps,
        loss_pct,
        jitter_ms: mean(input.interval_jitters),
        p50_ms: quantile(input.interval_latencies, 0.50),
        p95_ms: quantile(input.interval_latencies, 0.95),
        p99_ms: quantile(input.interval_latencies, 0.99),
    }
}

fn stream_stats_from_map(
    map: &BTreeMap<u32, StreamAccumulator>,
    elapsed_secs: f64,
) -> Vec<StreamStats> {
    map.iter()
        .map(|(stream_id, s)| StreamStats {
            stream_id: *stream_id,
            packets_sent: s.packets_sent,
            packets_acked: s.packets_acked,
            packets_lost: s.packets_lost,
            packets_reordered: s.packets_reordered,
            bytes: s.bytes,
            throughput_bps: (s.bytes as f64 * 8.0) / elapsed_secs,
            jitter_ms: mean(&s.jitters_ms),
            p50_ms: quantile(&s.latencies_ms, 0.50),
            p95_ms: quantile(&s.latencies_ms, 0.95),
            p99_ms: quantile(&s.latencies_ms, 0.99),
        })
        .collect()
}

fn build_aggregate(
    map: &BTreeMap<u32, StreamAccumulator>,
    totals: &Totals,
    elapsed_secs: f64,
) -> AggregateStats {
    let all_latencies: Vec<f64> = map.values().flat_map(|s| s.latencies_ms.clone()).collect();
    let all_jitters: Vec<f64> = map.values().flat_map(|s| s.jitters_ms.clone()).collect();
    let packets_reordered = map.values().map(|s| s.packets_reordered).sum();

    AggregateStats {
        packets_sent: totals.packets_sent,
        packets_acked: totals.packets_acked,
        packets_lost: totals.packets_lost,
        packets_reordered,
        bytes: totals.bytes,
        throughput_bps: (totals.bytes as f64 * 8.0) / elapsed_secs,
        packet_rate_pps: totals.packets_sent as f64 / elapsed_secs,
        loss_pct: if totals.packets_sent == 0 {
            0.0
        } else {
            totals.packets_lost as f64 * 100.0 / totals.packets_sent as f64
        },
        jitter_ms: mean(&all_jitters),
        p50_ms: quantile(&all_latencies, 0.50),
        p95_ms: quantile(&all_latencies, 0.95),
        p99_ms: quantile(&all_latencies, 0.99),
    }
}

fn build_histogram(latencies_ms: &[f64], histogram: &mut Histogram<u64>) -> LatencyHistogram {
    if latencies_ms.is_empty() {
        return LatencyHistogram::default();
    }

    let bounds = [1_u64, 2, 5, 10, 20, 50, 100, 200, 500, 1000, 2000, 5000];
    let mut buckets = Vec::with_capacity(bounds.len());

    for upper_ms in bounds {
        let upper_us = upper_ms * 1000;
        let count = histogram.count_between(1, upper_us);
        buckets.push(LatencyBucket {
            le_ms: upper_ms,
            count,
        });
    }

    LatencyHistogram { buckets }
}

fn resolve_rate(rate: RateMode, transport: Transport) -> (Option<u64>, Vec<Recommendation>) {
    match rate {
        RateMode::Bps(v) => (Some(v), Vec::new()),
        RateMode::Auto => match transport {
            Transport::Tcp => {
                let selected = 80_000_000_000;
                (
                    Some(selected),
                    vec![Recommendation {
                        label: "auto-rate-selected".to_string(),
                        score: selected as f64 / 1_000_000_000.0,
                        reason: "auto tuner selected high-throughput paced tcp mode".to_string(),
                    }],
                )
            }
            Transport::Udp => {
                let selected = 2_000_000_000;
                (
                    Some(selected),
                    vec![Recommendation {
                        label: "auto-rate-selected".to_string(),
                        score: selected as f64 / 1_000_000_000.0,
                        reason: format!(
                            "auto tuner selected {} bps baseline for {}",
                            selected, transport
                        ),
                    }],
                )
            }
            Transport::Quic => {
                let selected = 1_000_000_000;
                (
                    Some(selected),
                    vec![Recommendation {
                        label: "auto-rate-selected".to_string(),
                        score: selected as f64 / 1_000_000_000.0,
                        reason: format!(
                            "auto tuner selected {} bps baseline for {}",
                            selected, transport
                        ),
                    }],
                )
            }
        },
    }
}

fn score_run(objective: SweepObjective, agg: &AggregateStats) -> f64 {
    let throughput_gbps = agg.throughput_bps / 1_000_000_000.0;
    match objective {
        SweepObjective::Throughput => throughput_gbps,
        SweepObjective::MaxEfficiency => throughput_gbps / (1.0 + agg.p99_ms + agg.loss_pct),
        SweepObjective::Balanced => throughput_gbps - (agg.p99_ms * 0.01 + agg.loss_pct * 0.05),
    }
}

fn rate_label(rate: RateMode) -> String {
    match rate {
        RateMode::Auto => "auto".to_string(),
        RateMode::Bps(v) => v.to_string(),
    }
}

fn random_session_id() -> u64 {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    (nanos as u64) ^ 0xA5A5_5A5A_4242_1919
}

fn unix_ms() -> i128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as i128
}

fn quantile(values: &[f64], q: f64) -> f64 {
    if values.is_empty() {
        return 0.0;
    }
    let mut sorted = values.to_vec();
    sorted.sort_by(f64::total_cmp);
    let idx = ((sorted.len() - 1) as f64 * q.clamp(0.0, 1.0)).round() as usize;
    sorted[idx]
}

fn mean(values: &[f64]) -> f64 {
    if values.is_empty() {
        return 0.0;
    }
    values.iter().sum::<f64>() / values.len() as f64
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quantiles_are_stable() {
        let values = vec![1.0, 2.0, 3.0, 4.0, 5.0];
        assert_eq!(quantile(&values, 0.50), 3.0);
        assert_eq!(quantile(&values, 0.95), 5.0);
        assert_eq!(quantile(&values, 0.99), 5.0);
    }

    #[test]
    fn balanced_score_penalizes_loss() {
        let good = AggregateStats {
            throughput_bps: 2_000_000_000.0,
            loss_pct: 0.1,
            p99_ms: 2.0,
            ..AggregateStats::default()
        };
        let bad = AggregateStats {
            throughput_bps: 2_000_000_000.0,
            loss_pct: 4.0,
            p99_ms: 20.0,
            ..AggregateStats::default()
        };

        assert!(
            score_run(SweepObjective::Balanced, &good) > score_run(SweepObjective::Balanced, &bad)
        );
    }

    #[test]
    fn auto_rate_differs_by_transport() {
        let (tcp, _) = resolve_rate(RateMode::Auto, Transport::Tcp);
        let (udp, _) = resolve_rate(RateMode::Auto, Transport::Udp);
        let (quic, _) = resolve_rate(RateMode::Auto, Transport::Quic);
        assert!(tcp > udp);
        assert!(udp > quic);
    }
}
