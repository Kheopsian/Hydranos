# Changelog

All notable changes to Hydra are documented here. This project follows
[semantic versioning](https://semver.org).

## v3.6.1 — 2026-07-30

### Added
- **Import** is now a sub-tab of the Config tab — re-run the qBittorrent import any time, not just at the one-time first-run prompt.

### Fixed
- The qBittorrent import wizard "Skip" button is now styled (was an unstyled white button).

## v3.6.0 — 2026-07-29

### Added
- **Choose visible columns** — right-click a table header (Race or Hoard) to show/hide columns; remembered per table.
- **Changelog tab** — this page, rendered in-app from the repository CHANGELOG.
- **Update badge** — a small badge appears next to the version when a newer release exists. Checked server-side against GitHub (cached 6h, no browser CORS); opt out with .

## v3.5.0 — 2026-07-29

### Added
- **Column visibility** — right-click a table header (Race or Hoard) to pick which columns to show. Remembered per table.
- **Display units** — choose size units (binary `MiB` vs decimal `MB`) and speed units (bytes `MB/s` vs bits `Mbps`) in the Config tab; applied to every size/speed readout.
- **Change category without moving files** — right-clicking a hoard torrent now offers both `Change category` (re-tag only) and `Change category + move files`.

### Fixed
- The **Agents** tab now loads its data when you open it, not only when arriving via the `#agents` URL.

## v3.4.0 — 2026-07-29

### Added
- **Reworked Add form** — the category is the primary field and drives the mode + save path; mode/save-path move under an Advanced section. Clear confirmation on add.
- **Persistent table sort** — the column and direction you sort by are remembered across reloads, per table.
- **Config sub-tabs** — the settings menu is split into per-domain tabs instead of one long scroll.

### Fixed
- **No more re-downloading already-complete data** on a re-add: a recheck is no longer interrupted by the download slot manager (it used to pull 100–200 MB before finishing).
- **Torrent name shows immediately** in the list after adding — it previously appeared as the info-hash until a refresh (or two).
- **Category routing** — picking a category now routes to its engine role, instead of falling back to `race`.
- Fixed the category auto-fill in the Add form.

## v3.3.0 — 2026-07-29

### Added
- **Recheck** — adding a torrent whose data is already on disk now hash-checks it and seeds the valid pieces instead of blindly re-downloading over them. Missing/corrupt pieces are fetched; a complete torrent goes straight to seeding.
  - Available as the qBittorrent `POST /api/v2/torrents/recheck` and the native `POST /api/hoard/torrents/<hash>/verify` (hoard engine only).
  - `skip_checking=true` stays the trust-fast path for cross-seed / hardlinked data.
