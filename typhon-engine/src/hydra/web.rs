//! Serving the interface.
//!
//! The one thing a parity bench comparing `/api/*` answers cannot see: the
//! pages are not API routes, so a build that serves none of them passes every
//! comparison. This was found by opening the site, which is the only way it
//! could have been.
//!
//! Assets are baked into the binary, as 3.x bakes them: a single file that
//! needs nothing beside it is what makes a rollback a matter of swapping one
//! image.

use axum::body::Body;
use axum::extract::{Path, State};
use axum::http::{header, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};

use crate::api::AppState;

/// Everything under `web/`, at compile time.
static INDEX_HTML: &str = include_str!("../../../web/templates/index.html");

/// The interface itself.
///
/// The template carries one construct, `{{if not .FrontOnly}}`, around the
/// parts that only make sense on a node that runs engines. A controller's own
/// egress is not the fleet's, and showing an address no torrent announces from
/// is worse than showing nothing.
pub async fn index(State(state): State<AppState>) -> Response {
    page(state.engines.engines().is_empty())
}

/// The page, with or without its engine panels. Also served while the
/// daemon is still starting, so the startup screen has a page to draw on.
pub fn page(front_only: bool) -> Response {
    let html = render(INDEX_HTML, front_only);
    (
        StatusCode::OK,
        [
            (header::CONTENT_TYPE, HeaderValue::from_static("text/html; charset=utf-8")),
            // Clickjacking: the interface is never meant to be framed.
            (header::X_FRAME_OPTIONS, HeaderValue::from_static("DENY")),
            // Defense in depth. Only directives that cannot break a same-origin
            // single-page app are set: no plugins, no <base> hijack, no framing,
            // and scripts only from this origin. The page loads nothing from
            // anywhere else, so a script tag pointing off-site is refused.
            // 'unsafe-inline' stays: the page is built on inline handlers and
            // one inline script, and dropping it is a rewrite of the interface.
            (
                header::CONTENT_SECURITY_POLICY,
                HeaderValue::from_static(
                    "frame-ancestors 'none'; object-src 'none'; base-uri 'self'; script-src 'self' 'unsafe-inline'",
                ),
            ),
        ],
        html,
    )
        .into_response()
}

/// Resolve the one template construct the page uses.
///
/// Not a template engine: the page has exactly one conditional, and pulling in
/// an engine to evaluate it would be a dependency for a single `if`.
pub fn render(template: &str, front_only: bool) -> String {
    const OPEN: &str = "{{if not .FrontOnly}}";
    const CLOSE: &str = "{{end}}";
    let Some(start) = template.find(OPEN) else {
        return template.to_string();
    };
    let Some(end) = template[start..].find(CLOSE).map(|i| start + i) else {
        // An unclosed conditional means the page has changed shape. Serving it
        // whole is wrong in one way; serving nothing is wrong in every way.
        return template.to_string();
    };
    let mut out = String::with_capacity(template.len());
    out.push_str(&template[..start]);
    if !front_only {
        out.push_str(&template[start + OPEN.len()..end]);
    }
    out.push_str(&template[end + CLOSE.len()..]);
    out
}

/// One file from `web/static`.
pub async fn static_file(Path(path): Path<String>) -> Response {
    let Some((bytes, mime)) = lookup(&path) else {
        return (StatusCode::NOT_FOUND, "not found").into_response();
    };
    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, mime)
        // no-cache, as 3.x sends: the interface ships with the binary, so a
        // cached asset outliving an upgrade is a page half from each version.
        .header(header::CACHE_CONTROL, "no-cache")
        .body(Body::from(bytes))
        .unwrap_or_else(|_| StatusCode::INTERNAL_SERVER_ERROR.into_response())
}

macro_rules! asset {
    ($path:literal, $mime:literal) => {
        (
            $path,
            include_bytes!(concat!("../../../web/static/", $path)) as &[u8],
            $mime,
        )
    };
}

/// The files the page asks for.
///
/// Listed rather than walked: `include_bytes!` needs a literal path, and a
/// list that must be edited when an asset is added is a list whose absence is
/// noticed at build time rather than by a blank page.
const ASSETS: &[(&str, &[u8], &str)] = &[
    asset!("app.js", "application/javascript; charset=utf-8"),
    asset!("i18n.js", "application/javascript; charset=utf-8"),
    asset!("style.css", "text/css; charset=utf-8"),
    asset!("chart.umd.min.js", "application/javascript; charset=utf-8"),
    asset!("hydra-logo.png", "image/png"),
    asset!("hydra-banner.png", "image/png"),
    // The translations i18n.js fetches at runtime. Leaving them out gives an
    // English interface with nothing saying why.
    asset!("i18n/de.json", "application/json; charset=utf-8"),
    asset!("i18n/es.json", "application/json; charset=utf-8"),
    asset!("i18n/fr.json", "application/json; charset=utf-8"),
    asset!("i18n/it.json", "application/json; charset=utf-8"),
    asset!("i18n/nl.json", "application/json; charset=utf-8"),
    asset!("i18n/pt.json", "application/json; charset=utf-8"),
];

