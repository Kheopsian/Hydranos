//! qBittorrent-compatible `/api/v2` shim: the routes a qBittorrent client
//! calls, and the handlers behind them. Kept apart from `api.rs` so the shim
//! can be read and changed on its own. Routes are merged into the main router
//! by `api::router_with`.

use super::*;

/// Every `/api/v2` route. Merged into the main router by `api::router_with`.
pub(super) fn routes() -> Router<AppState> {
    Router::new()
        .route("/api/v2/torrents/delete", axum::routing::post(qbit_delete))
        .route(
            "/api/v2/torrents/add",
            axum::routing::post(qbit_torrent_add)
                .layer(axum::extract::DefaultBodyLimit::max(ADD_BODY_LIMIT)),
        )
        .route(
            "/api/v2/app/setPreferences",
            axum::routing::post(qbit_set_preferences),
        )
        .route(
            "/api/v2/torrents/reannounce",
            axum::routing::post(qbit_reannounce),
        )
        .route(
            "/api/v2/torrents/recheck",
            axum::routing::post(qbit_recheck),
        )
        .route(
            "/api/v2/torrents/categories",
            axum::routing::any(qbit_categories),
        )
        .route("/api/v2/torrents/tags", axum::routing::any(qbit_tags))
        .route("/api/v2/app/version", axum::routing::any(qbit_version))
        .route(
            "/api/v2/app/webapiVersion",
            axum::routing::any(qbit_webapi_version),
        )
        .route("/api/v2/app/buildInfo", axum::routing::any(qbit_build_info))
        .route(
            "/api/v2/app/preferences",
            axum::routing::any(qbit_preferences),
        )
        .route(
            "/api/v2/transfer/info",
            axum::routing::any(qbit_transfer_info),
        )
        .route(
            "/api/v2/torrents/info",
            axum::routing::any(qbit_torrents_info),
        )
        .route(
            "/api/v2/torrents/files",
            axum::routing::any(qbit_torrent_files),
        )
        .route(
            "/api/v2/torrents/trackers",
            axum::routing::any(qbit_torrent_trackers),
        )
        .route(
            "/api/v2/torrents/properties",
            axum::routing::any(qbit_torrent_properties),
        )
        .route(
            "/api/v2/torrents/export",
            axum::routing::any(qbit_torrent_export),
        )
        .route(
            "/api/v2/transfer/uploadLimit",
            axum::routing::any(qbit_transfer_upload_limit),
        )
        .route(
            "/api/v2/transfer/downloadLimit",
            axum::routing::any(qbit_transfer_download_limit),
        )
        .route(
            "/api/v2/transfer/setUploadLimit",
            axum::routing::post(qbit_transfer_set_upload_limit),
        )
        .route(
            "/api/v2/transfer/setDownloadLimit",
            axum::routing::post(qbit_transfer_set_download_limit),
        )
        .route(
            "/api/v2/torrents/uploadLimit",
            axum::routing::any(qbit_torrents_upload_limit),
        )
        .route(
            "/api/v2/torrents/downloadLimit",
            axum::routing::any(qbit_torrents_download_limit),
        )
        .route(
            "/api/v2/torrents/setUploadLimit",
            axum::routing::post(qbit_torrents_set_upload_limit),
        )
        .route(
            "/api/v2/torrents/setDownloadLimit",
            axum::routing::post(qbit_torrents_set_download_limit),
        )
        .route(
            "/api/v2/torrents/setShareLimits",
            axum::routing::post(qbit_set_share_limits),
        )
        .route(
            "/api/v2/torrents/setLocation",
            axum::routing::post(qbit_set_location),
        )
        .route(
            "/api/v2/torrents/setForceStart",
            axum::routing::post(qbit_set_force_start),
        )
        .route(
            "/api/v2/torrents/filePrio",
            axum::routing::post(qbit_file_prio),
        )
        .route(
            "/api/v2/torrents/toggleSequentialDownload",
            axum::routing::post(qbit_toggle_sequential),
        )
        .route(
            "/api/v2/torrents/toggleFirstLastPiecePrio",
            axum::routing::post(qbit_toggle_first_last),
        )
        .route("/api/v2/torrents/rename", axum::routing::post(qbit_rename))
        .route(
            "/api/v2/torrents/topPrio",
            axum::routing::post(qbit_queue_prio),
        )
        .route(
            "/api/v2/torrents/bottomPrio",
            axum::routing::post(qbit_queue_prio),
        )
        .route(
            "/api/v2/torrents/increasePrio",
            axum::routing::post(qbit_queue_prio),
        )
        .route(
            "/api/v2/torrents/decreasePrio",
            axum::routing::post(qbit_queue_prio),
        )
        .route(
            "/api/v2/torrents/count",
            axum::routing::any(qbit_torrents_count),
        )
        .route(
            "/api/v2/app/defaultSavePath",
            axum::routing::any(qbit_default_save_path_route),
        )
        .route(
            "/api/v2/sync/maindata",
            axum::routing::any(qbit_sync_maindata),
        )
        .route(
            "/api/v2/torrents/renameFile",
            axum::routing::post(qbit_rename_file),
        )
        .route(
            "/api/v2/torrents/renameFolder",
            axum::routing::post(qbit_rename_folder),
        )
        .route(
            "/api/v2/app/getDirectoryContent",
            axum::routing::any(crate::fslist::qbit_directory_content),
        )
        .route(
            "/api/v2/torrents/createCategory",
            axum::routing::post(qbit_create_category),
        )
        .route(
            "/api/v2/torrents/editCategory",
            axum::routing::post(qbit_edit_category),
        )
        .route(
            "/api/v2/torrents/removeCategories",
            axum::routing::post(qbit_remove_categories),
        )
        .route(
            "/api/v2/torrents/createTags",
            axum::routing::post(qbit_create_tags),
        )
        .route(
            "/api/v2/torrents/deleteTags",
            axum::routing::post(qbit_delete_tags),
        )
        .route(
            "/api/v2/torrents/addTags",
            axum::routing::post(qbit_add_tags),
        )
        .route(
            "/api/v2/torrents/removeTags",
            axum::routing::post(qbit_remove_tags),
        )
        .route("/api/v2/torrents/pause", axum::routing::post(qbit_pause))
        .route("/api/v2/torrents/resume", axum::routing::post(qbit_resume))
        .route(
            "/api/v2/torrents/setCategory",
            axum::routing::post(qbit_set_category),
        )
        .route("/api/v2/torrents/start", axum::routing::post(qbit_start))
        .route("/api/v2/torrents/stop", axum::routing::post(qbit_stop))
        .route(
            "/api/v2/torrents/addTrackers",
            axum::routing::post(qbit_add_trackers),
        )
        .route(
            "/api/v2/torrents/removeTrackers",
            axum::routing::post(qbit_remove_trackers),
        )
        .route("/api/v2/auth/login", axum::routing::post(qbit_login))
        .route("/api/v2/auth/logout", axum::routing::post(qbit_logout))
}

pub(super) async fn qbit_categories(
    State(state): State<AppState>,
    RawQuery(query): RawQuery,
    headers: HeaderMap,
) -> Response {
    let query = query.unwrap_or_default();
    guard!(state, headers, query);
    let cfg = state.cfg();

    let raw = {
        let store = state.store.lock().unwrap();
        store.meta_doc("categories")
    }
    .filter(|d| !d.is_empty())
    .or_else(|| {
        let path = std::path::Path::new(&cfg.daemon.data_dir).join("categories.json");
        std::fs::read_to_string(path).ok()
    });

    let map: std::collections::BTreeMap<String, serde_json::Value> = match raw {
        Some(doc) => serde_json::from_str(&doc).unwrap_or_default(),
        None => Default::default(),
    };

    let mut out = serde_json::Map::new();
    for (name, body) in map {
        let save_path = body
            .get("save_path")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        out.insert(
            name.clone(),
            serde_json::json!({"name": name, "savePath": save_path}),
        );
    }
    Json(serde_json::Value::Object(out)).into_response()
}

pub(super) async fn qbit_tags(
    State(state): State<AppState>,
    RawQuery(query): RawQuery,
    headers: HeaderMap,
) -> Response {
    let query = query.unwrap_or_default();
    guard!(state, headers, query);
    let tags = {
        let store = state.store.lock().unwrap();
        store.registered_tags().unwrap_or_default()
    };
    Json(tags).into_response()
}

/// The `hashes` field of a shim write: qBittorrent's `all` means every
/// torrent. 4.3 split it as a hash named "all", matched nothing and answered
/// 200 -- "pause all" in a client paused nothing.
pub(super) fn qbit_hashes(state: &AppState, form: &Fields) -> Vec<String> {
    let raw = form.get("hashes").map(String::as_str).unwrap_or("");
    if raw.trim().eq_ignore_ascii_case("all") {
        let store = state.store.read().unwrap_or_else(|p| p.into_inner());
        let mut all: Vec<String> = state
            .engines
            .engines()
            .iter()
            .flat_map(|e| store.all_hashes(&e.id).unwrap_or_default())
            .collect();
        all.sort();
        all.dedup();
        return all;
    }
    split_list(raw)
}

/// qBittorrent answers its mutations with an empty 200.
pub(super) fn qbit_ok() -> Response {
    (StatusCode::OK, "").into_response()
}

pub(super) async fn qbit_create_category(
    State(state): State<AppState>,
    RawQuery(query): RawQuery,
    headers: HeaderMap,
    Form(form): Form<Fields>,
) -> Response {
    let query = query.unwrap_or_default();
    guard!(state, headers, query);

    let name = form.get("category").cloned().unwrap_or_default();
    if name.is_empty() {
        return (StatusCode::BAD_REQUEST, "category name is empty").into_response();
    }
    // qBittorrent answers 409 for a name that exists. 4.3 overwrote it as a
    // hoard category with the given path, so a tool re-creating its category
    // on every start reset the operator's mode and path each time.
    if categories_map(&state).contains_key(&name) {
        return (StatusCode::CONFLICT, "Category already exists").into_response();
    }
    let save_path = form.get("savePath").cloned().unwrap_or_default();
    if let Err(e) = edit_categories(&state, |doc| {
        doc.insert(
            name,
            serde_json::json!({"save_path": save_path, "mode": "hoard"}),
        );
    }) {
        return store_refused(&state, "createCategory", e);
    }
    qbit_ok()
}

pub(super) async fn qbit_edit_category(
    State(state): State<AppState>,
    RawQuery(query): RawQuery,
    headers: HeaderMap,
    Form(form): Form<Fields>,
) -> Response {
    let query = query.unwrap_or_default();
    guard!(state, headers, query);

    let name = form.get("category").cloned().unwrap_or_default();
    let save_path = form.get("savePath").cloned().unwrap_or_default();
    if let Err(e) = edit_categories(&state, |doc| {
        // Edit in place so anything the category carries beyond save_path
        // survives; only a category that does not exist is created whole.
        match doc.get_mut(&name) {
            Some(serde_json::Value::Object(fields)) => {
                fields.insert("save_path".into(), serde_json::Value::String(save_path));
            }
            _ => {
                doc.insert(
                    name,
                    serde_json::json!({"save_path": save_path, "mode": "hoard"}),
                );
            }
        }
    }) {
        return store_refused(&state, "editCategory", e);
    }
    qbit_ok()
}

