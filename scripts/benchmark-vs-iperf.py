#!/usr/bin/env python3
import argparse
import json
import os
import re
import resource
import shutil
import statistics
import subprocess
import sys
import threading
import time
from datetime import datetime, timezone
from pathlib import Path


PING_TIME_RE = re.compile(r"time[=<]([0-9.]+)\s*ms")


def child_cpu_seconds():
    usage = resource.getrusage(resource.RUSAGE_CHILDREN)
    return float(usage.ru_utime + usage.ru_stime)


def run_checked(cmd, stdout_path=None):
    cpu_before = child_cpu_seconds()
    started = time.perf_counter()
    if stdout_path is None:
        subprocess.run(cmd, check=True)
    else:
        with open(stdout_path, "w", encoding="utf-8") as fh:
            subprocess.run(cmd, check=True, stdout=fh)
    wall_s = time.perf_counter() - started
    cpu_s = max(child_cpu_seconds() - cpu_before, 0.0)
    return {
        "wall_s": wall_s,
        "cpu_s": cpu_s,
        "cpu_pct": (cpu_s / wall_s * 100.0) if wall_s > 0 else 0.0,
    }


def wait_for_port_ready(host, port, timeout_s=5.0):
    import socket

    deadline = time.time() + timeout_s
    while time.time() < deadline:
        sock = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
        try:
            sock.settimeout(0.2)
            sock.connect((host, port))
            return
        except OSError:
            time.sleep(0.05)
        finally:
            sock.close()
    raise RuntimeError(f"server on {host}:{port} did not become ready in time")


def assert_port_available(host, port):
    import socket

    deadline = time.time() + 3.0
    last_error = None
    while time.time() < deadline:
        sock = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
        try:
            sock.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
            sock.bind((host, port))
            return
        except OSError as exc:
            last_error = exc
            time.sleep(0.05)
        finally:
            sock.close()
    raise RuntimeError(
        f"port {host}:{port} is already in use; choose a different port"
    ) from last_error


def terminate_process(proc):
    if proc is None or proc.poll() is not None:
        return
    proc.terminate()
    try:
        proc.wait(timeout=2)
    except subprocess.TimeoutExpired:
        proc.kill()
        proc.wait(timeout=2)


def load_json(path: Path):
    with path.open("r", encoding="utf-8") as fh:
        return json.load(fh)


def iperf_summary_block(data):
    end = data.get("end", {})
    if "sum_received" in end:
        return end["sum_received"]
    if "sum" in end:
        return end["sum"]
    if "sum_sent" in end:
        return end["sum_sent"]
    return {}


def iperf_throughput_bps(data):
    return float(iperf_summary_block(data).get("bits_per_second", 0.0))


def iperf_retransmits(data):
    end = data.get("end", {})
    if "sum_sent" in end and "retransmits" in end["sum_sent"]:
        return int(end["sum_sent"]["retransmits"])
    return None


def iperf_interval_throughputs(data):
    out = []
    for item in data.get("intervals", []):
        sum_data = item.get("sum", {})
        if "bits_per_second" in sum_data:
            out.append(float(sum_data["bits_per_second"]))
    return out


def ipfusch_interval_throughputs(data):
    return [float(i.get("throughput_bps", 0.0)) for i in data.get("intervals", [])]


def ipfusch_bytes(data):
    return float(data.get("aggregate", {}).get("bytes", 0.0))


def iperf_bytes(data):
    return float(iperf_summary_block(data).get("bytes", 0.0))


def throughput_bias_pct(reported_bps, observed_bps):
    if observed_bps <= 0:
        return 0.0
    return ((reported_bps - observed_bps) / observed_bps) * 100.0


def ipfusch_reported_duration_ms(data):
    intervals = data.get("intervals", [])
    if intervals:
        return float(intervals[-1].get("end_ms", data.get("config", {}).get("duration_ms", 0)))
    return float(data.get("config", {}).get("duration_ms", 0))


