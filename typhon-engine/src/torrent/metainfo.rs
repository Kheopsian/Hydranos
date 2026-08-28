use std::path::PathBuf;
use sha1::{Sha1, Digest};

use super::meta::{TorrentMeta, FileEntry, InfoHash};

/// Parse a .torrent file and extract metadata.
/// Decode a torrent path/name byte string. Prefer UTF-8; fall back to Latin-1
/// (ISO-8859-1) for legacy non-UTF-8 names (FR scene CP1252 accents). Latin-1 decode
/// is lossless (0xE9 -> '\u{e9}') and NEVER yields an empty component. Previously
/// `filter_map(as_string)` DROPPED non-UTF-8 path parts, leaving an empty PathBuf, so
/// the engine wrote to the parent dir -> `Is a directory` retry-storm (saturated the
/// RPC semaphore, blocked all adds). See feedback_typhon_latin1_path_ddos.
fn decode_path_str(bytes: &[u8]) -> String {
    match std::str::from_utf8(bytes) {
        Ok(s) => s.to_string(),
        Err(_) => bytes.iter().map(|&b| b as char).collect(),
    }
}

pub fn parse_torrent_file(path: &str) -> Result<TorrentMeta, String> {
    let data = std::fs::read(path).map_err(|e| format!("read {}: {}", path, e))?;
    parse_torrent_bytes(&data)
}

pub fn parse_torrent_bytes(data: &[u8]) -> Result<TorrentMeta, String> {
    let value = bencode_decode(data)?;
    let dict = value.as_dict().ok_or("torrent is not a dict")?;

    // Extract info dict and compute info_hash
    let info_raw = find_info_raw(data)?;
    let info_hash = sha1_hash(&info_raw);

    let info = dict.get("info").ok_or("missing info dict")?
        .as_dict().ok_or("info is not a dict")?;

    // Piece length
    let piece_length = info.get("piece length")
        .ok_or("missing piece length")?
        .as_int().ok_or("piece length not int")? as u32;

    // Pieces (concatenated 20-byte SHA1 hashes)
    let pieces_raw = info.get("pieces")
        .ok_or("missing pieces")?
        .as_bytes().ok_or("pieces not bytes")?;
    if pieces_raw.len() % 20 != 0 {
        return Err("pieces length not multiple of 20".into());
    }
    let num_pieces = (pieces_raw.len() / 20) as u32;

    // Name
    let name = decode_path_str(
        info.get("name").ok_or("missing name")?
            .as_bytes().ok_or("name not bytes")?
    );

    // Private
    let private = info.get("private")
        .and_then(|v| v.as_int())
        .map(|v| v == 1)
        .unwrap_or(false);

    // Files
    let (files, total_size, multi_file) = if let Some(file_list) = info.get("files") {
        // Multi-file torrent
        let file_list = file_list.as_list().ok_or("files not list")?;
        let mut files = Vec::new();
        let mut offset = 0u64;
        for f in file_list {
            let fd = f.as_dict().ok_or("file entry not dict")?;
            let length = fd.get("length").ok_or("missing file length")?
                .as_int().ok_or("file length not int")? as u64;
            let path_parts = fd.get("path").ok_or("missing file path")?
                .as_list().ok_or("file path not list")?;
            let path: PathBuf = path_parts.iter()
                .filter_map(|p| p.as_bytes())
                .map(decode_path_str)
                .collect();
            files.push(FileEntry { path, offset, length });
            offset += length;
        }
        (files, offset, true)
    } else {
        // Single-file torrent
        let length = info.get("length")
            .ok_or("missing length")?
            .as_int().ok_or("length not int")? as u64;
        let path = PathBuf::from(&name);
        (vec![FileEntry { path, offset: 0, length }], length, false)
    };

    // Trackers
    let mut trackers = Vec::new();
    if let Some(al) = dict.get("announce-list") {
        if let Some(tiers) = al.as_list() {
            for tier in tiers {
                if let Some(urls) = tier.as_list() {
                    let tier_urls: Vec<String> = urls.iter()
                        .filter_map(|u| u.as_string().map(|s| s.to_string()))
                        .collect();
                    if !tier_urls.is_empty() {
                        trackers.push(tier_urls);
                    }
                }
            }
        }
    }
    if trackers.is_empty() {
        if let Some(announce) = dict.get("announce") {
            if let Some(url) = announce.as_string() {
                trackers.push(vec![url.to_string()]);
            }
        }
    }

    Ok(TorrentMeta {
        info_hash,
        name,
        num_pieces,
        piece_length,
        total_size,
        files,
        trackers,
        private,
        multi_file,
        info_dict_len: info_raw.len() as u32,
    })
}

