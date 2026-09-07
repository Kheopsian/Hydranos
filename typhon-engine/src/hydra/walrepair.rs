//! Opening a database that lives on a network share.
//!
//! SQLite in WAL mode needs shared-memory locking that SMB and NFS do not
//! provide. A library moved onto a share therefore stops opening, and the
//! daemon cannot start -- which is exactly when it must still be able to say
//! why. The fix is a two-byte change to the file header: offsets 18 and 19
//! carry the write and read format versions, 2 for WAL and 1 for the rollback
//! journal, and that is the whole difference between a file the nolock VFS can
//! open and one it cannot.
//!
//! The honest way to make that change is to let SQLite make it
//! (`PRAGMA journal_mode=DELETE`, which also checkpoints anything still in the
//! -wal). The bytes are only written by hand when the share refuses that write,
//! and then only once it is certain there is nothing unmerged to lose.
//!
//! Ported from internal/sqlitex/walrepair.go.

use std::io::Read;
use std::path::Path;

const HDR_WRITE_VERSION: usize = 18;
const HDR_READ_VERSION: usize = 19;
const FMT_WAL: u8 = 2;
const HDR_LEN: usize = 100;
const MAGIC: &[u8] = b"SQLite format 3";

/// Why a database cannot be opened where it now lives.
#[derive(Debug, Default, Clone, serde::Serialize)]
pub struct Diagnosis {
    pub path: String,
    /// The path resolves to a filesystem that forces the nolock fallback.
    pub on_network: bool,
    /// The file exists and its header says WAL.
    pub in_wal: bool,
    /// A -wal sidecar still holds content, which makes checkpointing the only
    /// safe route: rewriting the header now would drop committed transactions.
    pub hot_wal: bool,
}

impl Diagnosis {
    /// Only the WAL-on-a-share case is this code's business. An absent file, a
    /// local disk, or a database already using the rollback journal must open
    /// normally.
    pub fn needs_repair(&self) -> bool {
        self.on_network && self.in_wal
    }
}

/// Inspect the file directly rather than through SQLite, so the answer is still
/// available in exactly the situation where opening is what fails.
pub fn diagnose(path: &Path) -> Result<Diagnosis, String> {
    let mut d = Diagnosis {
        path: path.display().to_string(),
        on_network: is_network(path),
        ..Default::default()
    };

    match header_says_wal(path) {
        Ok(in_wal) => d.in_wal = in_wal,
        Err(e) if e == "missing" => return Ok(d), // never created: it will be created correctly
        Err(e) => return Err(e),
    }

    let wal = path.with_extension(format!(
        "{}-wal",
        path.extension().and_then(|e| e.to_str()).unwrap_or("")
    ));
    let sidecar = if wal.exists() {
        wal
    } else {
        std::path::PathBuf::from(format!("{}-wal", path.display()))
    };
    if let Ok(meta) = std::fs::metadata(&sidecar) {
        d.hot_wal = meta.len() > 0;
    }
    Ok(d)
}

/// Read the two format bytes.
fn header_says_wal(path: &Path) -> Result<bool, String> {
    let mut file = match std::fs::File::open(path) {
        Ok(f) => f,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Err("missing".into()),
        Err(e) => return Err(e.to_string()),
    };
    let size = file.metadata().map_err(|e| e.to_string())?.len();
    if size == 0 {
        // SQLite creates the file before it writes a header: nothing has been
        // decided yet, so there is nothing to convert.
        return Ok(false);
    }

    let mut hdr = [0u8; HDR_LEN];
    let mut read = 0usize;
    loop {
        match file.read(&mut hdr[read..]) {
            Ok(0) => break,
            Ok(n) => {
                read += n;
                if read == HDR_LEN {
                    break;
                }
            }
            Err(e) => return Err(e.to_string()),
        }
    }

    if read < MAGIC.len() || &hdr[..MAGIC.len()] != MAGIC {
        return Err(format!("not a SQLite database: {}", path.display()));
    }
    if read < HDR_LEN {
        return Err(format!("truncated SQLite header: {}", path.display()));
    }
    Ok(hdr[HDR_WRITE_VERSION] == FMT_WAL || hdr[HDR_READ_VERSION] == FMT_WAL)
}

