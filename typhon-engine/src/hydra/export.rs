//! Exporting a selection: its hashes, a CSV of what the store knows, or the
//! `.torrent` files themselves in a zip.
//!
//! Built for the worst selection, not the typical one. Ctrl+A on a
//! million-torrent library is two keys away, and its `.torrent` files weigh
//! tens of gigabytes. So nothing here holds the whole answer: rows are read in
//! batches on the read-only connection and the guard is dropped before a byte
//! is written, and the archive leaves as it is produced. The zip's central
//! directory, the one part that has to wait for the end, is spilled to a
//! temporary file instead of growing in memory.
//!
//! The zip writer is ours rather than the `zip` crate's: that one needs a
//! seekable output and keeps every entry's metadata in memory until it
//! finishes. Stored entries whose bytes are in hand before their header is
//! written need neither, and ZIP64 is three fixed records.

use std::io::{self, Read, Seek, Write};

use crate::store::{ExportRow, StoreLock};

/// Hashes read per trip to the store. Small enough that the read guard is
/// never held long, large enough that a million hashes is 2 000 trips.
pub const BATCH: usize = 500;

/// A request body big enough for a million hashes, url-encoded and comma
/// separated (43 bytes each), with room to spare. The router's default is
/// 2 MiB, which is ~48 000 hashes: a Ctrl+A on a large library would be
/// refused before the handler ever ran.
pub const BODY_MAX: usize = 96 << 20;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Format {
    Zip,
    Txt,
    Csv,
}

impl Format {
    pub fn parse(s: &str) -> Option<Format> {
        match s {
            "" | "zip" => Some(Format::Zip),
            "txt" | "hashes" => Some(Format::Txt),
            "csv" => Some(Format::Csv),
            _ => None,
        }
    }

    pub fn content_type(self) -> &'static str {
        match self {
            Format::Zip => "application/zip",
            Format::Txt => "text/plain; charset=utf-8",
            Format::Csv => "text/csv; charset=utf-8",
        }
    }

    pub fn file_name(self, count: usize) -> String {
        match self {
            Format::Zip => format!("hydranos-{count}-torrents.zip"),
            Format::Txt => format!("hydranos-{count}-hashes.txt"),
            Format::Csv => format!("hydranos-{count}-torrents.csv"),
        }
    }
}

/// What an export did, for the log line.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Summary {
    pub written: usize,
    pub missing: usize,
}

/// Lowercase, keep only what can be an info hash, drop repeats, keep order.
///
/// A v1 hash is 40 hex digits, a v2 one 64. Anything else cannot name a row
/// and is dropped here rather than reported as "not in the library", which
/// would suggest it might have been.
pub fn clean_hashes(raw: &[String]) -> Vec<String> {
    let mut seen = std::collections::HashSet::with_capacity(raw.len());
    raw.iter()
        .map(|h| h.trim().to_ascii_lowercase())
        .filter(|h| (h.len() == 40 || h.len() == 64) && h.bytes().all(|b| b.is_ascii_hexdigit()))
        .filter(|h| seen.insert(h.clone()))
        .collect()
}

