//! Config keys that are accepted and do nothing.
//!
//! A key no struct names is dropped by serde, and a section no struct names is
//! kept verbatim in `Config::rest`, which nothing reads. Either way a 3.x file
//! or an old template loads without a word while its owner believes the keys
//! act -- a choking strategy, a retention window, a remote agent that is in
//! fact started nowhere. Refusing such a file would break every upgrade, so
//! the answer is to say it: one table below names every inert key, why it is
//! inert and what to use instead, and startup walks the RAW TOML against it
//! (serde no longer sees these keys) and logs one warning per key the file
//! holds.
//!
//! Only keys that are dead for good belong here. A key that a planned change
//! will wire (`inactivity_timeout`) is NOT listed: a warning telling the
//! operator to delete it would have them delete a setting the next release
//! reads. The proxy and relay keys (`socks5_outbound_*`, `announce_proxy`,
//! `announce_ip`, `*_proxy_v2`), the rate caps (`upload_rate_limit`,
//! `download_rate_limit`), `peer_timeout`, `choking`,
//! `max_uploads_per_torrent`, the queue (`active_seeds` and `active_limit`,
//! under `queueing = true`) and the share limits (`max_ratio`,
//! `max_seeding_time`, `max_inactive_seeding_time`, `share_limit_action`) are
//! read since 4.4.

use toml::Value;

