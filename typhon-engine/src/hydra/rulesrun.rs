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
pub fn gather(
    host: &EngineHost,
    store: &Store,
    engine_id: &str,
    links: &std::collections::HashMap<String, LinkFacts>,
) -> Vec<Facts> {
    let Some(engine) = host.engines().iter().find(|e| e.id == engine_id) else {
        return Vec::new();
    };
    let stored = store.workflow_facts(engine_id).unwrap_or_default();
    let now = crate::store::now_secs() as f64;

    engine
        .manager
        .all()
        .into_iter()
        .map(|t| {
            let hash: String = t.info_hash.iter().map(|b| format!("{b:02x}")).collect();
            let s = stored.get(&hash).cloned().unwrap_or_default();

            // ⚠️ NEVER, not zero, when no scan ran. Zero is a measurement, and
            // `external_links == 0` means "safe to delete" -- defaulting to it
            // would arm every deletion rule against the whole catalogue before
            // a single file had been looked at.
            let l = links.get(&hash);
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
                ratio: if downloaded > 0.0 {
                    uploaded / downloaded
                } else {
                    0.0
                },
                total_size: size,
                total_uploaded: uploaded,
                total_downloaded: downloaded,
                seeding_time: s.seeding_time as f64,
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
                link_count: l.map(|x| x.link_count as f64).unwrap_or(rules::NEVER),
                external_links: l.map(|x| x.external_links as f64).unwrap_or(rules::NEVER),
                freeable_bytes: l.map(|x| x.freeable_bytes as f64).unwrap_or(rules::NEVER),
                data_missing: l.is_some_and(|x| x.data_missing),
                ..Default::default()
            }
        })
        .collect()
}

/// Every file this catalogue holds, stat'd once, across ALL engines.
///
/// ⭐ Global on purpose, and it is not an optimisation. `owned` must count every
/// name we hold; a file held by hoard AND by race is two of ours. Building this
/// per engine would see one name, report an external holder that does not
/// exist, and the arithmetic would be wrong in the direction that keeps rubbish
/// forever -- or, with the engines the other way round, deletes a live file.
pub fn scan_links(host: &EngineHost, store: &Store) -> std::collections::HashMap<String, LinkFacts> {
    let mut entries: Vec<linkindex::Entry> = Vec::new();
    for engine in host.engines().iter() {
        let stored = store.workflow_facts(&engine.id).unwrap_or_default();
        for t in engine.manager.all() {
            let hash: String = t.info_hash.iter().map(|b| format!("{b:02x}")).collect();
            let Some(save_path) = stored.get(&hash).map(|s| s.save_path.clone()) else {
                continue;
            };
            if save_path.is_empty() {
                continue;
            }
            let base = std::path::Path::new(&save_path);
            // The layout rule `Layout::on_disk` encodes: a multi-file torrent
            // puts its files under a folder named after the torrent, a
            // single-file one is the name itself at the root of save_path.
            // ⚠️ Walking save_path instead would collect the NEIGHBOURS of a
            // torrent that shares a folder, and credit it with their links.
            let files = if t.meta.multi_file {
                let root = base.join(&t.meta.name);
                t.meta
                    .files
                    .iter()
                    .map(|f| root.join(&f.path))
                    .collect::<Vec<_>>()
            } else {
                vec![base.join(&t.meta.name)]
            };
            entries.push((
                hash,
                files
                    .into_iter()
                    .map(|p| {
                        let id = crate::platform::file_id(&p);
                        (p, id)
                    })
                    .collect(),
            ));
        }
    }
    linkindex::compute(&entries)
}

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

        let todo: Vec<Action> = w
            .then
            .iter()
            .filter(|a| !rules::already_satisfied(a, f))
            .cloned()
            .collect();
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
            actions: todo,
        });
    }
    Ok((out, report))
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
    store: &Arc<std::sync::Mutex<Store>>,
    w: &Workflow,
    m: &Match,
    pause_hook: &dyn Fn(&str, &str, bool),
    delete_hook: &dyn Fn(&str, &str, bool) -> Result<(), String>,
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
                let mut current = store
                    .workflow_facts(&m.engine)
                    .unwrap_or_default()
                    .get(&m.info_hash)
                    .cloned()
                    .unwrap_or_default()
                    .tags;
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
                // Through the hook, which is the route a human click takes:
                // calling `manager.remove_torrent` and dropping the row here
                // skipped the lifetime-byte carry-over, so a workflow that
                // deleted torrents quietly erased everything they had ever
                // uploaded from the all-time totals.
                delete_hook(&m.engine, &m.info_hash, *with_files)?;
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rules::{Cond, Node, Op};

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
