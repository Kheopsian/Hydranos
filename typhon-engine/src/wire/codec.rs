use bytes::{Buf, BufMut, BytesMut};
use tokio_util::codec::{Decoder, Encoder};
use std::io;

use crate::peer::message::Message;

/// BitTorrent wire protocol codec: 4-byte big-endian length prefix + payload.
/// Keepalive = length 0 (no payload).
pub struct BtCodec {
    max_frame_len: usize,
}

impl BtCodec {
    pub fn new() -> Self {
        // Max piece = 16 KiB data + 13 bytes header, plus some margin
        Self { max_frame_len: 1 << 18 } // 256 KiB
    }
}

impl Decoder for BtCodec {
    type Item = Message;
    type Error = io::Error;

    fn decode(&mut self, src: &mut BytesMut) -> Result<Option<Message>, io::Error> {
        if src.len() < 4 {
            return Ok(None);
        }
        let len = u32::from_be_bytes([src[0], src[1], src[2], src[3]]) as usize;
        if len == 0 {
            src.advance(4);
            return Ok(Some(Message::KeepAlive));
        }
        if len > self.max_frame_len {
            return Err(io::Error::new(io::ErrorKind::InvalidData,
                format!("frame too large: {}", len)));
        }
        if src.len() < 4 + len {
            src.reserve(4 + len - src.len());
            return Ok(None);
        }
        src.advance(4);
        let mut payload = src.split_to(len);
        let id = payload.get_u8();
        Message::decode(id, &mut payload)
            .map(Some)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
    }
}

impl Encoder<Message> for BtCodec {
    type Error = io::Error;

    fn encode(&mut self, msg: Message, dst: &mut BytesMut) -> Result<(), io::Error> {
        let payload = msg.encode_payload();
        dst.reserve(4 + payload.len());
        dst.put_u32(payload.len() as u32);
        dst.extend_from_slice(&payload);
        Ok(())
    }
}
