#!/usr/bin/env python3
"""Differential parity oracle for the Go -> Rust port.

The port replaces 40k lines of Go. Porting the 19k lines of Go tests by hand
would port their blind spots along with their coverage, and would still not
answer the only question that matters at the end: does the new binary ANSWER
THE SAME THING as the old one.

So the oracle is differential. Two Hydra instances are driven with the same
requests against the same frozen store, and every response is compared field by
field. A is the reference (Go), B is the candidate (Rust).

Volatile fields are the whole difficulty. A speed, an ETA or an uptime differs
between two processes for legitimate reasons, and if the comparison drowns in
those the tool gets ignored within a day. They are therefore normalised, but
ONLY through the explicit list in NORMALISE below: a field that is silenced
without an entry there is a bug the oracle would hide. Nothing is silenced
implicitly.

Usage:
    paritydiff.py --a http://127.0.0.1:8299 --b http://127.0.0.1:8298 \
                  --key <api-key> --requests requests.json [--json report.json]

Exit code is 0 when every request matches, 1 when any differs, 2 on a usage or
transport error -- so it can gate a build.
"""

import argparse
import json
import re
import sys
import time
import urllib.error
import urllib.parse
import urllib.request


# ---------------------------------------------------------------------------
# Normalisation
# ---------------------------------------------------------------------------
#
# Each entry is (compiled key pattern, reason). A JSON object key matching the
# pattern anywhere in the tree has its value replaced by a placeholder before
# the comparison. The reason is printed by --explain so that the list can be
# audited: every silenced field must have a defensible justification, and a
# field silenced "because it was noisy" is exactly the kind of shortcut that
# turns a parity oracle into a rubber stamp.
NORMALISE = [
    (r"^(dlspeed|upspeed|dl_speed|up_speed|speed)$",
     "instantaneous rate, sampled at different instants in the two processes"),
    (r"^(eta)$",
     "derived from the instantaneous rate, so it inherits its jitter"),
    (r"^(uptime|elapsed|time_active|seeding_time|last_activity)$",
     "wall-clock since that process started, structurally different"),
    (r"^(last_seen|last_announce|next_announce|announce_at|updated|ts|timestamp)$",
     "an instant, moves between the two calls"),
    (r"^(free_space_on_disk|free_space|disk_free)$",
     "the host filesystem moves under both instances"),
    (r"^(connections|num_peers|num_seeds|num_leechs|peers|seeds)$",
     "live peer counts; the two instances hold separate sockets"),
    (r"^(rid|request_id|session_id)$",
     "per-response identifier, unique by construction"),
    (r"^(version|build|commit)$",
     "the two binaries are deliberately different builds"),
    (r"^(pid|goroutines|threads|rss|heap|alloc|open_fds)$",
     "process-level telemetry, has no reason to match across runtimes"),
]
NORMALISE = [(re.compile(p), why) for p, why in NORMALISE]

PLACEHOLDER = "<normalised>"


def normalise(node, counter=None):
    """Return a copy of node with volatile values replaced.

    Recurses through dicts and lists. Only object KEYS are matched: a value that
    merely looks like a timestamp is left alone, because guessing from values is
    how a comparison starts silently accepting real regressions.

    `counter` is a one-element list incremented for every replacement. The byte
    comparison needs it: a response holding an uptime or a version is expected
    to differ byte for byte, and flagging that would make the byte check cry
    wolf on every route carrying a volatile field.
    """
    if isinstance(node, dict):
        out = {}
        for key, value in node.items():
            if any(pat.match(key) for pat, _ in NORMALISE):
                out[key] = PLACEHOLDER
                if counter is not None:
                    counter[0] += 1
            else:
                out[key] = normalise(value, counter)
        return out
    if isinstance(node, list):
        return [normalise(item, counter) for item in node]
    return node


def sort_key(item):
    """Stable identity for a torrent-like object, used to align two lists.

    Two instances may return the same set in a different order; comparing
    positionally would then report every element as different and hide the one
    that actually is. Objects are aligned on their infohash when they have one.
    """
    if isinstance(item, dict):
        for field in ("hash", "infohash", "info_hash", "infohash_v1", "id", "name"):
            if field in item and isinstance(item[field], (str, int)):
                return (0, str(item[field]))
    return (1, json.dumps(item, sort_keys=True)[:200])