pub(super) async fn qbit_remove_categories(
    State(state): State<AppState>,
    RawQuery(query): RawQuery,
    headers: HeaderMap,
    Form(form): Form<Fields>,
) -> Response {
    let query = query.unwrap_or_default();
    guard!(state, headers, query);

    // The field is newline-separated in qBit's own API.
    let names: Vec<String> = form
        .get("categories")
        .map(|raw| {
            raw.split(['\n', ','])
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default();
    if let Err(e) = edit_categories(&state, |doc| {
        for name in &names {
            doc.remove(name);
        }
    }) {
        return store_refused(&state, "removeCategories", e);
    }
    // qBittorrent clears the label from the torrents too.
    {
        let store = state.store.lock().unwrap();
        for name in &names {
            if let Err(e) = store.relabel_category(name, "") {
                drop(store);
                return store_refused(&state, "removeCategories", e);
            }
        }
    }
    qbit_ok()
}

pub(super) async fn qbit_create_tags(
    State(state): State<AppState>,
    RawQuery(query): RawQuery,
    headers: HeaderMap,
    Form(form): Form<Fields>,
) -> Response {
    let query = query.unwrap_or_default();
    guard!(state, headers, query);
    let tags = split_list(form.get("tags").map(String::as_str).unwrap_or(""));
    let written = state.store.lock().unwrap().register_tags(&tags);
    if let Err(e) = written {
        return store_refused(&state, "createTags", e);
    }
    qbit_ok()
}

pub(super) async fn qbit_delete_tags(
    State(state): State<AppState>,
    RawQuery(query): RawQuery,
    headers: HeaderMap,
    Form(form): Form<Fields>,
) -> Response {
    let query = query.unwrap_or_default();
    guard!(state, headers, query);
    let tags = split_list(form.get("tags").map(String::as_str).unwrap_or(""));
    // As qBittorrent does: the tag goes from the list and from every torrent.
    // 4.x reproduced a 3.x bug and deleted nothing, so a tag a client deleted
    // came back in the next listing.
    let written = {
        let store = state.store.lock().unwrap();
        store
            .unregister_tags(&tags)
            .and_then(|_| store.strip_tags(&tags).map(|_| ()))
    };
    if let Err(e) = written {
        return store_refused(&state, "deleteTags", e);
    }
    qbit_ok()
}

pub(super) async fn qbit_add_tags(
    State(state): State<AppState>,
    RawQuery(query): RawQuery,
    headers: HeaderMap,
    Form(form): Form<Fields>,
) -> Response {
    let query = query.unwrap_or_default();
    guard!(state, headers, query);
    if let Err(e) = retag(&state, &form, true) {
        return store_refused(&state, "addTags", e);
    }
    qbit_ok()
}

pub(super) async fn qbit_remove_tags(
    State(state): State<AppState>,
    RawQuery(query): RawQuery,
    headers: HeaderMap,
    Form(form): Form<Fields>,
) -> Response {
    let query = query.unwrap_or_default();
    guard!(state, headers, query);
    if let Err(e) = retag(&state, &form, false) {
        return store_refused(&state, "removeTags", e);
    }
    qbit_ok()
}

pub(super) async fn qbit_pause(
    State(state): State<AppState>,
    RawQuery(query): RawQuery,
    headers: HeaderMap,
    Form(form): Form<Fields>,
) -> Response {
    let query = query.unwrap_or_default();
    guard!(state, headers, query);
    if let Err(e) = set_paused(&state, &form, true) {
        return store_refused(&state, "pause", e);
    }
    qbit_ok()
}

pub(super) async fn qbit_resume(
    State(state): State<AppState>,
    RawQuery(query): RawQuery,
    headers: HeaderMap,
    Form(form): Form<Fields>,
) -> Response {
    let query = query.unwrap_or_default();
    guard!(state, headers, query);
    if let Err(e) = set_paused(&state, &form, false) {
        return store_refused(&state, "resume", e);
    }
    qbit_ok()
}

pub(super) async fn qbit_version(
    State(state): State<AppState>,
    RawQuery(query): RawQuery,
    headers: HeaderMap,
) -> Response {
    let query = query.unwrap_or_default();
    guard!(state, headers, query);
    let cfg = state.cfg();
    let _ = cfg;
    // Plain text, not JSON: qBittorrent answers a bare string here.
    QBIT_VERSION.into_response()
}

pub(super) async fn qbit_webapi_version(
    State(state): State<AppState>,
    RawQuery(query): RawQuery,
    headers: HeaderMap,
) -> Response {
    let query = query.unwrap_or_default();
    guard!(state, headers, query);
    let cfg = state.cfg();
    let _ = cfg;
    QBIT_WEBAPI_VERSION.into_response()
}

pub(super) async fn qbit_build_info(
    State(state): State<AppState>,
    RawQuery(query): RawQuery,
    headers: HeaderMap,
) -> Response {
    let query = query.unwrap_or_default();
    guard!(state, headers, query);
    let cfg = state.cfg();
    let _ = cfg;
    // The library versions a real qBittorrent 4.6.0 reports. Hydra runs none of
    // them; the values exist so a client's build check does not refuse to talk.
    Json(serde_json::json!({
        "bitness": 64, "boost": "1.83.0", "libtorrent": "2.0.9.0", // leak-ok: library versions
        "openssl": "3.1.4", "qt": "6.5.3",
    }))
    .into_response()
}

pub(super) async fn qbit_preferences(
    State(state): State<AppState>,
    RawQuery(query): RawQuery,
    headers: HeaderMap,
) -> Response {
    let query = query.unwrap_or_default();
    guard!(state, headers, query);
    let cfg = state.cfg();
    // The queue as the race engine runs it, once `queueing` is on; until then
    // the figures this page has always answered, which no client acts on.
    let queueing = cfg.race.queueing.unwrap_or(false);
    let queue_cap = |v: Option<i64>, shown: i64| {
        if queueing {
            v.filter(|n| *n > 0).unwrap_or(-1)
        } else {
            shown
        }
    };

    let mut prefs = serde_json::json!({
        "add_trackers_enabled": false,
        "alternative_webui_enabled": false,
        "create_subfolder_enabled": cfg.daemon.create_torrent_folder,
        "dht": cfg.race.enable_dht,
        "encryption": 1,
        "listen_port": cfg.race.listen_port,
        "locale": "en",
        // The page's engine, as its file says (applied at the next start).
        "lsd": qbit_preferences_lsd(&state),
        // Live in Hydranos with or without queueing; was a constant 20.
        "max_active_downloads": qbit_shown_active_downloads(&cfg),
        "max_active_torrents": queue_cap(cfg.race.active_limit, 100),
        "max_active_uploads": queue_cap(cfg.race.active_seeds, 50),
        "max_connec": cfg.race.max_connections,
        "max_uploads_per_torrent": cfg.race.max_uploads_per_torrent,
        "max_uploads_per_torrent_enabled": cfg.race.max_uploads_per_torrent >= 0,
        // The engine's global cap on unchoked peers (-1 = none, as qBittorrent).
        "max_uploads": cfg.race.max_uploads,
        "max_uploads_enabled": cfg.race.max_uploads >= 0,
        "pex": cfg.race.enable_pex,
        // The client-wide caps, bytes/s, 0 = none. qBittorrent's wiki says
        // KiB/s here; qBittorrent itself sends bytes/s (its own WebUI divides
        // by 1024 to display them), and that is what clients read.
        "up_limit": state.engines.client_rates().up.rate(),
        "dl_limit": state.engines.client_rates().down.rate(),
        "queueing_enabled": queueing,
        // The slow-torrent rule of the race engine, with qBittorrent's
        // defaults when its file says nothing (KiB/s, seconds).
        "dont_count_slow_torrents": cfg.race.dont_count_slow_torrents.unwrap_or(false),
        "slow_torrent_dl_rate_threshold": cfg.race.slow_torrent_dl_rate_threshold.unwrap_or(2),
        "slow_torrent_ul_rate_threshold": cfg.race.slow_torrent_ul_rate_threshold.unwrap_or(2),
        "slow_torrent_inactive_timer": cfg.race.slow_torrent_inactive_timer.unwrap_or(60),
        // Where an add with no category and no savepath lands, as a client
        // set it (`setPreferences`). Was the constant `/downloads`, a folder
        // nothing ever wrote to; empty until a client sets one, because
        // Hydranos has no default and refuses such an add otherwise.
        "save_path": qbit_default_save_path(&state),
        "temp_path_enabled": false,
        "web_ui_port": cfg.daemon.api_port,
    });
    // Share limits: qBittorrent has one session, so this page speaks for one
    // engine (see `sharelimits::preferences_engine`); a write goes to all.
    let share = crate::sharelimits::qbit_preferences(&crate::sharelimits::engine_limits_in(
        &cfg,
        &crate::sharelimits::preferences_engine(&state),
    ));
    if let (Some(obj), serde_json::Value::Object(fields)) = (prefs.as_object_mut(), share) {
        obj.extend(fields);
    }
    Json(prefs).into_response()
}

pub(super) async fn qbit_transfer_info(
    State(state): State<AppState>,
    RawQuery(query): RawQuery,
    headers: HeaderMap,
) -> Response {
    let query = query.unwrap_or_default();
    guard!(state, headers, query);
    let cfg = state.cfg();
    let _ = cfg;

    // qBittorrent's `*_info_data` is "this session": bytes moved since the
    // start. `session_totals` is a sum of lifetime counters over the torrents
    // loaded now, and fell by a torrent's whole history at each removal.
    let (up, down) = state.engines.moved_totals();
    // The engines' own rates, summed: 4.3 answered 0 for both speeds.
    let (up_speed, down_speed) = state.engines.engines().iter().fold((0u64, 0u64), |acc, e| {
        (
            acc.0 + e.manager.upload_rate.get(),
            acc.1 + e.manager.download_rate.get(),
        )
    });
    Json(serde_json::json!({
        "connection_status": "connected",
        "dht_nodes": state.engines.dht_nodes(),
        "dl_info_data": down,
        "dl_info_speed": down_speed,
        // The client-wide caps, bytes/s, 0 = none. Were constant zeroes.
        "dl_rate_limit": state.engines.client_rates().down.rate(),
        "up_info_data": up,
        "up_info_speed": up_speed,
        "up_rate_limit": state.engines.client_rates().up.rate(),
    }))
    .into_response()
}

/// Move torrents to a category.
///
/// An empty category clears it, which is how qBit clients "remove from
/// category" -- there is no separate call for that.
pub(super) async fn qbit_set_category(
    State(state): State<AppState>,
    RawQuery(query): RawQuery,
    headers: HeaderMap,
    Form(form): Form<Fields>,
) -> Response {
    let query = query.unwrap_or_default();
    guard!(state, headers, query);
    let cfg = state.cfg();
    let _ = cfg;

    let category = form.get("category").cloned().unwrap_or_default();
    let hashes = qbit_hashes(&state, &form);
    let store = state.store.lock().unwrap();
    for prefix in hashes {
        if let Some(hash) = store.resolve_hash(&prefix) {
            // The shim has no engine to name: qBit clients send a hash and a
            // category and nothing else. Every copy it is, deliberately.
            if let Err(e) = store.set_category_everywhere(&hash, &category) {
                drop(store);
                return store_refused(&state, "setCategory", e);
            }
        }
    }
    qbit_ok()
}

/// start/stop are qBittorrent 5's names for resume/pause. Both spellings are
/// served because clients in the wild send either depending on their vintage.
pub(super) async fn qbit_start(
    State(state): State<AppState>,
    RawQuery(query): RawQuery,
    headers: HeaderMap,
    Form(form): Form<Fields>,
) -> Response {
    let query = query.unwrap_or_default();
    guard!(state, headers, query);
    let cfg = state.cfg();
    let _ = cfg;
    if let Err(e) = set_paused(&state, &form, false) {
        return store_refused(&state, "resume", e);
    }
    qbit_ok()
}

pub(super) async fn qbit_stop(
    State(state): State<AppState>,
    RawQuery(query): RawQuery,
    headers: HeaderMap,
    Form(form): Form<Fields>,
) -> Response {
    let query = query.unwrap_or_default();
    guard!(state, headers, query);
    let cfg = state.cfg();
    let _ = cfg;
    if let Err(e) = set_paused(&state, &form, true) {
        return store_refused(&state, "pause", e);
    }
    qbit_ok()
}

/// qBittorrent's session login.
///
/// This is the *arr stack's only way in. Sonarr, Radarr, autobrr and
/// cross-seed configure a "qBittorrent" with a host, a port, a username and a
/// password; none of them can set `X-Api-Key`, and none of them can add a
/// query parameter. They POST here and then carry the `SID` cookie.
///
/// It used to answer `Ok.` to anything and set no cookie, which was harmless
/// only for as long as the instance authorised everyone anyway. Once the
/// placeholder key stopped being a free pass, every one of those clients
/// started taking a silent 401 on the request *after* a login that had told
/// them it worked.
///
/// The credentials are the admin account -- the same one the WebUI signs in
/// with at `/api/login`. Answers `Ok.` / `Fails.` with a 200 either way,
/// because that is what qBittorrent answers and the clients parse the body.
///
/// Failed logins count against the caller as on `/api/login`, and a blocked
/// caller gets qBittorrent's own 403. The API key still opens a session
/// while blocked, and neither counts nor clears: 256 random bits are not
/// guessed by trying, and the *arr stack behind the same proxy as a stranger
/// keeps working. The admin password does not, or the block would confirm it.
pub(super) async fn qbit_login(
    State(state): State<AppState>,
    axum::extract::ConnectInfo(peer): axum::extract::ConnectInfo<std::net::SocketAddr>,
    body: String,
) -> Response {
    let cfg = state.cfg();
    let username = form_field(&body, "username").unwrap_or_default();
    let password = form_field(&body, "password").unwrap_or_default();
    let who = crate::session::Caller::of(peer.ip());
    let throttle = &state.sessions.throttle;

    if !qbit_key_ok(&cfg, &password) {
        if throttle.blocked_for(who).is_some() {
            return (
                StatusCode::FORBIDDEN,
                "Your IP address has been banned after too many failed authentication attempts.",
            )
                .into_response();
        }
        match qbit_password_ok(&cfg, &username, &password).await {
            Ok(true) => throttle.succeeded(who),
            Ok(false) => {
                throttle.failed(who);
                // One answer for a wrong name, a wrong password and a wrong key alike.
                tracing::warn!(username = %crate::session::loggable(&username), "qBittorrent login refused: invalid credentials");
                return (StatusCode::OK, "Fails.").into_response();
            }
            Err(crate::session::Busy) => {
                return (
                    StatusCode::SERVICE_UNAVAILABLE,
                    [(axum::http::header::RETRY_AFTER, "2")],
                    "",
                )
                    .into_response();
            }
        }
    }

    let sid = state.sessions.create();
    tracing::info!(username = %crate::session::loggable(&username), "qBittorrent client logged in");
    // `HttpOnly` because no page of ours reads it, `SameSite=Lax` because no
    // cross-site form should be able to drive the API with it. Deliberately no
    // `Secure`: the *arr stack talks to this over plain HTTP on the LAN, and a
    // cookie marked Secure would simply never come back.
    let cookie = format!(
        "{}={}; Path=/; HttpOnly; SameSite=Lax",
        crate::session::COOKIE_NAME,
        sid
    );
    (
        StatusCode::OK,
        [(axum::http::header::SET_COOKIE, cookie)],
        "Ok.",
    )
        .into_response()
}

/// Whether these credentials may open a qBittorrent session.
///
/// Two ways in, because the clients have one password field and there are two
/// things worth putting in it:
///
///  1. **The admin account**, as the WebUI signs in at `/api/login`.
///  2. **The API key**, typed into the password box.
///
/// The second exists so nobody has to rotate anything to fix an install that
/// is already broken: every one of these clients holds the API key today, in a
/// field the daemon stopped reading. It is not a weaker credential -- it is
/// the SAME secret the header carries, over the same connection, and a caller
/// who has it can already drive the whole API. It is compared in constant time
/// for that reason, and never echoed back the way `/api/login` echoes it to
/// the WebUI.
///
/// An instance with neither a key nor an admin account authorises nobody. That
/// guard is not decoration: without it an empty password would compare equal
/// to an empty expected key, which is the 4.14 hole in a new doorway.
///
/// Split in two because the throttle treats them apart: the key is checked
/// first and costs no bcrypt.
pub(super) fn qbit_key_ok(cfg: &crate::config::Config, password: &str) -> bool {
    crate::config::api_key_usable(&cfg.daemon.api_key)
        && !password.is_empty()
        && constant_time_eq(password.as_bytes(), cfg.daemon.api_key.as_bytes())
}

pub(super) async fn qbit_password_ok(
    cfg: &crate::config::Config,
    username: &str,
    password: &str,
) -> Result<bool, crate::session::Busy> {
    if cfg.auth.password_hash.is_empty() || username != cfg.auth.username {
        return Ok(false);
    }
    crate::session::verify_password(password, &cfg.auth.password_hash).await
}

/// End the session this request carries, and tell the client to drop it.
pub(super) async fn qbit_logout(State(state): State<AppState>, headers: HeaderMap) -> Response {
    if let Some(sid) = session_of(&headers) {
        state.sessions.revoke(&sid);
    }
    let cookie = format!(
        "{}=; Path=/; HttpOnly; SameSite=Lax; Max-Age=0",
        crate::session::COOKIE_NAME
    );
    (
        StatusCode::OK,
        [(axum::http::header::SET_COOKIE, cookie)],
        "",
    )
        .into_response()
}

/// The torrent listing qBittorrent clients poll.
///
/// Both engines in one list, each row carrying its engine as the fallback
/// category. This is the endpoint the *arr stack reads on a timer, so it is
/// built from the engines directly -- in 3.x it was rebuilt from a cached copy
/// of a copy, which is where 618 MB of the Go heap lived.
/// Does a qBittorrent state belong to a named filter?
///
/// Ported from filterStateMatch. `paused` and `stopped` name the same set:
/// qBittorrent 5 renamed the filter and we still answer the old spelling for
/// everything written before it.
pub(super) fn qbit_filter_matches(state: &str, filter: &str) -> bool {
    match filter {
        "all" => true,
        "downloading" => matches!(
            state,
            "downloading" | "stalledDL" | "checkingDL" | "queuedDL" | "allocating"
        ),
        "seeding" => matches!(state, "uploading" | "stalledUP" | "queuedUP" | "checkingUP"),
        "completed" => matches!(
            state,
            "uploading" | "stalledUP" | "pausedUP" | "stoppedUP" | "queuedUP" | "checkingUP"
        ),
        "paused" | "stopped" => {
            matches!(state, "pausedDL" | "pausedUP" | "stoppedDL" | "stoppedUP")
        }
        "active" => matches!(state, "downloading" | "uploading"),
        "inactive" => matches!(
            state,
            "stalledDL" | "stalledUP" | "pausedDL" | "pausedUP" | "stoppedDL" | "stoppedUP"
        ),
        "stalled" => matches!(state, "stalledDL" | "stalledUP"),
        "stalled_uploading" => state == "stalledUP",
        "stalled_downloading" => state == "stalledDL",
        // qBittorrent counts missing files as errored too.
        "errored" => matches!(state, "error" | "missingFiles"),
        // qBittorrent's own: a torrent whose files are being checked, waiting
        // for its turn included -- qBittorrent reports that wait as checking
        // too, and has no state of its own for it. Listed everything before.
        "checking" => matches!(state, "checkingUP" | "checkingDL" | "checkingResumeData"),
        // Files being moved: never the case here, so nothing. It listed the
        // whole library, which a client reads as every torrent moving.
        "moving" => state == "moving",
        "resumed" | "running" => {
            !matches!(state, "pausedDL" | "pausedUP" | "stoppedDL" | "stoppedUP")
        }
        // An unknown filter name lists everything, as qBittorrent does. Hiding
        // every torrent instead would read to a client as "the queue is empty".
        _ => true,
    }
}

/// Order two listing values the way compareValues did: same-typed values on
/// their own terms, anything else on its rendered form.
pub(super) fn qbit_value_cmp(a: &serde_json::Value, b: &serde_json::Value) -> std::cmp::Ordering {
    use serde_json::Value;
    use std::cmp::Ordering;
    match (a, b) {
        (Value::String(x), Value::String(y)) => x.cmp(y),
        (Value::Number(x), Value::Number(y)) => x
            .as_f64()
            .unwrap_or(0.0)
            .partial_cmp(&y.as_f64().unwrap_or(0.0))
            .unwrap_or(Ordering::Equal),
        (Value::Bool(x), Value::Bool(y)) => x.cmp(y),
        _ => a.to_string().cmp(&b.to_string()),
    }
}

/// One engine's torrents in the qBittorrent shape, with the listing's category
/// and hash filters applied *before* each row is built.
///
/// Filtering here rather than over the finished list is the whole point.
/// Building a row serialises a torrent to JSON, so an unfiltered build is the
/// entire library every time -- and the *arr stack, cross-seed and autobrr each
/// poll this endpoint several times a minute. The category a row is filtered on
/// is the one it will be reported under, engine-name fallback included, so a
/// client that asks for what it sees gets it back.
pub(crate) fn engine_qbit_rows(
    state: &AppState,
    engine_id: &str,
    now: i64,
    category: Option<&str>,
    hashes: Option<&std::collections::HashSet<String>>,
) -> Vec<serde_json::Value> {
    let Some(engine) = state.engines.get(engine_id) else {
        return Vec::new();
    };

    let agent = local_agent(engine_id);
    let empty = crate::row::StoreFacts::default();
    let mut rows = Vec::new();
    // Share limits: the engine's, once, and the sparse per-torrent table,
    // once -- never a store read per row.
    let share_engine = crate::sharelimits::engine_limits(state, engine_id);
    let share_own = crate::sharelimits::overrides(state);
    let share_follow = crate::sharelimits::TorrentShareLimits::default();

    // *arr asks by category and nothing else, over and over. Walking the whole
    // catalogue to keep one category cost 1.5 s per poll on a 300k library:
    // 300k StoreFacts built, 1972 kept.
    //
    // A NAMED category can be answered from the store's index alone, because a
    // torrent with no row there has no category either -- it can only fall
    // under the engine's own name. So this shortcut is exact for every category
    // except that one, which still takes the walk below.
    if let Some(wanted) = category.filter(|c| *c != engine_id) {
        let facts = {
            let store = state.store.read().unwrap();
            store
                .facts_in_category(engine_id, wanted)
                .unwrap_or_default()
        };
        for (hash, torrent_facts) in facts.iter() {
            if let Some(only) = hashes {
                if !only.contains(hash) {
                    continue;
                }
            }
            let Ok(key) = typhon_engine::torrent::hex_decode(hash) else {
                continue;
            };
            // The store can name a torrent the engine no longer holds. It is
            // not this endpoint's job to reconcile that -- it reports what is
            // actually running.
            let Some(torrent) = engine.manager.get(&key) else {
                continue;
            };
            let raw = typhon_engine::rpc::dispatch::torrent_to_json(&torrent);
            let native = crate::row::build(&raw, torrent_facts, &agent);
            let mut row = crate::qbitrow::build(&native, engine_id, now);
            // The tracker qBittorrent reports: the first one announced to. 4.3
            // left it empty, so cross-seed and the *arrs could not tell which
            // tracker a torrent belonged to.
            if let Some(url) = torrent.live_trackers.read().iter().flatten().next() {
                row["tracker"] = serde_json::Value::String(url.clone());
            }
            let (up_cap, down_cap) = torrent.rate_limits();
            row["up_limit"] = qbit_limit_field(up_cap).into();
            row["dl_limit"] = qbit_limit_field(down_cap).into();
            let (seq_dl, f_l_piece_prio) = torrent.download_order_flags();
            row["seq_dl"] = seq_dl.into();
            row["f_l_piece_prio"] = f_l_piece_prio.into();
            crate::sharelimits::fill_qbit_row(
                &mut row,
                share_own.get(hash.as_str()).unwrap_or(&share_follow),
                &share_engine,
            );
            rows.push(row);
        }
        return rows;
    }

    // One query, not one per torrent: the rows of the hashes asked for when
    // there are a few of them (a client polling its own queue by hash), the
    // whole session otherwise. The bound keeps the query inside SQLite's limit
    // on bound variables; past it the whole session is the cheaper read.
    let facts = {
        let store = state.store.read().unwrap();
        match hashes {
            Some(wanted) if wanted.len() <= NAMED_HASHES_QUERY_MAX => {
                let named: Vec<String> = wanted.iter().cloned().collect();
                store
                    .facts_for_hashes(&named, engine_id)
                    .unwrap_or_default()
            }
            _ => store.facts_by_session(engine_id).unwrap_or_default(),
        }
    };

    for torrent in engine.manager.all().iter() {
        let hash = typhon_engine::torrent::hex_encode(&torrent.info_hash);
        if let Some(wanted) = hashes {
            if !wanted.contains(&hash) {
                continue;
            }
        }
        let torrent_facts = facts.get(&hash).unwrap_or(&empty);
        if let Some(wanted) = category {
            let effective = if torrent_facts.category.is_empty() {
                engine_id
            } else {
                torrent_facts.category.as_str()
            };
            if effective != wanted {
                continue;
            }
        }
        let raw = typhon_engine::rpc::dispatch::torrent_to_json(torrent);
        let native = crate::row::build(&raw, torrent_facts, &agent);
        let mut row = crate::qbitrow::build(&native, engine_id, now);
        // The tracker qBittorrent reports: the first one announced to. 4.3
        // left it empty, so cross-seed and the *arrs could not tell which
        // tracker a torrent belonged to.
        if let Some(url) = torrent.live_trackers.read().iter().flatten().next() {
            row["tracker"] = serde_json::Value::String(url.clone());
        }
        let (up_cap, down_cap) = torrent.rate_limits();
        row["up_limit"] = qbit_limit_field(up_cap).into();
        row["dl_limit"] = qbit_limit_field(down_cap).into();
        let (seq_dl, f_l_piece_prio) = torrent.download_order_flags();
        row["seq_dl"] = seq_dl.into();
        row["f_l_piece_prio"] = f_l_piece_prio.into();
        crate::sharelimits::fill_qbit_row(
            &mut row,
            share_own.get(hash.as_str()).unwrap_or(&share_follow),
            &share_engine,
        );
        rows.push(row);
    }
    rows
}

/// The qBittorrent listing.
///
/// The filter arguments are not decoration. Every client of this endpoint is
/// configured with one category and assumes the answer is scoped to it: Sonarr
/// and Radarr treat what comes back as *their own queue*, and a listing that
/// ignores `category` hands each of them the whole library -- 300k torrents,
/// ebooks included -- to fail an import on, one row at a time.
pub(super) async fn qbit_torrents_info(
    State(state): State<AppState>,
    RawQuery(query): RawQuery,
    headers: HeaderMap,
    body: String,
) -> Response {
    let query = query.unwrap_or_default();
    guard!(state, headers, query);

    // The route answers any verb, and qBittorrent takes these either on the
    // query string or as a POST form, so both are read. On a GET the body is
    // empty and the second lookup costs nothing.
    let param = |name: &str| shim_param(&query, &body, name);

    let filter = param("filter").unwrap_or_else(|| "all".to_string());
    let category = param("category");
    let tag = param("tag");
    let sort_field = param("sort").unwrap_or_else(|| "added_on".to_string());
    let reverse = param("reverse").as_deref() == Some("true");
    let limit = param("limit")
        .and_then(|v| v.parse::<usize>().ok())
        .unwrap_or(0);
    let offset = param("offset")
        .and_then(|v| v.parse::<usize>().ok())
        .unwrap_or(0);

    // "all" is qBittorrent's word for "no hash filter"; taken literally it is a
    // hash that matches nothing, which reads to a client as an empty queue.
    let hashes = param("hashes").filter(|raw| raw != "all").map(|raw| {
        raw.split(['|', ','])
            .map(|h| h.trim().to_lowercase())
            .filter(|h| !h.is_empty())
            .collect::<std::collections::HashSet<String>>()
    });

    let now = now_secs();

    // Non-empty by construction: an empty listing must marshal as [], never
    // null. Clients dereference the array directly -- cross-seed calls
    // torrents.find(...) straight on the parsed body -- so a null throws there
    // instead of reading as "no torrents".
    // Every engine, not only race and hoard, and each torrent once: a torrent
    // held by two engines was listed twice, which a client reads as two
    // torrents with one hash. The first engine holding it answers.
    let mut rows: Vec<serde_json::Value> = Vec::new();
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
    let ids: Vec<String> = state
        .engines
        .engines()
        .iter()
        .map(|e| e.id.clone())
        .collect();
    for engine in &ids {
        for row in engine_qbit_rows(&state, engine, now, category.as_deref(), hashes.as_ref()) {
            let h = row
                .get("hash")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            if seen.insert(h) {
                rows.push(row);
            }
        }
    }

    if filter != "all" {
        rows.retain(|row| {
            let state = row
                .get("state")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("");
            qbit_filter_matches(state, &filter)
        });
    }

    if let Some(wanted) = tag.as_deref() {
        rows.retain(|row| {
            row.get("tags")
                .and_then(serde_json::Value::as_str)
                .map(|tags| tags.split(',').any(|t| t.trim() == wanted))
                .unwrap_or(false)
        });
    }

    let null = serde_json::Value::Null;
    rows.sort_by(|a, b| {
        let ordering = qbit_value_cmp(
            a.get(&sort_field).unwrap_or(&null),
            b.get(&sort_field).unwrap_or(&null),
        );
        if reverse {
            ordering.reverse()
        } else {
            ordering
        }
    });

    if offset > 0 {
        rows = if offset < rows.len() {
            rows.split_off(offset)
        } else {
            // Past the end is an empty page, not the whole list again.
            Vec::new()
        };
    }
    if limit > 0 && limit < rows.len() {
        rows.truncate(limit);
    }

    Json(rows).into_response()
}

/// `POST /api/v2/torrents/renameFile` (`hash`, `oldPath`, `newPath`): the same
/// rename, with paths as `torrents/files` names them, folder included, and the
/// folder may change (a file by file move of a torrent to another folder).
pub(super) async fn qbit_rename_file(
    State(state): State<AppState>,
    RawQuery(query): RawQuery,
    headers: HeaderMap,
    body: String,
) -> Response {
    let query = query.unwrap_or_default();
    guard!(state, headers, query);
    let form = shim_form(&query, &body);
    let (Some(hash), Some(old), Some(new)) =
        (form.get("hash"), form.get("oldPath"), form.get("newPath"))
    else {
        return qbit_refuse(
            StatusCode::BAD_REQUEST,
            "hash, oldPath and newPath are required",
        );
    };
    // The paths are the engine's own: below the save path, folder included
    // (which may change: a file can go to another folder).
    match rename_file_on_copies(&state, hash, "", true, &|_| {
        Ok(vec![(old.clone(), new.clone())])
    }) {
        Ok(_) => qbit_ok(),
        Err((code, msg)) => qbit_refuse(code, &msg),
    }
}

/// `POST /api/v2/torrents/renameFolder` (`hash`, `oldPath`, `newPath`): every
/// file below the folder `oldPath` is renamed below `newPath`, as
/// `renameFile` renames one, and all or none of them are.
pub(super) async fn qbit_rename_folder(
    State(state): State<AppState>,
    RawQuery(query): RawQuery,
    headers: HeaderMap,
    body: String,
) -> Response {
    let query = query.unwrap_or_default();
    guard!(state, headers, query);
    let form = shim_form(&query, &body);
    let (Some(hash), Some(old), Some(new)) =
        (form.get("hash"), form.get("oldPath"), form.get("newPath"))
    else {
        return qbit_refuse(
            StatusCode::BAD_REQUEST,
            "hash, oldPath and newPath are required",
        );
    };
    match rename_file_on_copies(&state, hash, "", true, &|meta| {
        folder_rename_paths(meta, old, new)
    }) {
        Ok(_) => qbit_ok(),
        Err((code, msg)) => qbit_refuse(code, &msg),
    }
}

/// Files of one torrent, qBittorrent shape.
pub(super) async fn qbit_torrent_files(
    State(state): State<AppState>,
    RawQuery(query): RawQuery,
    headers: HeaderMap,
    body: String,
) -> Response {
    let query = query.unwrap_or_default();
    guard!(state, headers, query);
    let cfg = state.cfg();
    let _ = cfg;

    let hash = shim_param(&query, &body, "hash").unwrap_or_default();
    let Some((_, torrent)) = find_torrent(&state, &hash) else {
        return not_found();
    };
    // Each file's pieces and how many of them are held. 4.3 answered every
    // file at 100 % on piece range [0, 0], so a client waiting for one file
    // of an incomplete torrent saw it finished.
    let piece_len = torrent.meta.piece_length.max(1) as u64;
    let complete = torrent.bytes_left() == 0;
    // The same per-file bytes the native Content tab shows, so the two never
    // disagree. qBit has no "unknown": a file with no piece map reads 0.
    let done = file_done_bytes(&torrent);
    let order = torrent.download_order();
    let files: Vec<serde_json::Value> = torrent
        .meta
        .files
        .iter()
        .enumerate()
        .map(|(index, f)| {
            let (first, last) = file_piece_range(f.offset, f.length, piece_len);
            let progress = match done[index] {
                _ if f.length == 0 => 1.0,
                Some(d) => d as f64 / f.length as f64,
                None => 0.0,
            };
            serde_json::json!({
                "availability": 1,
                "index": index,
                "is_seed": complete,
                "name": qbit_file_name(&torrent.meta, index),
                "piece_range": [first, last],
                // 1, 6 or 7: never 0, every file is downloaded.
                "priority": order.file_priority(index),
                "progress": progress,
                "size": f.length,
            })
        })
        .collect();
    Json(files).into_response()
}

/// A file's name as qBittorrent gives it: relative to the save path, so a
/// multi-file torrent's files carry the torrent's folder in front.
///
/// cross-seed matches these names against the files of the torrent it wants
/// to inject; without the folder every multi-file candidate looked like a
/// different layout.
pub(super) fn qbit_file_name(
    meta: &typhon_engine::torrent::meta::TorrentMeta,
    index: usize,
) -> String {
    // Below the save path already: the folder is in the path, and is not the
    // .torrent's once the file was renamed into another.
    let rel = meta.disk_rel(index);
    // qBit answers with forward slashes on every platform. Joined component by
    // component rather than by replacing '\\': on Linux a backslash is a legal
    // character inside a file name.
    rel.components()
        .map(|c| c.as_os_str().to_string_lossy())
        .collect::<Vec<_>>()
        .join("/")
}

/// One torrent's `.torrent`, qBittorrent's `torrents/export`.
///
/// Served from the store like the native route, so the info dict is the one
/// that was added and the info hash still matches it.
pub(super) async fn qbit_torrent_export(
    State(state): State<AppState>,
    RawQuery(query): RawQuery,
    headers: HeaderMap,
    body: String,
) -> Response {
    let query = query.unwrap_or_default();
    guard!(state, headers, query);
    let hash = shim_param(&query, &body, "hash")
        .unwrap_or_default()
        .to_lowercase();
    let blob = {
        let store = state.store.lock().unwrap();
        store.torrent_blob(&hash).ok().flatten()
    };
    match blob {
        Some(bytes) => (
            [(axum::http::header::CONTENT_TYPE, "application/x-bittorrent")],
            current_torrent_file(&state, &hash, bytes),
        )
            .into_response(),
        None => not_found(),
    }
}

/// One torrent's properties panel, qBittorrent shape.
///
/// The transfer figures are the real ones, as in the listing (see qbitrow.rs
/// for why they were zero until 4.4). `dl_limit` / `up_limit` are the
/// torrent's own caps in bytes/s, -1 -- qBittorrent's "no limit" -- when it
/// has none.
pub(super) async fn qbit_torrent_properties(
    State(state): State<AppState>,
    RawQuery(query): RawQuery,
    headers: HeaderMap,
    body: String,
) -> Response {
    let query = query.unwrap_or_default();
    guard!(state, headers, query);
    let cfg = state.cfg();
    let _ = cfg;

    let hash = shim_param(&query, &body, "hash").unwrap_or_default();
    let Some((engine_id, torrent)) = find_torrent(&state, &hash) else {
        return not_found();
    };

    let raw = typhon_engine::rpc::dispatch::torrent_to_json(&torrent);
    // This torrent's row only. The whole session used to be read into a map
    // to keep one entry of it: on a big library that held the store for
    // seconds and put gigabytes in memory, once per call, and a client asks
    // every few seconds. The key is the stored lowercase hash, not the
    // spelling of the request.
    let key = typhon_engine::torrent::hex_encode(&torrent.info_hash);
    let facts = {
        let store = state.store.read().unwrap();
        store
            .facts_for_hashes(std::slice::from_ref(&key), &engine_id)
            .unwrap_or_default()
    };
    let empty = crate::row::StoreFacts::default();
    let native = crate::row::build(&raw, facts.get(&key).unwrap_or(&empty), "");

    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    let added = native
        .get("added_time")
        .and_then(|v| v.as_i64())
        .unwrap_or(0);
    let save_path = native
        .get("engine_save_path")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    // The transfer figures from the listing's own builder, so the panel and
    // `torrents/info` cannot report two ratios for one torrent.
    let listed = crate::qbitrow::build(&native, &engine_id, now);
    let share_own = state
        .store
        .read()
        .map(|s| s.share_limits_of(&typhon_engine::torrent::hex_encode(&torrent.info_hash)))
        .unwrap_or_default();
    let share = crate::sharelimits::qbit_fields(
        &share_own,
        &crate::sharelimits::engine_limits(&state, &engine_id),
    );

    // From the .torrent itself, as qBittorrent reports them. 4.3 sent the
    // addition date as the creation date and empty strings for the rest.
    let blob = {
        let store = state.store.lock().unwrap();
        store
            .torrent_blob(&typhon_engine::torrent::hex_encode(&torrent.info_hash))
            .ok()
            .flatten()
    };
    let (created, created_by, comment) = blob
        .as_deref()
        .map(crate::qbitrow::metainfo_extras)
        .unwrap_or((None, String::new(), String::new()));
    let int = |k: &str| native.get(k).and_then(|v| v.as_i64()).unwrap_or(0);
    let elapsed = (now - added).max(0);
    // qBittorrent's averages: what moved, over the time the torrent has been
    // here. Lifetime bytes over time since added, so a restart does not reset
    // them to the session's.
    let avg = |bytes: &serde_json::Value| {
        if elapsed > 0 {
            bytes.as_i64().unwrap_or(0) / elapsed
        } else {
            0
        }
    };
    // Connected peers are one count in this engine (seeds and leechers are not
    // told apart once connected): all of them are `peers`, and `seeds` -- the
    // connected seeds -- is not measured. The swarm totals are the tracker's.
    let connected = int("num_peers");
    let complete = native
        .get("progress")
        .and_then(|v| v.as_f64())
        .unwrap_or(0.0)
        >= 1.0;
    // Last time a full copy was seen: now when we hold one or the tracker
    // counts a seed, -1 (qBittorrent's "never") otherwise.
    let last_seen = if complete || int("swarm_seeds") > 0 {
        now
    } else {
        -1
    };

    let mut props = serde_json::json!({
        "addition_date": added,
        "comment": comment,
        "completion_date": native.get("completed_time").and_then(|v| v.as_i64()).unwrap_or(0),
        "created_by": created_by,
        "creation_date": created.unwrap_or(-1),
        "dl_limit": qbit_limit_field(torrent.rate_limits().1),
        "dl_speed": native.get("download_rate").and_then(|v| v.as_i64()).unwrap_or(0),
        "dl_speed_avg": avg(&listed["downloaded"]),
        "eta": listed["eta"].clone(),
        "last_seen": last_seen,
        "nb_connections": connected,
        "peers": connected,
        "peers_total": int("swarm_leechers"),
        "piece_size": torrent.meta.piece_length,
        "save_path": save_path,
        "seeding_time": listed["seeding_time"].clone(),
        "seeds": 0,
        "seeds_total": int("swarm_seeds"),
        "share_ratio": listed["ratio"].clone(),
        "time_elapsed": now - added,
        "total_downloaded": listed["downloaded"].clone(),
        "total_downloaded_session": torrent.session_downloaded(),
        "total_size": native.get("total_size").and_then(|v| v.as_i64()).unwrap_or(0),
        "total_uploaded": listed["uploaded"].clone(),
        "total_uploaded_session": torrent.session_uploaded(),
        "total_wasted": 0,
        "up_limit": qbit_limit_field(torrent.rate_limits().0),
        "up_speed": native.get("upload_rate").and_then(|v| v.as_i64()).unwrap_or(0),
        "up_speed_avg": avg(&listed["uploaded"]),
    });
    if let (Some(obj), serde_json::Value::Object(fields)) = (props.as_object_mut(), share) {
        obj.extend(fields);
    }
    Json(props).into_response()
}

/// Trackers of one torrent, qBittorrent shape.
///
/// The three bracketed entries are qBittorrent's convention for its own peer
/// sources, and clients expect them before any real tracker. status 2 means
/// "working".
pub(super) async fn qbit_torrent_trackers(
    State(state): State<AppState>,
    RawQuery(query): RawQuery,
    headers: HeaderMap,
    body: String,
) -> Response {
    let query = query.unwrap_or_default();
    guard!(state, headers, query);
    let cfg = state.cfg();
    let _ = cfg;

    let pseudo = |url: &str| {
        serde_json::json!({
            "msg": "", "num_downloaded": 0, "num_leeches": 0, "num_peers": 0,
            "num_seeds": 0, "status": 2, "tier": "", "url": url,
        })
    };
    let mut rows = vec![pseudo("** [DHT] **"), pseudo("** [PeX] **")];

    // And the real ones. This route used to return the three pseudo rows and
    // nothing else, whatever the torrent: a client asking "who is this
    // announcing to" got an answer that was the same for every torrent in the
    // catalogue, and looked like a torrent with no trackers at all.
    let hash = shim_param(&query, &body, "hash")
        .unwrap_or_default()
        .to_lowercase();
    let Some((engine_id, torrent)) = find_torrent(&state, &hash) else {
        // qBit answers an unknown hash with 404, not with an empty list: a
        // 200 here read as "this torrent has no trackers".
        return not_found();
    };
    rows.push(lsd_pseudo_row(&state, &engine_id, &torrent));
    {
        let last_error = torrent
            .last_announce_error
            .lock()
            .map(|g| g.clone())
            .unwrap_or_default();
        let announced = torrent
            .last_announce_at
            .load(std::sync::atomic::Ordering::Relaxed)
            > 0;
        // qBit's vocabulary: 0 disabled, 1 not contacted yet, 2 working,
        // 4 not working. "Not contacted yet" is the state of a torrent added
        // stopped, and it is the one qBit clients render without an alarm.
        let status = if !announced {
            1
        } else if last_error.is_empty() {
            2
        } else {
            4
        };
        let seeders = torrent
            .scrape_seeders
            .load(std::sync::atomic::Ordering::Relaxed) as i64;
        let leechers = torrent
            .scrape_leechers
            .load(std::sync::atomic::Ordering::Relaxed) as i64;
        for (tier, urls) in torrent.live_trackers.read().iter().enumerate() {
            for url in urls {
                rows.push(serde_json::json!({
                    "url": url,
                    "tier": tier,
                    "status": status,
                    "msg": if status == 4 { last_error.clone() } else { String::new() },
                    "num_peers": 0,
                    "num_seeds": seeders,
                    "num_leeches": leechers,
                    "num_downloaded": 0,
                }));
            }
        }
    }
    Json(serde_json::Value::Array(rows)).into_response()
}

/// qBittorrent's tracker add/remove, routed through the same edit path as the
/// native API so the two cannot drift.
/// qBittorrent's tracker add/remove.
///
/// The form field is `hash`, SINGULAR -- unlike every other qBit route here,
/// which takes `hashes`. Reading the wrong one yields an empty value and a 400
/// that looks like a rejected edit.
///
/// ⚠ 3.x also RE-ANNOUNCES after an add: a freshly added tracker would
/// otherwise wait up to a full re-announce interval before the new swarm hears
/// from us. That half is not ported yet -- this build has no announcer -- so an
/// add is durable here but silent. It belongs to the network slice.
pub(super) async fn qbit_edit_trackers(state: &AppState, form: &Fields, op: &str) -> Response {
    let hash = form
        .get("hash")
        .map(|h| h.trim().to_lowercase())
        .unwrap_or_default();
    let urls: Vec<String> = form
        .get("urls")
        .map(|raw| {
            raw.split(['\n', '|'])
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default();

    if hash.is_empty() || urls.is_empty() {
        // qBittorrent answers 400 on a malformed call and cross-seed reads the
        // status: a 200 here would make a dropped tracker look applied.
        return (StatusCode::BAD_REQUEST, "").into_response();
    }

    let req = TrackerEdit {
        op: op.to_string(),
        urls,
        ..Default::default()
    };
    let outcome = edit_trackers(state, &hash, &req);
    match outcome.status() {
        StatusCode::OK => (StatusCode::OK, "").into_response(),
        StatusCode::NOT_FOUND => (StatusCode::NOT_FOUND, "").into_response(),
        _ => (StatusCode::BAD_REQUEST, "").into_response(),
    }
}

pub(super) async fn qbit_add_trackers(
    State(state): State<AppState>,
    RawQuery(query): RawQuery,
    headers: HeaderMap,
    Form(form): Form<Fields>,
) -> Response {
    let query = query.unwrap_or_default();
    guard!(state, headers, query);
    let cfg = state.cfg();
    let _ = cfg;
    let r = qbit_edit_trackers(&state, &form, "add").await;
    // Announce to the new trackers now, as qBittorrent does when a tracker
    // is added, instead of waiting out the old interval.
    if r.status() == StatusCode::OK {
        let hash = form
            .get("hash")
            .map(|h| h.trim().to_lowercase())
            .unwrap_or_default();
        for (id, copy) in copies_of(&state, &hash) {
            if let Some(bump) = state.engines.get(&id).and_then(|e| e.bump.get()) {
                let _ = bump.try_send(crate::announce::scheduler::BumpReq {
                    info_hash: copy.info_hash,
                    reply: None,
                    forced: false,
                });
            }
        }
    }
    r
}

pub(super) async fn qbit_remove_trackers(
    State(state): State<AppState>,
    RawQuery(query): RawQuery,
    headers: HeaderMap,
    Form(form): Form<Fields>,
) -> Response {
    let query = query.unwrap_or_default();
    guard!(state, headers, query);
    let cfg = state.cfg();
    let _ = cfg;
    qbit_edit_trackers(&state, &form, "remove").await
}

/// A torrent's own cap as qBit's lists show it: bytes/s, -1 for none.
pub(super) fn qbit_limit_field(bytes: u64) -> i64 {
    if bytes == 0 {
        -1
    } else {
        bytes.min(i64::MAX as u64) as i64
    }
}

pub(super) async fn qbit_transfer_limit_read(
    state: &AppState,
    query: &str,
    headers: &HeaderMap,
    dir: typhon_engine::torrent::ratelimit::Dir,
) -> Response {
    let query = query.to_string();
    guard!(*state, *headers, query);
    (
        StatusCode::OK,
        state.engines.client_rates().get(dir).rate().to_string(),
    )
        .into_response()
}

pub(super) async fn qbit_transfer_upload_limit(
    State(state): State<AppState>,
    RawQuery(query): RawQuery,
    headers: HeaderMap,
) -> Response {
    qbit_transfer_limit_read(
        &state,
        &query.unwrap_or_default(),
        &headers,
        typhon_engine::torrent::ratelimit::Dir::Up,
    )
    .await
}

pub(super) async fn qbit_transfer_download_limit(
    State(state): State<AppState>,
    RawQuery(query): RawQuery,
    headers: HeaderMap,
) -> Response {
    qbit_transfer_limit_read(
        &state,
        &query.unwrap_or_default(),
        &headers,
        typhon_engine::torrent::ratelimit::Dir::Down,
    )
    .await
}

pub(super) async fn qbit_transfer_set_limit(
    state: &AppState,
    query: &str,
    headers: &HeaderMap,
    body: &str,
    dir: typhon_engine::torrent::ratelimit::Dir,
) -> Response {
    let query = query.to_string();
    guard!(*state, *headers, query);
    let form = shim_form(&query, body);
    let Some(limit) = shim_limit(&form) else {
        return (StatusCode::BAD_REQUEST, "limit is required (bytes/s)").into_response();
    };
    if !set_client_rate_limit(state, dir, limit) {
        tracing::warn!("client rate cap set but not persisted: the store refused the write");
    }
    qbit_ok()
}

pub(super) async fn qbit_transfer_set_upload_limit(
    State(state): State<AppState>,
    RawQuery(query): RawQuery,
    headers: HeaderMap,
    body: String,
) -> Response {
    qbit_transfer_set_limit(
        &state,
        &query.unwrap_or_default(),
        &headers,
        &body,
        typhon_engine::torrent::ratelimit::Dir::Up,
    )
    .await
}

pub(super) async fn qbit_transfer_set_download_limit(
    State(state): State<AppState>,
    RawQuery(query): RawQuery,
    headers: HeaderMap,
    body: String,
) -> Response {
    qbit_transfer_set_limit(
        &state,
        &query.unwrap_or_default(),
        &headers,
        &body,
        typhon_engine::torrent::ratelimit::Dir::Down,
    )
    .await
}

pub(super) async fn qbit_torrents_limit_read(
    state: &AppState,
    query: &str,
    headers: &HeaderMap,
    body: &str,
    dir: typhon_engine::torrent::ratelimit::Dir,
) -> Response {
    let query = query.to_string();
    guard!(*state, *headers, query);
    let form = shim_form(&query, body);
    let mut out = serde_json::Map::new();
    for hash in shim_resolved_hashes(state, &form) {
        if let Some((_, t)) = find_torrent(state, &hash) {
            let (up, down) = t.rate_limits();
            let v = if dir == typhon_engine::torrent::ratelimit::Dir::Up {
                up
            } else {
                down
            };
            out.insert(hash, serde_json::json!(v));
        }
    }
    Json(serde_json::Value::Object(out)).into_response()
}

pub(super) async fn qbit_torrents_upload_limit(
    State(state): State<AppState>,
    RawQuery(query): RawQuery,
    headers: HeaderMap,
    body: String,
) -> Response {
    qbit_torrents_limit_read(
        &state,
        &query.unwrap_or_default(),
        &headers,
        &body,
        typhon_engine::torrent::ratelimit::Dir::Up,
    )
    .await
}

pub(super) async fn qbit_torrents_download_limit(
    State(state): State<AppState>,
    RawQuery(query): RawQuery,
    headers: HeaderMap,
    body: String,
) -> Response {
    qbit_torrents_limit_read(
        &state,
        &query.unwrap_or_default(),
        &headers,
        &body,
        typhon_engine::torrent::ratelimit::Dir::Down,
    )
    .await
}

pub(super) async fn qbit_torrents_set_limit(
    state: &AppState,
    query: &str,
    headers: &HeaderMap,
    body: &str,
    dir: typhon_engine::torrent::ratelimit::Dir,
) -> Response {
    let query = query.to_string();
    guard!(*state, *headers, query);
    let form = shim_form(&query, body);
    let Some(limit) = shim_limit(&form) else {
        return (StatusCode::BAD_REQUEST, "limit is required (bytes/s)").into_response();
    };
    let hashes = shim_resolved_hashes(state, &form);
    let applied = match dir {
        typhon_engine::torrent::ratelimit::Dir::Up => {
            shim_cap_torrents(state, &hashes, Some(limit), None)
        }
        typhon_engine::torrent::ratelimit::Dir::Down => {
            shim_cap_torrents(state, &hashes, None, Some(limit))
        }
    };
    match applied {
        Ok(()) => qbit_ok(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e).into_response(),
    }
}

pub(super) async fn qbit_torrents_set_upload_limit(
    State(state): State<AppState>,
    RawQuery(query): RawQuery,
    headers: HeaderMap,
    body: String,
) -> Response {
    qbit_torrents_set_limit(
        &state,
        &query.unwrap_or_default(),
        &headers,
        &body,
        typhon_engine::torrent::ratelimit::Dir::Up,
    )
    .await
}

pub(super) async fn qbit_torrents_set_download_limit(
    State(state): State<AppState>,
    RawQuery(query): RawQuery,
    headers: HeaderMap,
    body: String,
) -> Response {
    qbit_torrents_set_limit(
        &state,
        &query.unwrap_or_default(),
        &headers,
        &body,
        typhon_engine::torrent::ratelimit::Dir::Down,
    )
    .await
}

/// qBittorrent's `torrents/setShareLimits`: `hashes` (or `all`), `ratioLimit`
/// and `seedingTimeLimit` required as in qBittorrent, which answers 400
/// without them; `inactiveSeedingTimeLimit` optional, -2 when absent (its
/// default since 4.6). Query and body alike, POST only.
pub(super) async fn qbit_set_share_limits(
    State(state): State<AppState>,
    RawQuery(query): RawQuery,
    headers: HeaderMap,
    body: String,
) -> Response {
    let query = query.unwrap_or_default();
    guard!(state, headers, query);
    let form = shim_form(&query, &body);
    let num = |k: &str| {
        form.get(k)
            .and_then(|v| v.trim().parse::<f64>().ok())
            .filter(|v| v.is_finite())
    };
    let (Some(ratio), Some(seeding)) = (num("ratioLimit"), num("seedingTimeLimit")) else {
        return (
            StatusCode::BAD_REQUEST,
            "ratioLimit and seedingTimeLimit are required",
        )
            .into_response();
    };
    let inactive =
        num("inactiveSeedingTimeLimit").unwrap_or(crate::sharelimits::FOLLOW_ENGINE as f64);
    let hashes = shim_resolved_hashes(&state, &form);
    match crate::sharelimits::set_torrent_limits(
        &state,
        &hashes,
        Some(ratio),
        Some(seeding as i64),
        Some(inactive as i64),
    ) {
        Ok(_) => qbit_ok(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e).into_response(),
    }
}

/// qBittorrent's add: multipart, one or more `torrents` file parts.
///
/// This is the route autobrr, cross-seed and the *arrs use, so it is the one
/// that decides whether anything can reach this node at all. It answers plain
/// "Ok."/"Fails." like qBit, because those clients match on the body.
pub(super) async fn qbit_torrent_add(
    State(state): State<AppState>,
    RawQuery(query): RawQuery,
    headers: HeaderMap,
    mut multipart: axum::extract::Multipart,
) -> Response {
    let query = query.unwrap_or_default();
    guard!(state, headers, query);

    // `Bytes` as the reader hands them: a batch of up to `ADD_BODY_LIMIT` is
    // not copied a second time.
    let mut files: Vec<axum::body::Bytes> = Vec::new();
    let mut category = String::new();
    let mut save_path = String::new();
    let mut tags = String::new();
    let mut paused = false;
    // skip_checking is qBit's "trust the data on disk"; cross-seed relies on it
    // and treating it as false would re-hash every cross-seeded torrent.
    let mut seed_mode = false;
    let mut urls: Vec<String> = Vec::new();
    let mut up_limit: Option<u64> = None;
    let mut dl_limit: Option<u64> = None;
    // Share limits at add, `setShareLimits`' encoding: -2 follow, -1 none.
    let mut ratio_limit: Option<f64> = None;
    let mut seeding_limit: Option<i64> = None;
    let mut inactive_limit: Option<i64> = None;
    // qBittorrent's layout fields. `contentLayout` (4.3.2+) wins over the
    // older `root_folder` boolean; neither = the daemon's
    // `create_torrent_folder`, as `create_subfolder_enabled` reports it.
    let mut layout: Option<String> = None;
    let mut root_folder: Option<bool> = None;
    let mut auto_tmm = false;
    let mut sequential = false;
    let mut first_last = false;
    // Hydranos' own fields, the native upload's: where the batch goes, by
    // engine id or by role. Absent, the category decides, as it always has.
    // qBittorrent has neither and ignores what it does not know, so a client
    // may send them to both.
    let mut engine = String::new();
    let mut mode = String::new();
    let mut ignored: Vec<String> = Vec::new();

    // Read whole before anything is added. A batch past the body limit used to
    // end the loop quietly: the torrents read so far were added, the rest
    // dropped, and the client told "Ok." for all of them.
    let cut_short = |e: axum::extract::multipart::MultipartError| {
        let (status, why) = unreadable_upload(&e);
        tracing::warn!(error = %why, "qbit add refused");
        (status, "Fails.").into_response()
    };
    loop {
        let field = match multipart.next_field().await {
            Ok(Some(f)) => f,
            Ok(None) => break,
            Err(e) => return cut_short(e),
        };
        let name = field.name().unwrap_or_default().to_string();
        match name.as_str() {
            "torrents" | "file" | "fileselect" => match field.bytes().await {
                Ok(data) => files.push(data),
                Err(e) => return cut_short(e),
            },
            _ => {
                let value = match field.text().await {
                    Ok(v) => v,
                    Err(e) => return cut_short(e),
                };
                match name.as_str() {
                    "category" => category = value,
                    "savepath" => save_path = value,
                    "tags" => tags = value,
                    "paused" | "stopped" => paused = value == "true" || value == "1",
                    "skip_checking" => seed_mode = value == "true" || value == "1",
                    // Caps at add, bytes/s; 0 or negative = none.
                    "upLimit" => {
                        up_limit = value.trim().parse::<i64>().ok().map(|v| v.max(0) as u64)
                    }
                    "dlLimit" => {
                        dl_limit = value.trim().parse::<i64>().ok().map(|v| v.max(0) as u64)
                    }
                    "ratioLimit" => {
                        ratio_limit = value.trim().parse::<f64>().ok().filter(|v| v.is_finite())
                    }
                    "seedingTimeLimit" => {
                        seeding_limit = value.trim().parse::<f64>().ok().map(|v| v as i64)
                    }
                    "inactiveSeedingTimeLimit" => {
                        inactive_limit = value.trim().parse::<f64>().ok().map(|v| v as i64)
                    }
                    // One link per line: magnets, or URLs of .torrent files.
                    // It is how autobrr, Sonarr and Radarr send a magnet.
                    "urls" => urls.extend(
                        value
                            .lines()
                            .map(str::trim)
                            .filter(|l| !l.is_empty())
                            .map(String::from),
                    ),
                    "contentLayout" => layout = Some(value.trim().to_string()),
                    "root_folder" => root_folder = Some(value == "true" || value == "1"),
                    // Automatic management: the category decides where the
                    // data goes, which is what Hydranos does with no savepath.
                    "autoTMM" => auto_tmm = value == "true" || value == "1",
                    // Stop once the metadata is in, or once the files are
                    // checked: an add that stays stopped. Hydranos checks a
                    // stopped add's data all the same (`add_recheck_wanted`).
                    "stopCondition" => {
                        if value == "MetadataReceived" || value == "FilesChecked" {
                            paused = true;
                        }
                    }
                    // Download order at add: which pieces come first, never
                    // which come at all.
                    "sequentialDownload" => sequential = value == "true" || value == "1",
                    "firstLastPiecePrio" => first_last = value == "true" || value == "1",
                    "engine" => engine = value,
                    "mode" => mode = value,
                    // Accepted and without effect, said in the log once per
                    // add: `rename` (the name is the .torrent's), `cookie`.
                    other => {
                        if !value.is_empty() {
                            ignored.push(other.to_string());
                        }
                    }
                }
            }
        }
    }

    if files.is_empty() && urls.is_empty() {
        return (StatusCode::BAD_REQUEST, "Bad request").into_response();
    }
    if !ignored.is_empty() {
        tracing::info!(fields = ?ignored, "qbit add: these fields have no effect in Hydranos");
    }
    // Named and unknown is refused, never a quiet fallback to the category:
    // a batch meant for hoard that lands in race is then moved by somebody,
    // and a move in the middle of the add's recheck loses the recheck.
    let engine = match add_target_engine(&state, &engine, &mode) {
        Ok(e) => e,
        Err(e) => {
            tracing::warn!(error = %e, "qbit add refused");
            return (StatusCode::BAD_REQUEST, "Fails.").into_response();
        }
    };
    // autoTMM: the category's folder, never the one sent alongside -- that is
    // what qBittorrent does with a managed torrent's savepath.
    if auto_tmm && !category.is_empty() && !placement(&state, &category, "").1.is_empty() {
        save_path.clear();
    }
    // Neither a savepath nor a category with one: the folder a client set
    // with `setPreferences` `save_path`, if any (else the add is refused, as
    // Hydranos never guesses a destination).
    if save_path.is_empty() && placement(&state, &category, "").1.is_empty() {
        save_path = qbit_default_save_path(&state);
    }
    // (give a single-file torrent a folder, strip a multi-file torrent's).
    // The engine writes a multi-file torrent under its name and cannot strip
    // that folder, so NoSubfolder on one is refused rather than accepted into
    // a layout the client did not ask for -- cross-seed would link from paths
    // that do not exist. On a single file it is what happens anyway.
    let (subfolder, no_subfolder) = match layout.as_deref() {
        Some("Subfolder") => (true, false),
        Some("NoSubfolder") => (false, true),
        Some("Original") => (false, false),
        _ => match root_folder {
            Some(true) => (true, false),
            Some(false) => (false, true),
            None => (state.cfg().daemon.create_torrent_folder, false),
        },
    };

    // Sonarr and Radarr send their indexer's seed goal here (ratioLimit,
    // seedingTimeLimit in minutes): it lands on the torrent's own limits.
    let share_at_add = std::sync::Arc::new(move |st: &AppState, hash: String| {
        if ratio_limit.is_some() || seeding_limit.is_some() || inactive_limit.is_some() {
            if let Err(e) = crate::sharelimits::set_torrent_limits(
                st,
                &[hash],
                ratio_limit,
                seeding_limit,
                inactive_limit,
            ) {
                tracing::warn!("qbit add: share limits not stored: {e}");
            }
        }
    });
    let mut url_failed = 0;
    for u in &urls {
        let outcome = if crate::rss::is_magnet(u) {
            let (st, u2) = (state.clone(), u.clone());
            let (c, sp, tg) = (category.clone(), save_path.clone(), tags.clone());
            let eng = engine.clone();
            let shared = share_at_add.clone();
            // Kept with the request: applied once the metadata is in.
            let opts = crate::magnets::AddOptions {
                up_limit,
                dl_limit,
                sequential,
                first_last,
                subfolder,
                no_subfolder,
            };
            tokio::task::spawn_blocking(move || {
                crate::magnets::request_with_options(
                    &st, &u2, &c, &sp, &tg, paused, seed_mode, &eng, &opts,
                )
                .map(|hash| {
                    // Keyed by hash, so a magnet's limits wait for it in the
                    // store and apply once it has resolved.
                    shared(&st, hash);
                })
            })
            .await
            .unwrap_or_else(|e| Err(e.to_string()))
        } else {
            match crate::mcp::fetch_torrent(u).await {
                Ok(b) => {
                    files.push(b.into());
                    Ok(())
                }
                Err(e) => Err(e),
            }
        };
        if let Err(e) = outcome {
            if !e.contains("already added") {
                // A .torrent URL from a tracker carries the passkey: the
                // error names the URL, the log must not.
                let e = e.replace(u.as_str(), "<url>");
                tracing::warn!(error = %e, "qbit add: url refused");
                url_failed += 1;
            }
        }
    }
    if files.is_empty() {
        return if url_failed == urls.len() {
            (StatusCode::BAD_REQUEST, "Fails.").into_response()
        } else {
            (StatusCode::OK, "Ok.").into_response()
        };
    }
    // Off the async runtime: each add takes the store's lock, writes SQLite,
    // stats the save path and may wait on a path reservation. A batch of a
    // hundred run inline held a runtime worker for all of it.
    let total = files.len();
    let st = state.clone();
    let failed = tokio::task::spawn_blocking(move || {
        let state = st;
        let mut failed = 0usize;
        for bytes in &files {
            if no_subfolder
                && typhon_engine::torrent::metainfo::parse_torrent_bytes(bytes)
                    .is_ok_and(|m| m.multi_file)
            {
                tracing::warn!("qbit add refused: contentLayout NoSubfolder on a multi-file torrent is not supported");
                failed += 1;
                continue;
            }
            let save_path =
                match add_save_path(&state, bytes, &category, &engine, &save_path, subfolder) {
                    Ok(p) => p,
                    Err(e) => {
                        tracing::warn!(error = %e, "qbit add refused");
                        failed += 1;
                        continue;
                    }
                };
            match add_torrent_bytes(
                &state, bytes, &category, &save_path, &tags, paused, seed_mode, &engine,
            ) {
                // upLimit / dlLimit and the download order land on the torrent
                // the moment it exists, and persist with it. A magnet keeps them
                // with its request until then (`magnets::AddOptions`).
                Ok((hash, _)) => {
                    apply_add_options(&state, &hash, up_limit, dl_limit, sequential, first_last);
                    share_at_add(&state, hash);
                }
                // "already added" is not a failure to a client that retries a
                // release it has seen before; qBit answers Ok. for it too.
                Err(e) if e.contains("already added") => {}
                Err(e) => {
                    tracing::warn!(error = %e, "qbit add refused");
                    failed += 1;
                }
            }
        }
        failed
    })
    .await
    .unwrap_or_else(|e| {
        tracing::error!(error = %e, "qbit add: the add task ended abnormally");
        total
    });

    if failed == total && url_failed == urls.len() {
        return (StatusCode::BAD_REQUEST, "Fails.").into_response();
    }
    (StatusCode::OK, "Ok.").into_response()
}

/// qBittorrent's delete. Always 200, even for a hash it never had -- that is
/// what qBit does, and clients treat anything else as a failed batch.
pub(super) async fn qbit_delete(
    State(state): State<AppState>,
    RawQuery(query): RawQuery,
    headers: HeaderMap,
    Form(form): Form<Fields>,
) -> Response {
    let query = query.unwrap_or_default();
    guard!(state, headers, query);
    let cfg = state.cfg();
    let _ = cfg;

    // deleteFiles goes through the same path as the native DELETE. It used to
    // be read and honoured by nobody: with the flag set this handler dropped
    // neither the payload NOR the row and answered "Ok.", so every *arr that
    // removed a torrent with its data left both behind. That is how /race
    // reached 100% with no client aware of having failed at anything.
    let wants_files = form
        .get("deleteFiles")
        .map(|v| matches!(v.trim(), "true" | "1"))
        .unwrap_or(false);

    let hashes = qbit_hashes(&state, &form);
    let mut failed: Vec<String> = Vec::new();
    for prefix in hashes {
        let resolved = {
            let store = state.store.lock().unwrap();
            store.resolve_hash(&prefix)
        };
        let Some(hash) = resolved else { continue };
        // Unqualified, as qBit means it: every copy of this torrent.
        let sessions = {
            let store = state.store.lock().unwrap();
            store.sessions_of(&hash)
        };
        match remove_one_torrent(&state, &hash, &sessions, "", wants_files) {
            Ok(dropped) => {
                tracing::info!(hash = %hash, delete_files = wants_files, copies = dropped,
                    "torrent removed (qbit shim)");
            }
            Err(e) => {
                tracing::warn!(hash = %hash, "shim removal failed: {e}");
                failed.push(format!("{hash}: {e}"));
            }
        }
    }
    if !failed.is_empty() {
        // A client that is told "Ok." believes the torrents are gone.
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({"error": "some torrents were not removed", "failed": failed})),
        )
            .into_response();
    }
    qbit_ok()
}

#[allow(clippy::result_large_err)]
pub(super) async fn qbit_library(
    body: &str,
) -> Result<(crate::importer::Qbit, Vec<crate::importer::Candidate>), Response> {
    let creds = serde_json::from_str::<crate::importer::QbitCreds>(body)
        .map_err(|_| bad_request("invalid body: url, username, password"))?;
    if creds.url.trim().is_empty() {
        return Err(bad_request("empty qBittorrent URL"));
    }
    let qbit = crate::importer::Qbit::login(&creds)
        .await
        .map_err(|e| bad_request(&e))?;
    qbit.check_export_supported()
        .await
        .map_err(|e| bad_request(&e))?;
    let list = qbit.torrents().await.map_err(|e| {
        (
            StatusCode::BAD_GATEWAY,
            Json(serde_json::json!({"error": e})),
        )
            .into_response()
    })?;
    Ok((
        qbit,
        list.iter()
            .map(crate::importer::Candidate::from_qbit)
            .collect(),
    ))
}

/// qBittorrent's preferences setter.
///
/// Requires a `json` form field or a JSON body, and says so. Accepting an empty
/// call would answer 200 to a client that sent nothing -- it would believe its
/// settings took.
pub(super) async fn qbit_set_preferences(
    State(state): State<AppState>,
    RawQuery(query): RawQuery,
    headers: HeaderMap,
    body: String,
) -> Response {
    let query = query.unwrap_or_default();
    guard!(state, headers, query);
    let cfg = state.cfg();
    let _ = cfg;

    let has_form_json = body.split('&').any(|pair| {
        pair.split_once('=')
            .is_some_and(|(k, v)| k == "json" && !v.is_empty())
    });
    let has_json_body = serde_json::from_str::<serde_json::Value>(&body).is_ok();

    if !has_form_json && !has_json_body {
        return (
            StatusCode::BAD_REQUEST,
            "expected a `json` form field or a JSON body: EOF",
        )
            .into_response();
    }
    // The share limits and `lsd` are the keys this page writes; the rest is
    // accepted and ignored as before. Every local engine: a qBit client has one
    // session in mind and means all of it.
    let doc: serde_json::Value = if has_form_json {
        form_field(&body, "json")
            .and_then(|j| serde_json::from_str(&j).ok())
            .unwrap_or_default()
    } else {
        serde_json::from_str(&body).unwrap_or_default()
    };
    // Refused before anything is written, so a bad field changes nothing.
    if let Some(p) = doc.get("save_path").and_then(|v| v.as_str()).map(str::trim) {
        if !p.is_empty() && !std::path::Path::new(p).is_absolute() {
            return (
                StatusCode::BAD_REQUEST,
                format!("save_path must be absolute: {p}"),
            )
                .into_response();
        }
    }
    let change = crate::sharelimits::change_from_qbit_preferences(&doc);
    let shown =
        crate::sharelimits::engine_limits(&state, &crate::sharelimits::preferences_engine(&state));
    if !change.is_empty() && !change.is_noop_for(&shown) {
        let ids: Vec<String> = state
            .cfg()
            .local_engines()
            .into_iter()
            .map(|l| l.id)
            .collect();
        for id in ids {
            if let Err(e) = crate::sharelimits::write_engine_limits(&state, &id, &change) {
                tracing::warn!(engine = %id, "setPreferences: share limits not written: {e}");
                return (StatusCode::INTERNAL_SERVER_ERROR, e).into_response();
            }
        }
    }
    // `lsd`, written only when it differs from what `preferences` shows: a
    // client that posts back the whole page it read must not switch LSD on
    // for a hoard that the page never spoke for. Takes effect at the next
    // start of the engines, like the settings tab's `enable_lsd`.
    let lsd = doc.get("lsd").and_then(|v| {
        v.as_bool().or_else(|| match v.as_str()? {
            "true" | "1" => Some(true),
            "false" | "0" => Some(false),
            _ => None,
        })
    });
    if let Some(on) = lsd.filter(|on| *on != qbit_preferences_lsd(&state)) {
        for id in state.cfg().local_engines().into_iter().map(|l| l.id) {
            if let Err(e) =
                write_engine_key(&state, &id, "enable_lsd", if on { "true" } else { "false" })
            {
                tracing::warn!(engine = %id, "setPreferences: lsd not written: {e}");
                return (StatusCode::INTERNAL_SERVER_ERROR, e).into_response();
            }
        }
    }
    if let Err(e) = qbit_apply_more_preferences(&state, &doc) {
        return (StatusCode::INTERNAL_SERVER_ERROR, e).into_response();
    }
    // The rest is accepted and does nothing, as documented: said once in the
    // log, so an operator wondering why a client's setting did not take can
    // find out without reading the source.
    let ignored: Vec<&str> = doc
        .as_object()
        .map(|o| {
            o.keys()
                .map(String::as_str)
                .filter(|k| !QBIT_PREFERENCES_APPLIED.contains(k))
                .collect()
        })
        .unwrap_or_default();
    if !ignored.is_empty() {
        tracing::info!(keys = ?ignored, "setPreferences: these keys have no effect in Hydranos");
    }
    qbit_ok()
}

pub(crate) fn qbit_default_save_path(state: &AppState) -> String {
    state
        .store
        .lock()
        .unwrap()
        .setting(QBIT_SAVE_PATH_KEY)
        .ok()
        .flatten()
        .unwrap_or_default()
}

/// The `setPreferences` keys added in 4.4 beyond share limits and LSD.
///
/// Each is written only when it differs from what `app/preferences` shows:
/// a client that posts back the whole page it read must change nothing.
///
/// - `up_limit` / `dl_limit`: the client-wide caps, bytes/s, live and kept
///   (as `transfer/setUploadLimit`).
/// - `dht` / `pex`: every local engine's `enable_dht` / `enable_pex`, at the
///   next start of the engines.
/// - `max_active_downloads` -> `active_downloads` (live); `max_active_uploads`
///   / `max_active_torrents` -> `active_seeds` / `active_limit`, only while
///   queueing is on, as qBittorrent reads them. Queueing itself is switched in
///   Hydranos, not by a client: turning it on stops seeds.
/// - `dont_count_slow_torrents` and its three `slow_torrent_*` figures (KiB/s,
///   seconds) -> the same keys, live, with or without queueing: Hydranos's
///   `active_downloads` is a ceiling either way.
/// - `save_path`: where a shim add with no category and no `savepath` lands.
pub(super) fn qbit_apply_more_preferences(
    state: &AppState,
    doc: &serde_json::Value,
) -> Result<(), String> {
    let num = |k: &str| {
        doc.get(k).and_then(|v| {
            v.as_i64()
                .or_else(|| v.as_f64().map(|f| f as i64))
                .or_else(|| v.as_str()?.trim().parse().ok())
        })
    };
    let flag = |k: &str| {
        doc.get(k).and_then(|v| {
            v.as_bool().or_else(|| match v.as_str()? {
                "true" | "1" => Some(true),
                "false" | "0" => Some(false),
                _ => None,
            })
        })
    };
    let cfg = state.cfg();
    let engines: Vec<String> = cfg.local_engines().into_iter().map(|l| l.id).collect();
    let all = |key: &str, literal: &str| -> Result<(), String> {
        for id in &engines {
            write_engine_key(state, id, key, literal).map_err(|e| {
                tracing::warn!(engine = %id, key, "setPreferences: not written: {e}");
                e
            })?;
        }
        Ok(())
    };

    let client = state.engines.client_rates();
    for (key, dir) in [
        ("up_limit", typhon_engine::torrent::ratelimit::Dir::Up),
        ("dl_limit", typhon_engine::torrent::ratelimit::Dir::Down),
    ] {
        if let Some(v) = num(key) {
            let bytes = v.max(0) as u64;
            if bytes != client.get(dir).rate() && !set_client_rate_limit(state, dir, bytes) {
                tracing::warn!(
                    "setPreferences: {key} set but not persisted: the store refused the write"
                );
            }
        }
    }
    if let Some(on) = flag("dht").filter(|on| *on != cfg.race.enable_dht) {
        all("enable_dht", if on { "true" } else { "false" })?;
    }
    if let Some(on) = flag("pex").filter(|on| *on != cfg.race.enable_pex) {
        all("enable_pex", if on { "true" } else { "false" })?;
    }
    if let Some(v) = num("max_active_downloads").filter(|v| *v != qbit_shown_active_downloads(&cfg))
    {
        all("active_downloads", &v.max(-1).to_string())?;
    }
    // The slots of the choker (live; they act only while `choking` is on).
    // A `*_enabled: false` beside the figure reads as qBittorrent reads it: no
    // cap, whatever the figure kept.
    for (key, enabled_key, shown) in [
        ("max_uploads", "max_uploads_enabled", cfg.race.max_uploads),
        (
            "max_uploads_per_torrent",
            "max_uploads_per_torrent_enabled",
            cfg.race.max_uploads_per_torrent,
        ),
    ] {
        let want = if flag(enabled_key) == Some(false) {
            Some(-1)
        } else {
            num(key).map(|v| v.max(-1))
        };
        if let Some(v) = want.filter(|v| *v != shown) {
            all(key, &v.to_string())?;
            // Not carried by the queue workers (they re-read the config):
            // the slots live in each engine's policy, so put them there now.
            state.engines.apply_config(&state.cfg());
        }
    }
    if cfg.race.queueing.unwrap_or(false) {
        let shown = |v: Option<i64>| v.filter(|n| *n > 0).unwrap_or(-1);
        if let Some(v) = num("max_active_uploads").filter(|v| *v != shown(cfg.race.active_seeds)) {
            all("active_seeds", &v.max(-1).to_string())?;
        }
        if let Some(v) = num("max_active_torrents").filter(|v| *v != shown(cfg.race.active_limit)) {
            all("active_limit", &v.max(-1).to_string())?;
        }
    }
    if let Some(on) = flag("dont_count_slow_torrents")
        .filter(|on| *on != cfg.race.dont_count_slow_torrents.unwrap_or(false))
    {
        all(
            "dont_count_slow_torrents",
            if on { "true" } else { "false" },
        )?;
    }
    for (key, shown) in [
        (
            "slow_torrent_dl_rate_threshold",
            cfg.race.slow_torrent_dl_rate_threshold.unwrap_or(2),
        ),
        (
            "slow_torrent_ul_rate_threshold",
            cfg.race.slow_torrent_ul_rate_threshold.unwrap_or(2),
        ),
        (
            "slow_torrent_inactive_timer",
            cfg.race.slow_torrent_inactive_timer.unwrap_or(60),
        ),
    ] {
        if let Some(v) = num(key).filter(|v| *v != shown) {
            all(key, &v.max(0).to_string())?;
        }
    }
    if let Some(p) = doc
        .get("save_path")
        .and_then(|v| v.as_str())
        .map(|s| s.trim().to_string())
    {
        if p != qbit_default_save_path(state) {
            state
                .store
                .lock()
                .unwrap()
                .put_setting(QBIT_SAVE_PATH_KEY, &p)
                .map_err(|e| format!("save_path not stored: {e}"))?;
        }
    }
    Ok(())
}

/// `max_active_downloads` as `app/preferences` shows it: the race engine's
/// `active_downloads`, -1 (qBittorrent's "no limit") when it sets none.
pub(super) fn qbit_shown_active_downloads(cfg: &Config) -> i64 {
    if cfg.race.active_downloads > 0 {
        cfg.race.active_downloads
    } else {
        -1
    }
}

pub(super) fn qbit_refuse(code: StatusCode, msg: &str) -> Response {
    (code, msg.to_string()).into_response()
}

/// `torrents/setLocation`: move every copy's data, as **Set location...**.
///
/// The same plan and refusals as the native move (`queue_location_move`): a
/// job per copy, visible on the Jobs tab. qBittorrent's codes: 400 without a
/// location, 409 when nothing could be queued (the reason is the body).
pub(super) async fn qbit_set_location(
    State(state): State<AppState>,
    RawQuery(query): RawQuery,
    headers: HeaderMap,
    body: String,
) -> Response {
    let query = query.unwrap_or_default();
    guard!(state, headers, query);
    let form = shim_form(&query, &body);
    let location = form.get("location").cloned().unwrap_or_default();
    if location.trim().is_empty() {
        return qbit_refuse(StatusCode::BAD_REQUEST, "location is required");
    }
    let mut queued = 0usize;
    let mut refusal = String::new();
    for hash in shim_resolved_hashes(&state, &form) {
        for (engine, _) in torrent_copies(&state, &hash, "") {
            match queue_location_move(&state, &engine, &hash, &location, false) {
                Ok(()) => queued += 1,
                Err(e) => {
                    tracing::warn!(hash = %hash, engine = %engine, error = %e, "setLocation refused");
                    refusal = e;
                }
            }
        }
    }
    if queued == 0 && !refusal.is_empty() {
        return qbit_refuse(StatusCode::CONFLICT, &refusal);
    }
    qbit_ok()
}

/// `torrents/setForceStart`: start, past the queue.
///
/// `value=true` marks every copy forced (the pin Force download sets) and
/// starts it; the queue runs a forced torrent whatever its ceiling, without
/// counting it, as qBittorrent does. `value=false` lifts the mark and leaves
/// the torrent to the queue.
pub(super) async fn qbit_set_force_start(
    State(state): State<AppState>,
    RawQuery(query): RawQuery,
    headers: HeaderMap,
    body: String,
) -> Response {
    let query = query.unwrap_or_default();
    guard!(state, headers, query);
    let form = shim_form(&query, &body);
    let on = matches!(
        form.get("value").map(String::as_str),
        Some("true") | Some("1")
    );
    let hashes = shim_resolved_hashes(&state, &form);
    // Forced is the pin the queue reads: it runs past the ceiling and takes
    // no slot. 4.4 refused this with 409 on any engine with a queue -- the
    // hoard's default `active_downloads = 10` included -- so Sonarr's Force
    // Start did nothing on a default install.
    {
        let store = state.store.lock().unwrap();
        for hash in &hashes {
            if let Err(e) = store.set_pinned_everywhere(hash, on) {
                drop(store);
                return store_refused(&state, "setForceStart", e);
            }
        }
    }
    if !on {
        return qbit_ok();
    }
    let joined = hashes.join("|");
    let mut f = Fields::new();
    f.insert("hashes".into(), joined);
    if let Err(e) = set_paused(&state, &f, false) {
        return store_refused(&state, "setForceStart", e);
    }
    qbit_ok()
}

/// `torrents/filePrio`: the ORDER files are fetched in, never the set.
///
/// 1, 6 and 7 ("normal", "high", "maximal") set the file's priority on every
/// local copy: the pieces of a higher file are picked first. Priority 0 ("do
/// not download") is refused (409) rather than answered 200 over a file that
/// will be downloaded anyway: Hydranos is a seeding client, and a torrent
/// left incomplete by choice is one it could never seed.
pub(super) async fn qbit_file_prio(
    State(state): State<AppState>,
    RawQuery(query): RawQuery,
    headers: HeaderMap,
    body: String,
) -> Response {
    let query = query.unwrap_or_default();
    guard!(state, headers, query);
    let form = shim_form(&query, &body);
    let hash = form.get("hash").cloned().unwrap_or_default();
    let ids = form.get("id").cloned().unwrap_or_default();
    let Some(prio) = form
        .get("priority")
        .and_then(|p| p.trim().parse::<i64>().ok())
    else {
        return qbit_refuse(StatusCode::BAD_REQUEST, "priority is required");
    };
    if ![0, 1, 6, 7].contains(&prio) {
        return qbit_refuse(StatusCode::BAD_REQUEST, "priority is not valid");
    }
    let Some((_, torrent)) = find_torrent(&state, &hash) else {
        return not_found();
    };
    let n = torrent.meta.files.len();
    if ids
        .split('|')
        .any(|i| i.trim().parse::<usize>().map_or(true, |i| i >= n))
    {
        return qbit_refuse(StatusCode::CONFLICT, "file id is not valid");
    }
    if prio == 0 {
        return qbit_refuse(StatusCode::CONFLICT, FILE_PRIO_ZERO_REFUSAL);
    }
    let ids: Vec<usize> = ids
        .split('|')
        .filter_map(|i| i.trim().parse().ok())
        .collect();
    let copies = torrent_copies(
        &state,
        &typhon_engine::torrent::hex_encode(&torrent.info_hash),
        "",
    );
    let applied = edit_download_order(&state, &copies, |o| {
        for &id in &ids {
            o.set_file_priority(id, n, prio as u8);
        }
    });
    if let Err(e) = applied {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({"error": format!("not applied: {e}")})),
        )
            .into_response();
    }
    qbit_ok()
}

