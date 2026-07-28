//! MSE/PE handshake implementation.
//!
//! Initiator (outgoing):
//!   1. Send: Ya = g^Xa mod P (96 bytes) + PadA (0-512 random)
//!   2. Recv: Yb (96 bytes) + PadB (0-512 random)
//!   3. Compute S = Yb^Xa mod P
//!   4. Send: HASH('req1', S) + HASH('req2', SKEY) XOR HASH('req3', S)
//!          + RC4(VC + crypto_provide + len(padC) + padC + len(IA) + IA)
//!   5. Recv: RC4(VC + crypto_select + len(padD) + padD)
//!
//! Receiver (incoming):
//!   1. Recv: Ya (96 bytes) + PadA
//!   2. Send: Yb = g^Xb mod P (96 bytes) + PadB
//!   3. Compute S = Ya^Xb mod P
//!   4. Recv: req1_hash + req2_xor + RC4(VC + crypto_provide + padC + IA)
//!   5. Send: RC4(VC + crypto_select + padD)

use sha1::{Sha1, Digest};
use num_bigint::BigUint;
use num_traits::One;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

use crate::peer::transport::PeerTransport;
use super::rc4::Rc4;

/// The 768-bit prime from the MSE spec.
const P_HEX: &str = "FFFFFFFFFFFFFFFFC90FDAA22168C234C4C6628B80DC1CD129024E088A67CC74020BBEA63B139B22514A08798E3404DDEF9519B3CD3A431B302B0A6DF25F14374FE1356D6D51C245E485B576625E7EC6F44C42E9A63A36210000000000090563";
const G: u32 = 2;

const VC: [u8; 8] = [0u8; 8];
const CRYPTO_PLAIN: u32 = 0x01;
const CRYPTO_RC4: u32 = 0x02;

/// Result of MSE handshake — contains the initial payload (IA) if we're the receiver.
pub struct MseHandshakeResult {
    pub peer_id: [u8; 20],
    pub info_hash: [u8; 20],
    pub fast_extension: bool,
    pub extended_protocol: bool,
}

fn get_prime() -> BigUint {
    BigUint::parse_bytes(P_HEX.as_bytes(), 16).unwrap()
}

fn gen_keypair() -> (BigUint, Vec<u8>) {
    use rand::RngCore;
    let p = get_prime();
    let g = BigUint::from(G);

    // Generate 160-bit private key
    let mut xa_bytes = [0u8; 20];
    rand::thread_rng().fill_bytes(&mut xa_bytes);
    let xa = BigUint::from_bytes_be(&xa_bytes);

    // Ya = g^Xa mod P
    let ya = g.modpow(&xa, &p);
    let mut ya_bytes = ya.to_bytes_be();
    // Pad to 96 bytes
    while ya_bytes.len() < 96 {
        ya_bytes.insert(0, 0);
    }

    (xa, ya_bytes)
}

fn compute_secret(their_pubkey: &[u8], our_privkey: &BigUint) -> Vec<u8> {
    let p = get_prime();
    let yb = BigUint::from_bytes_be(their_pubkey);
    let s = yb.modpow(our_privkey, &p);
    let mut s_bytes = s.to_bytes_be();
    while s_bytes.len() < 96 {
        s_bytes.insert(0, 0);
    }
    s_bytes
}

fn sha1_hash(data: &[u8]) -> [u8; 20] {
    let mut h = Sha1::new();
    h.update(data);
    let r = h.finalize();
    let mut out = [0u8; 20];
    out.copy_from_slice(&r);
    out
}

pub fn sha1_combine(prefix: &[u8], data: &[u8]) -> [u8; 20] {
    let mut h = Sha1::new();
    h.update(prefix);
    h.update(data);
    let r = h.finalize();
    let mut out = [0u8; 20];
    out.copy_from_slice(&r);
    out
}

fn make_rc4_keys(prefix: &[u8], s: &[u8], skey: &[u8]) -> Rc4 {
    let key = sha1_combine(prefix, &[s, skey].concat());
    let mut rc4 = Rc4::new(&key);
    rc4.discard(1024);
    rc4
}