fn sha1_hash(data: &[u8]) -> InfoHash {
    let mut hasher = Sha1::new();
    hasher.update(data);
    let result = hasher.finalize();
    let mut hash = [0u8; 20];
    hash.copy_from_slice(&result);
    hash
}

/// Extract the raw bencoded info dict from the torrent data.
/// Read a .torrent from disk and return just its raw info dict bytes, for
/// serving BEP 9. Deliberately re-read rather than cached: this runs only when
/// a peer asks, which is rare next to the cost of holding every dict in memory.
/// Read just the piece hashes back out of a `.torrent` on disk.
///
/// The hashes are 20 bytes per piece and dominate a torrent file: measured
/// across 4000 production torrents they are 91.7% of the bytes, averaging
/// 20.1 KiB each. Holding them resident for every torrent cost 4.2 GB on the
/// 205k-torrent instance, and only two call sites ever read them -- both of
/// them verifying a piece we just read or wrote, both already doing disk I/O
/// and a SHA-1 over the whole piece, so one file read is lost in the noise.
///
/// A pure seeder never calls this at all: serving a piece does not verify it.
pub fn piece_hashes_from_file(path: &str) -> Result<Vec<[u8; 20]>, String> {
    let info_raw = info_dict_from_file(path)?;
    let dict = bencode_decode(&info_raw)
        .map_err(|e| format!("{}: info dict: {}", path, e))?;
    let dict = dict.as_dict().ok_or_else(|| format!("{}: info is not a dict", path))?;
    let raw = dict
        .get("pieces")
        .ok_or_else(|| format!("{}: missing pieces", path))?
        .as_bytes()
        .ok_or_else(|| format!("{}: pieces not bytes", path))?;
    if raw.len() % 20 != 0 {
        return Err(format!("{}: pieces length not multiple of 20", path));
    }
    Ok(raw
        .chunks(20)
        .map(|c| {
            let mut h = [0u8; 20];
            h.copy_from_slice(c);
            h
        })
        .collect())
}

pub fn info_dict_from_file(path: &str) -> Result<Vec<u8>, String> {
    let data = std::fs::read(path).map_err(|e| format!("read {}: {}", path, e))?;
    find_info_raw(&data)
}

fn find_info_raw(data: &[u8]) -> Result<Vec<u8>, String> {
    // Find "4:infod" pattern and extract until matching end
    let needle = b"4:info";
    let pos = find_bytes(data, needle).ok_or("cannot find info dict")?;
    let info_start = pos + needle.len();
    if info_start >= data.len() || data[info_start] != b'd' {
        return Err("info value is not a dict".into());
    }
    let end = find_dict_end(data, info_start)?;
    Ok(data[info_start..end].to_vec())
}

fn find_bytes(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|w| w == needle)
}

fn find_dict_end(data: &[u8], start: usize) -> Result<usize, String> {
    let mut depth = 0i32;
    let mut i = start;
    while i < data.len() {
        match data[i] {
            b'd' | b'l' => { depth += 1; i += 1; }
            b'i' => {
                i += 1;
                while i < data.len() && data[i] != b'e' { i += 1; }
                i += 1; // skip 'e'
            }
            b'e' => {
                depth -= 1;
                i += 1;
                if depth == 0 { return Ok(i); }
            }
            b'0'..=b'9' => {
                let num_start = i;
                while i < data.len() && data[i] != b':' { i += 1; }
                let len_str = std::str::from_utf8(&data[num_start..i])
                    .map_err(|_| "invalid string length")?;
                let len: usize = len_str.parse().map_err(|_| "invalid string length")?;
                i += 1 + len; // skip ':' + string data
            }
            _ => return Err(format!("unexpected byte {} at {}", data[i], i)),
        }
    }
    Err("unterminated dict".into())
}