/// `torrents/toggleSequentialDownload` and `toggleFirstLastPiecePrio`:
/// `hashes` (or `all`), each torrent flipped from what it is now, as
/// qBittorrent does. Every local copy of a hash takes the new value of the
/// first, so two copies never end up flipped in opposite directions.
pub(super) async fn qbit_toggle_order_flag(
    state: &AppState,
    query: &str,
    headers: &HeaderMap,
    body: &str,
    sequential: bool,
) -> Response {
    let query = query.to_string();
    guard!(*state, *headers, query);
    let form = shim_form(&query, body);
    for hash in shim_resolved_hashes(state, &form) {
        let copies = torrent_copies(state, &hash, "");
        let Some((_, first)) = copies.first() else {
            continue;
        };
        let (seq, fl) = first.download_order_flags();
        let on = !if sequential { seq } else { fl };
        if let Err(e) = edit_download_order(state, &copies, |o| {
            if sequential {
                o.sequential = on;
            } else {
                o.first_last = on;
            }
        }) {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({"error": format!("not applied: {e}")})),
            )
                .into_response();
        }
    }
    qbit_ok()
}

pub(super) async fn qbit_toggle_sequential(
    State(state): State<AppState>,
    RawQuery(query): RawQuery,
    headers: HeaderMap,
    body: String,
) -> Response {
    qbit_toggle_order_flag(&state, &query.unwrap_or_default(), &headers, &body, true).await
}