/// Perform MSE handshake as initiator (outgoing connection).
pub async fn handshake_outgoing(
    stream: &mut PeerTransport,
    info_hash: &[u8; 20],
    peer_id: &[u8; 20],
) -> Result<(Rc4, Rc4, MseHandshakeResult), String> {
    // DH modpow is ~1-3 ms of CPU. If run directly on a tokio worker thread,
    // it blocks the reactor — with N concurrent dials we serialize those ms
    // across workers and starve all other async tasks on those cores.
    let (xa, ya_bytes) = tokio::task::spawn_blocking(gen_keypair)
        .await
        .map_err(|e| format!("spawn_blocking(gen_keypair): {}", e))?;

    // Step 1: Send Ya + PadA (random 0-64 bytes for simplicity)
    let mut pad_a = vec![0u8; 0]; // no padding for now
    stream.write_all(&ya_bytes).await.map_err(|e| e.to_string())?;
    stream.write_all(&pad_a).await.map_err(|e| e.to_string())?;

    // Step 2: Read Yb (96 bytes) — may be followed by PadB which we read later
    let mut yb_bytes = [0u8; 96];
    stream.read_exact(&mut yb_bytes).await.map_err(|e| e.to_string())?;

    // Step 3: Compute shared secret — offload modpow to blocking pool.
    let s = {
        let yb_vec = yb_bytes.to_vec();
        let xa_clone = xa.clone();
        tokio::task::spawn_blocking(move || compute_secret(&yb_vec, &xa_clone))
            .await
            .map_err(|e| format!("spawn_blocking(compute_secret): {}", e))?
    };
    let skey = info_hash;

    // Step 4: Send HASH('req1', S)
    let req1 = sha1_combine(b"req1", &s);
    stream.write_all(&req1).await.map_err(|e| e.to_string())?;

    // Send HASH('req2', SKEY) XOR HASH('req3', S)
    let req2 = sha1_combine(b"req2", skey);
    let req3 = sha1_combine(b"req3", &s);
    let mut req2_xor = [0u8; 20];
    for i in 0..20 {
        req2_xor[i] = req2[i] ^ req3[i];
    }
    stream.write_all(&req2_xor).await.map_err(|e| e.to_string())?;

    // Create RC4 keys
    // Initiator encrypts with key: SHA1('keyA', S, SKEY)
    // Initiator decrypts with key: SHA1('keyB', S, SKEY)
    let mut enc = make_rc4_keys(b"keyA", &s, skey);
    let mut dec = make_rc4_keys(b"keyB", &s, skey);

    // Send encrypted: VC + crypto_provide + len(padC=0) + padC + len(IA=68) + IA
    // IA = the BT handshake
    let bt_handshake = build_bt_handshake(info_hash, peer_id);

    let mut payload = Vec::new();
    payload.extend_from_slice(&VC);                         // 8 bytes VC
    payload.extend_from_slice(&(CRYPTO_RC4 | CRYPTO_PLAIN).to_be_bytes()); // 4 bytes crypto_provide
    payload.extend_from_slice(&0u16.to_be_bytes());         // 2 bytes len(padC)
    // padC = empty
    payload.extend_from_slice(&(bt_handshake.len() as u16).to_be_bytes()); // 2 bytes len(IA)
    payload.extend_from_slice(&bt_handshake);               // 68 bytes IA

    enc.process(&mut payload);
    stream.write_all(&payload).await.map_err(|e| e.to_string())?;

    // Step 5: Read encrypted VC + crypto_select + len(padD) + padD
    // We need to find VC (8 zero bytes after decryption)
    // Read and decrypt until we find VC
    let mut vc_buf = [0u8; 8];
    stream.read_exact(&mut vc_buf).await.map_err(|e| e.to_string())?;
    dec.process(&mut vc_buf);
    if vc_buf != VC {
        return Err("VC mismatch after decryption".into());
    }

    // Read crypto_select (4 bytes)
    let mut cs_buf = [0u8; 4];
    stream.read_exact(&mut cs_buf).await.map_err(|e| e.to_string())?;
    dec.process(&mut cs_buf);
    let crypto_select = u32::from_be_bytes(cs_buf);

    // Read len(padD) (2 bytes)
    let mut pad_len_buf = [0u8; 2];
    stream.read_exact(&mut pad_len_buf).await.map_err(|e| e.to_string())?;
    dec.process(&mut pad_len_buf);
    let pad_d_len = u16::from_be_bytes(pad_len_buf) as usize;

    // Read and discard padD
    if pad_d_len > 0 {
        let mut pad_d = vec![0u8; pad_d_len];
        stream.read_exact(&mut pad_d).await.map_err(|e| e.to_string())?;
        dec.process(&mut pad_d);
    }

    // Now read the BT handshake response (should come encrypted)
    let mut hs_buf = [0u8; 68];
    stream.read_exact(&mut hs_buf).await.map_err(|e| e.to_string())?;
    if crypto_select & CRYPTO_RC4 != 0 {
        dec.process(&mut hs_buf);
    }

    // Parse BT handshake
    if hs_buf[0] != 19 || &hs_buf[1..20] != b"BitTorrent protocol" {
        return Err("invalid BT handshake in MSE response".into());
    }
    let reserved = &hs_buf[20..28];
    let fast_ext = (reserved[7] & 0x04) != 0;
    let ext_proto = (reserved[5] & 0x10) != 0;
    let mut remote_peer_id = [0u8; 20];
    remote_peer_id.copy_from_slice(&hs_buf[48..68]);

    // If plaintext selected, disable encryption
    if crypto_select & CRYPTO_RC4 == 0 {
        // Return dummy rc4 that does nothing? Or use Option.
        // For simplicity, we still return the rc4 instances but they won't be used
        // since we'll check is_encrypted
    }

    Ok((enc, dec, MseHandshakeResult {
        peer_id: remote_peer_id,
        info_hash: *info_hash,
        fast_extension: fast_ext,
        extended_protocol: ext_proto,
    }))
}

