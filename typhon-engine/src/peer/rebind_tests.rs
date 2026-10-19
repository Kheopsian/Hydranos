//! The listener supervisor's contract, on real sockets: a hot rebind moves
//! every listener or none, the old port is let go only once the new one is
//! held, and a failed bind leaves the engine listening where it was.

use super::*;
use crate::config::ResolvedBinding;

fn engine(tag: &str) -> (Arc<TorrentManager>, Arc<DiskManager>) {
    let root = std::env::temp_dir().join(format!("typhon-rebind-{tag}-{}", std::process::id()));
    let (data, resume) = (root.join("data"), root.join("resume"));
    std::fs::create_dir_all(&data).unwrap();
    std::fs::create_dir_all(&resume).unwrap();
    let disk = Arc::new(DiskManager::new(1));
    let mgr = Arc::new(TorrentManager::new(
        data.to_string_lossy().into_owned(),
        resume.to_string_lossy().into_owned(),
        disk.clone(),
    ));
    (mgr, disk)
}

/// A TCP port nobody holds right now.
fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port()
}

fn binding(id: u32, ip: &str, port: u16) -> ResolvedBinding {
    ResolvedBinding {
        id,
        addr: SocketAddr::new(ip.parse().unwrap(), port),
        peer_id: [id as u8; 20],
        egress: Default::default(),
        advertised_port: port,
        only_v6: false,
    }
}

/// Whether something accepts a TCP connection at `ip:port` now.
async fn accepts(ip: &str, port: u16) -> bool {
    tokio::net::TcpStream::connect((ip, port)).await.is_ok()
}

struct Listening {
    cur: Vec<ResolvedBinding>,
    handles: Vec<tokio::task::JoinHandle<()>>,
    utp_task: Option<tokio::task::JoinHandle<()>>,
    utp: UtpHandle,
    mgr: Arc<TorrentManager>,
    disk: Arc<DiskManager>,
}

impl Listening {
    /// An engine listening on `cur`, as `listen` leaves it after its first
    /// round.
    fn start(tag: &str, cur: Vec<ResolvedBinding>, utp: UtpHandle) -> Listening {
        let (mgr, disk) = engine(tag);
        let first = bind_round(&cur).expect("first round binds");
        let handles = spawn_tcp(first, &mgr, &disk, &utp);
        let utp_task = spawn_utp(&utp, &cur, cur[0].addr.port(), &mgr, &disk);
        Listening { cur, handles, utp_task, utp, mgr, disk }
    }

    async fn rebind(&mut self, port: u16) -> Result<u16, String> {
        rebind(port, &mut self.cur, &mut self.handles, &mut self.utp_task, &self.utp, &self.mgr, &self.disk).await
    }
}

/// Port 0 is not a port, and asking for the port already held is a success
/// that touches nothing: neither socket is closed and reopened.
#[tokio::test]
async fn a_rebind_to_port_zero_is_refused_and_to_the_current_port_is_a_no_op() {
    let was = free_port();
    let mut l = Listening::start("noop", vec![binding(1, "127.0.0.1", was)], UtpHandle::off());
    assert_eq!(l.rebind(0).await, Err("port 0 is not a port".to_string()));
    assert_eq!(l.rebind(was).await, Ok(was));
    assert!(!l.handles[0].is_finished(), "the accept loop was disturbed");
    assert!(accepts("127.0.0.1", was).await, "nothing listens on {was} any more");
}

/// ⭐ The new sockets are bound BEFORE the old ones go. A port already in use
/// is refused with the old port named, and the engine goes on listening on it:
/// 4.3 aborted first and bound after, leaving no listener for good.
#[tokio::test]
async fn a_rebind_onto_a_busy_port_keeps_the_old_listener() {
    let was = free_port();
    let mut l = Listening::start("busy", vec![binding(1, "127.0.0.1", was)], UtpHandle::off());
    let squatter = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let busy = squatter.local_addr().unwrap().port();

    let err = l.rebind(busy).await.expect_err("the port is held by someone else");
    assert!(err.contains(&format!("still listening on {was}")), "{err}");
    // An accept loop that was aborted stops at its next poll: give it one.
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(l.cur[0].addr.port(), was, "the live binding moved anyway");
    assert_eq!(l.cur[0].advertised_port, was);
    assert!(!l.handles[0].is_finished(), "the old accept loop was stopped");
    assert!(accepts("127.0.0.1", was).await, "the old port stopped accepting");
}

/// A successful rebind moves EVERY binding -- port and advertised port both --
/// and the old port is really let go.
#[tokio::test]
async fn a_rebind_moves_every_binding_and_frees_the_old_port() {
    let was = free_port();
    let mut l = Listening::start(
        "move",
        vec![binding(1, "127.0.0.1", was), binding(2, "127.0.0.2", was)],
        UtpHandle::off(),
    );
    let now = free_port();
    assert_eq!(l.rebind(now).await, Ok(now));
    for b in &l.cur {
        assert_eq!((b.addr.port(), b.advertised_port), (now, now), "binding {} did not move", b.id);
    }
    assert_eq!(l.handles.len(), 2, "one accept loop per binding");
    assert!(accepts("127.0.0.1", now).await && accepts("127.0.0.2", now).await, "the new port does not accept");
    assert!(!accepts("127.0.0.1", was).await, "the old port still accepts");
    assert!(!accepts("127.0.0.2", was).await, "the second binding's old port still accepts");
}