pub(super) async fn qbit_toggle_first_last(
    State(state): State<AppState>,
    RawQuery(query): RawQuery,
    headers: HeaderMap,
    body: String,
) -> Response {
    qbit_toggle_order_flag(&state, &query.unwrap_or_default(), &headers, &body, false).await
}

/// `torrents/rename`: refused, 409.
///
/// The name Hydranos shows is the .torrent's: there is no separate display
/// name to change, and answering 200 would let a client believe the torrent
/// is now called something it is not.
pub(super) async fn qbit_rename(
    State(state): State<AppState>,
    RawQuery(query): RawQuery,
    headers: HeaderMap,
    body: String,
) -> Response {
    let query = query.unwrap_or_default();
    guard!(state, headers, query);
    let form = shim_form(&query, &body);
    let hash = form.get("hash").cloned().unwrap_or_default();
    if find_torrent(&state, &hash).is_none() {
        return not_found();
    }
    qbit_refuse(
        StatusCode::CONFLICT,
        "Hydranos shows the name from the .torrent; renaming a torrent is not supported",
    )
}

/// `torrents/topPrio`, `bottomPrio`, `increasePrio`, `decreasePrio`: 409.
///
/// qBittorrent answers 409 when queueing is off, and Hydranos's queue, when
/// on, orders by age: there is no position to move a torrent to.
pub(super) async fn qbit_queue_prio(
    State(state): State<AppState>,
    RawQuery(query): RawQuery,
    headers: HeaderMap,
    body: String,
) -> Response {
    let query = query.unwrap_or_default();
    guard!(state, headers, query);
    // The queues of the engines holding these torrents, not `[race] queueing`
    // alone: the hoard queues downloads by default (`active_downloads`).
    let form = shim_form(&query, &body);
    let queued: std::collections::HashSet<String> = state
        .cfg()
        .local_engines()
        .into_iter()
        .filter(|l| !crate::workers::QueueLimits::from_session(&l.session).is_empty())
        .map(|l| l.id)
        .collect();
    let any_queued = shim_resolved_hashes(&state, &form).iter().any(|h| {
        torrent_copies(&state, h, "")
            .iter()
            .any(|(e, _)| queued.contains(e))
    });
    if !any_queued {
        return qbit_refuse(StatusCode::CONFLICT, "Torrent queueing is not enabled");
    }
    qbit_refuse(StatusCode::CONFLICT, "Hydranos orders its queue itself (most seeders first for downloads, oldest first for seeds): positions cannot be set; force start runs a torrent past it")
}

