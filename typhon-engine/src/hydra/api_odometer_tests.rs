use super::testing::*;
use super::*;
use std::sync::atomic::Ordering;

const KEY: &str = "0123456789abcdef0123456789abcdef";

fn torrent_bytes(name: &str) -> Vec<u8> {
    let mut info = Vec::new();
    info.extend_from_slice(format!("d6:lengthi16384e4:name{}:{name}", name.len()).as_bytes());
    info.extend_from_slice(b"12:piece lengthi16384e6:pieces20:");
    let mut piece = [0xCDu8; 20];
    piece[0] = name.as_bytes()[0];
    piece[1] = name.len() as u8;
    info.extend_from_slice(&piece);
    info.push(b'e');
    let announce = "https://tracker.example/announce";
    let mut out = Vec::new();
    out.extend_from_slice(format!("d8:announce{}:{announce}4:info", announce.len()).as_bytes());
    out.extend_from_slice(&info);
    out.push(b'e');
    out
}

fn add(s: &TestState, engine_id: &str, name: &str) -> [u8; 20] {
    let engine = s
        .engines
        .get(engine_id)
        .unwrap_or_else(|| panic!("no engine {engine_id}"));
    engine
        .manager
        .add_torrent_bytes(&torrent_bytes(name), "/tmp", true, true)
        .unwrap_or_else(|e| panic!("add {name}: {e}"))
        .0
}

/// The torrent was loaded with `ul` lifetime bytes from its resume
/// record: history that predates this process.
fn load_lifetime(s: &TestState, engine_id: &str, ih: &[u8; 20], ul: u64) {
    let t = s.engines.get(engine_id).unwrap().manager.get(ih).unwrap();
    t.restore_lifetime(ul, 0);
}

/// What main.rs does once the engines have loaded.
fn boot(s: &TestState) {
    *s.odometer.lock().unwrap() = Odometer::at_boot();
}

fn upload(s: &TestState, engine_id: &str, ih: &[u8; 20], n: u64) {
    s.engines
        .get(engine_id)
        .unwrap()
        .manager
        .get(ih)
        .unwrap()
        .total_uploaded
        .fetch_add(n, Ordering::Relaxed);
}

fn tick(s: &TestState) {
    for e in s.engines.engines() {
        e.manager.update_rates();
    }
}

#[tokio::test]
async fn daily_traffic_checkpoint_survives_reopening_the_database() {
    // A fixed date, not the clock: a run across midnight must not roll the
    // day between the save and the restore.
    const DAY: &str = "2026-11-15";
    fn day() -> String {
        DAY.to_string()
    }
    let boot = |s: &TestState| {
        *s.odometer.lock().unwrap() = Odometer {
            day_baseline: (0, 0),
            day_date: DAY.into(),
        };
    };
    let today_of = |s: &TestState| {
        let session = s.engines.moved_totals();
        (session, s.odometer.lock().unwrap().day(session, DAY))
    };

    let mut first = state_from("odo-persist-first", "");
    let path = first.dir.join("daily.db");
    let store = crate::store::Store::open(&path, false).unwrap();
    store.ensure_schema().unwrap();
    first.state.store = Arc::new(crate::store::StoreLock::new(store));
    boot(&first);
    let ih = add(&first, "hoard", "first-session");
    upload(&first, "hoard", &ih, 700);
    first
        .engines
        .get("hoard")
        .unwrap()
        .manager
        .get(&ih)
        .unwrap()
        .total_downloaded
        .fetch_add(10, Ordering::Relaxed);
    tick(&first);
    save_daily_traffic_on(first.state.clone(), day)
        .await
        .unwrap();

    let mut second = state_from("odo-persist-second", "");
    second.state.store = Arc::new(crate::store::StoreLock::new(
        crate::store::Store::open(&path, false).unwrap(),
    ));
    boot(&second);
    let saved = second
        .store
        .lock()
        .unwrap()
        .setting(DAILY_TRAFFIC_KEY)
        .unwrap()
        .unwrap();
    second.odometer.lock().unwrap().restore(&saved).unwrap();
    let (session, today) = today_of(&second);
    assert_eq!(session, (0, 0));
    assert_eq!(today, (700, 10));
    let ih = add(&second, "hoard", "second-session");
    upload(&second, "hoard", &ih, 20);
    tick(&second);
    assert_eq!(today_of(&second).1, (720, 10));
    save_daily_traffic_on(second.state.clone(), day)
        .await
        .unwrap();
    let saved = first
        .store
        .lock()
        .unwrap()
        .setting(DAILY_TRAFFIC_KEY)
        .unwrap()
        .unwrap();
    let mut third = Odometer {
        day_baseline: (0, 0),
        day_date: DAY.into(),
    };
    third.restore(&saved).unwrap();
    assert_eq!(third.day((0, 0), DAY), (720, 10));
}