/// Perform MSE handshake as receiver (incoming connection).
/// `first_bytes` are the initial bytes already read (if we need to detect MSE vs plaintext).
pub async fn handshake_incoming(
    stream: &mut PeerTransport,
    first_byte: u8,
    remaining_ya: &[u8], // 95 bytes (we already read 1 to detect MSE vs plain)
    our_peer_id: &[u8; 20],
    info_hash_lookup: impl Fn(&[u8; 20]) -> Option<[u8; 20]>,
) -> Result<(Rc4, Rc4, MseHandshakeResult), String> {
    let (xb, yb_bytes) = tokio::task::spawn_blocking(gen_keypair)
        .await
        .map_err(|e| format!("spawn_blocking(gen_keypair): {}", e))?;

    // Reconstruct Ya from first_byte + remaining
    let mut ya_bytes = vec![first_byte];
    ya_bytes.extend_from_slice(remaining_ya);
    if ya_bytes.len() < 96 {
        let mut more = vec![0u8; 96 - ya_bytes.len()];
        stream.read_exact(&mut more).await.map_err(|e| e.to_string())?;
        ya_bytes.extend_from_slice(&more);
    }

    // Send Yb
    stream.write_all(&yb_bytes).await.map_err(|e| e.to_string())?;

    // Compute shared secret (offload modpow to blocking pool).
    let s = {
        let ya_vec: Vec<u8> = ya_bytes[..96].to_vec();
        let xb_clone = xb.clone();
        tokio::task::spawn_blocking(move || compute_secret(&ya_vec, &xb_clone))
            .await
            .map_err(|e| format!("spawn_blocking(compute_secret): {}", e))?
    };

    // Read HASH('req1', S) - 20 bytes
    let mut req1_buf = [0u8; 20];
    // May need to skip PadA first — but we don't know its length.
    // Read bytes until we find req1 hash match.
    // Strategy: read 20 bytes at a time, check if it matches HASH('req1', S).
    // PadA is 0-512 bytes. We'll buffer and scan.
    let expected_req1 = sha1_combine(b"req1", &s);

    let mut scan_buf = Vec::with_capacity(512 + 20);
    // We may have already consumed Ya (96 bytes). PadA follows.
    // Read in chunks looking for req1 hash
    let mut found_req1 = false;
    let mut attempts = 0;
    while attempts < 600 {
        let mut byte = [0u8; 1];
        stream.read_exact(&mut byte).await.map_err(|e| e.to_string())?;
        scan_buf.push(byte[0]);
        attempts += 1;

        if scan_buf.len() >= 20 {
            let tail = &scan_buf[scan_buf.len() - 20..];
            if tail == expected_req1 {
                found_req1 = true;
                break;
            }
        }
    }

    if !found_req1 {
        return Err("could not find req1 hash in MSE handshake".into());
    }

    // Read HASH('req2', SKEY) XOR HASH('req3', S) — 20 bytes
    let mut req2_xor_buf = [0u8; 20];
    stream.read_exact(&mut req2_xor_buf).await.map_err(|e| e.to_string())?;

    // Compute req3 and recover HASH('req2', SKEY)
    let req3 = sha1_combine(b"req3", &s);
    let mut req2_hash = [0u8; 20];
    for i in 0..20 {
        req2_hash[i] = req2_xor_buf[i] ^ req3[i];
    }

    // Find matching SKEY (info_hash) by checking against known torrents
    let skey = info_hash_lookup(&req2_hash)
        .ok_or("no matching info_hash for MSE SKEY")?;

    // Create RC4 keys (reverse of initiator)
    // Receiver decrypts with key: SHA1('keyA', S, SKEY)
    // Receiver encrypts with key: SHA1('keyB', S, SKEY)
    let mut dec = make_rc4_keys(b"keyA", &s, &skey);
    let mut enc = make_rc4_keys(b"keyB", &s, &skey);

    // Read encrypted payload: VC + crypto_provide + len(padC) + padC + len(IA) + IA
    let mut vc_buf = [0u8; 8];
    stream.read_exact(&mut vc_buf).await.map_err(|e| e.to_string())?;
    dec.process(&mut vc_buf);
    // VC should be all zeros
    if vc_buf != VC {
        return Err("VC mismatch".into());
    }

    let mut cp_buf = [0u8; 4];
    stream.read_exact(&mut cp_buf).await.map_err(|e| e.to_string())?;
    dec.process(&mut cp_buf);
    let crypto_provide = u32::from_be_bytes(cp_buf);

    let mut pad_c_len_buf = [0u8; 2];
    stream.read_exact(&mut pad_c_len_buf).await.map_err(|e| e.to_string())?;
    dec.process(&mut pad_c_len_buf);
    let pad_c_len = u16::from_be_bytes(pad_c_len_buf) as usize;

    if pad_c_len > 0 {
        let mut pad_c = vec![0u8; pad_c_len];
        stream.read_exact(&mut pad_c).await.map_err(|e| e.to_string())?;
        dec.process(&mut pad_c);
    }

    let mut ia_len_buf = [0u8; 2];
    stream.read_exact(&mut ia_len_buf).await.map_err(|e| e.to_string())?;
    dec.process(&mut ia_len_buf);
    let ia_len = u16::from_be_bytes(ia_len_buf) as usize;

    // Read IA (Initial Application data = BT handshake)
    let mut ia = vec![0u8; ia_len];
    if ia_len > 0 {
        stream.read_exact(&mut ia).await.map_err(|e| e.to_string())?;
        dec.process(&mut ia);
    }

    // Select crypto method (prefer RC4)
    let crypto_select = if crypto_provide & CRYPTO_RC4 != 0 {
        CRYPTO_RC4
    } else {
        CRYPTO_PLAIN
    };

    // Send our response: encrypted(VC + crypto_select + len(padD=0) + padD)
    let mut response = Vec::new();
    response.extend_from_slice(&VC);
    response.extend_from_slice(&crypto_select.to_be_bytes());
    response.extend_from_slice(&0u16.to_be_bytes()); // padD = 0
    enc.process(&mut response);
    stream.write_all(&response).await.map_err(|e| e.to_string())?;

    // Send our BT handshake (encrypted)
    let bt_hs = build_bt_handshake(&skey, our_peer_id);
    let mut bt_hs_enc = bt_hs.clone();
    if crypto_select == CRYPTO_RC4 {
        enc.process(&mut bt_hs_enc);
    }
    stream.write_all(&bt_hs_enc).await.map_err(|e| e.to_string())?;

    // Parse IA as BT handshake
    let mut peer_id_out = [0u8; 20];
    let info_hash_out = skey;
    let mut fast_ext = false;
    let mut ext_proto = false;
    if ia.len() >= 68 && ia[0] == 19 && &ia[1..20] == b"BitTorrent protocol" {
        fast_ext = (ia[27] & 0x04) != 0;
        ext_proto = (ia[25] & 0x10) != 0;
        peer_id_out.copy_from_slice(&ia[48..68]);
    }

    Ok((enc, dec, MseHandshakeResult {
        peer_id: peer_id_out,
        info_hash: info_hash_out,
        fast_extension: fast_ext,
        extended_protocol: ext_proto,
    }))
}

fn build_bt_handshake(info_hash: &[u8; 20], peer_id: &[u8; 20]) -> Vec<u8> {
    let mut buf = Vec::with_capacity(68);
    buf.push(19u8);
    buf.extend_from_slice(b"BitTorrent protocol");
    let mut reserved = [0u8; 8];
    reserved[7] |= 0x04; // BEP 6 Fast Extension
    reserved[5] |= 0x10; // BEP 10 Extension Protocol
    buf.extend_from_slice(&reserved);
    buf.extend_from_slice(info_hash);
    buf.extend_from_slice(peer_id);
    buf
}
