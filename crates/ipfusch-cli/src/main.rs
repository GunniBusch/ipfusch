use anyhow::{Context, Result};
use clap::{Args, CommandFactory, Parser, Subcommand, ValueEnum};
use clap_complete::{Generator, Shell};
use indicatif::{ProgressBar, ProgressStyle};
use ipfusch_core::config::{
    RateMode, RunConfig, ServerConfig, ServerTransport, SweepConfig, SweepObjective, Transport,
};
use ipfusch_core::report::{IntervalStats, RunReport, StreamStats};
use ipfusch_engine::{SweepResult, run_benchmark, run_sweep};
use std::io;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::str::FromStr;
use std::time::Duration;
use tokio::sync::mpsc;
use tokio::time::Instant;

#[derive(Debug, Parser)]
#[command(
    name = "ipfusch",
    version,
    about = "Modern high-precision iperf alternative"
)]
struct Cli {
    #[arg(long, global = true)]
    json: bool,
    #[arg(long, global = true)]
    jsonl: bool,
    #[arg(long, global = true)]
    no_color: bool,
    #[arg(long, default_value = "info", global = true)]
    log_level: String,
    #[command(subcommand)]
    command: Commands,
}

#[derive(Debug, Subcommand)]
enum Commands {
    Server(ServerArgs),
    Run(RunArgs),
    Sweep(SweepArgs),
    Completion(CompletionArgs),
    Report(ReportArgs),
}

#[derive(Debug, Args)]
struct ServerArgs {
    #[arg(long, default_value = "0.0.0.0:5201")]
    bind: SocketAddr,
    #[arg(long, value_enum, default_value = "all")]
    transport: ServerTransportArg,
    #[arg(long)]
    token: Option<String>,
    #[arg(long, default_value_t = 256)]
    max_clients: usize,
    #[arg(long, default_value = "30s", value_parser = parse_duration)]
    idle_timeout: Duration,
}

#[derive(Debug, Args)]
struct RunArgs {
    #[arg(long)]
    host: SocketAddr,
    #[arg(long, value_enum)]
    transport: TransportArg,
    #[arg(long, default_value_t = 1)]
    streams: u16,
    #[arg(long, default_value = "10s", value_parser = parse_duration)]
    duration: Duration,
    #[arg(long, default_value = "2s", value_parser = parse_duration)]
    warmup: Duration,
    #[arg(long)]
    payload_bytes: Option<usize>,
    #[arg(long, default_value = "auto", value_parser = parse_rate_mode)]
    rate: RateMode,
    #[arg(long)]
    tui: bool,
    #[arg(long)]
    token: Option<String>,
    #[arg(long, default_value = "100ms", value_parser = parse_duration)]
    interval: Duration,
    #[arg(long)]
    output: Option<PathBuf>,
}

#[derive(Debug, Args)]
struct SweepArgs {
    #[arg(long)]
    host: SocketAddr,
    #[arg(long, value_enum)]
    transport: TransportArg,
    #[arg(long, default_value = "1,2,4,8", value_parser = parse_streams_list)]
    streams: Vec<u16>,
    #[arg(long, default_value = "auto,100m,500m,1g", value_parser = parse_rates_list)]
    rates: Vec<RateMode>,
    #[arg(long, default_value = "10s", value_parser = parse_duration)]
    duration: Duration,
    #[arg(long, default_value = "2s", value_parser = parse_duration)]
    warmup: Duration,
    #[arg(long)]
    payload_bytes: Option<usize>,
    #[arg(long)]
    token: Option<String>,
    #[arg(long, default_value = "100ms", value_parser = parse_duration)]
    interval: Duration,
    #[arg(long, value_enum, default_value = "balanced")]
    objective: ObjectiveArg,
    #[arg(long, default_value_t = 3)]
    top: usize,
    #[arg(long)]
    output: Option<PathBuf>,
}

#[derive(Debug, Args)]
struct CompletionArgs {
    #[arg(value_enum)]
    shell: CompletionShell,
}

#[derive(Debug, Args)]
struct ReportArgs {
    #[arg(long)]
    input: PathBuf,
}

#[derive(Debug, Clone, Copy, ValueEnum)]
enum TransportArg {
    Tcp,
    Udp,
    Quic,
}

#[derive(Debug, Clone, Copy, ValueEnum)]
enum ServerTransportArg {
    Tcp,
    Udp,
    Quic,
    All,
}

#[derive(Debug, Clone, Copy, ValueEnum)]
enum ObjectiveArg {
    Throughput,
    MaxEfficiency,
    Balanced,
}