def iperf_reported_duration_ms(data):
    end = data.get("end", {})
    seconds = None
    if "sum_received" in end and "seconds" in end["sum_received"]:
        seconds = float(end["sum_received"]["seconds"])
    elif "sum_sent" in end and "seconds" in end["sum_sent"]:
        seconds = float(end["sum_sent"]["seconds"])
    elif "sum" in end and "seconds" in end["sum"]:
        seconds = float(end["sum"]["seconds"])
    return (seconds or 0.0) * 1000.0


def aggregate(values):
    if not values:
        return {
            "mean": 0.0,
            "median": 0.0,
            "min": 0.0,
            "max": 0.0,
            "stddev": 0.0,
            "cv_pct": 0.0,
        }

    mean = statistics.fmean(values)
    stddev = statistics.pstdev(values) if len(values) > 1 else 0.0
    return {
        "mean": mean,
        "median": statistics.median(values),
        "min": min(values),
        "max": max(values),
        "stddev": stddev,
        "cv_pct": (stddev / mean * 100.0) if mean > 0 else 0.0,
    }


def format_gbps(bps):
    return bps / 1_000_000_000.0


def parse_stream_list(value):
    out = []
    for part in value.split(","):
        part = part.strip()
        if not part:
            continue
        out.append(int(part))
    if not out:
        raise argparse.ArgumentTypeError("stream sweep must contain at least one stream count")
    return out


def parse_ping_rtt(line):
    match = PING_TIME_RE.search(line)
    return float(match.group(1)) if match else None


def run_ping_baseline(host, count):
    if shutil.which("ping") is None:
        return []
    result = subprocess.run(
        ["ping", "-c", str(count), host],
        capture_output=True,
        text=True,
        check=False,
    )
    samples = []
    for line in (result.stdout or "").splitlines():
        rtt = parse_ping_rtt(line)
        if rtt is not None:
            samples.append(rtt)
    return samples


class PingMonitor:
    def __init__(self, host):
        self.host = host
        self.proc = None
        self.samples = []
        self.thread = None

    def start(self):
        if shutil.which("ping") is None:
            return False
        self.proc = subprocess.Popen(
            ["ping", self.host],
            stdout=subprocess.PIPE,
            stderr=subprocess.STDOUT,
            text=True,
            bufsize=1,
        )
        self.thread = threading.Thread(target=self._collect, daemon=True)
        self.thread.start()
        return True

    def _collect(self):
        if self.proc is None or self.proc.stdout is None:
            return
        for line in self.proc.stdout:
            rtt = parse_ping_rtt(line)
            if rtt is not None:
                self.samples.append(rtt)

    def stop(self):
        terminate_process(self.proc)
        if self.thread is not None:
            self.thread.join(timeout=1.0)
        return list(self.samples)


def summarize_latency(samples):
    if not samples:
        return {
            "mean": None,
            "p95": None,
            "max": None,
        }
    ordered = sorted(samples)
    p95_index = min(len(ordered) - 1, max(0, int(len(ordered) * 0.95) - 1))
    return {
        "mean": statistics.fmean(samples),
        "p95": ordered[p95_index],
        "max": max(samples),
    }


def metric_mean(stats):
    return None if stats is None else stats.get("mean")


def format_optional_ms(stats):
    value = metric_mean(stats)
    return "n/a" if value is None else f"{value:.3f}"


def aggregate_nullable(values):
    filtered = [float(v) for v in values if v is not None]
    return None if not filtered else aggregate(filtered)


