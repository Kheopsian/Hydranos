# Fuzzing the parsers

libFuzzer targets (via `cargo-fuzz`) for everything the engine parses that
arrives from outside. A crate of its own: it has its own `[workspace]` and its
own `Cargo.lock`, so `libfuzzer-sys` never enters typhon-engine's lockfile, and
`cargo test --locked` / `cargo llvm-cov` on `typhon-engine/` do not see it.

| target | what it feeds |
| --- | --- |
| `bencode_decode`, `bencode_decode_prefix` | the bencode decoder |
| `metainfo_parse`, `metainfo_info_dict`, `metainfo_piece_hashes` | `.torrent` bytes |
| `merkle_root` | BEP 52 merkle root (first byte = leaf count exponent) |
| `ext_handshake`, `metadata_msg`, `pex` | BEP 10 / 9 / 11 payloads |
| `handshake` | the 68-byte handshake |
| `peer_message` | the length-prefixed wire codec and `Message::decode` |
| `http_announce`, `http_scrape` | HTTP tracker reply bodies |
| `udp_tracker` | BEP 15 connect / announce / scrape replies |
| `mse_handshake` | the MSE / PE handshake (BEP 8), both sides, with fixed keys so the input reaches the stages behind the key exchange |
| `lsd_announce` | BEP 14 `BT-SEARCH` datagrams |
| `proxy_protocol` | the PROXY protocol v2 header in front of an inbound handshake |
| `magnet_uri` | `magnet:?` links (`typhon_engine::magnet_uri`) |

Not covered: `with_trackers` / `without_trackers` (`hydra/export.rs`), which
also needs `crate::store` and so cannot be compiled on its own. Unit tests only.
(`src/magnet.rs` in the engine is the resolution job, not a parser: the magnet
URI reader is `magnet_uri.rs`, moved out of the binary crate so it is reachable.)

`mse_handshake` is the one target that does more than hand bytes over: the
handshake makes a random key pair per connection, so the harness uses fixed
private keys (`fuzz_api::mse_*`, seeds in place of the random generator) and
encrypts the input with the session key as the real peer would. Without that
no input could get past the `req1` hash. Its seeds are the plaintext of the
encrypted part; a `raw` selector bit sends the input unencrypted.

Parsers that are not `pub` are reached through `typhon_engine::fuzz_api`, a
module compiled only under `--cfg fuzzing` (which cargo-fuzz sets): thin
forwarders, no logic, absent from every normal build.

## Run

Nightly Rust and `cargo install cargo-fuzz --locked`. From the repository root
(there is no root crate, so cargo-fuzz must be told where `fuzz/` is):

    cargo fuzz run --fuzz-dir fuzz bencode_decode -- -max_total_time=60 -ignore_ooms=1

CI (`.github/workflows/fuzz.yml`) runs every target for 10 minutes, weekly.

On a server with no toolchain, build an image once and mount the checkout:

    docker run --rm --network none --memory 4g -v "$PWD":/src -w /src \
      -e CARGO_TARGET_DIR=/tmp/fz-target <image with nightly + cargo-fuzz> \
      cargo fuzz run --fuzz-dir fuzz <target> -- -max_total_time=3600 -ignore_ooms=1

(Fetch the crates once with the network on, `cargo fetch` in `fuzz/` with
`CARGO_HOME` on a volume, then run offline with `CARGO_NET_OFFLINE=true`.)

`--network none`: the targets never touch a socket. Use a target dir of your own
and delete it afterwards. Run the long ones when a commit touches a parser, not
on a timer.

## Triage

- `crash-*`, `leak-*`, `timeout-*` in `fuzz/artifacts/<target>/`: real. Replay
  with `cargo fuzz run <target> <file>`, minimise with `cargo fuzz tmin`.
- `oom-*`: noise under ASAN. libFuzzer stops on its own `-rss_limit_mb` because
  ASAN's quarantine inflates RSS to ~2 GB for ~25 MB of live heap; it is not the
  kernel OOM killer. Replay the input once (it takes milliseconds and the RSS
  stays flat) before believing otherwise. `-ignore_ooms=1` keeps it quiet.

**Every crash becomes a normal unit test** in `typhon-engine` (the minimised
input inlined as bytes) that fails first, then the fix. A fuzzer finding that
is not a test comes back.

## Seeds

`corpus/` holds a few tiny valid inputs per target, written by `gen_corpus.py`
(deterministic: rerun it to regenerate). They are generated, never taken from a
real `.torrent` or tracker: those carry private announce URLs and passkeys.
Hosts are `*.example.invalid`, keys are zeros. Files libFuzzer adds during a run
(hex names) are git-ignored.
