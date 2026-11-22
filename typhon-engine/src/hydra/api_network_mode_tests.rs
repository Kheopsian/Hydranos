use super::testing::*;
use super::*;

const KEY: &str = "0123456789abcdef0123456789abcdef";

const SOCKS_FILE: &str = "[daemon]\napi_key = \"0123456789abcdef0123456789abcdef\"\n\n\
    [race]\nlisten_port = 16171\nsocks5_outbound_host = \"10.0.0.1\"\nsocks5_outbound_port = 1080\n\
    announce_ip = \"198.51.100.7\"\nlisten_port_proxy_v2 = 16271\n\n\
    [hoard]\nlisten_port = 16172\nsocks5_outbound_host = \"10.0.0.1\"\n\n\
    [[engine]]\nname = \"vpn1\"\nrole = \"hoard\"\nengine_id = \"vpn1\"\n  [engine.session]\n  listen_port = 16999\n  socks5_outbound_host = \"10.0.0.2\"\n";

const DIRECT_FILE: &str = "[daemon]\napi_key = \"0123456789abcdef0123456789abcdef\"\n\n\
    [race]\nlisten_port = 16171\n\n[hoard]\nlisten_port = 16172\n";

async fn save(s: &AppState, body: serde_json::Value) -> (StatusCode, serde_json::Value) {
    let r = post_network_mode(
        State(s.clone()),
        RawQuery(None),
        keyed(KEY),
        body.to_string(),
    )
    .await;
    let status = r.status();
    (status, body_json(r).await)
}

async fn load(s: &AppState) -> serde_json::Value {
    body_json(get_network_mode(State(s.clone()), RawQuery(None), keyed(KEY)).await).await
}

const MARKED_FILE: &str = "[daemon]\napi_key = \"0123456789abcdef0123456789abcdef\"\n\n\
    [network]\nmode_unconfirmed = true\n\n\
    [race]\nlisten_port = 16171\n\n[hoard]\nlisten_port = 16172\n";

/// Saving the Network tab, direct mode included, is the confirmation: the
/// mark leaves the file, the GET stops reporting it, and an engine the
/// running process holds off the network because of it asks for a restart.
#[tokio::test]
async fn network_mode_save_removes_the_unconfirmed_mark() {
    let s = state_from("netconfirm-save", MARKED_FILE);
    for e in s.state.engines.engines() {
        let _ = e.blocked.set(crate::killswitch::CONFIRM_HOLD.to_string());
    }
    assert_eq!(load(&s.state).await["mode_unconfirmed"], true);
    let (st, body) = save(&s.state, form("direct", serde_json::json!({}))).await;
    assert_eq!(st, StatusCode::OK, "{body}");
    let text = std::fs::read_to_string(&s.config_path).unwrap();
    assert!(!text.contains("mode_unconfirmed"), "{text}");
    assert!(text.contains("mode = \"direct\""), "{text}");
    assert!(!s.cfg().network.mode_unconfirmed);
    assert_eq!(load(&s.state).await["mode_unconfirmed"], false);
    assert_eq!(body["restart_required"], true, "{body}");
}

/// Both ways the daemon writes its own template carry the mark: the file
/// created at startup, and the reset to defaults of the API.
#[tokio::test]
async fn network_mode_reset_to_defaults_writes_the_unconfirmed_mark() {
    let s = state_from("netconfirm-reset", DIRECT_FILE);
    assert!(!s.cfg().network.mode_unconfirmed);
    let r = post_settings_reset(State(s.state.clone()), RawQuery(None), keyed(KEY)).await;
    assert_eq!(r.status(), StatusCode::OK);
    let text = std::fs::read_to_string(&s.config_path).unwrap();
    let written: Config = toml::from_str(&text).unwrap();
    assert!(written.network.mode_unconfirmed, "{text}");
    assert!(s.cfg().network.mode_unconfirmed);
    assert_eq!(load(&s.state).await["mode_unconfirmed"], true);
}