/// Write the export of `hashes` to `out`.
///
/// `hashes` must already be clean (`clean_hashes`). A hash list needs no store
/// at all: the selection is the answer, including torrents that live on
/// another node.
pub fn run<W: Write>(
    store: &StoreLock,
    hashes: &[String],
    format: Format,
    strip_trackers: bool,
    out: W,
) -> io::Result<Summary> {
    match format {
        Format::Txt => {
            let mut out = out;
            for h in hashes {
                out.write_all(h.as_bytes())?;
                out.write_all(b"\n")?;
            }
            out.flush()?;
            Ok(Summary { written: hashes.len(), missing: 0 })
        }
        Format::Csv => {
            let mut out = out;
            out.write_all(b"info_hash,name,size,category,tags,trackers,added_unix,save_path\n")?;
            let mut summary = Summary::default();
            each_row(store, hashes, |hash, row| {
                match row {
                    Some(row) => {
                        out.write_all(csv_line(row).as_bytes())?;
                        summary.written += 1;
                    }
                    // Still a line: dropping it would make the file disagree
                    // with the selection without saying so.
                    None => {
                        writeln!(out, "{hash},,,,,,,")?;
                        summary.missing += 1;
                    }
                }
                Ok(())
            })?;
            out.flush()?;
            Ok(summary)
        }
        Format::Zip => {
            let mut zip = ZipStream::new(out)?;
            let mut summary = Summary::default();
            let mut missing: Vec<(String, &'static str)> = Vec::new();
            each_row(store, hashes, |hash, row| {
                let Some(row) = row else {
                    missing.push((hash.to_string(), "not in this node's library"));
                    return Ok(());
                };
                let name = typhon_engine::torrent::metainfo::parse_torrent_bytes(&row.torrent)
                    .map(|m| m.name)
                    .unwrap_or_default();
                let data = if strip_trackers {
                    match without_trackers(&row.torrent) {
                        Ok(d) => d,
                        // Fail closed: the operator asked for the passkeys to
                        // stay home, and a file we cannot rewrite may carry one.
                        Err(_) => {
                            missing.push((hash.to_string(), "unreadable .torrent, trackers could not be removed"));
                            return Ok(());
                        }
                    }
                } else {
                    row.torrent.clone()
                };
                zip.add(&entry_name(&name, hash), &data)?;
                summary.written += 1;
                Ok(())
            })?;
            if !missing.is_empty() {
                let mut body = String::new();
                for (h, why) in &missing {
                    body.push_str(h);
                    body.push('\t');
                    body.push_str(why);
                    body.push('\n');
                }
                zip.add("missing.txt", body.as_bytes())?;
            }
            summary.missing = missing.len();
            let mut out = zip.finish()?;
            out.flush()?;
            Ok(summary)
        }
    }
}

/// Call `f` for every hash in order, with its row when the store has one.
///
/// The read guard lives for one batch query and is dropped before `f` runs:
/// `f` writes to a client that may be slow, and a guard held across that
/// would make the export's pace everyone else's.
fn each_row(
    store: &StoreLock,
    hashes: &[String],
    mut f: impl FnMut(&str, Option<&ExportRow>) -> io::Result<()>,
) -> io::Result<()> {
    for chunk in hashes.chunks(BATCH) {
        let rows = {
            let guard = store.read().unwrap_or_else(|p| p.into_inner());
            guard.export_rows(chunk).map_err(|e| io::Error::new(io::ErrorKind::Other, e.to_string()))?
        };
        let by_hash: std::collections::HashMap<&str, &ExportRow> =
            rows.iter().map(|r| (r.info_hash.as_str(), r)).collect();
        for h in chunk {
            f(h, by_hash.get(h.as_str()).copied())?;
        }
    }
    Ok(())
}

/// A file name for a torrent inside the archive: its name, made safe on every
/// OS, and the start of its hash so two torrents with one name both survive.
pub fn entry_name(name: &str, hash: &str) -> String {
    let cleaned: String = name
        .chars()
        .map(|c| if c.is_control() || "/\\:*?\"<>|".contains(c) { '_' } else { c })
        .collect();
    let mut s = cleaned.trim().trim_start_matches('.').to_string();
    // 150 bytes leaves room for the suffix under the 255 most filesystems allow.
    if s.len() > 150 {
        let mut cut = 150;
        while !s.is_char_boundary(cut) {
            cut -= 1;
        }
        s.truncate(cut);
    }
    let s = s.trim_end_matches(['.', ' ']);
    let short = &hash[..hash.len().min(8)];
    if s.is_empty() {
        format!("{hash}.torrent")
    } else {
        format!("{s} [{short}].torrent")
    }
}

/// The same `.torrent` without `announce` and `announce-list`.
///
/// Private trackers put the account's passkey in the announce URL, so a file
/// shared with its trackers is an account shared with it. Both keys live
/// outside the info dict, so the info hash -- and the torrent's identity in
/// every swarm -- is unchanged; the info dict is copied byte for byte.
pub fn without_trackers(data: &[u8]) -> Result<Vec<u8>, String> {
    if data.first() != Some(&b'd') {
        return Err("not a bencoded dictionary".into());
    }
    let mut out = Vec::with_capacity(data.len());
    out.push(b'd');
    let mut pos = 1;
    loop {
        match data.get(pos) {
            None => return Err("unterminated dictionary".into()),
            Some(b'e') => break,
            Some(b'0'..=b'9') => {}
            Some(_) => return Err(format!("dictionary key is not a string at {pos}")),
        }
        let key_end = skip(data, pos)?;
        let key = string_body(&data[pos..key_end]);
        let value_end = skip(data, key_end)?;
        if key != b"announce" && key != b"announce-list" {
            out.extend_from_slice(&data[pos..value_end]);
        }
        pos = value_end;
    }
    out.push(b'e');
    Ok(out)
}

/// The bytes of a bencoded string, `<len>:<bytes>` -> `<bytes>`.
fn string_body(s: &[u8]) -> &[u8] {
    match s.iter().position(|&b| b == b':') {
        Some(i) => &s[i + 1..],
        None => &[],
    }
}

/// The offset just past the bencoded value that starts at `pos`.
fn skip(data: &[u8], pos: usize) -> Result<usize, String> {
    match data.get(pos) {
        None => Err("unexpected end of data".into()),
        Some(b'i') => {
            let end = data[pos + 1..].iter().position(|&b| b == b'e').ok_or("unterminated int")?;
            Ok(pos + 1 + end + 1)
        }
        Some(b'l') | Some(b'd') => {
            let mut p = pos + 1;
            loop {
                match data.get(p) {
                    None => return Err("unterminated container".into()),
                    Some(b'e') => return Ok(p + 1),
                    Some(_) => p = skip(data, p)?,
                }
            }
        }
        Some(b'0'..=b'9') => {
            let colon = data[pos..].iter().position(|&b| b == b':').ok_or("string without ':'")? + pos;
            let len: usize = std::str::from_utf8(&data[pos..colon])
                .ok()
                .and_then(|s| s.parse().ok())
                .ok_or("invalid string length")?;
            let end = colon.checked_add(1 + len).ok_or("string length overflows")?;
            if end > data.len() {
                return Err("string runs past the end".into());
            }
            Ok(end)
        }
        Some(&b) => Err(format!("unexpected byte {b:#04x} at {pos}")),
    }
}

/// One CSV line for a row, newline included.
///
/// Trackers are listed by HOST only: the full URL of a private tracker holds
/// the passkey, and a spreadsheet is exactly the kind of file that gets passed
/// around.
fn csv_line(row: &ExportRow) -> String {
    let meta = typhon_engine::torrent::metainfo::parse_torrent_bytes(&row.torrent).ok();
    let (name, size, hosts) = match &meta {
        Some(m) => {
            let mut hosts: Vec<String> = Vec::new();
            for url in m.trackers.iter().flatten() {
                let h = tracker_host(url);
                if !h.is_empty() && !hosts.contains(&h) {
                    hosts.push(h);
                }
            }
            (m.name.clone(), m.total_size.to_string(), hosts.join(" "))
        }
        None => (String::new(), String::new(), String::new()),
    };
    let fields = [
        row.info_hash.clone(),
        name,
        size,
        row.category.clone(),
        row.tags.join(","),
        hosts,
        (row.added_time as i64).to_string(),
        row.save_path.clone(),
    ];
    let mut line = fields.iter().map(|f| csv_field(f)).collect::<Vec<_>>().join(",");
    line.push('\n');
    line
}

/// The host of a tracker URL, without scheme, port, path or credentials.
fn tracker_host(url: &str) -> String {
    let rest = url.split_once("://").map(|(_, r)| r).unwrap_or(url);
    let authority = rest.split(['/', '?', '#']).next().unwrap_or("");
    let host = authority.rsplit('@').next().unwrap_or("");
    // An IPv6 literal keeps its brackets; otherwise the port goes.
    if host.starts_with('[') {
        return host.split(']').next().map(|h| format!("{h}]")).unwrap_or_default();
    }
    host.split(':').next().unwrap_or("").to_ascii_lowercase()
}

fn csv_field(s: &str) -> String {
    if s.contains([',', '"', '\n', '\r']) {
        format!("\"{}\"", s.replace('"', "\"\""))
    } else {
        s.to_string()
    }
}

// ---------------------------------------------------------------------------
// Streaming zip
// ---------------------------------------------------------------------------

const U32_FULL: u64 = 0xFFFF_FFFF;

/// A zip written front to back, never seeking.
///
/// Every entry is STORED: a `.torrent` is mostly SHA-1 digests, which do not
/// compress, and deflating tens of gigabytes to save a few percent would make
/// the export CPU-bound for nothing. Because each entry's bytes are in hand
/// before its header is written, the CRC and sizes go in the local header and
/// no data descriptor is needed -- every unzipper reads that.
pub struct ZipStream<W: Write> {
    out: W,
    /// Bytes written to `out` so far: the next entry's offset.
    at: u64,
    entries: u64,
    cd: io::BufWriter<Spill>,
    cd_len: u64,
    /// Where 32-bit fields give up and ZIP64 takes over. Always `U32_FULL`
    /// outside tests; lowered there so the ZIP64 path runs without writing
    /// four gigabytes.
    big: u64,
    time: u16,
    date: u16,
}

impl<W: Write> ZipStream<W> {
    pub fn new(out: W) -> io::Result<Self> {
        let (time, date) = dos_now();
        Ok(ZipStream {
            out,
            at: 0,
            entries: 0,
            cd: io::BufWriter::new(Spill::new()?),
            cd_len: 0,
            big: U32_FULL,
            time,
            date,
        })
    }

    pub fn add(&mut self, name: &str, data: &[u8]) -> io::Result<()> {
        let size = u32::try_from(data.len())
            .ok()
            .filter(|&s| (s as u64) < U32_FULL)
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "entry larger than 4 GiB"))?;
        let name_b = name.as_bytes();
        let name_len = u16::try_from(name_b.len())
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "entry name too long"))?;
        let crc = crc32fast::hash(data);
        let offset = self.at;

        let mut h = Vec::with_capacity(30 + name_b.len());
        put32(&mut h, 0x0403_4b50);
        put16(&mut h, 20); // version needed
        put16(&mut h, 0x0800); // names are UTF-8
        put16(&mut h, 0); // stored
        put16(&mut h, self.time);
        put16(&mut h, self.date);
        put32(&mut h, crc);
        put32(&mut h, size);
        put32(&mut h, size);
        put16(&mut h, name_len);
        put16(&mut h, 0); // no extra field
        h.extend_from_slice(name_b);
        self.out.write_all(&h)?;
        self.out.write_all(data)?;
        self.at += h.len() as u64 + data.len() as u64;

        // Only the offset can outgrow 32 bits: a single .torrent never will.
        let zip64 = offset >= self.big;
        let mut c = Vec::with_capacity(46 + name_b.len() + 12);
        put32(&mut c, 0x0201_4b50);
        put16(&mut c, (3 << 8) | 45); // made by: unix, 4.5
        put16(&mut c, if zip64 { 45 } else { 20 });
        put16(&mut c, 0x0800);
        put16(&mut c, 0);
        put16(&mut c, self.time);
        put16(&mut c, self.date);
        put32(&mut c, crc);
        put32(&mut c, size);
        put32(&mut c, size);
        put16(&mut c, name_len);
        put16(&mut c, if zip64 { 12 } else { 0 });
        put16(&mut c, 0); // comment
        put16(&mut c, 0); // disk
        put16(&mut c, 0); // internal attributes
        put32(&mut c, 0o100644 << 16); // a regular file, rw-r--r--
        put32(&mut c, if zip64 { U32_FULL as u32 } else { offset as u32 });
        c.extend_from_slice(name_b);
        if zip64 {
            put16(&mut c, 0x0001);
            put16(&mut c, 8);
            put64(&mut c, offset);
        }
        self.cd.write_all(&c)?;
        self.cd_len += c.len() as u64;
        self.entries += 1;
        Ok(())
    }

    pub fn finish(mut self) -> io::Result<W> {
        let cd_start = self.at;
        let mut spill = self.cd.into_inner().map_err(|e| e.into_error())?;
        spill.file.seek(io::SeekFrom::Start(0))?;
        let copied = io::copy(&mut (&mut spill.file).take(self.cd_len), &mut self.out)?;
        if copied != self.cd_len {
            return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "central directory spill came back short"));
        }
        self.at += self.cd_len;

        let many = self.entries >= 0xFFFF;
        let far = cd_start >= self.big;
        let large = self.cd_len >= self.big;
        let mut e = Vec::with_capacity(22 + 56 + 20);
        if many || far || large {
            let eocd64_at = self.at;
            put32(&mut e, 0x0606_4b50);
            put64(&mut e, 44); // size of the rest of this record
            put16(&mut e, (3 << 8) | 45);
            put16(&mut e, 45);
            put32(&mut e, 0);
            put32(&mut e, 0);
            put64(&mut e, self.entries);
            put64(&mut e, self.entries);
            put64(&mut e, self.cd_len);
            put64(&mut e, cd_start);
            put32(&mut e, 0x0706_4b50);
            put32(&mut e, 0);
            put64(&mut e, eocd64_at);
            put32(&mut e, 1);
        }
        put32(&mut e, 0x0605_4b50);
        put16(&mut e, 0);
        put16(&mut e, 0);
        let n16 = if many { 0xFFFF } else { self.entries as u16 };
        put16(&mut e, n16);
        put16(&mut e, n16);
        put32(&mut e, if large { U32_FULL as u32 } else { self.cd_len as u32 });
        put32(&mut e, if far { U32_FULL as u32 } else { cd_start as u32 });
        put16(&mut e, 0);
        self.out.write_all(&e)?;
        self.at += e.len() as u64;
        Ok(self.out)
    }
}

