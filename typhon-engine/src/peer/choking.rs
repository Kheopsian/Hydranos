//! Global choking engine -- runs every `tick_interval` and decides, per
//! seeding torrent, which interested peers are let through.
//!
//! Design: the engine only mutates `PeerStats` atomics (`choked`,
//! `choking_gen`, `choke_turn`, `choke_round`, `uploaded_last_tick`). Each
//! peer task observes `choking_gen` and emits a Choke/Unchoke message on the
//! wire when it changes. This keeps the engine lock-free and the peer task
//! cheap.
//!
//! Seeding: fair rotation, libtorrent's round robin for seeds. Every
//! interested peer waits in one queue, ordered by the round it joined it --
//! when it was first seen, or when its last turn ended. The head of the queue
//! gets a regular slot for `TURN_ROUNDS` rounds, then goes to the back. One
//! slot (out of the same count, when there are at least two) is optimistic:
//! a random waiting peer gets it for `OPTIMISTIC_ROUNDS`, so a peer far down
//! the queue does not have to wait its whole place. Ties in the queue go to
//! the lower address, never to the order the peer map hands them out.
//!
//! What a peer holds plays no part. The score this replaced ranked by how
//! little a peer had (`0.7 * rarity + 0.3 * speed`), so a peer at 99% stayed
//! choked behind every newcomer with nothing, and the same top N kept the
//! slots for as long as they stayed.

use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Duration;
use tokio::time;
use tracing::{debug, info};

use crate::torrent::meta::{PeerStats, TorrentState, TorrentStatus};
use crate::torrent::TorrentManager;

/// How often the choker re-ranks. Ten seconds is BEP 3's suggestion and what
/// the loop always used. One tick is one choker round.
pub const CHOKE_TICK: Duration = Duration::from_secs(10);

/// Rounds a regular turn lasts before its peer goes to the back of the queue:
/// thirty seconds at one round per tick.
const TURN_ROUNDS: u64 = 3;

/// Rounds the optimistic slot stays with one peer: BEP 3's thirty seconds.
const OPTIMISTIC_ROUNDS: u64 = 3;

/// What the choker last gave a peer, kept in `PeerStats::choke_turn`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
enum Turn {
    /// Waiting in the queue (or never seen).
    Queued = 0,
    /// A regular slot, begun at `choke_round`.
    Regular = 1,
    /// The optimistic slot, begun at `choke_round`.
    Optimistic = 2,
}

impl Turn {
    fn of(s: &PeerStats) -> Self {
        match s.choke_turn.load(Ordering::Relaxed) {
            1 => Self::Regular,
            2 => Self::Optimistic,
            _ => Self::Queued,
        }
    }
}

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
    // Rounds count from 1: a `choke_round` of 0 means "not seen yet".
    let mut round: u64 = 0;
    loop {
        if !crate::stop::global().tick(&mut ticker).await {
            return;
        }
        round += 1;
        was_active = choke_pass(
            torrent_mgr.policy(),
            || torrent_mgr.all(),
            was_active,
            round,
        );
    }
}