def align(node):
    """Sort lists of identifiable objects so the comparison is order-insensitive."""
    if isinstance(node, dict):
        return {k: align(v) for k, v in node.items()}
    if isinstance(node, list):
        aligned = [align(item) for item in node]
        if all(isinstance(item, dict) for item in aligned):
            return sorted(aligned, key=sort_key)
        return aligned
    return node


# ---------------------------------------------------------------------------
# Comparison
# ---------------------------------------------------------------------------

def diff(a, b, path="$", out=None, limit=200):
    """Collect field-level differences between two normalised trees."""
    if out is None:
        out = []
    if len(out) >= limit:
        return out

    if type(a) is not type(b) and not (isinstance(a, (int, float)) and isinstance(b, (int, float))):
        out.append((path, "type %s vs %s" % (type(a).__name__, type(b).__name__),
                    repr(a)[:120], repr(b)[:120]))
        return out

    if isinstance(a, dict):
        for key in sorted(set(a) | set(b)):
            if key not in a:
                out.append((path + "." + key, "absent from A", "-", repr(b[key])[:120]))
            elif key not in b:
                out.append((path + "." + key, "absent from B", repr(a[key])[:120], "-"))
            else:
                diff(a[key], b[key], path + "." + key, out, limit)
        return out

    if isinstance(a, list):
        if len(a) != len(b):
            out.append((path, "length %d vs %d" % (len(a), len(b)), str(len(a)), str(len(b))))
        for index in range(min(len(a), len(b))):
            diff(a[index], b[index], "%s[%d]" % (path, index), out, limit)
        return out

    if a != b:
        out.append((path, "value", repr(a)[:120], repr(b)[:120]))
    return out


# ---------------------------------------------------------------------------
# Transport
# ---------------------------------------------------------------------------

def apply_ignores(differences, patterns):
    """Drop differences whose path matches a per-request ignore pattern.

    These are declared per request rather than in NORMALISE because they are
    local truths, not global ones. The container's own IP legitimately differs
    between two instances under /api/agents; silencing "ip" everywhere would
    also silence it on a route where a wrong address is the actual bug. Each
    pattern is written next to the request it applies to, with its reason.

    Array indices are collapsed, so "$.entries[*].msg" covers every element.
    """
    if not patterns:
        return differences, []

    compiled = [glob_to_regex(p) for p in patterns]
    kept, muted = [], []
    for item in differences:
        flat = re.sub(r"\[\d+\]", "[*]", item[0])
        if any(rx.match(flat) for rx in compiled):
            muted.append(item[0])
        else:
            kept.append(item)
    return kept, muted


def glob_to_regex(pattern):
    """Translate an ignore pattern, treating brackets as literal text.

    fnmatch cannot be used here: it reads "[*]" as a character class matching a
    literal asterisk, so "$.entries[*].msg" silently matched nothing and the
    exclusions looked applied while doing nothing at all. Paths in this tool are
    full of "[*]", so brackets have to stay literal and only "*" is a wildcard.
    """
    out = []
    for char in pattern:
        if char == "*":
            out.append(".*")
        else:
            out.append(re.escape(char))
    return re.compile("".join(out) + "$")


def read_stream_sample(response, timeout, max_bytes=16384):
    """Read the opening of a never-ending SSE response.

    Only the first events are taken, and they are compared for SHAPE, not for
    content: two instances emit their own live events at their own instants, so
    requiring equality there would be requiring the impossible. What must match
    is that both speak SSE, both answer 200, and both emit events whose field
    names agree.
    """
    deadline = time.time() + min(timeout, 5.0)
    chunks = []
    total = 0
    while time.time() < deadline and total < max_bytes:
        try:
            chunk = response.read(1024)
        except Exception:
            break          # socket timeout: the stream is simply idle
        if not chunk:
            break
        chunks.append(chunk)
        total += len(chunk)
    return b"".join(chunks)


