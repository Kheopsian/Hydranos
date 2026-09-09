//! Surgical edits to the configuration file.
//!
//! The daemon never re-serialises default.toml. It edits the text in place,
//! and that is a deliberate choice worth keeping: the file is the operator's.
//! It carries section banners, explanations, and values commented out for
//! later. Round-tripping it through a struct would silently delete all of that
//! the first time someone flipped a switch in the UI.
//!
//! Ported from internal/config/tomledit.go, semantics for semantics, including
//! the parts that look like caution and are: a missing key is an ERROR rather
//! than an append, because appending to the wrong table is how a config quietly
//! stops meaning what it says.

/// Index of the `#` that opens an inline comment, ignoring `#` inside strings.
fn find_inline_comment(s: &str) -> Option<usize> {
    let bytes = s.as_bytes();
    let mut in_string = false;
    let mut quote = b'"';
    for (i, &c) in bytes.iter().enumerate() {
        if in_string {
            if c == quote {
                in_string = false;
            }
            continue;
        }
        if c == b'"' || c == b'\'' {
            in_string = true;
            quote = c;
            continue;
        }
        if c == b'#' {
            return Some(i);
        }
    }
    None
}

fn is_table_header(line: &str) -> bool {
    let t = line.trim();
    t.starts_with('[') && t.ends_with(']')
}

/// The table a header declares: `[a.b]` -> `a.b`.
///
/// Empty for an array-of-tables header (`[[x]]`), which therefore never matches
/// a section by name -- its keys are not ours to touch.
fn section_name(line: &str) -> String {
    let t = line.trim();
    if is_table_header(line) && !t.starts_with("[[") {
        t[1..t.len() - 1].trim().to_string()
    } else {
        String::new()
    }
}

/// Render a string as a quoted TOML key.
///
/// Tracker hosts contain dots, and a bare dot is the table-path separator, so
/// an unquoted host would become a nested table instead of one key.
pub fn quote_toml_key(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\t' => out.push_str("\\t"),
            '\r' => out.push_str("\\r"),
            _ => out.push(c),
        }
    }
    out.push('"');
    out
}

/// Replace one key's value inside `[section]`, keeping everything else byte for
/// byte -- comments, blank lines, ordering, indentation, and any inline comment
/// on the edited line.
///
/// `section` empty targets the top-level table. Returns Err when the key is not
/// there: callers that want to create it go through `set_toml_table`.
pub fn set_toml_value(doc: &str, section: &str, key: &str, value: &str) -> Result<String, String> {
    let mut lines: Vec<String> = doc.split('\n').map(str::to_string).collect();
    let mut current = String::new();

    for i in 0..lines.len() {
        let line = lines[i].clone();
        let trimmed = line.trim();
        if trimmed.starts_with('[') && trimmed.ends_with(']') && !trimmed.starts_with("[[") {
            current = trimmed[1..trimmed.len() - 1].trim().to_string();
            continue;
        }
        if current != section || trimmed.starts_with('#') {
            continue;
        }
        let Some(eq) = line.find('=') else { continue };
        if line[..eq].trim() != key {
            continue;
        }

        let indent: String = line.chars().take_while(|c| *c == ' ' || *c == '\t').collect();
        let after = &line[eq + 1..];
        let comment = match find_inline_comment(after) {
            Some(ci) => format!("  {}", after[ci..].trim()),
            None => String::new(),
        };
        lines[i] = format!("{indent}{key} = {value}{comment}");
        return Ok(lines.join("\n"));
    }
    Err(format!("toml: key {key:?} not found in section {section:?}"))
}

/// Set several keys in `[section]`, creating the keys and the section if needed.
pub fn set_toml_table(doc: &str, section: &str, kv: &[(String, String)]) -> Result<String, String> {
    if section.trim().is_empty() {
        return Err("toml: empty section".into());
    }

    let mut out = doc.to_string();
    let mut missing: Vec<&(String, String)> = Vec::new();
    for pair in kv {
        match set_toml_value(&out, section, &pair.0, &pair.1) {
            Ok(next) => out = next,
            Err(_) => missing.push(pair),
        }
    }
    if missing.is_empty() {
        return Ok(out);
    }

    let added: Vec<String> = missing
        .iter()
        .map(|p| format!("{} = {}", p.0, p.1))
        .collect();

    let lines: Vec<&str> = out.split('\n').collect();
    for (i, line) in lines.iter().enumerate() {
        if section_name(line) != section {
            continue;
        }
        // The section exists but these keys do not: insert under its header.
        let mut result: Vec<String> = lines[..=i].iter().map(|s| s.to_string()).collect();
        result.extend(added.iter().cloned());
        result.extend(lines[i + 1..].iter().map(|s| s.to_string()));
        return Ok(result.join("\n"));
    }

    // A brand new table goes at the END of the file: the only position where we
    // are sure we are not landing inside somebody else's table.
    Ok(format!(
        "{}\n\n[{}]\n{}\n",
        out.trim_end_matches('\n'),
        section,
        added.join("\n")
    ))
}

