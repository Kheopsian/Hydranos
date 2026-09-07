#!/usr/bin/env python3
"""Extract the HTTP route table from the Go source.

This is the contract the Rust binary has to reproduce, so it is derived from the
code rather than written by hand: a checklist maintained separately from the
router drifts from it, and the first thing anyone would trust it about -- "is
everything ported?" -- is exactly what it would get wrong.

gin cannot be asked at runtime (the binary sets its own mode and prints a custom
banner instead of the route dump), but the routing code is regular enough to
read directly: a group is `name := parent.Group("/prefix")` and a route is
`name.METHOD("/path", handler)`. Prefixes are resolved transitively.

    routes.py --repo <path> [--json routes.json] [--requests requests.json]
"""

import argparse
import json
import os
import re
import sys

GROUP = re.compile(r'^\s*(\w+)\s*:=\s*(?:(\w+)\.)?(?:\w+\.)*Group\("([^"]*)"')
# The receiver may be several segments -- `router.GET`, but also
# `s.router.GET`. Matching only one swallowed every route registered through a
# field, which is how /api/login, /api/setup, /api/stats and /api/config stayed
# out of the denominator while coverage reported 100%.
ROUTE = re.compile(
    r'^\s*(?:\w+\.)*?(\w+)\.(GET|POST|PUT|DELETE|PATCH|HEAD|Any)\("([^"]*)"\s*,\s*([^)]*)'
)

# Receivers that are the router itself rather than a group.
ROOT_RECEIVERS = {"r", "router", "engine"}


def extract(repo):
    api_dir = os.path.join(repo, "internal", "api")
    routes, groups = [], {}

    for name in sorted(os.listdir(api_dir)):
        if not name.endswith(".go") or name.endswith("_test.go"):
            continue
        path = os.path.join(api_dir, name)
        with open(path, encoding="utf-8", errors="replace") as fh:
            for lineno, line in enumerate(fh, 1):
                m = GROUP.match(line)
                if m:
                    var, parent, prefix = m.group(1), m.group(2), m.group(3)
                    # s.router.Group("/api") -> parent is "router", not a group.
                    base = groups.get(parent, "") if parent not in ROOT_RECEIVERS else ""
                    groups[var] = base + prefix
                    continue
                m = ROUTE.match(line)
                if m:
                    var, method, sub, handler = m.groups()
                    prefix = groups.get(var, "")
                    if var in ROOT_RECEIVERS or var == "s":
                        prefix = ""
                    full = (prefix + sub) or "/"
                    full = re.sub(r"//+", "/", full)
                    routes.append({
                        "method": method,
                        "path": full,
                        "handler": handler.strip().rstrip(","),
                        "source": "%s:%d" % (name, lineno),
                        "params": re.findall(r"[:*](\w+)", full),
                    })

    seen, unique = set(), []
    for r in routes:
        key = (r["method"], r["path"])
        if key not in seen:
            seen.add(key)
            unique.append(r)
    unique.sort(key=lambda r: (r["path"], r["method"]))
    return unique


# Per-route field exclusions, with the reason each one is defensible.
#
# Every entry here was earned: it appeared as a difference when two IDENTICAL Go
# instances were compared against each other, which proves the field varies for
# reasons that have nothing to do with the port. Anything that does NOT show up
# in that twin run has no business being added here.
# A few routes need an argument to be comparable at all. Written next to the
# reason, like the exclusions.
QUERIES = {
    # The default path is "/", and the two containers are built from different
    # base images, so their root listings legitimately differ. /configs is the
    # same directory mounted into both.
    "/api/fs/browse": {"path": "/configs"},
}

IGNORES = {
    "/api/agents": (
        ["$[*].interfaces[*].ip", "$[*].interfaces[*].addresses[*]"],
        "the container's own address, different for every instance",
    ),
    "/api/network/interfaces": (
        ["$.interfaces[*].ip", "$.interfaces[*].addresses[*]"],
        "same: the address belongs to the instance, not to the answer",
    ),
    "/api/network/engines": (
        ["$.engines", "$.engines[*]*", "$.measured_at"],
        "a periodic probe; whichever instance has not measured yet answers empty",
    ),
    "/api/benchmark/current": (
        ["$.arc_*", "$.*_per_sec", "$.*_pct"],
        "host ZFS ARC counters, sampled at two different instants",
    ),
    "/api/health/anomalies": (
        ["$.generated_at", "$.gc_cpu_pct", "$.scan_duration_ms"],
        "an instant, a Go runtime metric with no equivalent in Rust, and how "
        "long the sweep took -- a duration two instances cannot agree on, and "
        "which two Go instances would not agree on either",
    ),
    "/api/logs": (
        ["$.entries", "$.entries[*]*"],
        "each process has its own log; only the envelope is comparable",
    ),
    "/api/drain/status": (
        ["$.disk_total", "$.disk_used", "$.disk_used_pct"],
        "both sides stat the SAME mounted directory, and ZFS reports a total "
        "that moves with the pool: observed drifting by exactly one 131072-byte "
        "block between two calls seconds apart",
    ),
    "/api/port-forward": (
        ["$.*_reach.at"],
        "the instant of the last reachability probe, per instance",
    ),
}


def surface(path):
    """Which half of the API a route belongs to. The qBit shim is the risky one:
    the *arr stack and cross-seed parse it and never report a mismatch."""
    if path.startswith("/api/v2"):
        return "qbit-shim"
    if path.startswith("/api"):
        return "native"
    return "ui"


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--repo", default=".")
    parser.add_argument("--json", help="write the full table here")
    parser.add_argument("--requests", help="write a paritydiff request file here")
    args = parser.parse_args()

    routes = extract(args.repo)

    counts = {}
    for r in routes:
        counts[surface(r["path"])] = counts.get(surface(r["path"]), 0) + 1
    print("%d routes: %s" % (len(routes),
                             ", ".join("%s %d" % (k, v) for k, v in sorted(counts.items()))))

    by_method = {}
    for r in routes:
        by_method[r["method"]] = by_method.get(r["method"], 0) + 1
    print("by method: %s" % ", ".join("%s %d" % kv for kv in sorted(by_method.items())))

    if args.json:
        with open(args.json, "w") as fh:
            json.dump(routes, fh, indent=2)
        print("table -> %s" % args.json)

    if args.requests:
        # Only parameterless GETs are generated automatically. A GET taking an
        # :info_hash needs a hash that exists in the bench store, and a POST
        # mutates -- both are added deliberately, not guessed here.
        reqs = []
        for r in routes:
            if r["method"] != "GET" or r["params"] or surface(r["path"]) == "ui":
                continue
            entry = {"name": "%s %s" % (r["method"], r["path"]),
                     "method": "GET", "path": r["path"]}
            if r["path"] in QUERIES:
                entry["query"] = QUERIES[r["path"]]
            if r["path"] in IGNORES:
                patterns, why = IGNORES[r["path"]]
                entry["ignore"] = patterns
                entry["ignore_reason"] = why
            reqs.append(entry)
        with open(args.requests, "w") as fh:
            json.dump(reqs, fh, indent=2)
        print("%d parameterless GETs -> %s" % (len(reqs), args.requests))
        skipped = [r for r in routes if r["method"] == "GET" and r["params"]]
        print("%d parameterised GETs need a fixture and are NOT generated" % len(skipped))

    return 0


if __name__ == "__main__":
    sys.exit(main())
