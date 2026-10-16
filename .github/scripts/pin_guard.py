#!/usr/bin/env python3
"""Everything the build pulls in from outside is named by content, not by label.

A tag like `actions/checkout@v4`, `rust:1-bookworm` or `rust:alpine` is a
pointer its owner can move. Whatever it points at on the day of a release runs
with that workflow's token and ends up in the binaries and the image. So:

  1. Every action a workflow `uses:` is pinned to a full commit SHA. The tag it
     stood for stays in a comment beside it, for the reader and for Dependabot.
  2. Every image the build runs -- a Dockerfile `FROM`, a `docker run` in a
     workflow -- carries an `@sha256:` digest.
  3. Every `cargo build`, `cargo check` and `cargo test` the CI, the release
     and the Dockerfile run is `--locked`: the Cargo.lock in the repository is
     the one that gets built, never one rewritten on the way.

The pins are kept fresh by .github/dependabot.yml, which proposes new SHAs and
digests as pull requests; this check only ever says no.

Run it with no arguments from the repository root. `--self-test` checks the
checker against known-good and known-bad inputs and touches nothing.
"""

import re
import shlex
import sys
from pathlib import Path

WORKFLOWS = Path(".github/workflows")
DOCKERFILE = Path("Dockerfile")

USES = re.compile(r"^\s*(?:-\s*)?uses:\s*([^\s#]+)")
PINNED_ACTION = re.compile(r"^[\w.-]+/[\w.-]+(?:/[\w./-]+)?@[0-9a-f]{40}$")
FROM = re.compile(r"^\s*FROM\s+(?:--platform=\S+\s+)?(\S+)", re.IGNORECASE)
DIGEST = re.compile(r"@sha256:[0-9a-f]{64}$")
CARGO = re.compile(r"\bcargo\s+(build|check|test)\b")
# The image of a `docker run`: the first word after the options. Options
# with a value are listed so their value is not taken for the image.
RUN_OPTS_WITH_VALUE = {"-v", "--volume", "-w", "--workdir", "-e", "--env", "--name",
                       "--network", "--entrypoint", "-u", "--user", "--platform", "-p"}


def check_uses(line: str) -> str | None:
    m = USES.match(line)
    if not m:
        return None
    ref = m.group(1)
    if ref.startswith("./") or ref.startswith("docker://"):
        return None if not ref.startswith("docker://") or DIGEST.search(ref) else f"{ref}: no @sha256 digest"
    return None if PINNED_ACTION.match(ref) else f"{ref}: not pinned to a commit SHA"


def check_from(line: str, stages: set[str]) -> str | None:
    m = FROM.match(line)
    if not m:
        return None
    image = m.group(1)
    if image in stages:
        return None
    return None if DIGEST.search(image) else f"FROM {image}: no @sha256 digest"


def split_words(line: str) -> list[str]:
    """Shell words, quotes honoured. The line may end inside a quote that the
    next lines close (`sh -c '...`): closing it here is enough to find the
    image, which comes before."""
    for tail in ("", "'", '"'):
        try:
            return shlex.split(line + tail)
        except ValueError:
            continue
    return line.split()


def docker_run_images(script: str) -> list[str]:
    """The images `docker run` starts in a shell snippet (continuations joined)."""
    joined = re.sub(r"\\\n\s*", " ", script)
    images = []
    for m in re.finditer(r"\bdocker\s+run\b([^\n]*)", joined):
        words = split_words(m.group(1))
        i = 0
        while i < len(words):
            w = words[i]
            if w in RUN_OPTS_WITH_VALUE:
                i += 2
                continue
            if w.startswith("-"):
                i += 1
                continue
            images.append(w)
            break
    return images


def check_run_image(image: str) -> str | None:
    # A variable is an image this workflow built itself (docker.yml checks
    # the one it just pushed), not one pulled by name.
    if image.startswith(("$", '"$')):
        return None
    return None if DIGEST.search(image) else f"docker run {image}: no @sha256 digest"


