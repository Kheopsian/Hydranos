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
    let front_only = state.engines.engines().is_empty();
    let html = render(INDEX_HTML, front_only);
    (
        StatusCode::OK,
        [(header::CONTENT_TYPE, HeaderValue::from_static("text/html; charset=utf-8"))],
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
        assert_eq!(render(t, true), "beforeafter", "a controller shows no engine panel");
    }

    #[test]
    fn a_page_without_the_construct_is_served_whole() {
        assert_eq!(render("plain page", false), "plain page");
        // An unclosed conditional is a changed page, not a reason to serve
        // nothing.
        assert_eq!(render("a{{if not .FrontOnly}}b", false), "a{{if not .FrontOnly}}b");
    }

    #[test]
    fn the_real_page_is_baked_in_and_renders() {
        assert!(INDEX_HTML.len() > 10_000, "the interface is embedded, not a stub");
        let rendered = render(INDEX_HTML, false);
        assert!(!rendered.contains("{{if not .FrontOnly}}"), "the construct is resolved");
        assert!(rendered.contains("<html") || rendered.contains("<!DOCTYPE"));
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
