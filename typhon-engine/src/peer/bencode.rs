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

pub fn decode(input: &[u8]) -> Result<Bencode, &'static str> {
    let mut pos = 0;
    let v = decode_one(input, &mut pos)?;
    Ok(v)
}

fn decode_one(input: &[u8], pos: &mut usize) -> Result<Bencode, &'static str> {
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
                list.push(decode_one(input, pos)?);
            }
            if *pos >= input.len() { return Err("no e for list"); }
            *pos += 1;
            Ok(Bencode::List(list))
        }
        b'd' => {
            *pos += 1;
            let mut dict = BTreeMap::new();
            while *pos < input.len() && input[*pos] != b'e' {
                let k = decode_one(input, pos)?;
                let key = match k { Bencode::Bytes(b) => b, _ => return Err("dict key not bytes") };
                let v = decode_one(input, pos)?;
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
            if *pos + n > input.len() { return Err("bytes truncated"); }
            let bytes = input[*pos..*pos + n].to_vec();
            *pos += n;
            Ok(Bencode::Bytes(bytes))
        }
        _ => Err("unknown type"),
    }
}
