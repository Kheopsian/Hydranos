//! Editing a torrent's tracker list.
//!
//! Trackers are grouped in tiers, and the tier structure is not decoration: the
//! announce loop walks tiers in order and only falls through to the next when
//! the current one fails. Flattening them changes which tracker a torrent talks
//! to first, which changes who gets credited for the upload.
//!
//! Ported from internal/api/trackers_edit.go.

/// Accept only a URL a tracker could actually live at.
pub fn normalise_url(raw: &str) -> Result<String, String> {
    let url = raw.trim();
    if url.is_empty() {
        return Err("empty tracker URL".into());
    }
    let Some((scheme, rest)) = url.split_once("://") else {
        return Err(format!(
            "{url:?}: a tracker URL has to start with http://, https:// or udp://"
        ));
    };
    // A UDP tracker has no default port: one typed without it would be
    // announced to nowhere, every pass, with an error that does not say why.
    if scheme.eq_ignore_ascii_case("udp") {
        if typhon_engine::tracker::udp::split_url(url).is_none() {
            return Err(format!(
                "{url:?}: a UDP tracker needs a host and a port, udp://host:port/announce"
            ));
        }
        return Ok(url.to_string());
    }
    if !matches!(scheme, "http" | "https") {
        return Err(format!(
            "{url:?}: a tracker URL has to start with http://, https:// or udp://"
        ));
    }
    let host = rest.split(['/', '?', '#']).next().unwrap_or("");
    if host.is_empty() {
        return Err(format!("{url:?} has no host"));
    }
    Ok(url.to_string())
}

fn contains(tiers: &[Vec<String>], want: &str) -> bool {
    tiers.iter().any(|tier| tier.iter().any(|u| u == want))
}

/// Drop empty tiers and empty URLs.
///
/// An empty tier is a level of the fallback chain that can never answer, so the
/// announce loop would just walk past it -- but it would still count as a tier
/// when the list is renumbered, which shifts everything below it.
fn compact(tiers: Vec<Vec<String>>) -> Vec<Vec<String>> {
    tiers
        .into_iter()
        .map(|tier| {
            tier.into_iter()
                .filter(|u| !u.is_empty())
                .collect::<Vec<_>>()
        })
        .filter(|tier: &Vec<String>| !tier.is_empty())
        .collect()
}

/// Apply one edit. Returns the new tiers and whether anything actually moved.
pub fn apply(
    tiers: &[Vec<String>],
    op: &str,
    urls: &[String],
    from: &str,
    to: &str,
) -> Result<(Vec<Vec<String>>, bool), String> {
    match op {
        "add" => {
            let mut out: Vec<Vec<String>> = tiers.to_vec();
            let mut added = false;
            for raw in urls {
                let url = normalise_url(raw)?;
                // Adding one that is already there is a no-op, not a duplicate:
                // a torrent announcing twice to the same URL doubles its own
                // load and shows up at the tracker as two peers.
                if contains(&out, &url) {
                    continue;
                }
                out.push(vec![url]);
                added = true;
            }
            Ok((compact(out), added))
        }

        "remove" => {
            let wanted: std::collections::HashSet<&str> = urls
                .iter()
                .map(|u| u.trim())
                .filter(|u| !u.is_empty())
                .collect();
            if wanted.is_empty() {
                return Err("remove needs at least one URL".into());
            }
            let mut removed = false;
            let out: Vec<Vec<String>> = tiers
                .iter()
                .map(|tier| {
                    tier.iter()
                        .filter(|u| {
                            let keep = !wanted.contains(u.as_str());
                            if !keep {
                                removed = true;
                            }
                            keep
                        })
                        .cloned()
                        .collect()
                })
                .collect();
            Ok((compact(out), removed))
        }

        "replace" => {
            let src = from.trim();
            if src.is_empty() {
                return Err("replace needs the URL to change".into());
            }
            let dst = normalise_url(to)?;
            if !contains(tiers, src) {
                return Err(format!("this torrent does not announce to {src:?}"));
            }
            let mut changed = false;
            let out: Vec<Vec<String>> = tiers
                .iter()
                .map(|tier| {
                    tier.iter()
                        .map(|u| {
                            if u == src {
                                changed = changed || src != dst;
                                dst.clone()
                            } else {
                                u.clone()
                            }
                        })
                        .collect()
                })
                .collect();
            Ok((compact(out), changed))
        }

        "set" => {
            // A flat list here means one tier per URL, which is what the detail
            // view produces. Callers wanting real tiers send the tiers field.
            let mut out = Vec::with_capacity(urls.len());
            for raw in urls {
                out.push(vec![normalise_url(raw)?]);
            }
            Ok((compact(out), true))
        }

        "rehost" => rehost(tiers, from, to),

        // `from` and `to` carry the two templates.
        "reformat" => reformat(tiers, from, to).map(|(out, changed, _)| (out, changed > 0)),

        other => Err(format!("unknown op {other:?}")),
    }
}

/// A host to move trackers to: a bare name and an optional port.
///
/// A name, not a URL: scheme, path and query stay the torrent's own (the
/// passkey lives in them). An IPv6 literal is not accepted; a tracker that
/// changed domain has a domain.
pub fn parse_new_host(raw: &str) -> Result<(String, Option<u16>), String> {
    let s = raw.trim();
    if s.is_empty() {
        return Err("the new host is empty".into());
    }
    if s.contains("://") || s.contains(['/', '?', '#', '@', '\\']) {
        return Err(format!(
            "{s:?}: give a host name only (and a port if it changes), not a URL"
        ));
    }
    if s.chars().any(|c| c.is_whitespace()) {
        return Err(format!("{s:?}: a host name has no spaces"));
    }
    let (host, port) = match s.split_once(':') {
        Some((h, p)) => {
            let port: u16 = p
                .parse()
                .ok()
                .filter(|n| *n != 0)
                .ok_or_else(|| format!("{s:?}: {p:?} is not a port"))?;
            (h, Some(port))
        }
        None => (s, None),
    };
    let ok = !host.is_empty()
        && !host.starts_with(['.', '-'])
        && !host.ends_with(['.', '-'])
        && host
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_'));
    if !ok {
        return Err(format!("{s:?} is not a host name"));
    }
    Ok((host.to_string(), port))
}