def run_ipfusch_round(args, repo_root, ipfusch_bin, raw_dir, streams, round_id):
    suffix = f"s{streams}-r{round_id}" if args.stream_sweep else str(round_id)
    server_log_path = raw_dir / f"ipfusch-server-{suffix}.log"
    result_path = raw_dir / f"ipfusch-{suffix}.json"
    ping_baseline = summarize_latency(run_ping_baseline(args.host, args.ping_count))
    monitor = PingMonitor(args.host)

    with server_log_path.open("w", encoding="utf-8") as server_log:
        server = subprocess.Popen(
            [
                str(ipfusch_bin),
                "server",
                "--bind",
                f"{args.host}:{args.ipfusch_port}",
                "--transport",
                "tcp",
            ],
            stdout=server_log,
            stderr=subprocess.STDOUT,
            preexec_fn=os.setsid if os.name != "nt" else None,
        )

    try:
        wait_for_port_ready(args.host, args.ipfusch_port)
        monitor_started = monitor.start()
        timing = run_checked(
            [
                str(ipfusch_bin),
                "--json",
                "run",
                "--host",
                f"{args.host}:{args.ipfusch_port}",
                "--transport",
                "tcp",
                "--streams",
                str(streams),
                "--duration",
                f"{args.duration}s",
                "--warmup",
                f"{args.warmup}s",
                "--interval",
                args.interval,
            ],
            stdout_path=result_path,
        )
    finally:
        under_load_ping = summarize_latency(monitor.stop() if monitor_started else [])
        terminate_process(server)

    data = load_json(result_path)
    interval_tp = ipfusch_interval_throughputs(data)
    reported_ms = ipfusch_reported_duration_ms(data)
    reported_bps = float(data["aggregate"]["throughput_bps"])
    observed_bps = (ipfusch_bytes(data) * 8.0 / timing["wall_s"]) if timing["wall_s"] > 0 else 0.0
    baseline_mean = ping_baseline["mean"]
    under_load_mean = under_load_ping["mean"]
    return {
        "round": round_id,
        "throughput_bps": reported_bps,
        "throughput_bias_pct": throughput_bias_pct(reported_bps, observed_bps),
        "loss_pct": float(data["aggregate"]["loss_pct"]),
        "p99_ms": float(data["aggregate"]["p99_ms"]),
        "packet_rate_pps": float(data["aggregate"]["packet_rate_pps"]),
        "runtime_s": timing["wall_s"],
        "cpu_s": timing["cpu_s"],
        "cpu_pct": timing["cpu_pct"],
        "runtime_error_ms": abs(reported_ms - args.duration * 1000.0),
        "wall_clock_error_ms": abs(timing["wall_s"] - (args.duration + args.warmup)) * 1000.0,
        "interval_throughput_cv_pct": aggregate(interval_tp)["cv_pct"],
        "baseline_rtt_ms": baseline_mean,
        "under_load_rtt_ms": under_load_mean,
        "latency_impact_ms": (
            under_load_mean - baseline_mean
            if baseline_mean is not None and under_load_mean is not None
            else None
        ),
    }


def run_iperf_round(args, raw_dir, streams, round_id):
    suffix = f"s{streams}-r{round_id}" if args.stream_sweep else str(round_id)
    server_log_path = raw_dir / f"iperf3-server-{suffix}.log"
    result_path = raw_dir / f"iperf3-{suffix}.json"
    ping_baseline = summarize_latency(run_ping_baseline(args.host, args.ping_count))
    monitor = PingMonitor(args.host)

    with server_log_path.open("w", encoding="utf-8") as server_log:
        server = subprocess.Popen(
            ["iperf3", "-s", "-p", str(args.iperf_port)],
            stdout=server_log,
            stderr=subprocess.STDOUT,
            preexec_fn=os.setsid if os.name != "nt" else None,
        )

    try:
        wait_for_port_ready(args.host, args.iperf_port)
        monitor_started = monitor.start()
        timing = run_checked(
            [
                "iperf3",
                "-c",
                args.host,
                "-p",
                str(args.iperf_port),
                "-P",
                str(streams),
                "-t",
                str(args.duration),
                "-J",
            ],
            stdout_path=result_path,
        )
    finally:
        under_load_ping = summarize_latency(monitor.stop() if monitor_started else [])
        terminate_process(server)

    data = load_json(result_path)
    interval_tp = iperf_interval_throughputs(data)
    reported_ms = iperf_reported_duration_ms(data)
    reported_bps = iperf_throughput_bps(data)
    observed_bps = (iperf_bytes(data) * 8.0 / timing["wall_s"]) if timing["wall_s"] > 0 else 0.0
    baseline_mean = ping_baseline["mean"]
    under_load_mean = under_load_ping["mean"]
    return {
        "round": round_id,
        "throughput_bps": reported_bps,
        "throughput_bias_pct": throughput_bias_pct(reported_bps, observed_bps),
        "retransmits": iperf_retransmits(data),
        "runtime_s": timing["wall_s"],
        "cpu_s": timing["cpu_s"],
        "cpu_pct": timing["cpu_pct"],
        "runtime_error_ms": abs(reported_ms - args.duration * 1000.0),
        "wall_clock_error_ms": abs(timing["wall_s"] - args.duration) * 1000.0,
        "interval_throughput_cv_pct": aggregate(interval_tp)["cv_pct"],
        "baseline_rtt_ms": baseline_mean,
        "under_load_rtt_ms": under_load_mean,
        "latency_impact_ms": (
            under_load_mean - baseline_mean
            if baseline_mean is not None and under_load_mean is not None
            else None
        ),
    }