/// Filesystem magics that force the nolock fallback.
///
/// Checked by magic rather than by mount table: a bind mount or an overlay can
/// hide the share's name while keeping its semantics, and the semantics are
/// what breaks WAL.
fn is_network(path: &Path) -> bool {
    const NFS: i64 = 0x6969;
    const SMB: i64 = 0x517B;
    const CIFS: i64 = 0xFF53_4D42_u32 as i64;
    const SMB2: i64 = 0xFE53_4D42_u32 as i64;
    const FUSE: i64 = 0x6573_5546;

    let Some(parent) = path.parent() else {
        return false;
    };
    let Ok(c_path) = std::ffi::CString::new(parent.to_string_lossy().as_bytes()) else {
        return false;
    };
    let mut st: libc::statfs = unsafe { std::mem::zeroed() };
    if unsafe { libc::statfs(c_path.as_ptr(), &mut st) } != 0 {
        return false;
    }
    matches!(st.f_type as i64, NFS | SMB | CIFS | SMB2 | FUSE)
}

/// Copy the database beside itself before anything rewrites it.
///
/// The -wal and -shm are deliberately NOT copied: a conversion only ever runs
/// against a checkpointed database, so they hold nothing the copy would need,
/// and a stale -shm restored next to a file would confuse the next open.
pub fn backup(path: &Path) -> Result<String, String> {
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let destination = format!("{}.bak-preconvert-{}", path.display(), stamp);

    // A copy that cannot be finished (no space, share dropped) is removed
    // rather than left as a half file somebody might later trust.
    if let Err(e) = std::fs::copy(path, &destination) {
        let _ = std::fs::remove_file(&destination);
        return Err(e.to_string());
    }
    Ok(destination)
}

