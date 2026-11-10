//! Library entry point for typhon-engine.
//! Binary (main.rs) re-uses these modules via `use typhon_engine::*;`.
//! Benchmarks in `benches/*.rs` access internals via `typhon_engine::...`.
pub mod config;
pub mod crypto;
pub mod dht;
pub mod disk;
pub mod egress;
pub mod ipfilter;
pub mod lsd;
pub mod magnet;
pub mod netpin;
pub mod peer;
pub mod rpc;
pub mod runtime;
pub mod session;
pub mod stop;
pub mod torrent;
pub mod tracker;
pub mod tunneldns;
pub mod webseed;
pub mod wire;

// Entry points for the fuzz crate (../fuzz) onto parsers that are private to
// the crate. Only compiled under `--cfg fuzzing`, which cargo-fuzz sets and a
// normal build never does: nothing here exists in a shipped binary.
#[cfg(fuzzing)]
pub mod fuzz_api;
