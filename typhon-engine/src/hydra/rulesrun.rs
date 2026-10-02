//! Running workflows: gather the facts, match, then act.
//!
//! Deliberately separate from `rules`, which is pure. Everything that needs an
//! engine, a store or a clock lives here, so the matching logic stays testable
//! without any of them.
//!
//! ## The two-phase pass, which is not an optimisation
//!
//! Matching walks the engine's torrent map; applying stops torrents, edits the
//! store and can start a data move. Doing the second inside the first would
//! hold the engine's map for the whole duration of the actions -- on a rule
//! matching a few hundred torrents that is long enough to be felt everywhere
//! else. So: collect the matches, drop everything, then act.

use std::sync::atomic::Ordering;
use std::sync::Arc;

use crate::engines::EngineHost;
use crate::linkindex::{self, LinkFacts};
use crate::rules::{self, Action, Facts, Workflow};
use crate::store::{ActivityEntry, Store};

/// A torrent one workflow decided about, carried between the two phases.
#[derive(Debug, Clone, serde::Serialize)]
pub struct Match {
    pub info_hash: String,
    pub name: String,
    pub engine: String,
    pub total_size: f64,
    /// Actions still worth doing: the ones it is already satisfying are
    /// dropped here, so `applied` counts changes rather than passes.
    pub actions: Vec<Action>,
    /// What a webhook action posts, built from the facts the rule matched
    /// on. None when the workflow has no webhook: most do not, and a pass
    /// over a million torrents need not build a document per match.
    #[serde(skip)]
    pub payload: Option<serde_json::Value>,
}

#[derive(Debug, Default, serde::Serialize)]
pub struct PassReport {
    pub workflow_id: String,
    pub workflow_name: String,
    pub matched: usize,
    pub applied: usize,
    pub skipped: usize,
    pub failed: usize,
    /// Bytes the delete action would free, for the free-space accounting.
    pub freed_bytes: f64,
    pub capped: bool,
}

/// Build the fact table for one engine.
///
/// One store query for the whole session -- an index-only scan -- joined to the
/// engine's live map in memory. The alternative, a lookup per torrent, is
/// 300 000 queries.
///
/// ⚠️ `want_free_space` is not an optimisation to take lightly. It is one
/// `statvfs` per distinct save path, and a library where every torrent has its
/// own folder has as many save paths as torrents: 915 000 of them in
/// production, which kept this function -- then under the store's writer lock
/// -- busy for over twenty seconds a pass. A rule that does not read the field
/// does not pay for it.
pub fn gather(
    host: &EngineHost,
    store: &Store,
    engine_id: &str,
    links: &std::collections::HashMap<String, LinkFacts>,
    want_free_space: bool,
) -> Vec<Facts> {
    let Some(engine) = host.engines().iter().find(|e| e.id == engine_id) else {
        return Vec::new();
    };
    let stored = store.workflow_facts(engine_id).unwrap_or_default();
    let now = crate::store::now_secs() as f64;
    // statvfs once per distinct save_path, not once per torrent: a catalogue
    // shares a handful of them, and the syscall is the same answer every time.
    let mut free_by_path: std::collections::HashMap<String, f64> =
        std::collections::HashMap::new();

    engine
        .manager
        .all()
        .into_iter()
        .map(|t| {
            let hash: String = t.info_hash.iter().map(|b| format!("{b:02x}")).collect();
            let s = stored.get(&hash).cloned().unwrap_or_default();
            let free_space = if s.save_path.is_empty() || !want_free_space {
                rules::NEVER
            } else {
                *free_by_path
                    .entry(s.save_path.clone())
                    .or_insert_with(|| free_space_at(&s.save_path))
            };
            let l = links.get(&hash);
            facts_of(&t, hash, s, engine_id, l, free_space, now)
        })
        .collect()
}

fn free_space_at(save_path: &str) -> f64 {
    crate::platform::free_space(std::path::Path::new(save_path))
        .map(|b| b as f64)
        .unwrap_or(rules::NEVER)
}

/// The facts of ONE torrent, for an event.
///
/// ⚠️ Not `gather` filtered down: that reads the store row of every torrent
/// in the session, a million of them, to answer about one that just finished.
/// This is one engine lookup and one indexed row. `None` when either side has
/// never heard of the torrent.
pub fn gather_one(host: &EngineHost, store: &Store, engine_id: &str, info_hash: &str) -> Option<Facts> {
    let engine = host.engines().iter().find(|e| e.id == engine_id)?;
    let t = engine.manager.get(&crate::store::hex20(info_hash)?)?;
    let s = store.workflow_facts_of(engine_id, info_hash).ok().flatten()?;
    let free_space = if s.save_path.is_empty() { rules::NEVER } else { free_space_at(&s.save_path) };
    let now = crate::store::now_secs() as f64;
    // No link facts: an event workflow may not ask for them (see
    // `CompileError::LinkFieldOnEvent`), so NEVER is the honest answer.
    Some(facts_of(&t, info_hash.to_string(), s, engine_id, None, free_space, now))
}

