//! What a workflow tells the outside world: the document a webhook receives
//! and how the sender behaves when the receiver does not answer, and which
//! tracker URLs a workflow may add.

use super::*;
use crate::rules::{Cond, Node, Op, Trigger};
use std::io::{Read, Write};
use std::sync::atomic::{AtomicUsize, Ordering};

const GIB: f64 = 1024.0 * 1024.0 * 1024.0;

fn workflow(trigger: Trigger) -> Workflow {
    Workflow {
        id: "w".into(),
        name: "Tell me".into(),
        enabled: true,
        position: 0,
        trigger,
        interval_secs: rules::DEFAULT_INTERVAL_SECS,
        when: Node::Cond(Cond {
            field: "ratio".into(),
            op: Op::Gt,
            value: "1".into(),
            hold: None,
            tag: None,
        }),
        then: vec![Action::AddTags {
            tags: vec!["told".into()],
        }],
        cap: rules::DEFAULT_CAP,
    }
}

fn facts() -> Facts {
    Facts {
        info_hash: "ab".repeat(20),
        name: "Some.Release".into(),
        category: "films".into(),
        tags: vec!["a".into(), "b".into()],
        engine: "hoard".into(),
        save_path: "/data/films".into(),
        state: "stalledUP".into(),
        tracker_host: "tracker.example".into(),
        total_size: 3.0 * GIB,
        progress: 100.0,
        ratio: 2.5,
        total_uploaded: 7.0 * GIB,
        total_downloaded: 3.0 * GIB,
        seeding_time: 86_400.0,
        ..Default::default()
    }
}

/// ⭐ The document is the same one-line summary under the three names the
/// common receivers read, with the size in GiB to two places, and the
/// torrent's own fields exactly as it has them.
#[test]
fn a_webhook_carries_one_summary_under_three_names_and_the_torrents_fields() {
    for (trigger, event, line) in [
        (
            Trigger::Completed,
            "completed",
            "Some.Release finished downloading (3.00 GiB)",
        ),
        (Trigger::Added, "added", "Some.Release added (3.00 GiB)"),
        (
            Trigger::Schedule,
            "matched",
            "Tell me: Some.Release (3.00 GiB)",
        ),
    ] {
        let before = crate::store::now_secs();
        let p = webhook_payload(&workflow(trigger), &facts());
        assert_eq!(p["event"], event);
        assert_eq!(p["workflow"], "Tell me");
        assert_eq!(p["content"], line);
        assert_eq!(p["text"], line);
        assert_eq!(p["message"], line);
        let at = p["at"].as_i64().unwrap();
        assert!(at >= before && at <= crate::store::now_secs());
    }

    let p = webhook_payload(&workflow(Trigger::Completed), &facts());
    let t = &p["torrent"];
    assert_eq!(t["info_hash"], "ab".repeat(20));
    assert_eq!(t["name"], "Some.Release");
    assert_eq!(t["category"], "films");
    assert_eq!(t["tags"], serde_json::json!(["a", "b"]));
    assert_eq!(t["engine"], "hoard");
    assert_eq!(t["save_path"], "/data/films");
    assert_eq!(t["state"], "stalledUP");
    assert_eq!(t["tracker"], "tracker.example");
    assert_eq!(t["size"], 3.0 * GIB);
    assert_eq!(t["progress"], 100.0);
    assert_eq!(t["ratio"], 2.5);
    assert_eq!(t["uploaded"], 7.0 * GIB);
    assert_eq!(t["downloaded"], 3.0 * GIB);
    assert_eq!(t["seeding_time"], 86_400.0);
}

/// The size in GiB is the bytes over 2^30, not over 10^9: a 1.5 GiB torrent
/// reads 1.50, and one of a byte reads 0.00.
#[test]
fn the_size_in_the_summary_is_in_binary_gigabytes() {
    let said = |bytes: f64| {
        let f = Facts {
            total_size: bytes,
            ..facts()
        };
        webhook_payload(&workflow(Trigger::Added), &f)["content"]
            .as_str()
            .unwrap()
            .to_string()
    };
    assert_eq!(said(1.5 * GIB), "Some.Release added (1.50 GiB)");
    assert_eq!(said(1_000_000_000.0), "Some.Release added (0.93 GiB)");
    assert_eq!(said(1.0), "Some.Release added (0.00 GiB)");
    assert_eq!(said(1024.0 * GIB), "Some.Release added (1024.00 GiB)");
}

