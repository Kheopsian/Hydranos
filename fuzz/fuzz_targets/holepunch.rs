#![no_main]
use libfuzzer_sys::fuzz_target;
use typhon_engine::peer::holepunch::{answer_rendezvous, is_punchable, Punch};

// A BEP 55 `ut_holepunch` message from a stranger. What decodes encodes back to
// a message that decodes to the same thing, and whatever address it names goes
// through the rendezvous answer (which must never aim a dial at a private one).
fuzz_target!(|data: &[u8]| {
    let Some(p) = Punch::decode(data) else {
        return;
    };
    assert_eq!(Punch::decode(&p.encode()), Some(p.clone()));
    let addr = match p {
        Punch::Rendezvous(a) | Punch::Connect(a) | Punch::Error(a, _) => a,
    };
    let asker = "192.0.2.1:6881".parse().unwrap();
    if let Punch::Connect(to) = answer_rendezvous(asker, addr, true, true) {
        assert!(is_punchable(&to));
    }
});
