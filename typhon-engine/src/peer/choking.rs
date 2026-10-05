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

/// How often the choker re-ranks. Ten seconds is BEP 3's suggestion and what
/// the loop always used.
pub const CHOKE_TICK: Duration = Duration::from_secs(10);

/// The choker of one engine.
///
/// Spawned for every engine and OFF unless `choking = true`: each tick reads
/// the engine's `PeerPolicy`, so the switch and the slot count move live, and
/// an engine with the choker off pays one atomic load every ten seconds.
///
/// Off by default because of what it did the one time it ran on a hoard
/// (2.4.13-typhon): re-ranking ~13k seeding torrents every tick and choking
/// all but the top four peers of each churned the peers so hard that upload
/// fell to a ceiling of ~300 transfers per peer-set instead of ~11k sustained.
/// A seedbox wants every interested peer served; a choker only pays when the
/// uplink is the bottleneck and a few fast peers should have it.
///
/// Switched off while running, it unchokes every peer it had choked -- leaving
/// them choked would turn "off" into "frozen in the last ranking".
pub async fn choking_loop(torrent_mgr: Arc<TorrentManager>) {
    let mut ticker = time::interval(CHOKE_TICK);
    // First tick fires immediately; skip it so peer tasks have time to register.
    ticker.tick().await;
    let mut was_active = false;
    loop {
        ticker.tick().await;
        was_active = choke_pass(torrent_mgr.policy(), || torrent_mgr.all(), was_active);
    }
}

/// One tick of the choker under `policy`. `was_active` is whether the last
/// tick ranked; answers whether this one did. The torrent list is asked for
/// only when there is something to do, so an engine with the choker off
/// never walks its library.
fn choke_pass(
    policy: &crate::peer::extension::PeerPolicy,
    torrents: impl FnOnce() -> Vec<Arc<TorrentState>>,
    was_active: bool,
) -> bool {
    let slots = if policy.choking() { policy.unchoke_slots() } else { None };
    match slots {
        Some(max_unchoked) => {
            if !was_active {
                info!("[choking] on: {} unchoke slots per seeding torrent", max_unchoked);
            }
            let all = torrents();
            let mut total_unchoked = 0usize;
            let mut total_choked = 0usize;
            let mut torrents_with_interested = 0usize;
            for t in &all {
                let (u, c, had_interested) = tick_torrent(t, max_unchoked);
                total_unchoked += u;
                total_choked += c;
                if had_interested {
                    torrents_with_interested += 1;
                }
            }
            debug!(
                "[choking] tick: +{} unchoke -{} choke across {} torrents (interested on {})",
                total_unchoked, total_choked, all.len(), torrents_with_interested
            );
            true
        }
        None if was_active => {
            let released: usize = torrents().iter().map(|t| release_torrent(t)).sum();
            info!("[choking] off: {} choked peers unchoked", released);
            false
        }
        None => false,
    }
}