/// One torrent's facts, from the engine's live state and the store's row.
/// The single place a `Facts` is built, so a pass and an event read the same.
fn facts_of(
    t: &std::sync::Arc<typhon_engine::torrent::meta::TorrentState>,
    hash: String,
    s: crate::store::WorkflowFacts,
    engine_id: &str,
    l: Option<&LinkFacts>,
    free_space: f64,
    now: f64,
) -> Facts {
    let tracker_err = t
        .last_announce_error
        .lock()
        .map(|g| g.clone())
        .unwrap_or_default();
    let downloaded = t.total_downloaded.load(Ordering::Relaxed) as f64;
    let uploaded = t.total_uploaded.load(Ordering::Relaxed) as f64;
    let size = t.meta.total_size as f64;
    let completed = t.completed_time.load(Ordering::Relaxed);

    Facts {
        info_hash: hash,
        name: t.meta.name.clone(),
        category: s.category,
        tags: s.tags,
        engine: engine_id.to_string(),
        save_path: s.save_path,
        // The engine's flag is the effective state; the store's is the
        // operator's intent. A condition on `user_paused` means the
        // intent, which is what a person clicked.
        user_paused: s.paused,
        multi_file: t.meta.files.len() > 1,

        progress: if size > 0.0 {
            (downloaded / size * 100.0).min(100.0)
        } else {
            0.0
        },
        // The row's definition (`row::share_ratio`). Uploaded / downloaded
        // here made a cross-seed 0 for every rule while its row read 3.2
        // (02/10/2026): "ratio > 2" never matched what the operator saw.
        ratio: crate::row::torrent_ratio(t),
        total_size: size,
        total_uploaded: uploaded,
        total_downloaded: downloaded,
        // The engine accumulates this; the store column never gets
        // written, which is why the field used to be withheld.
        seeding_time: t.seed_time_now(now as i64) as f64,
        added_age: if s.added_time > 0.0 {
            now - s.added_time
        } else {
            rules::NEVER
        },
        // ⚠️ NEVER, not zero. A torrent that has not completed has no
        // completion age, and any finite stand-in would satisfy
        // "completed less than a day ago". See rules::NEVER.
        completed_age: if completed > 0 {
            now - completed as f64
        } else {
            rules::NEVER
        },
        // ⚠️ Everything below used to fall through `..Default::default()`
        // and read 0 or "" for every torrent in the catalogue, while
        // being offered in the field picker. `num_peers == 0` matched
        // EVERYTHING; a condition on a tracker matched nothing.
        // ⚠️ The ENGINE's state is not the state anyone sees. It says
        // "paused" for any halt and cannot tell a scheduler hold from a
        // user pressing stop, so `derive_state` folds in the intent --
        // and the list does the same. Reporting the raw one here would
        // make `state == stopped` match torrents the UI shows as
        // queued, which is a view contradicting another.
        state: crate::row::derive_state_static(
            typhon_engine::rpc::dispatch::state_str(
                t.status.load(Ordering::Relaxed),
                t.is_paused.load(Ordering::Relaxed),
            ),
            s.paused,
        )
        .to_string(),
        tracker_host: t
            .live_trackers
            .read()
            .iter()
            .flatten()
            .next()
            .map(|u| typhon_engine::rpc::dispatch::tracker_host_of(u))
            .unwrap_or_default(),
        tracker_error: !tracker_err.is_empty(),
        tracker_error_msg: tracker_err,
        torrent_error: t.status.load(Ordering::Relaxed)
            == typhon_engine::torrent::meta::TorrentStatus::Error as u8,
        upload_rate: t.upload_rate.get() as f64,
        download_rate: t.download_rate.get() as f64,
        num_peers: t.peers_connected.load(Ordering::Relaxed) as f64,
        num_seeds: t.scrape_seeders.load(Ordering::Relaxed) as f64,
        swarm_seeds: t.scrape_seeders.load(Ordering::Relaxed) as f64,
        swarm_leechers: t.scrape_leechers.load(Ordering::Relaxed) as f64,
        free_space,
        // ⚠️ NEVER, not zero, when no scan ran. Zero is a measurement, and
        // `external_links == 0` means "safe to delete" -- defaulting to it
        // would arm every deletion rule against the whole catalogue before
        // a single file had been looked at.
        link_count: l.map(|x| x.link_count as f64).unwrap_or(rules::NEVER),
        external_links: l.map(|x| x.external_links as f64).unwrap_or(rules::NEVER),
        freeable_bytes: l.map(|x| x.freeable_bytes as f64).unwrap_or(rules::NEVER),
        data_missing: l.is_some_and(|x| x.data_missing),
        // ⚠️⚠️ NO `..Default::default()` here, deliberately. It is what
        // let thirteen fields read 0 or "" for every torrent while the
        // picker offered them: `num_peers == 0` matched EVERYTHING, a
        // tracker condition matched nothing, and nothing complained.
        // Listing every field makes the compiler refuse a new one that
        // nobody taught this function to measure.
    }
}

/// The save path the ENGINE reads the files from.
///
/// ⚠️ Not the store's `save_path`, which is not always the same thing: for the
/// Internet Archive items the store records the item's own folder
/// (`…/internet-archive/<item>`) while the engine holds its parent and adds the
/// torrent's name, like every multi-file torrent. Resolving from the store's
/// value put the name in twice, and 17 000 torrents that were seeding normally
/// were reported as having lost their files. The engine's path is the one that
/// actually serves the bytes, so it is the one to measure.
pub fn engine_save_path(t: &typhon_engine::torrent::meta::TorrentState) -> String {
    t.save_path.read().to_string_lossy().into_owned()
}

/// Where one torrent's files live on disk.
///
/// The layout rule `dedup::Layout::on_disk` encodes: a multi-file torrent puts
/// its files under a folder named after the torrent, a single-file one is the
/// name itself at the root of save_path.
///
/// ⚠️ Walking save_path instead would collect the NEIGHBOURS of a torrent that
/// shares a folder and credit it with their links. And the guard below has to
/// call the SAME function as the scan: two resolutions that disagree would
/// protect one set of files while measuring another.
pub fn torrent_files(
    t: &std::sync::Arc<typhon_engine::torrent::meta::TorrentState>,
    save_path: &str,
) -> Vec<std::path::PathBuf> {
    let base = std::path::Path::new(save_path);
    if t.meta.multi_file {
        let root = base.join(&t.meta.name);
        t.meta.files.iter().map(|f| root.join(&f.path)).collect()
    } else {
        vec![base.join(&t.meta.name)]
    }
}

/// One torrent copy as the link index sees it: where it lives and the files
/// it resolves to there. No syscall has happened yet.
#[derive(Debug, Clone)]
pub struct CatalogueEntry {
    pub info_hash: String,
    pub session: String,
    pub save_path: String,
    pub paths: Vec<std::path::PathBuf>,
}

