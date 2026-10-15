//! Minimal bencode encoder/decoder, just enough for BEP 10 extension handshake and BEP 11 PEX.
//! Only handles the subset we send/receive: ints, byte strings, lists, dicts.

use std::collections::BTreeMap;

#[derive(Debug, Clone)]
pub enum Bencode {
    Int(i64),
    Bytes(Vec<u8>),
    List(Vec<Bencode>),
    Dict(BTreeMap<Vec<u8>, Bencode>),
}

impl Bencode {
    pub fn as_int(&self) -> Option<i64> { if let Bencode::Int(i) = self { Some(*i) } else { None } }
    pub fn as_bytes(&self) -> Option<&[u8]> { if let Bencode::Bytes(b) = self { Some(b) } else { None } }
    pub fn as_dict(&self) -> Option<&BTreeMap<Vec<u8>, Bencode>> { if let Bencode::Dict(d) = self { Some(d) } else { None } }

    pub fn dict_get(&self, key: &[u8]) -> Option<&Bencode> {
        self.as_dict().and_then(|d| d.get(key))
    }

    pub fn encode(&self, out: &mut Vec<u8>) {
        match self {
            Bencode::Int(i) => {
                out.push(b'i');
                out.extend_from_slice(i.to_string().as_bytes());
                out.push(b'e');
            }
            Bencode::Bytes(b) => {
                out.extend_from_slice(b.len().to_string().as_bytes());
                out.push(b':');
                out.extend_from_slice(b);
            }
            Bencode::List(l) => {
                out.push(b'l');
                for v in l { v.encode(out); }
                out.push(b'e');
            }
            Bencode::Dict(d) => {
                out.push(b'd');
                for (k, v) in d {
                    Bencode::Bytes(k.clone()).encode(out);
                    v.encode(out);
                }
                out.push(b'e');
            }
        }
    }

    pub fn to_vec(&self) -> Vec<u8> {
        let mut v = Vec::with_capacity(64);
        self.encode(&mut v);
        v
    }
}

/// Max bencode nesting depth. A hostile peer can send a frame that is nothing
/// but opening tokens (`llll...`); decoding it recursively would overflow the
/// worker stack and abort the whole process. Honest messages (the extension
/// handshake, PEX, ut_metadata) nest only a handful deep, so this ceiling is
/// never reached by real traffic.
const MAX_DEPTH: usize = 100;

pub fn decode(input: &[u8]) -> Result<Bencode, &'static str> {
    let mut pos = 0;
    let v = decode_one(input, &mut pos, 0)?;
    Ok(v)
}

/// Decode one value from the front of `input` and report how many bytes it
/// occupied. BEP 9 needs this: a ut_metadata message is a bencoded dict
/// immediately followed by the raw block, with nothing marking where the dict
/// ends -- the decoder's own end position is the only boundary.
pub fn decode_prefix(input: &[u8]) -> Result<(Bencode, usize), &'static str> {
    let mut pos = 0;
    let v = decode_one(input, &mut pos, 0)?;
    Ok((v, pos))
}

fn decode_one(input: &[u8], pos: &mut usize, depth: usize) -> Result<Bencode, &'static str> {
    if depth >= MAX_DEPTH { return Err("bencode nested too deep"); }
    if *pos >= input.len() { return Err("eof"); }
    match input[*pos] {
        b'i' => {
            *pos += 1;
            let end = input[*pos..].iter().position(|&b| b == b'e').ok_or("no e for int")?;
            let s = std::str::from_utf8(&input[*pos..*pos + end]).map_err(|_| "int utf8")?;
            let n: i64 = s.parse().map_err(|_| "int parse")?;
            *pos += end + 1;
            Ok(Bencode::Int(n))
        }
        b'l' => {
            *pos += 1;
            let mut list = Vec::new();
            while *pos < input.len() && input[*pos] != b'e' {
                list.push(decode_one(input, pos, depth + 1)?);
            }
            if *pos >= input.len() { return Err("no e for list"); }
            *pos += 1;
            Ok(Bencode::List(list))
        }
        b'd' => {
            *pos += 1;
            let mut dict = BTreeMap::new();
            while *pos < input.len() && input[*pos] != b'e' {
                let k = decode_one(input, pos, depth + 1)?;
                let key = match k { Bencode::Bytes(b) => b, _ => return Err("dict key not bytes") };
                let v = decode_one(input, pos, depth + 1)?;
                dict.insert(key, v);
            }
            if *pos >= input.len() { return Err("no e for dict"); }
            *pos += 1;
            Ok(Bencode::Dict(dict))
        }
        b'0'..=b'9' => {
            let colon = input[*pos..].iter().position(|&b| b == b':').ok_or("no colon")?;
            let s = std::str::from_utf8(&input[*pos..*pos + colon]).map_err(|_| "len utf8")?;
            let n: usize = s.parse().map_err(|_| "len parse")?;
            *pos += colon + 1;
            let end = pos.checked_add(n).ok_or("len overflow")?;
            if end > input.len() { return Err("bytes truncated"); }
            let bytes = input[*pos..end].to_vec();
            *pos = end;
            Ok(Bencode::Bytes(bytes))
        }
        _ => Err("unknown type"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deeply_nested_is_rejected_not_overflow() {
        // A hostile peer's extension payload of nothing but `l` must error out
        // well before it can overflow the worker stack and abort the process.
        // (Bencode is Debug/Clone only, no PartialEq, so match on the Err.)
        let bomb = vec![b'l'; 200_000];
        assert!(matches!(decode(&bomb), Err("bencode nested too deep")));
    }

    #[test]
    fn honest_nesting_still_decodes() {
        // d2:mdl... a couple of levels deep, like a real extension handshake.
        let input = b"d1:md11:ut_metadatai2eee";
        assert!(decode(input).is_ok());
    }

    #[test]
    fn huge_string_length_does_not_panic() {
        // len far past the buffer, and a len that would overflow pos + n.
        assert!(decode(b"99999999999999999999:x").is_err());
    }
}
