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
        saw_removal = text.contains(&format!("\"info_hash\":\"{gone}\""))
            && text.contains("torrent_removed");
        saw_last = text.contains("\"upload_rate\":1299");
    }
    assert!(saw_removal, "the removal reached the page");
    assert!(saw_last, "the latest figures reached the page");
}
