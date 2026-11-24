use super::testing::*;
use super::*;
use futures::StreamExt;

const KEY: &str = "0123456789abcdef0123456789abcdef";

/// ⭐ A page that stops reading the event stream does not make the
/// engine's bus keep every event since for it: the relay goes on draining
/// and drops the frames that page cannot take.
#[tokio::test]
async fn a_page_that_stops_reading_does_not_hold_the_bus() {
    let s = state_from("sse-stall", &format!("[daemon]\napi_key = \"{KEY}\"\n"));
    let resp = stream_events(
        State(s.state.clone()),
        RawQuery(Some("hydrate=0".into())),
        keyed(KEY),
    )
    .await;
    let mut body = resp.into_body().into_data_stream();
    // Read up to the first status frame of the live loop: by then every
    // engine's relay is subscribed.
    let mut statuses = 0;
    while statuses < 2 {
        let chunk = tokio::time::timeout(std::time::Duration::from_secs(5), body.next())
            .await
            .expect("a frame")
            .expect("not the end")
            .expect("a chunk");
        statuses += String::from_utf8_lossy(&chunk)
            .matches("\"status_snapshot\"")
            .count();
    }
    // From here the page reads nothing more.
    let manager = s.state.engines.engines()[0].manager.clone();
    let mini = || typhon_engine::rpc::events::TorrentStatsMini {
        info_hash: "ab".repeat(20),
        status: 0,
        total_uploaded: 0,
        total_downloaded: 0,
        upload_rate: 0,
        download_rate: 0,
        peers_connected: 0,
        peers_interested: 0,
        progress: 0.0,
        checking_progress: None,
        check_queued: false,
    };
    for _ in 0..500 {
        manager
            .bus()
            .publish(typhon_engine::rpc::events::Event::StatsSnapshot {
                torrents: vec![mini()],
            });
        tokio::task::yield_now().await;
    }
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    let held = manager.bus().backlog();
    assert!(
        held < 100,
        "the bus still holds {held} of 500 events for a page that stopped reading"
    );
    drop(body);
}

/// ⭐ What a stalled page could not take is held, not dropped: once it reads
/// again it gets every removal and the latest figures of every torrent. A
/// stats frame is a delta and a removal is sent once, so losing either
/// would leave the list wrong until a reload.
#[tokio::test]
async fn a_page_that_reads_again_gets_every_removal_and_the_latest_figures() {
    let s = state_from("sse-catchup", &format!("[daemon]\napi_key = \"{KEY}\"\n"));
    let resp = stream_events(
        State(s.state.clone()),
        RawQuery(Some("hydrate=0".into())),
        keyed(KEY),
    )
    .await;
    let mut body = resp.into_body().into_data_stream();
    let mut statuses = 0;
    while statuses < 2 {
        let chunk = tokio::time::timeout(std::time::Duration::from_secs(5), body.next())
            .await
            .expect("a frame")
            .expect("not the end")
            .expect("a chunk");
        statuses += String::from_utf8_lossy(&chunk)
            .matches("\"status_snapshot\"")
            .count();
    }
    let manager = s.state.engines.engines()[0].manager.clone();
    let row = |hash: &str, up: u64| typhon_engine::rpc::events::TorrentStatsMini {
        info_hash: hash.to_string(),
        status: 3,
        total_uploaded: up,
        total_downloaded: 0,
        upload_rate: up,
        download_rate: 0,
        peers_connected: 0,
        peers_interested: 0,
        progress: 1.0,
        checking_progress: None,
        check_queued: false,
    };
    let (kept, gone) = ("aa".repeat(20), "bb".repeat(20));
    for i in 0..300u64 {
        manager
            .bus()
            .publish(typhon_engine::rpc::events::Event::StatsSnapshot {
                torrents: vec![row(&kept, 1000 + i)],
            });
        tokio::task::yield_now().await;
    }
    manager
        .bus()
        .publish(typhon_engine::rpc::events::Event::TorrentRemoved {
            info_hash: gone.clone(),
        });
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    // The page reads again.
    let (mut saw_removal, mut saw_last) = (false, false);
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
    let mut text = String::new();
    while !(saw_removal && saw_last) && tokio::time::Instant::now() < deadline {
        let Ok(Some(Ok(chunk))) = tokio::time::timeout_at(deadline, body.next()).await else {
            break;
        };
        text.push_str(&String::from_utf8_lossy(&chunk));
        saw_removal =
            text.contains(&format!("\"info_hash\":\"{gone}\"")) && text.contains("torrent_removed");
        saw_last = text.contains("\"upload_rate\":1299");
    }
    assert!(saw_removal, "the removal reached the page");
    assert!(saw_last, "the latest figures reached the page");
}