/// Every torrent copy this node holds, with its files resolved.
///
/// ⚠️ Pure bookkeeping, no `stat`: this half needs the store (for the save
/// paths), the other half is disk wait whose cost cannot be bounded. Build
/// this under the lock, drop it, then touch the filesystem.
/// The store's half of the catalogue, per engine: the only part that needs a
/// store connection. Take it, let the connection go, then `catalogue_from`.
pub fn stored_facts(
    host: &EngineHost,
    store: &Store,
) -> Vec<(String, std::collections::HashMap<String, crate::store::WorkflowFacts>)> {
    host.engines()
        .iter()
        .map(|e| (e.id.clone(), store.workflow_facts(&e.id).unwrap_or_default()))
        .collect()
}

/// Resolve every copy's files from the engines' metadata. No store, no
/// syscall: at a million torrents this is seconds of path building, which no
/// connection has to wait for.
pub fn catalogue_from(
    host: &EngineHost,
    facts: &[(String, std::collections::HashMap<String, crate::store::WorkflowFacts>)],
) -> Vec<CatalogueEntry> {
    let mut out = Vec::new();
    for engine in host.engines().iter() {
        let Some((_, stored)) = facts.iter().find(|(id, _)| *id == engine.id) else {
            continue;
        };
        for t in engine.manager.all() {
            let hash: String = t.info_hash.iter().map(|b| format!("{b:02x}")).collect();
            // A copy the store does not know is not part of any pass.
            if !stored.contains_key(&hash) {
                continue;
            }
            let save_path = engine_save_path(&t);
            if save_path.is_empty() {
                continue;
            }
            let paths = torrent_files(&t, &save_path);
            out.push(CatalogueEntry { info_hash: hash, session: engine.id.clone(), save_path, paths });
        }
    }
    out
}

/// The catalogue as the store last measured it, ready for `linkindex::compute`.
///
/// ⭐ A copy with no row, a row taken under another save path, or a row whose
/// file count no longer matches, is left OUT -- not answered with zeros. Its
/// facts then read `NEVER`, so no rule can match a torrent nobody has looked
/// at yet. Leaving it out also drops its names from `owned`, which can only
/// raise another torrent's `external_links`: the direction that keeps files.
///
/// Returns, beside the entries, the index in `cat` each one came from: a pass
/// that re-measures a candidate writes it back to the store under that copy.
pub fn entries_from_store(
    cat: &[CatalogueEntry],
    rows: &LinkRows,
) -> (Vec<linkindex::Entry>, Vec<usize>) {
    let mut out = Vec::with_capacity(rows.len().min(cat.len()));
    let mut origin = Vec::with_capacity(out.capacity());
    for (i, c) in cat.iter().enumerate() {
        let Some((save_path, _, blob)) = rows.get(&(c.info_hash.clone(), c.session.clone())) else {
            continue;
        };
        if *save_path != c.save_path {
            continue;
        }
        let Some(stats) = linkindex::unpack(blob) else { continue };
        if stats.len() != c.paths.len() {
            continue;
        }
        out.push((c.info_hash.clone(), c.paths.iter().cloned().zip(stats).collect()));
        origin.push(i);
    }
    (out, origin)
}

/// The store's link rows, as `Store::link_index_stats` reads them:
/// (info_hash, session) -> (save_path, measured_at, packed stats).
pub type LinkRows = std::collections::HashMap<(String, String), (String, i64, Vec<u8>)>;

/// The catalogue's link facts as the store last measured them.
pub struct StoredLinks {
    pub entries: Vec<linkindex::Entry>,
    /// Index in the catalogue each entry came from.
    pub origin: Vec<usize>,
    /// When each entry was measured, in step with `entries`.
    pub measured_at: Vec<i64>,
    pub links: HashMapFacts,
}

/// ⭐ THE computation of the link facts from the store. A workflow pass
/// (`rulesapi::decide`) and the list's summary (`linkscan::refresh_summary`)
/// both call this one, so the "Hardlinks" column and a condition on
/// `external_links` cannot be two definitions of one number.
pub fn links_from_store(cat: &[CatalogueEntry], rows: &LinkRows) -> StoredLinks {
    let (entries, origin) = entries_from_store(cat, rows);
    let measured_at = origin
        .iter()
        .map(|&i| {
            rows.get(&(cat[i].info_hash.clone(), cat[i].session.clone()))
                .map(|r| r.1)
                .unwrap_or(0)
        })
        .collect();
    let links = linkindex::compute(&entries);
    StoredLinks { entries, origin, measured_at, links }
}

/// What the list shows, from what a pass or the scanner just computed.
///
/// Keyed by hash alone, like `compute`'s answer: when two engines hold the same
/// torrent, the copy listed last wins in both, so the column dates the very
/// measurement whose count it shows.
pub fn link_summary(
    cat: &[CatalogueEntry],
    origin: &[usize],
    measured_at: &[i64],
    links: &HashMapFacts,
) -> linkindex::Summary {
    let mut out = linkindex::Summary::default();
    for (&i, &at) in origin.iter().zip(measured_at) {
        let hash = &cat[i].info_hash;
        let (Some(f), Some(raw)) = (links.get(hash), crate::store::hex20(hash)) else {
            continue;
        };
        out.insert(raw, linkindex::Cached { external_links: f.external_links, measured_at: at });
    }
    out
}

/// The store row for one measured copy.
pub fn link_row(c: &CatalogueEntry, stats: &[Option<crate::platform::FileId>], now: i64) -> crate::store::LinkRow {
    crate::store::LinkRow {
        info_hash: c.info_hash.clone(),
        session: c.session.clone(),
        save_path: c.save_path.clone(),
        measured_at: now,
        files: stats.len() as i64,
        missing: linkindex::missing_files(stats) as i64,
        stats: linkindex::pack(stats),
    }
}

/// How many threads stat at once.
///
/// ⭐ Measured, not guessed: on the 293k catalogue the scanning thread spent
/// **98% of its life inside `statx`** at ~43 ms a call -- pure disk wait on
/// cold ZFS metadata, near-zero CPU. Threads here buy overlap in the disk
/// queue, so the count is set against the storage and not against the CPU.
///
/// ⚠️ Not unbounded, and lower than when the scan ran on demand: these are
/// real `statx` on the pool that is also serving torrents, around the clock
/// now that the index is kept by a background thread.
pub fn scan_threads() -> usize {
    std::env::var("HYDRANOS_LINK_SCAN_THREADS")
        .ok()
        .and_then(|v| v.parse::<usize>().ok())
        .filter(|n| *n > 0)
        .unwrap_or(16)
        .min(256)
}