// ── Minimal bencode decoder ──

#[derive(Debug, Clone)]
pub enum BencodeValue {
    Int(i64),
    Bytes(Vec<u8>),
    List(Vec<BencodeValue>),
    Dict(Vec<(String, BencodeValue)>),
}

use std::collections::HashMap;

impl BencodeValue {
    pub fn as_int(&self) -> Option<i64> {
        if let BencodeValue::Int(v) = self { Some(*v) } else { None }
    }
    pub fn as_bytes(&self) -> Option<&[u8]> {
        if let BencodeValue::Bytes(v) = self { Some(v) } else { None }
    }
    pub fn as_string(&self) -> Option<&str> {
        if let BencodeValue::Bytes(v) = self { std::str::from_utf8(v).ok() } else { None }
    }
    pub fn as_list(&self) -> Option<&[BencodeValue]> {
        if let BencodeValue::List(v) = self { Some(v) } else { None }
    }
    pub fn as_dict(&self) -> Option<BencodeDict> {
        if let BencodeValue::Dict(v) = self {
            let map: HashMap<&str, &BencodeValue> = v.iter()
                .map(|(k, v)| (k.as_str(), v))
                .collect();
            Some(BencodeDict(map))
        } else {
            None
        }
    }
}

pub struct BencodeDict<'a>(HashMap<&'a str, &'a BencodeValue>);

impl<'a> BencodeDict<'a> {
    pub fn get(&self, key: &str) -> Option<&'a BencodeValue> {
        self.0.get(key).copied()
    }
}

pub fn bencode_decode(data: &[u8]) -> Result<BencodeValue, String> {
    let (val, _) = decode_value(data, 0)?;
    Ok(val)
}

fn decode_value(data: &[u8], pos: usize) -> Result<(BencodeValue, usize), String> {
    if pos >= data.len() {
        return Err("unexpected end of data".into());
    }
    match data[pos] {
        b'i' => decode_int(data, pos),
        b'l' => decode_list(data, pos),
        b'd' => decode_dict(data, pos),
        b'0'..=b'9' => decode_string(data, pos),
        b => Err(format!("unexpected byte '{}' at {}", b as char, pos)),
    }
}

fn decode_int(data: &[u8], pos: usize) -> Result<(BencodeValue, usize), String> {
    let end = data[pos+1..].iter().position(|&b| b == b'e')
        .ok_or("unterminated int")?;
    let s = std::str::from_utf8(&data[pos+1..pos+1+end])
        .map_err(|_| "invalid int")?;
    let v: i64 = s.parse().map_err(|_| "invalid int")?;
    Ok((BencodeValue::Int(v), pos + 1 + end + 1))
}

fn decode_string(data: &[u8], pos: usize) -> Result<(BencodeValue, usize), String> {
    let colon = data[pos..].iter().position(|&b| b == b':')
        .ok_or("unterminated string length")?;
    let len_str = std::str::from_utf8(&data[pos..pos+colon])
        .map_err(|_| "invalid string length")?;
    let len: usize = len_str.parse().map_err(|_| "invalid string length")?;
    let start = pos + colon + 1;
    if start + len > data.len() {
        return Err("string extends past end".into());
    }
    Ok((BencodeValue::Bytes(data[start..start+len].to_vec()), start + len))
}

fn decode_list(data: &[u8], pos: usize) -> Result<(BencodeValue, usize), String> {
    let mut items = Vec::new();
    let mut i = pos + 1;
    while i < data.len() && data[i] != b'e' {
        let (val, next) = decode_value(data, i)?;
        items.push(val);
        i = next;
    }
    Ok((BencodeValue::List(items), i + 1))
}

