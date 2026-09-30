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

/// A file path from the metainfo, relative to the save path: only plain names.
fn check_contained(rel: &std::path::Path) -> Result<(), String> {
    use std::path::Component;
    if rel.as_os_str().is_empty() {
        return Err("unsafe path in torrent: a file has no name".into());
    }
    for c in rel.components() {
        match c {
            Component::Normal(_) | Component::CurDir => {}
            _ => {
                return Err(format!(
                    "unsafe path in torrent: {:?} would be written outside the download folder",
                    rel.display().to_string()
                ))
            }
        }
    }
    Ok(())
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

    // BEP 52, v2 only: no v1 `pieces`, a `file tree` instead. A hybrid has
    // both and is read below as the v1 torrent it also is -- its v1 hashes
    // cover every byte, alignment padding included.
    let is_v2 = info.get("meta version").and_then(|v| v.as_int()) == Some(2);
    if is_v2 && info.get("pieces").is_none() {
        return parse_v2(dict, &info, &info_raw);
    }

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
            // BEP 47 padding: zeros that align the next file to a piece. Part
            // of the stream -- the offsets and the v1 hashes count them --
            // but never a file on disk. Kept as a gap between two files,
            // which `map_block` reads as zeros and never writes.
            let attr = fd.get("attr").and_then(|a| a.as_bytes()).unwrap_or(&[]);
            let is_pad = attr.contains(&b'p') || path.starts_with(".pad");
            if !is_pad {
                files.push(FileEntry { path, offset, length });
            }
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

    // Every file must stay inside the torrent's folder. The names come from
    // whoever made the .torrent, and joined onto a save path, `..` climbs out
    // of it and an absolute component replaces it outright: a torrent naming
    // `../../etc/cron.d/x` would be downloaded THERE. Refused rather than
    // rewritten -- a legitimate torrent never contains either, and a silently
    // renamed file is one the other seeders' layout no longer matches.
    for f in &files {
        let rel = if multi_file { std::path::Path::new(&name).join(&f.path) } else { f.path.clone() };
        check_contained(&rel)?;
    }

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

    // BEP 19 webseeds. The key is either a single string or a list of
    // them; Internet Archive ships two (the collection URL and the
    // storage node that currently holds the item).
    let mut url_list = Vec::new();
    if let Some(ul) = dict.get("url-list") {
        if let Some(s) = ul.as_string() {
            if !s.is_empty() {
                url_list.push(s.to_string());
            }
        } else if let Some(items) = ul.as_list() {
            for u in items {
                if let Some(s) = u.as_string() {
                    if !s.is_empty() {
                        url_list.push(s.to_string());
                    }
                }
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
        url_list,
        private,
        multi_file,
        info_dict_len: info_raw.len() as u32,
        v2: false,
    })
}

/// Trackers and web seeds: the same keys, v1 or v2.
fn trackers_and_seeds(dict: &BencodeDict) -> (Vec<Vec<String>>, Vec<String>) {
    let mut trackers = Vec::new();
    if let Some(tiers) = dict.get("announce-list").and_then(|v| v.as_list()) {
        for tier in tiers {
            if let Some(urls) = tier.as_list() {
                let t: Vec<String> = urls.iter().filter_map(|u| u.as_string().map(|s| s.to_string())).collect();
                if !t.is_empty() {
                    trackers.push(t);
                }
            }
        }
    }
    if trackers.is_empty() {
        if let Some(url) = dict.get("announce").and_then(|a| a.as_string()) {
            trackers.push(vec![url.to_string()]);
        }
    }
    let mut url_list = Vec::new();
    if let Some(ul) = dict.get("url-list") {
        if let Some(s) = ul.as_string().filter(|s| !s.is_empty()) {
            url_list.push(s.to_string());
        } else if let Some(items) = ul.as_list() {
            url_list.extend(items.iter().filter_map(|u| u.as_string()).filter(|s| !s.is_empty()).map(String::from));
        }
    }
    (trackers, url_list)
}

/// One file of a v2 `file tree`, in tree order.
struct V2File {
    path: PathBuf,
    length: u64,
    root: Option<[u8; 32]>,
}

fn walk_file_tree(node: &BencodeValue, prefix: &std::path::Path, out: &mut Vec<V2File>) -> Result<(), String> {
    let BencodeValue::Dict(entries) = node else {
        return Err("file tree node is not a dict".into());
    };
    for (name, child) in entries {
        let child_dict = child.as_dict().ok_or("file tree entry is not a dict")?;
        let path = prefix.join(name);
        match child_dict.get("") {
            // A file: its properties under the empty key.
            Some(leaf) => {
                let leaf = leaf.as_dict().ok_or("file entry is not a dict")?;
                let length = leaf.get("length").and_then(|l| l.as_int()).ok_or("file without length")?;
                if length < 0 {
                    return Err("negative file length".into());
                }
                let root = match leaf.get("pieces root").and_then(|r| r.as_bytes()) {
                    Some(r) if r.len() == 32 => {
                        let mut h = [0u8; 32];
                        h.copy_from_slice(r);
                        Some(h)
                    }
                    Some(_) => return Err("pieces root is not 32 bytes".into()),
                    None if length > 0 => return Err("a non-empty file has no pieces root".into()),
                    None => None,
                };
                out.push(V2File { path, length: length as u64, root });
            }
            None => walk_file_tree(child, &path, out)?,
        }
    }
    Ok(())
}

fn v2_files(info: &BencodeDict) -> Result<Vec<V2File>, String> {
    let tree = info.get("file tree").ok_or("v2 torrent without a file tree")?;
    let mut files = Vec::new();
    walk_file_tree(tree, std::path::Path::new(""), &mut files)?;
    if files.is_empty() {
        return Err("empty file tree".into());
    }
    Ok(files)
}

/// A v2-only torrent (BEP 52).
///
/// Its identity everywhere a 20-byte hash goes -- handshake, trackers, DHT,
/// the store -- is the SHA-256 of the info dict, truncated to 20 bytes, as
/// BEP 52 specifies. Every file starts on a piece boundary; the gap after a
/// file's last byte is alignment, read as zeros and never stored, exactly as
/// the pad files of a hybrid are.
fn parse_v2(dict: BencodeDict, info: &BencodeDict, info_raw: &[u8]) -> Result<TorrentMeta, String> {
    use sha2::{Digest, Sha256};
    let full: [u8; 32] = Sha256::digest(info_raw).into();
    let mut info_hash = [0u8; 20];
    info_hash.copy_from_slice(&full[..20]);

    let piece_length = info.get("piece length").and_then(|v| v.as_int()).ok_or("missing piece length")?;
    if piece_length < 16384 || (piece_length as u64).count_ones() != 1 || piece_length > u32::MAX as i64 {
        return Err("a v2 piece length is a power of two of at least 16 KiB".into());
    }
    let pl = piece_length as u64;
    let name = decode_path_str(info.get("name").and_then(|v| v.as_bytes()).ok_or("missing name")?);
    let private = info.get("private").and_then(|v| v.as_int()) == Some(1);

    let tree = v2_files(info)?;
    // One file at the top of the tree is a single-file torrent: the file sits
    // at the save path under its own name, as a v1 single-file one does.
    let multi_file = !(tree.len() == 1 && tree[0].path.components().count() == 1);
    let mut files = Vec::with_capacity(tree.len());
    let mut pos = 0u64;
    let mut end = 0u64;
    for f in &tree {
        files.push(FileEntry { path: f.path.clone(), offset: pos, length: f.length });
        if f.length > 0 {
            end = pos + f.length;
            pos = end.div_ceil(pl) * pl;
        }
    }
    for f in &files {
        let rel = if multi_file { std::path::Path::new(&name).join(&f.path) } else { f.path.clone() };
        check_contained(&rel)?;
    }
    let (trackers, url_list) = trackers_and_seeds(&dict);
    Ok(TorrentMeta {
        info_hash,
        name,
        num_pieces: end.div_ceil(pl) as u32,
        piece_length: pl as u32,
        total_size: end,
        files,
        trackers,
        url_list,
        private,
        multi_file,
        info_dict_len: info_raw.len() as u32,
        v2: true,
    })
}

/// Skip one bencoded value, returning where the next one starts.
fn skip_value(data: &[u8], pos: usize) -> Result<usize, String> {
    match data.get(pos) {
        Some(b'd') | Some(b'l') => find_dict_end(data, pos),
        Some(b'i') => {
            let e = data[pos..].iter().position(|&b| b == b'e').ok_or("unterminated int")?;
            Ok(pos + e + 1)
        }
        Some(b'0'..=b'9') => raw_string(data, pos).map(|(_, end)| end),
        _ => Err("bad value".into()),
    }
}

/// A bencoded string at `pos`: its bytes and where it ends.
fn raw_string(data: &[u8], pos: usize) -> Result<(&[u8], usize), String> {
    let c = data[pos..].iter().position(|&b| b == b':').ok_or("bad string")?;
    let len: usize = std::str::from_utf8(&data[pos..pos + c]).map_err(|_| "bad string")?.parse().map_err(|_| "bad string")?;
    let start = pos + c + 1;
    let bytes = data.get(start..start + len).ok_or("string past the end")?;
    Ok((bytes, start + len))
}

/// `piece layers`, read with its keys as the raw 32 bytes they are: the
/// general decoder turns keys into text, and a SHA-256 is not text.
fn piece_layers(data: &[u8]) -> Result<std::collections::HashMap<[u8; 32], Vec<u8>>, String> {
    let mut out = std::collections::HashMap::new();
    if data.first() != Some(&b'd') {
        return Err("torrent is not a dict".into());
    }
    let mut i = 1;
    while i < data.len() && data[i] != b'e' {
        let (key, after) = raw_string(data, i)?;
        if key == b"piece layers" {
            if data.get(after) != Some(&b'd') {
                return Err("piece layers is not a dict".into());
            }
            let mut j = after + 1;
            while j < data.len() && data[j] != b'e' {
                let (k, v_at) = raw_string(data, j)?;
                let (v, next) = raw_string(data, v_at)?;
                if k.len() == 32 {
                    let mut h = [0u8; 32];
                    h.copy_from_slice(k);
                    out.insert(h, v.to_vec());
                }
                j = next;
            }
            return Ok(out);
        }
        i = skip_value(data, after)?;
    }
    Ok(out)
}

/// What each piece of a v2 torrent is checked against, in piece order.
///
/// A file of more than one piece takes its hashes from `piece layers`, which
/// is checked against the file's `pieces root` first: the layers sit outside
/// the info dict, so the info hash does not vouch for them -- the root does.
pub fn v2_piece_table(data: &[u8]) -> Result<Vec<crate::torrent::merkle::PieceCheck>, String> {
    use crate::torrent::merkle::{self, PieceCheck, BLOCK};
    let value = bencode_decode(data)?;
    let dict = value.as_dict().ok_or("torrent is not a dict")?;
    let info = dict.get("info").and_then(|i| i.as_dict()).ok_or("missing info dict")?;
    let pl = info.get("piece length").and_then(|v| v.as_int()).ok_or("missing piece length")? as u64;
    let layers = piece_layers(data)?;
    let per_piece = (pl / BLOCK as u64) as usize;
    // The root of a subtree of zero leaves, one piece wide: what stands for
    // the pieces past the end of a file when its layer is checked.
    let mut pad = [0u8; 32];
    let mut w = 1;
    while w < per_piece {
        pad = merkle_pair(&pad, &pad);
        w *= 2;
    }
    let mut out = Vec::new();
    for f in v2_files(&info)? {
        let Some(root) = f.root else { continue };
        let n = f.length.div_ceil(pl) as usize;
        if f.length <= pl {
            out.push(PieceCheck { hash: root, data_len: f.length as u32, leaves: merkle::small_file_leaves(f.length) as u32 });
            continue;
        }
        let layer = layers.get(&root).ok_or_else(|| format!("no piece layer for {}", f.path.display()))?;
        if layer.len() != n * 32 {
            return Err(format!("the piece layer of {} has {} hashes, expected {n}", f.path.display(), layer.len() / 32));
        }
        let mut level: Vec<[u8; 32]> = layer.chunks(32).map(|c| c.try_into().unwrap()).collect();
        level.resize(n.next_power_of_two(), pad);
        while level.len() > 1 {
            level = level.chunks(2).map(|p| merkle_pair(&p[0], &p[1])).collect();
        }
        if level[0] != root {
            return Err(format!("the piece layer of {} does not hash to its pieces root", f.path.display()));
        }
        for (j, h) in layer.chunks(32).enumerate() {
            let data_len = (f.length - j as u64 * pl).min(pl);
            out.push(PieceCheck { hash: h.try_into().unwrap(), data_len: data_len as u32, leaves: per_piece as u32 });
        }
    }
    Ok(out)
}

fn merkle_pair(a: &[u8; 32], b: &[u8; 32]) -> [u8; 32] {
    use sha2::{Digest, Sha256};
    let mut h = Sha256::new();
    h.update(a);
    h.update(b);
    h.finalize().into()
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
/// The piece hash table of a metainfo held in memory.
///
/// The bytes come from the store, which keys them by info-hash, so there is
/// no path to name in the errors -- the caller knows which torrent it asked
/// for. See `TorrentState::piece_hash`.
pub fn piece_hashes_from_bytes(data: &[u8]) -> Result<Vec<[u8; 20]>, String> {
    let info_raw = find_info_raw(data)?;
    let dict = bencode_decode(&info_raw).map_err(|e| format!("info dict: {}", e))?;
    let dict = dict.as_dict().ok_or("info is not a dict")?;
    let raw = dict
        .get("pieces")
        .ok_or("missing pieces")?
        .as_bytes()
        .ok_or("pieces not bytes")?;
    if raw.len() % 20 != 0 {
        return Err("pieces length not multiple of 20".into());
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

#[cfg(test)]
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

/// The raw info dict of a metainfo held in memory, for BEP 9.
pub fn info_dict_from_bytes(data: &[u8]) -> Result<Vec<u8>, String> {
    find_info_raw(data)
}

#[cfg(test)]
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

    /// A torrent from its name and file list, lengths computed.
    fn with_paths(name: &str, files: &[&[&str]]) -> Vec<u8> {
        let mut info = Vec::new();
        if files.is_empty() {
            info.extend_from_slice(b"d6:lengthi1e");
        } else {
            info.extend_from_slice(b"d5:filesl");
            for parts in files {
                info.extend_from_slice(b"d6:lengthi1e4:pathl");
                for p in *parts {
                    info.extend_from_slice(format!("{}:{p}", p.len()).as_bytes());
                }
                info.extend_from_slice(b"ee");
            }
            info.push(b'e');
        }
        info.extend_from_slice(format!("4:name{}:{name}", name.len()).as_bytes());
        info.extend_from_slice(b"12:piece lengthi16384e6:pieces20:");
        info.extend_from_slice(&[7u8; 20]);
        info.push(b'e');
        let mut b = b"d4:info".to_vec();
        b.extend_from_slice(&info);
        b.push(b'e');
        b
    }

    fn refused(bytes: &[u8]) -> String {
        match parse_torrent_bytes(bytes) {
            Ok(m) => panic!("accepted a torrent that escapes its folder: {:?}", m.files),
            Err(e) => e,
        }
    }

    #[test]
    fn a_file_path_that_climbs_out_is_refused() {
        let e = refused(&with_paths("show", &[&["..", "..", "etc", "cron.d", "x"]]));
        assert!(e.contains("unsafe path"), "{e}");
        refused(&with_paths("show", &[&["ok.bin"], &["sub", "..", "..", "x"]]));
    }

    #[test]
    fn an_absolute_component_is_refused() {
        refused(&with_paths("show", &[&["/etc", "passwd"]]));
        refused(&with_paths("show", &[&["a/../../x"]]));
    }

    #[test]
    fn a_name_that_climbs_out_is_refused() {
        refused(&with_paths("..", &[]));
        refused(&with_paths("../x.bin", &[]));
        refused(&with_paths("..", &[&["a.bin"]]));
        refused(&with_paths("/abs", &[]));
    }

    #[test]
    fn ordinary_nested_paths_still_parse() {
        let m = parse_torrent_bytes(&with_paths("Show S01", &[&["e01.mkv"], &["Extras", "making of.mkv"]]))
            .expect("a normal torrent");
        assert_eq!(m.files.len(), 2);
        parse_torrent_bytes(&with_paths("file..name.v2.bin", &[])).expect("dots inside a name are fine");
        parse_torrent_bytes(&with_paths("show", &[&[".hidden"]])).expect("a dotfile is a plain name");
    }

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

    // --- BEP 47 / BEP 52 --------------------------------------------------

    fn bstr(b: &[u8]) -> Vec<u8> {
        let mut o = format!("{}:", b.len()).into_bytes();
        o.extend_from_slice(b);
        o
    }

    /// A hybrid-style v1 torrent with a pad file between its two files.
    fn padded(pl: u64) -> Vec<u8> {
        let mut info = b"d5:filesl".to_vec();
        info.extend(b"d6:lengthi100e4:pathl5:a.bineed4:attr1:p6:lengthi");
        info.extend(format!("{}e4:pathl4:.pad3:924ee", pl - 100).as_bytes());
        info.extend(b"d6:lengthi10e4:pathl5:b.bineee");
        info.extend(b"4:name3:set12:piece lengthi");
        info.extend(format!("{pl}e6:pieces40:").as_bytes());
        info.extend([1u8; 40]);
        info.push(b'e');
        let mut b = b"d4:info".to_vec();
        b.extend(info);
        b.push(b'e');
        b
    }

    /// A pad file is part of the stream, never a file: the next file starts
    /// after it, and nothing named `.pad` is in the list.
    #[test]
    fn a_pad_file_is_a_gap_not_a_file() {
        let m = parse_torrent_bytes(&padded(16384)).unwrap();
        assert_eq!(m.files.len(), 2);
        assert_eq!((m.files[0].offset, m.files[1].offset), (0, 16384));
        assert!(m.files.iter().all(|f| !f.path.starts_with(".pad")));
        let ops = m.map_block(0, 0, 16384);
        assert_eq!(ops.len(), 2);
        assert!(!ops[0].pad && ops[0].length == 100);
        assert!(ops[1].pad && ops[1].length == 16384 - 100, "the rest of piece 0 is padding");
        let ops = m.map_block(1, 0, 10);
        assert_eq!((ops.len(), ops[0].pad, ops[0].file_offset), (1, false, 0), "piece 1 is b.bin");
    }

    struct V2 {
        torrent: Vec<u8>,
        files: Vec<(String, Vec<u8>)>,
    }

    /// A v2-only torrent built by BEP 52's rules, piece layers included.
    fn v2_torrent(pl: usize, files: &[(&str, usize)]) -> V2 {
        use crate::torrent::merkle::{root, small_file_leaves, BLOCK};
        let mut tree = b"d".to_vec();
        let mut layers = Vec::new();
        let mut out_files = Vec::new();
        let mut sorted: Vec<_> = files.to_vec();
        sorted.sort();
        for (i, (name, len)) in sorted.iter().enumerate() {
            let data: Vec<u8> = (0..*len).map(|j| ((j * 7 + i * 13) % 251) as u8).collect();
            let per_piece = pl / BLOCK;
            let (rt, layer) = if *len <= pl {
                (root(&data, small_file_leaves(*len as u64)), None)
            } else {
                let pieces: Vec<[u8; 32]> = data.chunks(pl).map(|c| root(c, per_piece)).collect();
                let total_leaves = (len.div_ceil(BLOCK)).next_power_of_two();
                (root(&data, total_leaves), Some(pieces.concat()))
            };
            tree.extend(bstr(name.as_bytes()));
            tree.extend(b"d0:d6:lengthi");
            tree.extend(format!("{len}e").as_bytes());
            tree.extend(bstr(b"pieces root"));
            tree.extend(bstr(&rt));
            tree.extend(b"ee");
            if let Some(l) = layer {
                layers.push((rt, l));
            }
            out_files.push((name.to_string(), data));
        }
        tree.push(b'e');
        let mut info = b"d".to_vec();
        info.extend(bstr(b"file tree"));
        info.extend(tree);
        info.extend(b"12:meta versioni2e4:name3:set12:piece lengthi");
        info.extend(format!("{pl}ee").as_bytes());
        layers.sort();
        let mut t = b"d4:info".to_vec();
        t.extend(info);
        t.extend(bstr(b"piece layers"));
        t.push(b'd');
        for (k, v) in layers {
            t.extend(bstr(&k));
            t.extend(bstr(&v));
        }
        t.extend(b"ee");
        V2 { torrent: t, files: out_files }
    }

    /// ⭐ Every piece of a v2 torrent is aligned to its file and checks
    /// against its merkle hash; the identity is the truncated SHA-256.
    #[test]
    fn a_v2_torrent_reads_and_every_piece_checks() {
        use sha2::{Digest, Sha256};
        let pl = 65536;
        let v = v2_torrent(pl, &[("a.bin", 100), ("b.bin", 3 * 16384 + 5), ("c.bin", 3 * pl + 1000)]);
        let m = parse_torrent_bytes(&v.torrent).expect("v2 parses");
        assert!(m.v2 && m.multi_file);
        let info = find_info_raw(&v.torrent).unwrap();
        assert_eq!(m.info_hash[..], Sha256::digest(&info)[..20], "the truncated SHA-256");
        let offs: Vec<u64> = m.files.iter().map(|f| f.offset).collect();
        assert_eq!(offs, vec![0, pl as u64, 2 * pl as u64], "each file on a piece boundary");
        assert_eq!(m.num_pieces, 1 + 1 + 4);

        let table = v2_piece_table(&v.torrent).expect("layers check out");
        assert_eq!(table.len(), 6);
        // Rebuild each piece as `map_block` lays it out and check it.
        let mut stream = vec![0u8; m.total_size as usize];
        for (f, (_, data)) in m.files.iter().zip(&v.files) {
            stream[f.offset as usize..f.offset as usize + data.len()].copy_from_slice(data);
        }
        for p in 0..m.num_pieces {
            let start = p as usize * pl;
            let piece = &stream[start..start + m.piece_size(p) as usize];
            assert!(table[p as usize].matches(piece), "piece {p}");
        }
        let mut bad = stream[..pl].to_vec();
        bad[5] ^= 1;
        assert!(!table[0].matches(&bad));
    }

    /// The piece layers are outside the info dict: one that does not hash
    /// to its file's pieces root is refused, not trusted.
    #[test]
    fn a_tampered_piece_layer_is_refused() {
        let v = v2_torrent(16384, &[("big.bin", 5 * 16384)]);
        let mut t = v.torrent.clone();
        let at = t.windows(12).position(|w| w == b"piece layers").unwrap();
        let last = t.len() - 3;
        assert!(at < last);
        t[last] ^= 0xff;
        assert!(v2_piece_table(&t).unwrap_err().contains("does not hash to its pieces root"));
    }

    #[test]
    fn a_malformed_v2_torrent_is_refused() {
        let v = v2_torrent(65536, &[("a.bin", 10)]);
        let bad_pl = String::from_utf8_lossy(&v.torrent).replace("piece lengthi65536e", "piece lengthi65535e");
        assert!(parse_torrent_bytes(bad_pl.as_bytes()).is_err(), "not a power of two");
        let single = parse_torrent_bytes(&v.torrent).unwrap();
        assert!(!single.multi_file, "one file at the top is a single-file torrent");
        assert_eq!(single.files[0].path, std::path::PathBuf::from("a.bin"));
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