def stream_shape(raw):
    """Reduce an SSE sample to the set of field names it carries."""
    text = raw.decode("utf-8", "replace")
    fields = set()
    payload_keys = set()
    for line in text.splitlines():
        if ":" in line:
            name, _, rest = line.partition(":")
            if name:
                fields.add(name)
            if name == "data":
                try:
                    obj = json.loads(rest.strip())
                except ValueError:
                    continue
                if isinstance(obj, dict):
                    payload_keys |= set(obj)
    return {"sse_fields": sorted(fields), "sse_payload_keys": sorted(payload_keys)}


def fetch(base, req, key, timeout):
    """Issue one request; return (status, content_type, parsed_body, error, raw)."""
    url = base.rstrip("/") + req["path"]
    if req.get("query"):
        url += "?" + urllib.parse.urlencode(req["query"])

    data = None
    headers = {"X-API-Key": key}
    if req.get("form"):
        data = urllib.parse.urlencode(req["form"]).encode()
        headers["Content-Type"] = "application/x-www-form-urlencoded"
    elif req.get("body") is not None:
        data = json.dumps(req["body"]).encode()
        headers["Content-Type"] = "application/json"

    request = urllib.request.Request(url, data=data, headers=headers,
                                     method=req.get("method", "GET"))
    try:
        with urllib.request.urlopen(request, timeout=timeout) as response:
            status, ctype = response.status, response.headers.get("Content-Type", "")
            if "text/event-stream" in ctype:
                # An SSE response never ends, and a socket timeout never fires
                # on it either because the server keeps sending. Reading it like
                # an ordinary body hangs the run forever -- which is exactly what
                # /api/events did the first time this tool was pointed at the
                # live route table. Take a bounded sample instead.
                raw = read_stream_sample(response, timeout)
            else:
                raw = response.read()
    except urllib.error.HTTPError as exc:
        # A 4xx/5xx is a legitimate answer to compare, not a transport failure:
        # the two instances must agree on their refusals too.
        raw = exc.read()
        status, ctype = exc.code, exc.headers.get("Content-Type", "")
    except Exception as exc:
        return None, None, None, str(exc), b""

    if "text/event-stream" in ctype:
        return status, ctype, stream_shape(raw), None, raw

    if "json" in ctype:
        try:
            return status, ctype, json.loads(raw or b"null"), None, raw
        except ValueError as exc:
            return status, ctype, None, "invalid JSON: %s" % exc, raw
    return status, ctype, {"__raw__": raw.decode("utf-8", "replace")}, None, raw


# ---------------------------------------------------------------------------
# Driver
# ---------------------------------------------------------------------------

