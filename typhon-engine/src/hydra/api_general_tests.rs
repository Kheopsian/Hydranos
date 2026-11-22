use super::*;

#[test]
fn daily_traffic_survives_restarts_and_expires_at_midnight() {
    let fresh = || Odometer {
        day_date: "2026-11-15".into(),
        ..Default::default()
    };
    let mut odo = fresh();
    let today = odo.day_date.clone();
    let saved = odo.checkpoint((700, 10), &today);
    let mut restarted = fresh();
    restarted.restore(&saved).unwrap();
    assert_eq!(restarted.day((0, 0), &today), (700, 10));
    assert_eq!(restarted.day((20, 3), &today), (720, 13));
    let saved = restarted.checkpoint((20, 3), &today);
    let mut again = fresh();
    again.restore(&saved).unwrap();
    assert_eq!(again.day((5, 2), &today), (725, 15));
    assert_eq!(again.day((6, 3), "2099-01-01"), (0, 0));
    assert_eq!(again.day((9, 7), "2099-01-01"), (3, 4));
    let mut tomorrow = Odometer {
        day_date: "2099-01-01".into(),
        ..Default::default()
    };
    tomorrow.restore(&saved).unwrap();
    assert_eq!(tomorrow.day((5, 2), "2099-01-01"), (5, 2));
    assert!(tomorrow.restore("broken").is_err());
    assert!(tomorrow.restore(r#"["2099-01-01",-1,0]"#).is_err());
}

#[test]
fn daily_traffic_saturates_when_restored_totals_reach_the_limit() {
    let mut odo = Odometer {
        day_date: "2026-11-15".into(),
        ..Default::default()
    };
    odo.restore(&format!(r#"["2026-11-15",{},{}]"#, i64::MAX, i64::MAX))
        .unwrap();
    assert_eq!(odo.day((1, 2), "2026-11-15"), (i64::MAX, i64::MAX));
}

/// The day is the session minus its value at the last local midnight.
#[test]
fn midnight_resets_the_day_but_not_the_session() {
    let mut odo = Odometer {
        day_baseline: (0, 0),
        day_date: "2026-09-08".into(),
    };
    assert_eq!(
        odo.day((700, 10), "2026-09-08"),
        (700, 10),
        "the boot day counts from zero"
    );

    // The date rolls: the session keeps counting, the day starts over.
    assert_eq!(
        odo.day((900, 10), "2026-09-09"),
        (0, 0),
        "the day does not survive midnight"
    );
    assert_eq!(odo.day((1000, 15), "2026-09-09"), (100, 5));
    assert_eq!(
        odo.day_baseline,
        (900, 10),
        "the baseline is the session at the rollover"
    );

    // A second midnight: yesterday's traffic is not today's.
    assert_eq!(odo.day((5000, 15), "2026-09-10"), (0, 0));
    assert_eq!(odo.day((5001, 15), "2026-09-10"), (1, 0));
}

/// A fresh odometer opens its day where the session is when first read --
/// the test states start it empty -- and `at_boot` opens it at zero, on
/// today's date, so nothing moved before the first poll is lost from it.
#[test]
fn a_first_read_opens_the_day() {
    let mut odo = Odometer::default();
    assert_eq!(odo.day((300, 0), "2026-10-01"), (0, 0));
    let mut boot = Odometer::at_boot();
    let today = boot.day_date.clone();
    assert_eq!(boot.day((300, 0), &today), (300, 0));
}

/// `state` with the key accepted in the query too (`api_key_in_query`).
fn state_q(key: &str, password_hash: &str) -> AppState {
    state_with(key, password_hash, true)
}

fn state(key: &str, password_hash: &str) -> AppState {
    state_with(key, password_hash, false)
}

fn state_with(key: &str, password_hash: &str, in_query: bool) -> AppState {
    let mut cfg = Config::default();
    cfg.daemon.api_key_in_query = in_query;
    cfg.daemon.api_key = key.into();
    cfg.auth.password_hash = password_hash.into();
    cfg.auth.username = "admin".into();
    AppState {
        imports: Default::default(),
        page_refreshes: Default::default(),
        boot_roots: Arc::new(crate::pathroots::BootRoots::from_config(&cfg)),
        config: Arc::new(std::sync::RwLock::new(Arc::new(cfg))),
        config_path: std::path::PathBuf::from("/nonexistent.toml"),
        update_check: Arc::new(tokio::sync::Mutex::new(None)),
        // offline, not start: a unit test must not open a listener.
        engines: Arc::new(crate::engines::EngineHost::offline(
            &Config::default(),
            std::path::Path::new("/tmp/hydra-test-engines"),
        )),
        store: Arc::new(crate::store::StoreLock::new(
            crate::store::Store::open_in_memory().unwrap(),
        )),
        public_ip: Arc::new(tokio::sync::Mutex::new((String::new(), String::new()))),
        net_engines: Arc::new(tokio::sync::Mutex::new((Vec::new(), 0))),
        odometer: Default::default(),
        records: Default::default(),
        bench_path: std::path::PathBuf::new(),
        started_at: 0,
        logs: crate::logbuf::LogBuffer::new(),
        reconnect: Default::default(),
        bench: None,
        sessions: Default::default(),
        spread: Default::default(),
    }
}

/// An unconfigured instance whose config is a real file, so first-run
/// setup can write the account.
fn unconfigured(name: &str) -> (AppState, std::path::PathBuf) {
    let path = std::env::temp_dir().join(format!("hydra-setup-{}-{name}.toml", std::process::id()));
    std::fs::write(&path, include_str!("../../../configs/default.toml")).unwrap();
    let mut st = state("k", "");
    st.config_path = path.clone();
    (st, path)
}

async fn setup_call(st: &AppState, body: &str) -> (StatusCode, String) {
    let peer: std::net::SocketAddr = "127.0.0.1:50000".parse().unwrap();
    let r = crate::bootstrap::setup_password(
        State(st.clone()),
        axum::extract::ConnectInfo(peer),
        HeaderMap::new(),
        body.to_string(),
    )
    .await;
    let status = r.status();
    let bytes = axum::body::to_bytes(r.into_body(), usize::MAX)
        .await
        .unwrap();
    (status, String::from_utf8_lossy(&bytes).into_owned())
}

/// Being local is not proof of owning the instance: behind Docker's port
/// forwarding or a NAT every caller is loopback or private. The code in
/// the daemon's log is, so setup wants it even from 127.0.0.1.
#[tokio::test]
async fn first_run_setup_wants_the_log_code_even_from_loopback() {
    let (st, path) = unconfigured("nocode");
    let before = std::fs::read_to_string(&path).unwrap();
    for body in [
        r#"{"username":"x","password":"AAAAAAAA"}"#,
        r#"{"username":"x","password":"AAAAAAAA","setup_code":"WRONG-CODE2"}"#,
    ] {
        let (status, _) = setup_call(&st, body).await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
    }
    assert_eq!(
        std::fs::read_to_string(&path).unwrap(),
        before,
        "no account written"
    );
    assert!(st.cfg().auth.password_hash.is_empty());
    let _ = std::fs::remove_file(&path);
}

/// The right code creates the account once, never shows up in an
/// answer, and is gone afterwards.
#[tokio::test]
async fn the_log_code_creates_the_account_once() {
    let (st, path) = unconfigured("code");
    crate::bootstrap::set_setup_code_for_test("ABCDE-FGH23");
    let (status, text) =
        setup_call(&st, r#"{"password":"AAAAAAAA","setup_code":"WRONG-CODE2"}"#).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert!(!text.contains("ABCDE") && !text.contains("FGH23"), "{text}");
    let (status, text) = setup_call(
        &st,
        r#"{"password":"AAAAAAAA","setup_code":" abcde fgh23 "}"#,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{text}");
    assert!(!st.cfg().auth.password_hash.is_empty());
    assert!(
        !crate::bootstrap::setup_code_pending(),
        "the code dies with the first account"
    );
    let (status, _) =
        setup_call(&st, r#"{"password":"BBBBBBBB","setup_code":"ABCDE-FGH23"}"#).await;
    assert_eq!(status, StatusCode::CONFLICT);
    let _ = std::fs::remove_file(&path);
}

fn with_key(value: &str) -> HeaderMap {
    let mut h = HeaderMap::new();
    h.insert("X-Api-Key", value.parse().unwrap());
    h
}

fn with_cookie(value: &str) -> HeaderMap {
    let mut h = HeaderMap::new();
    h.insert(axum::http::header::COOKIE, value.parse().unwrap());
    h
}

/// The *arr stack's path in, asserted from the outside.
///
/// Sonarr, Radarr, autobrr and cross-seed configure a "qBittorrent" with a
/// username and a password and cannot set a header, so a session cookie is
/// the only credential they can present. Before this, `/api/v2/auth/login`
/// answered `Ok.` to anything and set no cookie, and the very next request
/// took a silent 401 -- which is exactly how it looked in production when
/// the placeholder key stopped being a free pass.
#[test]
fn a_live_session_cookie_authorises_and_nothing_else_does() {
    let s = state("secret", "$2a$hash");
    let sid = s.sessions.create();

    assert!(
        authorised(&s, &with_cookie(&format!("SID={sid}")), ""),
        "a minted session must pass without any key"
    );
    assert!(
        authorised(&s, &with_cookie(&format!("other=1; SID={sid}; x=2")), ""),
        "and must still be found among other cookies"
    );
    assert!(
        !authorised(&s, &with_cookie("SID=deadbeef"), ""),
        "an invented session must be refused"
    );
    assert!(
        !authorised(&s, &with_cookie("SID="), ""),
        "an empty session must be refused"
    );
    assert!(
        !authorised(&s, &with_cookie("XSID=".to_string().as_str()), ""),
        "a cookie that merely ends in SID is not the session cookie"
    );

    // Logging out must actually end it, or a stolen cookie outlives the
    // client that dropped it.
    s.sessions.revoke(&sid);
    assert!(!authorised(&s, &with_cookie(&format!("SID={sid}")), ""));
}

/// A cookie is a credential, so it cannot rescue an instance that has none.
///
/// Without this, `authorised` returning on the session branch would be a
/// second way past the empty-key rule the test above pins down.
#[test]
fn a_session_cannot_open_an_instance_with_no_key() {
    let open = state("", "$2a$hash");
    let sid = open.sessions.create();
    assert!(!authorised(&open, &with_cookie(&format!("SID={sid}")), ""));
}

/// The login accepts the key, and the key alone is not a blank cheque.
///
/// This is what let the seven broken clients be fixed by typing what they
/// already held into a field they already had, instead of rotating an
/// admin password nobody had written down.
#[test]
fn the_api_key_is_accepted_where_a_password_is_expected() {
    // bcrypt("hunter2"), so the admin path is real and not stubbed.
    let hash = bcrypt::hash("hunter2", 4).unwrap();
    let s = state("secret-key", &hash);

    assert!(
        qbit_creds_ok(&s, "admin", "hunter2"),
        "the admin account works"
    );
    assert!(
        qbit_creds_ok(&s, "admin", "secret-key"),
        "so does the API key"
    );
    // Any username with the key: the clients let people type anything
    // there, and the key is what is being checked.
    assert!(qbit_creds_ok(&s, "whatever", "secret-key"));

    assert!(
        !qbit_creds_ok(&s, "admin", "wrong"),
        "a wrong password is refused"
    );
    assert!(
        !qbit_creds_ok(&s, "notadmin", "hunter2"),
        "the right password under the wrong name is refused"
    );
    assert!(
        !qbit_creds_ok(&s, "admin", ""),
        "an empty password is refused"
    );
}

/// An instance with nothing configured authorises nobody here either.
///
/// Without the empty guard, an empty password would compare equal to an
/// empty key and log anyone in -- the 4.14 hole, in a new doorway.
#[test]
fn an_unconfigured_instance_issues_no_session() {
    let fresh = state("", "");
    assert!(!qbit_creds_ok(&fresh, "admin", ""));
    assert!(!qbit_creds_ok(&fresh, "admin", "anything"));
    assert!(!qbit_creds_ok(&fresh, "", ""));
}

/// The real decision `qbit_login` makes, called directly.
fn qbit_creds_ok(state: &AppState, username: &str, password: &str) -> bool {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(async {
            let cfg = state.cfg();
            qbit_key_ok(&cfg, password)
                || qbit_password_ok(&cfg, username, password).await == Ok(true)
        })
}

/// A bcrypt check is a quarter of a second of CPU. Run inline in the
/// handler it held the worker thread, and every task sharing it waited;
/// a burst of logins stalled the API. On a one-thread runtime the ticker
/// below only advances if the login lets go of the thread meanwhile.
#[tokio::test(flavor = "current_thread")]
async fn a_login_leaves_the_runtime_free_while_bcrypt_runs() {
    use std::sync::atomic::{AtomicUsize, Ordering::SeqCst};
    let st = state("k", &bcrypt::hash("right-password", 8).unwrap());
    let ticks = Arc::new(AtomicUsize::new(0));
    let t = ticks.clone();
    let ticker = tokio::spawn(async move {
        loop {
            t.fetch_add(1, SeqCst);
            tokio::task::yield_now().await;
        }
    });
    tokio::task::yield_now().await;

    let before = ticks.load(SeqCst);
    let peer = axum::extract::ConnectInfo("192.0.2.1:40000".parse().unwrap());
    let r = qbit_login(
        State(st.clone()),
        peer,
        "username=admin&password=wrong".into(),
    )
    .await;
    let during_qbit = ticks.load(SeqCst) - before;
    assert_eq!(r.status(), StatusCode::OK);

    let before = ticks.load(SeqCst);
    let r = crate::bootstrap::login(
        State(st.clone()),
        peer,
        r#"{"username":"admin","password":"wrong"}"#.into(),
    )
    .await;
    let during_web = ticks.load(SeqCst) - before;
    assert_eq!(r.status(), StatusCode::UNAUTHORIZED);
    ticker.abort();

    assert!(
        during_qbit > 0,
        "/api/v2/auth/login held the runtime for its bcrypt check"
    );
    assert!(
        during_web > 0,
        "/api/login held the runtime for its bcrypt check"
    );
}

/// Five wrong passwords from one caller block it on both login routes,
/// through the real router. While blocked the right password is refused
/// too; on the qBittorrent shim the API key still opens a session; other
/// callers are untouched.
#[tokio::test]
async fn failed_logins_block_the_caller_on_both_routes() {
    use tower::ServiceExt;
    let st = state("the-api-key", &bcrypt::hash("right-password", 4).unwrap());
    let call = |ip: &str, req: axum::http::Request<axum::body::Body>| {
        let peer = std::net::SocketAddr::new(ip.parse().unwrap(), 40000);
        super::router(st.clone())
            .layer(axum::extract::connect_info::MockConnectInfo(peer))
            .oneshot(req)
    };
    let web = |password: &str| {
        axum::http::Request::builder()
            .method("POST")
            .uri("/api/login")
            .header("content-type", "application/json")
            .body(axum::body::Body::from(
                serde_json::json!({"username": "admin", "password": password}).to_string(),
            ))
            .unwrap()
    };
    let qbit = |password: &str| {
        axum::http::Request::builder()
            .method("POST")
            .uri("/api/v2/auth/login")
            .header("content-type", "application/x-www-form-urlencoded")
            .body(axum::body::Body::from(format!(
                "username=admin&password={password}"
            )))
            .unwrap()
    };
    let text = |r: Response| async move {
        String::from_utf8_lossy(
            &axum::body::to_bytes(r.into_body(), usize::MAX)
                .await
                .unwrap(),
        )
        .into_owned()
    };

    for _ in 0..3 {
        assert_eq!(
            call("203.0.113.9", web("wrong")).await.unwrap().status(),
            StatusCode::UNAUTHORIZED
        );
    }
    for _ in 0..2 {
        let r = call("203.0.113.9", qbit("wrong")).await.unwrap();
        assert_eq!(text(r).await, "Fails.");
    }

    let r = call("203.0.113.9", web("right-password")).await.unwrap();
    assert_eq!(
        r.status(),
        StatusCode::TOO_MANY_REQUESTS,
        "blocked, right password or not"
    );
    assert!(r.headers().contains_key(axum::http::header::RETRY_AFTER));
    let r = call("203.0.113.9", qbit("right-password")).await.unwrap();
    assert_eq!(r.status(), StatusCode::FORBIDDEN);
    let r = call("203.0.113.9", qbit("the-api-key")).await.unwrap();
    assert!(
        r.headers().contains_key(axum::http::header::SET_COOKIE),
        "the key still opens a session"
    );
    assert_eq!(text(r).await, "Ok.");
    let r = call("203.0.113.9", web("right-password")).await.unwrap();
    assert_eq!(
        r.status(),
        StatusCode::TOO_MANY_REQUESTS,
        "and the key lifted nothing"
    );

    let r = call("198.51.100.7", web("right-password")).await.unwrap();
    assert_eq!(r.status(), StatusCode::OK, "another caller is not blocked");
}

#[test]
fn a_form_body_is_split_like_a_query() {
    assert_eq!(
        form_field("username=admin&password=p", "username").as_deref(),
        Some("admin")
    );
    assert_eq!(
        form_field("username=admin&password=p", "password").as_deref(),
        Some("p")
    );
    // The passwords people actually use.
    assert_eq!(
        form_field("password=a%2Bb%26c", "password").as_deref(),
        Some("a+b&c")
    );
    assert_eq!(
        form_field("password=a+b", "password").as_deref(),
        Some("a b")
    );
    assert_eq!(form_field("username=admin", "password"), None);
}

#[test]
fn a_real_key_is_required_and_checked() {
    let s = state("secret", "$2a$hash");
    assert!(
        !authorised(&s, &HeaderMap::new(), ""),
        "no key must be refused"
    );
    assert!(
        !authorised(&s, &with_key("wrong"), ""),
        "wrong key must be refused"
    );
    assert!(
        authorised(&s, &with_key("secret"), ""),
        "right key must pass"
    );
}

#[test]
fn the_key_may_arrive_as_a_query_parameter() {
    let s = state_q("secret", "$2a$hash");
    assert!(authorised(&s, &HeaderMap::new(), "apikey=secret"));
    assert!(authorised(&s, &HeaderMap::new(), "other=1&apikey=secret"));
    assert!(!authorised(&s, &HeaderMap::new(), "apikey=wrong"));
}

/// The hole this closes, asserted from the outside.
///
/// A fresh Docker install had `api_key = ""` from the shipped template.
/// `provided == expected` then made *sending nothing* equal *having
/// nothing*: no key got 200, a wrong key got 401. Measured on 4.14.0
/// against a container with a virgin /config.
///
/// Both halves matter. That an empty key refuses a caller who sends
/// nothing is the fix; that it also refuses one who sends the empty string
/// is what stops the same equality sneaking back in another spelling.
#[test]
fn an_instance_with_no_key_authorises_nobody() {
    let open = state("", "$2a$hash");
    assert!(
        !authorised(&open, &HeaderMap::new(), ""),
        "no key configured must refuse a caller who sends none"
    );
    assert!(
        !authorised(&open, &with_key(""), ""),
        "nor one who sends an empty key"
    );
    assert!(
        !authorised(&open, &HeaderMap::new(), "apikey="),
        "nor one who sends an empty key in the query"
    );
    assert!(
        !authorised(&open, &with_key("anything"), ""),
        "nor anyone else"
    );

    // Same before setup: an unconfigured install must not be wide open
    // either, which is the half a refactor would quietly drop.
    let fresh = state("", "");
    assert!(!authorised(&fresh, &HeaderMap::new(), ""));
}

/// The placeholder is neither a bypass nor a key.
///
/// It used to switch the check off entirely once an admin password
/// existed -- which production had, so production authenticated nothing.
/// It was then compared like any other value, so an install whose config
/// could not be rewritten (`config::ensure_api_key` keeps the old value
/// then) still opened to the one string every reader of the repository
/// knows. It opens nothing now, at any door that compares a key.
#[test]
fn the_placeholder_key_is_neither_a_bypass_nor_a_key() {
    let configured = state(DEFAULT_API_KEY, "$2a$hash");
    assert!(
        !authorised(&configured, &HeaderMap::new(), ""),
        "placeholder + password set must NOT wave a keyless caller through"
    );
    assert!(
        !authorised(&configured, &with_key(DEFAULT_API_KEY), ""),
        "the published placeholder must not open the door as a key"
    );
    assert!(
        !key_authorised(&configured, &with_key(DEFAULT_API_KEY), ""),
        "nor at the gate the MCP endpoint and the key-only routes use"
    );
    assert!(
        !qbit_key_ok(&configured.cfg(), DEFAULT_API_KEY),
        "nor as the qBittorrent password"
    );

    // The rescue router reads the key from the file itself.
    let path = std::env::temp_dir().join(format!(
        "hydra-placeholder-rescue-{}.toml",
        std::process::id()
    ));
    std::fs::write(
        &path,
        format!("[daemon]\napi_key = \"{DEFAULT_API_KEY}\"\n"),
    )
    .unwrap();
    let rescue = RescueState {
        diagnosis: Default::default(),
        config_path: path.clone(),
    };
    assert!(
        !rescue_authorised(&rescue, &with_key(DEFAULT_API_KEY)),
        "nor on the rescue router"
    );
    std::fs::write(
        &path,
        "[daemon]\napi_key = \"a-real-key-0123456789abcdef\"\n",
    )
    .unwrap();
    assert!(
        rescue_authorised(&rescue, &with_key("a-real-key-0123456789abcdef")),
        "a real key still opens it"
    );
    let _ = std::fs::remove_file(&path);

    let real = state("a-real-key-0123456789abcdef", "$2a$hash");
    assert!(authorised(
        &real,
        &with_key("a-real-key-0123456789abcdef"),
        ""
    ));
    assert!(qbit_key_ok(&real.cfg(), "a-real-key-0123456789abcdef"));
}

#[test]
fn keys_compare_in_constant_time_but_still_compare() {
    assert!(constant_time_eq(b"secret", b"secret"));
    assert!(!constant_time_eq(b"secret", b"secrey"));
    assert!(!constant_time_eq(b"secret", b"secret-longer"));
    assert!(!constant_time_eq(b"", b"x"));
    assert!(constant_time_eq(b"", b""));
}

// The trap this guards: "3.9.0" is lexically greater than "3.180.0", so a
// string comparison would offer 3.9.0 as an upgrade from 3.180.0.
#[test]
/// Locks the 2026-09-12 regression: adding a torrent PAUSED must still
/// hash-check data already on disk. The `!paused` gate that used to live
/// here left a complete 184 GB torrent reporting 0%, with a Verify button
/// that was itself a stub -- the two holes covered each other.
fn a_paused_add_still_checks_the_data_on_disk() {
    // The case that regressed: paused is simply not an input.
    assert!(add_recheck_wanted(false, true));
}

#[test]
fn an_add_with_no_data_on_disk_checks_nothing() {
    assert!(!add_recheck_wanted(false, false));
}

#[test]
fn seed_mode_keeps_its_trust_fast_path() {
    // skip_checking is an assertion by the caller; honour it.
    assert!(!add_recheck_wanted(true, true));
    assert!(!add_recheck_wanted(true, false));
}

#[test]
fn a_local_engine_is_named_local_something() {
    assert_eq!(local_agent("race"), "local-race");
    assert_eq!(local_agent("hoard"), "local-hoard");
}

#[test]
fn versions_compare_numerically_not_lexically() {
    assert!(version_less("3.9.0", "3.180.0"));
    assert!(!version_less("3.180.0", "3.9.0"));
    assert!(version_less("3.180.0-typhon", "v3.181.0"));
    assert!(!version_less("3.180.0-typhon", "v3.180.0"));
}

// Go hands encoding/json a float64, which prints an integral value as "0".
// Emitting 0.0 would change the bytes every client parses.
#[test]
fn integral_floats_are_emitted_as_integers() {
    let parsed: toml::Value = toml::from_str("a = 0.0\nb = 1.5\nc = 3\n").unwrap();
    let json = toml_to_json(&parsed);
    assert_eq!(
        serde_json::to_string(&json).unwrap(),
        r#"{"a":0,"b":1.5,"c":3}"#
    );
}

#[test]
fn only_three_part_numeric_tags_are_considered() {
    assert!(is_semver_tag("v3.180.0"));
    assert!(is_semver_tag("3.180.0"));
    assert!(!is_semver_tag("v3.180"));
    assert!(!is_semver_tag("nightly"));
    assert!(!is_semver_tag("v3.180.0-rc1"));
}

#[test]
fn percent_encoding_is_decoded_in_the_query_fallback() {
    let s = state_q("a b", "$2a$hash");
    assert!(authorised(&s, &HeaderMap::new(), "apikey=a%20b"));
    assert!(authorised(&s, &HeaderMap::new(), "apikey=a+b"));
}
#[test]
fn agent_ids_carry_the_local_prefix() {
    assert_eq!(local_agent("race"), "local-race");
    assert_eq!(local_agent("hoard"), "local-hoard");
}