def build_suite_summary(args, streams, ipfusch_runs, iperf_runs):
    ipfusch_tp_stats = aggregate([r["throughput_bps"] for r in ipfusch_runs])
    iperf_tp_stats = aggregate([r["throughput_bps"] for r in iperf_runs])

    return {
        "streams": streams,
        "ipfusch": {
            "rounds": ipfusch_runs,
            "throughput_bps": ipfusch_tp_stats,
            "throughput_bias_pct": aggregate([r["throughput_bias_pct"] for r in ipfusch_runs]),
            "loss_pct": aggregate([r["loss_pct"] for r in ipfusch_runs]),
            "p99_ms": aggregate([r["p99_ms"] for r in ipfusch_runs]),
            "packet_rate_pps": aggregate([r["packet_rate_pps"] for r in ipfusch_runs]),
            "runtime_s": aggregate([r["runtime_s"] for r in ipfusch_runs]),
            "cpu_s": aggregate([r["cpu_s"] for r in ipfusch_runs]),
            "cpu_pct": aggregate([r["cpu_pct"] for r in ipfusch_runs]),
            "runtime_error_ms": aggregate([r["runtime_error_ms"] for r in ipfusch_runs]),
            "wall_clock_error_ms": aggregate([r["wall_clock_error_ms"] for r in ipfusch_runs]),
            "interval_throughput_cv_pct": aggregate(
                [r["interval_throughput_cv_pct"] for r in ipfusch_runs]
            ),
            "baseline_rtt_ms": aggregate_nullable([r["baseline_rtt_ms"] for r in ipfusch_runs]),
            "under_load_rtt_ms": aggregate_nullable([r["under_load_rtt_ms"] for r in ipfusch_runs]),
            "latency_impact_ms": aggregate_nullable([r["latency_impact_ms"] for r in ipfusch_runs]),
        },
        "iperf3": {
            "rounds": iperf_runs,
            "throughput_bps": iperf_tp_stats,
            "throughput_bias_pct": aggregate([r["throughput_bias_pct"] for r in iperf_runs]),
            "runtime_s": aggregate([r["runtime_s"] for r in iperf_runs]),
            "cpu_s": aggregate([r["cpu_s"] for r in iperf_runs]),
            "cpu_pct": aggregate([r["cpu_pct"] for r in iperf_runs]),
            "runtime_error_ms": aggregate([r["runtime_error_ms"] for r in iperf_runs]),
            "wall_clock_error_ms": aggregate([r["wall_clock_error_ms"] for r in iperf_runs]),
            "interval_throughput_cv_pct": aggregate(
                [r["interval_throughput_cv_pct"] for r in iperf_runs]
            ),
            "baseline_rtt_ms": aggregate_nullable([r["baseline_rtt_ms"] for r in iperf_runs]),
            "under_load_rtt_ms": aggregate_nullable([r["under_load_rtt_ms"] for r in iperf_runs]),
            "latency_impact_ms": aggregate_nullable([r["latency_impact_ms"] for r in iperf_runs]),
            "retransmits": aggregate(
                [float(r["retransmits"] or 0.0) for r in iperf_runs]
            ),
        },
        "comparison": {
            "mean_throughput_ratio_ipfusch_vs_iperf3": (
                ipfusch_tp_stats["mean"] / iperf_tp_stats["mean"]
                if iperf_tp_stats["mean"] > 0
                else None
            ),
            "throughput_cv_advantage_pct_points": (
                iperf_tp_stats["cv_pct"] - ipfusch_tp_stats["cv_pct"]
            ),
            "runtime_error_advantage_ms": (
                aggregate([r["runtime_error_ms"] for r in iperf_runs])["mean"]
                - aggregate([r["runtime_error_ms"] for r in ipfusch_runs])["mean"]
            ),
            "cpu_efficiency_advantage_pct_points": (
                aggregate([r["cpu_pct"] for r in iperf_runs])["mean"]
                - aggregate([r["cpu_pct"] for r in ipfusch_runs])["mean"]
            ),
        },
    }