/// Stat every file of a plan, in parallel. No lock held, no engine touched.
///
/// ## Why the work is pulled and not divided
///
/// ⚠️ A torrent is one unit of work here, and torrents are wildly uneven: an
/// Internet Archive item is three files, a season pack is eight hundred.
/// Handing each thread a contiguous slice of the plan would leave one of them
/// still stat-ing a pack long after the rest went idle. So the threads share a
/// cursor and take the next torrent when they finish one; the lock is held for
/// the length of an iterator step, against a work item that costs tens of
/// milliseconds.
///
/// The result comes back in plan order: a measurement whose shape depends on
/// thread scheduling is one nobody can reproduce from a log.
pub fn stat_plan_with(
    plan: Vec<(String, Vec<std::path::PathBuf>)>,
    threads: usize,
) -> Vec<linkindex::Entry> {
    let planned = plan.len();
    let threads = threads.max(1).min(planned.max(1));
    let queue = std::sync::Mutex::new(plan.into_iter().enumerate());
    let mut numbered: Vec<(usize, linkindex::Entry)> = Vec::with_capacity(planned);

    std::thread::scope(|scope| {
        let workers: Vec<_> = (0..threads)
            .map(|_| {
                let queue = &queue;
                scope.spawn(move || {
                    let mut mine: Vec<(usize, linkindex::Entry)> = Vec::new();
                    loop {
                        // Locked only to hand out the next item, never across
                        // the syscalls that follow.
                        let next = queue.lock().unwrap().next();
                        let Some((i, (hash, paths))) = next else { break };
                        let stats = paths
                            .into_iter()
                            .map(|p| {
                                let id = crate::platform::file_id(&p);
                                (p, id)
                            })
                            .collect();
                        mine.push((i, (hash, stats)));
                    }
                    mine
                })
            })
            .collect();

        for w in workers {
            // A panicking scan must not be silently half a scan: `compute`
            // would read the missing torrents as having no names of ours and
            // call somebody else's hardlinks external.
            numbered.extend(w.join().expect("link scan thread panicked"));
        }
    });

    numbered.sort_unstable_by_key(|(i, _)| *i);
    numbered.into_iter().map(|(_, e)| e).collect()
}

/// Put freshly measured link facts on the candidates, and only on them.
///
/// The other torrents were not re-measured; changing their facts now would let
/// a torrent nobody just looked at into the pass.
pub fn patch_link_facts(
    facts: &mut [Facts],
    links: &HashMapFacts,
    want: &std::collections::HashSet<String>,
) {
    for f in facts.iter_mut().filter(|f| want.contains(&f.info_hash)) {
        let l = links.get(&f.info_hash);
        f.link_count = l.map(|x| x.link_count as f64).unwrap_or(rules::NEVER);
        f.external_links = l.map(|x| x.external_links as f64).unwrap_or(rules::NEVER);
        f.freeable_bytes = l.map(|x| x.freeable_bytes as f64).unwrap_or(rules::NEVER);
        f.data_missing = l.is_some_and(|x| x.data_missing);
    }
}

pub type HashMapFacts = std::collections::HashMap<String, LinkFacts>;

/// Phase one: decide, without touching anything.
///
/// Also used verbatim by preview, which is the point -- a preview that ran
/// different code from the pass would be a preview of something else.
pub fn evaluate(w: &Workflow, facts: &[Facts]) -> Result<(Vec<Match>, PassReport), String> {
    let matcher = rules::compile_workflow(w).map_err(|e| e.to_string())?;
    let mut report = PassReport {
        workflow_id: w.id.clone(),
        workflow_name: w.name.clone(),
        ..Default::default()
    };
    let mut out = Vec::new();

    for f in facts {
        if !matcher(f) {
            continue;
        }
        report.matched += 1;

        let mut todo: Vec<Action> = w
            .then
            .iter()
            .filter(|a| !a.is_webhook() && !rules::already_satisfied(a, f))
            .cloned()
            .collect();
        // A webhook is never "already done". On a timer it goes out in the
        // pass where something else changes the torrent -- after which that
        // something converges and the webhook stops. On an event it goes out
        // every time: the event itself happens once.
        if !todo.is_empty() || w.trigger.is_event() {
            todo.extend(w.then.iter().filter(|a| a.is_webhook()).cloned());
        }
        if todo.is_empty() {
            report.skipped += 1;
            continue;
        }
        if out.len() >= w.cap {
            report.capped = true;
            break;
        }
        report.freed_bytes += if todo.iter().any(Action::is_delete) {
            f.total_size
        } else {
            0.0
        };
        out.push(Match {
            info_hash: f.info_hash.clone(),
            name: f.name.clone(),
            engine: f.engine.clone(),
            total_size: f.total_size,
            payload: if todo.iter().any(Action::is_webhook) { Some(webhook_payload(w, f)) } else { None },
            actions: todo,
        });
    }
    Ok((out, report))
}

/// The document a webhook receives.
///
/// Our fields under `torrent`, plus the same one-line summary under the three
/// names the common receivers read -- `content` (Discord), `text` (Slack,
/// Mattermost), `message` (Gotify) -- so pointing a workflow at one of them
/// works with nothing in between.
pub fn webhook_payload(w: &Workflow, f: &Facts) -> serde_json::Value {
    let event = w.trigger.event_name();
    let gib = f.total_size / (1024.0 * 1024.0 * 1024.0);
    let line = match w.trigger {
        rules::Trigger::Completed => format!("{} finished downloading ({gib:.2} GiB)", f.name),
        rules::Trigger::Added => format!("{} added ({gib:.2} GiB)", f.name),
        rules::Trigger::Schedule => format!("{}: {} ({gib:.2} GiB)", w.name, f.name),
    };
    let num = |x: f64| if x.is_finite() { serde_json::json!(x) } else { serde_json::Value::Null };
    serde_json::json!({
        "event": event,
        "workflow": w.name,
        "at": crate::store::now_secs(),
        "content": line,
        "text": line,
        "message": line,
        "torrent": {
            "info_hash": f.info_hash,
            "name": f.name,
            "category": f.category,
            "tags": f.tags,
            "engine": f.engine,
            "save_path": f.save_path,
            "state": f.state,
            "tracker": f.tracker_host,
            "size": num(f.total_size),
            "progress": num(f.progress),
            "ratio": num(f.ratio),
            "uploaded": num(f.total_uploaded),
            "downloaded": num(f.total_downloaded),
            "seeding_time": num(f.seeding_time),
        },
    })
}

