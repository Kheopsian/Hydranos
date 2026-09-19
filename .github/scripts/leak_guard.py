#!/usr/bin/env python3
"""Refuse to publish something that identifies the person running this.

This repository is public. Three kinds of thing have no business in it, and
none of them were being checked until a release found one by hand:

  1. A real public IP address. One belonging to the maintainer's own line sat
     in three files from 2026-07-30 -- entered, of all places, by a commit
     fixing a different leak -- and was public for seven weeks. It was in
     TESTS, where a documentation address would have done exactly the same
     job. A public repository tied to a named account plus the address of the
     machine it runs on is an identification, not a configuration detail.

  2. A credential. API keys, passkeys, private keys, provider tokens.

  3. Anything from the maintainer's own denylist -- a name, a topic, a
     hostname. Those CANNOT live in this file.

⚠ POINT 3 IS THE SUBTLE ONE. A public check that reads

      if "<a person's name>" in text: fail

  publishes that name, and states that its owner wants it hidden. The same is
  true of an address: writing it into the pattern puts it in the repository
  the pattern defends. So the specific values are NOT here. They arrive in
  `LEAK_DENYLIST`, one per line, from a repository secret. Absent -- a fork, a
  pull request from outside -- the generic rules still run.

⚠ AND THE LOGS OF A PUBLIC REPOSITORY ARE PUBLIC. A scanner that prints what
  it matched publishes it in its own failure report, which is worse than not
  running: it turns one leak into a leak plus a signpost. Nothing here ever
  prints the matched text. A finding is a path, a line number and a rule name.

A line that must keep a real value carries `leak-ok` in a comment. That makes
the exception explicit, greppable and reviewable, instead of a silent hole.
"""

from __future__ import annotations

import ipaddress
import os
import re
import subprocess
import sys

# Directories whose contents are not ours to police.
SKIP_DIRS = ("vendor/", "node_modules/", ".git/", "third_party/")
# Binary-ish and lockfiles: no prose, huge, and full of false hex.
SKIP_SUFFIX = (".png", ".jpg", ".jpeg", ".gif", ".ico", ".pdf", ".woff",
               ".woff2", ".zip", ".gz", ".torrent", ".sum", ".lock", ".min.js")

ALLOW_MARK = "leak-ok"

IPV4_RE = re.compile(r"\b(?:\d{1,3}\.){3}\d{1,3}\b")
IPV6_RE = re.compile(r"\b(?:[0-9a-fA-F]{1,4}:){2,7}[0-9a-fA-F]{1,4}\b")
# 32 hex characters or more: an API key, a passkey, an info hash in a config.
HEX_RE = re.compile(r"\b[0-9a-fA-F]{32,}\b")
PRIVKEY_RE = re.compile(r"-----BEGIN [A-Z ]*PRIVATE KEY-----")
TOKEN_RE = re.compile(
    r"\b(?:AKIA[0-9A-Z]{16}"          # AWS access key
    r"|gh[pousr]_[A-Za-z0-9]{20,}"    # GitHub token
    r"|xox[abprs]-[A-Za-z0-9-]{10,}"  # Slack
    r"|eyJ[A-Za-z0-9_-]{10,}\.[A-Za-z0-9_-]{10,}\.)"  # JWT
)


def is_documentation_ipv4(text: str) -> bool:
    """Addresses a public file may name.

    RFC 5737 reserves three ranges for documentation precisely so that nobody
    has to borrow a real one. RFC 1918, loopback, link-local and multicast
    describe no particular machine on the internet. Everything else does.
    """
    try:
        ip = ipaddress.IPv4Address(text)
    except ValueError:
        return True  # Not an address at all (a version, a piece length).
    if ip.is_private or ip.is_loopback or ip.is_link_local:
        return True
    if ip.is_multicast or ip.is_unspecified or ip.is_reserved:
        return True
    for net in ("192.0.2.0/24", "198.51.100.0/24", "203.0.113.0/24"):
        if ip in ipaddress.IPv4Network(net):
            return True
    return False


def is_documentation_ipv6(text: str) -> bool:
    try:
        ip = ipaddress.IPv6Address(text)
    except ValueError:
        return True
    if ip.is_private or ip.is_loopback or ip.is_link_local or ip.is_multicast:
        return True
    return ip in ipaddress.IPv6Network("2001:db8::/32")


def tracked_files() -> list[str]:
    out = subprocess.run(["git", "ls-files"], capture_output=True, text=True, check=True)
    files = []
    for path in out.stdout.splitlines():
        if path.startswith(SKIP_DIRS) or path.endswith(SKIP_SUFFIX):
            continue
        files.append(path)
    return files


def denylist() -> list[str]:
    """The maintainer's own strings, from the environment. Never from here."""
    raw = os.environ.get("LEAK_DENYLIST", "")
    return [line.strip() for line in raw.splitlines() if line.strip()]


def scan(path: str, deny: list[str]) -> list[tuple[int, str]]:
    try:
        with open(path, "r", encoding="utf-8", errors="ignore") as fh:
            lines = fh.readlines()
    except OSError:
        return []

    found: list[tuple[int, str]] = []
    for n, line in enumerate(lines, 1):
        if ALLOW_MARK in line:
            continue
        for m in IPV4_RE.findall(line):
            if not is_documentation_ipv4(m):
                found.append((n, "public IPv4 literal"))
                break
        for m in IPV6_RE.findall(line):
            if not is_documentation_ipv6(m):
                found.append((n, "public IPv6 literal"))
                break
        if PRIVKEY_RE.search(line):
            found.append((n, "private key block"))
        if TOKEN_RE.search(line):
            found.append((n, "provider token"))
        if HEX_RE.search(line):
            found.append((n, "32+ hex characters (key or passkey?)"))
        low = line.lower()
        for needle in deny:
            if needle.lower() in low:
                # The needle is NOT echoed: printing it here would publish it.
                found.append((n, "denylisted string"))
                break
    return found


def main() -> int:
    deny = denylist()
    if not deny:
        print("leak guard: no LEAK_DENYLIST in the environment, generic rules only")

    findings: list[str] = []
    for path in tracked_files():
        for n, rule in scan(path, deny):
            findings.append(f"  {path}:{n}  {rule}")

    if not findings:
        print(f"leak guard: clean ({len(tracked_files())} files)")
        return 0

    print("leak guard: this must not be published\n")
    print("\n".join(sorted(findings)))
    print(
        "\nThe matched text is deliberately not shown: these logs are public.\n"
        "Open the file at the line given. Use a documentation address\n"
        "(RFC 5737: 203.0.113.0/24) or a placeholder. A value that genuinely\n"
        "has to stay gets `leak-ok` in a comment on its line."
    )
    return 1


if __name__ == "__main__":
    sys.exit(main())