def check_cargo(line: str) -> str | None:
    if line.lstrip().startswith("#"):
        return None
    m = CARGO.search(line)
    if not m:
        return None
    return None if "--locked" in line else f"cargo {m.group(1)} without --locked"


def scan_workflow(text: str) -> list[str]:
    problems = []
    for n, line in enumerate(text.splitlines(), 1):
        for p in (check_uses(line), check_cargo(line)):
            if p:
                problems.append(f"{n}: {p}")
    for image in docker_run_images(text):
        p = check_run_image(image)
        if p:
            problems.append(p)
    return problems


def scan_dockerfile(text: str) -> list[str]:
    stages = set(re.findall(r"^\s*FROM\s+\S+\s+AS\s+(\S+)", text, re.IGNORECASE | re.MULTILINE))
    problems = []
    joined = re.sub(r"\\\n", " ", text)
    for n, line in enumerate(text.splitlines(), 1):
        p = check_from(line, stages)
        if p:
            problems.append(f"{n}: {p}")
    for line in joined.splitlines():
        p = check_cargo(line)
        if p:
            problems.append(p)
    return problems


def self_test() -> int:
    sha = "0" * 40
    dig = "@sha256:" + "a" * 64
    cases = [
        ("uses tag", scan_workflow("      - uses: actions/checkout@v4\n"), True),
        ("uses branch", scan_workflow("      - uses: dtolnay/rust-toolchain@stable\n"), True),
        ("uses short sha", scan_workflow("      - uses: actions/checkout@11d5960\n"), True),
        ("uses sha", scan_workflow(f"      - uses: actions/checkout@{sha} # v4\n"), False),
        ("uses sub-path sha", scan_workflow(f"      uses: github/codeql-action/init@{sha}\n"), False),
        ("uses local", scan_workflow("      - uses: ./.github/actions/x\n"), False),
        ("cargo unlocked", scan_workflow("        run: cargo test\n"), True),
        ("cargo locked", scan_workflow("        run: cargo test --locked\n"), False),
        ("cargo in comment", scan_workflow("      # cargo build is slow\n"), False),
        ("docker run tag", scan_workflow("          docker run --rm \\\n            -v a:/b -w /c \\\n            rust:alpine sh -c 'x'\n"), True),
        ("docker run quoted env", scan_workflow("          docker run --rm -e R=\"--cfg x\" rust:alpine sh -c '\n"), True),
        ("docker run digest", scan_workflow(f"          docker run --rm -v a:/b rust:alpine{dig} sh\n"), False),
        ("docker run own image", scan_workflow('          OUT="$(docker run --rm "$REF" --version)"\n'), False),
        ("FROM tag", scan_dockerfile("FROM rust:1-bookworm AS b\n"), True),
        ("FROM digest", scan_dockerfile(f"FROM rust:1-bookworm{dig} AS b\nFROM b\n"), False),
        ("Dockerfile cargo continued", scan_dockerfile("RUN --mount=x \\\n    cargo build --release \\\n    --bin h\n"), True),
        ("Dockerfile cargo locked", scan_dockerfile("RUN --mount=x \\\n    cargo build --release --locked \\\n    --bin h\n"), False),
    ]
    bad = [name for name, problems, want in cases if bool(problems) != want]
    for name in bad:
        print(f"self-test FAILED: {name}")
    print(f"self-test: {len(cases) - len(bad)}/{len(cases)} cases right")
    return 1 if bad else 0


def main() -> int:
    if "--self-test" in sys.argv:
        return self_test()
    problems = []
    for wf in sorted(WORKFLOWS.glob("*.yml")):
        problems += [f"{wf}:{p}" for p in scan_workflow(wf.read_text())]
    problems += [f"{DOCKERFILE}:{p}" for p in scan_dockerfile(DOCKERFILE.read_text())]
    for p in problems:
        print(p)
    if problems:
        print(f"{len(problems)} unpinned input(s)")
        return 1
    print("every action, image and cargo build is pinned")
    return 0


if __name__ == "__main__":
    sys.exit(main())
