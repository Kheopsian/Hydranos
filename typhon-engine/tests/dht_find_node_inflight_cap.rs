// Regression test: the find_node recursion of the vendored librqbit-dht must
// bound its in-flight requests (as the get_peers path does with
// MAX_INFLIGHT_REQUESTS = 256), however many nodes one peer's replies name.
//
// Every find_node future is wrapped in one `find_node` tracing span, so
// counting live spans counts the futures in flight, through the public API only.

use librqbit_dht::{DhtConfig, DhtState, Id20};
use std::collections::HashMap;
use std::net::{SocketAddr, UdpSocket};
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

// Every `find_node` future of `find_node_for_routing_table` is wrapped in one
// `error_span!("find_node", ..)` (third_party/librqbit-dht/src/dht.rs:283),
// created when the future is pushed and dropped when it completes. Counting
// those spans counts the futures in the unbounded FuturesUnordered, without
// touching a line of the crate.
static LIVE: AtomicI64 = AtomicI64::new(0);
static TOTAL: AtomicU64 = AtomicU64::new(0);
static NEXT: AtomicU64 = AtomicU64::new(1);
static REFS: Mutex<Option<HashMap<u64, usize>>> = Mutex::new(None);

// Live heap bytes of the test process, so growth is told apart from allocator
// fragmentation. The library defines no allocator of its own.
struct Counting;
static HEAP: AtomicI64 = AtomicI64::new(0);
unsafe impl std::alloc::GlobalAlloc for Counting {
    unsafe fn alloc(&self, l: std::alloc::Layout) -> *mut u8 {
        HEAP.fetch_add(l.size() as i64, Ordering::Relaxed);
        unsafe { std::alloc::System.alloc(l) }
    }
    unsafe fn dealloc(&self, p: *mut u8, l: std::alloc::Layout) {
        HEAP.fetch_sub(l.size() as i64, Ordering::Relaxed);
        unsafe { std::alloc::System.dealloc(p, l) }
    }
}
#[global_allocator]
static ALLOC: Counting = Counting;

fn heap_mb() -> i64 {
    HEAP.load(Ordering::Relaxed) / (1 << 20)
}

struct FindNodeSpans;