/// Move every announce URL of host `from` to host `to`.
///
/// `from` is matched the way the Trackers column reads a URL
/// (`tracker_host_str`), because the operator picks the row of that column.
/// Scheme, path, query (the passkey) and port are kept; `to` may carry a port,
/// which then replaces the URL's own. Tiers and order are kept. A rewritten URL
/// the torrent already announces to is dropped rather than doubled. The bool is
/// whether any URL was on `from`.
pub fn rehost(
    tiers: &[Vec<String>],
    from: &str,
    to: &str,
) -> Result<(Vec<Vec<String>>, bool), String> {
    let from = from.trim();
    if from.is_empty() {
        return Err("the host to move from is empty".into());
    }
    let (host, port) = parse_new_host(to)?;
    if host.eq_ignore_ascii_case(from) && port.is_none() {
        return Err("the new host is the same as the old one".into());
    }
    let existing: std::collections::HashSet<&str> = tiers
        .iter()
        .flat_map(|t| t.iter().map(|u| u.as_str()))
        .collect();
    let mut produced: std::collections::HashSet<String> = Default::default();
    let mut changed = false;
    let out: Vec<Vec<String>> = tiers
        .iter()
        .map(|tier| {
            let mut level = Vec::with_capacity(tier.len());
            for u in tier {
                let Some(new) = rehost_url(u, from, &host, port) else {
                    level.push(u.clone());
                    continue;
                };
                changed = true;
                // A URL the move left as it was stays. A changed one stays
                // unless it is already announced to, or two of its URLs became
                // one: then the copy goes.
                if new == *u || (!existing.contains(new.as_str()) && produced.insert(new.clone())) {
                    level.push(new);
                }
            }
            level
        })
        .collect();
    Ok((compact(out), changed))
}

/// The part of a URL after `scheme://` when its host is `from`, read as the
/// Trackers column reads it.
fn after_scheme_if_on<'a>(url: &'a str, from: &str) -> Option<(&'a str, &'a str)> {
    let (scheme, rest) = url.split_once("://")?;
    (typhon_engine::rpc::dispatch::tracker_host_str(url) == from && rest.starts_with(from))
        .then_some((scheme, rest))
}

/// Does any announce URL sit on host `from`? What `rehost` would act on.
pub fn mentions_host(tiers: &[Vec<String>], from: &str) -> bool {
    tiers
        .iter()
        .flatten()
        .any(|u| after_scheme_if_on(u, from).is_some())
}

fn rehost_url(url: &str, from: &str, host: &str, port: Option<u16>) -> Option<String> {
    let (scheme, rest) = after_scheme_if_on(url, from)?;
    let tail = &rest[from.len()..];
    let tail = match tail.strip_prefix(':') {
        Some(after) if port.is_some() => after.trim_start_matches(|c: char| c.is_ascii_digit()),
        Some(_) => return Some(format!("{scheme}://{host}{tail}")),
        None => tail,
    };
    Some(match port {
        Some(p) => format!("{scheme}://{host}:{p}{tail}"),
        None => format!("{scheme}://{host}{tail}"),
    })
}

// ---------------------------------------------------------------------------
// A format move: the announce URL changes shape, not only host.
// ---------------------------------------------------------------------------
//
// A template is a URL whose path segments and query values may be holes,
// `{name}`: `https://old.example.invalid/tracker/announce?passkey={passkey}`.
// A hole fills a whole path segment or a whole query value, never part of
// one. `{rest}` is the one catch-all: it is the LAST path segment and takes
// everything after the literal part of the path (slashes included); it is not
// allowed in a query.
//
// A URL matches the `from` template only when it matches EXACTLY: scheme, host
// (case-insensitive, port included when the template has one), every literal
// path segment, and the query parameters by name, in any order. A URL with a
// query parameter the template does not name, a fragment, a hole value that is
// empty or holds a character a URL does not keep as is, does NOT match and is
// left untouched: nothing is rewritten by guess. Percent-encoding in a value is
// kept as it is.

#[derive(Debug, Clone, PartialEq)]
enum Part {
    Lit(String),
    Hole(String),
}

#[derive(Debug, Clone)]
pub struct Template {
    scheme: String,
    host: String,
    port: Option<String>,
    path: Vec<Part>,
    has_path: bool,
    query: Vec<(String, Part)>,
}

fn is_hole_name(n: &str) -> bool {
    !n.is_empty()
        && n.chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
        && !n.starts_with(|c: char| c.is_ascii_digit())
}

fn parse_part(s: &str) -> Result<Part, String> {
    if let Some(name) = s.strip_prefix('{').and_then(|r| r.strip_suffix('}')) {
        if !is_hole_name(name) {
            return Err(format!(
                "{{{name}}} is not a hole name: lower case letters, digits and _"
            ));
        }
        return Ok(Part::Hole(name.to_string()));
    }
    if s.contains(['{', '}']) {
        return Err("a {hole} must fill a whole path segment or query value".into());
    }
    if s.chars().any(|c| c.is_whitespace() || c == '#') {
        return Err("a format has no spaces and no #".into());
    }
    Ok(Part::Lit(s.to_string()))
}