/// The 01/10/2026 incident, as an interleaving: deleting 1,813 torrents
/// pushed "today" and "this session" up by about their lifetime bytes,
/// because a poll between the engine removal and the next 1 Hz cache
/// refresh saw the fall of the totals after `forget` had already paid for
/// it. Failed on the old odometer with session = 1,000,300 at the first
/// poll after the delete.
#[tokio::test]
async fn a_poll_between_a_delete_and_the_next_tick_leaves_the_session_alone() {
    let s = state_from(
        "odo-delete-race",
        &format!("[daemon]\napi_key = \"{KEY}\"\n"),
    );
    let old = add(&s, "hoard", "old");
    let busy = add(&s, "hoard", "busy");
    load_lifetime(&s, "hoard", &old, 1_000_000);
    tick(&s);
    boot(&s);
    // main.rs's 60 s roll timer polls once at startup: that opens the day.
    let _ = session_and_day(&s.state);

    upload(&s, "hoard", &busy, 300);
    tick(&s);
    let (_, session, day) = session_and_day(&s.state);
    assert_eq!(session.0, 300);
    assert_eq!(day.0, 300);

    let hash = typhon_engine::torrent::hex_encode(&old);
    remove_one_torrent(&s.state, &hash, &["hoard".to_string()], "hoard", false).expect("removed");

    // The poll lands before the 1 Hz cache refresh.
    let (_, session, day) = session_and_day(&s.state);
    assert_eq!(session.0, 300, "a delete is not traffic (stale cache)");
    assert_eq!(day.0, 300);
    assert_eq!(engine_session(&s.state, "hoard").0, 300);

    tick(&s);
    let (_, session, day) = session_and_day(&s.state);
    assert_eq!(
        session.0, 300,
        "a delete is not traffic (after the refresh)"
    );
    assert_eq!(day.0, 300);
    assert_eq!(engine_session(&s.state, "hoard").0, 300);
}

/// Re-adding a torrent that carries lifetime counters -- the reload path,
/// a move between engines -- adds nothing to the session, and the session
/// of each engine is its own.
#[tokio::test]
async fn a_moved_torrent_brings_its_history_but_not_into_the_session() {
    let s = state_from("odo-move", &format!("[daemon]\napi_key = \"{KEY}\"\n"));
    let ih = add(&s, "hoard", "mover");
    load_lifetime(&s, "hoard", &ih, 5_000_000);
    tick(&s);
    boot(&s);
    upload(&s, "hoard", &ih, 40);
    tick(&s);
    assert_eq!(session_and_day(&s.state).1, (40, 0));

    // What an engine move does: export, import on the other side, let go.
    let hoard = &s.engines.get("hoard").unwrap().manager;
    let race = &s.engines.get("race").unwrap().manager;
    let blob = torrent_bytes("mover");
    race.set_blob_source(Arc::new(move |_h: &str| Some(blob.clone())));
    let rd = hoard.export_state(&ih).expect("exported");
    race.import_state(&rd).expect("imported");
    // A poll while both engines hold it: the old sum counted it twice.
    let (life, session, day) = session_and_day(&s.state);
    assert_eq!(session, (40, 0), "holding it twice is not traffic");
    assert_eq!(day, (40, 0));
    assert!(life.0 >= 5_000_040);
    hoard.remove_torrent(&ih, true).expect("hoard lets go");
    tick(&s);
    assert_eq!(session_and_day(&s.state).1, (40, 0));
    assert_eq!(
        engine_session(&s.state, "hoard"),
        (40, 0),
        "hoard keeps what it moved"
    );
    assert_eq!(
        engine_session(&s.state, "race"),
        (0, 0),
        "race moved nothing yet"
    );

    upload(&s, "race", &ih, 2);
    tick(&s);
    assert_eq!(session_and_day(&s.state).1, (42, 0));
    assert_eq!(engine_session(&s.state, "race"), (2, 0));
}

/// Deleting a torrent that moved bytes since the last tick keeps them in
/// the session, through the whole API path (absorb included).
#[tokio::test]
async fn bytes_moved_just_before_a_delete_stay_in_the_session() {
    let s = state_from("odo-settle", &format!("[daemon]\napi_key = \"{KEY}\"\n"));
    let ih = add(&s, "race", "late");
    load_lifetime(&s, "race", &ih, 77_000);
    tick(&s);
    boot(&s);
    upload(&s, "race", &ih, 9);
    let hash = typhon_engine::torrent::hex_encode(&ih);
    remove_one_torrent(&s.state, &hash, &["race".to_string()], "race", false).expect("removed");
    assert_eq!(
        session_and_day(&s.state).1,
        (9, 0),
        "settled by the removal, not lost"
    );
    tick(&s);
    let (_, session, day) = session_and_day(&s.state);
    assert_eq!(
        (session, day),
        ((9, 0), (9, 0)),
        "and not counted again by the tick"
    );
    assert_eq!(engine_session(&s.state, "race"), (9, 0));
}
