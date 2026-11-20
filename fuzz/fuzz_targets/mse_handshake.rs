#![no_main]
use libfuzzer_sys::fuzz_target;
use std::sync::OnceLock;
use typhon_engine::crypto::mse::sha1_combine;
use typhon_engine::fuzz_api::{mse_incoming, mse_outgoing, mse_public, mse_rc4, mse_secret};

// The MSE / PE handshake (BEP 8), both sides, against a peer we play.
//
// The handshake makes a random key pair per connection, so raw bytes could
// never carry a valid `req1` hash or a tail the RC4 stream decrypts: the
// fuzzer would never get past the key exchange. The private keys are fixed
// instead (`fuzz_api` takes them as seeds), and this harness, knowing both,
// builds the wire as the real peer would:
//
//   byte 0      selector: bit 0 = we initiate, bit 1 = RC4 only, bit 2 = raw
//   bytes 1..3  length of the padding after the public key (read as u16)
//   then        the padding, then the PLAINTEXT of the encrypted part
//               (VC, crypto_provide/select, padC/padD length + pad, IA / the
//               BitTorrent handshake), which is encrypted here with the
//               session key. Raw: no keys and no encryption, the rest of the
//               input is the stream as it is.
const SEED_THEM: [u8; 20] = [0x11; 20];
const SEED_US: [u8; 20] = [0x22; 20];
const SKEY: [u8; 20] = [0x33; 20];

struct Fixture {
    ya: Vec<u8>,
    yb: Vec<u8>,
    secret: Vec<u8>,
}

fn fixture() -> &'static Fixture {
    static F: OnceLock<Fixture> = OnceLock::new();
    F.get_or_init(|| {
        let ya = mse_public(&SEED_THEM);
        let yb = mse_public(&SEED_US);
        let secret = mse_secret(&ya, &SEED_US);
        Fixture { ya, yb, secret }
    })
}

fuzz_target!(|data: &[u8]| {
    if data.len() < 3 {
        return;
    }
    let f = fixture();
    let (sel, rest) = (data[0], &data[3..]);
    let initiate = sel & 1 != 0;
    let rc4_only = sel & 2 != 0;
    let raw = sel & 4 != 0;
    let pad_len = (u16::from_be_bytes([data[1], data[2]]) as usize % 600).min(rest.len());
    let (pad, tail) = rest.split_at(pad_len);

    // The two ends use mirrored keys: A encrypts with keyA, B with keyB.
    let mut wire = Vec::new();
    if raw {
        wire.extend_from_slice(rest);
    } else if initiate {
        // We are A and read B's answer: Yb, PadB, ENCRYPT(VC, ...) with keyB.
        wire.extend_from_slice(&f.yb);
        wire.extend_from_slice(pad);
        let mut enc = tail.to_vec();
        mse_rc4(b"keyB", &f.secret, &SKEY).process(&mut enc);
        wire.extend_from_slice(&enc);
    } else {
        // We are B and read A's opening after Ya: PadA, req1, req2^req3, and
        // ENCRYPT(VC, ...) with keyA.
        wire.extend_from_slice(pad);
        wire.extend_from_slice(&sha1_combine(b"req1", &f.secret));
        let req2 = sha1_combine(b"req2", &SKEY);
        let req3 = sha1_combine(b"req3", &f.secret);
        wire.extend(req2.iter().zip(req3.iter()).map(|(a, b)| a ^ b));
        let mut enc = tail.to_vec();
        mse_rc4(b"keyA", &f.secret, &SKEY).process(&mut enc);
        wire.extend_from_slice(&enc);
    }

    // The handshake spawns its modpow on the blocking pool: a runtime is needed.
    thread_local! {
        static RT: tokio::runtime::Runtime = tokio::runtime::Builder::new_current_thread()
            .build()
            .expect("runtime");
    }
    RT.with(|rt| {
        rt.block_on(async {
            if initiate {
                mse_outgoing(&wire, &SEED_THEM, SKEY, rc4_only).await;
            } else {
                mse_incoming(&wire, f.ya[0], &f.ya[1..], &SEED_US, SKEY, rc4_only).await;
            }
        })
    });
});