/// Characters a hole value may hold: what a URL keeps as it is. `%` must start
/// a two-digit escape, which is carried through unchanged.
fn safe_value(v: &str, allow_slash: bool) -> bool {
    if v.is_empty() {
        return false;
    }
    let b = v.as_bytes();
    let mut i = 0;
    while i < b.len() {
        let c = b[i];
        match c {
            b'%' => {
                if i + 2 >= b.len()
                    || !b[i + 1].is_ascii_hexdigit()
                    || !b[i + 2].is_ascii_hexdigit()
                {
                    return false;
                }
                i += 3;
                continue;
            }
            b'/' if allow_slash => {}
            c if c.is_ascii_alphanumeric() || matches!(c, b'-' | b'.' | b'_' | b'~' | b'+') => {}
            _ => return false,
        }
        i += 1;
    }
    true
}

/// Show the start of a hole value, never all of it: a quarter of it, at most 4
/// characters.
pub fn mask_value(v: &str) -> String {
    let n = v.chars().count();
    let keep = (n / 4).min(4);
    format!("{}…", v.chars().take(keep).collect::<String>())
}

impl Template {
    pub fn parse(raw: &str) -> Result<Template, String> {
        let s = raw.trim();
        let Some((scheme, rest)) = s.split_once("://") else {
            return Err("a format starts with http://, https:// or udp://".into());
        };
        let scheme = scheme.to_ascii_lowercase();
        if !matches!(scheme.as_str(), "http" | "https" | "udp") {
            return Err("a format starts with http://, https:// or udp://".into());
        }
        let auth_end = rest.find(['/', '?']).unwrap_or(rest.len());
        let (authority, tail) = rest.split_at(auth_end);
        let (host, port) = match authority.split_once(':') {
            Some((h, p)) => {
                if p.is_empty() || !p.chars().all(|c| c.is_ascii_digit()) {
                    return Err(format!("{p:?} is not a port"));
                }
                (h, Some(p.to_string()))
            }
            None => (authority, None),
        };
        let host_ok = !host.is_empty()
            && !host.starts_with(['.', '-'])
            && !host.ends_with(['.', '-'])
            && host
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_'));
        if !host_ok {
            return Err("the host of a format is a plain host name; it cannot hold a hole".into());
        }
        let (path_str, query_str) = match tail.split_once('?') {
            Some((p, q)) => (p, Some(q)),
            None => (tail, None),
        };
        let mut path = Vec::new();
        if !path_str.is_empty() {
            for seg in path_str[1..].split('/') {
                path.push(parse_part(seg)?);
            }
        }
        for (i, p) in path.iter().enumerate() {
            if *p == Part::Hole("rest".into()) && i + 1 != path.len() {
                return Err("{rest} can only be the last path segment".into());
            }
        }
        let mut query = Vec::new();
        if let Some(q) = query_str {
            for pair in q.split('&') {
                let Some((name, val)) = pair.split_once('=') else {
                    return Err("every query parameter of a format is name=value".into());
                };
                if name.is_empty() || name.contains(['{', '}']) {
                    return Err("a query parameter name is plain text".into());
                }
                if query.iter().any(|(n, _): &(String, Part)| n == name) {
                    return Err(format!("the query parameter {name:?} appears twice"));
                }
                let part = parse_part(val)?;
                if part == Part::Hole("rest".into()) {
                    return Err("{rest} is for the path, not for a query value".into());
                }
                query.push((name.to_string(), part));
            }
        }
        Ok(Template {
            scheme,
            host: host.to_ascii_lowercase(),
            port,
            path,
            has_path: !path_str.is_empty(),
            query,
        })
    }

    fn parts(&self) -> impl Iterator<Item = &Part> {
        self.path.iter().chain(self.query.iter().map(|(_, p)| p))
    }

    fn hole_names(&self) -> Vec<&str> {
        self.parts()
            .filter_map(|p| match p {
                Part::Hole(n) => Some(n.as_str()),
                Part::Lit(_) => None,
            })
            .collect()
    }

    fn render_with(&self, mut show: impl FnMut(&Part) -> String) -> String {
        let mut out = format!("{}://{}", self.scheme, self.host);
        if let Some(p) = &self.port {
            out.push(':');
            out.push_str(p);
        }
        if self.has_path {
            for seg in &self.path {
                out.push('/');
                out.push_str(&show(seg));
            }
        }
        for (i, (name, part)) in self.query.iter().enumerate() {
            out.push(if i == 0 { '?' } else { '&' });
            out.push_str(name);
            out.push('=');
            out.push_str(&show(part));
        }
        out
    }

    /// The template as written, holes as `{name}`.
    fn shape(&self) -> String {
        self.render_with(|p| match p {
            Part::Lit(s) => s.clone(),
            Part::Hole(n) => format!("{{{n}}}"),
        })
    }

    fn render(&self, vals: &[(String, String)]) -> Option<String> {
        let mut missing = false;
        let out = self.render_with(|p| match p {
            Part::Lit(s) => s.clone(),
            Part::Hole(n) => match vals.iter().find(|(k, _)| k == n) {
                Some((_, v)) => v.clone(),
                None => {
                    missing = true;
                    String::new()
                }
            },
        });
        (!missing).then_some(out)
    }

