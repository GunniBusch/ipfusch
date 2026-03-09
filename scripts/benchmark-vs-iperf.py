#!/usr/bin/env python3
import argparse
import json
import os
import statistics
import subprocess
import sys
import time
from datetime import datetime, timezone
from pathlib import Path


def run_checked(cmd, stdout_path=None):
    if stdout_path is None:
        return subprocess.run(cmd, check=True)
    with open(stdout_path, "w", encoding="utf-8") as fh:
        return subprocess.run(cmd, check=True, stdout=fh)


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


def iperf_throughput_bps(data):
    end = data.get("end", {})
    if "sum_received" in end:
        return float(end["sum_received"].get("bits_per_second", 0.0))
    if "sum" in end:
        return float(end["sum"].get("bits_per_second", 0.0))
    if "sum_sent" in end:
        return float(end["sum_sent"].get("bits_per_second", 0.0))
    return 0.0


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


def main():
    parser = argparse.ArgumentParser(
        description="Benchmark ipfusch and iperf3 side-by-side with identical run settings"
    )
    parser.add_argument("--host", default="127.0.0.1")
    parser.add_argument("--ipfusch-port", type=int, default=5201)
    parser.add_argument("--iperf-port", type=int, default=6201)
    parser.add_argument("--streams", type=int, default=4)
    parser.add_argument("--duration", type=int, default=10, help="seconds")
    parser.add_argument("--warmup", type=int, default=0, help="ipfusch warmup seconds")
    parser.add_argument("--runs", type=int, default=3)
    parser.add_argument("--interval", default="1s")
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
    print(
        "Running "
        f"{args.runs} rounds | ipfusch={args.host}:{args.ipfusch_port} "
        f"iperf3={args.host}:{args.iperf_port} streams={args.streams} "
        f"duration={args.duration}s warmup={args.warmup}s"
    )

    ipfusch_runs = []
    iperf_runs = []

    for i in range(1, args.runs + 1):
        print(f"[round {i}] ipfusch")
        ipfusch_server_log = raw_dir / f"ipfusch-server-{i}.log"
        ipfusch_result = raw_dir / f"ipfusch-{i}.json"

        with ipfusch_server_log.open("w", encoding="utf-8") as server_log:
            ipfusch_server = subprocess.Popen(
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
            started = time.perf_counter()
            run_checked(
                [
                    str(ipfusch_bin),
                    "--json",
                    "run",
                    "--host",
                    f"{args.host}:{args.ipfusch_port}",
                    "--transport",
                    "tcp",
                    "--streams",
                    str(args.streams),
                    "--duration",
                    f"{args.duration}s",
                    "--warmup",
                    f"{args.warmup}s",
                    "--interval",
                    args.interval,
                ],
                stdout_path=ipfusch_result,
            )
            elapsed_s = time.perf_counter() - started
        finally:
            terminate_process(ipfusch_server)

        ipfusch_data = load_json(ipfusch_result)
        ipfusch_int = ipfusch_interval_throughputs(ipfusch_data)
        ipfusch_reported_ms = ipfusch_reported_duration_ms(ipfusch_data)
        ipfusch_runs.append(
            {
                "round": i,
                "throughput_bps": float(ipfusch_data["aggregate"]["throughput_bps"]),
                "loss_pct": float(ipfusch_data["aggregate"]["loss_pct"]),
                "p99_ms": float(ipfusch_data["aggregate"]["p99_ms"]),
                "packet_rate_pps": float(ipfusch_data["aggregate"]["packet_rate_pps"]),
                "runtime_s": elapsed_s,
                "runtime_error_ms": abs(ipfusch_reported_ms - args.duration * 1000.0),
                "wall_clock_error_ms": abs(elapsed_s - (args.duration + args.warmup)) * 1000.0,
                "interval_throughput_cv_pct": aggregate(ipfusch_int)["cv_pct"],
            }
        )

        print(f"[round {i}] iperf3")
        iperf_server_log = raw_dir / f"iperf3-server-{i}.log"
        iperf_result = raw_dir / f"iperf3-{i}.json"
        with iperf_server_log.open("w", encoding="utf-8") as server_log:
            iperf_server = subprocess.Popen(
                ["iperf3", "-s", "-p", str(args.iperf_port)],
                stdout=server_log,
                stderr=subprocess.STDOUT,
                preexec_fn=os.setsid if os.name != "nt" else None,
            )

        try:
            wait_for_port_ready(args.host, args.iperf_port)
            started = time.perf_counter()
            run_checked(
                [
                    "iperf3",
                    "-c",
                    args.host,
                    "-p",
                    str(args.iperf_port),
                    "-P",
                    str(args.streams),
                    "-t",
                    str(args.duration),
                    "-J",
                ],
                stdout_path=iperf_result,
            )
            elapsed_s = time.perf_counter() - started
        finally:
            terminate_process(iperf_server)

        iperf_data = load_json(iperf_result)
        iperf_int = iperf_interval_throughputs(iperf_data)
        iperf_reported_ms = iperf_reported_duration_ms(iperf_data)
        iperf_runs.append(
            {
                "round": i,
                "throughput_bps": iperf_throughput_bps(iperf_data),
                "retransmits": iperf_retransmits(iperf_data),
                "runtime_s": elapsed_s,
                "runtime_error_ms": abs(iperf_reported_ms - args.duration * 1000.0),
                "wall_clock_error_ms": abs(elapsed_s - args.duration) * 1000.0,
                "interval_throughput_cv_pct": aggregate(iperf_int)["cv_pct"],
            }
        )

    ipfusch_tp = [r["throughput_bps"] for r in ipfusch_runs]
    iperf_tp = [r["throughput_bps"] for r in iperf_runs]

    ipfusch_tp_stats = aggregate(ipfusch_tp)
    iperf_tp_stats = aggregate(iperf_tp)

    summary = {
        "meta": {
            "host": args.host,
            "ipfusch_port": args.ipfusch_port,
            "iperf_port": args.iperf_port,
            "streams": args.streams,
            "duration_s": args.duration,
            "warmup_s": args.warmup,
            "runs": args.runs,
            "interval": args.interval,
            "generated_at_utc": datetime.now(tz=timezone.utc).isoformat(),
        },
        "ipfusch": {
            "rounds": ipfusch_runs,
            "throughput_bps": ipfusch_tp_stats,
            "loss_pct": aggregate([r["loss_pct"] for r in ipfusch_runs]),
            "p99_ms": aggregate([r["p99_ms"] for r in ipfusch_runs]),
            "packet_rate_pps": aggregate([r["packet_rate_pps"] for r in ipfusch_runs]),
            "runtime_s": aggregate([r["runtime_s"] for r in ipfusch_runs]),
            "runtime_error_ms": aggregate([r["runtime_error_ms"] for r in ipfusch_runs]),
            "wall_clock_error_ms": aggregate([r["wall_clock_error_ms"] for r in ipfusch_runs]),
            "interval_throughput_cv_pct": aggregate(
                [r["interval_throughput_cv_pct"] for r in ipfusch_runs]
            ),
        },
        "iperf3": {
            "rounds": iperf_runs,
            "throughput_bps": iperf_tp_stats,
            "runtime_s": aggregate([r["runtime_s"] for r in iperf_runs]),
            "runtime_error_ms": aggregate([r["runtime_error_ms"] for r in iperf_runs]),
            "wall_clock_error_ms": aggregate([r["wall_clock_error_ms"] for r in iperf_runs]),
            "interval_throughput_cv_pct": aggregate(
                [r["interval_throughput_cv_pct"] for r in iperf_runs]
            ),
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
        },
    }

    summary_json = out_dir / "summary.json"
    with summary_json.open("w", encoding="utf-8") as fh:
        json.dump(summary, fh, indent=2)

    md = out_dir / "summary.md"
    ipfusch_mean = summary["ipfusch"]["throughput_bps"]["mean"]
    iperf_mean = summary["iperf3"]["throughput_bps"]["mean"]
    ratio = summary["comparison"]["mean_throughput_ratio_ipfusch_vs_iperf3"]

    lines = [
        "# ipfusch vs iperf3 Benchmark",
        "",
        f"- ipfusch endpoint: `{args.host}:{args.ipfusch_port}`",
        f"- iperf3 endpoint: `{args.host}:{args.iperf_port}`",
        f"- Streams: `{args.streams}`",
        f"- Duration per run: `{args.duration}s`",
        f"- Warmup (ipfusch): `{args.warmup}s`",
        f"- Runs: `{args.runs}`",
        "",
        "## Performance",
        "",
        f"- ipfusch throughput (mean): `{format_gbps(ipfusch_mean):.3f} Gbps`",
        f"- iperf3 throughput (mean): `{format_gbps(iperf_mean):.3f} Gbps`",
        f"- Ratio (ipfusch/iperf3): `{ratio:.3f}`" if ratio is not None else "- Ratio: `n/a`",
        "",
        "## Precision and stability",
        "",
        f"- Throughput CV across runs (lower is better): ipfusch `{summary['ipfusch']['throughput_bps']['cv_pct']:.2f}%`, iperf3 `{summary['iperf3']['throughput_bps']['cv_pct']:.2f}%`",
        f"- Runtime error vs target duration (mean): ipfusch `{summary['ipfusch']['runtime_error_ms']['mean']:.1f} ms`, iperf3 `{summary['iperf3']['runtime_error_ms']['mean']:.1f} ms`",
        f"- Wall-clock command overhead error (mean): ipfusch `{summary['ipfusch']['wall_clock_error_ms']['mean']:.1f} ms`, iperf3 `{summary['iperf3']['wall_clock_error_ms']['mean']:.1f} ms`",
        f"- In-run interval throughput CV (mean): ipfusch `{summary['ipfusch']['interval_throughput_cv_pct']['mean']:.2f}%`, iperf3 `{summary['iperf3']['interval_throughput_cv_pct']['mean']:.2f}%`",
        "",
        "## ipfusch quality metrics (mean)",
        "",
        f"- Loss: `{summary['ipfusch']['loss_pct']['mean']:.3f}%`",
        f"- p99 latency: `{summary['ipfusch']['p99_ms']['mean']:.3f} ms`",
        f"- Packet rate: `{summary['ipfusch']['packet_rate_pps']['mean']:.1f} pps`",
        f"- iperf3 retransmits: `{summary['iperf3']['retransmits']['mean']:.1f}`",
        "",
        "## Raw files",
        "",
        f"- JSON summary: `{summary_json}`",
        f"- Per-round raw data: `{raw_dir}`",
    ]
    md.write_text("\n".join(lines) + "\n", encoding="utf-8")

    print("Done.")
    print(f"Summary JSON: {summary_json}")
    print(f"Summary Markdown: {md}")


if __name__ == "__main__":
    try:
        main()
    except Exception as exc:
        print(f"benchmark failed: {exc}", file=sys.stderr)
        sys.exit(1)
