#!/usr/bin/env python3
"""How much of the Go route table the Rust binary actually serves.

Counted from the two sources of truth -- routes.json, extracted from the Go
router, and the .route() calls in the Rust one -- rather than from a tally kept
by hand. A hand-kept figure drifts, and it drifts optimistically: the parity
bench only drives parameterless GETs, so counting "bench green" as "ported"
quietly omitted nine parameterised GETs for a whole day.

    coverage.py [--repo PATH] [--missing]
"""

import argparse
import collections
import json
import os
import re
import sys


def normalise(path):
    """Collapse path parameters so :info_hash and {info_hash} compare equal."""
    return re.sub(r"[:{][a-z_]+}?", "{p}", path)


def rust_routes(repo):
    src = open(os.path.join(repo, "typhon-engine/src/hydra/api.rs")).read()
    routes = set()
    for match in re.finditer(r'\.route\("([^"]+)"\s*,\s*(.+?)\)\s*$', src, re.M):
        path, spec = match.group(1), match.group(2)
        for method in re.findall(r"\b(get|post|put|delete|any)\s*\(", spec):
            routes.add((method.upper(), normalise(path)))
    return routes


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--repo", default=".")
    parser.add_argument("--missing", action="store_true", help="list what is left")
    args = parser.parse_args()

    go = json.load(open(os.path.join(args.repo, "tools/paritydiff/routes.json")))
    wanted = {(r["method"], normalise(r["path"])) for r in go}
    have = rust_routes(args.repo)

    covered, missing = 0, []
    for method, path in sorted(wanted):
        # A Go "Any" route accepts every verb; on the Rust side either an any()
        # or a get() answers the calls that matter.
        if method == "Any":
            ok = ("ANY", path) in have or ("GET", path) in have
        else:
            ok = (method, path) in have or ("ANY", path) in have
        if ok:
            covered += 1
        else:
            missing.append((method, path))

    print("routes: %d/%d covered (%.0f%%), %d left"
          % (covered, len(wanted), 100 * covered / len(wanted), len(missing)))
    by_method = collections.Counter(m for m, _ in missing)
    print("left by method: %s"
          % ", ".join("%s %d" % kv for kv in sorted(by_method.items())))

    if args.missing:
        for method, path in missing:
            print("  %-7s %s" % (method, path))
    return 0


if __name__ == "__main__":
    sys.exit(main())
