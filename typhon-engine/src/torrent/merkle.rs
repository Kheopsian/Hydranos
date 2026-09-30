//! BEP 52 merkle hashing: how a v2 torrent names the content of a piece.
//!
//! Each file is its own tree. Its leaves are the SHA-256 of every 16 KiB
//! block of the file (the last block as long as it is), the leaf count is
//! padded to a power of two with zero hashes, and every inner node is the
//! SHA-256 of its two children.
//!
//! What a piece is checked against depends on the file's size:
//! - a file longer than one piece: the `piece layers` entry, the root of the
//!   piece's own subtree of `piece_length / 16 KiB` leaves -- its last piece
//!   padded with zero leaves up to that count;
//! - a file of one piece or less: the file's `pieces root`, the root of a
//!   tree of the next power of two of its block count -- NOT of a whole
//!   piece's worth of leaves.

use sha2::{Digest, Sha256};

pub const BLOCK: usize = 16 * 1024;

pub type Hash = [u8; 32];

fn sha256(parts: &[&[u8]]) -> Hash {
    let mut h = Sha256::new();
    for p in parts {
        h.update(p);
    }
    h.finalize().into()
}

/// The root of a tree over `data`, with `leaves` leaves: one per 16 KiB block,
/// then zero hashes up to `leaves`. `leaves` must be a power of two no smaller
/// than the block count.
pub fn root(data: &[u8], leaves: usize) -> Hash {
    let mut layer: Vec<Hash> = data.chunks(BLOCK).map(|b| sha256(&[b])).collect();
    if layer.is_empty() {
        layer.push([0u8; 32]);
    }
    let leaves = leaves.max(layer.len()).next_power_of_two();
    layer.resize(leaves, [0u8; 32]);
    while layer.len() > 1 {
        layer = layer.chunks(2).map(|p| sha256(&[&p[0], &p[1]])).collect();
    }
    layer[0]
}

/// Leaves of a file of `len` bytes that fits in one piece: its block count,
/// rounded up to a power of two.
pub fn small_file_leaves(len: u64) -> usize {
    (len.div_ceil(BLOCK as u64).max(1) as usize).next_power_of_two()
}

/// What one piece of a v2 torrent is checked against.
#[derive(Debug, Clone, PartialEq)]
pub struct PieceCheck {
    pub hash: Hash,
    /// Bytes of file data in the piece. The rest of a piece, up to the next
    /// file's first piece, is alignment padding and is not hashed.
    pub data_len: u32,
    pub leaves: u32,
}

impl PieceCheck {
    pub fn matches(&self, piece: &[u8]) -> bool {
        let n = (self.data_len as usize).min(piece.len());
        root(&piece[..n], self.leaves as usize) == self.hash
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn one_block_is_its_own_hash() {
        let d = vec![7u8; 1000];
        assert_eq!(root(&d, 1), sha256(&[&d]));
    }

    /// Two blocks: the parent of their two hashes. Three: padded to four
    /// with a zero leaf.
    #[test]
    fn the_tree_is_built_as_bep_52_says() {
        let d: Vec<u8> = (0..(3 * BLOCK)).map(|i| (i % 251) as u8).collect();
        let h: Vec<Hash> = d.chunks(BLOCK).map(|b| sha256(&[b])).collect();
        let two = sha256(&[&h[0], &h[1]]);
        assert_eq!(root(&d[..2 * BLOCK], 2), two);
        let right = sha256(&[&h[2], &[0u8; 32]]);
        assert_eq!(root(&d, 4), sha256(&[&two, &right]));
        assert_eq!(root(&d, 1), root(&d, 4), "never fewer leaves than blocks");
    }

    #[test]
    fn a_small_file_has_the_next_power_of_two_of_its_blocks() {
        assert_eq!(small_file_leaves(1), 1);
        assert_eq!(small_file_leaves(BLOCK as u64), 1);
        assert_eq!(small_file_leaves(BLOCK as u64 + 1), 2);
        assert_eq!(small_file_leaves(3 * BLOCK as u64), 4);
    }

    /// The padding after a file's data is not part of what is hashed.
    #[test]
    fn alignment_padding_is_not_hashed() {
        let d = vec![1u8; BLOCK + 10];
        let c = PieceCheck { hash: root(&d, 4), data_len: d.len() as u32, leaves: 4 };
        let mut padded = d.clone();
        padded.resize(4 * BLOCK, 0);
        assert!(c.matches(&padded));
        padded[3] ^= 1;
        assert!(!c.matches(&padded));
    }
}