fn put16(v: &mut Vec<u8>, x: u16) {
    v.extend_from_slice(&x.to_le_bytes());
}
fn put32(v: &mut Vec<u8>, x: u32) {
    v.extend_from_slice(&x.to_le_bytes());
}
fn put64(v: &mut Vec<u8>, x: u64) {
    v.extend_from_slice(&x.to_le_bytes());
}

/// Now, in the MS-DOS form zip headers carry (UTC: a zip has no time zone).
fn dos_now() -> (u16, u16) {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    dos_time(secs)
}

fn dos_time(unix: i64) -> (u16, u16) {
    let days = unix.div_euclid(86_400);
    let rem = unix.rem_euclid(86_400);
    // Civil date from days since 1970-01-01 (Howard Hinnant's algorithm).
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + if m <= 2 { 1 } else { 0 };
    // DOS dates start in 1980 and end in 2107.
    let y = y.clamp(1980, 2107);
    let time = ((rem / 3600) << 11 | (rem % 3600 / 60) << 5 | (rem % 60) / 2) as u16;
    let date = ((y - 1980) << 9 | m << 5 | d) as u16;
    (time, date)
}

/// A temporary file that removes itself.
///
/// Removed on drop rather than unlinked at once: Windows cannot delete a file
/// that is still open, and this has to run there too.
struct Spill {
    path: std::path::PathBuf,
    file: std::fs::File,
}