    /// The hole values of `url`, when it matches this template exactly.
    fn capture(&self, url: &str) -> Option<Vec<(String, String)>> {
        let (scheme, rest) = url.split_once("://")?;
        if !scheme.eq_ignore_ascii_case(&self.scheme) || url.contains('#') {
            return None;
        }
        let auth_end = rest.find(['/', '?']).unwrap_or(rest.len());
        let (authority, tail) = rest.split_at(auth_end);
        let want = match &self.port {
            Some(p) => format!("{}:{p}", self.host),
            None => self.host.clone(),
        };
        if !authority.eq_ignore_ascii_case(&want) {
            return None;
        }
        let (path_str, query_str) = match tail.split_once('?') {
            Some((p, q)) => (p, Some(q)),
            None => (tail, None),
        };
        let mut vals: Vec<(String, String)> = Vec::new();
        if self.has_path == path_str.is_empty() {
            return None;
        }
        if self.has_path {
            let segs: Vec<&str> = path_str[1..].split('/').collect();
            let catch_all = matches!(self.path.last(), Some(Part::Hole(n)) if n == "rest");
            let fixed = if catch_all {
                self.path.len() - 1
            } else {
                self.path.len()
            };
            if segs.len() < self.path.len() || (!catch_all && segs.len() != self.path.len()) {
                return None;
            }
            for (part, seg) in self.path[..fixed].iter().zip(&segs) {
                match part {
                    Part::Lit(l) if l == seg => {}
                    Part::Lit(_) => return None,
                    Part::Hole(n) => {
                        if !safe_value(seg, false) {
                            return None;
                        }
                        vals.push((n.clone(), (*seg).to_string()));
                    }
                }
            }
            if catch_all {
                let rest = segs[fixed..].join("/");
                if !safe_value(&rest, true) {
                    return None;
                }
                vals.push(("rest".into(), rest));
            }
        }
        let pairs: Vec<(&str, &str)> = match query_str {
            None => Vec::new(),
            Some(q) => {
                let mut v = Vec::new();
                for pair in q.split('&') {
                    v.push(pair.split_once('=')?);
                }
                v
            }
        };
        // A parameter the template does not name makes the URL a different
        // kind of URL than the one described: not matched.
        if pairs.len() != self.query.len() {
            return None;
        }
        for (name, part) in &self.query {
            let mut found = pairs.iter().filter(|(n, _)| n == name);
            let (_, val) = found.next()?;
            if found.next().is_some() {
                return None;
            }
            match part {
                Part::Lit(l) if l == val => {}
                Part::Lit(_) => return None,
                Part::Hole(n) => {
                    if !safe_value(val, false) {
                        return None;
                    }
                    vals.push((n.clone(), (*val).to_string()));
                }
            }
        }
        Some(vals)
    }
}

/// A validated pair of templates: what a format move applies.
#[derive(Debug, Clone)]
pub struct Format {
    from: Template,
    to: Template,
}

impl Format {
    pub fn parse(from: &str, to: &str) -> Result<Format, String> {
        let from = Template::parse(from.trim()).map_err(|e| format!("old format: {e}"))?;
        let to = Template::parse(to.trim()).map_err(|e| format!("new format: {e}"))?;
        let known = from.hole_names();
        for (i, n) in known.iter().enumerate() {
            if known[..i].contains(n) {
                return Err(format!("old format: {{{n}}} appears twice"));
            }
        }
        for n in to.hole_names() {
            if !known.contains(&n) {
                return Err(format!("new format: {{{n}}} is not in the old format"));
            }
        }
        if from.shape() == to.shape() {
            return Err("the new format is the same as the old one".into());
        }
        // The new format has to give a tracker URL for any value.
        let dummy: Vec<(String, String)> = known
            .iter()
            .map(|n| (n.to_string(), "x".to_string()))
            .collect();
        let sample = to.render(&dummy).expect("every hole is known");
        normalise_url(&sample)
            .map_err(|_| "new format: it does not give a tracker URL".to_string())?;
        Ok(Format { from, to })
    }

    /// The old and the new host, without port.
    pub fn old_host(&self) -> &str {
        &self.from.host
    }
    pub fn new_host(&self) -> &str {
        &self.to.host
    }

    /// Is this URL on the old format's host? What "not matching" is counted
    /// against: a URL elsewhere is not this move's business.
    pub fn on_host(&self, url: &str) -> bool {
        let Some((_, rest)) = url.split_once("://") else {
            return false;
        };
        let end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
        let host = rest[..end].split(':').next().unwrap_or("");
        host.eq_ignore_ascii_case(&self.from.host)
    }

    /// The URL in the new format, when it matches the old one.
    pub fn convert(&self, url: &str) -> Option<String> {
        let vals = self.from.capture(url)?;
        let out = self.to.render(&vals)?;
        normalise_url(&out).ok()
    }

    /// The same, with every hole value masked, and the old URL shown in the
    /// old format too: what a preview may print.
    pub fn masked(&self, url: &str) -> Option<(String, String)> {
        let vals: Vec<(String, String)> = self
            .from
            .capture(url)?
            .into_iter()
            .map(|(k, v)| (k, mask_value(&v)))
            .collect();
        Some((self.from.render(&vals)?, self.to.render(&vals)?))
    }

    /// For one torrent: how many of its URLs a move would change, and how many
    /// are on the old host without matching.
    pub fn scan(&self, tiers: &[Vec<String>]) -> (usize, usize) {
        let (mut changes, mut unmatched) = (0, 0);
        for u in tiers.iter().flatten() {
            match self.convert(u) {
                Some(new) if new != *u => changes += 1,
                Some(_) => {}
                None if self.on_host(u) => unmatched += 1,
                None => {}
            }
        }
        (changes, unmatched)
    }
}

/// Rewrite every announce URL matching `from_tpl` into `to_tpl`.
///
/// Tiers, their order and the dedupe rule are `rehost`'s: a rewritten URL the
/// torrent already has, or that two of its URLs became, is dropped rather than
/// doubled. Returns the tiers, how many URLs changed, and how many URLs on the
/// old host did not match the template (left as they were).
pub fn reformat(
    tiers: &[Vec<String>],
    from_tpl: &str,
    to_tpl: &str,
) -> Result<(Vec<Vec<String>>, usize, usize), String> {
    Ok(reformat_with(tiers, &Format::parse(from_tpl, to_tpl)?))
}

