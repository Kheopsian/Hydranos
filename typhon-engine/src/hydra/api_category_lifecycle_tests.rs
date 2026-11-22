use super::testing::*;
use super::*;

const KEY: &str = "0123456789abcdef0123456789abcdef";

fn labelled(s: &TestState, hash: &str, cat: &str) {
    s.state
        .store
        .lock()
        .unwrap()
        .insert_torrent(
            hash,
            "hoard",
            b"d4:infod4:name1:xee",
            "/data",
            cat,
            0.0,
            false,
            "",
        )
        .unwrap();
}
fn label_of(s: &TestState, hash: &str) -> String {
    s.state
        .store
        .lock()
        .unwrap()
        .category_of(hash, "hoard")
        .unwrap_or_default()
}

/// Rename moves the definition AND the torrents' labels; delete clears
/// the labels; a mode in another case is normalised, an unknown one
/// refused; an orphaned label is listed with what its torrents do.
#[tokio::test]
async fn a_category_lives_and_dies_with_its_labels() {
    let s = state_from("cat-life", &format!("[daemon]\napi_key = \"{KEY}\"\n"));
    let h1 = "a".repeat(40);
    let r = category_create(
        State(s.state.clone()),
        RawQuery(None),
        keyed(KEY),
        r#"{"name":"films","save_path":"/data/films","mode":"Hoard"}"#.into(),
    )
    .await;
    assert_eq!(r.status(), StatusCode::CREATED);
    assert_eq!(
        categories_map(&s.state)["films"].mode,
        "hoard",
        "normalised"
    );
    let r = category_create(
        State(s.state.clone()),
        RawQuery(None),
        keyed(KEY),
        r#"{"name":"x","save_path":"/x","mode":"turbo"}"#.into(),
    )
    .await;
    assert_eq!(r.status(), StatusCode::BAD_REQUEST);

    labelled(&s, &h1, "films");
    let r = category_update(
        State(s.state.clone()),
        Path("films".into()),
        RawQuery(None),
        keyed(KEY),
        r#"{"name":"movies","save_path":"/data/films","mode":"hoard"}"#.into(),
    )
    .await;
    assert_eq!(r.status(), StatusCode::OK);
    assert!(
        categories_map(&s.state).contains_key("movies")
            && !categories_map(&s.state).contains_key("films")
    );
    assert_eq!(label_of(&s, &h1), "movies");

    let v = body_json(
        category_delete(
            State(s.state.clone()),
            Path("movies".into()),
            RawQuery(None),
            keyed(KEY),
        )
        .await,
    )
    .await;
    assert_eq!(v["cleared"], 1);
    assert_eq!(label_of(&s, &h1), "");

    let h2 = "b".repeat(40);
    labelled(&s, &h2, "ghost");
    let v =
        body_json(get_categories_orphans(State(s.state.clone()), RawQuery(None), keyed(KEY)).await)
            .await;
    assert_eq!(v[0]["name"], "ghost");
    assert_eq!(v[0]["torrents"], 1);
    assert_eq!(v[0]["mode"], "hoard");
    assert_eq!(v[0]["save_path"], "/data");
}

/// The qBittorrent shim (addTags, removeTags, deleteTags) and the native
/// tags route (the UI editor, MCP `set_tags`) keep the date of each tag.
#[tokio::test]
async fn the_shim_and_the_native_route_keep_the_dates_of_the_tags() {
    let s = state_from(
        "tag-since-shim",
        &format!("[daemon]\napi_key = \"{KEY}\"\n"),
    );
    let h = "d".repeat(40);
    labelled(&s, &h, "films");
    s.state
        .store
        .lock()
        .unwrap()
        .put_meta("tag_since_migrated_at", "1000")
        .unwrap();
    let form = |pairs: &[(&str, &str)]| -> Form<Fields> {
        Form(
            pairs
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
        )
    };
    let rows = || s.state.store.lock().unwrap().tag_since_rows(&h);
    let names = || rows().into_iter().map(|r| r.0).collect::<Vec<_>>();

    let r = qbit_add_tags(
        State(s.state.clone()),
        RawQuery(None),
        keyed(KEY),
        form(&[("hashes", h.as_str()), ("tags", "x,y")]),
    )
    .await;
    assert!(r.status().is_success());
    assert_eq!(names(), ["x", "y"], "addTags");
    assert!(rows().iter().all(|r| r.1 > 1000));

    let r = qbit_remove_tags(
        State(s.state.clone()),
        RawQuery(None),
        keyed(KEY),
        form(&[("hashes", h.as_str()), ("tags", "x")]),
    )
    .await;
    assert!(r.status().is_success());
    assert_eq!(names(), ["y"], "removeTags");

    let r = set_torrent_tags(
        State(s.state.clone()),
        Path(h.clone()),
        RawQuery(None),
        keyed(KEY),
        r#"{"tags":["z"]}"#.into(),
    )
    .await;
    assert!(r.status().is_success());
    assert_eq!(names(), ["z"], "the native route replaces the set");

    let r = qbit_delete_tags(
        State(s.state.clone()),
        RawQuery(None),
        keyed(KEY),
        form(&[("tags", "z")]),
    )
    .await;
    assert!(r.status().is_success());
    assert!(rows().is_empty(), "deleteTags");
}