fn form(mode: &str, extra: serde_json::Value) -> serde_json::Value {
    let mut fields = serde_json::json!({"race_listen_port": 16171, "hoard_listen_port": 16172});
    for (k, v) in extra.as_object().unwrap() {
        fields[k] = v.clone();
    }
    serde_json::json!({"mode": mode, "fields": fields})
}

/// Put a test state's engine on the network: the real listener
/// supervisor, TCP and uTP, on a free port.
async fn listening(s: &TestState, id: &str) -> u16 {
    let eng = s.state.engines.get(id).expect("engine");
    let port = free_port();
    let binding = typhon_engine::config::ResolvedBinding {
        id: 0,
        addr: std::net::SocketAddr::from(([127, 0, 0, 1], port)),
        peer_id: [b'-'; 20],
        egress: Default::default(),
        advertised_port: port,
        only_v6: false,
    };
    let utp = typhon_engine::peer::UtpHandle::bind(port, None)
        .await
        .expect("uTP binds");
    let (m, d, flag) = (eng.manager.clone(), eng.disk.clone(), eng.listening.clone());
    tokio::spawn(async move {
        let _ = typhon_engine::peer::listen(vec![binding], port, m, d, utp, flag).await;
    });
    for _ in 0..100 {
        if eng.listening.load(std::sync::atomic::Ordering::Relaxed) {
            return port;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    panic!("the listener never came up");
}

/// A port nothing holds right now, TCP and UDP.
fn free_port() -> u16 {
    loop {
        let t = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let p = t.local_addr().unwrap().port();
        if std::net::UdpSocket::bind(("0.0.0.0", p)).is_ok() {
            return p;
        }
    }
}

/// ⭐ A listen port that is the only change moves on the running listener:
/// no restart is asked for, the answer says what was applied, and the
/// engine really accepts on the new port.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_port_only_save_moves_the_listener_without_a_restart() {
    let s = state_from("netmode-port-live", DIRECT_FILE);
    let old = listening(&s, "hoard").await;
    let new = free_port();
    let (status, body) = save(
        &s,
        serde_json::json!({"mode": "direct", "fields": {
            "race_listen_port": 16171, "hoard_listen_port": new}}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["restart_required"], false, "{body}");
    assert_eq!(
        body["applied_live"],
        serde_json::json!([{"engine": "hoard", "port": new}]),
        "{body}"
    );
    let eng = s.state.engines.get("hoard").unwrap();
    assert_eq!(eng.manager.listen_port_now(eng.listen_port), new);
    assert!(
        std::net::TcpStream::connect(("127.0.0.1", new)).is_ok(),
        "TCP on the new port"
    );
    assert!(
        std::net::TcpStream::connect(("127.0.0.1", old)).is_err(),
        "the old port is let go"
    );
    assert_eq!(
        s.state.cfg().hoard.listen_port,
        new,
        "and the file keeps it"
    );
}

/// A port change together with another one still asks for the restart.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_port_save_with_another_change_still_needs_a_restart() {
    let s = state_from("netmode-port-and-ipv6", DIRECT_FILE);
    listening(&s, "hoard").await;
    let new = free_port();
    let (status, body) = save(
        &s,
        serde_json::json!({"mode": "direct", "fields": {
            "race_listen_port": 16171, "hoard_listen_port": new,
            "enable_ipv6": true}}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["restart_required"], true, "{body}");
    assert_eq!(body["applied_live"], serde_json::json!([]), "{body}");
}

/// ⭐ A save that sends only what it changes keeps everything else. Absent
/// fields used to be read as 0 / "": a race port of 0 refused the save,
/// and an absent proxy host emptied the proxy.
#[tokio::test]
async fn a_partial_save_keeps_the_fields_it_does_not_send() {
    let s = state_from("netmode-partial", SOCKS_FILE);
    let (status, body) = save(
        &s,
        serde_json::json!({"mode": "socks5", "fields": {"socks5_port": 1081}}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let f = &load(&s).await["fields"];
    assert_eq!(f["socks5_port"], 1081, "the field sent is written");
    assert_eq!(
        f["socks5_host"], "10.0.0.1",
        "the proxy host it did not send is kept"
    );
    assert_eq!(f["race_listen_port"], 16171, "and the listen ports");
    assert_eq!(f["hoard_listen_port"], 16172);
}

/// ⭐ The SOCKS5 password does not leave the daemon: the GETs answer the
/// mask, and a save that posts the mask back keeps the real one.
#[tokio::test]
async fn network_secrets_are_masked_and_a_masked_save_keeps_them() {
    let file = SOCKS_FILE.replace(
        "socks5_outbound_port = 1080\n",
        "socks5_outbound_port = 1080\nsocks5_outbound_user = \"u\"\nsocks5_outbound_pass = \"s3cr3t-pw\"\n",
    );
    let s = state_from("netmode-secret", &file);
    let f = &load(&s).await["fields"];
    assert_eq!(f["socks5_pass"], SECRET_MASK);
    assert_eq!(f["socks5_user"], "u", "only the secret is masked");
    let settings =
        body_json(get_settings(State(s.clone()), RawQuery(None), keyed(KEY)).await).await;
    assert!(!settings.to_string().contains("s3cr3t-pw"), "{settings}");
    assert_eq!(settings["race"]["socks5_outbound_pass"], SECRET_MASK);

    let (st, body) = save(&s, form("socks5", serde_json::json!({
        "socks5_host": "10.0.0.1", "socks5_port": 1080, "socks5_user": "u", "socks5_pass": SECRET_MASK,
    }))).await;
    assert_eq!(st, StatusCode::OK, "{body}");
    assert_eq!(
        s.cfg().race.socks5_outbound_pass,
        "s3cr3t-pw",
        "the mask was written as the password"
    );

    let (st, body) = save(&s, form("socks5", serde_json::json!({
        "socks5_host": "10.0.0.1", "socks5_port": 1080, "socks5_user": "u", "socks5_pass": "new-pw",
    }))).await;
    assert_eq!(st, StatusCode::OK, "{body}");
    assert_eq!(
        s.cfg().race.socks5_outbound_pass,
        "new-pw",
        "a new one is written"
    );
}

/// ⭐ A tracker passkey does not leave the daemon either: `/api/settings`
/// and `/api/announce/passkeys` answer the mask for a host that has one,
/// and posting the mask back (a form saved as it was) keeps the real value.
#[tokio::test]
async fn tracker_passkeys_are_masked_and_a_masked_save_keeps_them() {
    let file =
        format!("{DIRECT_FILE}\n[announce_passkeys]\n\"tracker.example\" = \"pk-s3cr3t\"\n");
    let s = state_from("passkey-mask", &file);

    let settings =
        body_json(get_settings(State(s.clone()), RawQuery(None), keyed(KEY)).await).await;
    assert!(!settings.to_string().contains("pk-s3cr3t"), "{settings}");
    assert_eq!(
        settings["announce_passkeys"]["tracker.example"],
        SECRET_MASK
    );
    let list =
        body_json(get_passkeys(State(s.clone()), RawQuery(None), keyed(KEY)).await).await;
    assert!(!list.to_string().contains("pk-s3cr3t"), "{list}");
    assert_eq!(
        list["tracker.example"], SECRET_MASK,
        "which hosts have one stays readable"
    );

    let kept = |s: &TestState| {
        s.cfg()
            .announce_passkeys
            .get("tracker.example")
            .cloned()
            .unwrap_or_default()
    };
    let change = |v: &str| {
        serde_json::json!({"changes": [
        {"section": "announce_passkeys", "key": "tracker.example", "value": v}]})
        .to_string()
    };

    let r = post_settings(
        State(s.clone()),
        RawQuery(None),
        keyed(KEY),
        change(SECRET_MASK),
    )
    .await;
    assert_eq!(r.status(), StatusCode::OK);
    assert_eq!(kept(&s), "pk-s3cr3t", "the mask was written as the passkey");

    let body =
        serde_json::json!({"host": "tracker.example", "passkey": SECRET_MASK}).to_string();
    let r = set_announce_passkey(State(s.clone()), RawQuery(None), keyed(KEY), body).await;
    let answer = body_json(r).await;
    assert!(!answer.to_string().contains("pk-s3cr3t"), "{answer}");
    assert_eq!(kept(&s), "pk-s3cr3t", "the tracker form wrote the mask");

    let r = post_settings(
        State(s.clone()),
        RawQuery(None),
        keyed(KEY),
        change("pk-new"),
    )
    .await;
    assert_eq!(r.status(), StatusCode::OK);
    assert_eq!(kept(&s), "pk-new", "a new one is written");
}

/// The API key and the admin password hash do not leave the daemon through
/// the settings read either: a cookie session reads the mask, and a save of
/// the form as it was keeps the real values.
#[tokio::test]
async fn the_api_key_and_password_hash_are_masked_and_a_masked_save_keeps_them() {
    let hash = "$2b$12$abcdefghijklmnopqrstuuABCDEFGHIJKLMNOPQRSTUVWXYZ01234";
    let file =
        format!("{DIRECT_FILE}\n[auth]\nusername = \"admin\"\npassword_hash = \"{hash}\"\n");
    let s = state_from("daemon-secrets-mask", &file);

    let settings =
        body_json(get_settings(State(s.clone()), RawQuery(None), keyed(KEY)).await).await;
    let text = settings.to_string();
    assert!(!text.contains(KEY), "the API key was read back: {text}");
    assert!(
        !text.contains(hash),
        "the password hash was read back: {text}"
    );
    assert_eq!(settings["daemon"]["api_key"], SECRET_MASK);
    assert_eq!(settings["auth"]["password_hash"], SECRET_MASK);
    assert_eq!(
        settings["auth"]["username"], "admin",
        "only the secrets are masked"
    );

    let r = post_settings(
        State(s.clone()),
        RawQuery(None),
        keyed(KEY),
        serde_json::json!({"changes": [
            {"section": "daemon", "key": "api_key", "value": SECRET_MASK},
            {"section": "auth", "key": "password_hash", "value": SECRET_MASK},
        ]})
        .to_string(),
    )
    .await;
    assert_eq!(r.status(), StatusCode::OK);
    assert_eq!(
        s.cfg().daemon.api_key,
        KEY,
        "the mask was written as the key"
    );
    assert_eq!(
        s.cfg().auth.password_hash,
        hash,
        "the mask was written as the hash"
    );

    let r = post_settings(
        State(s.clone()),
        RawQuery(None),
        keyed(KEY),
        serde_json::json!({"changes": [
            {"section": "daemon", "key": "api_key", "value": "a-new-key-0123456789abcdef0123456789"},
        ]})
        .to_string(),
    )
    .await;
    assert_eq!(r.status(), StatusCode::OK);
    assert_eq!(
        s.cfg().daemon.api_key,
        "a-new-key-0123456789abcdef0123456789",
        "a new one is written"
    );
}

/// Rotating the API key closes the browser sessions too: a cookie opened
/// with the old credentials must not outlive the key that was rotated
/// because it might be known.
#[tokio::test]
async fn rotating_the_api_key_ends_the_cookie_sessions() {
    let s = state_from("rotate-sessions", DIRECT_FILE);
    let sid = s.state.sessions.create();
    assert!(s.state.sessions.validate(&sid));

    let pending = crate::nodes::begin_local_rotation();
    let r = post_api_key_confirm(State(s.state.clone()), keyed(&pending)).await;
    assert_eq!(r.status(), StatusCode::OK);
    assert_eq!(s.cfg().daemon.api_key, pending, "the new key is live");
    assert!(
        !s.state.sessions.validate(&sid),
        "a session opened before the rotation still works"
    );
}

#[test]
fn a_proxy_url_masks_its_password_only() {
    assert_eq!(
        mask_proxy_url("socks5h://u:pw@10.0.0.1:1080"),
        "socks5h://u:********@10.0.0.1:1080"
    );
    assert_eq!(
        mask_proxy_url("http://u:p@ss@h:3128/x"),
        "http://u:********@h:3128/x",
        "the last @ ends the userinfo"
    );
    assert_eq!(
        mask_proxy_url("socks5h://u@10.0.0.1:1080"),
        "socks5h://u@10.0.0.1:1080",
        "no password, nothing to hide"
    );
    assert_eq!(
        mask_proxy_url("socks5h://10.0.0.1:1080"),
        "socks5h://10.0.0.1:1080"
    );
    assert_eq!(mask_proxy_url(""), "");
    assert_eq!(
        mask_proxy_url("http://h:3128/a@b"),
        "http://h:3128/a@b",
        "an @ in the path is not userinfo"
    );

    let stored = "socks5h://u:pw@10.0.0.1:1080";
    assert_eq!(
        unmask_proxy_url("socks5h://u:********@10.0.0.1:1080", stored),
        stored
    );
    assert_eq!(
        unmask_proxy_url("socks5h://u:********@10.0.0.9:1080", stored),
        "socks5h://u:pw@10.0.0.9:1080",
        "host edited, password kept"
    );
    assert_eq!(
        unmask_proxy_url("socks5h://u:new@10.0.0.1:1080", stored),
        "socks5h://u:new@10.0.0.1:1080"
    );
    assert_eq!(
        unmask_proxy_url("socks5h://u:********@10.0.0.1:1080", ""),
        "socks5h://u@10.0.0.1:1080",
        "the mask is never written"
    );
}

/// ⭐ `announce_proxy` carries its password in the URL: masked in both GETs,
/// and a save posting the mask back -- from the Network tab or the
/// settings screen -- keeps the real one.
#[tokio::test]
async fn the_announce_proxy_password_is_masked_and_kept() {
    let file = SOCKS_FILE.replace(
        "announce_ip = ",
        "announce_proxy = \"socks5h://u:s3cr3t-pw@10.0.0.1:1080\"\nannounce_ip = ",
    );
    let s = state_from("netmode-proxy-secret", &file);
    let masked = "socks5h://u:********@10.0.0.1:1080";
    assert_eq!(load(&s).await["fields"]["announce_proxy"], masked);
    let settings =
        body_json(get_settings(State(s.clone()), RawQuery(None), keyed(KEY)).await).await;
    assert!(!settings.to_string().contains("s3cr3t-pw"), "{settings}");
    assert_eq!(settings["race"]["announce_proxy"], masked);

    let (st, body) = save(
        &s,
        form(
            "socks5",
            serde_json::json!({
                "socks5_host": "10.0.0.1", "socks5_port": 1080, "announce_proxy": masked,
            }),
        ),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{body}");
    assert_eq!(
        s.cfg().race.announce_proxy,
        "socks5h://u:s3cr3t-pw@10.0.0.1:1080",
        "the Network tab wrote the mask"
    );

    let change = serde_json::json!({"changes": [{"section": "race", "key": "announce_proxy", "value": masked}]});
    let r = post_settings(
        State(s.clone()),
        RawQuery(None),
        keyed(KEY),
        change.to_string(),
    )
    .await;
    assert_eq!(r.status(), StatusCode::OK);
    assert_eq!(
        s.cfg().race.announce_proxy,
        "socks5h://u:s3cr3t-pw@10.0.0.1:1080",
        "the settings screen wrote the mask"
    );
}

/// ⭐⭐ Leaving SOCKS5 for direct REMOVES the proxy keys -- from race, from
/// hoard, and from an extra engine's own block -- records the mode, reads
/// it back, and asks for a restart because the running engines still dial
/// through the proxy.
#[tokio::test]
async fn switching_from_socks5_to_direct_clears_the_proxy_everywhere() {
    let s = state_from("netmode-socks-direct", SOCKS_FILE);
    assert_eq!(
        load(&s).await["mode"],
        "proxy_v2",
        "an old file is still deduced"
    );

    let (st, body) = save(&s, form("direct", serde_json::json!({}))).await;
    assert_eq!(st, StatusCode::OK, "{body}");
    assert_eq!(
        body["restart_required"], true,
        "the running engines still use the proxy"
    );

    let text = std::fs::read_to_string(&s.config_path).unwrap();
    for key in [
        "socks5_outbound_host",
        "socks5_outbound_port",
        "announce_ip",
        "listen_port_proxy_v2",
    ] {
        assert!(!text.contains(key), "{key} survived the switch:\n{text}");
    }
    assert!(
        text.contains("listen_port = 16999"),
        "the extra engine keeps the rest of its block:\n{text}"
    );
    let now = s.cfg();
    assert_eq!(now.network.mode, "direct");
    assert!(now
        .local_engines()
        .iter()
        .all(|e| e.session.socks5_outbound_host.is_empty()));
    assert_eq!(load(&s).await["mode"], "direct", "read back as saved");
}

/// ⭐ `restart_required` is exact: a save that changes nothing an engine
/// reads at boot needs none, one that adds a proxy does.
#[tokio::test]
async fn restart_required_follows_what_actually_changes() {
    let s = state_from("netmode-restart", DIRECT_FILE);
    let (st, body) = save(&s, form("direct", serde_json::json!({}))).await;
    assert_eq!(st, StatusCode::OK, "{body}");
    assert_eq!(
        body["restart_required"], false,
        "same network, nothing to restart: {body}"
    );

    let (st, body) = save(
        &s,
        form(
            "socks5",
            serde_json::json!({"socks5_host": "10.0.0.1", "socks5_port": 1080}),
        ),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{body}");
    assert_eq!(body["restart_required"], true);
    assert_eq!(load(&s).await["mode"], "socks5");
    assert_eq!(load(&s).await["fields"]["socks5_host"], "10.0.0.1");

    // Undone before any restart: the running engines never had it.
    let (_, body) = save(&s, form("direct", serde_json::json!({}))).await;
    assert_eq!(body["restart_required"], false, "{body}");
}

/// ⭐ Said when it is asked for: a proxy with UDP trackers on is a proxy
/// under which those trackers go silent, and the save answers so.
#[tokio::test]
async fn saving_a_proxy_warns_that_udp_trackers_will_not_be_announced() {
    let s = state_from("netmode-udp", DIRECT_FILE);
    let (_, body) = save(
        &s,
        form("socks5", serde_json::json!({"socks5_host": "10.0.0.1"})),
    )
    .await;
    let warned = body["warnings"]
        .as_array()
        .unwrap()
        .iter()
        .any(|w| w == crate::netmode::UDP_BEHIND_PROXY);
    assert!(warned, "{body}");
    let page = load(&s).await;
    assert!(
        page["warnings"]
            .as_array()
            .unwrap()
            .iter()
            .any(|w| w == crate::netmode::UDP_BEHIND_PROXY),
        "{page}"
    );
}

#[tokio::test]
async fn a_proxy_mode_without_its_essentials_is_refused() {
    let s = state_from("netmode-refuse", DIRECT_FILE);
    let (st, _) = save(&s, form("socks5", serde_json::json!({}))).await;
    assert_eq!(
        st,
        StatusCode::BAD_REQUEST,
        "SOCKS5 with no host is direct under another name"
    );
    let (st, _) = save(&s, form("proxy_v2", serde_json::json!({"race_proxy_v2_port": 16271, "proxy_v2_trusted_sources": ["not-an-ip"]}))).await;
    assert_eq!(st, StatusCode::BAD_REQUEST);
    let (st, _) = save(&s, form("teleport", serde_json::json!({}))).await;
    assert_eq!(st, StatusCode::BAD_REQUEST);
    assert!(s.cfg().network.mode.is_empty(), "nothing was written");
}

/// The gluetun engine is read from the file, not answered as "hoard".
#[tokio::test]
async fn the_gluetun_engine_is_read_back() {
    let s = state_from("netmode-gluetun", DIRECT_FILE);
    let (st, body) = save(
        &s,
        form(
            "gluetun",
            serde_json::json!({"gluetun_port_forward": true, "gluetun_port_engine": "race"}),
        ),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{body}");
    let page = load(&s).await;
    assert_eq!(page["mode"], "gluetun");
    assert_eq!(page["fields"]["gluetun_port_engine"], "race");
    assert_eq!(page["fields"]["gluetun_port_forward"], true);
}