fn lookup(path: &str) -> Option<(Vec<u8>, &'static str)> {
    // A path that climbs out of the asset set is refused rather than resolved:
    // the files are baked in, so there is nothing above to reach, and a lookup
    // that accepted `..` would be a habit waiting for a filesystem.
    if path.contains("..") {
        return None;
    }
    ASSETS
        .iter()
        .find(|(name, _, _)| *name == path)
        .map(|(_, bytes, mime)| (bytes.to_vec(), *mime))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_engine_only_block_is_kept_on_a_node_that_runs_engines() {
        let t = "before{{if not .FrontOnly}}ENGINES{{end}}after";
        assert_eq!(render(t, false), "beforeENGINESafter");
        assert_eq!(
            render(t, true),
            "beforeafter",
            "a controller shows no engine panel"
        );
    }

    #[test]
    fn a_page_without_the_construct_is_served_whole() {
        assert_eq!(render("plain page", false), "plain page");
        // An unclosed conditional is a changed page, not a reason to serve
        // nothing.
        assert_eq!(
            render("a{{if not .FrontOnly}}b", false),
            "a{{if not .FrontOnly}}b"
        );
    }

    #[test]
    fn the_real_page_is_baked_in_and_renders() {
        assert!(
            INDEX_HTML.len() > 10_000,
            "the interface is embedded, not a stub"
        );
        let rendered = render(INDEX_HTML, false);
        assert!(
            !rendered.contains("{{if not .FrontOnly}}"),
            "the construct is resolved"
        );
        assert!(rendered.contains("<html") || rendered.contains("<!DOCTYPE"));
    }

    /// The category form offers only what routing reads. For all of 4.x it
    /// offered a strategy, a free-space reserve and per-agent placement that
    /// `placement()` never looked at: settings that saved and did nothing. A
    /// routing field comes back here together with the code that honours it.
    #[test]
    fn the_category_form_offers_no_routing_the_engine_ignores() {
        for dead in [
            "cat-strategy",
            "cat-min-free",
            "cat-placement",
            "least_torrents",
            "fill_then_next",
        ] {
            assert!(
                !INDEX_HTML.contains(dead),
                "index.html offers `{dead}` again"
            );
        }
        let (js, _) = lookup("app.js").expect("app.js");
        let js = String::from_utf8_lossy(&js);
        for dead in [
            "cat-strategy",
            "cat-min-free",
            ".cat-agent-cb",
            "_renderCatPlacement",
        ] {
            assert!(!js.contains(dead), "app.js reads `{dead}` again");
        }
    }

    /// The page authenticates its streams, its export and its node link with
    /// the session cookie. A key in an address lands in the browser history, a
    /// proxy's access log and the Referer; the daemon still accepts it for
    /// callers that cannot do better, but the page itself never sends it.
    #[test]
    fn the_page_never_puts_the_api_key_in_an_address() {
        let (js, _) = lookup("app.js").expect("app.js");
        let js = String::from_utf8_lossy(&js);
        assert!(
            !js.to_ascii_lowercase().contains("apikey="),
            "app.js builds an address carrying the key"
        );
        assert!(
            !js.contains("\"apikey\""),
            "app.js sets a `apikey` query parameter"
        );
    }

    /// The page builds rows out of text the swarm and the lists supply
    /// (tracker hosts, node names, categories). `esc()` writes HTML entities,
    /// which the browser decodes BEFORE it runs an event attribute, so a value
    /// interpolated into `onclick="f('${esc(x)}')"` is not a string literal any
    /// more. A handler attribute therefore carries no interpolation at all: the
    /// value goes in an escaped `data-*` attribute and the handler reads it
    /// from `this.dataset`.
    ///
    /// The scan: after each `on<event>="`, the first of `"` or `${` decides. A
    /// `${` first means an expression sits inside the handler.
    #[test]
    fn no_event_attribute_interpolates_a_value() {
        let (js, _) = lookup("app.js").expect("app.js");
        let js = String::from_utf8_lossy(&js);
        let start = regex::Regex::new(r#"\bon[a-z]+="|\bon[a-z]+=\\?'"#).unwrap();
        let mut bad = Vec::new();
        for m in start.find_iter(&js) {
            let rest = &js[m.end()..];
            let quote = if m.as_str().ends_with('"') { '"' } else { '\'' };
            let q = rest.find(quote).unwrap_or(rest.len());
            let d = rest.find("${").unwrap_or(rest.len());
            if d < q {
                let line = js[..m.start()].matches('\n').count() + 1;
                let shown: String = js[m.start()..].chars().take(m.len() + d + 20).collect();
                bad.push(format!("line {line}: {shown}"));
            }
        }
        assert!(
            bad.is_empty(),
            "event attributes interpolate a value:\n{}",
            bad.join("\n")
        );
    }

    #[test]
    fn every_asset_the_page_needs_is_present() {
        for name in ["app.js", "i18n.js", "style.css"] {
            let (bytes, _) = lookup(name).unwrap_or_else(|| panic!("{name} is missing"));
            assert!(!bytes.is_empty(), "{name} is empty");
        }
        assert!(lookup("../../etc/passwd").is_none(), "no climbing out");
        assert!(lookup("nope.js").is_none());
    }
}

#[cfg(test)]
mod security_header_tests {
    use crate::startup::{gate, Early};
    use axum::body::Body;
    use axum::http::{header, Request, StatusCode};
    use std::sync::{Arc, OnceLock};
    use tower::ServiceExt;

    /// The headers every page answer must carry, whichever way the page is
    /// served: one that can be framed by another site, or that lets a plugin or
    /// a `<base>` tag in, is a page someone else can drive.
    fn assert_page_is_locked_down(h: &axum::http::HeaderMap, what: &str) {
        let xfo = h
            .get(header::X_FRAME_OPTIONS)
            .unwrap_or_else(|| panic!("{what}: no X-Frame-Options"));
        assert_eq!(xfo, "DENY", "{what}: the interface must not be framable");
        let csp = h
            .get(header::CONTENT_SECURITY_POLICY)
            .unwrap_or_else(|| panic!("{what}: no Content-Security-Policy"))
            .to_str()
            .expect("the policy is text");
        for directive in [
            "frame-ancestors 'none'",
            "object-src 'none'",
            "base-uri 'self'",
        ] {
            assert!(
                csp.contains(directive),
                "{what}: policy lacks `{directive}`: {csp}"
            );
        }
        // Scripts from this origin only, with the inline handlers the page is
        // built on still allowed: a policy that cut those would blank the UI.
        let script_src = csp
            .split(';')
            .map(str::trim)
            .find(|d| d.starts_with("script-src"))
            .unwrap_or_else(|| panic!("{what}: policy has no script-src: {csp}"));
        assert!(
            script_src.contains("'self'"),
            "{what}: script-src lacks 'self': {script_src}"
        );
        assert!(
            script_src.contains("'unsafe-inline'"),
            "{what}: script-src would block the page's inline handlers: {script_src}"
        );
        for foreign in ["*", "http:", "https:", "data:"] {
            assert!(
                !script_src.split_whitespace().any(|t| t == foreign),
                "{what}: script-src admits `{foreign}`: {script_src}"
            );
        }
    }

    /// The page as the listener serves it: through the gate, on the real `/`
    /// route, with and without the engine panels.
    #[tokio::test]
    async fn the_interface_page_forbids_framing_and_plugins() {
        for front_only in [false, true] {
            let early = Early {
                front_only,
                needs_setup: false,
                network_storage: "",
            };
            let router = gate(Arc::new(OnceLock::new()), early);
            let resp = router
                .oneshot(Request::builder().uri("/").body(Body::empty()).unwrap())
                .await
                .unwrap();
            assert_eq!(resp.status(), StatusCode::OK);
            assert_page_is_locked_down(resp.headers(), &format!("GET / (front_only={front_only})"));
            assert_eq!(
                resp.headers()
                    .get(header::CONTENT_TYPE)
                    .map(|v| v.to_str().unwrap()),
                Some("text/html; charset=utf-8"),
                "the page keeps its content type"
            );
        }
    }

    /// The handler on its own, which the full router also reaches.
    #[tokio::test]
    async fn the_page_handler_sets_the_same_headers() {
        for front_only in [false, true] {
            let resp = super::page(front_only);
            assert_page_is_locked_down(resp.headers(), &format!("page({front_only})"));
        }
    }
}
