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
    if !matches!(scheme, "http" | "https" | "udp") {
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
        .map(|tier| tier.into_iter().filter(|u| !u.is_empty()).collect::<Vec<_>>())
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
            let wanted: std::collections::HashSet<&str> =
                urls.iter().map(|u| u.trim()).filter(|u| !u.is_empty()).collect();
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

        other => Err(format!("unknown op {other:?}")),
    }
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
        let (out, changed) = apply(&current, "add", &urls(&["https://a/announce"]), "", "").unwrap();
        assert!(!changed, "a duplicate would make the torrent announce twice");
        assert_eq!(out, current);
    }

    #[test]
    fn adding_puts_the_new_tracker_in_its_own_tier() {
        let current = tiers(&[&["https://a/announce"]]);
        let (out, changed) = apply(&current, "add", &urls(&["udp://b:6969"]), "", "").unwrap();
        assert!(changed);
        assert_eq!(out, tiers(&[&["https://a/announce"], &["udp://b:6969"]]));
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
        let err = apply(&current, "replace", &[], "https://nope/announce", "https://c/announce")
            .unwrap_err();
        assert!(err.contains("does not announce"), "{err}");
    }

    #[test]
    fn replacing_keeps_the_tier_it_was_in() {
        let current = tiers(&[&["https://a/announce", "https://b/announce"]]);
        let (out, changed) =
            apply(&current, "replace", &[], "https://a/announce", "https://c/announce").unwrap();
        assert!(changed);
        assert_eq!(out, tiers(&[&["https://c/announce", "https://b/announce"]]));
    }

    #[test]
    fn only_tracker_schemes_are_accepted() {
        assert!(normalise_url("https://t/announce").is_ok());
        assert!(normalise_url("udp://t:6969").is_ok());
        assert!(normalise_url("ftp://t/announce").is_err());
        assert!(normalise_url("not a url").is_err());
        assert!(normalise_url("https://").is_err(), "no host");
        assert!(normalise_url("   ").is_err());
    }

    #[test]
    fn an_unknown_op_is_refused_rather_than_ignored() {
        let err = apply(&tiers(&[&["https://a/announce"]]), "nope", &[], "", "").unwrap_err();
        assert!(err.contains("nope"), "{err}");
    }
}
