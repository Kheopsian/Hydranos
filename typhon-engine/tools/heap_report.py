#!/usr/bin/env python3
"""Symbolise a Typhon jemalloc heap profile and report allocation sites.

`jeprof` cannot symbolise these dumps on its own. The engine is a PIE, so the
addresses in the profile are runtime addresses, and jemalloc's MAPPED_LIBRARIES
section lists only anonymous mappings -- there is no file-backed entry telling
jeprof where the binary was loaded. Every frame comes back as `?`.

This does the two things jeprof is missing: it reads the load base out of
/proc/<pid>/maps and subtracts it, then batches the rest through addr2line.

Usage, on the host:

    kill -USR1 <engine pid>                  # writes /configs/jeprof.<nspid>.N.mN.heap
    python3 heap_report.py \\
        --heap /mnt/cache/appdata/hydra/jeprof.39.0.m0.heap \\
        --base 0x5634e3027000 \\
        --binary /tmp/hydra-engine

`--base` is the start of the first mapping of the binary in /proc/<pid>/maps.
Pass `--pid` instead to have it read from there directly. addr2line comes from
binutils, which Unraid does not ship; run this inside a container if needed:

    docker run --rm -v /tmp:/tmp debian:12 sh -c \\
      'apt-get update -qq && apt-get install -y -qq binutils python3 && python3 /tmp/heap_report.py ...'
"""
import argparse, re, subprocess, sys, collections

def load_base(pid, binary_name="hydra-engine"):
    for line in open("/proc/%s/maps" % pid):
        if binary_name in line:
            return int(line.split("-")[0], 16)
    raise SystemExit("no mapping for %s in /proc/%s/maps" % (binary_name, pid))

def parse(path):
    """Return [(bytes, objects, [frame addresses])] for every sampled stack."""
    stacks, cur = [], None
    for line in open(path, errors="replace"):
        line = line.rstrip("\n")
        if line.startswith("MAPPED"):
            break
        if line.startswith("@"):
            cur = [int(a, 16) for a in line[1:].split()]
            continue
        m = re.match(r"^\s*t\*: (\d+): (\d+) \[", line)
        if m and cur is not None:
            stacks.append((int(m.group(2)), int(m.group(1)), cur))
            cur = None
    return stacks

def symbolise(addrs, base, binary):
    inp = "\n".join("%x" % (a - base) for a in addrs)
    r = subprocess.run(["addr2line", "-f", "-C", "-e", binary],
                       input=inp, capture_output=True, text=True)
    if r.returncode != 0:
        raise SystemExit("addr2line failed: %s" % r.stderr.strip())
    out = r.stdout.rstrip("\n").split("\n")
    sym = {}
    for i, a in enumerate(addrs):
        fn = out[2 * i] if 2 * i < len(out) else "?"
        loc = out[2 * i + 1] if 2 * i + 1 < len(out) else "?"
        sym[a] = (fn, loc.split("/")[-1])
    return sym

# Frames that are the allocator itself, not the code that asked for memory.
NOISE = re.compile(r"prof_backtrace|tsd_post|imalloc|^_rjem|malloc_|je_|"
                   r"alloc::alloc|RawVec|raw_vec|__rust_alloc|Vec<T>::|"
                   r"::reserve|::with_capacity|::from_iter|^\?\?$|^\?$")

def blame(frames, sym):
    """First frame that is real Typhon/library code rather than allocator plumbing."""
    for a in frames:
        fn, loc = sym.get(a, ("?", "?"))
        if not NOISE.search(fn):
            return fn, loc
    return "<unattributed>", "?"

def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--heap", required=True)
    ap.add_argument("--binary", required=True)
    ap.add_argument("--base")
    ap.add_argument("--pid")
    ap.add_argument("--top", type=int, default=25)
    ap.add_argument("--rss-kb", type=int, help="VmRSS, to report coverage")
    a = ap.parse_args()

    base = int(a.base, 16) if a.base else load_base(a.pid)
    stacks = parse(a.heap)
    if not stacks:
        raise SystemExit("no sampled stacks in %s" % a.heap)

    addrs = sorted({f for _, _, fr in stacks for f in fr})
    sym = symbolise(addrs, base, a.binary)

    agg = collections.defaultdict(lambda: [0, 0])
    for b, o, fr in stacks:
        k = blame(fr, sym)
        agg[k][0] += b
        agg[k][1] += o

    total = sum(b for b, _, _ in stacks)
    print("sampled live : %.2f GB across %d stacks" % (total / 1e9, len(stacks)))
    if a.rss_kb:
        rss = a.rss_kb * 1024
        print("process RSS  : %.2f GB  -> the profile accounts for %.0f%% of it"
              % (rss / 1e9, 100.0 * total / rss))
        print("              (a low number means live allocations the sampler missed,")
        print("               allocator slop, or dirty pages -- jemalloc stats tell which)")
    print()
    print("%12s %10s  %-52s %s" % ("BYTES", "OBJECTS", "ALLOCATION SITE", "FILE:LINE"))
    for (fn, loc), (b, o) in sorted(agg.items(), key=lambda kv: -kv[1][0])[:a.top]:
        size = "%.1f MB" % (b / 1e6) if b < 1e9 else "%.2f GB" % (b / 1e9)
        print("%12s %10d  %-52s %s" % (size, o, fn[:52], loc))

if __name__ == "__main__":
    main()
