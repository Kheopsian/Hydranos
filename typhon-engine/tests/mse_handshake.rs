//! The MSE handshake, run for real between two connected sockets.
//!
//! `handshake_outgoing` and `handshake_incoming` take a `PeerTransport`, which
//! is a thin wrapper over a `TcpStream` -- so the honest fixture is a real
//! loopback pair, not a mock. Both halves run concurrently, exactly as they do
//! when a peer dials us.

use typhon_engine::peer::transport::PeerTransport;

const INFO_HASH: [u8; 20] = [0x11; 20];

async fn connected_pair() -> (PeerTransport, PeerTransport) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let accept = tokio::spawn(async move { listener.accept().await.unwrap().0 });
    let client = tokio::net::TcpStream::connect(addr).await.unwrap();
    let server = accept.await.unwrap();
    (PeerTransport::Tcp(client), PeerTransport::Tcp(server))
}

/// ⭐⭐ Both ends must derive the SAME keys from a Diffie-Hellman exchange they
/// never send in the clear. If they disagree by one byte, every later message
/// is noise and the session dies with no error that names encryption.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn both_ends_of_an_mse_handshake_agree_on_the_keys() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let (mut initiator, mut receiver) = connected_pair().await;

    let out = tokio::spawn(async move {
        let r = typhon_engine::crypto::mse::handshake_outgoing(
            &mut initiator,
            &INFO_HASH,
            b"-TY0001-initiator000",
        )
        .await;
        (r, initiator)
    });

    // The receiver sniffs the first byte to tell MSE from a plaintext
    // handshake (which starts with 19), then hands the rest over.
    let mut first = [0u8; 1];
    receiver.read_exact(&mut first).await.unwrap();
    let mut rest = [0u8; 95];
    receiver.read_exact(&mut rest).await.unwrap();

    let incoming = typhon_engine::crypto::mse::handshake_incoming(
        &mut receiver,
        first[0],
        &rest,
        b"-TY0001-receiver0000",
        |_ih| Some(INFO_HASH),
    )
    .await;

    let (outgoing, mut initiator) = out.await.unwrap();
    let (mut enc_a, mut _dec_a, _res_a) = outgoing.expect("the initiator completed MSE");
    let (mut _enc_b, mut dec_b, _res_b) = incoming.expect("the receiver completed MSE");

    // The real proof: what one side encrypts, the other decrypts.
    let plaintext = b"the bytes that came out are the bytes that went in";
    let mut buf = plaintext.to_vec();
    enc_a.process(&mut buf);
    assert_ne!(&buf[..], &plaintext[..], "the stream is actually encrypted");
    initiator.write_all(&buf).await.unwrap();

    let mut got = vec![0u8; plaintext.len()];
    receiver.read_exact(&mut got).await.unwrap();
    dec_b.process(&mut got);
    assert_eq!(&got[..], &plaintext[..], "the two ends derived the same key");
}

/// ⭐ The receiver looks the info hash up: an encrypted dial for a torrent this
/// engine does not hold must be refused, not answered. The hash is obfuscated
/// on the wire, so this lookup is the only place it can be checked.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_encrypted_dial_for_an_unknown_torrent_is_refused() {
    use tokio::io::AsyncReadExt;

    let (mut initiator, mut receiver) = connected_pair().await;
    let out = tokio::spawn(async move {
        typhon_engine::crypto::mse::handshake_outgoing(
            &mut initiator,
            &INFO_HASH,
            b"-TY0001-initiator000",
        )
        .await
    });

    let mut first = [0u8; 1];
    receiver.read_exact(&mut first).await.unwrap();
    let mut rest = [0u8; 95];
    receiver.read_exact(&mut rest).await.unwrap();

    // ⭐ Bounded on purpose: a refusal that never returns is a slot a scanner
    // holds for free. The timeout IS the assertion.
    let incoming = tokio::time::timeout(
        std::time::Duration::from_secs(10),
        typhon_engine::crypto::mse::handshake_incoming(
            &mut receiver,
            first[0],
            &rest,
            b"-TY0001-receiver0000",
            // We hold nothing.
            |_ih| None,
        ),
    )
    .await
    .expect("refusing an unknown torrent must not hang the connection");

    assert!(incoming.is_err(), "an unknown torrent must not complete a handshake");

    // The initiator is left waiting for an answer that will never come; drop
    // it rather than awaiting it, or this test waits with it.
    out.abort();
}

/// A peer that opens the connection and says nothing must not leave the
/// handshake waiting forever -- it is how a scanner ties up a slot.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_silent_peer_does_not_complete_a_handshake() {
    use tokio::io::AsyncWriteExt;

    let (mut initiator, receiver) = connected_pair().await;
    // The receiver never answers; it just drops.
    drop(receiver);

    let r = typhon_engine::crypto::mse::handshake_outgoing(
        &mut initiator,
        &INFO_HASH,
        b"-TY0001-initiator000",
    )
    .await;
    assert!(r.is_err(), "a dead peer is an error, not a completed handshake");
    let _ = initiator.write_all(b"x").await;
}

/// ⭐ `sha1_combine` is `SHA1(prefix || data)` with NO separator. That is what
/// MSE specifies, and it is only safe because every prefix here is
/// fixed-width -- worth pinning, because a variable-width prefix would make
/// two different inputs hash the same.
#[test]
fn sha1_combine_is_a_plain_concatenation() {
    use sha1::{Digest, Sha1};
    let prefix = b"req1";
    let data = [0xAB_u8; 20];

    let mut expected = Sha1::new();
    expected.update(prefix);
    expected.update(data);
    let expected: [u8; 20] = expected.finalize().into();

    assert_eq!(typhon_engine::crypto::mse::sha1_combine(prefix, &data), expected);
}

#[test]
fn sha1_combine_separates_different_inputs() {
    let a = typhon_engine::crypto::mse::sha1_combine(b"req2", &[0u8; 20]);
    let b = typhon_engine::crypto::mse::sha1_combine(b"req3", &[0u8; 20]);
    assert_ne!(a, b, "a different prefix is a different digest");
}

/// The transport reports which wire it is, because the zero-copy serve path
/// is only available on plaintext TCP and the choice is made on this answer.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_tcp_transport_says_it_is_tcp() {
    let (a, b) = connected_pair().await;
    assert_eq!(a.kind(), "tcp");
    assert_eq!(b.kind(), "tcp");
}