/// A round is all or nothing: when the second binding cannot be bound, the
/// first one is not left holding its socket.
#[tokio::test]
async fn a_round_that_cannot_bind_one_binding_holds_none() {
    let squatter = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let busy = squatter.local_addr().unwrap().port();
    let free = free_port();
    let err = bind_round(&[binding(1, "127.0.0.1", free), binding(2, "127.0.0.1", busy)])
        .err()
        .expect("the second binding is busy");
    assert!(err.contains(&format!("cannot bind 127.0.0.1:{busy}")), "{err}");
    std::net::TcpListener::bind(("127.0.0.1", free)).expect("the first binding's socket was kept after the failure");
}

/// An `enable_ipv6` listener sits BESIDE the v4 one, so it must not also take
/// v4: a v4 peer would arrive as `::ffff:a.b.c.d` and stop matching every
/// address compared downstream.
#[tokio::test]
async fn an_only_v6_listener_does_not_take_v4() {
    if std::net::TcpListener::bind("[::1]:0").is_err() {
        eprintln!("no IPv6 on this host: the only_v6 listener is not testable here");
        return;
    }
    let mut b = binding(1, "::", 0);
    b.only_v6 = true;
    let round = bind_round(&[b]).expect("a v6 wildcard binds");
    let port = round[0].0.local_addr().unwrap().port();
    assert!(accepts("::1", port).await, "the v6 listener does not accept v6");
    assert!(!accepts("127.0.0.1", port).await, "the v6-only listener took a v4 connection");
}

/// uTP follows the TCP port: the socket is replaced by one on the new port,
/// the old UDP port is released, and the handle gives out the new one.
#[tokio::test]
async fn a_rebind_moves_the_utp_socket_with_the_tcp_listeners() {
    let was = free_port();
    let utp = UtpHandle::bind(0, None).await.expect("a uTP socket");
    let old_udp = utp.get().unwrap().bind_addr().port();
    let mut l = Listening::start("utp-move", vec![binding(1, "127.0.0.1", was)], utp);
    let now = free_port();
    assert_eq!(l.rebind(now).await, Ok(now));
    assert_eq!(l.utp.get().unwrap().bind_addr().port(), now, "dials after the rebind leave from the old uTP port");
    assert!(l.utp_task.as_ref().is_some_and(|t| !t.is_finished()), "no uTP accept loop on the new socket");
    // The old socket is cancelled, not just dropped: its port frees up.
    let mut freed = false;
    for _ in 0..40 {
        if std::net::UdpSocket::bind(("0.0.0.0", old_udp)).is_ok() {
            freed = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(freed, "the old uTP port {old_udp} is still bound");
}

/// ⭐ All or nothing across TCP and uTP: the TCP port is free but the UDP one
/// is not, so the rebind fails -- and the TCP sockets bound for it are given
/// back, the old ones kept, the uTP socket unchanged.
#[tokio::test]
async fn a_rebind_whose_utp_port_is_busy_changes_nothing() {
    let was = free_port();
    let utp = UtpHandle::bind(0, None).await.expect("a uTP socket");
    let udp_before = utp.get().unwrap().bind_addr().port();
    let mut l = Listening::start("utp-busy", vec![binding(1, "127.0.0.1", was)], utp);
    let target = free_port();
    let _udp_squatter = std::net::UdpSocket::bind(("0.0.0.0", target)).expect("hold the UDP port");

    let err = l.rebind(target).await.expect_err("the uTP port is held");
    assert!(err.contains("uTP socket") && err.contains(&format!("still listening on {was}")), "{err}");
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(l.cur[0].addr.port(), was);
    assert!(accepts("127.0.0.1", was).await, "the old TCP listener is gone");
    assert!(!accepts("127.0.0.1", target).await, "the new TCP socket was left bound after the failure");
    assert_eq!(l.utp.get().unwrap().bind_addr().port(), udp_before, "the uTP socket moved");
}

/// An engine without uTP moves TCP only, and stays without it.
#[tokio::test]
async fn an_engine_without_utp_rebinds_tcp_only() {
    let was = free_port();
    let mut l = Listening::start("no-utp", vec![binding(1, "127.0.0.1", was)], UtpHandle::off());
    let now = free_port();
    assert_eq!(l.rebind(now).await, Ok(now));
    assert!(l.utp.get().is_none() && l.utp_task.is_none(), "a uTP socket appeared");
    assert!(accepts("127.0.0.1", now).await);
}

/// After a failed accept the wait doubles from 10 ms to a second, then stays
/// there. Virtual time: the sleep is read off the clock, not waited out.
#[tokio::test(start_paused = true)]
async fn the_accept_backoff_doubles_from_ten_milliseconds_to_a_second() {
    let e = std::io::Error::from_raw_os_error(24);
    let mut failures = 0u32;
    let mut waits = Vec::new();
    for _ in 0..10 {
        let t0 = tokio::time::Instant::now();
        accept_backoff("tcp", &e, &mut failures).await;
        waits.push(t0.elapsed().as_millis() as u64);
    }
    assert_eq!(waits, [10, 20, 40, 80, 160, 320, 640, 1000, 1000, 1000]);
    assert_eq!(failures, 10);
    // A success resets the counter in the loop; the backoff itself never
    // overflows however long the condition lasts.
    let mut failures = u32::MAX;
    accept_backoff("tcp", &e, &mut failures).await;
    assert_eq!(failures, u32::MAX);
}