/// Remove `key` from `[section]`. Idempotent.
pub fn delete_toml_key(doc: &str, section: &str, key: &str) -> String {
    let mut out: Vec<String> = Vec::new();
    let mut current = String::new();
    for line in doc.split('\n') {
        if is_table_header(line) {
            current = section_name(line);
            out.push(line.to_string());
            continue;
        }
        if current == section && !line.trim().starts_with('#') {
            if let Some(eq) = line.find('=') {
                if line[..eq].trim() == key {
                    continue;
                }
            }
        }
        out.push(line.to_string());
    }
    out.join("\n")
}

/// Remove a `[section]` header and every line it owns, up to the next header.
/// Idempotent, like `delete_toml_key`.
pub fn delete_toml_table(doc: &str, section: &str) -> String {
    if section.trim().is_empty() {
        return doc.to_string();
    }
    let mut out: Vec<&str> = Vec::new();
    let mut dropping = false;
    for line in doc.split('\n') {
        if is_table_header(line) {
            dropping = section_name(line) == section;
        }
        if dropping {
            continue;
        }
        out.push(line);
    }
    format!("{}\n", out.join("\n").trim_end_matches('\n'))
}

/// Drop a `[section]` header whose body holds nothing but blank lines.
///
/// An empty table and an absent one mean the same thing to the decoder, so the
/// file should not keep a header suggesting otherwise. A body with COMMENTS is
/// left alone: those are the operator's words, not ours.
pub fn prune_empty_table(doc: &str, section: &str) -> String {
    if section.trim().is_empty() {
        return doc.to_string();
    }
    let lines: Vec<&str> = doc.split('\n').collect();
    let Some(start) = lines.iter().position(|l| section_name(l) == section) else {
        return doc.to_string();
    };

    let mut end = lines.len();
    for (i, line) in lines.iter().enumerate().skip(start + 1) {
        if is_table_header(line) {
            end = i;
            break;
        }
        if !line.trim().is_empty() {
            return doc.to_string(); // a key or a comment still lives here
        }
    }

    let mut result: Vec<&str> = lines[..start].to_vec();
    result.extend_from_slice(&lines[end..]);
    format!("{}\n", result.join("\n").trim_end_matches('\n'))
}

/// Append one `[[agent]]` block, which is how a node declares an extra engine.
///
/// Appended at the END of the file rather than inserted: an array-of-tables
/// entry owns every key that follows it until the next header, so putting one
/// in the middle would silently adopt the keys of whatever section came after.
///
/// The `[agent.session]` sub-table is a SPARSE override of the role profile,
/// which is why only the keys the operator actually set are written: everything
/// shared keeps coming from `[race]` or `[hoard]`, where it is changed once.
pub fn append_agent_block(doc: &str, id: &str, role: &str, session: &[(String, String)]) -> String {
    // Written as `[[engine]]`, which is what the block is. `[[agent]]` is still
    // READ, so an existing file and a rollback both keep working, but nothing
    // new is written under the old name.
    let mut out = String::from(doc.trim_end_matches('\n'));
    out.push_str("\n\n[[engine]]\n");
    out.push_str(&format!("name = {}\n", quote_toml_key(id)));
    out.push_str(&format!("role = {}\n", quote_toml_key(role)));
    out.push_str(&format!("engine_id = {}\n", quote_toml_key(id)));
    if !session.is_empty() {
        // The sub-table has to carry the SAME name as the array it belongs to:
        // `[agent.session]` under `[[engine]]` parses as a map where a sequence
        // is expected, and the daemon refuses to boot.
        out.push_str("  [engine.session]\n");
        for (k, v) in session {
            out.push_str(&format!("  {k} = {v}\n"));
        }
    }
    out
}