/// `app/defaultSavePath`: the `save_path` of `app/preferences`.
pub(super) async fn qbit_default_save_path_route(
    State(state): State<AppState>,
    RawQuery(query): RawQuery,
    headers: HeaderMap,
) -> Response {
    let query = query.unwrap_or_default();
    guard!(state, headers, query);
    (StatusCode::OK, qbit_default_save_path(&state)).into_response()
}

/// `torrents/count`: how many torrents `torrents/info` lists.
pub(super) async fn qbit_torrents_count(
    State(state): State<AppState>,
    RawQuery(query): RawQuery,
    headers: HeaderMap,
) -> Response {
    let query = query.unwrap_or_default();
    guard!(state, headers, query);
    (StatusCode::OK, qbit_all_rows(&state).len().to_string()).into_response()
}

/// Every torrent once, as `torrents/info` lists it unfiltered.
pub(super) fn qbit_all_rows(state: &AppState) -> Vec<serde_json::Value> {
    let now = now_secs();
    let mut rows = Vec::new();
    let mut seen = std::collections::HashSet::new();
    let ids: Vec<String> = state
        .engines
        .engines()
        .iter()
        .map(|e| e.id.clone())
        .collect();
    for engine in &ids {
        for row in engine_qbit_rows(state, engine, now, None, None) {
            let h = row
                .get("hash")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            if seen.insert(h) {
                rows.push(row);
            }
        }
    }
    rows
}

