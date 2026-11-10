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
                if new == *u {
                    level.push(new);
                } else if !existing.contains(new.as_str()) && produced.insert(new.clone()) {
                    level.push(new);
                }
                // Else it is already announced to, or two of its URLs became
                // one: the copy goes.
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
        assert_eq!(out, tiers(&[&["https://new.example/a", "https://new.example/b"]]));
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
}
