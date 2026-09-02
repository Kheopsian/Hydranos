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

/// A read buffer that once had to hold a large frame keeps that capacity for
/// the life of the connection: `BytesMut::reserve` grows by doubling, and
/// `split_to` below hands the payload out on the same allocation, so the
/// buffer cannot be reclaimed in place while that payload is alive. Production
/// at 192k torrents showed 2042 live read/write buffers averaging ~131 KB --
/// 128 KiB apiece, on connections whose steady state is 17-byte Requests.
///
/// So: when the buffer is *empty* and has ratcheted past the high-water mark,
/// swap in a right-sized one. Empty means there is nothing to copy, and the
/// check is one comparison on a path that already returns early. A connection
/// actively receiving 16 KiB blocks stays above the mark between frames and is
/// left alone; it is the seeding majority, which never needs more than a few
/// hundred bytes, that gets its 128 KiB back.
const READ_BUF_HIGH_WATER: usize = 64 * 1024;
const READ_BUF_TARGET: usize = 32 * 1024;
/// Sized so a full 16 KiB Piece plus its header always fits after a swap: an
/// actively serving connection must never trade a shrink for a regrow.
const WRITE_BUF_HIGH_WATER: usize = 64 * 1024;
const WRITE_BUF_TARGET: usize = 32 * 1024;

impl Decoder for BtCodec {
    type Item = Message;
    type Error = io::Error;

    fn decode(&mut self, src: &mut BytesMut) -> Result<Option<Message>, io::Error> {
        if src.is_empty() && src.capacity() > READ_BUF_HIGH_WATER {
            *src = BytesMut::with_capacity(READ_BUF_TARGET);
        }
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
        // Same ratchet as the read side, and on a seeding instance this is the
        // half that matters: the write buffer is the one carrying 16 KiB Piece
        // payloads, and messages fed without an intervening flush accumulate
        // before the framing layer drains them. The high-water mark sits well
        // above one full Piece so an actively serving peer never oscillates --
        // it only fires on a buffer that batched far more than it now needs.
        if dst.is_empty() && dst.capacity() > WRITE_BUF_HIGH_WATER {
            *dst = BytesMut::with_capacity(WRITE_BUF_TARGET);
        }
        let payload = msg.encode_payload();
        dst.reserve(4 + payload.len());
        dst.put_u32(payload.len() as u32);
        dst.extend_from_slice(&payload);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The regression itself: a buffer that ratcheted to 128 KiB and then
    /// drained must not keep that capacity. Remove the swap in `decode` and
    /// this fails -- capacity stays at 131072.
    #[test]
    fn empty_oversized_read_buffer_is_reclaimed() {
        let mut codec = BtCodec::new();
        let mut buf = BytesMut::with_capacity(128 * 1024);
        assert!(buf.capacity() > READ_BUF_HIGH_WATER);
        assert!(buf.is_empty());

        let out = codec.decode(&mut buf).expect("decode must not error");
        assert!(out.is_none(), "an empty buffer yields no message");
        assert!(
            buf.capacity() <= READ_BUF_TARGET,
            "oversized empty buffer kept {} bytes",
            buf.capacity()
        );
    }

    /// The other half of the contract: buffered bytes are never discarded.
    /// A swap on a non-empty buffer would silently eat a partial frame.
    #[test]
    fn buffered_bytes_are_never_dropped() {
        let mut codec = BtCodec::new();
        let mut buf = BytesMut::with_capacity(128 * 1024);
        buf.extend_from_slice(&[0u8, 0, 0]); // 3 bytes: a partial length prefix
        let out = codec.decode(&mut buf).expect("decode must not error");
        assert!(out.is_none(), "3 bytes is not yet a frame");
        assert_eq!(buf.len(), 3, "partial frame was discarded");
    }

    /// A buffer already at a sane size must not be reallocated: that would put
    /// an allocation on the hot path for every message.
    #[test]
    fn right_sized_buffer_is_left_alone() {
        let mut codec = BtCodec::new();
        let mut buf = BytesMut::with_capacity(8 * 1024);
        let before = buf.capacity();
        let _ = codec.decode(&mut buf).expect("decode must not error");
        assert_eq!(buf.capacity(), before, "a small buffer was needlessly swapped");
    }

    /// The write half: a batched-up buffer that drained must not keep its peak.
    /// Remove the swap in `encode` and this fails -- capacity stays at 131072.
    #[test]
    fn empty_oversized_write_buffer_is_reclaimed() {
        let mut codec = BtCodec::new();
        let mut buf = BytesMut::with_capacity(128 * 1024);
        assert!(buf.is_empty());
        codec.encode(Message::KeepAlive, &mut buf).expect("encode must not error");
        assert!(
            buf.capacity() <= WRITE_BUF_TARGET,
            "oversized empty write buffer kept {} bytes",
            buf.capacity()
        );
    }

    /// Bytes already queued for the socket must never be discarded by the swap.
    #[test]
    fn queued_write_bytes_are_never_dropped() {
        let mut codec = BtCodec::new();
        let mut buf = BytesMut::with_capacity(128 * 1024);
        buf.extend_from_slice(&[1u8, 2, 3, 4]);
        codec.encode(Message::KeepAlive, &mut buf).expect("encode must not error");
        assert_eq!(&buf[..4], &[1u8, 2, 3, 4], "queued bytes were dropped");
        assert_eq!(buf.len(), 8, "keepalive should append 4 bytes");
    }

    /// A full keepalive still decodes with the swap in front of it.
    #[test]
    fn keepalive_still_decodes() {
        let mut codec = BtCodec::new();
        let mut buf = BytesMut::with_capacity(8 * 1024);
        buf.extend_from_slice(&[0u8, 0, 0, 0]);
        let out = codec.decode(&mut buf).expect("decode must not error");
        assert!(matches!(out, Some(Message::KeepAlive)));
    }
}