def run_suite(args, repo_root, ipfusch_bin, raw_dir, streams):
    print(
        "Running "
        f"{args.runs} rounds | ipfusch={args.host}:{args.ipfusch_port} "
        f"iperf3={args.host}:{args.iperf_port} streams={streams} "
        f"duration={args.duration}s warmup={args.warmup}s"
    )
    ipfusch_runs = []
    iperf_runs = []

    for round_id in range(1, args.runs + 1):
        print(f"[streams {streams}] [round {round_id}] ipfusch")
        ipfusch_runs.append(run_ipfusch_round(args, repo_root, ipfusch_bin, raw_dir, streams, round_id))

        print(f"[streams {streams}] [round {round_id}] iperf3")
        iperf_runs.append(run_iperf_round(args, raw_dir, streams, round_id))

    return build_suite_summary(args, streams, ipfusch_runs, iperf_runs)


def write_markdown(args, out_dir, raw_dir, summary):
    summary_json = out_dir / "summary.json"
    suites = summary["suites"]
    lines = [
        "# ipfusch vs iperf3 Benchmark",
        "",
        f"- ipfusch endpoint: `{args.host}:{args.ipfusch_port}`",
        f"- iperf3 endpoint: `{args.host}:{args.iperf_port}`",
        f"- Duration per run: `{args.duration}s`",
        f"- Warmup (ipfusch): `{args.warmup}s`",
        f"- Runs: `{args.runs}`",
        f"- Ping baseline samples: `{args.ping_count}`",
    ]

    if args.stream_sweep:
        lines.append(f"- Stream sweep: `{','.join(str(v) for v in args.stream_sweep)}`")
    else:
        lines.append(f"- Streams: `{args.streams}`")

    lines.extend(
        [
            "",
            "## Benchmark Dimensions",
            "",
            "- Throughput accuracy: reported rate versus transferred bytes over elapsed wall time",
            "- CPU overhead: client CPU seconds and CPU percent during the run",
            "- Latency impact: ping RTT baseline versus under-load RTT while the test is active",
            "- Repeatability: throughput stddev and coefficient of variation across rounds",
            "- Scalability: mean throughput across a stream sweep",
            "- Packet loss reporting: reported here for ipfusch; loss accuracy needs controlled UDP impairment and is not validated in TCP mode",
        ]
    )

    for suite in suites:
        ipfusch = suite["ipfusch"]
        iperf3 = suite["iperf3"]
        ratio = suite["comparison"]["mean_throughput_ratio_ipfusch_vs_iperf3"]
        lines.extend(
            [
                "",
                f"## Streams: {suite['streams']}",
                "",
                "### Performance",
                "",
                f"- ipfusch throughput (mean): `{format_gbps(ipfusch['throughput_bps']['mean']):.3f} Gbps`",
                f"- iperf3 throughput (mean): `{format_gbps(iperf3['throughput_bps']['mean']):.3f} Gbps`",
                f"- Ratio (ipfusch/iperf3): `{ratio:.3f}`" if ratio is not None else "- Ratio: `n/a`",
                "",
                "### Accuracy, Precision, and Reliability",
                "",
                f"- Throughput bias vs transferred bytes over wall time: ipfusch `{ipfusch['throughput_bias_pct']['mean']:.2f}%`, iperf3 `{iperf3['throughput_bias_pct']['mean']:.2f}%`",
                f"- Throughput stddev across runs: ipfusch `{format_gbps(ipfusch['throughput_bps']['stddev']):.3f} Gbps`, iperf3 `{format_gbps(iperf3['throughput_bps']['stddev']):.3f} Gbps`",
                f"- Throughput CV across runs (lower is better): ipfusch `{ipfusch['throughput_bps']['cv_pct']:.2f}%`, iperf3 `{iperf3['throughput_bps']['cv_pct']:.2f}%`",
                f"- Runtime error vs target duration (mean): ipfusch `{ipfusch['runtime_error_ms']['mean']:.1f} ms`, iperf3 `{iperf3['runtime_error_ms']['mean']:.1f} ms`",
                f"- In-run interval throughput CV (mean): ipfusch `{ipfusch['interval_throughput_cv_pct']['mean']:.2f}%`, iperf3 `{iperf3['interval_throughput_cv_pct']['mean']:.2f}%`",
                "",
                "### Efficiency and Latency",
                "",
                f"- Client CPU overhead (mean CPU percent): ipfusch `{ipfusch['cpu_pct']['mean']:.1f}%`, iperf3 `{iperf3['cpu_pct']['mean']:.1f}%`",
                f"- Client CPU time (mean): ipfusch `{ipfusch['cpu_s']['mean']:.3f}s`, iperf3 `{iperf3['cpu_s']['mean']:.3f}s`",
                f"- Under-load RTT (mean): ipfusch `{format_optional_ms(ipfusch['under_load_rtt_ms'])} ms`, iperf3 `{format_optional_ms(iperf3['under_load_rtt_ms'])} ms`",
                f"- RTT impact vs baseline (mean): ipfusch `{format_optional_ms(ipfusch['latency_impact_ms'])} ms`, iperf3 `{format_optional_ms(iperf3['latency_impact_ms'])} ms`",
                "",
                "### Reported Quality Metrics",
                "",
                f"- ipfusch loss: `{ipfusch['loss_pct']['mean']:.3f}%`",
                f"- ipfusch p99 latency: `{ipfusch['p99_ms']['mean']:.3f} ms`",
                f"- ipfusch packet rate: `{ipfusch['packet_rate_pps']['mean']:.1f} pps`",
                f"- iperf3 retransmits: `{iperf3['retransmits']['mean']:.1f}`",
            ]
        )

    if len(suites) > 1:
        base_ipfusch = suites[0]["ipfusch"]["throughput_bps"]["mean"]
        base_iperf3 = suites[0]["iperf3"]["throughput_bps"]["mean"]
        lines.extend(
            [
                "",
                "## Scalability",
                "",
                "| Streams | ipfusch mean | iperf3 mean | Ratio | ipfusch scaling | iperf3 scaling |",
                "| --- | ---: | ---: | ---: | ---: | ---: |",
            ]
        )
        for suite in suites:
            ratio = suite["comparison"]["mean_throughput_ratio_ipfusch_vs_iperf3"]
            ipfusch_scale = (
                suite["ipfusch"]["throughput_bps"]["mean"] / base_ipfusch
                if base_ipfusch > 0
                else 0.0
            )
            iperf3_scale = (
                suite["iperf3"]["throughput_bps"]["mean"] / base_iperf3
                if base_iperf3 > 0
                else 0.0
            )
            lines.append(
                f"| {suite['streams']} | {format_gbps(suite['ipfusch']['throughput_bps']['mean']):.3f} Gbps | "
                f"{format_gbps(suite['iperf3']['throughput_bps']['mean']):.3f} Gbps | "
                f"{ratio:.3f} | {ipfusch_scale:.3f}x | {iperf3_scale:.3f}x |"
            )

    lines.extend(
        [
            "",
            "## Raw files",
            "",
            f"- JSON summary: `{summary_json}`",
            f"- Per-round raw data: `{raw_dir}`",
        ]
    )

    md = out_dir / "summary.md"
    md.write_text("\n".join(lines) + "\n", encoding="utf-8")
    return md