/// One tick of the choker under `policy`, as round `round`. `was_active` is
/// whether the last tick ranked; answers whether this one did. The torrent
/// list is asked for only when there is something to do, so an engine with
/// the choker off never walks its library.
fn choke_pass(
    policy: &crate::peer::extension::PeerPolicy,
    torrents: impl FnOnce() -> Vec<Arc<TorrentState>>,
    was_active: bool,
    round: u64,
) -> bool {
    // (slots per torrent, cap over all torrents). Either alone is enough to
    // run the choker; with neither it is off by another name. A cap with
    // unlimited slots per torrent still ranks every torrent.
    let limits = if policy.choking() {
        match (policy.unchoke_slots(), policy.max_uploads()) {
            (None, None) => None,
            (slots, cap) => Some((slots.unwrap_or(usize::MAX), cap)),
        }
    } else {
        None
    };
    match limits {
        Some((max_unchoked, cap)) => {
            if !was_active {
                let per_torrent = if max_unchoked == usize::MAX {
                    "unlimited".to_string()
                } else {
                    max_unchoked.to_string()
                };
                match cap {
                    Some(cap) => info!(
                        "[choking] on: {} unchoke slots per seeding torrent, at most {} unchoked in all",
                        per_torrent, cap
                    ),
                    None => info!(
                        "[choking] on: {} unchoke slots per seeding torrent",
                        per_torrent
                    ),
                }
            }
            let all = torrents();
            let (total_unchoked, total_choked, torrents_with_interested) = match cap {
                None => {
                    let mut total_unchoked = 0usize;
                    let mut total_choked = 0usize;
                    let mut torrents_with_interested = 0usize;
                    for t in &all {
                        let (u, c, had_interested) = tick_torrent(t, max_unchoked, round);
                        total_unchoked += u;
                        total_choked += c;
                        if had_interested {
                            torrents_with_interested += 1;
                        }
                    }
                    (total_unchoked, total_choked, torrents_with_interested)
                }
                Some(cap) => {
                    let refs: Vec<&TorrentState> = all.iter().map(|t| &**t).collect();
                    select(&refs, max_unchoked, cap, round)
                }
            };
            debug!(
                "[choking] tick: +{} unchoke -{} choke across {} torrents (interested on {})",
                total_unchoked,
                total_choked,
                all.len(),
                torrents_with_interested
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
    t.peers.for_each(|s| {
        if s.choked.swap(false, Ordering::Relaxed) {
            s.choking_gen.fetch_add(1, Ordering::Relaxed);
            s.punch_wake.notify_one();
            n += 1;
        }
    });
    n
}

/// Where an interested peer stands this round. The order is the queue's:
/// a peer in an unfinished regular turn keeps it first, then the waiting
/// peers, and last the holder of the optimistic slot, which only the
/// optimistic pick reaches.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum Standing {
    InTurn,
    Waiting,
    Optimistic,
}

/// An interested peer of a seeding torrent, and its place in the queue.
struct Cand {
    stats: Arc<PeerStats>,
    /// Index of its torrent in the pass.
    torrent: usize,
    standing: Standing,
    /// The round its turn began (in a turn) or it joined the queue (waiting).
    /// Rounds are global, so the order holds across torrents.
    since: u64,
}

impl Cand {
    fn key(&self) -> (Standing, u64, std::net::SocketAddr) {
        (self.standing, self.since, self.stats.addr)
    }
}

/// Where `s` stands in round `round`. A peer seen for the first time joins
/// the queue now. A turn that has run its rounds puts its peer back in the
/// queue at the round it ended -- behind everyone who was already waiting.
fn standing(s: &PeerStats, round: u64) -> (Standing, u64) {
    let mark = s.choke_round.load(Ordering::Relaxed);
    if mark == 0 {
        s.choke_round.store(round, Ordering::Relaxed);
        return (Standing::Waiting, round);
    }
    let open = !s.choked.load(Ordering::Relaxed);
    match Turn::of(s) {
        Turn::Queued => (Standing::Waiting, mark),
        Turn::Regular if open && round < mark + TURN_ROUNDS => (Standing::InTurn, mark),
        Turn::Regular => (Standing::Waiting, mark + TURN_ROUNDS),
        Turn::Optimistic if open && round < mark + OPTIMISTIC_ROUNDS => {
            (Standing::Optimistic, mark)
        }
        Turn::Optimistic => (Standing::Waiting, mark + OPTIMISTIC_ROUNDS),
    }
}

/// The interested peers of a seeding torrent, queue order. `None` for a
/// torrent that is not seeding (it is left alone); an empty list when nobody
/// is interested.
fn candidates(t: &TorrentState, torrent: usize, round: u64) -> Option<Vec<Cand>> {
    let status = t.status.load(Ordering::Relaxed);
    // Only control choking on seeding torrents (where our upload = our job).
    // Downloading torrents: we're not uploading much anyway, and unchoking
    // everyone who's interested is cheaper than tracking churn.
    if status != TorrentStatus::Seeding as u8 {
        return None;
    }
    let mut cands: Vec<Cand> = t
        .peers
        .snapshot()
        .into_iter()
        .filter(|s| s.interested.load(Ordering::Relaxed))
        .map(|s| {
            let (standing, since) = standing(&s, round);
            Cand {
                stats: s,
                torrent,
                standing,
                since,
            }
        })
        .collect();
    cands.sort_by_key(Cand::key);
    Some(cands)
}

/// What one candidate gets this round.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Grant {
    Regular,
    Optimistic,
    Choke,
}

/// Let one candidate through or choke it, telling its session only when that
/// changes something, record its turn, and close its tick window. Answers
/// (unchoked, choked) as 0 or 1 each.
fn apply_decision(c: &Cand, grant: Grant, round: u64) -> (usize, usize) {
    let s = &c.stats;
    match grant {
        // A turn already running keeps its start; any other begins now.
        Grant::Regular if c.standing != Standing::InTurn => {
            s.choke_turn.store(Turn::Regular as u8, Ordering::Relaxed);
            s.choke_round.store(round, Ordering::Relaxed);
        }
        Grant::Optimistic if c.standing != Standing::Optimistic => {
            s.choke_turn
                .store(Turn::Optimistic as u8, Ordering::Relaxed);
            s.choke_round.store(round, Ordering::Relaxed);
        }
        // Back in the queue, at the place it stands: the end of a finished
        // turn, the start of one cut short, or where it already waited.
        Grant::Choke => {
            s.choke_turn.store(Turn::Queued as u8, Ordering::Relaxed);
            s.choke_round.store(c.since, Ordering::Relaxed);
        }
        Grant::Regular | Grant::Optimistic => {}
    }
    let should_unchoke = grant != Grant::Choke;
    let mut out = (0, 0);
    let was_choking = s.choked.load(Ordering::Relaxed);
    if should_unchoke && was_choking {
        s.choked.store(false, Ordering::Relaxed);
        s.choking_gen.fetch_add(1, Ordering::Relaxed);
        // A seeding session keeps no timer (its choke tick is disabled),
        // so it only turns its loop -- where `choking_gen` is flushed to
        // the wire -- when something wakes it. A choked peer waiting for
        // its unchoke sends nothing, and without this it would wait for
        // its own idle timeout. `punch_wake`'s arm falls through to the
        // top of the loop with nothing to send when its outbox is empty.
        s.punch_wake.notify_one();
        out.0 = 1;
    } else if !should_unchoke && !was_choking {
        s.choked.store(true, Ordering::Relaxed);
        s.choking_gen.fetch_add(1, Ordering::Relaxed);
        s.punch_wake.notify_one();
        out.1 = 1;
    }
    // Reset delta for the next tick window.
    s.uploaded_last_tick.store(0, Ordering::Relaxed);
    out
}

/// One torrent under its own `max_unchoked` slots, as round `round`.
/// Returns (newly_unchoked, newly_choked, had_any_interested_peer).
fn tick_torrent(t: &TorrentState, max_unchoked: usize, round: u64) -> (usize, usize, bool) {
    let (u, c, with_interested) = select(&[t], max_unchoked, max_unchoked, round);
    (u, c, with_interested > 0)
}

/// One pass over `torrents`, at most `max_unchoked` let through per torrent
/// and `cap` in all. A torrent alone is `cap == max_unchoked`.
///
/// Each torrent's queue keeps its first `max_unchoked`; those are merged in
/// queue order (rounds are global, so the order holds across torrents) and
/// the first `cap - 1` get regular slots. The last slot is optimistic: it
/// stays with the peer that holds it while its rounds last, or else goes to
/// a random peer not let through yet whose torrent still has a slot. With a
/// single slot, or one peer left to give it to, it is a regular slot like the
/// others. Everybody else is choked. Returns
/// (newly_unchoked, newly_choked, torrents_with_an_interested_peer).
fn select(
    torrents: &[&TorrentState],
    max_unchoked: usize,
    cap: usize,
    round: u64,
) -> (usize, usize, usize) {
    let mut eligible: Vec<Cand> = Vec::new();
    let mut refused: Vec<Cand> = Vec::new();
    let mut with_interested = 0usize;
    for (i, t) in torrents.iter().enumerate() {
        let Some(mut cands) = candidates(t, i, round) else {
            continue;
        };
        if cands.is_empty() {
            continue;
        }
        with_interested += 1;
        refused.extend(cands.split_off(max_unchoked.min(cands.len())));
        eligible.extend(cands);
    }
    // Stable: equal keys (one address on two torrents) keep torrent order.
    eligible.sort_by_key(Cand::key);

    let reserve = usize::from(cap >= 2);
    let regular = (cap - reserve).min(eligible.len());
    refused.extend(eligible.split_off(regular));
    let mut taken = vec![0usize; torrents.len()];
    for c in &eligible {
        taken[c.torrent] += 1;
    }
    // Who the last slot may go to: not let through yet, and its torrent has room.
    let pool: Vec<usize> = (0..refused.len())
        .filter(|&i| reserve == 1 && taken[refused[i].torrent] < max_unchoked)
        .collect();
    let last = match pool.as_slice() {
        [] => None,
        [only] => Some((*only, Grant::Regular)),
        _ => {
            let held = pool
                .iter()
                .copied()
                .find(|&i| refused[i].standing == Standing::Optimistic);
            let pick = held.unwrap_or_else(|| {
                use rand::Rng;
                pool[rand::thread_rng().gen_range(0..pool.len())]
            });
            Some((pick, Grant::Optimistic))
        }
    };

    let mut newly_unchoked = 0usize;
    let mut newly_choked = 0usize;
    let mut tally = |(u, ch): (usize, usize)| {
        newly_unchoked += u;
        newly_choked += ch;
    };
    for c in &eligible {
        tally(apply_decision(c, Grant::Regular, round));
    }
    for (i, c) in refused.iter().enumerate() {
        let grant = match last {
            Some((pick, grant)) if pick == i => grant,
            _ => Grant::Choke,
        };
        tally(apply_decision(c, grant, round));
    }
    (newly_unchoked, newly_choked, with_interested)
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
    use crate::torrent::meta::TorrentMeta;
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
            renames: Default::default(),
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
        let addr: std::net::SocketAddr = format!("203.0.113.{n}:6881").parse().unwrap();
        let s = Arc::new(PeerStats::new(addr, [n; 20], "test".into(), false, false));
        s.num_pieces_have.store(have, Ordering::Relaxed);
        s.interested.store(interested, Ordering::Relaxed);
        s.uploaded_last_tick.store(bytes, Ordering::Relaxed);
        t.peers.insert(addr, s.clone());
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
        assert_eq!(tick_torrent(&t, 4, 1), (0, 0, false));
    }

    #[test]
    fn no_interested_peer_is_nothing_to_decide() {
        let t = seeding(100);
        peer(&t, 1, 50, false, 0);
        assert_eq!(
            tick_torrent(&t, 4, 1),
            (0, 0, false),
            "nobody asked for anything"
        );
    }

    fn open(peers: &[Arc<PeerStats>]) -> Vec<usize> {
        peers
            .iter()
            .enumerate()
            .filter(|(_, p)| !p.choked.load(Ordering::Relaxed))
            .map(|(i, _)| i)
            .collect()
    }

    /// ⭐ The bug this rotation replaced: ranked by how little a peer holds,
    /// one at 99% stayed choked behind every peer with nothing, for as long as
    /// they stayed. In turns, it gets its slot like the others.
    #[test]
    fn a_nearly_complete_peer_gets_its_turn() {
        let t = seeding(100);
        let nearly_done = peer(&t, 1, 99, true, 1_000_000);
        for n in 2..=4 {
            peer(&t, n, 0, true, 0);
        }
        let mut served = false;
        for round in 1..=4 * TURN_ROUNDS {
            tick_torrent(&t, 1, round);
            served |= !nearly_done.choked.load(Ordering::Relaxed);
        }
        assert!(served, "four peers, one slot, four turns: everyone's comes");
    }

    /// ⭐ A turn lasts `TURN_ROUNDS`, then passes to whoever waited longest,
    /// and comes back round. With one slot there is no optimistic slot, so the
    /// order is the queue's alone: ties at the start go to the lower address,
    /// not to the order the peers were added in.
    #[test]
    fn turns_rotate_in_queue_order() {
        let t = seeding(100);
        // Added highest address first: map order must not decide anything.
        let c = peer(&t, 3, 0, true, 0);
        let b = peer(&t, 2, 50, true, 0);
        let a = peer(&t, 1, 99, true, 0);
        let peers = [a, b, c];
        let mut holders = Vec::new();
        for round in 1..=4 * TURN_ROUNDS {
            tick_torrent(&t, 1, round);
            let o = open(&peers);
            assert_eq!(o.len(), 1, "round {round}: one slot, {o:?}");
            holders.push(o[0]);
        }
        let turn = TURN_ROUNDS as usize;
        let expect: Vec<usize> = [0, 1, 2, 0]
            .iter()
            .flat_map(|&p| std::iter::repeat_n(p, turn))
            .collect();
        assert_eq!(holders, expect);
    }

    /// A peer that connects mid-rotation queues behind those already waiting,
    /// not in front of them.
    #[test]
    fn a_newcomer_waits_behind_the_queue() {
        let t = seeding(100);
        let a = peer(&t, 1, 0, true, 0);
        let b = peer(&t, 2, 0, true, 0);
        tick_torrent(&t, 1, 1);
        // Lowest address, but it arrives at round 2.
        let early = peer(&t, 0, 0, true, 0);
        for round in 2..=TURN_ROUNDS {
            tick_torrent(&t, 1, round);
        }
        assert!(!a.choked.load(Ordering::Relaxed), "a's turn still runs");
        tick_torrent(&t, 1, TURN_ROUNDS + 1);
        assert!(!b.choked.load(Ordering::Relaxed), "b waited since round 1");
        assert!(early.choked.load(Ordering::Relaxed), "it came at round 2");
    }

    /// ⭐ Two slots or more: one of them is optimistic. It goes to a peer the
    /// queue has not reached, and stays with it for `OPTIMISTIC_ROUNDS`.
    #[test]
    fn one_slot_is_optimistic_and_held_for_its_rounds() {
        let t = seeding(100);
        let peers: Vec<_> = (1..=6).map(|i| peer(&t, i, 0, true, 0)).collect();
        tick_torrent(&t, 2, 1);
        let first = open(&peers);
        assert_eq!(first.len(), 2, "{first:?}");
        assert_eq!(first[0], 0, "the regular slot is the head of the queue");
        let lucky = first[1];
        assert_eq!(
            Turn::of(&peers[lucky]),
            Turn::Optimistic,
            "and the other one is the optimistic slot"
        );
        for round in 2..=OPTIMISTIC_ROUNDS {
            tick_torrent(&t, 2, round);
            assert!(
                open(&peers).contains(&lucky),
                "round {round}: the optimistic peer keeps its slot"
            );
        }
    }

    /// The optimistic slot is taken out of the slots, not added to them:
    /// `unchoke_slots` is still how many are let through.
    #[test]
    fn only_the_slots_are_let_through() {
        let t = seeding(100);
        let peers: Vec<_> = (1..=5)
            .map(|i| peer(&t, i, i as u32 * 10, true, 0))
            .collect();

        let (unchoked, _, _) = tick_torrent(&t, 2, 1);
        assert_eq!(unchoked, 2);
        assert_eq!(open(&peers).len(), 2, "two slots, two peers");
    }

    /// Slots enough for everyone: everyone is let through, every round, and
    /// nobody is churned for the sake of an optimistic pick.
    #[test]
    fn with_room_for_all_nobody_is_choked() {
        let t = seeding(100);
        let peers: Vec<_> = (1..=4).map(|i| peer(&t, i, 0, true, 0)).collect();
        for round in 1..=3 * TURN_ROUNDS {
            let (_, choked, _) = tick_torrent(&t, 4, round);
            assert_eq!(choked, 0, "round {round}");
            assert_eq!(open(&peers).len(), 4);
        }
    }

    /// The window is closed every tick. Leaving it would make a peer that was
    /// fast once look fast forever.
    #[test]
    fn the_tick_window_is_reset_for_the_next_one() {
        let t = seeding(100);
        let p = peer(&t, 1, 10, true, 500_000);
        tick_torrent(&t, 4, 1);
        assert_eq!(p.uploaded_last_tick.load(Ordering::Relaxed), 0);
    }

    /// A decision that does not change anything must not bump the generation:
    /// the peer task emits a Choke or Unchoke on the wire every time it moves.
    /// That holds past the end of a turn renewed for want of anyone else.
    #[test]
    fn an_unchanged_decision_sends_nothing() {
        let t = seeding(100);
        let p = peer(&t, 1, 0, true, 0);
        let (first, _, _) = tick_torrent(&t, 4, 1);
        assert_eq!(first, 1, "unchoked once");
        let gen_after_first = p.choking_gen.load(Ordering::Relaxed);

        for round in 2..=2 * TURN_ROUNDS {
            let (again, choked_again, _) = tick_torrent(&t, 4, round);
            assert_eq!((again, choked_again), (0, 0), "round {round}");
        }
        assert_eq!(
            p.choking_gen.load(Ordering::Relaxed),
            gen_after_first,
            "the generation only moves when the wire has to"
        );
    }

    /// A torrent with no pieces at all still has its peers served.
    #[test]
    fn a_torrent_of_no_pieces_is_served() {
        let t = seeding(0);
        let p = peer(&t, 1, 0, true, 0);
        let (unchoked, _, had) = tick_torrent(&t, 4, 1);
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
        tick_torrent(&t, 4, 1);
        tokio::time::timeout(
            std::time::Duration::from_millis(100),
            p.punch_wake.notified(),
        )
        .await
        .expect("the unchoke must wake the session");
    }

    /// ⭐ Turning the choker off releases every peer it had choked. Left as
    /// they were, "off" would mean "frozen in the last ranking".
    #[test]
    fn switching_the_choker_off_unchokes_everyone() {
        let t = seeding(100);
        let peers: Vec<_> = (1..=5)
            .map(|i| peer(&t, i, i as u32 * 10, true, 0))
            .collect();
        tick_torrent(&t, 1, 1);
        assert_eq!(
            peers
                .iter()
                .filter(|p| p.choked.load(Ordering::Relaxed))
                .count(),
            4
        );
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
        let peers: Vec<_> = (1..=5)
            .map(|i| peer(&t, i, i as u32 * 10, true, 0))
            .collect();
        // What a session does with the choker off: it unchokes whoever is
        // interested, itself.
        peers
            .iter()
            .for_each(|p| p.choked.store(false, Ordering::Relaxed));
        let choked = || {
            peers
                .iter()
                .filter(|p| p.choked.load(Ordering::Relaxed))
                .count()
        };
        let policy = crate::peer::extension::PeerPolicy::default();
        let mut listed = 0;

        let active = choke_pass(
            &policy,
            || {
                listed += 1;
                vec![t.clone()]
            },
            false,
            1,
        );
        assert!(!active);
        assert_eq!(
            (choked(), listed),
            (0, 0),
            "off: no choke, no walk of the library"
        );

        policy.set_choking(true);
        policy.set_unchoke_slots(2);
        let active = choke_pass(&policy, || vec![t.clone()], active, 2);
        assert!(active);
        assert_eq!(choked(), 3, "on, two slots");

        policy.set_choking(false);
        let active = choke_pass(&policy, || vec![t.clone()], active, 3);
        assert!(!active);
        assert_eq!(choked(), 0, "off again: released");

        policy.set_choking(true);
        policy.set_unchoke_slots(-1);
        choke_pass(&policy, || vec![t.clone()], false, 4);
        assert_eq!(choked(), 0, "unlimited slots choke nobody");
    }

    /// Three seeding torrents of four interested peers each.
    fn three_torrents() -> (Vec<Arc<TorrentState>>, Vec<Vec<Arc<PeerStats>>>) {
        let haves: [[u32; 4]; 3] = [[0, 10, 20, 30], [5, 15, 25, 35], [1, 11, 21, 31]];
        let mut ts = Vec::new();
        let mut ps = Vec::new();
        for (ti, row) in haves.iter().enumerate() {
            let t = Arc::new(seeding(100));
            let peers: Vec<_> = row
                .iter()
                .enumerate()
                .map(|(i, h)| peer(&t, (ti * 4 + i) as u8 + 1, *h, true, 0))
                .collect();
            // What a session does with the choker off: unchoke the interested.
            peers
                .iter()
                .for_each(|p| p.choked.store(false, Ordering::Relaxed));
            ts.push(t);
            ps.push(peers);
        }
        (ts, ps)
    }

    fn open_counts(ps: &[Vec<Arc<PeerStats>>]) -> Vec<usize> {
        ps.iter().map(|row| open(row).len()).collect()
    }

    /// ⭐ The global cap: three torrents, three slots each, five in all.
    /// Every round lets exactly the cap through with no torrent past its own
    /// three, and the rotation runs across torrents: within a few turns every
    /// one of the twelve peers has been served.
    #[test]
    fn a_global_cap_rotates_across_all_torrents() {
        let (ts, ps) = three_torrents();
        let policy = crate::peer::extension::PeerPolicy::default();
        policy.set_choking(true);
        policy.set_unchoke_slots(3);
        policy.set_max_uploads(5);
        let mut served = vec![vec![false; 4]; 3];
        let mut active = false;
        for round in 1..=4 * TURN_ROUNDS {
            active = choke_pass(&policy, || ts.clone(), active, round);
            assert!(active);
            let counts = open_counts(&ps);
            assert_eq!(counts.iter().sum::<usize>(), 5, "round {round}: {counts:?}");
            assert!(counts.iter().all(|&n| n <= 3), "round {round}: {counts:?}");
            for (ti, row) in ps.iter().enumerate() {
                for i in open(row) {
                    served[ti][i] = true;
                }
            }
        }
        assert!(
            served.iter().flatten().all(|&s| s),
            "everyone had a turn: {served:?}"
        );
    }

    /// Without the global cap the same engine lets three through per torrent.
    #[test]
    fn without_a_global_cap_each_torrent_keeps_its_own_slots() {
        let (ts, ps) = three_torrents();
        let policy = crate::peer::extension::PeerPolicy::default();
        policy.set_choking(true);
        policy.set_unchoke_slots(3);
        policy.set_max_uploads(-1);
        choke_pass(&policy, || ts.clone(), false, 1);
        assert_eq!(open_counts(&ps), vec![3, 3, 3]);
    }

    /// A global cap alone (unlimited per torrent) still caps the engine, and
    /// a cap above what the slots allow changes nothing.
    #[test]
    fn the_global_cap_works_without_a_per_torrent_limit_and_never_adds() {
        let (ts, ps) = three_torrents();
        let policy = crate::peer::extension::PeerPolicy::default();
        policy.set_choking(true);
        policy.set_unchoke_slots(-1);
        policy.set_max_uploads(2);
        assert!(choke_pass(&policy, || ts.clone(), false, 1));
        assert_eq!(open_counts(&ps).iter().sum::<usize>(), 2);

        policy.set_unchoke_slots(1);
        policy.set_max_uploads(100);
        choke_pass(&policy, || ts.clone(), true, 2);
        assert_eq!(open_counts(&ps), vec![1, 1, 1]);
    }

    /// The cap and the choker switch: 0 reads as 4 like the per-torrent
    /// slots, and turning the choker off releases every peer the cap choked.
    #[test]
    fn the_global_cap_reads_zero_as_four_and_off_releases_everyone() {
        let p = crate::peer::extension::PeerPolicy::default();
        assert_eq!(p.max_uploads(), None, "no cap unless asked");
        p.set_max_uploads(0);
        assert_eq!(p.max_uploads(), Some(4));
        p.set_max_uploads(7);
        assert_eq!(p.max_uploads(), Some(7));
        p.set_max_uploads(-1);
        assert_eq!(p.max_uploads(), None);

        let (ts, ps) = three_torrents();
        p.set_choking(true);
        p.set_max_uploads(2);
        let active = choke_pass(&p, || ts.clone(), false, 1);
        assert_eq!(open_counts(&ps).iter().sum::<usize>(), 2);
        p.set_choking(false);
        assert!(!choke_pass(&p, || ts.clone(), active, 2));
        assert_eq!(open_counts(&ps).iter().sum::<usize>(), 12);
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
        assert_eq!(
            count_bitfield_pieces(&[0xFF], 100),
            8,
            "one byte, eight pieces"
        );
    }
}
