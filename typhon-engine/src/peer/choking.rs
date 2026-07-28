//! Global choking engine — runs every `tick_interval` and re-ranks
//! interested peers per torrent. Top-N get unchoked, the rest get choked.
//!
//! Design: the engine only mutates `PeerStats` atomics (`choked`,
//! `choking_gen`, `uploaded_last_tick`). Each peer task observes
//! `choking_gen` and emits a Choke/Unchoke message on the wire when it
//! changes. This keeps the engine lock-free and the peer task cheap.
//!
//! Scoring (seeding torrent):
//!   score = 0.7 * rarity + 0.3 * speed
//!   rarity = 1 - (peer.num_pieces_have / total)     // leechers rank high
//!   speed  = peer.uploaded_last_tick / max_last_tick // active peers rank high
//!
//! Rarity-first biases toward peers who actually need our bytes; speed
//! provides a tie-break that rewards useful connections.

use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;
use tokio::time;
use tracing::{debug, info};

use crate::torrent::TorrentManager;
use crate::torrent::meta::{TorrentState, TorrentStatus};

#[derive(Clone, Debug)]
pub struct ChokingConfig {
    pub max_unchoked_per_torrent: usize,
    pub tick_interval: Duration,
}

impl Default for ChokingConfig {
    fn default() -> Self {
        Self {
            max_unchoked_per_torrent: 4,
            tick_interval: Duration::from_secs(10),
        }
    }
}

pub async fn choking_loop(torrent_mgr: Arc<TorrentManager>, cfg: ChokingConfig) {
    info!(
        "[choking] loop started: max_unchoked_per_torrent={}, tick={:?}",
        cfg.max_unchoked_per_torrent, cfg.tick_interval
    );
    let mut ticker = time::interval(cfg.tick_interval);
    // First tick fires immediately; skip it so peer tasks have time to register.
    ticker.tick().await;
    loop {
        ticker.tick().await;
        let mut total_unchoked = 0usize;
        let mut total_choked = 0usize;
        let mut torrents_with_interested = 0usize;
        for t in torrent_mgr.all() {
            let (u, c, had_interested) = tick_torrent(&t, cfg.max_unchoked_per_torrent);
            total_unchoked += u;
            total_choked += c;
            if had_interested {
                torrents_with_interested += 1;
            }
        }
        debug!(
            "[choking] tick: +{} unchoke -{} choke across {} torrents (interested on {})",
            total_unchoked, total_choked, torrent_mgr.count(), torrents_with_interested
        );
    }
}

/// Returns (newly_unchoked, newly_choked, had_any_interested_peer).
fn tick_torrent(t: &TorrentState, max_unchoked: usize) -> (usize, usize, bool) {
    let status = t.status.load(Ordering::Relaxed);
    // Only control choking on seeding torrents (where our upload = our job).
    // Downloading torrents: we're not uploading much anyway, and unchoking
    // everyone who's interested is cheaper than tracking churn.
    if status != TorrentStatus::Seeding as u8 {
        return (0, 0, false);
    }

    let num_pieces = t.meta.num_pieces();
    // Snapshot (addr, Arc<PeerStats>) for interested peers.
    struct Cand {
        stats: Arc<crate::torrent::meta::PeerStats>,
        score: f64,
        bytes_last_tick: u64,
    }
    let mut cands: Vec<Cand> = t
        .peer_stats
        .iter()
        .filter_map(|e| {
            let s = e.value().clone();
            if !s.interested.load(Ordering::Relaxed) {
                return None;
            }
            let bytes = s.uploaded_last_tick.load(Ordering::Relaxed);
            Some(Cand { stats: s, score: 0.0, bytes_last_tick: bytes })
        })
        .collect();

    if cands.is_empty() {
        return (0, 0, false);
    }

    let max_rate = cands.iter().map(|c| c.bytes_last_tick).max().unwrap_or(0);
    for c in cands.iter_mut() {
        let have = c.stats.num_pieces_have.load(Ordering::Relaxed) as f64;
        let rarity = if num_pieces == 0 {
            0.0
        } else {
            1.0 - (have / num_pieces as f64).min(1.0)
        };
        let speed = if max_rate == 0 {
            0.0
        } else {
            c.bytes_last_tick as f64 / max_rate as f64
        };
        c.score = 0.7 * rarity + 0.3 * speed;
    }
    cands.sort_by(|a, b| b.score.partial_cmp(&a.score).unwrap_or(std::cmp::Ordering::Equal));

    let mut newly_unchoked = 0usize;
    let mut newly_choked = 0usize;
    for (i, c) in cands.iter().enumerate() {
        let should_unchoke = i < max_unchoked;
        let was_choking = c.stats.choked.load(Ordering::Relaxed);
        if should_unchoke && was_choking {
            c.stats.choked.store(false, Ordering::Relaxed);
            c.stats.choking_gen.fetch_add(1, Ordering::Relaxed);
            newly_unchoked += 1;
        } else if !should_unchoke && !was_choking {
            c.stats.choked.store(true, Ordering::Relaxed);
            c.stats.choking_gen.fetch_add(1, Ordering::Relaxed);
            newly_choked += 1;
        }
        // Reset delta for the next tick window.
        c.stats.uploaded_last_tick.store(0, Ordering::Relaxed);
    }
    (newly_unchoked, newly_choked, true)
}

/// Best-effort peer-client identification from the 20-byte peer_id.
/// Recognizes the Azureus-style `-XX0000-` prefix used by mainline clients.
pub fn client_from_peer_id(pid: &[u8; 20]) -> String {
    if pid.len() >= 8 && pid[0] == b'-' && pid[7] == b'-' {
        let code = std::str::from_utf8(&pid[1..3]).unwrap_or("??");
        let ver = std::str::from_utf8(&pid[3..7]).unwrap_or("????");
        let name = match code {
            "qB" => "qBittorrent",
            "UT" => "uTorrent",
            "TR" => "Transmission",
            "DE" => "Deluge",
            "LT" => "libtorrent",
            "AZ" => "Azureus",
            _ => code,
        };
        format!("{} {}", name, ver)
    } else {
        String::new()
    }
}

/// Count set bits in a bitfield, capped at `num_pieces` (BT pads the last byte).
pub fn count_bitfield_pieces(bf: &[u8], num_pieces: u32) -> u32 {
    let mut count = 0u32;
    let cap = num_pieces as usize;
    for (i, byte) in bf.iter().enumerate() {
        let base = i * 8;
        if base >= cap {
            break;
        }
        for bit in 0..8 {
            if base + bit >= cap {
                break;
            }
            if byte & (1 << (7 - bit)) != 0 {
                count += 1;
            }
        }
    }
    count
}