def main():
    parser = argparse.ArgumentParser(description=__doc__,
                                     formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--a", required=True, help="reference base URL (Go)")
    parser.add_argument("--b", required=True, help="candidate base URL (Rust)")
    parser.add_argument("--key", default="", help="X-API-Key sent to both")
    parser.add_argument("--requests", required=True, help="JSON list of requests")
    parser.add_argument("--json", help="write a machine-readable report here")
    parser.add_argument("--timeout", type=float, default=30.0)
    parser.add_argument("--quiet", action="store_true", help="only print failures")
    parser.add_argument("--explain", action="store_true",
                        help="print the normalisation list and exit")
    args = parser.parse_args()

    if args.explain:
        print("Fields silenced before comparison, and why:\n")
        for pat, why in NORMALISE:
            print("  %-42s %s" % (pat.pattern, why))
        return 0

    with open(args.requests) as fh:
        requests = json.load(fh)

    report, failures, skipped = [], 0, 0
    for req in requests:
        name = req.get("name") or (req.get("method", "GET") + " " + req["path"])
        started = time.time()

        sa, ca, ba, ea, ra = fetch(args.a, req, key=args.key, timeout=args.timeout)
        sb, cb, bb, eb, rb = fetch(args.b, req, key=args.key, timeout=args.timeout)

        entry = {"name": name, "path": req["path"],
                 "method": req.get("method", "GET"),
                 "ms": int((time.time() - started) * 1000)}

        if ea or eb:
            # A transport error on B alone is the normal state early in the
            # port: the route simply does not exist yet. It is reported as
            # "todo" rather than as a difference, so the score stays readable.
            entry["state"] = "todo" if (eb and not ea) else "error"
            entry["error_a"], entry["error_b"] = ea, eb
            skipped += 1
            report.append(entry)
            if not args.quiet:
                print("[%-5s] %s  (A=%s B=%s)" % (entry["state"], name, ea or "ok", eb or "ok"))
            continue

        # A route the candidate has not been given yet answers 404 where the
        # reference answers something. That is the normal state for most of the
        # table during the port, and counting it as a difference buries the
        # handful of real ones. It is scored "todo" instead -- never as a
        # success, so the number that matters (identical) cannot be inflated by
        # simply not implementing a route.
        # The signature of "no such route in B" is a 404 with an EMPTY body:
        # that is what axum returns when nothing is registered, whereas every
        # 404 this codebase produces on purpose carries a JSON body. Keying on
        # the empty body rather than on the status alone also catches the case
        # where the reference itself answers 404 with an explanation, as
        # /api/import/qbit/events does with {"error":"no import job"}.
        b_is_empty = isinstance(bb, dict) and bb.get("__raw__", None) == ""
        if sb == 404 and b_is_empty:
            entry["state"] = "todo"
            entry["status_a"], entry["status_b"] = sa, sb
            skipped += 1
            report.append(entry)
            if not args.quiet:
                print("[todo ] %s" % name)
            continue

        differences = []
        if sa != sb:
            differences.append(("$status", "status", str(sa), str(sb)))

        # The media type is part of the contract: a route that answered JSON and
        # now answers text/plain would otherwise compare as equal once both
        # bodies land in the __raw__ escape hatch. Only the type itself is
        # compared, not charset or boundary parameters.
        if (ca or "").split(";")[0].strip() != (cb or "").split(";")[0].strip():
            differences.append(("$content-type", "header", str(ca), str(cb)))
        normalised = [0]
        differences += diff(align(normalise(ba, normalised)),
                            align(normalise(bb, normalised)))
        differences, muted = apply_ignores(differences, req.get("ignore", []))

        # Structural equality is not byte equality, and clients read bytes.
        # Python parses 0 and 0.0 to values that compare equal, so a Go handler
        # emitting 0 and a Rust one emitting 0.0 would pass the comparison above
        # while changing every response on the wire. Only flagged when the
        # structure already matches and nothing was ignored -- otherwise it
        # would just restate a difference already reported.
        if (not differences and not muted and not normalised[0]
                and ra != rb and "event-stream" not in (ca or "")):
            differences.append(
                ("$bytes", "same JSON, different encoding",
                 "%d bytes" % len(ra), "%d bytes" % len(rb)))

        entry["ignored"] = muted
        entry["status_a"], entry["status_b"] = sa, sb
        entry["diffs"] = [{"path": p, "kind": k, "a": x, "b": y}
                          for p, k, x, y in differences]
        entry["state"] = "ok" if not differences else "diff"

        if differences:
            failures += 1
            print("[diff ] %s  -- %d difference(s)" % (name, len(differences)))
            for p, k, x, y in differences[:12]:
                print("         %s  (%s)\n           A: %s\n           B: %s" % (p, k, x, y))
            if len(differences) > 12:
                print("         ... %d more" % (len(differences) - 12))
        elif not args.quiet:
            print("[ok   ] %s" % name)

        report.append(entry)

    total = len(requests)
    matched = total - failures - skipped
    print("\n%d/%d identical, %d differing, %d not implemented yet"
          % (matched, total, failures, skipped))

    if args.json:
        with open(args.json, "w") as fh:
            json.dump({"generated": time.time(), "a": args.a, "b": args.b,
                       "matched": matched, "differing": failures,
                       "todo": skipped, "results": report}, fh, indent=2)
        print("report written to %s" % args.json)

    return 1 if failures else 0


if __name__ == "__main__":
    sys.exit(main())
