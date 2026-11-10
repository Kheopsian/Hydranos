#![no_main]
use bytes::BytesMut;
use libfuzzer_sys::fuzz_target;
use tokio_util::codec::Decoder;
use typhon_engine::peer::message::Message;
use typhon_engine::wire::codec::BtCodec;

// A byte stream as a peer would send it, fed to the length-prefixed codec until
// it wants more or refuses; plus the id-and-payload decoder on its own, with
// the first byte as the id.
fuzz_target!(|data: &[u8]| {
    let mut codec = BtCodec::new();
    let mut src = BytesMut::from(data);
    while let Ok(Some(_)) = codec.decode(&mut src) {}

    if let Some((&id, rest)) = data.split_first() {
        let mut payload = BytesMut::from(rest);
        let _ = Message::decode(id, &mut payload);
    }
});