impl Spill {
    fn new() -> io::Result<Self> {
        let path = std::env::temp_dir().join(format!(
            "hydranos-export-{}-{:016x}.cd",
            std::process::id(),
            rand::random::<u64>()
        ));
        let file = std::fs::OpenOptions::new().read(true).write(true).create_new(true).open(&path)?;
        Ok(Spill { path, file })
    }
}

impl Write for Spill {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.file.write(buf)
    }
    fn flush(&mut self) -> io::Result<()> {
        self.file.flush()
    }
}

impl Drop for Spill {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

// ---------------------------------------------------------------------------
// Into an HTTP body
// ---------------------------------------------------------------------------

pub type Chunk = Result<bytes::Bytes, io::Error>;

/// A `Write` that hands its bytes to an async response body in 64 KiB chunks.
///
/// The channel is bounded, so a slow client slows the export down instead of
/// the export piling the archive up in memory; and a client that goes away
/// turns the next write into an error, which ends the export.
pub struct ChanWriter {
    tx: tokio::sync::mpsc::Sender<Chunk>,
    buf: Vec<u8>,
}

const CHUNK: usize = 64 << 10;

impl ChanWriter {
    pub fn new(tx: tokio::sync::mpsc::Sender<Chunk>) -> Self {
        ChanWriter { tx, buf: Vec::with_capacity(CHUNK) }
    }