/// POST one webhook, with two more tries on a failure.
///
/// ⚠️ The URL is never in the error: a Discord webhook URL IS its secret, and
/// errors go to the activity log that the whole UI can read.
pub fn send_webhook(url: &str, payload: &serde_json::Value) -> Result<(), String> {
    static CLIENT: std::sync::OnceLock<reqwest::blocking::Client> = std::sync::OnceLock::new();
    let client = CLIENT.get_or_init(|| {
        reqwest::blocking::Client::builder()
            .timeout(std::time::Duration::from_secs(10))
            .user_agent(typhon_engine::config::user_agent())
            .build()
            .unwrap_or_default()
    });
    let mut last = String::new();
    for (attempt, wait) in [0u64, 2, 5].into_iter().enumerate() {
        if wait > 0 {
            std::thread::sleep(std::time::Duration::from_secs(wait));
        }
        match client.post(url).json(payload).send() {
            Ok(r) if r.status().is_success() => return Ok(()),
            // A 4xx will not get better by asking again: the URL or the body
            // is wrong. Only a 5xx or no answer is retried.
            Ok(r) if r.status().is_client_error() => {
                return Err(format!("webhook: HTTP {}", r.status().as_u16()));
            }
            Ok(r) => last = format!("webhook: HTTP {} after {} tries", r.status().as_u16(), attempt + 1),
            Err(e) => {
                last = format!(
                    "webhook: {} after {} tries",
                    if e.is_timeout() { "timed out" } else if e.is_connect() { "could not connect" } else { "request failed" },
                    attempt + 1
                )
            }
        }
    }
    Err(last)
}

/// Is this workflow due to run?
///
/// Its own interval, measured from its own last run -- not from when the daemon
/// started, or every restart would fire every workflow at once.
pub fn is_due(w: &crate::store::StoredWorkflow, now: i64) -> bool {
    if !w.enabled {
        return false;
    }
    let interval = w.interval_secs.max(rules::MIN_INTERVAL_SECS);
    now - w.last_run >= interval
}

/// Refuse a deletion the scan no longer justifies.
///
/// ⭐ The cache is an hour old by design, and for tagging that is fine. For
/// `delete` it is not: between the scan and the action a cross-seed, an import
/// or a hand-made link can claim a name, and the whole point of
/// `external_links == 0` is that nobody else wants these bytes.
///
/// So the nlink values are read again, now, against the `owned` count the scan
/// established. Only an INCREASE refuses: a name that appeared since the scan
/// means someone took an interest, and the answer has to be no. A name that
/// disappeared leaves the stale count conservative, which is the harmless
/// direction.
///
/// ⚠️ Deliberately not a re-evaluation of the whole rule. The condition may
/// have stopped holding for a dozen reasons between the pass and the action;
/// this guards the one whose cost is irreversible.
pub fn link_guard(
    host: &EngineHost,
    m: &Match,
    cached: &LinkFacts,
) -> Result<(), String> {
    let Some(engine) = host.engines().iter().find(|e| e.id == m.engine) else {
        return Err(format!("engine {} is not running here", m.engine));
    };
    let Some(t) = engine
        .manager
        .all()
        .into_iter()
        .find(|t| t.info_hash.iter().map(|b| format!("{b:02x}")).collect::<String>() == m.info_hash)
    else {
        return Err("torrent is no longer in the engine".into());
    };
    let save_path = engine_save_path(&t);
    if save_path.is_empty() {
        return Err("no save path to check the links against".into());
    }
    let fresh = linkindex::recheck(&torrent_files(&t, &save_path), cached);
    linkindex::guard_verdict(&fresh, cached)
}

/// Record what happened, including what did not.
pub fn log(store: &Store, w: &Workflow, m: &Match, action: &str, outcome: &str, detail: &str) {
    let _ = store.log_workflow_activity(&ActivityEntry {
        at: crate::store::now_secs(),
        workflow_id: w.id.clone(),
        workflow_name: w.name.clone(),
        info_hash: m.info_hash.clone(),
        torrent_name: m.name.clone(),
        action: action.to_string(),
        outcome: outcome.to_string(),
        detail: detail.to_string(),
    });
}

