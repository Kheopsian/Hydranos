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

/// Who the peer says it is. The table lives in `peerclient`, which knows the
/// conventions BEFORE Azureus as well as the codes after it -- this used to be
/// six entries copied into two files, and every other client read as raw bytes.
pub fn client_from_peer_id(pid: &[u8; 20]) -> String {
    super::peerclient::identify(pid)
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::torrent::meta::{PeerStats, TorrentMeta};
    use std::path::PathBuf;

    fn meta(num_pieces: u32) -> TorrentMeta {
        TorrentMeta {
            info_hash: [7u8; 20],
            name: "t".into(),
            num_pieces,
            piece_length: 16384,
            total_size: num_pieces as u64 * 16384,
            files: Vec::new(),
            trackers: Vec::new(),
            url_list: Vec::new(),
            private: false,
            multi_file: false,
            info_dict_len: 0,
        }
    }

    fn seeding(num_pieces: u32) -> TorrentState {
        let t = TorrentState::new(meta(num_pieces), PathBuf::from("/tmp"), true);
        t.status
            .store(TorrentStatus::Seeding as u8, Ordering::Relaxed);
        t
    }

    /// Register a peer on the torrent, the way a real session's RAII guard does.
    fn peer(t: &TorrentState, n: u8, have: u32, interested: bool, bytes: u64) -> Arc<PeerStats> {
        let addr: std::net::SocketAddr = format!("93.184.216.{n}:6881").parse().unwrap();
        let s = Arc::new(PeerStats::new(addr, [n; 20], "test".into(), false, false));
        s.num_pieces_have.store(have, Ordering::Relaxed);
        s.interested.store(interested, Ordering::Relaxed);
        s.uploaded_last_tick.store(bytes, Ordering::Relaxed);
        t.peer_stats.insert(addr, s.clone());
        s
    }

    /// Choking is about who gets our upload, and a torrent we are still
    /// fetching has little to give. Ranking its peers would be work that
    /// decides nothing.
    #[test]
    fn a_torrent_that_is_not_seeding_is_left_alone() {
        let t = TorrentState::new(meta(100), PathBuf::from("/tmp"), false);
        t.status
            .store(TorrentStatus::Downloading as u8, Ordering::Relaxed);
        peer(&t, 1, 0, true, 0);
        assert_eq!(tick_torrent(&t, 4), (0, 0, false));
    }

    #[test]
    fn no_interested_peer_is_nothing_to_decide() {
        let t = seeding(100);
        peer(&t, 1, 50, false, 0);
        assert_eq!(tick_torrent(&t, 4), (0, 0, false), "nobody asked for anything");
    }

    /// The point of the ranking: bytes go to whoever needs them most. A peer
    /// holding nothing outranks one that is nearly done, whatever their speed,
    /// because rarity carries 0.7 of the score and speed only 0.3.
    #[test]
    fn the_peer_that_needs_us_most_is_unchoked_first() {
        let t = seeding(100);
        let empty = peer(&t, 1, 0, true, 0);
        let nearly_done = peer(&t, 2, 99, true, 1_000_000);

        let (unchoked, choked, had) = tick_torrent(&t, 1);
        assert!(had);
        assert_eq!((unchoked, choked), (1, 0), "one let through, one already choked");
        assert!(!empty.choked.load(Ordering::Relaxed), "the one with nothing");
        assert!(
            nearly_done.choked.load(Ordering::Relaxed),
            "a peer that is almost a seed does not outrank one that has nothing, \
             even uploading a megabyte a tick"
        );
    }

    /// Between two peers that need us equally, the one actually taking bytes
    /// wins. That is the whole of the speed term.
    #[test]
    fn speed_breaks_a_tie_between_equal_needs() {
        let t = seeding(100);
        let idle = peer(&t, 1, 50, true, 0);
        let busy = peer(&t, 2, 50, true, 999_999);

        tick_torrent(&t, 1);
        assert!(!busy.choked.load(Ordering::Relaxed), "the one using the connection");
        assert!(idle.choked.load(Ordering::Relaxed));
    }

    /// The cap is the point: unchoking everyone would spread the upstream so
    /// thin that nobody gets a usable rate.
    #[test]
    fn only_the_top_slots_are_let_through() {
        let t = seeding(100);
        let peers: Vec<_> = (1..=5).map(|i| peer(&t, i, i as u32 * 10, true, 0)).collect();

        let (unchoked, _, _) = tick_torrent(&t, 2);
        assert_eq!(unchoked, 2);
        let open = peers.iter().filter(|p| !p.choked.load(Ordering::Relaxed)).count();
        assert_eq!(open, 2, "two slots, two peers");
    }

    /// The speed term is a rate over one tick, so the window has to be closed.
    /// Leaving it would make a peer that was fast once look fast forever.
    #[test]
    fn the_tick_window_is_reset_for_the_next_one() {
        let t = seeding(100);
        let p = peer(&t, 1, 10, true, 500_000);
        tick_torrent(&t, 4);
        assert_eq!(p.uploaded_last_tick.load(Ordering::Relaxed), 0);
    }

    /// A decision that does not change anything must not bump the generation:
    /// the peer task emits a Choke or Unchoke on the wire every time it moves.
    #[test]
    fn an_unchanged_decision_sends_nothing() {
        let t = seeding(100);
        let p = peer(&t, 1, 0, true, 0);
        let (first, _, _) = tick_torrent(&t, 4);
        assert_eq!(first, 1, "unchoked once");
        let gen_after_first = p.choking_gen.load(Ordering::Relaxed);

        let (again, choked_again, _) = tick_torrent(&t, 4);
        assert_eq!((again, choked_again), (0, 0), "nothing changed");
        assert_eq!(
            p.choking_gen.load(Ordering::Relaxed),
            gen_after_first,
            "the generation only moves when the wire has to"
        );
    }

    /// A torrent with no pieces at all must not divide by it.
    #[test]
    fn a_torrent_of_no_pieces_does_not_divide_by_zero() {
        let t = seeding(0);
        let p = peer(&t, 1, 0, true, 0);
        let (unchoked, _, had) = tick_torrent(&t, 4);
        assert!(had);
        assert_eq!(unchoked, 1);
        assert!(!p.choked.load(Ordering::Relaxed));
    }

    // -----------------------------------------------------------------------
    // count_bitfield_pieces
    // -----------------------------------------------------------------------

    /// BEP 3: the high bit of the first byte is piece 0.
    #[test]
    fn a_bitfield_is_read_high_bit_first() {
        assert_eq!(count_bitfield_pieces(&[0b1000_0000], 8), 1);
        assert_eq!(count_bitfield_pieces(&[0b0000_0001], 8), 1);
        assert_eq!(count_bitfield_pieces(&[0b1111_1111], 8), 8);
    }

    /// BEP 3 pads the last byte with zeroes, but a peer may set them. Counting
    /// them would make a peer look like it holds pieces the torrent does not
    /// have -- and `is_seed` is derived from this count.
    #[test]
    fn the_padding_of_the_last_byte_is_not_counted() {
        // Ten pieces: eight in the first byte, two in the second, six padding
        // bits that this peer has wrongly set.
        assert_eq!(count_bitfield_pieces(&[0xFF, 0xFF], 10), 10);
        assert_eq!(count_bitfield_pieces(&[0x00, 0xFF], 10), 2);
    }

    #[test]
    fn an_empty_or_short_bitfield_counts_what_is_there() {
        assert_eq!(count_bitfield_pieces(&[], 100), 0);
        assert_eq!(count_bitfield_pieces(&[0xFF], 100), 8, "one byte, eight pieces");
    }
}
