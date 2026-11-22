#!/usr/bin/env python3
"""Compare test-only event tracking prototypes: python tools/bench_event_tracking.py.

Uses the existing optimized live-bench profile. Results: benchmarks/event-tracking.json.
No tracking algorithm is enabled in the shipping engine by this benchmark.
"""
import argparse
import datetime
import json
import os
from pathlib import Path
import platform
import statistics
import subprocess
import tempfile

STRATEGIES = ["sampler", "fused_rate_pass", "embedded_flags", "fused_flags", "dense_flags", "bitmap",
              "mutex_set", "sharded_set", "raw_queue", "raw_queue_signal", "dedup_queue", "bitmap_queue"]


def checked(command, **kwargs):
    run = subprocess.run(command, text=True, stdout=subprocess.PIPE,
                         stderr=subprocess.STDOUT, **kwargs)
    if run.returncode:
        print(run.stdout, flush=True)
        run.check_returncode()
    return run.stdout


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--sizes", nargs="+", type=int, default=[100000, 300000])
    parser.add_argument("--strategies", nargs="+", choices=STRATEGIES, default=STRATEGIES)
    args = parser.parse_args()
    if any(n < 400 for n in args.sizes):
        parser.error("use catalogues of at least 400 torrents")
    root = Path(__file__).resolve().parents[1]
    build = subprocess.run(["cargo", "test", "--manifest-path", str(root / "typhon-engine/Cargo.toml"),
                            "--profile", "live-bench", "--lib", "--no-run", "--offline", "-j", "4",
                            "--message-format=json"], cwd=root, text=True, stdout=subprocess.PIPE, check=True)
    executable = None
    for line in build.stdout.splitlines():
        artifact = json.loads(line)
        if artifact.get("reason") == "compiler-artifact" and artifact.get("executable"):
            if artifact["target"]["name"] == "typhon_engine" and artifact["profile"]["test"]:
                executable = artifact["executable"]
    if not executable:
        raise RuntimeError("Cargo did not return the library test executable")
    parent = root / "tmp"
    parent.mkdir(exist_ok=True)
    records = []
    with tempfile.TemporaryDirectory(prefix="event-tracking-", dir=parent) as scratch:
        if not Path(scratch).resolve().is_relative_to(parent.resolve()):
            raise RuntimeError("scratch directory escaped the workspace")
        for test in ["event_tracking_", "live_tests", "live_stats_emit_rate_only_and_pause_only_changes"]:
            print(checked([executable, test, "--test-threads=1"], cwd=scratch, timeout=120), flush=True)
        for n in args.sizes:
            for strategy in args.strategies:
                env = dict(os.environ, HYDRANOS_BENCH_N=str(n), HYDRANOS_EVENT_STRATEGY=strategy)
                output = checked([executable, "benchmark_event_tracking", "--ignored", "--nocapture",
                                  "--test-threads=1"], cwd=scratch, env=env, timeout=300)
                found = [json.loads(line.split("LIVE_BENCH ", 1)[1])["data"]
                         for line in output.splitlines() if "LIVE_BENCH " in line]
                if len(found) != 10:
                    raise RuntimeError(f"Expected ten workloads for {strategy}, got {len(found)}")
                for item in found:
                    medians = {key: statistics.median(sample[key] for sample in item["samples"])
                               for key in ["producer_ms", "producer_cpu_ms", "consumer_ms",
                                           "consumer_cpu_ms", "tracking_and_snapshot_ms", "rate_phase_ms",
                                           "json_encode_ms", "json_bytes", "rows"]}
                    item["median"] = medians
                records.extend(found)
                print(f"{n}: {strategy}: ten workloads passed", flush=True)
    result = {
        "measured_at_utc": datetime.datetime.now(datetime.timezone.utc).isoformat(),
        "platform": platform.platform(), "processor": platform.processor(),
        "logical_processors": os.cpu_count(), "profile": "live-bench (opt-level 3, no LTO)",
        "notes": [
            "Each strategy/catalogue runs in a separate process; three samples per workload.",
            "Consumer time includes the existing production rate/index pass and identical TorrentStatsMini construction.",
            "The fused prototypes observe fingerprints or embedded dirty flags inside the rate pass; other strategies drain afterward.",
            "Producer cost includes real atomic uploaded-byte updates plus notification hooks; thread startup is excluded.",
            "Batched notifications perform the same data updates but mark each changed torrent once per batch.",
            "Repeated-update tests include clustered and spread-out IDs; set shards use a mixed ID hash to avoid slot-stride bias.",
            "Dense strategies assume stable slot IDs. Removal/reuse needs generation checks before deployment; not modeled here.",
            "Embedded flags live in an actual TorrentState test-only field; that field is absent from production builds.",
            "Set strategies use existing 20-byte info hashes; queues use the installed Tokio bounded mpsc implementation.",
            "Queues are drained once per delivery batch; a continuously draining actor is not benchmarked.",
            "Threads are not affinity-pinned; small timing differences are scheduler/cache sensitive.",
            "Queue overflow falls back to dirty flags/bits, full catalogue resync, or a client resync signal, depending on the variant.",
            "raw_queue_signal excludes client refresh/REST/re-hydration work; its low server-side number is not end-to-end recovery cost.",
            "Reserved payload bytes exclude allocator, hash-table control bytes, padding and channel bookkeeping.",
            "RSS priming includes allocator high-water effects; consumer JSON and HTTP/gzip/network costs are separate.",
            "Correctness tests exercise deduplication, overflow and racing drains with injected notifications; engine hook coverage is not implemented.",
            "CPU time is summed across threads and has OS timer quantization; use busy workloads for per-update estimates.",
        ],
        "records": records,
    }
    destination = root / "benchmarks/event-tracking.json"
    destination.parent.mkdir(exist_ok=True)
    destination.write_text(json.dumps(result, indent=2) + "\n", encoding="utf-8")
    print(f"Saved {destination}")


if __name__ == "__main__":
    main()