/// Unchoke every peer of a torrent the choker had choked. Answers how many.
fn release_torrent(t: &TorrentState) -> usize {
    let mut n = 0;
    for e in t.peer_stats.iter() {
        let s = e.value();
        if s.choked.swap(false, Ordering::Relaxed) {
            s.choking_gen.fetch_add(1, Ordering::Relaxed);
            s.punch_wake.notify_one();
            n += 1;
        }
    }
    n
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
            // A seeding session keeps no timer (its choke tick is disabled),
            // so it only turns its loop -- where `choking_gen` is flushed to
            // the wire -- when something wakes it. A choked peer waiting for
            // its unchoke sends nothing, and without this it would wait for
            // its own idle timeout. `punch_wake`'s arm falls through to the
            // top of the loop with nothing to send when its outbox is empty.
            c.stats.punch_wake.notify_one();
            newly_unchoked += 1;
        } else if !should_unchoke && !was_choking {
            c.stats.choked.store(true, Ordering::Relaxed);
            c.stats.choking_gen.fetch_add(1, Ordering::Relaxed);
            c.stats.punch_wake.notify_one();
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
            v2: false,
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

    /// ⭐ A decision reaches an IDLE seeding session: its choke tick is
    /// disabled, so the choker has to wake it, or a choked peer waiting for
    /// its unchoke would sit there until its own idle timeout.
    #[tokio::test]
    async fn a_decision_wakes_the_session_that_must_send_it() {
        let t = seeding(100);
        let p = peer(&t, 1, 0, true, 0);
        p.choked.store(true, Ordering::Relaxed);
        tick_torrent(&t, 4);
        tokio::time::timeout(std::time::Duration::from_millis(100), p.punch_wake.notified())
            .await
            .expect("the unchoke must wake the session");
    }

    /// ⭐ Turning the choker off releases every peer it had choked. Left as
    /// they were, "off" would mean "frozen in the last ranking".
    #[test]
    fn switching_the_choker_off_unchokes_everyone() {
        let t = seeding(100);
        let peers: Vec<_> = (1..=5).map(|i| peer(&t, i, i as u32 * 10, true, 0)).collect();
        tick_torrent(&t, 1);
        assert_eq!(peers.iter().filter(|p| p.choked.load(Ordering::Relaxed)).count(), 4);
        assert_eq!(release_torrent(&t), 4);
        assert!(peers.iter().all(|p| !p.choked.load(Ordering::Relaxed)));
        assert_eq!(release_torrent(&t), 0, "a second release changes nothing");
    }

    /// The policy knob: off by default, 0 = the default four slots, negative =
    /// unlimited (the choker then has nothing to do).
    #[test]
    fn the_choker_is_off_by_default_and_slots_read_as_documented() {
        let p = crate::peer::extension::PeerPolicy::default();
        assert!(!p.choking(), "off unless asked");
        assert_eq!(p.unchoke_slots(), Some(4));
        p.set_unchoke_slots(0);
        assert_eq!(p.unchoke_slots(), Some(4));
        p.set_unchoke_slots(-1);
        assert_eq!(p.unchoke_slots(), None);
        p.set_unchoke_slots(12);
        assert_eq!(p.unchoke_slots(), Some(12));
    }

    /// ⭐ The switch, tick by tick, as the loop runs it. Off (the default):
    /// nobody is choked and the library is never even listed. On with two
    /// slots: all but two interested peers are choked. Off again: every one
    /// of them is released. Slots = -1 with the switch on chokes nobody.
    #[test]
    fn the_choker_switch_off_on_off() {
        let t = Arc::new(seeding(100));
        let peers: Vec<_> = (1..=5).map(|i| peer(&t, i, i as u32 * 10, true, 0)).collect();
        // What a session does with the choker off: it unchokes whoever is
        // interested, itself.
        peers.iter().for_each(|p| p.choked.store(false, Ordering::Relaxed));
        let choked = || peers.iter().filter(|p| p.choked.load(Ordering::Relaxed)).count();
        let policy = crate::peer::extension::PeerPolicy::default();
        let mut listed = 0;

        let active = choke_pass(&policy, || { listed += 1; vec![t.clone()] }, false);
        assert!(!active);
        assert_eq!((choked(), listed), (0, 0), "off: no choke, no walk of the library");

        policy.set_choking(true);
        policy.set_unchoke_slots(2);
        let active = choke_pass(&policy, || vec![t.clone()], active);
        assert!(active);
        assert_eq!(choked(), 3, "on, two slots");

        policy.set_choking(false);
        let active = choke_pass(&policy, || vec![t.clone()], active);
        assert!(!active);
        assert_eq!(choked(), 0, "off again: released");

        policy.set_choking(true);
        policy.set_unchoke_slots(-1);
        choke_pass(&policy, || vec![t.clone()], false);
        assert_eq!(choked(), 0, "unlimited slots choke nobody");
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