/// Where a dead key lives.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scope {
    /// A dotted path from the root of the file: `daemon.agent_token`, or a
    /// whole section such as `metrics`.
    Root(&'static str),
    /// A key of an engine section: `[race]`, `[hoard]`, and the `session`
    /// sub-table of every `[[engine]]` / `[[agent]]` block, which is laid over
    /// them (`config::merge_session`) and so can carry the same dead keys.
    Engine(&'static str),
    /// A key of an `[[engine]]` / `[[agent]]` block itself.
    Block(&'static str),
    /// A field of a stored category (the store's `categories` document, not
    /// the TOML).
    Category(&'static str),
}

/// One inert key.
#[derive(Debug, Clone, Copy)]
pub struct DeadKey {
    pub scope: Scope,
    /// Why nothing reads it.
    pub why: &'static str,
    /// What to do instead.
    pub instead: &'static str,
    /// Warn only when the value asks for something. An `[[engine]]` block
    /// with `addr = ""` is a local engine, which is live; a category written
    /// by 4.x never carries `strategy = ""`, but one from 3.x may, and an
    /// empty value requests nothing worth a warning at every start.
    pub only_if_set: bool,
}

const NODES: &str = "enrol that machine as a node: install.sh --register-to <url> --token <token> \
                     (the Nodes page gives the full command)";

const fn dead(scope: Scope, why: &'static str, instead: &'static str) -> DeadKey {
    DeadKey { scope, why, instead, only_if_set: false }
}

const fn dead_if_set(scope: Scope, why: &'static str, instead: &'static str) -> DeadKey {
    DeadKey { scope, why, instead, only_if_set: true }
}

/// Every key Hydranos accepts and ignores.
pub const DEAD_KEYS: &[DeadKey] = &[
    // 3.x agent/front split: 4.x is one process and machines join as nodes.
    dead(
        Scope::Root("daemon.agent_token"),
        "it guarded the 3.x agent/front gRPC channel, which no longer exists; nodes authenticate with API keys",
        NODES,
    ),
    dead_if_set(
        Scope::Block("addr"),
        "a 3.x remote agent: 4.x has no agent channel, so this block starts nothing here and reaches nothing there",
        NODES,
    ),
    dead(
        Scope::Block("token"),
        "a credential of the 3.x agent channel, which no longer exists",
        NODES,
    ),
    dead(
        Scope::Block("tls_ca"),
        "a credential of the 3.x agent channel, which no longer exists",
        NODES,
    ),
    dead(
        Scope::Block("engine"),
        "a 3.x per-agent engine override; nothing matches it",
        "put the overridden keys in the block's `session` sub-table",
    ),
    // Engine keys the engine never receives (`engines::engine_config`).
    dead(
        Scope::Engine("listen_interfaces"),
        "it is never passed to the engine; peer sockets are pinned by interface name only",
        "bind_interface = \"<interface name>\"",
    ),
    dead(
        Scope::Engine("file_pool_size"),
        "not a setting: the open-file pool is sized by aio_threads",
        "aio_threads",
    ),
    dead(
        Scope::Engine("custom_choking"),
        "a 3.x choking strategy; the engine has one built-in choker and no strategy to pick",
        "choking = true and max_uploads_per_torrent = <slots> (off by default; read their help first)",
    ),
    dead(
        Scope::Engine("disk_slots"),
        "per-disk seeding slots were never implemented; every seeding torrent stays active",
        "nothing; stop torrents yourself if a disk must rest",
    ),
    // Sections nothing reads.
    dead(
        Scope::Root("bench"),
        "bench recording and its retention are built in; none of these keys is read",
        "nothing; delete the section",
    ),
    dead(
        Scope::Root("metrics"),
        "the /metrics endpoint takes no configuration",
        "nothing; /metrics is always served",
    ),
    dead(
        Scope::Root("peer_intel"),
        "peer intelligence was not carried over from 3.x",
        "nothing; delete the section",
    ),
    dead(
        Scope::Root("arr_cleanup"),
        "the *arr cleanup was not carried over from 3.x; it removes nothing",
        "the *arr's own completed-download handling",
    ),
    dead(
        Scope::Root("notify"),
        "notifications are sent by workflows, not by this section",
        "a workflow with a webhook action (Workflows tab)",
    ),
    dead(
        Scope::Root("vpn_speedtest.iperf3_port"),
        "there is no periodic speedtest and nothing reads this key",
        "nothing; delete the key",
    ),
    dead(
        Scope::Root("vpn_speedtest.interval_secs"),
        "there is no periodic speedtest and nothing reads this key",
        "nothing; delete the key",
    ),
    dead(
        Scope::Root("vpn_speedtest.duration_secs"),
        "there is no periodic speedtest and nothing reads this key",
        "nothing; delete the key",
    ),
    dead(
        Scope::Root("race_drain.min_age_minutes"),
        "the drain protects a torrent by its tracker's minimum seed time, not by its age",
        "the tracker's Minimum seed on the Trackers tab ([announce_min_seed_hours])",
    ),
    // 3.x multi-agent routing on categories: `api::placement` routes on the
    // mode alone. The fields are kept so a rollback to 3.x finds them.
    dead_if_set(
        Scope::Category("placement"),
        "3.x multi-agent routing; a category routes on its mode alone (kept only so a rollback to 3.x finds it)",
        "engine= when adding, or Move to engine afterwards",
    ),
    dead_if_set(
        Scope::Category("agents"),
        "3.x multi-agent routing; a category routes on its mode alone (kept only so a rollback to 3.x finds it)",
        "engine= when adding, or Move to engine afterwards",
    ),
    dead_if_set(
        Scope::Category("strategy"),
        "3.x multi-agent routing; a category routes on its mode alone (kept only so a rollback to 3.x finds it)",
        "engine= when adding, or Move to engine afterwards",
    ),
    dead_if_set(
        Scope::Category("min_free_bytes"),
        "3.x multi-agent routing; a category routes on its mode alone (kept only so a rollback to 3.x finds it)",
        "engine= when adding, or Move to engine afterwards",
    ),
];

fn message(location: &str, key: &DeadKey) -> String {
    format!("config: {location} is ignored: {}. Instead: {}.", key.why, key.instead)
}

fn is_set_toml(v: &Value) -> bool {
    match v {
        Value::String(s) => !s.trim().is_empty(),
        Value::Integer(n) => *n != 0,
        Value::Float(f) => *f != 0.0,
        Value::Boolean(b) => *b,
        Value::Array(a) => !a.is_empty(),
        Value::Table(t) => !t.is_empty(),
        Value::Datetime(_) => true,
    }
}

fn is_set_json(v: &serde_json::Value) -> bool {
    match v {
        serde_json::Value::Null => false,
        serde_json::Value::String(s) => !s.trim().is_empty(),
        serde_json::Value::Number(n) => n.as_f64().is_some_and(|f| f != 0.0),
        serde_json::Value::Bool(b) => *b,
        serde_json::Value::Array(a) => !a.is_empty(),
        serde_json::Value::Object(o) => !o.is_empty(),
    }
}

/// The `[[agent]]` and `[[engine]]` blocks, each with a label naming it the
/// way the operator wrote it.
fn blocks(root: &toml::value::Table) -> Vec<(String, &toml::value::Table)> {
    let mut out = Vec::new();
    for kind in ["agent", "engine"] {
        let Some(Value::Array(list)) = root.get(kind) else { continue };
        for (i, item) in list.iter().enumerate() {
            let Value::Table(t) = item else { continue };
            let named = ["name", "engine_id"]
                .iter()
                .find_map(|k| t.get(*k).and_then(Value::as_str).filter(|s| !s.trim().is_empty()));
            let label = match named {
                Some(n) => format!("[[{kind}]] {n:?}"),
                None => format!("[[{kind}]] #{}", i + 1),
            };
            out.push((label, t));
        }
    }
    out
}

/// One warning per dead key present in this config text.
///
/// A file that does not parse yields nothing: `Config::load` refuses it with
/// the parse error, which is the message that matters then.
pub fn config_warnings(raw: &str) -> Vec<String> {
    let Ok(Value::Table(root)) = raw.parse::<Value>() else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for key in DEAD_KEYS {
        let mut hit = |location: String, v: &Value| {
            if !key.only_if_set || is_set_toml(v) {
                out.push(message(&location, key));
            }
        };
        match key.scope {
            Scope::Root(path) => {
                let mut parts = path.split('.');
                let first = parts.next().unwrap_or_default();
                let mut cur = root.get(first);
                for p in parts {
                    cur = cur.and_then(|v| v.get(p));
                }
                if let Some(v) = cur {
                    let location = if v.is_table() { format!("[{path}]") } else { path.to_string() };
                    hit(location, v);
                }
            }
            Scope::Engine(name) => {
                for section in ["race", "hoard"] {
                    if let Some(v) = root.get(section).and_then(|s| s.get(name)) {
                        hit(format!("{section}.{name}"), v);
                    }
                }
                for (label, block) in blocks(&root) {
                    if let Some(v) = block.get("session").and_then(|s| s.get(name)) {
                        hit(format!("{label} session.{name}"), v);
                    }
                }
            }
            Scope::Block(name) => {
                for (label, block) in blocks(&root) {
                    if let Some(v) = block.get(name) {
                        hit(format!("{label} {name}"), v);
                    }
                }
            }
            Scope::Category(_) => {}
        }
    }
    out
}

/// One warning per dead category field, naming every category that sets it.
///
/// `doc` is the categories document as stored: a JSON object keyed by name.
/// Grouped by field rather than one line per category, because the fields are
/// kept on purpose (a rollback to 3.x needs them) and so come back at every
/// start; thirty identical lines would bury the rest of the log.
pub fn category_warnings(doc: &str) -> Vec<String> {
    let Ok(serde_json::Value::Object(cats)) = serde_json::from_str::<serde_json::Value>(doc) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for key in DEAD_KEYS {
        let Scope::Category(field) = key.scope else { continue };
        let names: Vec<&str> = cats
            .iter()
            .filter(|(_, c)| c.get(field).is_some_and(|v| !key.only_if_set || is_set_json(v)))
            .map(|(name, _)| name.as_str())
            .collect();
        if !names.is_empty() {
            out.push(message(&format!("category field `{field}` (in {})", names.join(", ")), key));
        }
    }
    out
}

/// Log every dead key of the config file once.
pub fn warn_config(path: &std::path::Path) {
    let Ok(raw) = std::fs::read_to_string(path) else { return };
    for w in config_warnings(&raw) {
        tracing::warn!("{w}");
    }
}

/// Log every dead category field once.
pub fn warn_categories(doc: Option<&str>) {
    for w in doc.map(category_warnings).unwrap_or_default() {
        tracing::warn!("{w}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The template every install is seeded from.
    const TEMPLATE: &str = include_str!("../../../configs/default.toml");

    /// ⭐ The anti-regression guard: the template a fresh install is seeded
    /// from must not ship a single key it then warns about. A dead key added
    /// back to default.toml fails here, not in a user's log.
    #[test]
    fn the_shipped_template_has_no_dead_key() {
        let w = config_warnings(TEMPLATE);
        assert!(w.is_empty(), "default.toml ships dead keys: {w:#?}");
    }

    /// The template still loads, so removing the keys broke nothing a reader
    /// depends on.
    #[test]
    fn the_shipped_template_still_loads() {
        let cfg: crate::config::Config = toml::from_str(TEMPLATE).expect("default.toml parses");
        assert_eq!(cfg.race.listen_port, 16171);
        assert_eq!(cfg.hoard.listen_port, 16172);
    }

    /// A file carried over from 3.x: every dead key it holds is named once,
    /// with the reason and the replacement.
    #[test]
    fn a_3x_config_warns_once_per_dead_key() {
        let raw = r#"
[daemon]
api_key = "k"
agent_token = ""
create_torrent_folder = false

[race]
listen_port = 16171
listen_interfaces = ""
max_uploads_per_torrent = 100
peer_timeout = 30
inactivity_timeout = 20

[race.custom_choking]
enabled = true
strategy = "rarity_captive"

[hoard]
listen_port = 16172
listen_interfaces = "10.0.0.2:16172"
active_seeds = -1
active_limit = -1

[hoard.disk_slots]
enabled = true

[bench]
enabled = true
retention_days = 365

[vpn_speedtest]
enabled = true
iperf3_server = "ping.online.net"
iperf3_port = 5201
interval_secs = 3600
duration_secs = 10

[race_drain]
enabled = true
min_age_minutes = 30

[metrics]
enabled = true

[peer_intel]
enabled = true

[arr_cleanup]
radarr_url = "http://radarr:7878"

[notify]
webhook_url = "https://discord.example/hook"

[proxy]
socks5_host = "10.0.0.1"

[[agent]]
name = "de-1"
addr = "10.0.0.5:9090"
token = "s3cret"
tls_ca = ""

[[agent.engine]]
id = "race-0"
"#;
        let w = config_warnings(raw);
        let expect = [
            "daemon.agent_token",
            "[[agent]] \"de-1\" addr",
            "[[agent]] \"de-1\" token",
            "[[agent]] \"de-1\" tls_ca",
            "[[agent]] \"de-1\" engine",
            "race.listen_interfaces",
            "hoard.listen_interfaces",
            "race.custom_choking",
            "hoard.disk_slots",
            "[bench]",
            "[metrics]",
            "[peer_intel]",
            "[arr_cleanup]",
            "[notify]",
            "vpn_speedtest.iperf3_port",
            "vpn_speedtest.interval_secs",
            "vpn_speedtest.duration_secs",
            "race_drain.min_age_minutes",
        ];
        for loc in expect {
            let n = w.iter().filter(|m| m.starts_with(&format!("config: {loc} is ignored"))).count();
            assert_eq!(n, 1, "{loc} warned {n} times in {w:#?}");
        }
        assert_eq!(w.len(), expect.len(), "nothing else is warned about: {w:#?}");
        // The remote agent is told where multi-machine went.
        let addr = w.iter().find(|m| m.contains("\"de-1\" addr")).unwrap();
        assert!(addr.contains("install.sh --register-to"), "{addr}");
        // The reason and the replacement are both in the line.
        let li = w.iter().find(|m| m.contains("race.listen_interfaces")).unwrap();
        assert!(li.contains("bind_interface"), "{li}");
    }

    /// Keys a planned change will wire, and the network-mode keys, are never
    /// called dead: an operator told to delete them would lose a setting the
    /// next release reads.
    #[test]
    fn keys_awaiting_their_wiring_are_not_called_dead() {
        let raw = r#"
[race]
max_uploads_per_torrent = 100
peer_timeout = 30
inactivity_timeout = 20
active_seeds = 5
active_limit = 10
upload_rate_limit = 0
download_rate_limit = 0
choking = false
announce_rate_limit = 0
start_paused = false
announce_proxy = "socks5h://10.0.0.1:1080"
announce_ip = ""
socks5_outbound_host = "10.0.0.1"
listen_port_proxy_v2 = 0
proxy_v2_trusted_sources = []
gluetun_port_forward = false

[proxy]
socks5_host = "10.0.0.1"
"#;
        assert_eq!(config_warnings(raw), Vec::<String>::new());
    }

    /// An `[[engine]]` with no addr runs here, which is live; the same block
    /// carrying a dead engine key in its session is still named.
    #[test]
    fn a_local_engine_block_is_live_but_its_dead_session_keys_are_named() {
        let raw = "[[engine]]\nname = \"vpn7\"\nrole = \"race\"\naddr = \"\"\n\
                   [engine.session]\nlisten_port = 26991\nlisten_interfaces = \"10.0.0.2:26991\"\n";
        let w = config_warnings(raw);
        assert_eq!(w.len(), 1, "{w:#?}");
        assert!(w[0].starts_with("config: [[engine]] \"vpn7\" session.listen_interfaces is ignored"), "{}", w[0]);
    }

    #[test]
    fn a_file_that_does_not_parse_warns_about_nothing() {
        assert!(config_warnings("[daemon\nagent_token = 1").is_empty());
    }

    /// Category routing fields: one line per field, naming the categories;
    /// an empty value requests nothing and is not warned about.
    #[test]
    fn dead_category_fields_are_grouped_by_field() {
        let doc = r#"{
            "films":  {"save_path": "/data/movies", "mode": "hoard", "strategy": "fill_then_next", "min_free_bytes": 0},
            "series": {"save_path": "/data/tv", "mode": "hoard", "strategy": "least_torrents", "placement": ["de-1"]},
            "race":   {"save_path": "/race", "mode": "race", "strategy": "", "agents": {}}
        }"#;
        let w = category_warnings(doc);
        assert_eq!(w.len(), 2, "{w:#?}");
        assert!(w.iter().any(|m| m.contains("`strategy` (in films, series)")), "{w:#?}");
        assert!(w.iter().any(|m| m.contains("`placement` (in series)")), "{w:#?}");
        assert!(category_warnings("{}").is_empty());
        assert!(category_warnings("not json").is_empty());
    }

    /// The settings editor hides every dead config key: it builds its rows
    /// from whatever the file holds, so a key missing from its hide list comes
    /// back as an editable field that looks live. Categories are not in the
    /// editor and are skipped.
    #[test]
    fn the_settings_editor_hides_every_dead_config_key() {
        let js = include_str!("../../../web/static/app.js");
        let start = js.find("const _DEAD_SETTINGS").expect("app.js has _DEAD_SETTINGS");
        let list = &js[start..start + js[start..].find("]);").expect("list ends")];
        for k in DEAD_KEYS {
            let wanted: Vec<String> = match k.scope {
                Scope::Root(p) => vec![match p.rsplit_once('.') {
                    Some((section, key)) => format!("\"{section}::{key}\""),
                    None => format!("\"{p}\""),
                }],
                Scope::Engine(name) => ["race", "hoard"]
                    .iter()
                    .map(|s| {
                        let (key, table) = (format!("\"{s}::{name}\""), format!("\"{s}.{name}\""));
                        if list.contains(&key) { key } else { table }
                    })
                    .collect(),
                Scope::Block(_) | Scope::Category(_) => continue,
            };
            for w in wanted {
                assert!(list.contains(&w), "app.js _DEAD_SETTINGS lacks {w}");
            }
        }
    }

    /// Every entry says why and what instead, and none is listed twice.
    #[test]
    fn every_dead_key_has_a_reason_and_a_replacement() {
        for (i, k) in DEAD_KEYS.iter().enumerate() {
            assert!(!k.why.is_empty() && !k.instead.is_empty(), "{:?}", k.scope);
            assert!(
                DEAD_KEYS[i + 1..].iter().all(|o| o.scope != k.scope),
                "{:?} listed twice",
                k.scope
            );
        }
    }
}