async fn read_until(
    body: &mut (impl futures::Stream<Item = Result<axum::body::Bytes, axum::Error>> + Unpin),
    wanted: &[&str],
    secs: u64,
) -> String {
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(secs);
    let mut text = String::new();
    while !wanted.iter().all(|w| text.contains(w)) && tokio::time::Instant::now() < deadline {
        let Ok(Some(Ok(chunk))) = tokio::time::timeout_at(deadline, body.next()).await else {
            break;
        };
        text.push_str(&String::from_utf8_lossy(&chunk));
    }
    text
}

/// ⭐ Who started to serve reaches the page with its row, and who stopped as a
/// hash, so the Actively Seeding list needs no request to follow.
#[tokio::test]
async fn the_page_is_told_who_started_and_who_stopped_serving() {
    let s = state_from("sse-active", &format!("[daemon]
api_key = \"{KEY}\"
"));
    let engine = s.state.engines.get("hoard").expect("hoard engine");
    let (ih, _) = engine
        .manager
        .add_torrent_bytes(&sse_torrent("served"), "/tmp", true, true)
        .expect("added");
    let hash = typhon_engine::torrent::hex_encode(&ih);
    let resp = stream_events(
        State(s.state.clone()),
        RawQuery(Some("hydrate=0".into())),
        keyed(KEY),
    )
    .await;
    let mut body = resp.into_body().into_data_stream();
    // The first status frame leaves before the relays exist; the second is
    // sent by the live loop, after they are subscribed.
    read_until(&mut body, &["\"status_snapshot\""], 5).await;
    read_until(&mut body, &["\"status_snapshot\""], 5).await;

    let gone = "cd".repeat(20);
    engine
        .manager
        .bus()
        .publish(typhon_engine::rpc::events::Event::ActiveChanged {
            added: vec![hash.clone()],
            removed: vec![gone.clone()],
            count: 1,
        });
    let text = read_until(&mut body, &["active_changed", &gone], 5).await;
    assert!(text.contains("active_changed"), "no event in: {text}");
    assert!(text.contains(&gone), "the removal is named: {text}");
    assert!(
        text.contains(&format!("\"info_hash\":\"{hash}\"")),
        "the joiner comes with its row: {text}"
    );
    assert!(text.contains("\"state\":\""), "a built row, with a state: {text}");
}

/// ⭐ The chips of the list are pushed, counted by the page's own function, so
/// they move with the rows instead of a poll later.
#[tokio::test]
async fn the_chips_are_pushed_and_move_when_a_figure_moves() {
    let s = state_from("sse-facets", &format!("[daemon]
api_key = \"{KEY}\"
"));
    let engine = s.state.engines.get("hoard").expect("hoard engine");
    engine
        .manager
        .add_torrent_bytes(&sse_torrent("first"), "/tmp", true, true)
        .expect("added");
    let resp = stream_events(
        State(s.state.clone()),
        RawQuery(Some("hydrate=0".into())),
        keyed(KEY),
    )
    .await;
    let mut body = resp.into_body().into_data_stream();
    let text = read_until(&mut body, &["facets_snapshot"], 6).await;
    assert!(text.contains("facets_snapshot"), "no chips pushed: {text}");
    assert!(text.contains("\"all\":1"), "one torrent counted: {text}");

    engine
        .manager
        .add_torrent_bytes(&sse_torrent("second"), "/tmp", true, true)
        .expect("added");
    let text = read_until(&mut body, &["\"all\":2"], 8).await;
    assert!(text.contains("\"all\":2"), "the new count is pushed: {text}");
}

fn sse_torrent(name: &str) -> Vec<u8> {
    let mut info = Vec::new();
    info.extend_from_slice(format!("d6:lengthi16384e4:name{}:{name}", name.len()).as_bytes());
    info.extend_from_slice(b"12:piece lengthi16384e6:pieces20:");
    let mut piece = [0xABu8; 20];
    piece[0] = name.as_bytes()[0];
    piece[1] = name.len() as u8;
    piece[2] = name.as_bytes()[name.len() - 1];
    info.extend_from_slice(&piece);
    info.push(b'e');
    let mut out = Vec::new();
    out.extend_from_slice(b"d8:announce32:https://tracker.example/announce4:info");
    out.extend_from_slice(&info);
    out.push(b'e');
    out
}
