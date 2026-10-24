//! Directory listing for a tool that cannot see this host's disk, limited to the
//! folders Hydranos downloads into.
//!
//! Two routes, one listing: `GET /api/fs/list` (native) and
//! `/api/v2/app/getDirectoryContent` (the qBittorrent shim, GET or POST).
//!
//! What may be listed is what `pathroots::within_download_dirs` allows: the
//! download folders the daemon knows and what lies below them, compared after
//! both sides are canonicalised, so `..` and symbolic links cannot lead out,
//! and never the data folder (config, database). A path outside them is
//! refused with 403 whether or not it exists.
//!
//! Limits: at most `MAX_ENTRIES` entries per listing. The native answer then
//! carries `"truncated": true`; the shim, an array that cannot say so, refuses
//! the folder with 400 rather than answer a part of it, since its callers read
//! a name missing from the array as a name that is free. A symbolic
//! link is listed as a non-folder (`dir: false`, and not in `mode=dirs`): the
//! listing never follows a link to name its kind. Opening one as the listed
//! folder is allowed only if it resolves inside the roots.

use crate::api::{authorised, guard, query_param, shim_param, unauthorised, AppState};
use axum::{
    extract::{RawQuery, State},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    Json,
};
use std::path::{Component, Path, PathBuf};

/// Most entries one listing returns.
pub(crate) const MAX_ENTRIES: usize = 100_000;

enum Refusal {
    BadRequest(&'static str),
    Forbidden,
    NotFound,
}

impl Refusal {
    fn status(&self) -> StatusCode {
        match self {
            Refusal::BadRequest(_) => StatusCode::BAD_REQUEST,
            Refusal::Forbidden => StatusCode::FORBIDDEN,
            Refusal::NotFound => StatusCode::NOT_FOUND,
        }
    }
    fn message(&self) -> &'static str {
        match self {
            Refusal::BadRequest(m) => m,
            Refusal::Forbidden => "path is outside the download folders",
            Refusal::NotFound => "no such folder",
        }
    }
}

/// The canonical form of `raw` if it may be listed. The bounds are
/// `pathroots::within_download_dirs` (the download folders, never the data
/// folder); this only adds the answers a listing owes its caller: 400 for a
/// malformed path, 404 for a missing one inside the bounds, 403 for the rest.
/// Blocking.
fn permitted(state: &AppState, raw: &str) -> Result<PathBuf, Refusal> {
    if raw.is_empty() {
        return Err(Refusal::BadRequest("path is empty"));
    }
    let path = Path::new(raw);
    if !path.is_absolute() {
        return Err(Refusal::BadRequest("path must be absolute"));
    }
    if let Ok(real) = crate::pathroots::within_download_dirs(state, raw) {
        return Ok(real);
    }
    if std::fs::symlink_metadata(path).is_ok() {
        return Err(Refusal::Forbidden);
    }
    // Missing: judge it by its nearest existing ancestor, so a path outside
    // the bounds is 403 whatever is there. A `..` in the part that does not
    // exist cannot be resolved, so it is refused.
    for anc in path.ancestors().skip(1) {
        if std::fs::symlink_metadata(anc).is_ok() {
            let rest = path.strip_prefix(anc).unwrap_or(path);
            if rest.components().any(|c| c == Component::ParentDir) {
                return Err(Refusal::Forbidden);
            }
            let inside = anc
                .to_str()
                .is_some_and(|a| crate::pathroots::within_download_dirs(state, a).is_ok());
            return Err(if inside {
                Refusal::NotFound
            } else {
                Refusal::Forbidden
            });
        }
    }
    Err(Refusal::Forbidden)
}

struct Listing {
    /// (name, is a real folder), sorted by name.
    entries: Vec<(String, bool)>,
    truncated: bool,
}

fn list_blocking(state: &AppState, raw: &str, cap: usize) -> Result<Listing, Refusal> {
    let dir = permitted(state, raw)?;
    if !dir.is_dir() {
        return Err(Refusal::BadRequest("path is not a folder"));
    }
    let rd = std::fs::read_dir(&dir).map_err(|_| Refusal::Forbidden)?;
    let mut entries = Vec::new();
    let mut truncated = false;
    for e in rd.filter_map(|e| e.ok()) {
        if entries.len() >= cap {
            truncated = true;
            break;
        }
        let is_dir = e.file_type().map(|t| t.is_dir()).unwrap_or(false);
        entries.push((e.file_name().to_string_lossy().into_owned(), is_dir));
    }
    entries.sort();
    Ok(Listing { entries, truncated })
}