/// `sync/maindata`: the whole state, every time (`full_update: true`).
///
/// qBittorrent answers a delta against the `rid` the client sends; a full
/// answer is valid at any `rid` and every client handles it -- it is what
/// they get on their first call. No delta is computed: that would mean
/// keeping each client's last view, and the listing this is built from costs
/// the same either way. `rid` still moves forward, as clients expect.
pub(super) async fn qbit_sync_maindata(
    State(state): State<AppState>,
    RawQuery(query): RawQuery,
    headers: HeaderMap,
) -> Response {
    let query = query.unwrap_or_default();
    guard!(state, headers, query);
    let rid = QBIT_SYNC_RID.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1;
    let mut torrents = serde_json::Map::new();
    for mut row in qbit_all_rows(&state) {
        let hash = row
            .get("hash")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        if let Some(o) = row.as_object_mut() {
            o.remove("hash");
        }
        torrents.insert(hash, row);
    }
    let mut categories = serde_json::Map::new();
    for (name, c) in categories_map(&state) {
        categories.insert(
            name.clone(),
            serde_json::json!({"name": name, "savePath": c.save_path}),
        );
    }
    let tags = state
        .store
        .lock()
        .unwrap()
        .registered_tags()
        .unwrap_or_default();
    // This session, as `transfer/info`.
    let (up, down) = state.engines.moved_totals();
    let (up_speed, down_speed) = state.engines.engines().iter().fold((0u64, 0u64), |acc, e| {
        (
            acc.0 + e.manager.upload_rate.get(),
            acc.1 + e.manager.download_rate.get(),
        )
    });
    let rates = state.engines.client_rates();
    Json(serde_json::json!({
        "rid": rid,
        "full_update": true,
        "torrents": torrents,
        "categories": categories,
        "tags": tags,
        "trackers": {},
        "server_state": {
            "connection_status": "connected",
            "dht_nodes": state.engines.dht_nodes(),
            "dl_info_data": down,
            "dl_info_speed": down_speed,
            "dl_rate_limit": rates.down.rate(),
            "up_info_data": up,
            "up_info_speed": up_speed,
            "up_rate_limit": rates.up.rate(),
            "queueing": state.cfg().race.queueing.unwrap_or(false),
            "use_alt_speed_limits": false,
            "refresh_interval": 1500,
        },
    }))
    .into_response()
}

