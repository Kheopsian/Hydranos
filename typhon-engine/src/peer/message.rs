use bytes::{Buf, BufMut, Bytes, BytesMut};

pub const MSG_CHOKE: u8 = 0;
pub const MSG_UNCHOKE: u8 = 1;
pub const MSG_INTERESTED: u8 = 2;
pub const MSG_NOT_INTERESTED: u8 = 3;
pub const MSG_HAVE: u8 = 4;
pub const MSG_BITFIELD: u8 = 5;
pub const MSG_REQUEST: u8 = 6;
pub const MSG_PIECE: u8 = 7;
pub const MSG_CANCEL: u8 = 8;

// BEP 6 Fast Extension
pub const MSG_HAVE_ALL: u8 = 0x0E;
pub const MSG_HAVE_NONE: u8 = 0x0F;
pub const MSG_SUGGEST: u8 = 0x0D;
pub const MSG_REJECT: u8 = 0x10;
pub const MSG_ALLOWED_FAST: u8 = 0x11;

// BEP 10 Extension Protocol
pub const MSG_EXTENDED: u8 = 20;

#[derive(Debug, Clone)]
pub enum Message {
    KeepAlive,
    Choke,
    Unchoke,
    Interested,
    NotInterested,
    Have { piece: u32 },
    Bitfield { data: Bytes },
    Request { index: u32, begin: u32, length: u32 },
    Piece { index: u32, begin: u32, data: Bytes },
    Cancel { index: u32, begin: u32, length: u32 },
    // BEP 6
    HaveAll,
    HaveNone,
    Suggest { piece: u32 },
    Reject { index: u32, begin: u32, length: u32 },
    AllowedFast { piece: u32 },
    // BEP 10 — extended. ext_id=0 is the extended handshake,
    // other ids map to extensions negotiated in the handshake (e.g. ut_pex).
    Extended { ext_id: u8, payload: Bytes },
    // Unknown
    Unknown { id: u8, payload: Bytes },
}

impl Message {
    pub fn decode(id: u8, buf: &mut BytesMut) -> Result<Self, String> {
        match id {
            MSG_CHOKE => Ok(Message::Choke),
            MSG_UNCHOKE => Ok(Message::Unchoke),
            MSG_INTERESTED => Ok(Message::Interested),
            MSG_NOT_INTERESTED => Ok(Message::NotInterested),
            MSG_HAVE if buf.len() >= 4 => Ok(Message::Have { piece: buf.get_u32() }),
            MSG_BITFIELD => Ok(Message::Bitfield { data: buf.copy_to_bytes(buf.remaining()) }),
            MSG_REQUEST if buf.len() >= 12 => Ok(Message::Request {
                index: buf.get_u32(),
                begin: buf.get_u32(),
                length: buf.get_u32(),
            }),
            MSG_PIECE if buf.len() >= 8 => {
                let index = buf.get_u32();
                let begin = buf.get_u32();
                let data = buf.copy_to_bytes(buf.remaining());
                Ok(Message::Piece { index, begin, data })
            }
            MSG_CANCEL if buf.len() >= 12 => Ok(Message::Cancel {
                index: buf.get_u32(),
                begin: buf.get_u32(),
                length: buf.get_u32(),
            }),
            MSG_HAVE_ALL => Ok(Message::HaveAll),
            MSG_HAVE_NONE => Ok(Message::HaveNone),
            MSG_SUGGEST if buf.len() >= 4 => Ok(Message::Suggest { piece: buf.get_u32() }),
            MSG_REJECT if buf.len() >= 12 => Ok(Message::Reject {
                index: buf.get_u32(),
                begin: buf.get_u32(),
                length: buf.get_u32(),
            }),
            MSG_ALLOWED_FAST if buf.len() >= 4 => Ok(Message::AllowedFast { piece: buf.get_u32() }),
            MSG_EXTENDED if !buf.is_empty() => {
                let ext_id = buf.get_u8();
                Ok(Message::Extended { ext_id, payload: buf.copy_to_bytes(buf.remaining()) })
            }
            _ => Ok(Message::Unknown { id, payload: buf.copy_to_bytes(buf.remaining()) }),
        }
    }

    pub fn encode_payload(&self) -> Vec<u8> {
        let mut buf = Vec::new();
        match self {
            Message::KeepAlive => {},
            Message::Choke => buf.push(MSG_CHOKE),
            Message::Unchoke => buf.push(MSG_UNCHOKE),
            Message::Interested => buf.push(MSG_INTERESTED),
            Message::NotInterested => buf.push(MSG_NOT_INTERESTED),
            Message::Have { piece } => {
                buf.push(MSG_HAVE);
                buf.put_u32(*piece);
            }
            Message::Bitfield { data } => {
                buf.push(MSG_BITFIELD);
                buf.extend_from_slice(data);
            }
            Message::Request { index, begin, length } => {
                buf.push(MSG_REQUEST);
                buf.put_u32(*index);
                buf.put_u32(*begin);
                buf.put_u32(*length);
            }
            Message::Piece { index, begin, data } => {
                buf.push(MSG_PIECE);
                buf.put_u32(*index);
                buf.put_u32(*begin);
                buf.extend_from_slice(data);
            }
            Message::Cancel { index, begin, length } => {
                buf.push(MSG_CANCEL);
                buf.put_u32(*index);
                buf.put_u32(*begin);
                buf.put_u32(*length);
            }
            Message::HaveAll => buf.push(MSG_HAVE_ALL),
            Message::HaveNone => buf.push(MSG_HAVE_NONE),
            Message::Suggest { piece } => {
                buf.push(MSG_SUGGEST);
                buf.put_u32(*piece);
            }
            Message::Reject { index, begin, length } => {
                buf.push(MSG_REJECT);
                buf.put_u32(*index);
                buf.put_u32(*begin);
                buf.put_u32(*length);
            }
            Message::AllowedFast { piece } => {
                buf.push(MSG_ALLOWED_FAST);
                buf.put_u32(*piece);
            }
            Message::Extended { ext_id, payload } => {
                buf.push(MSG_EXTENDED);
                buf.push(*ext_id);
                buf.extend_from_slice(payload);
            }
            Message::Unknown { id, payload } => {
                buf.push(*id);
                buf.extend_from_slice(payload);
            }
        }
        buf
    }
}