def main():
    parser = argparse.ArgumentParser(
        description="Benchmark ipfusch and iperf3 side-by-side with identical run settings"
    )
    parser.add_argument("--host", default="127.0.0.1")
    parser.add_argument("--ipfusch-port", type=int, default=5201)
    parser.add_argument("--iperf-port", type=int, default=6201)
    parser.add_argument("--streams", type=int, default=4)
    parser.add_argument("--stream-sweep", type=parse_stream_list)
    parser.add_argument("--duration", type=int, default=10, help="seconds")
    parser.add_argument("--warmup", type=int, default=0, help="ipfusch warmup seconds")
    parser.add_argument("--runs", type=int, default=3)
    parser.add_argument("--interval", default="1s")
    parser.add_argument("--ping-count", type=int, default=5)
    parser.add_argument("--output-dir", default="")
    parser.add_argument("--no-build", action="store_true")
    parser.add_argument("--ipfusch-bin", default="")
    args = parser.parse_args()

    repo_root = Path(__file__).resolve().parent.parent
    default_out_dir = (
        repo_root
        / "benchmark-results"
        / datetime.now(tz=timezone.utc).strftime("%Y%m%d-%H%M%S")
    )
    out_dir = Path(args.output_dir) if args.output_dir else default_out_dir
    raw_dir = out_dir / "raw"
    raw_dir.mkdir(parents=True, exist_ok=True)

    ipfusch_bin = (
        Path(args.ipfusch_bin)
        if args.ipfusch_bin
        else repo_root / "target" / "release" / "ipfusch"
    )

    if not args.no_build:
        run_checked(["cargo", "build", "--release", "-p", "ipfusch-cli"])

    if not ipfusch_bin.exists():
        raise RuntimeError(
            f"ipfusch binary not found: {ipfusch_bin}; run cargo build --release -p ipfusch-cli"
        )

    if subprocess.run(["which", "iperf3"], capture_output=True).returncode != 0:
        raise RuntimeError("iperf3 is not installed or not in PATH")

    assert_port_available(args.host, args.ipfusch_port)
    assert_port_available(args.host, args.iperf_port)

    print(f"Benchmark output directory: {out_dir}")
    stream_values = args.stream_sweep if args.stream_sweep else [args.streams]
    suites = [run_suite(args, repo_root, ipfusch_bin, raw_dir, streams) for streams in stream_values]

    summary = {
        "meta": {
            "host": args.host,
            "ipfusch_port": args.ipfusch_port,
            "iperf_port": args.iperf_port,
            "streams": args.streams,
            "stream_sweep": stream_values,
            "duration_s": args.duration,
            "warmup_s": args.warmup,
            "runs": args.runs,
            "interval": args.interval,
            "ping_count": args.ping_count,
            "generated_at_utc": datetime.now(tz=timezone.utc).isoformat(),
        },
        "suites": suites,
        "scaling": [
            {
                "streams": suite["streams"],
                "ipfusch_throughput_bps_mean": suite["ipfusch"]["throughput_bps"]["mean"],
                "iperf3_throughput_bps_mean": suite["iperf3"]["throughput_bps"]["mean"],
                "ratio": suite["comparison"]["mean_throughput_ratio_ipfusch_vs_iperf3"],
                "ipfusch_cpu_pct_mean": suite["ipfusch"]["cpu_pct"]["mean"],
                "iperf3_cpu_pct_mean": suite["iperf3"]["cpu_pct"]["mean"],
            }
            for suite in suites
        ],
    }

    summary_json = out_dir / "summary.json"
    with summary_json.open("w", encoding="utf-8") as fh:
        json.dump(summary, fh, indent=2)

    md = write_markdown(args, out_dir, raw_dir, summary)

    print("Done.")
    print(f"Summary JSON: {summary_json}")
    print(f"Summary Markdown: {md}")


if __name__ == "__main__":
    try:
        main()
    except Exception as exc:
        print(f"benchmark failed: {exc}", file=sys.stderr)
        sys.exit(1)