async fn list(state: &AppState, raw: String) -> Result<Listing, Refusal> {
    let st = state.clone();
    tokio::task::spawn_blocking(move || list_blocking(&st, &raw, MAX_ENTRIES))
        .await
        .unwrap_or(Err(Refusal::BadRequest("listing failed")))
}

/// `GET /api/fs/list?path=`: the entries of a download folder.
pub(crate) async fn get_fs_list(
    State(state): State<AppState>,
    RawQuery(query): RawQuery,
    headers: HeaderMap,
) -> Response {
    let query = query.unwrap_or_default();
    guard!(state, headers, query);
    let path = query_param(&query, "path").unwrap_or_default();
    match list(&state, path.clone()).await {
        Ok(l) => {
            let entries: Vec<_> = l
                .entries
                .iter()
                .map(|(n, d)| serde_json::json!({"name": n, "dir": d}))
                .collect();
            let mut out = serde_json::json!({"path": path, "entries": entries});
            if l.truncated {
                out["truncated"] = true.into();
            }
            Json(out).into_response()
        }
        Err(r) => (r.status(), Json(serde_json::json!({"error": r.message()}))).into_response(),
    }
}

/// `app/getDirectoryContent`: full paths, `mode` = all | dirs | files.
pub(crate) async fn qbit_directory_content(
    State(state): State<AppState>,
    RawQuery(query): RawQuery,
    headers: HeaderMap,
    body: String,
) -> Response {
    let query = query.unwrap_or_default();
    guard!(state, headers, query);
    let text = |status: StatusCode, msg: &str| {
        (
            status,
            [("content-type", "text/plain; charset=utf-8")],
            msg.to_string(),
        )
            .into_response()
    };
    let dir = shim_param(&query, &body, "dirPath").unwrap_or_default();
    let mode = shim_param(&query, &body, "mode").unwrap_or_else(|| "all".to_string());
    if !matches!(mode.as_str(), "all" | "dirs" | "files") {
        return text(StatusCode::BAD_REQUEST, "mode must be all, dirs or files");
    }
    match list(&state, dir.clone()).await {
        Ok(l) => match shim_paths(l, &dir, &mode) {
            Ok(paths) => Json(paths).into_response(),
            Err(why) => text(StatusCode::BAD_REQUEST, why),
        },
        // The shim has no 404: a missing folder is a bad `dirPath`, as in qBittorrent.
        Err(Refusal::NotFound) => text(StatusCode::BAD_REQUEST, Refusal::NotFound.message()),
        Err(r) => text(r.status(), r.message()),
    }
}

