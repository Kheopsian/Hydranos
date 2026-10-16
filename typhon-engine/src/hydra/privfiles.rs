//! `[daemon] private_files`: the daemon's own files readable by its user only.
//!
//! The log, the config (it holds the API key and other secrets), the store
//! `hydra.db` and the telemetry `bench.db` (torrent names, peers), each with
//! its `-wal`/`-shm`/`-journal`, are created 0600, and ones an earlier run
//! left at 0644 are brought back to 0600 at startup. Nothing here touches the
//! process umask: downloaded files keep the mode their writer gave them. Off
//! by default; without effect off Unix.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};

static ON: AtomicBool = AtomicBool::new(false);

/// Whether files the daemon creates from now on are created 0600.
pub fn enabled() -> bool {
    ON.load(Ordering::Relaxed)
}

/// The store's files: the database, and what SQLite keeps beside it (they come
/// and go, and take the database file's mode when SQLite creates them).
pub fn store_files(db: &Path) -> Vec<PathBuf> {
    ["", "-wal", "-shm", "-journal"]
        .iter()
        .map(|suffix| {
            let mut p = db.as_os_str().to_owned();
            p.push(suffix);
            PathBuf::from(p)
        })
        .collect()
}

/// The log and its rotated copies (`.1` to `.4`).
pub fn log_files(log: &Path) -> Vec<PathBuf> {
    std::iter::once(log.to_path_buf())
        .chain((1..=4).map(|i| {
            let mut p = log.as_os_str().to_owned();
            p.push(format!(".{i}"));
            PathBuf::from(p)
        }))
        .collect()
}

/// Bring one existing file to 0600. A missing file is not an error.
pub fn tighten(path: &Path) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        match std::fs::metadata(path) {
            Ok(m) if m.is_file() && m.permissions().mode() & 0o777 != 0o600 => {
                if let Err(e) = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)) {
                    tracing::warn!(path = %path.display(), error = %e, "private_files: cannot restrict the file");
                }
            }
            _ => {}
        }
    }
    #[cfg(not(unix))]
    let _ = path;
}

/// Create `path` empty with mode 0600 if it does not exist (so no moment at
/// the umask's mode). Used before SQLite creates the database.
pub fn precreate(path: &Path) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        let _ = std::fs::OpenOptions::new().write(true).create_new(true).mode(0o600).open(path);
    }
    #[cfg(not(unix))]
    let _ = path;
}

/// `OpenOptions` for the log: 0600 at creation when the option is on.
pub fn log_open_options() -> std::fs::OpenOptions {
    let mut o = std::fs::OpenOptions::new();
    o.create(true).append(true);
    #[cfg(unix)]
    if enabled() {
        use std::os::unix::fs::OpenOptionsExt;
        o.mode(0o600);
    }
    o
}

/// Apply the option as read from the config. On: files made from now on are
/// 0600 and the existing ones are tightened. Off: nothing changes.
pub fn apply(on: bool, config: &Path, log: &Path, db: &Path, bench: &Path) {
    ON.store(on, Ordering::Relaxed);
    if !on {
        return;
    }
    tighten(config);
    for p in log_files(log).iter().chain(store_files(db).iter()).chain(store_files(bench).iter()) {
        tighten(p);
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    /// The three tests flip the process-wide flag: one at a time.
    static FLAG: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn flag_lock() -> std::sync::MutexGuard<'static, ()> {
        FLAG.lock().unwrap_or_else(|p| p.into_inner())
    }

    fn mode(p: &Path) -> u32 {
        std::fs::metadata(p).unwrap().permissions().mode() & 0o777
    }

    fn put(p: &Path) {
        std::fs::write(p, b"x").unwrap();
        std::fs::set_permissions(p, std::fs::Permissions::from_mode(0o644)).unwrap();
    }

    fn dir(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("hydra-priv-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn private_files_brings_existing_files_to_0600() {
        let _g = flag_lock();
        let d = dir("on");
        let (cfg, log, db, bench) = (d.join("hydranos.toml"), d.join("hydranos.log"), d.join("hydra.db"), d.join("bench.db"));
        put(&cfg);
        put(&log);
        put(&d.join("hydranos.log.1"));
        // A database left at 0644, with the -wal and -shm SQLite makes after the fact.
        let conn = rusqlite::Connection::open(&db).unwrap();
        conn.query_row("PRAGMA journal_mode=WAL", [], |_| Ok(())).unwrap();
        conn.execute_batch("CREATE TABLE t(x); INSERT INTO t VALUES (1);").unwrap();
        for p in store_files(&db) {
            if p.exists() {
                std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o644)).unwrap();
            }
        }
        let bench_db = crate::benchdb::BenchDb::open(&bench).unwrap();
        for p in store_files(&bench) {
            if p.exists() {
                std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o644)).unwrap();
            }
        }
        assert!(d.join("bench.db-wal").exists(), "bench.db is in WAL mode");
        assert!(d.join("hydra.db-wal").exists(), "the database is in WAL mode");
        assert!(d.join("hydra.db-shm").exists());
        apply(true, &cfg, &log, &db, &bench);
        for p in [&cfg, &log, &d.join("hydranos.log.1"), &db, &d.join("hydra.db-wal"), &d.join("hydra.db-shm"), &bench, &d.join("bench.db-wal"), &d.join("bench.db-shm")] {
            assert_eq!(mode(p), 0o600, "{}", p.display());
        }
        drop(conn);
        drop(bench_db);
        ON.store(false, Ordering::Relaxed);
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn private_files_off_changes_nothing() {
        let _g = flag_lock();
        let d = dir("off");
        let (cfg, log, db, bench) = (d.join("hydranos.toml"), d.join("hydranos.log"), d.join("hydra.db"), d.join("bench.db"));
        for p in [&cfg, &log, &db, &bench] {
            put(p);
        }
        apply(false, &cfg, &log, &db, &bench);
        for p in [&cfg, &log, &db, &bench] {
            assert_eq!(mode(p), 0o644, "{}", p.display());
        }
        assert!(!enabled());
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn private_files_creates_a_new_store_0600() {
        let _g = flag_lock();
        let d = dir("new");
        let db = d.join("hydra.db");
        ON.store(true, Ordering::Relaxed);
        let store = crate::store::Store::open(&db, false).unwrap();
        let bench = d.join("bench.db");
        let bench_db = crate::benchdb::BenchDb::open(&bench).unwrap();
        ON.store(false, Ordering::Relaxed);
        for p in store_files(&db).into_iter().chain(store_files(&bench)) {
            if p.exists() {
                assert_eq!(mode(&p), 0o600, "{}", p.display());
            }
        }
        drop(store);
        drop(bench_db);
        let _ = std::fs::remove_dir_all(&d);
    }

    /// What the option changes: with it off, a new database takes the mode its
    /// creator's umask gives it, like any file written beside it.
    #[test]
    fn a_new_store_takes_the_umask_mode_when_the_option_is_off() {
        let _g = flag_lock();
        let d = dir("umask");
        let reference = d.join("reference");
        std::fs::write(&reference, b"x").unwrap();
        let (db, bench) = (d.join("hydra.db"), d.join("bench.db"));
        let store = crate::store::Store::open(&db, false).unwrap();
        let bench_db = crate::benchdb::BenchDb::open(&bench).unwrap();
        for p in [&db, &bench] {
            assert_eq!(mode(p), mode(&reference), "{}", p.display());
        }
        drop(store);
        drop(bench_db);
        let _ = std::fs::remove_dir_all(&d);
    }
}