/// Remove the `[[agent]]` block that declares `id`.
///
/// Matched on `engine_id` first and `name` second, which is the order
/// `local_engines` resolves them in -- matching the other way round would drop
/// a different engine than the one asked for whenever the two disagree.
///
/// Returns None when no block matched, so the caller can answer 404 rather than
/// report a success it did not perform.
pub fn delete_agent_block(doc: &str, id: &str) -> Option<String> {
    let lines: Vec<&str> = doc.split('\n').collect();
    let mut blocks: Vec<(usize, usize)> = Vec::new();
    let mut start: Option<usize> = None;
    for (i, line) in lines.iter().enumerate() {
        let t = line.trim();
        // Both spellings, because a file may hold either and an operator who
        // edited one by hand must still be able to remove it from the UI.
        if t == "[[agent]]" || t == "[[engine]]" {
            if let Some(s) = start {
                blocks.push((s, i));
            }
            start = Some(i);
        } else if is_table_header(line)
            && !t.starts_with("[agent.")
            && !t.starts_with("[engine.")
            && start.is_some()
        {
            blocks.push((start.take().unwrap(), i));
        }
    }
    if let Some(s) = start {
        blocks.push((s, lines.len()));
    }

    for (s, e) in blocks {
        let mut name = String::new();
        let mut engine_id = String::new();
        for line in &lines[s..e] {
            let t = line.trim();
            if let Some(v) = t.strip_prefix("name") {
                if let Some(v) = v.trim().strip_prefix('=') {
                    name = v.trim().trim_matches('"').to_string();
                }
            } else if let Some(v) = t.strip_prefix("engine_id") {
                if let Some(v) = v.trim().strip_prefix('=') {
                    engine_id = v.trim().trim_matches('"').to_string();
                }
            }
        }
        let declared = if engine_id.is_empty() { name.as_str() } else { engine_id.as_str() };
        if declared == id {
            let mut kept: Vec<&str> = lines[..s].to_vec();
            kept.extend_from_slice(&lines[e..]);
            return Some(format!("{}\n", kept.join("\n").trim_end_matches('\n')));
        }
    }
    None
}

/// Set one key inside the session sub-table of an engine block.
///
/// Targeted text editing rather than parse-and-reserialise: `default.toml`
/// carries pages of comments that explain what each knob does, and round
/// tripping it through the decoder would delete every one of them.
///
/// Creates the sub-table when the block has none, and matches whichever
/// spelling the block uses so a file mid-migration keeps working.
pub fn set_agent_session_key(doc: &str, id: &str, key: &str, value: &str) -> Option<String> {
    let lines: Vec<&str> = doc.split('\n').collect();
    let mut blocks: Vec<(usize, usize, &str)> = Vec::new();
    let mut start: Option<(usize, &str)> = None;
    for (i, line) in lines.iter().enumerate() {
        let t = line.trim();
        if t == "[[agent]]" || t == "[[engine]]" {
            if let Some((s, name)) = start {
                blocks.push((s, i, name));
            }
            start = Some((i, if t == "[[agent]]" { "agent" } else { "engine" }));
        } else if is_table_header(line)
            && !t.starts_with("[agent.")
            && !t.starts_with("[engine.")
            && start.is_some()
        {
            let (s, name) = start.take().unwrap();
            blocks.push((s, i, name));
        }
    }
    if let Some((s, name)) = start {
        blocks.push((s, lines.len(), name));
    }

    for (s, e, name) in blocks {
        let mut declared = String::new();
        let mut fallback = String::new();
        for line in &lines[s..e] {
            let t = line.trim();
            if let Some(v) = t.strip_prefix("engine_id") {
                if let Some(v) = v.trim().strip_prefix('=') {
                    declared = v.trim().trim_matches('"').to_string();
                }
            } else if let Some(v) = t.strip_prefix("name") {
                if let Some(v) = v.trim().strip_prefix('=') {
                    fallback = v.trim().trim_matches('"').to_string();
                }
            }
        }
        let this = if declared.is_empty() { fallback.as_str() } else { declared.as_str() };
        if this != id {
            continue;
        }

        let header = format!("[{name}.session]");
        let mut out: Vec<String> = lines[..s].iter().map(|l| l.to_string()).collect();
        let mut sub = None;
        for (i, line) in lines[s..e].iter().enumerate() {
            if line.trim() == header {
                sub = Some(s + i);
            }
        }
        match sub {
            Some(h) => {
                // Replace the key in place if it is there, keeping every other
                // line of the block untouched.
                let mut replaced = false;
                for line in &lines[s..e] {
                    let t = line.trim();
                    if t.starts_with(key)
                        && t[key.len()..].trim_start().starts_with('=')
                        && !replaced
                        && lines[s..e].iter().position(|l| std::ptr::eq(*l, *line)).map(|p| s + p > h).unwrap_or(false)
                    {
                        out.push(format!("  {key} = {value}"));
                        replaced = true;
                    } else {
                        out.push(line.to_string());
                    }
                }
                if !replaced {
                    // The sub-table exists but not this key: insert right after
                    // its header, where it plainly belongs to it.
                    let at = h - s + out.len() - (e - s);
                    out.insert(at + 1, format!("  {key} = {value}"));
                }
            }
            None => {
                for line in &lines[s..e] {
                    out.push(line.to_string());
                }
                out.push(format!("  [{name}.session]"));
                out.push(format!("  {key} = {value}"));
            }
        }
        out.extend(lines[e..].iter().map(|l| l.to_string()));
        return Some(format!("{}\n", out.join("\n").trim_end_matches('\n')));
    }
    None
}