/// qBittorrent's `lsd` preference: whether the engine the preferences page
/// speaks for (`sharelimits::preferences_engine`) runs LSD, as its file says.
pub(super) fn qbit_preferences_lsd(state: &AppState) -> bool {
    let id = crate::sharelimits::preferences_engine(state);
    state
        .cfg()
        .local_engines()
        .into_iter()
        .find(|l| l.id == id)
        .is_some_and(|l| l.session.lsd_on(&l.role))
}

/// qBittorrent's bulk reannounce.
///
/// ⚠⚠ This was `qbit_empty_ok`: it answered an empty 200 and announced NOTHING,
/// while the native `/api/torrents/:info_hash/reannounce` had been doing the
/// real work since 4.17.2. autobrr and the *arr clients speak this dialect and
/// no other, so every bulk reannounce they asked for was a no-op reported as a
/// success -- the same shape of lie as the 540-torrent bulk of 2026-09-12, one
/// layer further out.
///
/// ⭐ `try_send` with no reply channel, deliberately, and this is the one place
/// that differs from the native route. The native one waits for the scheduler's
/// answer because it speaks for a single click. Waiting on N answers here would
/// be a cost this handler cannot bound -- a client may name every torrent in the
/// catalogue. What is bounded is the queue: a scheduler too busy to accept a
/// bump is counted and logged, not waited for.
pub(super) async fn qbit_reannounce(
    State(state): State<AppState>,
    RawQuery(query): RawQuery,
    headers: HeaderMap,
    Form(form): Form<Fields>,
) -> Response {
    let query = query.unwrap_or_default();
    guard!(state, headers, query);
    let resolved = resolved_hashes(&state, &form);
    let asked = resolved.len();
    let mut bumped = 0usize;
    let mut not_announcing = 0usize;
    let mut queue_full = 0usize;
    for hash in resolved {
        for (id, copy) in copies_of(&state, &hash) {
            let Some(engine) = state.engines.get(&id) else {
                continue;
            };
            // Loaded but not on the network: there is no announce loop to jump.
            let Some(bump) = engine.bump.get() else {
                not_announcing += 1;
                continue;
            };
            match bump.try_send(crate::announce::scheduler::BumpReq {
                info_hash: copy.info_hash,
                reply: None,
                forced: true,
            }) {
                Ok(()) => bumped += 1,
                Err(_) => queue_full += 1,
            }
        }
    }
    // The answer stays an empty 200 because that is the qBittorrent contract and
    // a client would break on anything else. The counts go to the log, so a
    // reannounce that reached nothing is visible somewhere.
    tracing::info!(
        asked,
        bumped,
        not_announcing,
        queue_full,
        "qbit bulk reannounce"
    );
    qbit_ok()
}