    fn send(&mut self) -> io::Result<()> {
        if self.buf.is_empty() {
            return Ok(());
        }
        let chunk = bytes::Bytes::from(std::mem::replace(&mut self.buf, Vec::with_capacity(CHUNK)));
        self.tx
            .blocking_send(Ok(chunk))
            .map_err(|_| io::Error::new(io::ErrorKind::BrokenPipe, "client went away"))
    }
}

impl Write for ChanWriter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.buf.extend_from_slice(buf);
        if self.buf.len() >= CHUNK {
            self.send()?;
        }
        Ok(buf.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        self.send()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::Store;

    /// A minimal valid single-file torrent, with trackers.
    fn torrent(name: &str, announce: &str) -> Vec<u8> {
        let info = format!("d6:lengthi5e4:name{}:{}12:piece lengthi16384e6:pieces20:AAAAAAAAAAAAAAAAAAAA7:privatei1ee", name.len(), name);
        format!(
            "d8:announce{}:{}13:announce-listll{}:{}ee7:comment2:hi4:info{}e",
            announce.len(),
            announce,
            announce.len(),
            announce,
            info
        )
        .into_bytes()
    }

    fn hash_of(t: &[u8]) -> String {
        let meta = typhon_engine::torrent::metainfo::parse_torrent_bytes(t).unwrap();
        typhon_engine::torrent::hex_encode(&meta.info_hash)
    }

    fn store_with(ts: &[Vec<u8>]) -> (StoreLock, Vec<String>) {
        let store = Store::open_in_memory().unwrap();
        let mut hashes = Vec::new();
        for t in ts {
            let h = hash_of(t);
            store.insert_torrent(&h, "hoard", t, "/data/x", "movies", 1_700_000_000.0, false, "a,b").unwrap();
            hashes.push(h);
        }
        (StoreLock::new(store), hashes)
    }

    fn unzip(bytes: Vec<u8>) -> zip::ZipArchive<io::Cursor<Vec<u8>>> {
        zip::ZipArchive::new(io::Cursor::new(bytes)).expect("a zip any reader can open")
    }

    fn read_entry(z: &mut zip::ZipArchive<io::Cursor<Vec<u8>>>, name: &str) -> Vec<u8> {
        let mut f = z.by_name(name).expect(name);
        let mut v = Vec::new();
        f.read_to_end(&mut v).unwrap();
        v
    }

    #[test]
    fn removing_trackers_keeps_the_info_hash_and_drops_the_passkey() {
        let t = torrent("film.mkv", "https://tracker.example/SECRETPASSKEY/announce");
        let stripped = without_trackers(&t).unwrap();
        assert!(!stripped.windows(13).any(|w| w == b"SECRETPASSKEY"));
        assert_eq!(hash_of(&stripped), hash_of(&t));
        let meta = typhon_engine::torrent::metainfo::parse_torrent_bytes(&stripped).unwrap();
        assert!(meta.trackers.is_empty());
        // Everything else is left alone.
        assert!(stripped.windows(10).any(|w| w == b"7:comment2"));
    }

    #[test]
    fn removing_trackers_refuses_what_it_cannot_parse() {
        assert!(without_trackers(b"not bencode").is_err());
        assert!(without_trackers(b"d8:announce").is_err());
        assert!(without_trackers(b"d8:announce999:x").is_err());
        assert!(without_trackers(b"di1e1:xe").is_err());
    }

    #[test]
    fn entry_names_are_safe_everywhere_and_never_collide_by_name() {
        let h1 = "0123456789abcdef0123456789abcdef01234567";
        assert_eq!(entry_name("a/b:c*?.mkv", h1), "a_b_c__.mkv [01234567].torrent");
        assert_eq!(entry_name("..", h1), format!("{h1}.torrent"));
        assert_eq!(entry_name("", h1), format!("{h1}.torrent"));
        assert_eq!(entry_name("  x. ", h1), "x [01234567].torrent");
        let long = "é".repeat(200);
        let n = entry_name(&long, h1);
        assert!(n.len() <= 150 + " [01234567].torrent".len());
        assert!(n.ends_with(" [01234567].torrent"));
    }

    #[test]
    fn hashes_are_cleaned_deduplicated_and_kept_in_order() {
        let a = "A".repeat(40);
        let b = "b".repeat(64);
        let raw = vec![a.clone(), "xyz".into(), b.clone(), a.to_lowercase(), "g".repeat(40)];
        assert_eq!(clean_hashes(&raw), vec![a.to_lowercase(), b]);
    }

    #[test]
    fn a_zip_export_opens_and_holds_the_torrents_byte_for_byte() {
        let t1 = torrent("one.mkv", "https://a.example/announce");
        let t2 = torrent("two.mkv", "https://b.example/announce");
        let (store, hashes) = store_with(&[t1.clone(), t2.clone()]);
        let absent = "f".repeat(40);
        let asked = vec![hashes[0].clone(), absent.clone(), hashes[1].clone()];
        let mut out = Vec::new();
        let s = run(&store, &asked, Format::Zip, false, &mut out).unwrap();
        assert_eq!(s, Summary { written: 2, missing: 1 });

        let mut z = unzip(out);
        assert_eq!(z.len(), 3);
        let n1 = entry_name("one.mkv", &hashes[0]);
        assert_eq!(read_entry(&mut z, &n1), t1);
        assert_eq!(read_entry(&mut z, &entry_name("two.mkv", &hashes[1])), t2);
        let missing = String::from_utf8(read_entry(&mut z, "missing.txt")).unwrap();
        assert_eq!(missing, format!("{absent}\tnot in this node's library\n"));
    }

    #[test]
    fn a_zip_export_can_leave_the_passkeys_home() {
        let t = torrent("one.mkv", "https://a.example/PASSKEY123/announce");
        let (store, hashes) = store_with(&[t.clone()]);
        let mut out = Vec::new();
        run(&store, &hashes, Format::Zip, true, &mut out).unwrap();
        let mut z = unzip(out);
        let got = read_entry(&mut z, &entry_name("one.mkv", &hashes[0]));
        assert!(!got.windows(10).any(|w| w == b"PASSKEY123"));
        assert_eq!(hash_of(&got), hashes[0]);
    }

    #[test]
    fn past_four_gigabytes_the_zip_switches_to_zip64_and_still_opens() {
        // The threshold is lowered so the ZIP64 records are written for real:
        // offsets past it go in the extra field, the end record points at the
        // ZIP64 one.
        let mut out = Vec::new();
        let mut zip = ZipStream::new(&mut out).unwrap();
        zip.big = 100;
        for i in 0..20 {
            zip.add(&format!("f{i}.torrent"), format!("payload {i}").as_bytes()).unwrap();
        }
        zip.finish().unwrap();
        let mut z = unzip(out);
        assert_eq!(z.len(), 20);
        for i in [0, 7, 19] {
            assert_eq!(read_entry(&mut z, &format!("f{i}.torrent")), format!("payload {i}").into_bytes());
        }
    }

    #[test]
    fn more_than_65535_entries_still_opens() {
        let mut out = Vec::new();
        let mut zip = ZipStream::new(&mut out).unwrap();
        for i in 0..70_000u32 {
            zip.add(&format!("{i}"), &i.to_le_bytes()).unwrap();
        }
        zip.finish().unwrap();
        let mut z = unzip(out);
        assert_eq!(z.len(), 70_000);
        assert_eq!(read_entry(&mut z, "69999"), 69_999u32.to_le_bytes());
    }

    #[test]
    fn the_central_directory_spill_is_removed() {
        let mut out = Vec::new();
        let zip = ZipStream::new(&mut out).unwrap();
        let path = zip.cd.get_ref().path.clone();
        assert!(path.exists());
        zip.finish().unwrap();
        assert!(!path.exists());
    }

    #[test]
    fn a_csv_export_names_tracker_hosts_never_their_urls() {
        let t = torrent("a, \"quoted\" name", "https://user@Tracker.Example:8443/PASSKEY/announce");
        let (store, hashes) = store_with(&[t]);
        let absent = "e".repeat(40);
        let mut out = Vec::new();
        let s = run(&store, &[hashes[0].clone(), absent.clone()], Format::Csv, false, &mut out).unwrap();
        assert_eq!(s, Summary { written: 1, missing: 1 });
        let text = String::from_utf8(out).unwrap();
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines[0], "info_hash,name,size,category,tags,trackers,added_unix,save_path");
        assert_eq!(
            lines[1],
            format!("{},\"a, \"\"quoted\"\" name\",5,movies,\"a,b\",tracker.example,1700000000,/data/x", hashes[0])
        );
        assert_eq!(lines[2], format!("{absent},,,,,,,"));
        assert!(!text.contains("PASSKEY"));
    }

    #[test]
    fn a_hash_list_is_the_selection_even_off_this_node() {
        let (store, _) = store_with(&[]);
        let asked = vec!["a".repeat(40), "b".repeat(40)];
        let mut out = Vec::new();
        run(&store, &asked, Format::Txt, false, &mut out).unwrap();
        assert_eq!(String::from_utf8(out).unwrap(), format!("{}\n{}\n", asked[0], asked[1]));
    }

    #[test]
    fn dos_time_matches_a_known_date() {
        // 2026-09-29 17:45:30 UTC
        let (time, date) = dos_time(1_790_703_930);
        assert_eq!(date, ((2026 - 1980) << 9 | 9 << 5 | 29) as u16);
        assert_eq!(time, (17 << 11 | 45 << 5 | 15) as u16);
    }
}
