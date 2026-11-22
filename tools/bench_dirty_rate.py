#!/usr/bin/env python3
"""Measure the production dirty-rate pass against the old fingerprint sampler."""
import datetime
import json
import os
from pathlib import Path
import platform
import statistics
import subprocess
import tempfile

from bench_event_tracking import checked


def main():
    root = Path(__file__).resolve().parents[1]
    build = subprocess.run([
        "cargo", "test", "--manifest-path", str(root / "typhon-engine/Cargo.toml"),
        "--profile", "live-bench", "--lib", "--no-run", "--offline", "-j", "4",
        "--message-format=json",
    ], cwd=root, text=True, stdout=subprocess.PIPE, check=True)
    executable = None
    for line in build.stdout.splitlines():
        item = json.loads(line)
        if (item.get("reason") == "compiler-artifact" and item.get("executable")
                and item["target"]["name"] == "typhon_engine" and item["profile"]["test"]):
            executable = item["executable"]
    if not executable:
        raise RuntimeError("Cargo did not return a library test executable")
    parent = root / "tmp"
    parent.mkdir(exist_ok=True)
    records = []
    with tempfile.TemporaryDirectory(prefix="dirty-rate-", dir=parent) as scratch:
        if not Path(scratch).resolve().is_relative_to(parent.resolve()):
            raise RuntimeError("scratch directory escaped the workspace")
        for test in ["dirty_", "live_stats_emit_rate_only_and_pause_only_changes"]:
            print(checked([executable, test, "--test-threads=1"], cwd=scratch, timeout=120), flush=True)
        for n in [100000, 300000]:
            for sample in range(3):
                output = checked([
                    executable, "benchmark_dirty_rate_pass", "--ignored", "--nocapture", "--test-threads=1",
                ], cwd=scratch, env=dict(os.environ, HYDRANOS_BENCH_N=str(n)), timeout=300)
                found = [json.loads(line.split("LIVE_BENCH ", 1)[1])["data"]
                         for line in output.splitlines() if "LIVE_BENCH " in line]
                if len(found) != 4:
                    raise RuntimeError(f"Expected four measurements, got {len(found)}")
                records.extend(dict(item, sample=sample) for item in found)
                print(f"{n}: sample {sample + 1}/3 passed", flush=True)
    medians = []
    for n in [100000, 300000]:
        for mode in ["fingerprint", "dirty_flags"]:
            for changing in [False, True]:
                group = [r for r in records if (r["catalogue"], r["mode"], r["changing"]) == (n, mode, changing)]
                medians.append(dict(catalogue=n, mode=mode, changing=changing,
                                    **{key: statistics.median(r[key] for r in group) for key in ["tick_ms", "cpu_ms"]}))
    result = dict(measured_at_utc=datetime.datetime.now(datetime.timezone.utc).isoformat(),
                  platform=platform.platform(), records=records, medians=medians,
                  notes=["20 ticks per measurement, three process samples per catalogue; includes the existing rate/index pass and event publication.",
                         "Changing workload increments uploaded totals on 100 cold seeding torrents; no per-packet dirty marks.",
                         "JSON encoding, network delivery, and real transfer throughput are not measured.",
                         "Both modes use the new TorrentState layout. RSS includes allocator high-water effects; fingerprint mode runs first.",
                         "Production tracking stores one AtomicU8 per torrent; structure padding may affect actual memory cost."])
    destination = root / "benchmarks/dirty-rate.json"
    destination.write_text(json.dumps(result, indent=2) + "\n", encoding="utf-8")
    print(json.dumps(medians, indent=2), flush=True)


if __name__ == "__main__":
    main()