/// Take a database out of WAL mode.
///
/// The clean route first: ask SQLite to checkpoint and switch journal mode.
/// Only if that fails is the header rewrite considered, and only when the log
/// is COLD -- rewriting two bytes while the -wal still holds committed pages
/// discards them silently. Losing the library is worse than not starting.
pub fn convert(path: &Path) -> Result<String, String> {
    let d = diagnose(path)?;
    if !d.in_wal {
        // Nothing to do, and that is not a failure.
        return Ok("already-rollback".to_string());
    }

    let clean = rusqlite::Connection::open(path)
        .and_then(|conn| conn.pragma_update(None, "journal_mode", "DELETE"));
    match clean {
        Ok(()) => return Ok("pragma".to_string()),
        Err(e) if d.hot_wal => {
            return Err(format!(
                "the write-ahead log still holds data and this filesystem refused \
the checkpoint, so the database was left untouched: {e}"
            ))
        }
        Err(_) => {}
    }

    use std::io::{Seek, SeekFrom, Write};
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .open(path)
        .map_err(|e| e.to_string())?;
    file.seek(SeekFrom::Start(18)).map_err(|e| e.to_string())?;
    // Offsets 18 and 19: write and read format versions. 1 is the rollback
    // journal, which the nolock VFS can open.
    file.write_all(&[1u8, 1u8]).map_err(|e| e.to_string())?;
    file.sync_all().map_err(|e| e.to_string())?;
    let _ = std::fs::remove_file(format!("{}-wal", path.display()));
    let _ = std::fs::remove_file(format!("{}-shm", path.display()));
    Ok("header".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn write(dir: &Path, name: &str, bytes: &[u8]) -> std::path::PathBuf {
        let path = dir.join(name);
        let mut f = std::fs::File::create(&path).unwrap();
        f.write_all(bytes).unwrap();
        path
    }

    fn header(write_version: u8, read_version: u8) -> Vec<u8> {
        let mut hdr = vec![0u8; HDR_LEN];
        hdr[..MAGIC.len()].copy_from_slice(MAGIC);
        hdr[HDR_WRITE_VERSION] = write_version;
        hdr[HDR_READ_VERSION] = read_version;
        hdr
    }

    #[test]
    fn a_wal_header_is_recognised_and_a_rollback_one_is_not() {
        let dir = std::env::temp_dir().join("walrepair-test-1");
        std::fs::create_dir_all(&dir).unwrap();

        let wal = write(&dir, "wal.db", &header(2, 2));
        assert!(header_says_wal(&wal).unwrap());

        let legacy = write(&dir, "legacy.db", &header(1, 1));
        assert!(!header_says_wal(&legacy).unwrap());

        // Either byte alone is enough to mean WAL.
        let half = write(&dir, "half.db", &header(1, 2));
        assert!(header_says_wal(&half).unwrap());

        std::fs::remove_dir_all(&dir).ok();
    }

    // A file SQLite has created but not yet written a header into is not a
    // database to convert -- treating it as one would rewrite bytes that mean
    // nothing yet.
    #[test]
    fn an_empty_file_has_decided_nothing() {
        let dir = std::env::temp_dir().join("walrepair-test-2");
        std::fs::create_dir_all(&dir).unwrap();
        let empty = write(&dir, "empty.db", b"");
        assert!(!header_says_wal(&empty).unwrap());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn something_that_is_not_a_database_is_refused_by_name() {
        let dir = std::env::temp_dir().join("walrepair-test-3");
        std::fs::create_dir_all(&dir).unwrap();

        let junk = write(&dir, "junk.db", b"this is not a database at all, really");
        let err = header_says_wal(&junk).unwrap_err();
        assert!(err.contains("not a SQLite database"), "{err}");

        // Right magic, too short to hold a header: also refused, differently.
        let short = write(&dir, "short.db", MAGIC);
        let err = header_says_wal(&short).unwrap_err();
        assert!(err.contains("truncated"), "{err}");

        std::fs::remove_dir_all(&dir).ok();
    }

    // The pair that matters: repair is for WAL ON A SHARE. A WAL database on a
    // local disk opens fine and must be left alone.
    #[test]
    fn repair_needs_both_the_share_and_the_wal() {
        let both = Diagnosis { on_network: true, in_wal: true, ..Default::default() };
        assert!(both.needs_repair());

        for d in [
            Diagnosis { on_network: true, in_wal: false, ..Default::default() },
            Diagnosis { on_network: false, in_wal: true, ..Default::default() },
            Diagnosis::default(),
        ] {
            assert!(!d.needs_repair(), "{d:?} must open normally");
        }
    }

    #[test]
    fn a_wal_database_is_converted_by_the_clean_route() {
        let dir = std::env::temp_dir();
        let path = dir.join("hydra-walrepair-convert.db");
        let _ = std::fs::remove_file(&path);
        {
            let conn = rusqlite::Connection::open(&path).unwrap();
            conn.execute_batch("CREATE TABLE t (a INTEGER); INSERT INTO t VALUES (1);").unwrap();
            conn.pragma_update(None, "journal_mode", "WAL").unwrap();
            conn.execute("INSERT INTO t VALUES (2)", []).unwrap();
        }
        assert_eq!(convert(&path).unwrap(), "pragma");
        // And converting again is a no-op rather than an error.
        assert_eq!(convert(&path).unwrap(), "already-rollback");
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn a_missing_database_is_not_an_error() {
        let path = std::env::temp_dir().join("walrepair-absent.db");
        std::fs::remove_file(&path).ok();
        let d = diagnose(&path).unwrap();
        assert!(!d.in_wal && !d.needs_repair());
    }
}
