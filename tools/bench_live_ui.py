#!/usr/bin/env python3
"""Run optimized live-view regression tests and synthetic CPU/latency benchmarks.

    python tools/bench_live_ui.py --sizes 100000 300000

No daemon, tracker or peer connections are opened. Results: benchmarks/live-ui.json.
The full-query comparison includes facets; live queries deliberately omit them.
Emitter fanout measures the event bus and JSON encoding, not HTTP/gzip/network IO.
"""
import argparse
import datetime
import json
import os
from pathlib import Path
import platform
import subprocess
import tempfile


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--sizes", type=int, nargs="+", default=[100000, 300000])
    args = parser.parse_args()
    if any(n < 400 for n in args.sizes):
        parser.error("catalogues need at least 400 torrents for the mixed-state fixture")
    root = Path(__file__).resolve().parents[1]
    build = subprocess.run([
        "cargo", "test", "--manifest-path", str(root / "typhon-engine/Cargo.toml"),
        "--profile", "live-bench", "--lib", "--bin", "hydranos", "--no-run",
        "--offline", "-j", "4", "--message-format=json",
    ], cwd=root, text=True, stdout=subprocess.PIPE, check=True)
    executables = {}
    for line in build.stdout.splitlines():
        record = json.loads(line)
        if record.get("reason") == "compiler-artifact" and record.get("executable"):
            executables[record["target"]["name"]] = record["executable"]
    scratch_parent = root / "tmp"
    scratch_parent.mkdir(exist_ok=True)
    with tempfile.TemporaryDirectory(prefix="live-ui-", dir=scratch_parent) as scratch:
        if not Path(scratch).resolve().is_relative_to(scratch_parent.resolve()):
            raise RuntimeError("benchmark scratch escaped its workspace directory")
        for target, test in [
            ("typhon_engine", "live_tests"),
            ("typhon_engine", "live_stats_emit_rate_only_and_pause_only_changes"),
            ("hydranos", "api_page_tests"),
            ("hydranos", "page_refresh"),
            ("hydranos", "api_event_stream_tests"),
        ]:
            run = subprocess.run([executables[target], test, "--test-threads=1"], cwd=scratch,
                                 text=True, stdout=subprocess.PIPE, stderr=subprocess.STDOUT)
            print(run.stdout, end="", flush=True)
            run.check_returncode()
        samples = []
        for size in args.sizes:
            for target, test in [("typhon_engine", "benchmark_live_emitter"), ("hydranos", "benchmark_live_pages")]:
                env = dict(os.environ, HYDRANOS_BENCH_N=str(size))
                run = subprocess.run([executables[target], test, "--ignored", "--nocapture", "--test-threads=1"],
                                     cwd=scratch, env=env, text=True, stdout=subprocess.PIPE,
                                     stderr=subprocess.STDOUT, timeout=300, check=True)
                found = []
                for line in run.stdout.splitlines():
                    if "LIVE_BENCH " in line:
                        sample = json.loads(line.split("LIVE_BENCH ", 1)[1])
                        found.append(sample)
                        print(json.dumps(sample), flush=True)
                if not found:
                    raise RuntimeError(f"{test} produced no measurements: {run.stdout}")
                samples.extend(found)
    report = {
        "measured_at_utc": datetime.datetime.now(datetime.timezone.utc).isoformat(),
        "platform": platform.platform(), "processor": platform.processor(),
        "logical_processors": os.cpu_count(), "profile": "live-bench (opt-level 3, no LTO)",
        "notes": [
            "Synthetic in-memory catalogues; 100 downloads, 100 checks and 100 errors in page fixtures.",
            "Store metadata is empty; real category/tag/search workloads may cost more.",
            "Page full_scan=true includes whole-library facets; indexed live queries omit facets.",
            "Production 20000-row parallel-scan threshold is used, not the test-only threshold.",
            "Emitter fixtures change 100 rows per tick; fanout includes bus delivery and JSON encoding, not HTTP/gzip/network IO.",
            "CPU is summed process user+kernel time across threads; small values have OS timer quantization.",
            "RSS includes allocator high-water marks after priming, not just live index or fingerprint storage.",
            "These are UI-path measurements, not torrent-transfer throughput measurements.",
        ],
        "samples": samples,
    }
    destination = root / "benchmarks/live-ui.json"
    destination.parent.mkdir(parents=True, exist_ok=True)
    destination.write_text(json.dumps(report, indent=2) + "\n", encoding="utf-8")
    print(f"Saved {destination}")


if __name__ == "__main__":
    main()