/// qBittorrent's bulk recheck.
///
/// ⚠⚠ Also `qbit_empty_ok` until now: it claimed to have checked and had not.
/// Same fix, same reason -- the *arr clients call this one after an import to
/// confirm the data on disk, and an empty OK told them the check had run.
pub(super) async fn qbit_recheck(
    State(state): State<AppState>,
    RawQuery(query): RawQuery,
    headers: HeaderMap,
    Form(form): Form<Fields>,
) -> Response {
    let query = query.unwrap_or_default();
    guard!(state, headers, query);
    let resolved = resolved_hashes(&state, &form);
    let asked = resolved.len();
    let mut checking = 0usize;
    let mut refused = 0usize;
    for hash in resolved {
        for (id, copy) in copies_of(&state, &hash) {
            if state.engines.get(&id).is_none() {
                continue;
            }
            // The engine owns the hash: take the typed one off the torrent
            // rather than re-parsing the prefix the caller sent. Guarded like
            // the Verify button: the *arr call this right after an import.
            match crate::pathclaim::guarded_recheck(&state, &id, &copy) {
                Ok(()) => checking += 1,
                // recheck refuses a torrent it cannot check -- no metadata yet,
                // or a check already running. Counted, not fatal to the batch.
                Err(_) => refused += 1,
            }
        }
    }
    tracing::info!(asked, checking, refused, "qbit bulk recheck");
    qbit_ok()
}

/// Answer an unauthenticated qBittorrent call the way qBittorrent answers it.
///
/// ⭐⭐ qBittorrent's WebUI returns **403 Forbidden** when there is no session,
/// not 401. Sonarr and Radarr rely on that: `QBittorrentProxySelector` probes
/// `/api/v2/app/webapiVersion` *before* logging in, reads a 403 as "log in
/// first", and treats anything else as the server not being a qBittorrent at
/// all. Against a 401 the test failed with "Unable to connect to qBittorrent"
/// -- with the network fine and the credentials correct, which is as
/// misleading as an error message gets.
///
/// Only the shim. The native API keeps 401, which is the right code and what
/// its own callers expect; this is compatibility with one client's reading of
/// another server's quirk, and it is scoped to the paths that imitate it.
pub(super) async fn qbit_refusal_shape(
    req: axum::extract::Request,
    next: axum::middleware::Next,
) -> Response {
    let is_shim = req.uri().path().starts_with("/api/v2/");
    let res = next.run(req).await;
    if is_shim && res.status() == StatusCode::UNAUTHORIZED {
        return (StatusCode::FORBIDDEN, "Forbidden.").into_response();
    }
    res
}