pub fn reformat_with(tiers: &[Vec<String>], fmt: &Format) -> (Vec<Vec<String>>, usize, usize) {
    let existing: std::collections::HashSet<&str> = tiers
        .iter()
        .flat_map(|t| t.iter().map(|u| u.as_str()))
        .collect();
    let mut produced: std::collections::HashSet<String> = Default::default();
    let (mut changed, mut unmatched) = (0, 0);
    let out: Vec<Vec<String>> = tiers
        .iter()
        .map(|tier| {
            let mut level = Vec::with_capacity(tier.len());
            for u in tier {
                let Some(new) = fmt.convert(u) else {
                    unmatched += usize::from(fmt.on_host(u));
                    level.push(u.clone());
                    continue;
                };
                if new == *u {
                    level.push(new);
                    continue;
                }
                changed += 1;
                if !existing.contains(new.as_str()) && produced.insert(new.clone()) {
                    level.push(new);
                }
            }
            level
        })
        .collect();
    (compact(out), changed, unmatched)
}

/// A template for the URLs of `url`'s host, with the passkey as `{passkey}`,
/// when it can be told apart: exactly one query value or path segment that
/// looks like a key (16 or more letters, digits, `-` or `_`). More than one,
/// or none, gives `None`: the operator types it. The key itself is never in
/// the answer.
pub fn guess_format(url: &str) -> Option<String> {
    let url = url.trim();
    let (scheme, rest) = url.split_once("://")?;
    if url.contains(['{', '}', '#']) {
        return None;
    }
    let keyish = |v: &str| {
        v.len() >= 16
            && v.chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_'))
    };
    let end = rest.find(['/', '?']).unwrap_or(rest.len());
    let (authority, tail) = rest.split_at(end);
    let (path, query) = match tail.split_once('?') {
        Some((p, q)) => (p, Some(q)),
        None => (tail, None),
    };
    let mut hits = 0;
    let mut path_out = String::new();
    if !path.is_empty() {
        for seg in path[1..].split('/') {
            path_out.push('/');
            if keyish(seg) {
                hits += 1;
                path_out.push_str("{passkey}");
            } else {
                path_out.push_str(seg);
            }
        }
    }
    let mut query_out = String::new();
    if let Some(q) = query {
        for (i, pair) in q.split('&').enumerate() {
            let (name, val) = pair.split_once('=')?;
            query_out.push(if i == 0 { '?' } else { '&' });
            query_out.push_str(name);
            query_out.push('=');
            if keyish(val) {
                hits += 1;
                query_out.push_str("{passkey}");
            } else {
                query_out.push_str(val);
            }
        }
    }
    if hits != 1 {
        return None;
    }
    let out = format!("{scheme}://{authority}{path_out}{query_out}");
    Template::parse(&out).ok().map(|_| out)
}

/// Validate an explicit tier structure, as sent by the editor.
pub fn from_tiers(tiers: &[Vec<String>]) -> Result<Vec<Vec<String>>, String> {
    let mut out = Vec::with_capacity(tiers.len());
    for tier in tiers {
        let mut level = Vec::with_capacity(tier.len());
        for raw in tier {
            level.push(normalise_url(raw)?);
        }
        out.push(level);
    }
    Ok(compact(out))
}

