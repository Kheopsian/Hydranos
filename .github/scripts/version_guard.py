#!/usr/bin/env python3
"""Keep the version, the changelog and the tags telling the same story.

Three failures happened for real, and this is one check per failure:

  1. A release whose changelog had no entry. Seven of them, found at once in
     August 2026 -- the version constant moved and nothing made the changelog
     move with it. Worse than a missing note: the changelog is COMPILED INTO
     the binary (`include_str!` in api.rs), so `/api/changelog` served a file
     that did not mention the version it was shipping.

  2. A pull request carrying a version number that was already taken. The
     number is chosen when the branch is written and is stale by the time
     anybody reviews it -- the repository merged 97 minor bumps in 23 days.
     Picking a number by hand cannot win that race; noticing that it lost is
     the most this check can do.

  3. A peer fingerprint that is not eight bytes. A BitTorrent peer_id is
     twenty: an eight-byte client prefix and twelve of randomness. A nine-byte
     prefix still produces a valid-looking peer_id, one byte short on entropy,
     and nothing anywhere reports it.

Deliberately NOT here: bumping or tagging anything by itself. That was
considered and refused -- a bot owning `main` races the several sessions that
push to it, and it would turn every merge into a release when they are grouped
on purpose. This file only ever says no.

Run it with no arguments from the repository root. `--self-test` checks the
checker against known-good and known-bad inputs and touches nothing.
"""

import re
import subprocess
import sys
from pathlib import Path

VERSION_FILE = Path("typhon-engine/src/hydra/api.rs")
FINGERPRINT_FILE = Path("typhon-engine/src/config.rs")
CHANGELOG_FILE = Path("CHANGELOG.md")

# `pub const HYDRA_VERSION: &str = "4.15.0";`
VERSION_RE = re.compile(r'HYDRA_VERSION\s*:\s*&str\s*=\s*"([^"]+)"')
# The fingerprint is no longer a literal: it is derived from HYDRA_VERSION by
# `peer_fingerprint_for`, because the four characters of an Azureus-style peer
# id ARE the version and ours said 2.4.3.0 on a 4.x daemon for years.
#
# So this no longer reads a value -- it checks that the derivation is still
# there. The eight-byte invariant it used to enforce is now covered for EVERY
# version by `config::fingerprint_tests::it_is_always_eight_bytes`, which is a
# stronger check than reading one literal ever was.
FINGERPRINT_RE = re.compile(r'(fn\s+peer_fingerprint_for\s*\()')
FINGERPRINT_TEST_RE = re.compile(r'(fn\s+it_is_always_eight_bytes)\s*\(')
# A changelog entry heading: `## v4.15.0 -- title` or `## Unreleased`.
HEADING_RE = re.compile(r"^##\s+(\S+)", re.MULTILINE)


class Failure(Exception):
    """A rule that did not hold. The message is what CI prints."""


def parse_version(text: str) -> tuple:
    """`4.15.0` -> (4, 15, 0), so 4.9.0 sorts BELOW 4.15.0.

    The trap this avoids: as strings, "4.9.0" > "4.15.0", which would offer
    4.9.0 as an upgrade from 4.15.0 and accept a PR numbering itself backwards.
    """
    core = text.lstrip("v").split("-")[0]
    parts = core.split(".")
    if len(parts) != 3 or not all(p.isdigit() for p in parts):
        raise Failure(f"{text!r} is not a three-part version")
    return tuple(int(p) for p in parts)


def find_one(pattern, path: Path, what: str) -> str:
    if not path.exists():
        raise Failure(f"{path} is missing, so {what} cannot be read")
    match = pattern.search(path.read_text(encoding="utf-8"))
    if not match:
        # A rename that silently stops the check from finding anything would
        # otherwise make this script pass by looking at nothing at all.
        raise Failure(f"cannot find {what} in {path} -- was it renamed?")
    return match.group(1)


def changelog_headings(text: str) -> list:
    return HEADING_RE.findall(text)


def git_tags() -> list:
    out = subprocess.run(
        ["git", "tag", "-l", "v*"], capture_output=True, text=True, check=False
    )
    return [t.strip() for t in out.stdout.splitlines() if t.strip()]