/// The shim's answer: full paths, or a refusal when the listing is not whole.
/// An array cannot say it was cut short, and its callers read a name missing
/// from it as a name that is free.
fn shim_paths(l: Listing, dir: &str, mode: &str) -> Result<Vec<String>, &'static str> {
    if l.truncated {
        return Err("folder has too many entries to list whole");
    }
    let base = dir.trim_end_matches('/');
    Ok(l.entries
        .into_iter()
        .filter(|(_, d)| match mode {
            "dirs" => *d,
            "files" => !*d,
            _ => true,
        })
        .map(|(n, _)| format!("{base}/{n}"))
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::testing::*;
    use axum::http::StatusCode;
    use tower::ServiceExt;

    const KEY: &str = "0123456789abcdef0123456789abcdef";

    fn st(tag: &str) -> TestState {
        state_from(tag, &format!("[daemon]\napi_key = \"{KEY}\"\n"))
    }

    /// `<dir>/dl` is a download folder (a category's); `<dir>/elsewhere` is not.
    fn tree(s: &TestState) -> (std::path::PathBuf, std::path::PathBuf) {
        let root = s.dir.join("dl");
        let other = s.dir.join("elsewhere");
        std::fs::create_dir_all(root.join("films/sub")).unwrap();
        std::fs::write(root.join("films/b.mkv"), b"x").unwrap();
        std::fs::write(root.join("films/a.nfo"), b"x").unwrap();
        std::fs::create_dir_all(&other).unwrap();
        std::fs::write(other.join("secret.txt"), b"x").unwrap();
        put_category(&s.state, "films", "hoard", root.to_str().unwrap());
        (root, other)
    }

    async fn call(
        s: &TestState,
        method: &str,
        uri: &str,
        form: &str,
        key: bool,
    ) -> (StatusCode, String, String) {
        let mut req = axum::http::Request::builder()
            .method(method)
            .uri(uri)
            .header("content-type", "application/x-www-form-urlencoded");
        if key {
            req = req.header("X-API-Key", KEY);
        }
        let resp = crate::api::router(s.state.clone())
            .oneshot(req.body(axum::body::Body::from(form.to_string())).unwrap())
            .await
            .unwrap();
        let status = resp.status();
        let ctype = resp
            .headers()
            .get("content-type")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_string();
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        (status, String::from_utf8_lossy(&bytes).into_owned(), ctype)
    }

    fn enc(v: &str) -> String {
        v.bytes()
            .map(|b| match b {
                b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' | b'/' => {
                    (b as char).to_string()
                }
                _ => format!("%{b:02X}"),
            })
            .collect()
    }

    async fn native(s: &TestState, path: &str) -> (StatusCode, serde_json::Value) {
        let (st, body, _) = call(
            s,
            "GET",
            &format!("/api/fs/list?path={}", enc(path)),
            "",
            true,
        )
        .await;
        (
            st,
            serde_json::from_str(&body).unwrap_or(serde_json::Value::Null),
        )
    }

    async fn shim(
        s: &TestState,
        method: &str,
        dir: &str,
        mode: Option<&str>,
    ) -> (StatusCode, String, String) {
        let mut q = format!("dirPath={}", enc(dir));
        if let Some(m) = mode {
            q.push_str(&format!("&mode={m}"));
        }
        if method == "GET" {
            call(
                s,
                "GET",
                &format!("/api/v2/app/getDirectoryContent?{q}"),
                "",
                true,
            )
            .await
        } else {
            call(s, "POST", "/api/v2/app/getDirectoryContent", &q, true).await
        }
    }

    #[tokio::test]
    async fn native_lists_a_download_folder_sorted_with_its_kind() {
        let s = st("fsl-list");
        let (root, _) = tree(&s);
        let dir = format!("{}/films", root.display());
        let (code, v) = native(&s, &dir).await;
        assert_eq!(code, StatusCode::OK);
        assert_eq!(v["path"], dir);
        assert_eq!(
            v["entries"],
            serde_json::json!([
                {"name": "a.nfo", "dir": false},
                {"name": "b.mkv", "dir": false},
                {"name": "sub", "dir": true},
            ])
        );
    }

    #[tokio::test]
    async fn native_lists_the_default_save_path_too() {
        let s = st("fsl-default");
        let d = s.dir.join("incoming");
        std::fs::create_dir_all(d.join("x")).unwrap();
        s.state
            .store
            .lock()
            .unwrap()
            .put_setting("qbit_default_save_path", d.to_str().unwrap())
            .unwrap();
        let (code, v) = native(&s, d.to_str().unwrap()).await;
        assert_eq!(code, StatusCode::OK);
        assert_eq!(
            v["entries"],
            serde_json::json!([{"name": "x", "dir": true}])
        );
    }

    #[tokio::test]
    async fn native_refuses_what_is_outside_the_download_folders() {
        let s = st("fsl-403");
        let (root, other) = tree(&s);
        assert_eq!(
            native(&s, other.to_str().unwrap()).await.0,
            StatusCode::FORBIDDEN
        );
        assert_eq!(native(&s, "/").await.0, StatusCode::FORBIDDEN);
        // The refusal does not tell a missing folder from a present one.
        let missing = other.join("nope");
        assert_eq!(
            native(&s, missing.to_str().unwrap()).await.0,
            StatusCode::FORBIDDEN
        );
        // A sibling that merely shares the root's name as a prefix is outside it.
        let lookalike = format!("{}-more", root.display());
        std::fs::create_dir_all(&lookalike).unwrap();
        assert_eq!(native(&s, &lookalike).await.0, StatusCode::FORBIDDEN);
    }

    #[tokio::test]
    async fn native_refuses_dotdot_that_leaves_the_root_and_follows_one_that_stays() {
        let s = st("fsl-dotdot");
        let (root, _other) = tree(&s);
        let out = format!("{}/films/../../elsewhere", root.display());
        assert_eq!(native(&s, &out).await.0, StatusCode::FORBIDDEN);
        let out_missing = format!("{}/films/../../elsewhere/nope", root.display());
        assert_eq!(native(&s, &out_missing).await.0, StatusCode::FORBIDDEN);
        let inside = format!("{}/films/sub/..", root.display());
        let (code, v) = native(&s, &inside).await;
        assert_eq!(code, StatusCode::OK);
        assert_eq!(v["entries"].as_array().unwrap().len(), 3);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn native_refuses_a_symlink_that_points_out_and_lists_it_as_not_a_dir() {
        let s = st("fsl-link");
        let (root, other) = tree(&s);
        std::os::unix::fs::symlink(&other, root.join("films/leak")).unwrap();
        std::os::unix::fs::symlink(root.join("films/sub"), root.join("films/inner")).unwrap();
        let leak = format!("{}/films/leak", root.display());
        assert_eq!(native(&s, &leak).await.0, StatusCode::FORBIDDEN);
        let leak_file = format!("{}/films/leak/secret.txt", root.display());
        assert_eq!(native(&s, &leak_file).await.0, StatusCode::FORBIDDEN);
        // A link is listed, as not a folder: the listing does not follow it.
        let (code, v) = native(&s, &format!("{}/films", root.display())).await;
        assert_eq!(code, StatusCode::OK);
        let kinds: Vec<(String, bool)> = v["entries"]
            .as_array()
            .unwrap()
            .iter()
            .map(|e| {
                (
                    e["name"].as_str().unwrap().to_string(),
                    e["dir"].as_bool().unwrap(),
                )
            })
            .collect();
        assert!(kinds.contains(&("leak".to_string(), false)));
        assert!(kinds.contains(&("inner".to_string(), false)));
        // A link that stays inside the root can be opened.
        let inner = format!("{}/films/inner", root.display());
        assert_eq!(native(&s, &inner).await.0, StatusCode::OK);
    }

    #[tokio::test]
    async fn native_404_for_a_missing_path_inside_a_root_and_400_for_the_malformed() {
        let s = st("fsl-404");
        let (root, _) = tree(&s);
        assert_eq!(
            native(&s, &format!("{}/nope", root.display())).await.0,
            StatusCode::NOT_FOUND
        );
        assert_eq!(native(&s, "").await.0, StatusCode::BAD_REQUEST);
        assert_eq!(native(&s, "films").await.0, StatusCode::BAD_REQUEST);
        assert_eq!(native(&s, "../etc").await.0, StatusCode::BAD_REQUEST);
        let (code, _, _) = call(&s, "GET", "/api/fs/list", "", true).await;
        assert_eq!(code, StatusCode::BAD_REQUEST);
        // A file, not a folder.
        let (code, v) = native(&s, &format!("{}/films/b.mkv", root.display())).await;
        assert_eq!(code, StatusCode::BAD_REQUEST);
        assert!(v["error"].is_string());
    }

    #[tokio::test]
    async fn a_folder_over_the_cap_is_listed_in_part_and_says_so() {
        let s = st("fsl-cap");
        let (root, _) = tree(&s);
        let big = root.join("big");
        std::fs::create_dir_all(&big).unwrap();
        for i in 0..5 {
            std::fs::write(big.join(format!("f{i}")), b"").unwrap();
        }
        let l = list_blocking(&s.state, big.to_str().unwrap(), 3)
            .ok()
            .unwrap();
        assert_eq!((l.entries.len(), l.truncated), (3, true));
        let l = list_blocking(&s.state, big.to_str().unwrap(), 5)
            .ok()
            .unwrap();
        assert_eq!((l.entries.len(), l.truncated), (5, false));
    }

    #[test]
    fn the_shim_refuses_a_listing_that_is_not_whole() {
        let part = || Listing {
            entries: vec![("a".into(), false), ("d".into(), true)],
            truncated: true,
        };
        for mode in ["all", "dirs", "files"] {
            assert!(shim_paths(part(), "/dl", mode).is_err(), "{mode}");
        }
        let whole = Listing {
            entries: vec![("a".into(), false), ("d".into(), true)],
            truncated: false,
        };
        assert_eq!(
            shim_paths(whole, "/dl/", "all").unwrap(),
            vec!["/dl/a".to_string(), "/dl/d".to_string()]
        );
    }

    #[tokio::test]
    async fn neither_route_lists_the_data_folder_even_under_a_download_folder() {
        // The data folder holds the config and the database: not even their
        // names are listed, wherever it sits.
        let base = std::env::temp_dir().join(format!("fsl-data-{}", std::process::id()));
        let data = base.join("dl").join("config");
        std::fs::create_dir_all(&data).unwrap();
        std::fs::write(data.join("default.toml"), b"x").unwrap();
        let s = state_from(
            "fsl-data",
            &format!(
                "[daemon]\napi_key = \"{KEY}\"\ndata_dir = \"{}\"\n",
                data.display()
            ),
        );
        put_category(
            &s.state,
            "films",
            "hoard",
            base.join("dl").to_str().unwrap(),
        );
        let (code, v) = native(&s, base.join("dl").to_str().unwrap()).await;
        assert_eq!(code, StatusCode::OK, "{v}");
        let (code, _) = native(&s, data.to_str().unwrap()).await;
        assert_eq!(code, StatusCode::FORBIDDEN);
        let (code, _) = native(&s, data.join("missing").to_str().unwrap()).await;
        assert_eq!(code, StatusCode::FORBIDDEN);
        let (code, _, _) = shim(&s, "GET", data.to_str().unwrap(), None).await;
        assert_eq!(code, StatusCode::FORBIDDEN);
        let _ = std::fs::remove_dir_all(&base);
    }

    #[tokio::test]
    async fn both_routes_refuse_without_the_key() {
        let s = st("fsl-auth");
        let (root, _) = tree(&s);
        let dir = format!("{}/films", root.display());
        let (code, _, _) = call(
            &s,
            "GET",
            &format!("/api/fs/list?path={}", enc(&dir)),
            "",
            false,
        )
        .await;
        assert_eq!(code, StatusCode::UNAUTHORIZED);
        // The shim answers an unkeyed call 403 "Forbidden." as qBittorrent does.
        let (code, body, _) = call(
            &s,
            "GET",
            &format!("/api/v2/app/getDirectoryContent?dirPath={}", enc(&dir)),
            "",
            false,
        )
        .await;
        assert_eq!((code, body.as_str()), (StatusCode::FORBIDDEN, "Forbidden."));
        let (code, body, _) = call(
            &s,
            "POST",
            "/api/v2/app/getDirectoryContent",
            &format!("dirPath={}", enc(&dir)),
            false,
        )
        .await;
        assert_eq!((code, body.as_str()), (StatusCode::FORBIDDEN, "Forbidden."));
    }

    #[tokio::test]
    async fn shim_modes_give_full_paths_by_get_and_post() {
        let s = st("fsl-shim");
        let (root, _) = tree(&s);
        let dir = format!("{}/films", root.display());
        for method in ["GET", "POST"] {
            let (code, body, _) = shim(&s, method, &dir, None).await;
            assert_eq!(code, StatusCode::OK);
            let all: Vec<String> = serde_json::from_str(&body).unwrap();
            assert_eq!(
                all,
                vec![
                    format!("{dir}/a.nfo"),
                    format!("{dir}/b.mkv"),
                    format!("{dir}/sub")
                ]
            );

            let (_, body, _) = shim(&s, method, &dir, Some("all")).await;
            assert_eq!(serde_json::from_str::<Vec<String>>(&body).unwrap().len(), 3);
            let (_, body, _) = shim(&s, method, &dir, Some("dirs")).await;
            assert_eq!(
                serde_json::from_str::<Vec<String>>(&body).unwrap(),
                vec![format!("{dir}/sub")]
            );
            let (_, body, _) = shim(&s, method, &dir, Some("files")).await;
            assert_eq!(
                serde_json::from_str::<Vec<String>>(&body).unwrap(),
                vec![format!("{dir}/a.nfo"), format!("{dir}/b.mkv")]
            );
        }
        // A trailing slash does not double up.
        let (_, body, _) = shim(&s, "GET", &format!("{dir}/"), Some("dirs")).await;
        assert_eq!(
            serde_json::from_str::<Vec<String>>(&body).unwrap(),
            vec![format!("{dir}/sub")]
        );
    }

    #[tokio::test]
    async fn shim_refuses_outside_roots_and_answers_400_text_for_the_rest() {
        let s = st("fsl-shim-err");
        let (root, other) = tree(&s);
        let (code, _, _) = shim(&s, "GET", other.to_str().unwrap(), None).await;
        assert_eq!(code, StatusCode::FORBIDDEN);
        let (code, _, _) = shim(
            &s,
            "POST",
            &format!("{}/films/../../elsewhere", root.display()),
            None,
        )
        .await;
        assert_eq!(code, StatusCode::FORBIDDEN);
        let (code, _, _) = shim(&s, "GET", "/nonexistent-root-dir", None).await;
        assert_eq!(code, StatusCode::FORBIDDEN);

        for (dir, mode) in [
            (format!("{}/nope", root.display()), None),
            (format!("{}/films/b.mkv", root.display()), None),
            ("films".to_string(), None),
            (String::new(), None),
            (format!("{}/films", root.display()), Some("nonsense")),
        ] {
            let (code, body, ctype) = shim(&s, "GET", &dir, mode).await;
            assert_eq!(code, StatusCode::BAD_REQUEST, "{dir:?} {mode:?}");
            assert!(ctype.starts_with("text/plain"), "{ctype}");
            assert!(!body.is_empty());
        }
    }
}