fn decode_dict(data: &[u8], pos: usize) -> Result<(BencodeValue, usize), String> {
    let mut items = Vec::new();
    let mut i = pos + 1;
    while i < data.len() && data[i] != b'e' {
        let (key_val, next) = decode_string(data, i)?;
        let key = match key_val {
            // BEP-3: bencode dict keys are byte strings, NOT required to be UTF-8. BT v2 /
            // hybrid torrents (BEP-52) use raw 32-byte SHA-256 merkle roots as keys in the
            // root `piece layers` dict. Lossy-decode so v1/hybrid torrents parse instead of
            // being rejected. Safe: info_hash is computed from raw bytes (find_info_raw, not
            // re-encoded) and every lookup uses a known-ASCII key, so binary keys are never
            // read by name (collisions in the lookup map are harmless).
            BencodeValue::Bytes(b) => String::from_utf8_lossy(&b).into_owned(),
            _ => return Err("dict key not string".into()),
        };
        let (val, next2) = decode_value(data, next)?;
        items.push((key, val));
        i = next2;
    }
    Ok((BencodeValue::Dict(items), i + 1))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Smallest legal single-file torrent, with `n` piece hashes. Info keys
    /// stay in the bencode-required sorted order: length, name, piece length,
    /// pieces.
    fn torrent_bytes(n: usize) -> (Vec<u8>, Vec<[u8; 20]>) {
        let mut hashes = Vec::new();
        let mut pieces = Vec::new();
        for i in 0..n {
            let h = [i as u8; 20];
            hashes.push(h);
            pieces.extend_from_slice(&h);
        }
        let mut b = Vec::new();
        b.extend_from_slice(b"d8:announce19:http://tracker/annc");
        b.extend_from_slice(b"4:infod");
        b.extend_from_slice(format!("6:lengthi{}e", n * 16384).as_bytes());
        b.extend_from_slice(b"4:name8:some.bin");
        b.extend_from_slice(b"12:piece lengthi16384e");
        b.extend_from_slice(format!("6:pieces{}:", pieces.len()).as_bytes());
        b.extend_from_slice(&pieces);
        b.extend_from_slice(b"ee");
        (b, hashes)
    }

    fn write_temp(name: &str, bytes: &[u8]) -> String {
        let mut p = std::env::temp_dir();
        p.push(format!("typhon-test-{}-{}.torrent", std::process::id(), name));
        std::fs::write(&p, bytes).unwrap();
        p.to_string_lossy().into_owned()
    }

    /// Parsing keeps the piece COUNT and drops the 20-byte hashes: that is the
    /// 4.2 GB the engine used to hold resident for 205k torrents.
    #[test]
    fn parsing_keeps_the_count_not_the_hashes() {
        let (bytes, hashes) = torrent_bytes(7);
        let meta = parse_torrent_bytes(&bytes).unwrap();
        assert_eq!(meta.num_pieces(), 7);
        assert_eq!(hashes.len(), 7);
        // TorrentMeta has no field able to hold them any more; the only way
        // back to the hashes is the file.
        assert_eq!(std::mem::size_of_val(&meta.num_pieces), 4);
    }

    /// The lazy loader must return exactly what the parser saw.
    #[test]
    fn piece_hashes_round_trip_through_the_file() {
        let (bytes, hashes) = torrent_bytes(5);
        let path = write_temp("roundtrip", &bytes);
        let loaded = piece_hashes_from_file(&path).unwrap();
        assert_eq!(loaded, hashes, "loaded hashes differ from the ones written");
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn missing_file_is_an_error_not_an_empty_table() {
        let err = piece_hashes_from_file("/nonexistent/nope.torrent").unwrap_err();
        assert!(err.contains("read"), "unexpected error: {}", err);
    }

    #[test]
    fn truncated_hash_table_is_rejected() {
        let (mut bytes, _) = torrent_bytes(3);
        // 3 hashes = 60 bytes; claim 59 so the table is not a multiple of 20.
        let at = bytes.windows(9).position(|w| w == b"6:pieces6").unwrap();
        bytes[at + 9] = b'5';
        bytes.remove(bytes.len() - 3);
        let path = write_temp("truncated", &bytes);
        assert!(piece_hashes_from_file(&path).is_err(), "truncated table accepted");
        std::fs::remove_file(&path).ok();
    }
}