/// Render a JSON value as a TOML literal.
///
/// Only scalars and flat arrays: the settings screen edits leaves, and letting
/// a table through here would write a nested structure into a position that
/// expects a value.
pub fn toml_scalar(value: &serde_json::Value) -> Result<String, String> {
    use serde_json::Value;
    match value {
        Value::String(s) => Ok(quote_toml_key(s)),
        Value::Bool(b) => Ok(b.to_string()),
        Value::Number(n) => Ok(n.to_string()),
        Value::Array(items) => {
            let mut parts = Vec::with_capacity(items.len());
            for item in items {
                match item {
                    Value::Array(_) | Value::Object(_) => {
                        return Err("nested arrays are not editable here".into())
                    }
                    other => parts.push(toml_scalar(other)?),
                }
            }
            Ok(format!("[{}]", parts.join(", ")))
        }
        // The message names the GO type, because that is what 3.x reports and
        // the settings screen shows the string verbatim. It reads oddly from a
        // Rust binary; changing it would be a visible difference for no gain.
        Value::Null => Err("unsupported value type <nil> (scalars only)".into()),
        Value::Object(_) => Err("unsupported value type map[string]interface {} (scalars only)".into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const DOC: &str = "\
# Hydra Torrent Daemon - Configuration

[daemon]
api_port = 8199
create_torrent_folder = true  # keep the folder

# ─── SESSION RACE ───
[race]
listen_port = 16171

[announce_ip_modes]
\"gemini-tracker.org\" = \"v4\"
";

    // The whole point of editing text instead of re-serialising: the banner,
    // the blank lines and the inline comment all have to survive.
    #[test]
    fn editing_a_value_keeps_every_other_byte() {
        let out = set_toml_value(DOC, "race", "listen_port", "16999").unwrap();
        assert!(out.contains("listen_port = 16999"));
        assert!(out.contains("# ─── SESSION RACE ───"), "banner lost:\n{out}");
        assert!(out.contains("# Hydra Torrent Daemon - Configuration"));
        assert!(out.contains("create_torrent_folder = true  # keep the folder"));
        assert_eq!(out.lines().count(), DOC.lines().count());
    }

    #[test]
    fn an_inline_comment_on_the_edited_line_survives() {
        let out = set_toml_value(DOC, "daemon", "create_torrent_folder", "false").unwrap();
        assert!(
            out.contains("create_torrent_folder = false  # keep the folder"),
            "comment dropped:\n{out}"
        );
    }

    // Proven by breaking it: a missing key must be refused, not appended, or a
    // typo silently creates a second setting nobody reads.
    #[test]
    fn a_missing_key_is_an_error_not_an_append() {
        let err = set_toml_value(DOC, "race", "no_such_key", "1").unwrap_err();
        assert!(err.contains("no_such_key"), "{err}");
        assert!(set_toml_value(DOC, "nope", "listen_port", "1").is_err());
    }

    #[test]
    fn a_hash_inside_a_string_is_not_a_comment() {
        let doc = "[x]\nkey = \"a#b\"\n";
        let out = set_toml_value(doc, "x", "key", "\"c\"").unwrap();
        assert_eq!(out, "[x]\nkey = \"c\"\n");
    }

    #[test]
    fn setting_a_table_creates_missing_keys_and_missing_sections() {
        let pairs = vec![("\"t.example\"".to_string(), "\"v6\"".to_string())];
        let out = set_toml_table(DOC, "announce_ip_modes", &pairs).unwrap();
        assert!(out.contains("\"t.example\" = \"v6\""));
        assert!(out.contains("\"gemini-tracker.org\" = \"v4\""), "existing key lost");

        let out = set_toml_table(DOC, "announce_passkeys", &pairs).unwrap();
        assert!(out.trim_end().ends_with("[announce_passkeys]\n\"t.example\" = \"v6\""),
                "a new table belongs at the end:\n{out}");
    }

    #[test]
    fn deleting_is_idempotent_and_scoped_to_its_section() {
        let out = delete_toml_key(DOC, "race", "listen_port");
        assert!(!out.contains("listen_port"));
        assert!(out.contains("api_port = 8199"), "another section was touched");
        // deleting again changes nothing
        assert_eq!(delete_toml_key(&out, "race", "listen_port"), out);
    }

    #[test]
    fn an_emptied_table_is_pruned_but_one_holding_a_comment_is_kept() {
        let emptied = delete_toml_key(DOC, "announce_ip_modes", "\"gemini-tracker.org\"");
        let pruned = prune_empty_table(&emptied, "announce_ip_modes");
        assert!(!pruned.contains("[announce_ip_modes]"), "header should be gone:\n{pruned}");

        let with_comment = "[a]\n# the operator's note\n";
        assert_eq!(prune_empty_table(with_comment, "a"), with_comment);
    }

    #[test]
    fn deleting_a_table_takes_its_body_and_stops_at_the_next_header() {
        let out = delete_toml_table(DOC, "race");
        assert!(!out.contains("[race]"));
        assert!(!out.contains("listen_port = 16171"));
        assert!(out.contains("[announce_ip_modes]"), "the next table was eaten:\n{out}");
        assert!(out.contains("api_port = 8199"), "an earlier table was eaten");
        assert_eq!(delete_toml_table(&out, "race"), out, "must be idempotent");
    }

    #[test]
    fn scalars_render_as_toml_literals() {
        use serde_json::json;
        assert_eq!(toml_scalar(&json!("hi")).unwrap(), "\"hi\"");
        assert_eq!(toml_scalar(&json!(true)).unwrap(), "true");
        assert_eq!(toml_scalar(&json!(42)).unwrap(), "42");
        assert_eq!(toml_scalar(&json!(1.5)).unwrap(), "1.5");
        assert_eq!(toml_scalar(&json!(["a", "b"])).unwrap(), "[\"a\", \"b\"]");
        // A table written where a value belongs would corrupt the document.
        // The message names the Go type on purpose: see the comment above.
        assert_eq!(
            toml_scalar(&json!({"a": 1})).unwrap_err(),
            "unsupported value type map[string]interface {} (scalars only)"
        );
        assert!(toml_scalar(&json!(null)).is_err());
    }

    #[test]
    fn a_host_is_quoted_so_its_dots_do_not_nest_tables() {
        assert_eq!(quote_toml_key("tk.tr4ker.net"), "\"tk.tr4ker.net\"");
        assert_eq!(quote_toml_key("a\"b"), "\"a\\\"b\"");
    }
}

#[cfg(test)]
mod agent_block_tests {
    use super::*;

    const DOC: &str = "[daemon]\napi_port = 8199\n\n[race]\nlisten_port = 16371\n";

    #[test]
    fn an_appended_block_round_trips_through_the_parser() {
        let out = append_agent_block(
            DOC,
            "vpn1",
            "hoard",
            &[("listen_port".into(), "16473".into())],
        );
        let v: toml::Value = toml::from_str(&out).expect("still valid TOML");
        let agents = v.get("engine").and_then(|a| a.as_array()).expect("one engine");
        assert_eq!(agents.len(), 1);
        assert_eq!(agents[0]["engine_id"].as_str(), Some("vpn1"));
        assert_eq!(agents[0]["role"].as_str(), Some("hoard"));
        assert_eq!(agents[0]["session"]["listen_port"].as_integer(), Some(16473));
        // The rest of the file is untouched.
        assert_eq!(v["race"]["listen_port"].as_integer(), Some(16371));
    }

    /// Deleting the FIRST of two blocks must not take the second with it, and
    /// must not swallow the section that follows.
    #[test]
    fn only_the_named_block_goes() {
        let doc = append_agent_block(DOC, "vpn1", "hoard", &[("listen_port".into(), "16473".into())]);
        let doc = append_agent_block(&doc, "vpn2", "race", &[("listen_port".into(), "16474".into())]);
        let out = delete_agent_block(&doc, "vpn1").expect("vpn1 was there");
        let v: toml::Value = toml::from_str(&out).expect("still valid TOML");
        let agents = v.get("engine").and_then(|a| a.as_array()).expect("one left");
        assert_eq!(agents.len(), 1);
        assert_eq!(agents[0]["engine_id"].as_str(), Some("vpn2"));
        assert_eq!(v["race"]["listen_port"].as_integer(), Some(16371));
    }

    /// A file may hold both spellings at once, mid-migration. Removing one must
    /// not disturb the other, and the sub-table must go with its own block.
    #[test]
    fn both_spellings_live_together_and_are_removed_independently() {
        let doc = format!(
            "{DOC}\n[[agent]]\nname = \"old\"\nrole = \"race\"\nengine_id = \"old\"\n  [agent.session]\n  listen_port = 1111\n"
        );
        let doc = append_agent_block(&doc, "new", "hoard", &[("listen_port".into(), "2222".into())]);
        let v: toml::Value = toml::from_str(&doc).expect("both spellings parse");
        assert_eq!(v["agent"].as_array().unwrap().len(), 1);
        assert_eq!(v["engine"].as_array().unwrap().len(), 1);

        let out = delete_agent_block(&doc, "old").expect("the old one was there");
        let v: toml::Value = toml::from_str(&out).expect("still valid after removal");
        assert!(v.get("agent").is_none(), "its sub-table must have gone with it");
        assert_eq!(v["engine"].as_array().unwrap().len(), 1);
        assert_eq!(v["race"]["listen_port"].as_integer(), Some(16371));
    }

    #[test]
    fn an_unknown_id_reports_that_nothing_was_removed() {
        let doc = append_agent_block(DOC, "vpn1", "hoard", &[]);
        assert!(delete_agent_block(&doc, "absent").is_none());
    }
}

#[cfg(test)]
mod session_key_tests {
    use super::*;

    const DOC: &str = "[daemon]\napi_port = 8199\n\n[race]\nlisten_port = 16371\n";

    #[test]
    fn an_existing_key_is_replaced_and_the_comments_survive() {
        let doc = format!(
            "{DOC}\n[[engine]]\n# the tunnel this one leaves by\nname = \"vpn1\"\nrole = \"hoard\"\nengine_id = \"vpn1\"\n  [engine.session]\n  listen_port = 16473\n  enable_dht = false\n"
        );
        let out = set_agent_session_key(&doc, "vpn1", "listen_port", "16999").unwrap();
        let v: toml::Value = toml::from_str(&out).unwrap();
        let e = &v["engine"].as_array().unwrap()[0];
        assert_eq!(e["session"]["listen_port"].as_integer(), Some(16999));
        // Everything else in the block is left alone.
        assert_eq!(e["session"]["enable_dht"].as_bool(), Some(false));
        assert!(out.contains("# the tunnel this one leaves by"), "a comment was eaten");
        assert_eq!(v["race"]["listen_port"].as_integer(), Some(16371));
    }

    #[test]
    fn a_missing_sub_table_is_created() {
        let doc = format!("{DOC}\n[[engine]]\nname = \"vpn1\"\nrole = \"hoard\"\nengine_id = \"vpn1\"\n");
        let out = set_agent_session_key(&doc, "vpn1", "bind_interface", "\"wg1\"").unwrap();
        let v: toml::Value = toml::from_str(&out).unwrap();
        assert_eq!(v["engine"].as_array().unwrap()[0]["session"]["bind_interface"].as_str(), Some("wg1"));
    }

    #[test]
    fn an_unknown_engine_changes_nothing() {
        let doc = format!("{DOC}\n[[engine]]\nname = \"vpn1\"\nengine_id = \"vpn1\"\nrole = \"hoard\"\n");
        assert!(set_agent_session_key(&doc, "absent", "listen_port", "1").is_none());
    }
}
