use super::testing::*;
use super::*;

const KEY: &str = "0123456789abcdef0123456789abcdef";

const CONF: &str =
    "[Interface]\nPrivateKey = aPrivateKeyValue=\nAddress = 10.2.0.2/32\nDNS = 10.2.0.1\n\n\
                    [Peer]\nPublicKey = aPublicKeyValue=\nPresharedKey = aPresharedValue=\n\
                    AllowedIPs = 0.0.0.0/0\nEndpoint = 192.0.2.10:51820\n";

/// A node whose data_dir is its own temporary directory, with a SOCKS5
/// proxy on race so a switch to WireGuard has something to clear.
fn node(tag: &str) -> TestState {
    let data = std::env::temp_dir().join(format!("hydra-wg-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&data);
    state_from(
        tag,
        &format!(
            "[daemon]\napi_key = \"{KEY}\"\ndata_dir = '{}'\n\n\
             [race]\nlisten_port = 16171\nsocks5_outbound_host = \"10.0.0.1\"\n\n\
             [hoard]\nlisten_port = 16172\n",
            data.display()
        ),
    )
}

async fn upload_raw(s: &AppState, name: &str, text: &str) -> (StatusCode, serde_json::Value) {
    let req = axum::extract::Request::builder()
        .body(axum::body::Body::from(text.to_string()))
        .unwrap();
    let r = post_wireguard_config_upload(
        State(s.clone()),
        RawQuery(Some(format!("name={name}"))),
        keyed(KEY),
        req,
    )
    .await;
    let st = r.status();
    (st, body_json(r).await)
}

async fn status(s: &AppState) -> String {
    let r = get_wireguard(State(s.clone()), RawQuery(None), keyed(KEY)).await;
    assert_eq!(r.status(), StatusCode::OK);
    String::from_utf8(
        axum::body::to_bytes(r.into_body(), usize::MAX)
            .await
            .unwrap()
            .to_vec(),
    )
    .unwrap()
}

async fn assign(s: &AppState, body: serde_json::Value) -> (StatusCode, serde_json::Value) {
    let r = post_wireguard_engines(
        State(s.clone()),
        RawQuery(None),
        keyed(KEY),
        body.to_string(),
    )
    .await;
    let st = r.status();
    (st, body_json(r).await)
}

fn race_on(file: &str) -> serde_json::Value {
    serde_json::json!({"engines": [
        {"engine_id": "race", "enabled": true, "config_file": file, "provider": "proton", "manual_port": 0, "port_forward": ""},
        {"engine_id": "hoard", "enabled": false, "config_file": "", "provider": "", "manual_port": 0, "port_forward": ""},
    ]})
}

/// ⭐ The block's three choices. A tunnel writes `wireguard_enabled =
/// true`; "direct" writes `allow_direct = true` and no tick box; "none"
/// writes neither, and the kill switch then blocks the engine. A caller
/// that sends only `enabled: false` keeps meaning "direct".
#[tokio::test]
async fn each_engine_gets_a_tunnel_direct_or_nothing() {
    crate::wgtunnel::force_support(Some(Ok(())));
    let s = node("wg-three-states");
    upload_raw(&s, "a.conf", CONF).await;
    let body = serde_json::json!({"engines": [
        {"engine_id": "race", "assignment": "tunnel", "config_file": "a.conf", "provider": "proton"},
        {"engine_id": "hoard", "assignment": "none"},
    ]});
    let (st, r) = assign(&s, body).await;
    assert_eq!(st, StatusCode::OK, "{r}");
    assert!(
        r["warnings"]
            .as_array()
            .unwrap()
            .iter()
            .any(|w| w.as_str().unwrap().contains("off the network")),
        "{r}"
    );
    let text = std::fs::read_to_string(&s.config_path).unwrap();
    let cfg = s.cfg();
    let plan = crate::killswitch::plan_with(&cfg, &|_| None, None);
    assert!(plan.blocked("race").is_none());
    assert!(
        plan.blocked("hoard").is_some(),
        "unassigned = blocked:\n{text}"
    );
    assert_eq!(r["restart_required"], true);

    let body = serde_json::json!({"engines": [
        {"engine_id": "race", "assignment": "tunnel", "config_file": "a.conf", "provider": "proton"},
        {"engine_id": "hoard", "assignment": "direct"},
    ]});
    assert_eq!(assign(&s, body).await.0, StatusCode::OK);
    let cfg = s.cfg();
    assert!(
        cfg.hoard.allow_direct && cfg.hoard.wireguard_enabled.is_none(),
        "{:?}",
        cfg.hoard
    );
    assert!(crate::killswitch::plan_with(&cfg, &|_| None, None)
        .blocked("hoard")
        .is_none());
    let page: serde_json::Value = serde_json::from_str(&status(&s).await).unwrap();
    assert_eq!(page["engines"]["hoard"]["assignment"], "direct");
    assert_eq!(page["engines"]["race"]["assignment"], "tunnel");

    // The old shape: enabled false is direct, as the tick box meant.
    assert_eq!(assign(&s, race_on("a.conf")).await.0, StatusCode::OK);
    assert!(s.cfg().hoard.allow_direct);
    let bad = serde_json::json!({"engines": [{"engine_id": "race", "assignment": "maybe"}]});
    assert_eq!(assign(&s, bad).await.0, StatusCode::BAD_REQUEST);
    // The Network tab's answer carries the same verdicts.
    let mode =
        body_json(get_network_mode(State(s.state.clone()), RawQuery(None), keyed(KEY)).await).await;
    assert_eq!(mode["kill_switch"]["armed"], true, "{mode:#}");
    assert!(mode["kill_switch"]["summary"]
        .as_array()
        .unwrap()
        .iter()
        .any(|l| l.as_str().unwrap().starts_with("hoard: direct on purpose")));
    crate::wgtunnel::force_support(None);
}

/// ⭐ Stored, listed, never served back: no private or preshared key in
/// any answer, the file at 0600, and a file that would not bring a
/// tunnel up refused before it is written.
#[tokio::test]
async fn a_conf_is_stored_listed_and_never_served_back() {
    let s = node("wg-upload");
    let (st, body) = upload_raw(&s, "proton-ch.conf", CONF).await;
    assert_eq!(st, StatusCode::OK, "{body}");
    assert_eq!(body["address"], "10.2.0.2/32");
    let page = status(&s).await;
    for secret in ["aPrivateKeyValue", "aPresharedValue"] {
        assert!(
            !body.to_string().contains(secret) && !page.contains(secret),
            "{secret} left the process:\n{page}"
        );
    }
    assert!(
        page.contains("\"name\":\"proton-ch.conf\"") && page.contains("192.0.2.10:51820"),
        "{page}"
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let dir = crate::wgtunnel::conf_dir(&s.cfg().daemon.data_dir);
        assert_eq!(
            std::fs::metadata(dir.join("proton-ch.conf"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
    }
    let (st, body) = upload_raw(&s, "broken.conf", "[Interface]\nAddress = 10.0.0.2/32\n").await;
    assert_eq!(st, StatusCode::BAD_REQUEST);
    assert!(
        body["error"].as_str().unwrap().contains("PrivateKey"),
        "{body}"
    );
    let (st, _) = upload_raw(&s, "", CONF).await;
    assert_eq!(st, StatusCode::BAD_REQUEST, "no name");
}

/// The page uploads with FormData: the multipart path, file name kept.
#[tokio::test]
async fn a_conf_is_accepted_from_the_pages_form() {
    let s = node("wg-multipart");
    let boundary = "hyboundary";
    let body = format!(
        "--{boundary}\r\nContent-Disposition: form-data; name=\"file\"; filename=\"mine.conf\"\r\n\
         Content-Type: application/octet-stream\r\n\r\n{CONF}\r\n--{boundary}--\r\n"
    );
    let ct = format!("multipart/form-data; boundary={boundary}");
    let req = axum::extract::Request::builder()
        .header("content-type", &ct)
        .body(axum::body::Body::from(body))
        .unwrap();
    let mut headers = keyed(KEY);
    headers.insert("content-type", ct.parse().unwrap());
    let r =
        post_wireguard_config_upload(State(s.state.clone()), RawQuery(None), headers, req).await;
    assert_eq!(r.status(), StatusCode::OK);
    assert!(status(&s).await.contains("\"name\":\"mine.conf\""));
}

#[tokio::test]
async fn a_conf_is_removed_once() {
    let s = node("wg-delete");
    upload_raw(&s, "gone.conf", CONF).await;
    let del = |n: &str| {
        delete_wireguard_config(
            State(s.state.clone()),
            Path(n.to_string()),
            RawQuery(None),
            keyed(KEY),
        )
    };
    assert_eq!(del("gone.conf").await.status(), StatusCode::OK);
    assert_eq!(del("gone.conf").await.status(), StatusCode::NOT_FOUND);
    assert_eq!(
        del("..%2Fdefault.toml").await.status(),
        StatusCode::BAD_REQUEST
    );
    assert!(!status(&s).await.contains("gone.conf"));
}

/// ⭐ Without the privilege, the mode and the assignment are refused with
/// the reason, nothing is written, and the page is told why.
#[tokio::test]
async fn without_net_admin_the_tunnels_are_refused_with_the_reason() {
    crate::wgtunnel::force_support(Some(Err(crate::wgtunnel::NEEDS_NET_ADMIN)));
    let s = node("wg-refused");
    upload_raw(&s, "a.conf", CONF).await;
    let before = std::fs::read_to_string(&s.config_path).unwrap();
    let (st, body) = assign(&s, race_on("a.conf")).await;
    assert_eq!(st, StatusCode::CONFLICT, "{body}");
    assert_eq!(body["error"], crate::wgtunnel::NEEDS_NET_ADMIN);
    let form = serde_json::json!({"mode": "wireguard", "fields": {"race_listen_port": 16171, "hoard_listen_port": 16172}});
    let r = post_network_mode(
        State(s.state.clone()),
        RawQuery(None),
        keyed(KEY),
        form.to_string(),
    )
    .await;
    assert_eq!(r.status(), StatusCode::CONFLICT);
    assert_eq!(
        std::fs::read_to_string(&s.config_path).unwrap(),
        before,
        "nothing written"
    );

    let page: serde_json::Value = serde_json::from_str(&status(&s).await).unwrap();
    assert_eq!(page["supported"], false);
    assert_eq!(page["unsupported_reason"], crate::wgtunnel::NEEDS_NET_ADMIN);
    let mode =
        body_json(get_network_mode(State(s.state.clone()), RawQuery(None), keyed(KEY)).await).await;
    assert_eq!(mode["wireguard"]["supported"], false);
    assert_eq!(
        mode["wireguard"]["reason"],
        crate::wgtunnel::NEEDS_NET_ADMIN
    );
    crate::wgtunnel::force_support(None);
}

/// Choosing the network through the WireGuard block alone is a choice
/// too: the "network to confirm" mark leaves the file, the GET stops
/// reporting it, and engines held for it ask for a restart.
#[tokio::test]
async fn network_mode_wireguard_assignment_removes_the_unconfirmed_mark() {
    crate::wgtunnel::force_support(Some(Ok(())));
    let data = std::env::temp_dir().join(format!("hydra-wg-mark-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&data);
    let s = state_from(
        "wg-mark",
        &format!(
            "[daemon]\napi_key = \"{KEY}\"\ndata_dir = '{}'\n\n\
             [network]\nmode_unconfirmed = true\n\n\
             [race]\nlisten_port = 16171\n\n[hoard]\nlisten_port = 16172\n",
            data.display()
        ),
    );
    for e in s.state.engines.engines() {
        let _ = e.blocked.set(crate::killswitch::CONFIRM_HOLD.to_string());
    }
    upload_raw(&s, "a.conf", CONF).await;
    let get = |s: &AppState| {
        let s = s.clone();
        async move { body_json(get_network_mode(State(s), RawQuery(None), keyed(KEY)).await).await }
    };
    assert_eq!(get(&s).await["mode_unconfirmed"], true);
    let (st, body) = assign(&s, race_on("a.conf")).await;
    assert_eq!(st, StatusCode::OK, "{body}");
    let text = std::fs::read_to_string(&s.config_path).unwrap();
    assert!(!text.contains("mode_unconfirmed"), "{text}");
    assert!(!s.cfg().network.mode_unconfirmed);
    assert_eq!(get(&s).await["mode_unconfirmed"], false);
    assert_eq!(body["restart_required"], true, "{body}");
    crate::wgtunnel::force_support(None);
}

/// ⭐⭐ Assigned: written per engine, the node switched to WireGuard, the
/// proxy keys of the mode it left removed, the engine pinned to its
/// device, and a restart asked for. Then leaving the mode clears them.
#[tokio::test]
async fn an_assignment_switches_the_node_to_wireguard_and_another_mode_clears_it() {
    crate::wgtunnel::force_support(Some(Ok(())));
    let s = node("wg-assign");
    upload_raw(&s, "a.conf", CONF).await;
    let (st, body) = assign(&s, race_on("a.conf")).await;
    assert_eq!(st, StatusCode::OK, "{body}");
    assert_eq!(body["restart_required"], true);
    let cfg = s.cfg();
    assert_eq!(cfg.network.mode, "wireguard");
    let race = cfg
        .local_engines()
        .into_iter()
        .find(|e| e.id == "race")
        .unwrap();
    assert_eq!(race.session.wireguard_enabled, Some(true));
    assert_eq!(race.session.wireguard_config, "a.conf");
    assert_eq!(
        race.session.bind_interface, "wg-race",
        "pinned to its device"
    );
    assert!(
        race.session.socks5_outbound_host.is_empty(),
        "the SOCKS5 mode's keys are gone"
    );
    let page: serde_json::Value = serde_json::from_str(&status(&s).await).unwrap();
    assert_eq!(page["engines"]["race"]["config_file"], "a.conf");
    assert_eq!(page["engines"]["race"]["device"], "wg-race");

    // One file, one engine.
    let mut both = race_on("a.conf");
    both["engines"][1] = serde_json::json!({"engine_id": "hoard", "enabled": true, "config_file": "a.conf", "provider": "proton"});
    assert_eq!(assign(&s, both).await.0, StatusCode::BAD_REQUEST);
    // A file that is not there, an engine that is not there.
    assert_eq!(
        assign(&s, race_on("missing.conf")).await.0,
        StatusCode::BAD_REQUEST
    );
    let ghost = serde_json::json!({"engines": [{"engine_id": "ghost", "enabled": false}]});
    assert_eq!(assign(&s, ghost).await.0, StatusCode::BAD_REQUEST);

    let form = serde_json::json!({"mode": "direct", "fields": {"race_listen_port": 16171, "hoard_listen_port": 16172}});
    let r = post_network_mode(
        State(s.state.clone()),
        RawQuery(None),
        keyed(KEY),
        form.to_string(),
    )
    .await;
    assert_eq!(r.status(), StatusCode::OK);
    let text = std::fs::read_to_string(&s.config_path).unwrap();
    assert!(
        !text.contains("wireguard_"),
        "a tunnel outlived the mode:\n{text}"
    );
    let race = s
        .cfg()
        .local_engines()
        .into_iter()
        .find(|e| e.id == "race")
        .unwrap();
    assert_eq!(race.session.bind_interface, "");
    crate::wgtunnel::force_support(None);
}

/// The running engines' DHT and tunnel, as they are, on the Network tab.
#[tokio::test]
async fn the_network_mode_reports_what_each_engine_runs() {
    let s = node("wg-state");
    let mode =
        body_json(get_network_mode(State(s.state.clone()), RawQuery(None), keyed(KEY)).await).await;
    let rows = mode["engine_state"].as_array().unwrap();
    assert_eq!(rows.len(), 2);
    let race = rows.iter().find(|r| r["id"] == "race").unwrap();
    assert_eq!(race["dht_running"], false);
    assert_eq!(
        race["dht_note"], "offline",
        "an engine not on the network says so"
    );
    assert!(
        mode["warnings"]
            .to_string()
            .contains("DHT is off behind the SOCKS5 proxy"),
        "{mode}"
    );
}