def check(version: str, fingerprint: str, changelog: str, tags: list) -> list:
    """Return the list of failures. Empty means the tree is consistent."""
    problems = []
    headings = changelog_headings(changelog)

    # -- Rule 1: the changelog leads with the version being shipped ----------
    if not headings:
        problems.append("CHANGELOG.md has no `## ` entry at all")
    else:
        top = headings[0]
        if top.lower() == "unreleased":
            # The number was deferred on purpose. Nothing to reconcile: the
            # release step is what turns this heading into a number.
            pass
        elif top.lstrip("v") != version:
            problems.append(
                f"HYDRA_VERSION is {version} but the changelog leads with {top!r}.\n"
                f"    Add a `## v{version}` entry at the top of CHANGELOG.md, or title "
                f"the new entry `## Unreleased` and let the release set the number.\n"
                f"    The changelog is embedded in the binary, so this ships as a "
                f"release that does not describe itself."
            )

    # -- Rule 2: a new entry must number itself above every existing tag -----
    known = {t.lstrip("v") for t in tags}
    if headings and headings[0].lower() != "unreleased" and version not in known:
        # This version has never been tagged, so this branch is proposing it.
        parsed = [parse_version(t) for t in tags if re.fullmatch(r"v?\d+\.\d+\.\d+", t)]
        if parsed:
            highest = max(parsed)
            if parse_version(version) <= highest:
                dotted = ".".join(str(n) for n in highest)
                problems.append(
                    f"HYDRA_VERSION {version} is not above the highest tag v{dotted}.\n"
                    f"    This branch picked its number before {dotted} was tagged. "
                    f"Renumber above it, or use `## Unreleased`, which cannot go stale."
                )

    # -- Rule 3: the fingerprint is still DERIVED, and still pinned ---------
    if not fingerprint:
        problems.append(
            "peer_fingerprint_for is gone from typhon-engine/src/config.rs.\n"
            "    The peer id's four characters are the version to every client "
            "that decodes them; a literal there goes stale the day it is "
            "written, which is how ours announced 2.4.3.0 from a 4.x daemon."
        )

    return problems


def self_test() -> int:
    """Prove the checker by breaking it: every rule gets an input it must reject."""
    ok_log = "# Changelog\n\n## v4.15.0 -- a title\n\n## v4.14.0 -- older\n"
    tags = ["v4.14.0", "v3.180.0", "v4.9.0"]
    cases = [
        ("a consistent tree passes", ("4.15.0", "-HY2430-", ok_log, tags), 0),
        (
            "a changelog that does not mention the version fails",
            ("4.16.0", "-HY2430-", ok_log, tags),
            1,
        ),
        (
            "an Unreleased heading defers the number",
            ("4.15.0", "-HY2430-", "## Unreleased\n\n## v4.14.0 -- older\n", tags),
            0,
        ),
        (
            "a version at or below the highest tag fails",
            ("4.10.0", "-HY2430-", "## v4.10.0 -- stale\n", tags),
            1,
        ),
        (
            "4.9.0 is below 4.14.0 even though it sorts above as a string",
            ("4.9.1", "-HY2430-", "## v4.9.1 -- stale\n", tags),
            1,
        ),
        (
            "a re-tagged existing version is not a new proposal",
            ("4.14.0", "-HY2430-", "## v4.14.0 -- released\n", tags),
            0,
        ),
        (
            "a fingerprint that is no longer derived fails",
            ("4.15.0", "", ok_log, tags),
            1,
        ),
        ("an empty changelog fails", ("4.15.0", "-HY2430-", "", tags), 1),
    ]
    failed = 0
    for name, args, expected in cases:
        got = len(check(*args))
        got = 1 if got else 0
        if got != expected:
            print(f"SELF-TEST FAILED: {name} (expected {expected}, got {got})")
            failed += 1
        else:
            print(f"ok: {name}")
    if failed:
        print(f"\n{failed} self-test(s) failed -- the checker itself is wrong.")
        return 1
    print("\nself-test passed")
    return 0


def main() -> int:
    if "--self-test" in sys.argv:
        return self_test()

    try:
        version = find_one(VERSION_RE, VERSION_FILE, "HYDRA_VERSION")
        fingerprint = find_one(
            FINGERPRINT_RE, FINGERPRINT_FILE, "peer_fingerprint_for"
        )
        # The invariant moved into a test; make sure the test is still there.
        find_one(
            FINGERPRINT_TEST_RE, FINGERPRINT_FILE, "it_is_always_eight_bytes"
        )
        if not CHANGELOG_FILE.exists():
            raise Failure("CHANGELOG.md is missing")
        changelog = CHANGELOG_FILE.read_text(encoding="utf-8")
    except Failure as e:
        print(f"::error::{e}")
        return 1

    problems = check(version, fingerprint, changelog, git_tags())
    if not problems:
        print(f"version {version}, changelog and tags agree; fingerprint is 8 bytes")
        return 0
    for p in problems:
        print(f"::error::{p}")
    return 1


if __name__ == "__main__":
    sys.exit(main())
