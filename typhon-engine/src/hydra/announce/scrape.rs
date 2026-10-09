//! Scrape: seeders and leechers for the torrents no tracker has answered for.
//!
//! A torrent's swarm counts come from its announces. One that has not been
//! announced yet -- queued behind a library's boot sweep, or stopped -- shows
//! zeros, and zero reads as "dead swarm". A scrape fills those counts, and
//! only those: it writes `scrape_seeders` / `scrape_leechers` on the torrent,
//! never the announce cache, so nothing that keys on "this torrent was
//! announced" (the scheduler, the race timeline, the trackers tab) can take a
//! scrape for an announce. It starts nothing and stops nothing.
//!
//! It leaves exactly as the announces do: the engine's interface, its proxy,
//! the tracker's passkey override, and not at all while the engine holds its
//! trackers (`start_paused`) or for a host whose breaker is open. A `udp://`
//! tracker is not scraped behind a proxy, as it is not announced there.

use std::collections::{BTreeMap, HashMap};
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::{Duration, Instant};

use futures::StreamExt;
use typhon_engine::torrent::meta::InfoHash;
use typhon_engine::torrent::TorrentManager;
use typhon_engine::tracker::scrape::{self as tscrape, ScrapeEntry, BATCH};

use super::breaker::Breaker;
use super::cache::Cache;
use super::policy::{apply_passkey, passkey_for, Policy};

/// First pass after the engine starts, then between passes.
const FIRST: Duration = Duration::from_secs(90);
const PASS: Duration = Duration::from_secs(300);
/// Torrents scraped per pass at most: a library of a million unannounced
/// torrents is scraped over hours, not in one burst at the trackers.
const PER_PASS: usize = 5000;
/// A torrent scraped and still unannounced is scraped again after this.
const AGAIN: Duration = Duration::from_secs(3600);
/// Between two requests to one tracker.
const HOST_GAP: Duration = Duration::from_secs(2);
/// Trackers scraped at once.
const HOSTS_AT_ONCE: usize = 8;

/// Where one batch goes.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub enum Target {
    /// An HTTP scrape URL, passkey applied.
    Http(String),
    /// A `udp://` tracker URL.
    Udp(String),
}

impl Target {
    fn host(&self) -> String {
        match self {
            Target::Http(u) | Target::Udp(u) => super::overrides::override_host(u),
        }
    }
}

/// The scrape target for one tracker URL, or None when it has none.
pub fn target_for(policy: &Policy, tracker_url: &str) -> Option<Target> {
    if typhon_engine::tracker::udp::is_udp(tracker_url) {
        return (!policy.skip_udp).then(|| Target::Udp(tracker_url.to_string()));
    }
    let url = match passkey_for(policy, tracker_url) {
        Some(k) => apply_passkey(tracker_url, k),
        None => tracker_url.to_string(),
    };
    tscrape::scrape_url(&url).map(Target::Http)
}

/// Which torrents to scrape this pass, grouped by target: those with no
/// announce answer and not scraped within `AGAIN`, first scrapable tracker in
/// tier order, at most `budget` of them.
pub fn plan(
    torrents: impl Iterator<Item = (InfoHash, Vec<Vec<String>>)>,
    announced: impl Fn(&InfoHash) -> bool,
    done: &HashMap<InfoHash, Instant>,
    policy: &Policy,
    allows: impl Fn(&str) -> bool,
    budget: usize,
    now: Instant,
) -> BTreeMap<Target, Vec<InfoHash>> {
    let mut out: BTreeMap<Target, Vec<InfoHash>> = BTreeMap::new();
    let mut left = budget;
    for (ih, tiers) in torrents {
        if left == 0 {
            break;
        }
        if announced(&ih) || done.get(&ih).is_some_and(|at| now.saturating_duration_since(*at) < AGAIN) {
            continue;
        }
        let target = tiers.iter().flatten().filter_map(|u| target_for(policy, u)).find(|t| allows(&t.host()));
        if let Some(t) = target {
            out.entry(t).or_default().push(ih);
            left -= 1;
        }
    }
    out
}

/// One engine's scrape loop, beside its announcer.
pub fn spawn(manager: Arc<TorrentManager>, policy: super::PolicyHandle, cache: Arc<Cache>, breaker: Arc<Breaker>) {
    tokio::spawn(async move {
        let mut done: HashMap<InfoHash, Instant> = HashMap::new();
        tokio::time::sleep(FIRST).await;
        loop {
            // Held: not a word to any tracker, a scrape included.
            if !manager.limiter().dials_paused() {
                let policy = policy.read().map(|p| p.clone()).unwrap_or_else(|e| e.into_inner().clone());
                pass(&manager, &policy, &cache, &breaker, &mut done).await;
            }
            tokio::time::sleep(PASS).await;
        }
    });
}