impl tracing::Subscriber for FindNodeSpans {
    fn enabled(&self, m: &tracing::Metadata<'_>) -> bool {
        m.is_span() && m.name() == "find_node"
    }
    fn new_span(&self, _: &tracing::span::Attributes<'_>) -> tracing::span::Id {
        let id = NEXT.fetch_add(1, Ordering::Relaxed);
        REFS.lock().unwrap().get_or_insert_with(HashMap::new).insert(id, 1);
        LIVE.fetch_add(1, Ordering::Relaxed);
        TOTAL.fetch_add(1, Ordering::Relaxed);
        tracing::span::Id::from_u64(id)
    }
    fn record(&self, _: &tracing::span::Id, _: &tracing::span::Record<'_>) {}
    fn record_follows_from(&self, _: &tracing::span::Id, _: &tracing::span::Id) {}
    fn event(&self, _: &tracing::Event<'_>) {}
    fn enter(&self, _: &tracing::span::Id) {}
    fn exit(&self, _: &tracing::span::Id) {}
    fn clone_span(&self, id: &tracing::span::Id) -> tracing::span::Id {
        if let Some(n) = REFS
            .lock()
            .unwrap()
            .get_or_insert_with(HashMap::new)
            .get_mut(&id.into_u64())
        {
            *n += 1;
        }
        id.clone()
    }
    fn try_close(&self, id: tracing::span::Id) -> bool {
        let mut g = REFS.lock().unwrap();
        let refs = g.get_or_insert_with(HashMap::new);
        let k = id.into_u64();
        match refs.get_mut(&k) {
            Some(n) if *n > 1 => {
                *n -= 1;
                false
            }
            Some(_) => {
                refs.remove(&k);
                LIVE.fetch_sub(1, Ordering::Relaxed);
                true
            }
            None => false,
        }
    }
}

/// A scripted DHT node on loopback.
struct Fake {
    addr: SocketAddr,
    nodes_per_reply: usize,
    answering: AtomicBool,
    stop: AtomicBool,
    queries: AtomicU64,
    find_node: AtomicU64,
    get_peers: AtomicU64,
    replies: AtomicU64,
    nodes_sent: AtomicU64,
    biggest_reply: AtomicU64,
}

fn find(hay: &[u8], needle: &[u8]) -> Option<usize> {
    hay.windows(needle.len()).rposition(|w| w == needle)
}

impl Fake {
    fn start(nodes_per_reply: usize) -> Arc<Fake> {
        let sock = UdpSocket::bind("127.0.0.1:0").unwrap();
        sock.set_read_timeout(Some(Duration::from_millis(100))).unwrap();
        let me = Arc::new(Fake {
            addr: sock.local_addr().unwrap(),
            nodes_per_reply,
            answering: AtomicBool::new(true),
            stop: AtomicBool::new(false),
            queries: AtomicU64::new(0),
            find_node: AtomicU64::new(0),
            get_peers: AtomicU64::new(0),
            replies: AtomicU64::new(0),
            nodes_sent: AtomicU64::new(0),
            biggest_reply: AtomicU64::new(0),
        });
        let f = me.clone();
        std::thread::spawn(move || f.serve(sock));
        me
    }

    fn serve(&self, sock: UdpSocket) {
        let port = self.addr.port().to_be_bytes();
        let mut buf = [0u8; 2048];
        // Each id handed out is closer to the target than every one before it,
        // so `should_request_node` never sorts it last (dht.rs:538-543).
        let mut closer = u64::MAX;
        while !self.stop.load(Ordering::Relaxed) {
            let Ok((n, from)) = sock.recv_from(&mut buf) else {
                continue;
            };
            let q = &buf[..n];
            self.queries.fetch_add(1, Ordering::Relaxed);
            let is_find = find(q, b"9:find_node").is_some();
            if is_find {
                self.find_node.fetch_add(1, Ordering::Relaxed);
            } else if find(q, b"9:get_peers").is_some() {
                self.get_peers.fetch_add(1, Ordering::Relaxed);
            }
            if !self.answering.load(Ordering::Relaxed) {
                continue;
            }
            let Some(t) = find(q, b"1:t2:") else { continue };
            let tid = &q[t + 5..t + 7];
            let mut nodes = Vec::new();
            if is_find {
                let Some(p) = find(q, b"6:target20:") else { continue };
                let target = &q[p + 11..p + 31];
                for _ in 0..self.nodes_per_reply {
                    let mut id = [0u8; 20];
                    id.copy_from_slice(target);
                    if self.nodes_per_reply > 1 {
                        closer -= 1;
                        for (b, d) in id[12..].iter_mut().zip(closer.to_be_bytes()) {
                            *b ^= d;
                        }
                    } else {
                        id[19] ^= 1;
                    }
                    nodes.extend_from_slice(&id);
                    nodes.extend_from_slice(&[127, 0, 0, 1]);
                    nodes.extend_from_slice(&port);
                }
                self.nodes_sent.fetch_add(self.nodes_per_reply as u64, Ordering::Relaxed);
            }
            let mut r = b"d1:rd2:id20:".to_vec();
            r.extend_from_slice(&[0xEE; 20]);
            r.extend_from_slice(format!("5:nodes{}:", nodes.len()).as_bytes());
            r.extend_from_slice(&nodes);
            r.extend_from_slice(b"e1:t2:");
            r.extend_from_slice(tid);
            r.extend_from_slice(b"1:y1:re");
            self.biggest_reply.fetch_max(r.len() as u64, Ordering::Relaxed);
            if sock.send_to(&r, from).is_ok() {
                self.replies.fetch_add(1, Ordering::Relaxed);
            }
        }
    }
}

async fn victim(bootstrap: SocketAddr) -> Arc<DhtState> {
    DhtState::with_config(DhtConfig {
        listen_addr: Some("127.0.0.1:0".parse().unwrap()),
        bootstrap_addrs: Some(vec![bootstrap.to_string()]),
        ..Default::default()
    })
    .await
    .unwrap()
}

/// Generous bound, comparable to MAX_INFLIGHT_REQUESTS (256).
const INFLIGHT_BOUND: i64 = 512;
/// A legitimate lookup must reach the wire within this long, flood or not
/// (an unattacked node does it in milliseconds).
const LOOKUP_BOUND: Duration = Duration::from_secs(2);

async fn get_peers_latency(dht: &Arc<DhtState>, fake: &Fake, cap: Duration) -> Option<Duration> {
    let before = fake.get_peers.load(Ordering::Relaxed);
    let t0 = Instant::now();
    let _stream = dht.get_peers(Id20::new([0x42; 20]), None);
    while t0.elapsed() < cap {
        if fake.get_peers.load(Ordering::Relaxed) > before {
            return Some(t0.elapsed());
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    None
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn find_node_recursion_keeps_inflight_requests_bounded() {
    tracing::subscriber::set_global_default(FindNodeSpans)
        .expect("run this test alone: it needs the global tracing subscriber");

    // One node answers every find_node with 620 ever-closer nodes.
    let evil = Fake::start(620);
    let dht = victim(evil.addr).await;
    let mut peak = 0i64;
    for _ in 0..200 {
        tokio::time::sleep(Duration::from_millis(100)).await;
        peak = peak.max(LIVE.load(Ordering::Relaxed));
        if peak > 5_000 {
            break; // uncapped: stop before memory grows further
        }
    }
    println!(
        "peak pending find_node futures={peak}, created={}, replies={}, heap={} MiB",
        TOTAL.load(Ordering::Relaxed),
        evil.replies.load(Ordering::Relaxed),
        heap_mb()
    );
    assert!(
        evil.replies.load(Ordering::Relaxed) > 5,
        "the flood must actually have been delivered"
    );
    assert!(
        TOTAL.load(Ordering::Relaxed) > 256,
        "the lookup must keep making progress (requests are issued as others drain)"
    );
    assert!(
        peak <= INFLIGHT_BOUND,
        "find_node recursion held {peak} futures in flight (bound {INFLIGHT_BOUND})"
    );
    // A legitimate lookup while the flood is still going on.
    let latency = get_peers_latency(&dht, &evil, Duration::from_secs(20)).await;
    println!("get_peers during the flood: first query on the wire after {latency:?}");
    assert!(
        latency.is_some_and(|l| l <= LOOKUP_BOUND),
        "legitimate lookup starved under the find_node flood: {latency:?} (bound {LOOKUP_BOUND:?})"
    );
    dht.cancellation_token().cancel();
    evil.stop.store(true, Ordering::Relaxed);
}