/// A write the store refused is never acknowledged. Every statement that
/// changes a category, a tag, a pause or a pin fails here with the error a
/// dead network mount gives ("database disk image is malformed"); the route
/// must say so, not answer 2xx for a change that was never written.
#[tokio::test]
async fn a_write_the_store_refused_is_not_acknowledged() {
    let s = state_from("cat-refused", &format!("[daemon]\napi_key = \"{KEY}\"\n"));
    let h = "c".repeat(40);
    labelled(&s, &h, "films");
    let r = category_create(
        State(s.state.clone()),
        RawQuery(None),
        keyed(KEY),
        r#"{"name":"films","save_path":"/data/films","mode":"hoard"}"#.into(),
    )
    .await;
    assert_eq!(r.status(), StatusCode::CREATED);

    let broken = "RAISE(ABORT, 'database disk image is malformed')";
    // A tag that exists, so that deleting it has a row to delete.
    let mut sql = String::from("INSERT INTO tag_registry (name) VALUES ('x');");
    for (table, events) in [
        ("meta", ["INSERT", "UPDATE", "DELETE"]),
        ("tag_registry", ["INSERT", "UPDATE", "DELETE"]),
        ("torrents", ["INSERT", "UPDATE", "DELETE"]),
    ] {
        for ev in events {
            sql += &format!("CREATE TRIGGER broken_{table}_{ev} BEFORE {ev} ON {table} BEGIN SELECT {broken}; END;");
        }
    }
    s.state.store.lock().unwrap().execute_batch_for_tests(&sql);

    fn form(pairs: &[(&str, &str)]) -> Form<Fields> {
        Form(
            pairs
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
        )
    }
    let st = s.state.clone();
    let p = |n: &str| Path(n.to_string());
    let hp = || Path(h.clone());
    let mut seen: Vec<(&str, StatusCode)> = Vec::new();
    macro_rules! call {
        ($name:expr, $fut:expr) => {
            seen.push(($name, $fut.await.status()))
        };
    }
    call!(
        "POST /api/categories",
        category_create(
            State(st.clone()),
            RawQuery(None),
            keyed(KEY),
            r#"{"name":"series","save_path":"/data/series","mode":"hoard"}"#.into()
        )
    );
    call!(
        "PUT /api/categories/:name",
        category_update(
            State(st.clone()),
            p("films"),
            RawQuery(None),
            keyed(KEY),
            r#"{"save_path":"/data/other"}"#.into()
        )
    );
    call!(
        "DELETE /api/categories/:name",
        category_delete(State(st.clone()), p("films"), RawQuery(None), keyed(KEY))
    );
    call!(
        "qbit createCategory",
        qbit_create_category(
            State(st.clone()),
            RawQuery(None),
            keyed(KEY),
            form(&[("category", "anime"), ("savePath", "/data/anime")])
        )
    );
    call!(
        "qbit editCategory",
        qbit_edit_category(
            State(st.clone()),
            RawQuery(None),
            keyed(KEY),
            form(&[("category", "films"), ("savePath", "/data/other")])
        )
    );
    call!(
        "qbit removeCategories",
        qbit_remove_categories(
            State(st.clone()),
            RawQuery(None),
            keyed(KEY),
            form(&[("categories", "films")])
        )
    );
    call!(
        "qbit createTags",
        qbit_create_tags(
            State(st.clone()),
            RawQuery(None),
            keyed(KEY),
            form(&[("tags", "x")])
        )
    );
    call!(
        "qbit deleteTags",
        qbit_delete_tags(
            State(st.clone()),
            RawQuery(None),
            keyed(KEY),
            form(&[("tags", "x")])
        )
    );
    call!(
        "qbit addTags",
        qbit_add_tags(
            State(st.clone()),
            RawQuery(None),
            keyed(KEY),
            form(&[("hashes", h.as_str()), ("tags", "x")])
        )
    );
    call!(
        "qbit removeTags",
        qbit_remove_tags(
            State(st.clone()),
            RawQuery(None),
            keyed(KEY),
            form(&[("hashes", h.as_str()), ("tags", "x")])
        )
    );
    call!(
        "qbit setCategory",
        qbit_set_category(
            State(st.clone()),
            RawQuery(None),
            keyed(KEY),
            form(&[("hashes", h.as_str()), ("category", "films")])
        )
    );
    call!(
        "qbit pause",
        qbit_pause(
            State(st.clone()),
            RawQuery(None),
            keyed(KEY),
            form(&[("hashes", h.as_str())])
        )
    );
    call!(
        "qbit resume",
        qbit_resume(
            State(st.clone()),
            RawQuery(None),
            keyed(KEY),
            form(&[("hashes", h.as_str())])
        )
    );
    call!(
        "POST torrent tags",
        set_torrent_tags(
            State(st.clone()),
            hp(),
            RawQuery(None),
            keyed(KEY),
            r#"{"tags":["x"]}"#.into()
        )
    );
    call!(
        "POST torrent category",
        set_torrent_category(
            State(st.clone()),
            hp(),
            RawQuery(None),
            keyed(KEY),
            r#"{"category":"films"}"#.into()
        )
    );
    call!(
        "POST torrent pause",
        hoard_pause_one(
            State(st.clone()),
            hp(),
            RawQuery(None),
            keyed(KEY),
            String::new()
        )
    );
    call!(
        "POST torrent resume",
        hoard_resume_one(
            State(st.clone()),
            hp(),
            RawQuery(None),
            keyed(KEY),
            String::new()
        )
    );
    call!(
        "POST torrent pin",
        hoard_pin_one(
            State(st.clone()),
            hp(),
            RawQuery(None),
            keyed(KEY),
            String::new()
        )
    );
    call!(
        "DELETE torrent pin",
        hoard_unpin_one(State(st.clone()), hp(), RawQuery(None), keyed(KEY))
    );

    let acked: Vec<String> = seen
        .iter()
        .filter(|(_, c)| c.is_success())
        .map(|(n, c)| format!("{n} -> {c}"))
        .collect();
    assert!(
        acked.is_empty(),
        "acknowledged although the store refused the write: {acked:?}"
    );
    for (n, c) in &seen {
        assert!(c.is_server_error(), "{n}: {c}");
    }
}