/// A figure the torrent does not have is null in the document, not a number
/// that is not one.
#[test]
fn a_figure_that_is_not_known_is_null() {
    let f = Facts {
        ratio: rules::NEVER,
        seeding_time: f64::INFINITY,
        ..facts()
    };
    let t = webhook_payload(&workflow(Trigger::Completed), &f)["torrent"].clone();
    assert!(t["ratio"].is_null());
    assert!(t["seeding_time"].is_null());
    assert_eq!(t["size"], 3.0 * GIB, "the known ones stay");
}

/// A receiver on 127.0.0.1 that answers `codes` in turn, and counts.
fn receiver(codes: Vec<u16>) -> (String, std::sync::Arc<AtomicUsize>) {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let hits = std::sync::Arc::new(AtomicUsize::new(0));
    let seen = hits.clone();
    std::thread::spawn(move || {
        for code in codes {
            let Ok((mut sock, _)) = listener.accept() else {
                return;
            };
            let mut buf = [0u8; 8192];
            let mut got = Vec::new();
            while !got.windows(4).any(|w| w == b"\r\n\r\n") {
                match sock.read(&mut buf) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => got.extend_from_slice(&buf[..n]),
                }
            }
            seen.fetch_add(1, Ordering::SeqCst);
            let _ = sock.write_all(
                format!("HTTP/1.1 {code} X\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
                    .as_bytes(),
            );
        }
    });
    (format!("http://127.0.0.1:{port}/hook"), hits)
}

/// ⭐ A receiver that says yes is told once; one that says the request is
/// wrong is told once (asking again would not mend it); one that fails is
/// asked again after 2 seconds and after 5 more, and the answer says how many
/// tries it took; one that recovers on the second try is told after 2 seconds.
#[test]
fn a_webhook_is_sent_once_unless_the_receiver_fails() {
    let payload = serde_json::json!({"hello": "world"});

    let (url, hits) = receiver(vec![200]);
    assert_eq!(send_webhook(&url, &payload), Ok(()));
    assert_eq!(hits.load(Ordering::SeqCst), 1);

    let (url, hits) = receiver(vec![404, 200, 200]);
    assert_eq!(
        send_webhook(&url, &payload),
        Err("webhook: HTTP 404".into())
    );
    assert_eq!(hits.load(Ordering::SeqCst), 1, "a 4xx is not asked again");

    let (url, hits) = receiver(vec![503, 200]);
    let t = std::time::Instant::now();
    assert_eq!(send_webhook(&url, &payload), Ok(()));
    assert_eq!(hits.load(Ordering::SeqCst), 2, "one retry mended it");
    assert!(
        t.elapsed() >= std::time::Duration::from_secs(2),
        "after waiting 2 s: {:?}",
        t.elapsed()
    );

    let (url, hits) = receiver(vec![500, 500, 500, 200]);
    let t = std::time::Instant::now();
    assert_eq!(
        send_webhook(&url, &payload),
        Err("webhook: HTTP 500 after 3 tries".into())
    );
    assert_eq!(hits.load(Ordering::SeqCst), 3, "three tries and no more");
    assert!(
        t.elapsed() >= std::time::Duration::from_secs(7),
        "2 s and then 5 s between the tries: {:?}",
        t.elapsed()
    );
}

/// What a workflow may add: announce URLs (http, https or udp with a host)
/// and the URL of a list. A good one among bad ones is not the one refused.
#[test]
fn only_announce_urls_may_be_added_and_the_bad_one_is_named() {
    let adding = |urls: &[&str], list: &str| {
        let mut w = workflow(Trigger::Schedule);
        w.then = vec![Action::AddTrackers {
            urls: urls.iter().map(|u| u.to_string()).collect(),
            list_url: list.into(),
        }];
        rules::compile_workflow(&w).err()
    };
    assert_eq!(adding(&["udp://t.example:80/announce"], ""), None);
    assert_eq!(
        adding(&["https://a.example/announce", "udp://t.example:80"], ""),
        None
    );
    assert_eq!(
        adding(
            &["udp://t.example:80", "ftp://bad.example/x", "nonsense"],
            ""
        ),
        Some(rules::CompileError::BadTracker(
            "ftp://bad.example/x".into()
        )),
        "the first bad one, not a good one"
    );
    assert_eq!(
        adding(&["udp://t.example:80"], "ftp://list.example/trackers.txt"),
        Some(rules::CompileError::BadTracker(
            "ftp://list.example/trackers.txt".into()
        ))
    );
    assert_eq!(adding(&[], "https://list.example/trackers.txt"), None);
    assert_eq!(adding(&[], "  "), Some(rules::CompileError::NoTrackers));
}