pub fn same(a: &[Vec<String>], b: &[Vec<String>]) -> bool {
    a == b
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tiers(v: &[&[&str]]) -> Vec<Vec<String>> {
        v.iter()
            .map(|t| t.iter().map(|s| s.to_string()).collect())
            .collect()
    }

    fn urls(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn adding_a_tracker_already_present_changes_nothing() {
        let current = tiers(&[&["https://a/announce"]]);
        let (out, changed) =
            apply(&current, "add", &urls(&["https://a/announce"]), "", "").unwrap();
        assert!(
            !changed,
            "a duplicate would make the torrent announce twice"
        );
        assert_eq!(out, current);
    }

    #[test]
    fn adding_puts_the_new_tracker_in_its_own_tier() {
        let current = tiers(&[&["https://a/announce"]]);
        let (out, changed) =
            apply(&current, "add", &urls(&["https://b/announce"]), "", "").unwrap();
        assert!(changed);
        assert_eq!(
            out,
            tiers(&[&["https://a/announce"], &["https://b/announce"]])
        );
    }

    // An emptied tier must disappear, not linger: it is a level of the fallback
    // chain that can never answer.
    #[test]
    fn removing_the_last_url_of_a_tier_drops_the_tier() {
        let current = tiers(&[&["https://a/announce"], &["https://b/announce"]]);
        let (out, changed) =
            apply(&current, "remove", &urls(&["https://a/announce"]), "", "").unwrap();
        assert!(changed);
        assert_eq!(out, tiers(&[&["https://b/announce"]]));
    }

    #[test]
    fn removing_needs_something_to_remove() {
        let current = tiers(&[&["https://a/announce"]]);
        assert!(apply(&current, "remove", &[], "", "").is_err());
    }

    #[test]
    fn replacing_refuses_a_url_the_torrent_does_not_have() {
        let current = tiers(&[&["https://a/announce"]]);
        let err = apply(
            &current,
            "replace",
            &[],
            "https://nope/announce",
            "https://c/announce",
        )
        .unwrap_err();
        assert!(err.contains("does not announce"), "{err}");
    }

    #[test]
    fn replacing_keeps_the_tier_it_was_in() {
        let current = tiers(&[&["https://a/announce", "https://b/announce"]]);
        let (out, changed) = apply(
            &current,
            "replace",
            &[],
            "https://a/announce",
            "https://c/announce",
        )
        .unwrap();
        assert!(changed);
        assert_eq!(out, tiers(&[&["https://c/announce", "https://b/announce"]]));
    }

    #[test]
    fn only_tracker_schemes_are_accepted() {
        assert!(normalise_url("http://t/announce").is_ok());
        assert!(normalise_url("https://t/announce").is_ok());
        assert!(normalise_url("ftp://t/announce").is_err());
        assert!(normalise_url("not a url").is_err());
        assert!(normalise_url("https://").is_err(), "no host");
        assert!(normalise_url("   ").is_err());
    }

    /// A UDP tracker is accepted -- with its port, which it cannot do
    /// without: there is no default to fall back on.
    #[test]
    fn a_udp_tracker_is_accepted_with_a_port_and_refused_without() {
        assert_eq!(
            normalise_url("udp://tracker.example:6969/announce").unwrap(),
            "udp://tracker.example:6969/announce"
        );
        let err = normalise_url("udp://tracker.example/announce").expect_err("no port");
        assert!(err.contains("port"), "{err}");
    }

    #[test]
    fn an_unknown_op_is_refused_rather_than_ignored() {
        let err = apply(&tiers(&[&["https://a/announce"]]), "nope", &[], "", "").unwrap_err();
        assert!(err.contains("nope"), "{err}");
    }

    fn moved(t: &[&[&str]], from: &str, to: &str) -> (Vec<Vec<String>>, bool) {
        rehost(&tiers(t), from, to).unwrap()
    }

    #[test]
    fn rehost_keeps_the_path_and_the_passkey_query() {
        let (out, changed) = moved(
            &[&["https://tracker.example/PASSKEY/announce?k=v&x=1"]],
            "tracker.example",
            "new.example",
        );
        assert!(changed);
        assert_eq!(
            out,
            tiers(&[&["https://new.example/PASSKEY/announce?k=v&x=1"]])
        );
    }

    #[test]
    fn rehost_keeps_the_port_unless_the_new_host_names_one() {
        let (out, _) = moved(
            &[&["http://tracker.example:8080/a"]],
            "tracker.example",
            "new.example",
        );
        assert_eq!(out, tiers(&[&["http://new.example:8080/a"]]));
        let (out, _) = moved(
            &[&["http://tracker.example:8080/a", "http://tracker.example/b"]],
            "tracker.example",
            "new.example:443",
        );
        assert_eq!(
            out,
            tiers(&[&["http://new.example:443/a", "http://new.example:443/b"]])
        );
    }

    // The column reads the host as written, so a differently cased host is
    // another row; the new host is kept as typed.
    #[test]
    fn rehost_matches_the_host_as_the_column_reads_it() {
        let (out, changed) = moved(
            &[&["https://Tracker.Example/a"]],
            "tracker.example",
            "new.example",
        );
        assert!(!changed);
        assert_eq!(out, tiers(&[&["https://Tracker.Example/a"]]));
        let (out, changed) = moved(
            &[&["https://Tracker.Example/a"]],
            "Tracker.Example",
            "New.Example",
        );
        assert!(changed);
        assert_eq!(out, tiers(&[&["https://New.Example/a"]]));
    }

    #[test]
    fn rehost_keeps_tiers_and_order_and_leaves_other_hosts() {
        let (out, changed) = moved(
            &[
                &["https://other.example/x", "https://tracker.example/a"],
                &["https://tracker.example/b"],
                &["https://third.example/c"],
            ],
            "tracker.example",
            "new.example",
        );
        assert!(changed);
        assert_eq!(
            out,
            tiers(&[
                &["https://other.example/x", "https://new.example/a"],
                &["https://new.example/b"],
                &["https://third.example/c"],
            ])
        );
    }

    #[test]
    fn rehost_drops_a_url_the_torrent_already_has() {
        let (out, changed) = moved(
            &[
                &["https://tracker.example/a"],
                &["https://new.example/a", "https://tracker.example/b"],
            ],
            "tracker.example",
            "new.example",
        );
        assert!(changed);
        assert_eq!(
            out,
            tiers(&[&["https://new.example/a", "https://new.example/b"]])
        );
        // Two URLs that become one.
        let (out, _) = moved(
            &[&["http://tracker.example:80/a", "http://tracker.example:81/a"]],
            "tracker.example",
            "new.example:80",
        );
        assert_eq!(out, tiers(&[&["http://new.example:80/a"]]));
    }

    #[test]
    fn rehost_moves_a_udp_tracker_and_keeps_its_port() {
        let (out, changed) = moved(
            &[&["udp://tracker.example:6969/announce"]],
            "tracker.example",
            "new.example",
        );
        assert!(changed);
        assert_eq!(out, tiers(&[&["udp://new.example:6969/announce"]]));
    }

    #[test]
    fn rehost_with_nothing_on_the_old_host_changes_nothing() {
        let current = tiers(&[&["https://other.example/a"]]);
        let (out, changed) = rehost(&current, "tracker.example", "new.example").unwrap();
        assert!(!changed);
        assert_eq!(out, current);
    }

    #[test]
    fn rehost_refuses_a_new_host_that_is_not_a_bare_host() {
        let current = tiers(&[&["https://tracker.example/a"]]);
        for bad in [
            "",
            "  ",
            "https://new.example",
            "new.example/announce",
            "new.example?x=1",
            "new example",
            "user@new.example",
            "new.example:abc",
            "new.example:0",
            "new.example:70000",
            "-new.example",
            "tracker.example",
            "TRACKER.example",
        ] {
            assert!(
                rehost(&current, "tracker.example", bad).is_err(),
                "{bad:?} should be refused"
            );
        }
        assert!(rehost(&current, "", "new.example").is_err());
        // Same name, new port: that is a change.
        assert!(rehost(&current, "tracker.example", "tracker.example:8443").is_ok());
    }

    const KEY: &str = "0123abcd0123abcd0123abcd0123abcd0123abcd0123abcd0123abcd0123abcd";
    const KEY2: &str = "fedc9876fedc9876fedc9876fedc9876fedc9876fedc9876fedc9876fedc9876";
    const OLD_Q: &str = "https://old.example.invalid/tracker/announce?passkey={passkey}";
    const NEW_P: &str = "https://new.example.invalid/{passkey}/announce";

    fn one(url: &str) -> Vec<Vec<String>> {
        vec![vec![url.to_string()]]
    }

    #[test]
    fn a_passkey_moves_from_the_query_to_a_path_segment() {
        let cur = one(&format!(
            "https://old.example.invalid/tracker/announce?passkey={KEY}"
        ));
        let (out, changed, unmatched) = reformat(&cur, OLD_Q, NEW_P).unwrap();
        assert_eq!(
            out,
            one(&format!("https://new.example.invalid/{KEY}/announce"))
        );
        assert_eq!((changed, unmatched), (1, 0));
    }

    #[test]
    fn a_passkey_moves_from_a_path_segment_to_the_query() {
        let cur = one(&format!("https://new.example.invalid/{KEY}/announce"));
        let (out, changed, _) = reformat(&cur, NEW_P, OLD_Q).unwrap();
        assert_eq!(
            out,
            one(&format!(
                "https://old.example.invalid/tracker/announce?passkey={KEY}"
            ))
        );
        assert_eq!(changed, 1);
    }

    #[test]
    fn a_template_can_change_only_the_host() {
        let cur = one(&format!(
            "https://old.example.invalid/tracker/announce?passkey={KEY}"
        ));
        let (out, changed, _) = reformat(
            &cur,
            OLD_Q,
            "https://new.example.invalid/tracker/announce?passkey={passkey}",
        )
        .unwrap();
        assert_eq!(
            out,
            one(&format!(
                "https://new.example.invalid/tracker/announce?passkey={KEY}"
            ))
        );
        assert_eq!(changed, 1);
    }

    #[test]
    fn the_host_is_matched_without_regard_to_case_and_query_order() {
        let cur = one(&format!(
            "https://OLD.Example.invalid/tracker/announce?uid=7&passkey={KEY}"
        ));
        let (out, changed, _) = reformat(
            &cur,
            "https://old.example.invalid/tracker/announce?passkey={passkey}&uid={uid}",
            "https://new.example.invalid/{uid}/{passkey}",
        )
        .unwrap();
        assert_eq!(out, one(&format!("https://new.example.invalid/7/{KEY}")));
        assert_eq!(changed, 1);
    }

    #[test]
    fn a_url_with_a_query_parameter_the_template_does_not_name_is_left_alone() {
        let url = format!("https://old.example.invalid/tracker/announce?passkey={KEY}&extra=1");
        let (out, changed, unmatched) = reformat(&one(&url), OLD_Q, NEW_P).unwrap();
        assert_eq!(out, one(&url));
        assert_eq!((changed, unmatched), (0, 1));
    }

    #[test]
    fn urls_that_do_not_fit_are_not_matched() {
        for url in [
            // another path
            format!("https://old.example.invalid/other/announce?passkey={KEY}"),
            // a path segment more
            format!("https://old.example.invalid/tracker/announce/x?passkey={KEY}"),
            // an empty value
            "https://old.example.invalid/tracker/announce?passkey=".to_string(),
            // a value that is not URL-safe
            "https://old.example.invalid/tracker/announce?passkey=a%zzb".to_string(),
            // a repeated parameter
            format!("https://old.example.invalid/tracker/announce?passkey={KEY}&passkey={KEY}"),
            // a fragment
            format!("https://old.example.invalid/tracker/announce?passkey={KEY}#f"),
            // no query at all
            "https://old.example.invalid/tracker/announce".to_string(),
        ] {
            let (out, changed, unmatched) = reformat(&one(&url), OLD_Q, NEW_P).unwrap();
            assert_eq!(out, one(&url), "{url}");
            assert_eq!((changed, unmatched), (0, 1), "{url}");
        }
        // Another host is not this move's business: not even counted.
        let other = one("https://elsewhere.example.invalid/tracker/announce?passkey=abcd");
        assert_eq!(reformat(&other, OLD_Q, NEW_P).unwrap().2, 0);
    }

    #[test]
    fn percent_encoding_in_a_value_is_kept_as_it_is() {
        let cur = one("https://old.example.invalid/tracker/announce?passkey=ab%2Fcd%41");
        let (out, _, _) = reformat(&cur, OLD_Q, NEW_P).unwrap();
        assert_eq!(out, one("https://new.example.invalid/ab%2Fcd%41/announce"));
    }

    #[test]
    fn templates_that_cannot_work_are_refused() {
        // A hole in the new format missing from the old one.
        let e = reformat(&[], OLD_Q, "https://new.example.invalid/{uid}/announce").unwrap_err();
        assert!(e.contains("uid"), "{e}");
        // The same format twice.
        assert!(reformat(&[], OLD_Q, OLD_Q).is_err());
        // Not a tracker scheme.
        assert!(reformat(&[], OLD_Q, "ftp://new.example.invalid/{passkey}").is_err());
        assert!(reformat(&[], OLD_Q, "new.example.invalid/{passkey}").is_err());
        // A hole inside a segment, a hole in the host, an unnamed query value.
        assert!(reformat(&[], OLD_Q, "https://new.example.invalid/a{passkey}b").is_err());
        assert!(reformat(&[], OLD_Q, "https://{passkey}.example.invalid/x").is_err());
        assert!(reformat(&[], "https://old.example.invalid/x?{passkey}", NEW_P).is_err());
        // A hole twice in the old format.
        assert!(reformat(
            &[],
            "https://old.example.invalid/{passkey}/{passkey}",
            NEW_P
        )
        .is_err());
        // A UDP tracker needs a port.
        assert!(reformat(&[], OLD_Q, "udp://new.example.invalid/{passkey}").is_err());
        // {rest} is the last path segment, and only there.
        assert!(reformat(&[], "https://old.example.invalid/{rest}/x", NEW_P).is_err());
        assert!(reformat(&[], "https://old.example.invalid/x?k={rest}", NEW_P).is_err());
    }

    #[test]
    fn a_literal_only_old_format_is_allowed_when_it_differs() {
        let cur = one("https://old.example.invalid/announce");
        let (out, changed, _) = reformat(
            &cur,
            "https://old.example.invalid/announce",
            "https://new.example.invalid/announce",
        )
        .unwrap();
        assert_eq!(out, one("https://new.example.invalid/announce"));
        assert_eq!(changed, 1);
    }

    #[test]
    fn the_rest_hole_carries_the_remaining_path() {
        let cur = one("https://old.example.invalid/t/a/b/c");
        let (out, changed, _) = reformat(
            &cur,
            "https://old.example.invalid/t/{rest}",
            "https://new.example.invalid/u/{rest}",
        )
        .unwrap();
        assert_eq!(out, one("https://new.example.invalid/u/a/b/c"));
        assert_eq!(changed, 1);
        // Nothing after the literal part: no value, no match.
        let bare = one("https://old.example.invalid/t");
        assert_eq!(
            reformat(
                &bare,
                "https://old.example.invalid/t/{rest}",
                "https://new.example.invalid/u/{rest}"
            )
            .unwrap()
            .1,
            0
        );
    }

    #[test]
    fn tiers_and_order_are_kept_and_only_matching_urls_change() {
        let a = format!("https://old.example.invalid/tracker/announce?passkey={KEY}");
        let b = format!("https://old.example.invalid/tracker/announce?passkey={KEY2}");
        let cur = vec![
            vec![a, "https://keep.example.invalid/announce".to_string()],
            vec![b],
        ];
        let (out, changed, unmatched) = reformat(&cur, OLD_Q, NEW_P).unwrap();
        assert_eq!(
            out,
            vec![
                vec![
                    format!("https://new.example.invalid/{KEY}/announce"),
                    "https://keep.example.invalid/announce".to_string()
                ],
                vec![format!("https://new.example.invalid/{KEY2}/announce")],
            ]
        );
        assert_eq!((changed, unmatched), (2, 0));
    }

    #[test]
    fn a_url_that_becomes_a_duplicate_is_dropped() {
        let old = format!("https://old.example.invalid/tracker/announce?passkey={KEY}");
        let already = format!("https://new.example.invalid/{KEY}/announce");
        // The rewritten URL is already in another tier.
        let cur = vec![vec![old.clone()], vec![already.clone()]];
        let (out, changed, _) = reformat(&cur, OLD_Q, NEW_P).unwrap();
        assert_eq!(out, vec![vec![already.clone()]]);
        assert_eq!(changed, 1);
        // Two URLs that become one.
        let twice = vec![vec![old.clone(), old]];
        let (out, _, _) = reformat(&twice, OLD_Q, NEW_P).unwrap();
        assert_eq!(out, one(&already));
    }

    #[test]
    fn a_udp_tracker_can_be_reformatted() {
        let cur = one(&format!("udp://old.example.invalid:6969/{KEY}/announce"));
        let (out, changed, _) = reformat(
            &cur,
            "udp://old.example.invalid:6969/{passkey}/announce",
            "udp://new.example.invalid:7070/announce/{passkey}",
        )
        .unwrap();
        assert_eq!(
            out,
            one(&format!("udp://new.example.invalid:7070/announce/{KEY}"))
        );
        assert_eq!(changed, 1);
        // The port is part of the host the template names.
        let other_port = one(&format!("udp://old.example.invalid:1111/{KEY}/announce"));
        assert_eq!(
            reformat(
                &other_port,
                "udp://old.example.invalid:6969/{passkey}/announce",
                "udp://new.example.invalid:7070/announce/{passkey}"
            )
            .unwrap()
            .1,
            0
        );
    }

    #[test]
    fn a_preview_never_shows_a_whole_value() {
        let fmt = Format::parse(OLD_Q, NEW_P).unwrap();
        let url = format!("https://old.example.invalid/tracker/announce?passkey={KEY}");
        let (before, after) = fmt.masked(&url).unwrap();
        assert_eq!(
            before,
            "https://old.example.invalid/tracker/announce?passkey=0123…"
        );
        assert_eq!(after, "https://new.example.invalid/0123…/announce");
        for shown in [&before, &after] {
            assert!(!shown.contains(&KEY[..8]), "{shown}");
        }
        // A short value is cut further: never all of it.
        assert_eq!(mask_value("abcdefgh"), "ab…");
        assert_eq!(mask_value("abc"), "…");
        assert!(fmt.masked("https://old.example.invalid/nope").is_none());
    }

    #[test]
    fn the_format_of_a_row_is_guessed_without_its_key() {
        let g = guess_format(&format!(
            "https://old.example.invalid/tracker/announce?passkey={KEY}"
        ))
        .unwrap();
        assert_eq!(g, OLD_Q);
        let g = guess_format(&format!("https://new.example.invalid/{KEY}/announce")).unwrap();
        assert_eq!(g, NEW_P);
        // No key to find, or two: the operator types it.
        assert!(guess_format("https://old.example.invalid/announce").is_none());
        assert!(guess_format(&format!(
            "https://old.example.invalid/announce?a={KEY}&b={KEY2}"
        ))
        .is_none());
    }
}