async fn pass(manager: &Arc<TorrentManager>, policy: &Policy, cache: &Cache, breaker: &Breaker, done: &mut HashMap<InfoHash, Instant>) {
    let now = Instant::now();
    let announced = |ih: &InfoHash| cache.has(&typhon_engine::torrent::hex_encode(ih));
    let groups = plan(
        manager.all().into_iter().map(|t| (t.info_hash, t.live_trackers.read().clone())),
        &announced,
        done,
        policy,
        |host| breaker.allows(host, now),
        PER_PASS,
        now,
    );
    if groups.is_empty() {
        done.retain(|ih, _| manager.get(ih).is_some());
        return;
    }
    for ih in groups.values().flatten() {
        done.insert(*ih, now);
    }
    let (device, proxy, ua) = (policy.device.clone(), policy.proxy.clone(), policy.user_agent.clone());
    let filled: usize = futures::stream::iter(groups)
        .map(|(target, hashes)| {
            let (device, proxy, ua) = (device.clone(), proxy.clone(), ua.clone());
            async move { scrape_host(&target, &hashes, &ua, &device, &proxy).await }
        })
        .buffer_unordered(HOSTS_AT_ONCE)
        .map(|entries| {
            let mut n = 0;
            for e in entries {
                let hex = typhon_engine::torrent::hex_encode(&e.info_hash);
                // An announce answered meanwhile is fresher: leave it.
                if cache.has(&hex) {
                    continue;
                }
                if let Some(t) = manager.get(&e.info_hash) {
                    t.scrape_seeders.store(e.complete, Ordering::Relaxed);
                    t.scrape_leechers.store(e.incomplete, Ordering::Relaxed);
                    n += 1;
                }
            }
            n
        })
        .fold(0, |a, n| async move { a + n })
        .await;
    tracing::info!(filled, "scrape: swarm counts for torrents not announced yet");
}

/// Every batch for one tracker, one after the other. Stops at the first
/// error: the next pass tries again, the tracker does not get hammered.
async fn scrape_host(target: &Target, hashes: &[InfoHash], ua: &str, device: &str, proxy: &str) -> Vec<ScrapeEntry> {
    let mut out = Vec::new();
    for (i, batch) in hashes.chunks(BATCH).enumerate() {
        if i > 0 {
            tokio::time::sleep(HOST_GAP).await;
        }
        let got = match target {
            Target::Http(url) => tscrape::http_scrape_on(url, batch, ua, device, proxy).await,
            Target::Udp(url) => typhon_engine::tracker::udp::send_scrape_on(url, batch, device, proxy).await,
        };
        match got {
            Ok(v) => out.extend(v),
            Err(e) => {
                tracing::debug!(tracker = %target.host(), error = %e, "scrape refused");
                break;
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ih(b: u8) -> InfoHash {
        [b; 20]
    }

    fn tiers(urls: &[&str]) -> Vec<Vec<String>> {
        vec![urls.iter().map(|s| s.to_string()).collect()]
    }

    /// ⭐ Announced torrents are never scraped: their counts are the
    /// announce's, and a scrape must not stand in for one.
    #[test]
    fn only_torrents_without_an_announce_answer_are_planned() {
        let p = Policy::default();
        let now = Instant::now();
        let ts = vec![(ih(1), tiers(&["https://t.example/announce"])), (ih(2), tiers(&["https://t.example/announce"]))];
        let g = plan(ts.into_iter(), |h| *h == ih(1), &HashMap::new(), &p, |_| true, 100, now);
        assert_eq!(g.get(&Target::Http("https://t.example/scrape".into())), Some(&vec![ih(2)]));
        assert_eq!(g.values().flatten().count(), 1);
    }

    /// The passkey override reaches the scrape URL; a tracker with no scrape
    /// falls to the next; skip_udp and an open breaker are honoured.
    #[test]
    fn the_target_follows_the_announce_policy() {
        let mut p = Policy::default();
        p.passkeys.insert("t.example".into(), "SECRET".into());
        // The passkey goes where the announce puts it: `/announce/<key>`,
        // which BEP 48 gives no scrape URL -- such a tracker is not scraped
        // rather than scraped with the key the announces replace.
        assert_eq!(target_for(&p, "https://t.example/announce/OLDKEY"), None);
        assert_eq!(target_for(&p, "https://t.example/announce?passkey=K"), Some(Target::Http("https://t.example/scrape?passkey=K".into())));
        let now = Instant::now();
        let ts = vec![(ih(3), tiers(&["https://noscrape.example/a", "udp://u.example:1337/announce"]))];
        let g = plan(ts.clone().into_iter(), |_| false, &HashMap::new(), &p, |_| true, 100, now);
        assert!(g.contains_key(&Target::Udp("udp://u.example:1337/announce".into())));
        p.skip_udp = true;
        assert!(plan(ts.clone().into_iter(), |_| false, &HashMap::new(), &p, |_| true, 100, now).is_empty());
        p.skip_udp = false;
        assert!(plan(ts.into_iter(), |_| false, &HashMap::new(), &p, |h| h != "u.example", 100, now).is_empty(), "breaker open");
    }

    /// The budget caps a pass, and a torrent scraped recently waits AGAIN.
    #[test]
    fn a_pass_is_bounded_and_does_not_repeat_itself() {
        let p = Policy::default();
        let now = Instant::now();
        let ts: Vec<_> = (0..10u8).map(|i| (ih(i), tiers(&["http://t.example/announce"]))).collect();
        assert_eq!(plan(ts.clone().into_iter(), |_| false, &HashMap::new(), &p, |_| true, 4, now).values().flatten().count(), 4);
        let done: HashMap<_, _> = (0..10u8).map(|i| (ih(i), now)).collect();
        assert!(plan(ts.clone().into_iter(), |_| false, &done, &p, |_| true, 100, now).is_empty());
        assert_eq!(plan(ts.into_iter(), |_| false, &done, &p, |_| true, 100, now + AGAIN).values().flatten().count(), 10);
    }
}