/// Phase two: carry out one torrent's actions.
///
/// Takes the store lock per action rather than for the pass: a workflow
/// touching five hundred torrents must not hold the database while it does.
pub fn apply(
    host: &EngineHost,
    store: &Arc<crate::store::StoreLock>,
    w: &Workflow,
    m: &Match,
    pause_hook: &dyn Fn(&str, &str, bool),
    delete_hook: &dyn Fn(&str, &str, bool) -> Result<(), String>,
    // The scan's answer for this torrent, when the rule depended on it.
    // `Some` is what arms the guard on the delete path below.
    link_facts: Option<&LinkFacts>,
) -> Result<(), String> {
    for action in &m.actions {
        match action {
            Action::Pause | Action::Resume => {
                let paused = matches!(action, Action::Pause);
                {
                    let store = store.lock().map_err(|_| "store lock")?;
                    store
                        .set_paused(&m.info_hash, &m.engine, paused)
                        .map_err(|e| e.to_string())?;
                }
                // Through the same path a human click takes, so a workflow
                // cannot pause more or less thoroughly than a person does.
                pause_hook(&m.engine, &m.info_hash, paused);
            }
            Action::AddTags { tags } | Action::RemoveTags { tags } => {
                let adding = matches!(action, Action::AddTags { .. });
                let store = store.lock().map_err(|_| "store lock")?;
                // One row by primary key. This read the whole session -- a
                // million rows, ~2.9 s under the writer -- for one torrent's
                // tags, once per torrent: a 500-torrent pass held the store
                // for twenty-odd minutes.
                let mut current = store.tags_of(&m.info_hash);
                for t in tags {
                    current.retain(|x| x != t);
                    if adding {
                        current.push(t.clone());
                    }
                }
                // Torrent-wide, like every other tag write: a tag identifies
                // the content, so all copies carry it. Only execution state
                // (pause, pin) is per copy.
                store
                    .set_tags(&m.info_hash, &current)
                    .map_err(|e| e.to_string())?;
            }
            Action::SetCategory { to } => {
                // Only the label here. Moving the bytes is a job, submitted by
                // the caller: a pass that copied terabytes inline would hold
                // itself open for hours.
                let store = store.lock().map_err(|_| "store lock")?;
                // Per COPY, unlike the tag write above: a tag identifies the
                // content, a category decides where THIS copy lives and what
                // the drain may do with it. `Match` carries its engine.
                store
                    .set_category_in(&m.info_hash, &m.engine, to)
                    .map_err(|e| e.to_string())?;
            }
            Action::Delete { with_files } => {
                if !host.engines().iter().any(|e| e.id == m.engine) {
                    return Err(format!("engine {} is not running here", m.engine));
                }
                if crate::store::hex20(&m.info_hash).is_none() {
                    return Err("bad info hash".into());
                }
                // A rule that decided on link facts has to decide again, now:
                // the numbers it used may be up to an hour old.
                // No store lock: the guard reads the engine and the disk only.
                if let Some(cached) = link_facts {
                    link_guard(host, m, cached)?;
                }
                // Through the hook, which is the route a human click takes:
                // calling `manager.remove_torrent` and dropping the row here
                // skipped the lifetime-byte carry-over, so a workflow that
                // deleted torrents quietly erased everything they had ever
                // uploaded from the all-time totals.
                delete_hook(&m.engine, &m.info_hash, *with_files)?;
            }
            Action::Webhook { url } => {
                let payload = m.payload.clone().unwrap_or(serde_json::Value::Null);
                send_webhook(url, &payload)?;
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {

    /// The scan is parallel for one reason -- 43 ms of disk wait per file --
    /// and it is only allowed to be if it answers exactly what one thread
    /// would. Real files, a real hardlink, and the two runs compared whole.
    #[test]
    fn a_threaded_scan_answers_what_one_thread_answers() {
        let root = std::env::temp_dir().join(format!("hyd-scanpar-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();

        // Uneven on purpose: one torrent of 60 files against many of 1 is the
        // shape that exposes a scan which slices the plan instead of pulling
        // from it. The names are also what `owned` is counted from.
        let mut plan: Vec<(String, Vec<std::path::PathBuf>)> = Vec::new();
        for t in 0..40 {
            let n = if t == 7 { 60 } else { 1 };
            let mut paths = Vec::new();
            for f in 0..n {
                let path = root.join(format!("t{t}-f{f}.bin"));
                std::fs::write(&path, b"x").unwrap();
                paths.push(path);
            }
            plan.push((format!("{t:040x}"), paths));
        }
        // One file held twice by the catalogue (a cross-seed) and one held by
        // an outsider: the two cases whose arithmetic differs.
        let shared = root.join("t0-f0.bin");
        plan.push(("cross".repeat(8), vec![shared.clone()]));
        let outside = root.join("outside.hardlink");
        std::fs::hard_link(&shared, &outside).unwrap();

        let one = run_scan_with(plan.clone(), 1);
        let many = run_scan_with(plan, 16);

        assert_eq!(one.len(), many.len(), "same torrents answered");
        for (hash, facts) in &one {
            assert_eq!(
                format!("{:?}", facts),
                format!("{:?}", many.get(hash).expect("torrent missing from threaded scan")),
                "torrent {hash} answered differently"
            );
        }
        // And the answer is the right one, not merely a consistent one.
        let crossed = one.get(&"cross".repeat(8)).unwrap();
        assert_eq!(
            crossed.external_links, 1,
            "one name is ours twice over, the third is the outsider link"
        );

        let _ = std::fs::remove_dir_all(&root);
    }

    /// A plan smaller than the thread count must not spawn threads with
    /// nothing to do, and an empty plan must not spawn any.
    #[test]
    fn the_thread_count_never_exceeds_the_work() {
        assert!(run_scan_with(Vec::new(), 64).is_empty());
        let root = std::env::temp_dir().join(format!("hyd-scansmall-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let f = root.join("only.bin");
        std::fs::write(&f, b"x").unwrap();
        let out = run_scan_with(vec![("a".repeat(40), vec![f])], 64);
        assert_eq!(out.len(), 1);
        let _ = std::fs::remove_dir_all(&root);
    }

    /// A file the scan cannot stat is counted, not skipped in silence: it is
    /// the difference between "nobody else holds this" and "I could not look".
    #[test]
    fn a_missing_file_is_still_a_torrent_in_the_answer() {
        let out = run_scan_with(
            vec![(
                "b".repeat(40),
                vec![std::path::PathBuf::from("/nonexistent/hydranos/scan/file.bin")],
            )],
            4,
        );
        assert_eq!(out.len(), 1, "the torrent is answered even with no file");
    }
    use super::*;
    use crate::rules::{Cond, Node, Op};

    /// The measurement and the arithmetic in one call, as the scanner and a
    /// pass chain them.
    fn run_scan_with(plan: Vec<(String, Vec<std::path::PathBuf>)>, threads: usize) -> HashMapFacts {
        linkindex::compute(&stat_plan_with(plan, threads))
    }

    fn cat(hash: &str, session: &str, save_path: &str, paths: &[&str]) -> CatalogueEntry {
        CatalogueEntry {
            info_hash: hash.into(),
            session: session.into(),
            save_path: save_path.into(),
            paths: paths.iter().map(std::path::PathBuf::from).collect(),
        }
    }

    fn fid(index: u64, links: u64) -> Option<crate::platform::FileId> {
        Some(crate::platform::FileId { volume: 1, index, links, size: 10 })
    }

    /// Only a row that describes the files the torrent has NOW is used.
    #[test]
    fn a_stale_or_absent_measurement_leaves_the_torrent_unmeasured() {
        let catalogue = vec![
            cat("fresh", "hoard", "/d", &["/d/a"]),
            cat("moved", "hoard", "/new", &["/new/b"]),
            cat("regrown", "hoard", "/d", &["/d/c1", "/d/c2"]),
            cat("never", "hoard", "/d", &["/d/n"]),
            cat("fresh", "race", "/r", &["/r/a"]),
        ];
        let mut rows = std::collections::HashMap::new();
        rows.insert(("fresh".to_string(), "hoard".to_string()), ("/d".to_string(), 1, linkindex::pack(&[fid(1, 1)])));
        rows.insert(("moved".to_string(), "hoard".to_string()), ("/old".to_string(), 1, linkindex::pack(&[fid(2, 1)])));
        rows.insert(("regrown".to_string(), "hoard".to_string()), ("/d".to_string(), 1, linkindex::pack(&[fid(3, 1)])));
        let (got, origin) = entries_from_store(&catalogue, &rows);
        assert_eq!(origin, vec![0], "and it says where it came from");
        let hashes: Vec<&str> = got.iter().map(|(h, _)| h.as_str()).collect();
        assert_eq!(hashes, vec!["fresh"], "moved, regrown, never and the race copy have no valid row");
        assert_eq!(got[0].1, vec![(std::path::PathBuf::from("/d/a"), fid(1, 1))]);
    }

    /// ⭐ The reason candidates are measured again: the library hardlinked one
    /// of them after the index saw it. The pass must drop it, and must not
    /// pick up a torrent that was never re-measured in its place.
    #[test]
    fn a_candidate_the_fresh_measurement_contradicts_drops_out() {
        let lf = |ext: u64| LinkFacts { external_links: ext, link_count: ext + 1, freeable_bytes: 0, data_missing: false };
        let mut facts = facts(3);
        let h: Vec<String> = facts.iter().map(|f| f.info_hash.clone()).collect();
        let mut indexed = HashMapFacts::new();
        indexed.insert(h[0].clone(), lf(0));
        indexed.insert(h[1].clone(), lf(0));
        indexed.insert(h[2].clone(), lf(1));
        let want_all: std::collections::HashSet<String> = h.iter().cloned().collect();
        patch_link_facts(&mut facts, &indexed, &want_all);

        let mut w = wf(vec![Action::AddTags { tags: vec!["noHL".into()] }], 500);
        w.when = Node::Cond(Cond { field: "external_links".into(), op: Op::Eq, value: "0".into() });
        let (first, _) = evaluate(&w, &facts).unwrap();
        assert_eq!(first.len(), 2);

        // Fresh: torrent 0 gained an outside name; torrent 2 LOST its outside
        // name but was not a candidate, so it must not be let in.
        let want: std::collections::HashSet<String> = first.iter().map(|m| m.info_hash.clone()).collect();
        let mut fresh = HashMapFacts::new();
        fresh.insert(h[0].clone(), lf(1));
        fresh.insert(h[1].clone(), lf(0));
        fresh.insert(h[2].clone(), lf(0));
        patch_link_facts(&mut facts, &fresh, &want);
        let (second, _) = evaluate(&w, &facts).unwrap();
        let got: Vec<&str> = second.iter().map(|m| m.info_hash.as_str()).collect();
        assert_eq!(got, vec![h[1].as_str()]);
    }

    /// The whole road a fact now travels: stat, pack into the store's BLOB,
    /// unpack, count. Real files and a real outside hardlink, so a mistake in
    /// the encoding cannot hide behind a mock.
    #[test]
    fn facts_read_back_from_the_store_are_the_facts_the_scan_measured() {
        let root = std::env::temp_dir().join(format!("hyd-linkstore-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let ours = root.join("ours.bin");
        let lib = root.join("lib.bin");
        let cross = root.join("cross.bin");
        for p in [&ours, &lib] {
            std::fs::write(p, b"x").unwrap();
        }
        std::fs::hard_link(&ours, &cross).unwrap(); // a cross-seed of ours
        let outside = root.join("library-copy.mkv");
        std::fs::hard_link(&lib, &outside).unwrap(); // the media library

        let s = |p: &std::path::Path| p.to_str().unwrap().to_string();
        let catalogue = vec![
            cat("ours", "hoard", &s(&root), &[&s(&ours)]),
            cat("cross", "hoard", &s(&root), &[&s(&cross)]),
            cat("lib", "hoard", &s(&root), &[&s(&lib)]),
            cat("gone", "hoard", &s(&root), &[&s(&root.join("gone.bin"))]),
        ];
        let plan = catalogue.iter().map(|c| (c.info_hash.clone(), c.paths.clone())).collect();
        let measured = stat_plan_with(plan, 4);
        let direct = linkindex::compute(&measured);

        let mut rows = std::collections::HashMap::new();
        for (c, (_, files)) in catalogue.iter().zip(&measured) {
            let stats: Vec<_> = files.iter().map(|(_, st)| *st).collect();
            rows.insert((c.info_hash.clone(), c.session.clone()), (c.save_path.clone(), 1, linkindex::pack(&stats)));
        }
        let stored = linkindex::compute(&entries_from_store(&catalogue, &rows).0);

        assert_eq!(direct.len(), stored.len(), "every measured torrent comes back");
        for (h, f) in &direct {
            assert_eq!(stored.get(h), Some(f), "{h} read back differently");
        }
        assert_eq!(stored["ours"].external_links, 0, "a cross-seed of ours is not an outsider");
        assert_eq!(stored["lib"].external_links, 1, "the library holds a name");
        assert!(stored["gone"].data_missing);
        let _ = std::fs::remove_dir_all(&root);
    }

    fn facts(n: usize) -> Vec<Facts> {
        (0..n)
            .map(|i| Facts {
                info_hash: format!("{i:040x}"),
                name: format!("t{i}"),
                category: "in-progress".into(),
                engine: "hoard".into(),
                progress: 100.0,
                total_size: 1_000_000_000.0,
                seeding_time: 3.0 * 86400.0,
                ..Default::default()
            })
            .collect()
    }

    fn wf(then: Vec<Action>, cap: usize) -> Workflow {
        Workflow {
            id: "w1".into(),
            name: "test".into(),
            enabled: true,
            position: 0,
            trigger: rules::Trigger::Schedule,
            interval_secs: rules::DEFAULT_INTERVAL_SECS,
            when: Node::Cond(Cond {
                field: "progress".into(),
                op: Op::Ge,
                value: "100".into(),
            }),
            then,
            cap,
        }
    }

    #[test]
    fn a_pass_reports_what_it_would_do() {
        let w = wf(vec![Action::SetCategory { to: "done".into() }], 500);
        let (matches, report) = evaluate(&w, &facts(3)).unwrap();
        assert_eq!(report.matched, 3);
        assert_eq!(matches.len(), 3);
        assert_eq!(report.skipped, 0);
    }

    /// The convergence rule: a torrent already in the target category is
    /// matched but not acted on, so the second pass changes nothing.
    #[test]
    fn a_workflow_converges_instead_of_reapplying_itself() {
        let w = wf(
            vec![Action::SetCategory {
                to: "in-progress".into(),
            }],
            500,
        );
        let (matches, report) = evaluate(&w, &facts(3)).unwrap();
        assert_eq!(report.matched, 3, "they all match the condition");
        assert_eq!(report.skipped, 3, "and are all already where they belong");
        assert!(matches.is_empty(), "so nothing is left to do");
    }

    /// ⭐ On a timer, the webhook rides on the action that changes the
    /// torrent: sent in the pass that tags it, never again once it is tagged.
    #[test]
    fn a_scheduled_webhook_goes_out_once_with_the_change_that_converges() {
        let hook = Action::Webhook { url: "https://hooks.example/x".into() };
        let w = wf(vec![Action::AddTags { tags: vec!["told".into()] }, hook.clone()], 500);
        let mut f = facts(1);
        let (m, _) = evaluate(&w, &f).unwrap();
        assert_eq!(m[0].actions, vec![Action::AddTags { tags: vec!["told".into()] }, hook]);
        let p = m[0].payload.as_ref().expect("a payload for a webhook");
        assert_eq!(p["torrent"]["name"], "t0");
        assert_eq!(p["event"], "matched");
        assert!(p["content"].as_str().unwrap().contains("t0"), "Discord reads `content`");

        f[0].tags.push("told".into());
        let (m, r) = evaluate(&w, &f).unwrap();
        assert!(m.is_empty(), "tagged: nothing left, the webhook included");
        assert_eq!(r.skipped, 1);
    }

    /// On a completion the webhook goes out even when the other actions
    /// have nothing to change: the event itself happens once.
    #[test]
    fn a_completion_webhook_goes_out_even_if_nothing_else_changes() {
        let hook = Action::Webhook { url: "https://hooks.example/x".into() };
        let mut w = wf(vec![Action::SetCategory { to: "in-progress".into() }, hook.clone()], 500);
        w.trigger = rules::Trigger::Completed;
        let (m, _) = evaluate(&w, &facts(1)).unwrap();
        assert_eq!(m[0].actions, vec![hook], "already in the category, still told");
        assert_eq!(m[0].payload.as_ref().unwrap()["event"], "completed");
        // And no payload is built where no webhook asks for one.
        let plain = wf(vec![Action::SetCategory { to: "done".into() }], 500);
        assert!(evaluate(&plain, &facts(1)).unwrap().0[0].payload.is_none());
    }

    /// The cap is what stops a mistyped rule touching the whole catalogue in
    /// one pass, and it has to be visible in the report or nobody learns why
    /// only some torrents moved.
    #[test]
    fn the_cap_bounds_a_pass_and_says_so() {
        let w = wf(vec![Action::SetCategory { to: "done".into() }], 2);
        let (matches, report) = evaluate(&w, &facts(10)).unwrap();
        assert_eq!(matches.len(), 2);
        assert!(report.capped);
    }

    /// Free-space accounting: the report carries the bytes a delete would
    /// release, so a caller can stop once it has freed enough instead of
    /// deleting everything that matched.
    #[test]
    fn a_delete_pass_reports_the_bytes_it_would_free() {
        let w = wf(vec![Action::Delete { with_files: true }], 500);
        let (_, report) = evaluate(&w, &facts(3)).unwrap();
        assert_eq!(report.freed_bytes, 3_000_000_000.0);
    }

    /// A disabled workflow never runs, and an enabled one waits out its own
    /// interval rather than firing on every tick of the scheduler.
    #[test]
    fn only_enabled_workflows_past_their_interval_are_due() {
        let mut w = crate::store::StoredWorkflow {
            enabled: false,
            interval_secs: 900,
            last_run: 0,
            ..Default::default()
        };
        assert!(!is_due(&w, 10_000));
        w.enabled = true;
        assert!(is_due(&w, 10_000));
        w.last_run = 9_500;
        assert!(!is_due(&w, 10_000), "only 500s of a 900s interval");
        // A one-second interval is clamped to the floor, so a workflow cannot
        // become a load generator by typo.
        w.interval_secs = 1;
        w.last_run = 9_990;
        assert!(!is_due(&w, 10_000));
    }

    #[test]
    fn a_workflow_that_does_not_compile_reports_why_instead_of_running() {
        let mut w = wf(vec![Action::Pause], 500);
        w.when = Node::Cond(Cond {
            field: "nonesuch".into(),
            op: Op::Eq,
            value: "x".into(),
        });
        assert!(evaluate(&w, &facts(1)).unwrap_err().contains("nonesuch"));
    }
}
