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
    let pieces: Vec<[u8; 20]> = pieces_raw.chunks(20)
        .map(|c| { let mut h = [0u8; 20]; h.copy_from_slice(c); h })
        .collect();

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
        pieces,
        piece_length,
        total_size,
        files,
        trackers,
        private,
        multi_file,
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
