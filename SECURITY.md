# Security Policy

## Supported Versions

Hydra ships from a single active line. Only the latest release receives fixes.
If you are running anything older, upgrade before opening a bug report: the
first question on any issue will be whether it reproduces on the current tag.

| Version | Supported |
| ------- | --------- |
| latest release | yes |
| anything older | no  |

## Release Artifact Retention

Prebuilt archives and container images are kept for the **10 most recent
releases**. Older releases keep their tag, their notes and their changelog
entry, but their binaries and images are pruned automatically.

The project moves fast and every release is a full rebuild of both the Go
process and the Rust engine, so old artifacts add up quickly while nobody
runs them. Anything older than the retention window is still reproducible
from source: check out the tag and build it.

Pruning runs after every release: `.github/workflows/retention.yml` for the
archives, `.github/workflows/prune-images.yml` for the container images.

## Reporting a Vulnerability

Open a [security advisory](https://github.com/Kheopsian/Hydra/security/advisories/new)
rather than a public issue. Include the version, your topology (direct,
proxied or agent) and the steps to reproduce.

Hydra speaks BitTorrent to untrusted peers by design. Reports about peer
handling, the tracker client, the qBittorrent shim and the authentication
layer are the most valuable.