/// ⭐ A rename follows every reference: another category's graduate_to,
/// an RSS rule, a watched folder, a workflow's condition and action.
#[tokio::test]
async fn a_rename_follows_every_reference() {
    let s = state_from("cat-follow", &format!("[daemon]\napi_key = \"{KEY}\"\n"));
    for body in [
        r#"{"name":"films","save_path":"/data/films","mode":"hoard"}"#,
        r#"{"name":"race-films","save_path":"/race/films","mode":"race","graduate_to":"films"}"#,
    ] {
        let r = category_create(
            State(s.state.clone()),
            RawQuery(None),
            keyed(KEY),
            body.into(),
        )
        .await;
        assert_eq!(r.status(), StatusCode::CREATED);
    }
    {
        let store = s.state.store.lock().unwrap();
        store.rss_rule_put("r1", r#"{"enabled":true,"mustContain":"x","assignedCategory":"films","torrentParams":{"category":"films"}}"#).unwrap();
        store
            .put_setting("watch_folders", r#"[{"path":"/w","category":"films"}]"#)
            .unwrap();
        let w = serde_json::json!({
            "id": "w1", "name": "w", "enabled": false,
            "when": {"kind": "cond", "field": "category", "op": "eq", "value": "Films"},
            "then": [{"type": "set_category", "to": "films"}],
        });
        store
            .put_workflow(&crate::store::StoredWorkflow {
                id: "w1".into(),
                name: "w".into(),
                body: w.to_string(),
                enabled: false,
                position: 0,
                interval_secs: 0,
                last_run: 0,
            })
            .unwrap();
    }
    let r = category_update(
        State(s.state.clone()),
        Path("films".into()),
        RawQuery(None),
        keyed(KEY),
        r#"{"name":"movies"}"#.into(),
    )
    .await;
    let v = body_json(r).await;
    assert_eq!(v["followed"]["rss_rules"], 1, "{v}");
    assert_eq!(categories_map(&s.state)["race-films"].graduate_to, "movies");
    assert_eq!(categories_map(&s.state)["movies"].mode, "hoard");
    let store = s.state.store.lock().unwrap();
    let (_, rule) = store
        .rss_rules()
        .unwrap()
        .into_iter()
        .find(|(n, _)| n == "r1")
        .unwrap();
    assert!(
        rule.contains("movies") && !rule.contains("\"films\""),
        "{rule}"
    );
    assert!(store
        .setting("watch_folders")
        .unwrap()
        .unwrap()
        .contains("\"movies\""));
    let body = &store.workflows().unwrap()[0].body;
    assert!(!body.to_lowercase().contains("\"films\""), "{body}");
}

/// A PUT naming only what changes keeps the rest: a missing `mode` used
/// to read as race, and the drain purges race.
#[tokio::test]
async fn a_partial_update_keeps_the_mode() {
    let s = state_from("cat-partial", &format!("[daemon]\napi_key = \"{KEY}\"\n"));
    let r = category_create(
        State(s.state.clone()),
        RawQuery(None),
        keyed(KEY),
        r#"{"name":"films","save_path":"/data/films","mode":"hoard"}"#.into(),
    )
    .await;
    assert_eq!(r.status(), StatusCode::CREATED);
    let r = category_update(
        State(s.state.clone()),
        Path("films".into()),
        RawQuery(None),
        keyed(KEY),
        r#"{"save_path":"/data/movies"}"#.into(),
    )
    .await;
    assert_eq!(r.status(), StatusCode::OK);
    let c = &categories_map(&s.state)["films"];
    assert_eq!(c.mode, "hoard", "a PUT without mode made it race");
    assert_eq!(c.save_path, "/data/movies");
}