#[derive(Debug, Clone, Copy, ValueEnum)]
enum CompletionShell {
    Bash,
    Zsh,
}

impl From<TransportArg> for Transport {
    fn from(value: TransportArg) -> Self {
        match value {
            TransportArg::Tcp => Self::Tcp,
            TransportArg::Udp => Self::Udp,
            TransportArg::Quic => Self::Quic,
        }
    }
}

impl From<ServerTransportArg> for ServerTransport {
    fn from(value: ServerTransportArg) -> Self {
        match value {
            ServerTransportArg::Tcp => Self::Tcp,
            ServerTransportArg::Udp => Self::Udp,
            ServerTransportArg::Quic => Self::Quic,
            ServerTransportArg::All => Self::All,
        }
    }
}

impl From<ObjectiveArg> for SweepObjective {
    fn from(value: ObjectiveArg) -> Self {
        match value {
            ObjectiveArg::Throughput => Self::Throughput,
            ObjectiveArg::MaxEfficiency => Self::MaxEfficiency,
            ObjectiveArg::Balanced => Self::Balanced,
        }
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();

    match cli.command {
        Commands::Server(args) => {
            if matches!(args.transport, ServerTransportArg::All) {
                eprintln!(
                    "transport=all maps to ports: tcp={}, udp={}, quic={}",
                    args.bind,
                    SocketAddr::new(args.bind.ip(), args.bind.port().saturating_add(1)),
                    SocketAddr::new(args.bind.ip(), args.bind.port().saturating_add(2)),
                );
            }

            ipfusch_net::run_server(ServerConfig {
                bind: args.bind,
                transport: args.transport.into(),
                token: args.token,
                max_clients: args.max_clients,
                idle_timeout: args.idle_timeout,
            })
            .await?;
        }
        Commands::Run(args) => {
            let cfg = RunConfig {
                host: args.host,
                transport: args.transport.into(),
                streams: args.streams,
                duration: args.duration,
                warmup: args.warmup,
                payload_bytes: args
                    .payload_bytes
                    .unwrap_or(default_payload_size(args.transport.into())),
                rate: args.rate,
                token: args.token,
                interval: args.interval,
            };

            let report = if args.tui {
                let (tx, rx) = mpsc::unbounded_channel();
                let runner = tokio::spawn(run_benchmark(cfg, Some(tx)));
                ipfusch_tui::run_dashboard(rx).await?;
                runner.await.context("join benchmark task")??
            } else if cli.jsonl {
                let (tx, mut rx) = mpsc::unbounded_channel();
                let printer = tokio::spawn(async move {
                    while let Some(interval) = rx.recv().await {
                        if let Ok(json) = serde_json::to_string(&interval) {
                            println!("{json}");
                        }
                    }
                });
                let report = run_benchmark(cfg, Some(tx)).await?;
                printer.await.context("join jsonl task")?;
                report
            } else if cli.json {
                run_benchmark(cfg, None).await?
            } else {
                run_with_progress(cfg, cli.no_color).await?
            };

            if let Some(path) = args.output {
                write_json(path, &report)?;
            }

            emit_report(&report, cli.json);
        }
        Commands::Sweep(args) => {
            let cfg = SweepConfig {
                host: args.host,
                transport: args.transport.into(),
                streams: args.streams,
                rates: args.rates,
                duration: args.duration,
                warmup: args.warmup,
                payload_bytes: args
                    .payload_bytes
                    .unwrap_or(default_payload_size(args.transport.into())),
                token: args.token,
                interval: args.interval,
                objective: args.objective.into(),
                top: args.top,
            };

            let sweep = run_sweep(cfg).await?;
            if let Some(path) = args.output {
                write_json(path, &sweep)?;
            }
            emit_sweep(&sweep, cli.json);
        }
        Commands::Completion(args) => {
            let mut cmd = Cli::command();
            match args.shell {
                CompletionShell::Bash => print_completion(Shell::Bash, &mut cmd),
                CompletionShell::Zsh => print_completion(Shell::Zsh, &mut cmd),
            }
        }
        Commands::Report(args) => {
            let raw = std::fs::read_to_string(&args.input)
                .with_context(|| format!("failed to read {}", args.input.display()))?;
            let report: RunReport = serde_json::from_str(&raw)
                .with_context(|| format!("failed to parse {}", args.input.display()))?;
            emit_report(&report, cli.json);
        }
    }

    Ok(())
}

fn emit_report(report: &RunReport, as_json: bool) {
    if as_json {
        println!(
            "{}",
            serde_json::to_string_pretty(report).unwrap_or_else(|_| "{}".to_string())
        );
        return;
    }

    println!(
        "session={} transport={} streams={} duration={}ms",
        report.run_meta.session_id,
        report.run_meta.transport,
        report.config.streams,
        report.config.duration_ms
    );
    println!(
        "throughput={:.2} Mbps packet_rate={:.2} pps loss={:.2}% p50/p95/p99={:.2}/{:.2}/{:.2} ms",
        report.aggregate.throughput_bps / 1_000_000.0,
        report.aggregate.packet_rate_pps,
        report.aggregate.loss_pct,
        report.aggregate.p50_ms,
        report.aggregate.p95_ms,
        report.aggregate.p99_ms
    );

    print_streams(&report.streams);

    if !report.recommendations.is_empty() {
        println!("recommendations:");
        for rec in &report.recommendations {
            println!("  - {} (score {:.3}): {}", rec.label, rec.score, rec.reason);
        }
    }
}

async fn run_with_progress(cfg: RunConfig, no_color: bool) -> Result<RunReport> {
    let measured_total_ms = cfg.duration.as_millis() as u64;
    let warmup_ms = cfg.warmup.as_millis() as u64;
    let total_ms = warmup_ms.saturating_add(measured_total_ms).max(1);
    let refresh = Duration::from_millis(100);

    let style_template = if no_color {
        "{spinner} [{elapsed_precise}] [{bar:40}] {percent:>3}% {msg}"
    } else {
        "{spinner:.cyan} [{elapsed_precise}] [{wide_bar:.blue/white}] {percent:>3}% {msg}"
    };

    let style = ProgressStyle::with_template(style_template)
        .context("invalid progress style template")?
        .progress_chars("█▉▊▋▌▍▎▏ ");

    let bar = ProgressBar::new(total_ms);
    bar.set_style(style);
    bar.enable_steady_tick(Duration::from_millis(120));
    bar.set_message("warming up...");
    bar.println(format!(
        "running {} stream(s) over {} for {} (+ {} warmup)",
        cfg.streams,
        cfg.transport,
        humantime::format_duration(cfg.duration),
        humantime::format_duration(cfg.warmup),
    ));
    bar.println("interval    transfer-rate    packet-rate      loss        p99-latency");

    let start = Instant::now();
    let (tx, mut rx) = mpsc::unbounded_channel::<IntervalStats>();
    let mut runner = tokio::spawn(run_benchmark(cfg, Some(tx)));
    let mut ticker = tokio::time::interval(refresh);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    loop {
        tokio::select! {
            tick = ticker.tick() => {
                let _ = tick;
                let elapsed_ms = start.elapsed().as_millis() as u64;
                let pos = elapsed_ms.min(total_ms);
                bar.set_position(pos);
                if elapsed_ms < warmup_ms {
                    let left = warmup_ms.saturating_sub(elapsed_ms);
                    bar.set_message(format!("warming up... {}ms", left));
                }
            }
            maybe_interval = rx.recv() => {
                let Some(interval) = maybe_interval else {
                    continue;
                };

                let progress = warmup_ms.saturating_add(interval.end_ms.min(measured_total_ms));
                bar.set_position(progress.min(total_ms));
                bar.set_message(format!(
                    "{:.2} Mbps | {:.2}% loss | p99 {:.2} ms",
                    interval.throughput_bps / 1_000_000.0,
                    interval.loss_pct,
                    interval.p99_ms
                ));
                bar.println(format!(
                    "{:>5.2}-{:>5.2}s  {:>12.2} Mbps  {:>12.2} pps  {:>7.2}%  {:>11.2} ms",
                    interval.start_ms as f64 / 1000.0,
                    interval.end_ms as f64 / 1000.0,
                    interval.throughput_bps / 1_000_000.0,
                    interval.packet_rate_pps,
                    interval.loss_pct,
                    interval.p99_ms,
                ));
            }
            result = &mut runner => {
                let report = result.context("join benchmark task")??;
                bar.set_position(total_ms);
                bar.finish_with_message(format!(
                    "complete: {:.2} Mbps avg, {:.2}% loss, p99 {:.2} ms",
                    report.aggregate.throughput_bps / 1_000_000.0,
                    report.aggregate.loss_pct,
                    report.aggregate.p99_ms
                ));
                return Ok(report);
            }
        }
    }
}

fn emit_sweep(result: &SweepResult, as_json: bool) {
    if as_json {
        println!(
            "{}",
            serde_json::to_string_pretty(result).unwrap_or_else(|_| "{}".to_string())
        );
        return;
    }

    println!("sweep runs: {}", result.runs.len());
    for rec in &result.top_recommendations {
        println!("  - {} (score {:.3})", rec.label, rec.score);
    }
}

fn print_streams(streams: &[StreamStats]) {
    if streams.is_empty() {
        return;
    }
    println!("per-stream:");
    for s in streams {
        println!(
            "  stream={} throughput={:.2} Mbps loss={} jitter={:.3}ms p99={:.3}ms",
            s.stream_id,
            s.throughput_bps / 1_000_000.0,
            s.packets_lost,
            s.jitter_ms,
            s.p99_ms
        );
    }
}

fn write_json<T: serde::Serialize>(path: PathBuf, value: &T) -> Result<()> {
    let json = serde_json::to_string_pretty(value).context("serialize output json")?;
    std::fs::write(&path, json).with_context(|| format!("write {}", path.display()))?;
    Ok(())
}

fn print_completion<G: Generator>(generator: G, cmd: &mut clap::Command) {
    clap_complete::generate(
        generator,
        cmd,
        cmd.get_name().to_string(),
        &mut io::stdout(),
    );
}

fn default_payload_size(transport: Transport) -> usize {
    match transport {
        Transport::Tcp => 256 * 1024,
        Transport::Udp | Transport::Quic => 1200,
    }
}

fn parse_duration(input: &str) -> std::result::Result<Duration, String> {
    humantime::parse_duration(input).map_err(|e| e.to_string())
}

fn parse_streams_list(input: &str) -> std::result::Result<Vec<u16>, String> {
    let mut out = Vec::new();
    for part in input.split(',') {
        let value = part
            .trim()
            .parse::<u16>()
            .map_err(|e| format!("invalid stream count `{part}`: {e}"))?;
        if value == 0 {
            return Err("stream count must be >= 1".to_string());
        }
        out.push(value);
    }
    if out.is_empty() {
        return Err("empty streams list".to_string());
    }
    Ok(out)
}

fn parse_rates_list(input: &str) -> std::result::Result<Vec<RateMode>, String> {
    let mut out = Vec::new();
    for part in input.split(',') {
        out.push(parse_rate_mode(part.trim())?);
    }
    if out.is_empty() {
        return Err("empty rates list".to_string());
    }
    Ok(out)
}

fn parse_rate_mode(input: &str) -> std::result::Result<RateMode, String> {
    let raw = input.trim().to_ascii_lowercase();
    if raw == "auto" {
        return Ok(RateMode::Auto);
    }

    let (digits, scale) =
        if let Some(v) = raw.strip_suffix("kbps").or_else(|| raw.strip_suffix('k')) {
            (v, 1_000u64)
        } else if let Some(v) = raw.strip_suffix("mbps").or_else(|| raw.strip_suffix('m')) {
            (v, 1_000_000u64)
        } else if let Some(v) = raw.strip_suffix("gbps").or_else(|| raw.strip_suffix('g')) {
            (v, 1_000_000_000u64)
        } else if let Some(v) = raw.strip_suffix("tbps").or_else(|| raw.strip_suffix('t')) {
            (v, 1_000_000_000_000u64)
        } else if let Some(v) = raw.strip_suffix("bps") {
            (v, 1u64)
        } else {
            (raw.as_str(), 1u64)
        };

    let base = u64::from_str(digits).map_err(|e| format!("invalid rate `{input}`: {e}"))?;
    let Some(total) = base.checked_mul(scale) else {
        return Err(format!("rate `{input}` is too large"));
    };
    if total == 0 {
        return Err("rate must be > 0".to_string());
    }

    Ok(RateMode::Bps(total))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_rate_suffixes_consistent() {
        assert_eq!(parse_rate_mode("auto").unwrap(), RateMode::Auto);
        assert_eq!(parse_rate_mode("1k").unwrap(), RateMode::Bps(1_000));
        assert_eq!(parse_rate_mode("250m").unwrap(), RateMode::Bps(250_000_000));
        assert_eq!(parse_rate_mode("2g").unwrap(), RateMode::Bps(2_000_000_000));
        assert!(parse_rate_mode("0").is_err());
    }

    #[test]
    fn parse_stream_list_consistent() {
        assert_eq!(parse_streams_list("1,2,8").unwrap(), vec![1, 2, 8]);
        assert!(parse_streams_list("0,1").is_err());
    }

    #[test]
    fn parse_rates_list_consistent() {
        assert_eq!(
            parse_rates_list("auto,100m").unwrap(),
            vec![RateMode::Auto, RateMode::Bps(100_000_000)]
        );
    }
}
