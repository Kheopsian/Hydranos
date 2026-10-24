//! `hydranos-update` -- replace the daemon's binaries with a newer release.
//!
//! The 3.x package had a Go updater driven from the tray. The tray is gone and
//! this is a command, but the contract is the one that mattered and it is kept
//! word for word: **if the download or the checksum fails, nothing is replaced
//! and the daemon is not stopped.**
//!
//! It is a SEPARATE binary for a reason that is not stylistic: Windows locks a
//! running program's own file, so `hydranos.exe` cannot overwrite itself. This
//! one waits for the daemon to exit before touching anything.
//!
//!   hydranos-update --check                 say what the latest release is
//!   hydranos-update --dir C:\Hydranos       download it and swap the binaries
//!   hydranos-update --dir . --yes           no confirmation prompt
//!   hydranos-update --dir . --tag v4.1.7    a specific release
//!
//! Its requests leave the way the daemon's do: `[daemon] egress` and the
//! kill switch of the config file (`--config`, default `default.toml` in
//! `--dir`), resolved by the daemon's own rule in `typhon_engine::egress`.

use std::io::{Read, Write};
use std::path::{Path, PathBuf};

const REPO: &str = "Kheopsian/Hydranos";
const UA: &str = "hydranos-update";

fn main() -> std::process::ExitCode {
    match run() {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("hydranos-update: {e}");
            std::process::ExitCode::FAILURE
        }
    }
}

struct Args {
    dir: PathBuf,
    config: Option<PathBuf>,
    tag: Option<String>,
    check: bool,
    yes: bool,
}

fn parse() -> Args {
    let mut a = Args {
        dir: PathBuf::from("."),
        config: None,
        tag: None,
        check: false,
        yes: false,
    };
    let mut it = std::env::args().skip(1);
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--dir" => {
                if let Some(v) = it.next() {
                    a.dir = PathBuf::from(v);
                }
            }
            "--tag" => a.tag = it.next(),
            "--config" => a.config = it.next().map(PathBuf::from),
            "--check" => a.check = true,
            "--yes" | "-y" => a.yes = true,
            "--help" | "-h" => {
                println!("{}", HELP);
                std::process::exit(0);
            }
            _ => {}
        }
    }
    a
}

const HELP: &str = "\
hydranos-update -- replace the daemon's binaries with a newer release

  --check          report the latest release and exit
  --dir <path>     the folder holding hydranos (default: .)
  --tag <vX.Y.Z>   a specific release instead of the latest
  --config <file>  the daemon's config, for the way its requests leave
                   ([daemon] egress, the kill switch, the network mode)
                   (default: <dir>/default.toml)
  --yes            do not ask before replacing

Nothing is replaced unless the archive downloads AND its SHA-256 matches the
checksum published beside it.";

fn run() -> Result<(), String> {
    let args = parse();
    let config = args
        .config
        .clone()
        .unwrap_or_else(|| args.dir.join("default.toml"));
    let route = route_from(&config)?;
    if !route.is_direct() || route.kill_switch {
        println!("network: {}", route.describe());
    }
    let _ = ROUTE.set(route);
    let latest = resolve(args.tag.as_deref())?;
    let running = installed_version(&args.dir);

    println!("installed: {}", running.as_deref().unwrap_or("unknown"));
    println!("available: {}", latest.tag);

    if args.check {
        return Ok(());
    }
    // ⚠ Compared as strings, deliberately. "Same tag" is the only claim that
    // needs no version arithmetic, and --tag exists precisely so an operator
    // can go backwards on purpose. Guessing at ordering would refuse the
    // downgrade they asked for.
    if running.as_deref() == Some(latest.tag.trim_start_matches('v')) && args.tag.is_none() {
        println!("already on the latest release, nothing to do");
        return Ok(());
    }

    let asset = pick_asset(&latest)?;
    if asset.size > MAX_ARCHIVE {
        return Err(format!(
            "{} declares {} bytes, more than an archive of this program can be",
            asset.name, asset.size
        ));
    }
    println!(
        "downloading {} ({:.1} MB)",
        asset.name,
        asset.size as f64 / 1e6
    );

    let bytes = get_bytes(&asset.url)?;
    let want = expected_sha(&latest, &asset.name)?;
    let got = sha256_hex(&bytes);
    if got != want {
        // The one refusal that matters. A truncated or tampered archive must
        // never reach the point where a binary is replaced.
        return Err(format!(
            "checksum mismatch -- nothing replaced\n  expected {want}\n  got      {got}"
        ));
    }
    println!("sha256 ok");

    if !args.yes && !confirm(&latest.tag)? {
        println!("cancelled, nothing replaced");
        return Ok(());
    }

    let staged = unpack(&bytes, &asset.name)?;
    if staged.is_empty() {
        return Err("the archive held no binary this updater recognises".into());
    }
    stop_daemon(&args.dir)?;
    swap(&args.dir, &staged)?;
    println!("updated to {}. Start hydranos again.", latest.tag);
    Ok(())
}

// ---------------------------------------------------------------------------
// The release
// ---------------------------------------------------------------------------

struct Release {
    tag: String,
    assets: Vec<Asset>,
}

#[derive(Clone)]
struct Asset {
    name: String,
    url: String,
    size: u64,
}

/// The GitHub API address of a release: the latest, or the one `tag` names.
/// A tag is `[v]X.Y.Z` with an optional `-suffix` of letters, digits and dots,
/// nothing that could reach another API path.
fn release_url(tag: Option<&str>) -> Result<String, String> {
    if let Some(t) = tag {
        let bare = t.strip_prefix('v').unwrap_or(t);
        let (core, suffix) = match bare.split_once('-') {
            Some((c, s)) => (c, Some(s)),
            None => (bare, None),
        };
        let numbers = core.split('.').collect::<Vec<_>>();
        let core_ok = numbers.len() == 3
            && numbers
                .iter()
                .all(|n| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()));
        let suffix_ok = suffix.is_none_or(|s| {
            !s.is_empty() && s.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'.')
        });
        if !core_ok || !suffix_ok {
            return Err(format!(
                "--tag {t:?} is not a release tag (expected vX.Y.Z)"
            ));
        }
    }
    Ok(match tag {
        Some(t) => format!("https://api.github.com/repos/{REPO}/releases/tags/{t}"),
        None => format!("https://api.github.com/repos/{REPO}/releases/latest"),
    })
}

fn resolve(tag: Option<&str>) -> Result<Release, String> {
    let url = release_url(tag)?;
    let body = get_text(&url)?;
    let v: serde_json::Value =
        serde_json::from_str(&body).map_err(|e| format!("the release index is not JSON: {e}"))?;
    if let Some(msg) = v.get("message").and_then(|m| m.as_str()) {
        return Err(format!("GitHub says: {msg}"));
    }
    let tag = v["tag_name"]
        .as_str()
        .ok_or("the release has no tag")?
        .to_string();
    let assets = v["assets"]
        .as_array()
        .map(|a| {
            a.iter()
                .filter_map(|x| {
                    Some(Asset {
                        name: x["name"].as_str()?.to_string(),
                        url: x["browser_download_url"].as_str()?.to_string(),
                        size: x["size"].as_u64().unwrap_or(0),
                    })
                })
                .collect()
        })
        .unwrap_or_default();
    Ok(Release { tag, assets })
}

/// Where the release index may send a download: GitHub itself, over TLS. The
/// index is data from the network; a URL in it is followed only if it names
/// the host the release is published on (GitHub's own redirect to its object
/// store is followed by the client afterwards, and is not ours to vet).
fn asset_url_ok(url: &str) -> Result<(), String> {
    let u = url::Url::parse(url).map_err(|_| format!("{url:?} is not a URL"))?;
    if u.scheme() == "https" && u.host_str() == Some("github.com") {
        Ok(())
    } else {
        Err(format!(
            "{url:?} is not a github.com address -- refusing to download from it"
        ))
    }
}

/// The archive for THIS platform.
fn pick_asset(r: &Release) -> Result<Asset, String> {
    let (os, arch) = (platform_os(), platform_arch());
    let want_ext = if cfg!(windows) { ".zip" } else { ".tar.gz" };
    r.assets
        .iter()
        .find(|a| a.name.contains(os) && a.name.contains(arch) && a.name.ends_with(want_ext))
        .cloned()
        .ok_or_else(|| {
            format!(
                "release {} publishes nothing for {os}-{arch}{want_ext}\n  it has: {}",
                r.tag,
                r.assets
                    .iter()
                    .map(|a| a.name.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            )
        })
        .and_then(|a| asset_url_ok(&a.url).map(|()| a))
}

fn platform_os() -> &'static str {
    if cfg!(windows) {
        "windows"
    } else {
        "linux"
    }
}

fn platform_arch() -> &'static str {
    match std::env::consts::ARCH {
        "aarch64" => "arm64",
        _ => "amd64",
    }
}

/// The published checksum, read from the `.sha256` beside the archive.
fn expected_sha(r: &Release, archive: &str) -> Result<String, String> {
    let name = format!("{archive}.sha256");
    let a = r.assets.iter().find(|a| a.name == name).ok_or_else(|| {
        format!("no {name} published beside the archive -- refusing to install unverified bytes")
    })?;
    asset_url_ok(&a.url)?;
    let text = get_text(&a.url)?;
    sha_from_text(&text).ok_or_else(|| format!("{name} does not hold a sha256"))
}

/// The digest in a `.sha256` file: "<hex>  <filename>", the sha256sum format.
/// Exactly 64 hex digits, or nothing: a shorter, longer or non-hex first word
/// is no checksum to compare against.
fn sha_from_text(text: &str) -> Option<String> {
    text.split_whitespace()
        .next()
        .map(|s| s.to_lowercase())
        .filter(|s| s.len() == 64 && s.chars().all(|c| c.is_ascii_hexdigit()))
}

// ---------------------------------------------------------------------------
// HTTP
// ---------------------------------------------------------------------------

/// The way out, read once from the config file.
static ROUTE: std::sync::OnceLock<typhon_engine::egress::Route> = std::sync::OnceLock::new();

/// The daemon's route as its config file describes it: `[daemon] egress`,
/// the kill switch as written or deduced from `[network] mode`, and the
/// engines' own ways out, put together by the daemon's own rule
/// (`egress::resolve`). No file is the default route; a file that does not
/// parse is refused, since it may be the one holding a kill switch.
///
/// The engines are read from the file the simple way -- `[race]`, `[hoard]`
/// and the local `[[engine]]`/`[[agent]]` blocks over their role -- which is
/// all a way out needs.
fn route_from(path: &Path) -> Result<typhon_engine::egress::Route, String> {
    let text = match std::fs::read_to_string(path) {
        Ok(t) => t,
        Err(_) => return Ok(typhon_engine::egress::Route::default()),
    };
    let v: toml::Value = toml::from_str(&text).map_err(|e| format!("{}: {e}", path.display()))?;
    Ok(resolve_file(&v).route)
}

fn resolve_file(v: &toml::Value) -> typhon_engine::egress::Resolved {
    use typhon_engine::egress::{EngineExit, Route};
    let s = |t: &toml::Value, k: &str| {
        t.get(k)
            .and_then(|x| x.as_str())
            .unwrap_or("")
            .trim()
            .to_string()
    };
    let n = |t: &toml::Value, k: &str| t.get(k).and_then(|x| x.as_integer()).unwrap_or(0);
    let b = |t: &toml::Value, k: &str| t.get(k).and_then(|x| x.as_bool());
    let empty = toml::Value::Table(Default::default());
    let section = |k: &str| v.get(k).cloned().unwrap_or_else(|| empty.clone());
    let (daemon, proxy, race, hoard) = (
        section("daemon"),
        section("proxy"),
        section("race"),
        section("hoard"),
    );

    // The mode as saved, else the daemon's pre-4.4 deduction.
    let saved = v.get("network").map(|t| s(t, "mode")).unwrap_or_default();
    let mode = if ["direct", "wireguard", "gluetun", "socks5", "proxy_v2"].contains(&saved.as_str())
    {
        saved
    } else if b(&race, "gluetun_port_forward") == Some(true)
        || b(&hoard, "gluetun_port_forward") == Some(true)
    {
        "gluetun".into()
    } else if n(&race, "listen_port_proxy_v2") != 0 || n(&hoard, "listen_port_proxy_v2") != 0 {
        "proxy_v2".into()
    } else if !s(&race, "socks5_outbound_host").is_empty()
        || !s(&hoard, "socks5_outbound_host").is_empty()
    {
        "socks5".into()
    } else {
        "direct".into()
    };
    let armed = b(&daemon, "kill_switch").unwrap_or(mode != "direct");

    // The engines: race, hoard, then the local blocks over their role.
    let mut engines: Vec<(String, toml::Value)> = vec![
        ("race".into(), race.clone()),
        ("hoard".into(), hoard.clone()),
    ];
    for list in ["agent", "engine"] {
        for a in v.get(list).and_then(|x| x.as_array()).into_iter().flatten() {
            let role = s(a, "role");
            if !s(a, "addr").is_empty() || (role != "race" && role != "hoard") {
                continue;
            }
            let id = if s(a, "engine_id").is_empty() {
                s(a, "name")
            } else {
                s(a, "engine_id")
            };
            if id.is_empty() {
                continue;
            }
            let mut merged = if role == "race" {
                race.clone()
            } else {
                hoard.clone()
            };
            let own = a
                .get("session")
                .and_then(|x| x.as_table())
                .cloned()
                .unwrap_or_default();
            if let Some(m) = merged.as_table_mut() {
                // An extra engine has neither its role's tunnel nor its
                // role's "direct on purpose" (`Config::local_engines`).
                if id != "race" && id != "hoard" {
                    m.remove("wireguard_enabled");
                    m.remove("allow_direct");
                }
                m.extend(own);
            }
            match engines.iter().position(|(e, _)| *e == id) {
                Some(i) => engines[i].1 = merged,
                None => engines.push((id, merged)),
            }
        }
    }
    let exits: Vec<EngineExit> = engines
        .iter()
        .map(|(id, t)| {
            let tunnelled = mode == "wireguard"
                && b(t, "wireguard_enabled") == Some(true)
                && !s(t, "wireguard_config").is_empty();
            let iface = if tunnelled {
                typhon_engine::egress::tunnel_device(id)
            } else {
                s(t, "bind_interface")
            };
            let host = s(t, "socks5_outbound_host");
            let route = (tunnelled || !iface.is_empty() || !host.is_empty()).then(|| {
                Route::new(
                    &host,
                    u16::try_from(n(t, "socks5_outbound_port")).unwrap_or(0),
                    &s(t, "socks5_outbound_user"),
                    &s(t, "socks5_outbound_pass"),
                    &iface,
                    false,
                )
            });
            let direct = b(t, "allow_direct") == Some(true)
                || (mode == "wireguard" && b(t, "wireguard_enabled") == Some(false));
            let blocked = (armed
                && !direct
                && if mode == "wireguard" {
                    !tunnelled
                } else {
                    route.is_none() && mode != "gluetun"
                })
            .then(|| "no way out of its own, and not marked direct".to_string());
            EngineExit {
                id: id.clone(),
                route,
                tunnelled,
                blocked,
            }
        })
        .collect();
    let proxy_route = Route::new(
        &s(&proxy, "socks5_host"),
        u16::try_from(n(&proxy, "socks5_port")).unwrap_or(0),
        &s(&proxy, "socks5_user"),
        &s(&proxy, "socks5_pass"),
        &s(&daemon, "bind_interface"),
        false,
    );
    typhon_engine::egress::resolve(&mode, armed, &s(&daemon, "egress"), &proxy_route, &exits)
}

fn client() -> Result<reqwest::blocking::Client, String> {
    let route = ROUTE.get().cloned().unwrap_or_default();
    let c = typhon_engine::egress::blocking_client_for(&route)
        .map_err(|e| format!("cannot build an HTTP client: {e}"))?;
    Ok(c)
}

/// One GET with this tool's identity and the generous timeout a release
/// archive needs.
fn get(url: &str) -> Result<reqwest::blocking::Response, String> {
    client()?
        .get(url)
        .header(reqwest::header::USER_AGENT, UA)
        .timeout(std::time::Duration::from_secs(900))
        .send()
        .map_err(|e| format!("GET {url}: {e}"))
}

fn get_text(url: &str) -> Result<String, String> {
    let r = get(url)?;
    if !r.status().is_success() {
        return Err(format!("GET {url}: HTTP {}", r.status()));
    }
    r.text().map_err(|e| format!("reading {url}: {e}"))
}

/// The largest archive this updater downloads. A release tarball is around
/// 55 MB; anything near this is not one.
const MAX_ARCHIVE: u64 = 1 << 30;

fn get_bytes(url: &str) -> Result<Vec<u8>, String> {
    let r = get(url)?;
    if !r.status().is_success() {
        return Err(format!("GET {url}: HTTP {}", r.status()));
    }
    read_capped(r, MAX_ARCHIVE).map_err(|e| format!("reading {url}: {e}"))
}

/// Everything `r` holds, refused past `max` bytes rather than buffered.
fn read_capped(r: impl Read, max: u64) -> Result<Vec<u8>, String> {
    let mut out = Vec::new();
    r.take(max + 1)
        .read_to_end(&mut out)
        .map_err(|e| e.to_string())?;
    if out.len() as u64 > max {
        return Err(format!("more than {max} bytes, refused"));
    }
    Ok(out)
}

// ---------------------------------------------------------------------------
// SHA-256
// ---------------------------------------------------------------------------

fn sha256_hex(data: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    let mut h = Sha256::new();
    h.update(data);
    h.finalize().iter().map(|b| format!("{b:02x}")).collect()
}

// ---------------------------------------------------------------------------
// Unpacking
// ---------------------------------------------------------------------------

/// Binaries this updater is willing to replace, by file name.
fn is_ours(name: &str) -> bool {
    matches!(
        name,
        "hydranos"
            | "hydranos.exe"
            | "hydranos-engine"
            | "hydranos-engine.exe"
            | "hydranos-update"
            | "hydranos-update.exe"
    )
}

/// Pull the binaries out of the archive into memory.
///
/// ⚠ Only the binaries. `default.toml`, `data/` and `hydranos.log` live beside
/// them and are NOT in the archive: that is what makes updating in place the
/// one route that cannot lose an installation's settings.
fn unpack(bytes: &[u8], name: &str) -> Result<Vec<(String, Vec<u8>)>, String> {
    let mut out = Vec::new();
    if name.ends_with(".zip") {
        let mut zip = zip::ZipArchive::new(std::io::Cursor::new(bytes))
            .map_err(|e| format!("the archive is not a zip: {e}"))?;
        for i in 0..zip.len() {
            let mut f = zip
                .by_index(i)
                .map_err(|e| format!("reading the zip: {e}"))?;
            // zip 9: `name()` is fallible (a name that is not valid text is not ours).
            let Some(base) = f
                .name()
                .ok()
                .and_then(|n| n.rsplit(['/', '\\']).next().map(str::to_string))
            else {
                continue;
            };
            if !is_ours(&base) {
                continue;
            }
            let mut buf = Vec::new();
            f.read_to_end(&mut buf)
                .map_err(|e| format!("extracting {base}: {e}"))?;
            out.push((base, buf));
        }
    } else {
        let gz = flate2::read::GzDecoder::new(bytes);
        let mut tar = tar::Archive::new(gz);
        for entry in tar
            .entries()
            .map_err(|e| format!("reading the tarball: {e}"))?
        {
            let mut entry = entry.map_err(|e| format!("reading the tarball: {e}"))?;
            let path = entry
                .path()
                .map_err(|e| format!("a tar entry has no path: {e}"))?;
            let Some(base) = path
                .file_name()
                .and_then(|s| s.to_str())
                .map(str::to_string)
            else {
                continue;
            };
            if !is_ours(&base) {
                continue;
            }
            let mut buf = Vec::new();
            entry
                .read_to_end(&mut buf)
                .map_err(|e| format!("extracting {base}: {e}"))?;
            out.push((base, buf));
        }
    }
    Ok(out)
}

// ---------------------------------------------------------------------------
// Stop, swap
// ---------------------------------------------------------------------------

/// Ask the running daemon to stop, and wait for it.
///
/// A clean stop is not a nicety: it is what flushes resume data for every
/// torrent. Killing it means the next start re-checks whatever was in flight.
fn stop_daemon(_dir: &Path) -> Result<(), String> {
    #[cfg(windows)]
    {
        let running = || {
            std::process::Command::new("tasklist")
                .args(["/FI", "IMAGENAME eq hydranos.exe", "/NH"])
                .output()
                .map(|o| String::from_utf8_lossy(&o.stdout).contains("hydranos.exe"))
                .unwrap_or(false)
        };
        if !running() {
            return Ok(());
        }
        println!("stopping hydranos...");
        // The daemon waits on this event and stops the way the tray's Quit
        // does: drain, flush resume data, exit. `taskkill` without /F could
        // not do it -- it posts WM_CLOSE to top-level windows, and the
        // daemon has none -- so it is only the fallback for an older daemon.
        if !signal_stop_event() {
            let _ = std::process::Command::new("taskkill")
                .args(["/IM", "hydranos.exe", "/T"])
                .output();
        }
        for _ in 0..60 {
            if !running() {
                return Ok(());
            }
            std::thread::sleep(std::time::Duration::from_secs(1));
        }
        return Err(
            "hydranos is still running after 60s -- stop it yourself, nothing was replaced".into(),
        );
    }
    #[cfg(not(windows))]
    {
        // On Linux the daemon is a service; replacing the file under a running
        // process is safe there (the inode survives until it exits), so the
        // restart is the operator's, and systemd's, business.
        println!("note: restart the service for the new binaries to take effect");
        Ok(())
    }
}

/// Set the daemon's stop event. False when there is none to set (a daemon
/// older than 4.4, or none running).
#[cfg(windows)]
fn signal_stop_event() -> bool {
    use windows_sys::Win32::Foundation::CloseHandle;
    use windows_sys::Win32::System::Threading::{OpenEventW, SetEvent, EVENT_MODIFY_STATE};
    // Same name as `shutdown::STOP_EVENT` in the daemon.
    let name: Vec<u16> = "Local\\HydranosStop"
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect();
    unsafe {
        let h = OpenEventW(EVENT_MODIFY_STATE, 0, name.as_ptr());
        if h.is_null() {
            return false;
        }
        let ok = SetEvent(h) != 0;
        CloseHandle(h);
        ok
    }
}

/// Replace the binaries, keeping the old ones until the new ones are in place.
fn swap(dir: &Path, staged: &[(String, Vec<u8>)]) -> Result<(), String> {
    let mut done: Vec<PathBuf> = Vec::new();
    for (name, bytes) in staged {
        let target = dir.join(name);
        if !target.exists() {
            // Only replace what this install already has. An archive that
            // grows a new binary should not scatter it into a folder that
            // never asked for it.
            continue;
        }
        let backup = dir.join(format!("{name}.old"));
        let _ = std::fs::remove_file(&backup);
        // ⚠ Rename, never delete. Windows refuses to delete a running
        // program's file but ALLOWS renaming it, which is the whole trick that
        // makes in-place replacement possible.
        std::fs::rename(&target, &backup)
            .map_err(|e| format!("cannot move {} aside: {e}", target.display()))?;
        if let Err(e) = write_exe(&target, bytes) {
            // Put it back rather than leave the install without a daemon.
            let _ = std::fs::rename(&backup, &target);
            return Err(format!(
                "writing {}: {e} -- the old binary is back",
                target.display()
            ));
        }
        done.push(backup);
        println!("  replaced {name}");
    }
    for b in done {
        let _ = std::fs::remove_file(b);
    }
    Ok(())
}

fn write_exe(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let mut f = std::fs::File::create(path)?;
    f.write_all(bytes)?;
    f.sync_all()?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755))?;
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Bits and pieces
// ---------------------------------------------------------------------------

/// What is installed, asked of the binary itself.
fn installed_version(dir: &Path) -> Option<String> {
    let exe = dir.join(if cfg!(windows) {
        "hydranos.exe"
    } else {
        "hydranos"
    });
    let out = std::process::Command::new(&exe)
        .arg("--version")
        .output()
        .ok()?;
    let text = String::from_utf8_lossy(&out.stdout);
    // "hydranos 4.1.7"
    text.split_whitespace().nth(1).map(str::to_string)
}

fn confirm(tag: &str) -> Result<bool, String> {
    print!("replace the binaries with {tag}? [y/N] ");
    std::io::stdout().flush().ok();
    let mut line = String::new();
    std::io::stdin()
        .read_line(&mut line)
        .map_err(|e| format!("cannot read the answer: {e}"))?;
    Ok(matches!(
        line.trim().to_lowercase().as_str(),
        "y" | "yes" | "o" | "oui"
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn release_with(url: &str) -> Release {
        let ext = if cfg!(windows) { ".zip" } else { ".tar.gz" };
        Release {
            tag: "v4.4.4".into(),
            assets: vec![Asset {
                name: format!("hydranos-v4.4.4-{}-{}{ext}", platform_os(), platform_arch()),
                url: url.into(),
                size: 1,
            }],
        }
    }

    #[test]
    fn an_asset_is_only_taken_from_github() {
        let ok = "https://github.com/Kheopsian/Hydranos/releases/download/v4.4.4/x";
        assert!(pick_asset(&release_with(ok)).is_ok());
        for bad in [
            "https://192.168.99.7/hydranos.tar.gz",
            "https://evil.example/releases/download/v4.4.4/x",
            "https://github.com.evil.example/x",
            "https://github.com@evil.example/x",
            "http://github.com/Kheopsian/Hydranos/releases/download/v4.4.4/x",
            "file:///etc/passwd",
            "not a url",
        ] {
            assert!(
                pick_asset(&release_with(bad)).is_err(),
                "{bad:?} must be refused"
            );
        }
        // The checksum comes from the same list, so it gets the same refusal
        // (and before any request is made).
        let mut r = release_with(ok);
        r.assets.push(Asset {
            name: format!("{}.sha256", r.assets[0].name),
            url: "https://192.168.99.7/x.sha256".into(),
            size: 1,
        });
        let archive = r.assets[0].name.clone();
        let e = expected_sha(&r, &archive).unwrap_err();
        assert!(e.contains("github.com"), "{e}");
    }

    #[test]
    fn a_download_is_refused_past_its_cap() {
        let body = [7u8; 100];
        assert_eq!(read_capped(&body[..], 100).unwrap().len(), 100);
        assert!(read_capped(&body[..], 99).is_err());
    }

    #[test]
    fn only_a_release_tag_reaches_the_release_address() {
        assert_eq!(
            release_url(Some("v4.4.4")).unwrap(),
            format!("https://api.github.com/repos/{REPO}/releases/tags/v4.4.4")
        );
        assert!(release_url(Some("4.5.0")).is_ok());
        assert!(release_url(Some("v4.4.0-rc8")).is_ok());
        assert!(release_url(None).unwrap().ends_with("/releases/latest"));
        for bad in [
            "../../../user/repos",
            "v4.4.4/../../x",
            "v4.4.4?x=1",
            "v4.4.4#",
            "",
            "latest",
            "v4.4",
            "v4.4.4 ",
        ] {
            assert!(release_url(Some(bad)).is_err(), "{bad:?} must be refused");
        }
    }

    #[test]
    fn the_updater_leaves_the_way_the_daemon_does() {
        let path =
            std::env::temp_dir().join(format!("hydranos-update-route-{}.toml", std::process::id()));
        std::fs::write(
            &path,
            "[daemon]\negress = \"proxy\"\nbind_interface = \"wg0\"\nkill_switch = true\n[proxy]\nsocks5_host = \"10.0.0.1\"\nsocks5_port = 1080\n",
        )
        .unwrap();
        let r = route_from(&path).unwrap();
        assert_eq!(r.proxy, "socks5h://10.0.0.1:1080");
        assert_eq!(r.interface, "wg0");
        assert!(r.kill_switch);
        // A kill switch with no way out refuses to download anything.
        std::fs::write(&path, "[daemon]\nkill_switch = true\n").unwrap();
        let r = route_from(&path).unwrap();
        assert!(typhon_engine::egress::blocking_client_for(&r).is_err());
        // A file that does not parse may be the one holding the switch.
        std::fs::write(&path, "[daemon\n").unwrap();
        assert!(route_from(&path).is_err());
        let _ = std::fs::remove_file(&path);
        // No file: the default route, as before.
        assert!(route_from(Path::new("/nonexistent/default.toml"))
            .unwrap()
            .is_direct());
    }

    fn resolved(text: &str) -> typhon_engine::egress::Resolved {
        resolve_file(&toml::from_str(text).unwrap())
    }

    /// `auto` follows the mode exactly as the daemon does: the race engine's
    /// tunnel or proxy, direct in direct mode, and a refusal -- never the
    /// default route -- when the mode's way out is missing.
    #[test]
    fn the_updater_follows_the_network_mode_like_the_daemon() {
        // A production-like file: no mode, no kill switch. Direct, as before.
        let r = resolved("[daemon]\napi_port = 8199\n[race]\nlisten_port = 16171\n[hoard]\nlisten_port = 16172\n");
        assert_eq!(r.kind, "direct");
        assert!(r.route.is_direct() && !r.route.kill_switch);
        let wg = resolved(
            "[network]\nmode = \"wireguard\"\n[race]\nwireguard_enabled = true\nwireguard_config = \"a.conf\"\n",
        );
        assert_eq!(
            (wg.kind, wg.route.interface.as_str()),
            ("engine_tunnel", "wg-race")
        );
        assert!(wg.route.kill_switch);
        let socks = resolved(
            "[network]\nmode = \"socks5\"\n[hoard]\nsocks5_outbound_host = \"10.0.0.2\"\n",
        );
        assert_eq!(
            socks.route.proxy, "socks5h://10.0.0.2:1080",
            "the first engine with a proxy"
        );
        let none = resolved("[network]\nmode = \"wireguard\"\n");
        assert_eq!(none.kind, "refused");
        assert!(typhon_engine::egress::blocking_client_for(&none.route).is_err());
        let disarmed = resolved("[network]\nmode = \"wireguard\"\n[daemon]\nkill_switch = false\n");
        assert!(disarmed.route.is_direct() && disarmed.route.preflight().is_ok());
    }

    // ---- the mode, deduced or saved ----

    #[test]
    fn an_old_config_without_a_saved_mode_has_its_mode_deduced_like_the_daemon_does() {
        let gluetun = resolved("[race]\ngluetun_port_forward = true\n");
        assert_eq!(
            gluetun.kind, "gluetun",
            "inside gluetun the default route is the tunnel"
        );
        let hoard_gluetun = resolved("[hoard]\ngluetun_port_forward = true\n");
        assert_eq!(
            hoard_gluetun.kind, "gluetun",
            "either engine's flag will do"
        );

        let v2 = resolved("[hoard]\nlisten_port_proxy_v2 = 9000\nsocks5_outbound_host = \"10.0.0.3\"\nsocks5_outbound_port = 1081\n");
        assert_eq!((v2.kind, v2.engine.as_str()), ("engine_proxy", "hoard"));
        assert_eq!(v2.route.proxy, "socks5h://10.0.0.3:1081");
        assert!(
            v2.route.kill_switch,
            "a non-direct mode arms the kill switch unless the file says otherwise"
        );

        let socks = resolved("[race]\nsocks5_outbound_host = \"10.0.0.2\"\n");
        assert_eq!(
            (socks.kind, socks.engine.as_str()),
            ("engine_proxy", "race")
        );
        assert_eq!(
            socks.route.proxy, "socks5h://10.0.0.2:1080",
            "the default port"
        );
    }

    #[test]
    fn a_saved_mode_beats_what_the_engines_sections_suggest() {
        let r = resolved("[network]\nmode = \"direct\"\n[race]\ngluetun_port_forward = true\nsocks5_outbound_host = \"10.0.0.2\"\n");
        assert_eq!(r.kind, "direct");
        assert!(
            r.route.is_direct() && !r.route.kill_switch,
            "direct mode is not armed by default"
        );
        // An unknown saved mode is not a mode: the deduction takes over.
        let unknown =
            resolved("[network]\nmode = \"carrier_pigeon\"\n[race]\ngluetun_port_forward = true\n");
        assert_eq!(unknown.kind, "gluetun");
    }

    #[test]
    fn the_daemon_setting_can_name_a_direct_exit_bound_to_an_interface() {
        let r = resolved("[daemon]\negress = \"direct\"\nbind_interface = \"eth9\"\n[network]\nmode = \"socks5\"\n");
        assert_eq!(r.kind, "direct");
        assert_eq!(r.route.interface, "eth9");
        assert!(
            !r.route.kill_switch,
            "an explicit direct is a choice, not a refusal"
        );
    }

    // ---- the engines a way out can be borrowed from ----

    #[test]
    fn an_extra_engine_block_can_lend_its_own_proxy() {
        let r = resolved(
            "[network]\nmode = \"socks5\"\n[[engine]]\nengine_id = \"vpn1\"\nrole = \"race\"\n\
             [engine.session]\nsocks5_outbound_host = \"10.0.0.9\"\nsocks5_outbound_port = 1090\n",
        );
        assert_eq!((r.kind, r.engine.as_str()), ("engine_proxy", "vpn1"));
        assert_eq!(r.route.proxy, "socks5h://10.0.0.9:1090");
        // Named as the exit, the same engine is borrowed whatever the mode picks.
        let named = resolved(
            "[daemon]\negress = \"engine:vpn1\"\n[network]\nmode = \"socks5\"\n[[agent]]\nname = \"vpn1\"\nrole = \"hoard\"\n\
             [agent.session]\nsocks5_outbound_host = \"10.0.0.8\"\n",
        );
        assert_eq!(
            (named.kind, named.engine.as_str()),
            ("engine_proxy", "vpn1"),
            "[[agent]] blocks count as well, named by `name`"
        );
        assert_eq!(named.route.proxy, "socks5h://10.0.0.8:1080");
    }

    /// A block named like a role's own engine replaces that engine's settings
    /// rather than adding a second one: the node overrides its race engine.
    #[test]
    fn a_block_named_race_overrides_the_race_engine() {
        let r = resolved(
            "[daemon]\negress = \"engine:race\"\n[network]\nmode = \"socks5\"\n[race]\nsocks5_outbound_host = \"10.0.0.2\"\n\
             [[engine]]\nengine_id = \"race\"\nrole = \"race\"\n[engine.session]\nbind_interface = \"wg9\"\n",
        );
        assert_eq!(
            (r.kind, r.engine.as_str()),
            ("engine_proxy", "race"),
            "the role's proxy is kept under the block's settings"
        );
        assert_eq!(
            r.route.interface, "wg9",
            "and the block's own device is laid over it"
        );
        assert_eq!(r.route.proxy, "socks5h://10.0.0.2:1080");
    }

    /// A block's own `engine_id` and a role's `allow_direct` do not mix: an
    /// extra engine has not the role's "direct on purpose", so it is blocked
    /// where the role's own engine is not.
    #[test]
    fn an_extra_engine_does_not_inherit_its_roles_permission_to_go_direct() {
        let toml = "[network]\nmode = \"socks5\"\n[race]\nallow_direct = true\n\
                    [[engine]]\nengine_id = \"vpn1\"\nrole = \"race\"\n";
        let race = resolved(&format!("[daemon]\negress = \"engine:race\"\n{toml}"));
        assert_eq!((race.kind, race.engine.as_str()), ("engine_direct", "race"));
        let vpn1 = resolved(&format!("[daemon]\negress = \"engine:vpn1\"\n{toml}"));
        assert_eq!(vpn1.kind, "refused");
        assert!(
            vpn1.via.contains("vpn1") && vpn1.via.contains("blocked by the kill switch"),
            "{}",
            vpn1.via
        );
        assert!(
            typhon_engine::egress::blocking_client_for(&vpn1.route).is_err(),
            "and nothing is downloaded through it"
        );
    }

    #[test]
    fn a_block_with_an_address_a_foreign_role_or_no_name_lends_nothing() {
        for block in [
            "[[agent]]\nname = \"far\"\nrole = \"race\"\naddr = \"192.0.2.5:7700\"\n",
            "[[agent]]\nname = \"far\"\nrole = \"watcher\"\n",
            "[[agent]]\nrole = \"race\"\n",
        ] {
            let r = resolved(&format!(
                "[daemon]\negress = \"engine:far\"\n[network]\nmode = \"socks5\"\n{block}"
            ));
            assert_eq!(r.kind, "refused", "{block}");
            assert!(r.via.contains("does not run"), "{}", r.via);
        }
    }

    #[test]
    fn an_extra_engine_with_its_own_tunnel_lends_its_device() {
        let r = resolved(
            "[daemon]\negress = \"engine:vpn1\"\n[network]\nmode = \"wireguard\"\n\
             [[engine]]\nengine_id = \"vpn1\"\nrole = \"race\"\n\
             [engine.session]\nwireguard_enabled = true\nwireguard_config = \"vpn1.conf\"\n",
        );
        assert_eq!((r.kind, r.engine.as_str()), ("engine_tunnel", "vpn1"));
        assert_eq!(
            r.route.interface,
            typhon_engine::egress::tunnel_device("vpn1")
        );
        assert!(r.route.kill_switch);
        // A tunnel turned off on purpose is a way to go direct, not a hole.
        let off = resolved("[network]\nmode = \"wireguard\"\n[race]\nwireguard_enabled = false\n");
        assert_eq!(
            off.kind, "refused",
            "auto in WireGuard mode wants a tunnel to lend; a direct engine is not one"
        );
        let named = resolved("[daemon]\negress = \"engine:race\"\n[network]\nmode = \"wireguard\"\n[race]\nwireguard_enabled = false\n");
        assert_eq!(
            (named.kind, named.engine.as_str()),
            ("engine_direct", "race")
        );
    }

    // ---- the deductions of the mode and of the engines' exits ----

    /// The pre-4.4 deduction reads the proxy_v2 listener of EITHER engine,
    /// and a mode that needs a proxy nobody has refuses rather than going out
    /// the default route.
    #[test]
    fn either_engines_proxy_v2_listener_deduces_the_proxy_v2_mode() {
        for section in ["race", "hoard"] {
            let r = resolved(&format!("[{section}]\nlisten_port_proxy_v2 = 9000\n"));
            assert_eq!(
                r.kind, "refused",
                "[{section}] alone: proxy_v2 mode, armed, and no proxy to lend"
            );
            assert!(r.route.kill_switch);
        }
        assert_eq!(
            resolved("[race]\nlisten_port_proxy_v2 = 0\n").kind,
            "direct",
            "a zero port is no listener"
        );
    }

    /// A block named like a role's engine IS that engine: it keeps the role's
    /// "direct on purpose" and its tunnel, which only a differently named
    /// extra engine loses.
    #[test]
    fn a_block_named_like_a_role_keeps_that_roles_direct_and_tunnel() {
        for role in ["race", "hoard"] {
            let direct = resolved(&format!(
                "[daemon]\negress = \"engine:{role}\"\n[network]\nmode = \"socks5\"\n[{role}]\nallow_direct = true\n\
                 [[engine]]\nengine_id = \"{role}\"\nrole = \"{role}\"\n"
            ));
            assert_eq!(
                (direct.kind, direct.engine.as_str()),
                ("engine_direct", role),
                "{role}"
            );
            let tunnel = resolved(&format!(
                "[daemon]\negress = \"engine:{role}\"\n[network]\nmode = \"wireguard\"\n\
                 [{role}]\nwireguard_enabled = true\nwireguard_config = \"a.conf\"\n\
                 [[engine]]\nengine_id = \"{role}\"\nrole = \"{role}\"\n"
            ));
            assert_eq!(
                (tunnel.kind, tunnel.engine.as_str()),
                ("engine_tunnel", role),
                "{role}"
            );
            assert_eq!(
                tunnel.route.interface,
                typhon_engine::egress::tunnel_device(role)
            );
        }
    }

    /// A tunnel is a tunnel only in WireGuard mode, switched on, with a
    /// config to bring it up: a config alone lends nothing.
    #[test]
    fn a_wireguard_config_alone_does_not_make_a_tunnel() {
        // Switched off on purpose: the engine goes direct.
        let off = resolved(
            "[daemon]\negress = \"engine:race\"\n[network]\nmode = \"wireguard\"\n[race]\nwireguard_enabled = false\nwireguard_config = \"a.conf\"\n",
        );
        assert_eq!((off.kind, off.engine.as_str()), ("engine_direct", "race"));
        // Switched on with no config: nothing to bring up, so blocked.
        let bare = resolved(
            "[daemon]\negress = \"engine:race\"\n[network]\nmode = \"wireguard\"\n[race]\nwireguard_enabled = true\n",
        );
        assert_eq!(bare.kind, "refused");
        // Another mode: the config is ignored and the engine's proxy is what it lends.
        let socks = resolved(
            "[daemon]\negress = \"engine:race\"\n[network]\nmode = \"socks5\"\n[race]\nwireguard_enabled = true\n\
             wireguard_config = \"a.conf\"\nsocks5_outbound_host = \"10.0.0.4\"\n",
        );
        assert_eq!(
            (socks.kind, socks.engine.as_str()),
            ("engine_proxy", "race")
        );
        assert_eq!(socks.route.proxy, "socks5h://10.0.0.4:1080");
    }

    /// An engine with only a bound interface, or only a proxy host, has a way
    /// out of its own.
    #[test]
    fn an_interface_alone_or_a_proxy_host_alone_is_a_way_out() {
        let iface =
            resolved("[daemon]\negress = \"engine:race\"\n[race]\nbind_interface = \"eth9\"\n");
        assert_eq!(
            (iface.kind, iface.engine.as_str()),
            ("engine_interface", "race")
        );
        assert_eq!(iface.route.interface, "eth9");
        let host = resolved(
            "[daemon]\negress = \"engine:hoard\"\n[hoard]\nsocks5_outbound_host = \"10.0.0.6\"\n",
        );
        assert_eq!((host.kind, host.engine.as_str()), ("engine_proxy", "hoard"));
        assert_eq!(host.route.proxy, "socks5h://10.0.0.6:1080");
    }

    /// "Direct on purpose" is `allow_direct`, or a tunnel switched OFF in
    /// WireGuard mode -- not WireGuard mode alone, nor a tunnel flag in
    /// another mode.
    #[test]
    fn only_a_deliberate_direct_exempts_an_engine_from_the_kill_switch() {
        let unset =
            resolved("[daemon]\negress = \"engine:race\"\n[network]\nmode = \"wireguard\"\n");
        assert_eq!(
            unset.kind, "refused",
            "WireGuard mode with the tunnel flag absent is a hole, not a choice"
        );
        assert!(
            unset.via.contains("blocked by the kill switch"),
            "{}",
            unset.via
        );
        let other_mode = resolved(
            "[daemon]\negress = \"engine:race\"\n[network]\nmode = \"socks5\"\n[race]\nwireguard_enabled = false\n",
        );
        assert_eq!(
            other_mode.kind, "refused",
            "a tunnel switched off means nothing outside WireGuard mode"
        );
        assert!(
            other_mode.via.contains("blocked by the kill switch"),
            "{}",
            other_mode.via
        );
    }

    // ---- reading and unpacking an archive ----

    #[test]
    fn sha256_is_the_hex_digest() {
        assert_eq!(
            sha256_hex(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        assert_eq!(
            sha256_hex(b""),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
    }

    #[test]
    fn only_the_daemons_own_binaries_are_replaceable() {
        for ours in [
            "hydranos",
            "hydranos.exe",
            "hydranos-engine",
            "hydranos-engine.exe",
            "hydranos-update",
            "hydranos-update.exe",
        ] {
            assert!(is_ours(ours), "{ours}");
        }
        for not in [
            "default.toml",
            "hydranos.log",
            "hydranos.old",
            "Hydranos",
            "hydranos-update.sha256",
            "",
            "data",
        ] {
            assert!(!is_ours(not), "{not:?} must stay where it is");
        }
    }

    #[test]
    fn a_read_that_fails_midway_is_an_error_not_a_short_archive() {
        struct Broken(u8);
        impl Read for Broken {
            fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
                if self.0 == 0 {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::ConnectionReset,
                        "reset by peer",
                    ));
                }
                self.0 -= 1;
                buf[0] = 1;
                Ok(1)
            }
        }
        assert!(read_capped(Broken(3), 100)
            .unwrap_err()
            .contains("reset by peer"));
        assert_eq!(read_capped(&b""[..], 0).unwrap(), Vec::<u8>::new());
        let e = read_capped(&[0u8; 11][..], 10).unwrap_err();
        assert!(e.contains("more than 10 bytes"), "{e}");
    }

    fn tarball(files: &[(&str, &[u8])]) -> Vec<u8> {
        let gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
        let mut b = tar::Builder::new(gz);
        for (path, data) in files {
            let mut h = tar::Header::new_gnu();
            h.set_size(data.len() as u64);
            h.set_mode(0o644);
            h.set_cksum();
            b.append_data(&mut h, path, *data).unwrap();
        }
        b.into_inner().unwrap().finish().unwrap()
    }

    fn zipped(files: &[(&str, &[u8])]) -> Vec<u8> {
        let mut z = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
        let opts = zip::write::SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Stored);
        for (path, data) in files {
            z.start_file(*path, opts).unwrap();
            z.write_all(data).unwrap();
        }
        z.finish().unwrap().into_inner()
    }

    fn names(staged: &[(String, Vec<u8>)]) -> Vec<&str> {
        let mut n: Vec<&str> = staged.iter().map(|(n, _)| n.as_str()).collect();
        n.sort();
        n
    }

    /// Whatever folder the release wraps them in, the binaries come out by
    /// their bare name, and nothing else of the archive does.
    #[test]
    fn a_tarball_gives_up_its_binaries_and_nothing_else() {
        let archive = tarball(&[
            ("hydranos-v4.4.4-linux-amd64/hydranos", b"daemon bytes"),
            (
                "hydranos-v4.4.4-linux-amd64/hydranos-update",
                b"updater bytes",
            ),
            ("hydranos-v4.4.4-linux-amd64/default.toml", b"[daemon]\n"),
            ("hydranos-v4.4.4-linux-amd64/data/hydranos.log", b"log"),
            ("hydranos-engine", b"engine bytes"),
        ]);
        let staged = unpack(&archive, "hydranos-v4.4.4-linux-amd64.tar.gz").unwrap();
        assert_eq!(
            names(&staged),
            vec!["hydranos", "hydranos-engine", "hydranos-update"]
        );
        let get = |n: &str| staged.iter().find(|(name, _)| name == n).unwrap().1.clone();
        assert_eq!(get("hydranos"), b"daemon bytes");
        assert_eq!(get("hydranos-engine"), b"engine bytes");
    }

    #[test]
    fn a_zip_gives_up_its_binaries_whatever_the_path_separator() {
        let archive = zipped(&[
            ("hydranos-v4.4.4-windows-amd64/hydranos.exe", b"daemon exe"),
            (
                "hydranos-v4.4.4-windows-amd64\\hydranos-update.exe",
                b"updater exe",
            ),
            ("hydranos-v4.4.4-windows-amd64/default.toml", b"[daemon]\n"),
            ("README.txt", b"read me"),
        ]);
        let staged = unpack(&archive, "hydranos-v4.4.4-windows-amd64.zip").unwrap();
        assert_eq!(names(&staged), vec!["hydranos-update.exe", "hydranos.exe"]);
        assert_eq!(
            staged.iter().find(|(n, _)| n == "hydranos.exe").unwrap().1,
            b"daemon exe"
        );
    }

    #[test]
    fn an_archive_with_no_binary_of_ours_stages_nothing() {
        let tgz = unpack(
            &tarball(&[("pkg/default.toml", b"x"), ("pkg/lib/hydranos.so", b"y")]),
            "r.tar.gz",
        )
        .unwrap();
        assert!(
            tgz.is_empty(),
            "the caller turns this into 'no binary this updater recognises'"
        );
        assert!(unpack(&zipped(&[("notes.txt", b"x")]), "r.zip")
            .unwrap()
            .is_empty());
    }

    #[test]
    fn bytes_that_are_not_the_archive_they_claim_to_be_are_refused() {
        let e = unpack(b"definitely not a zip", "r.zip").unwrap_err();
        assert!(e.contains("not a zip"), "{e}");
        let e = unpack(b"definitely not gzip", "r.tar.gz").unwrap_err();
        assert!(e.contains("tarball"), "{e}");
        // A zip handed over under a tarball's name is no tarball either.
        assert!(unpack(&zipped(&[("hydranos", b"x")]), "r.tar.gz").is_err());
    }

    // ---- replacing the binaries ----

    fn scratch(tag: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("hydranos-update-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn staged(items: &[(&str, &[u8])]) -> Vec<(String, Vec<u8>)> {
        items
            .iter()
            .map(|(n, b)| (n.to_string(), b.to_vec()))
            .collect()
    }

    /// Only what the install already has is replaced; the old copy goes once
    /// the new one is in; settings beside them are not touched.
    #[test]
    fn a_swap_replaces_what_is_installed_and_adds_nothing() {
        let dir = scratch("swap");
        std::fs::write(dir.join("hydranos"), b"old daemon").unwrap();
        std::fs::write(dir.join("hydranos-engine"), b"old engine").unwrap();
        std::fs::write(dir.join("default.toml"), b"[daemon]\napi_port = 8199\n").unwrap();
        std::fs::write(
            dir.join("hydranos-engine.old"),
            b"a leftover of an earlier update",
        )
        .unwrap();

        swap(
            &dir,
            &staged(&[
                ("hydranos", b"new daemon"),
                ("hydranos-engine", b"new engine"),
                ("hydranos-update", b"new updater"),
            ]),
        )
        .unwrap();

        assert_eq!(std::fs::read(dir.join("hydranos")).unwrap(), b"new daemon");
        assert_eq!(
            std::fs::read(dir.join("hydranos-engine")).unwrap(),
            b"new engine"
        );
        assert!(
            !dir.join("hydranos-update").exists(),
            "an install that never had it does not get it"
        );
        assert!(
            !dir.join("hydranos.old").exists() && !dir.join("hydranos-engine.old").exists(),
            "no backup left behind"
        );
        assert_eq!(
            std::fs::read(dir.join("default.toml")).unwrap(),
            b"[daemon]\napi_port = 8199\n",
            "settings stay"
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                std::fs::metadata(dir.join("hydranos"))
                    .unwrap()
                    .permissions()
                    .mode()
                    & 0o777,
                0o755,
                "runnable"
            );
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A binary that cannot be moved aside stops the swap before it touches
    /// that file: the install is whole.
    #[test]
    fn a_swap_that_cannot_move_a_binary_aside_leaves_it_in_place() {
        let dir = scratch("swap-stuck");
        std::fs::write(dir.join("hydranos"), b"old daemon").unwrap();
        // Something the backup name cannot replace: a directory that is not empty.
        std::fs::create_dir_all(dir.join("hydranos.old")).unwrap();
        std::fs::write(dir.join("hydranos.old").join("keep"), b"x").unwrap();

        let e = swap(&dir, &staged(&[("hydranos", b"new daemon")])).unwrap_err();
        assert!(e.contains("cannot move") && e.contains("aside"), "{e}");
        assert_eq!(
            std::fs::read(dir.join("hydranos")).unwrap(),
            b"old daemon",
            "the old daemon is still the daemon"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_swap_with_nothing_staged_is_a_no_op() {
        let dir = scratch("swap-empty");
        std::fs::write(dir.join("hydranos"), b"old daemon").unwrap();
        swap(&dir, &[]).unwrap();
        assert_eq!(std::fs::read(dir.join("hydranos")).unwrap(), b"old daemon");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn stopping_the_daemon_on_linux_is_the_services_business() {
        #[cfg(not(windows))]
        stop_daemon(Path::new(".")).expect("nothing to wait for");
    }

    #[test]
    fn the_target_is_this_platforms_archive() {
        assert!(matches!(platform_arch(), "arm64" | "amd64"));
        assert_eq!(
            platform_os(),
            if cfg!(windows) { "windows" } else { "linux" }
        );
    }

    // ---- choosing the asset ----

    fn named(names: &[&str]) -> Release {
        Release {
            tag: "v4.4.4".into(),
            assets: names
                .iter()
                .map(|n| Asset {
                    name: n.to_string(),
                    url: format!(
                        "https://github.com/Kheopsian/Hydranos/releases/download/v4.4.4/{n}"
                    ),
                    size: 10,
                })
                .collect(),
        }
    }

    #[test]
    fn the_asset_is_the_one_for_this_os_and_architecture_in_this_platforms_format() {
        let (os, arch) = (platform_os(), platform_arch());
        let other_os = if os == "linux" { "windows" } else { "linux" };
        let other_arch = if arch == "amd64" { "arm64" } else { "amd64" };
        let ext = if cfg!(windows) { ".zip" } else { ".tar.gz" };
        let other_ext = if cfg!(windows) { ".tar.gz" } else { ".zip" };
        let right = format!("hydranos-v4.4.4-{os}-{arch}{ext}");
        let r = named(&[
            &format!("hydranos-v4.4.4-{other_os}-{arch}{ext}"),
            &format!("hydranos-v4.4.4-{os}-{other_arch}{ext}"),
            &format!("hydranos-v4.4.4-{os}-{arch}{other_ext}"),
            &format!("{right}.sha256"),
            &right,
        ]);
        assert_eq!(
            pick_asset(&r).unwrap().name,
            right,
            "not the checksum that sits beside it either"
        );
    }

    #[test]
    fn a_release_with_nothing_for_this_platform_lists_what_it_has() {
        let e = pick_asset(&named(&["hydranos-v4.4.4-plan9-mips.tar.gz", "notes.txt"]))
            .err()
            .expect("refused");
        assert!(e.contains("v4.4.4") && e.contains(platform_os()), "{e}");
        assert!(
            e.contains("hydranos-v4.4.4-plan9-mips.tar.gz, notes.txt"),
            "{e}"
        );
        let empty = pick_asset(&named(&[])).err().expect("refused");
        assert!(empty.contains("it has: "), "{empty}");
    }

    #[test]
    fn a_checksum_that_is_not_published_beside_the_archive_is_a_refusal() {
        let r = named(&["hydranos-v4.4.4-linux-amd64.tar.gz", "other.tar.gz.sha256"]);
        let e = expected_sha(&r, "hydranos-v4.4.4-linux-amd64.tar.gz").unwrap_err();
        assert!(
            e.contains("hydranos-v4.4.4-linux-amd64.tar.gz.sha256") && e.contains("unverified"),
            "{e}"
        );
    }

    // ---- the HTTP half, against a server on this machine ----

    /// One canned answer on a loopback port; the request it received comes
    /// back on the channel.
    fn serve(status: &str, body: Vec<u8>) -> (String, std::sync::mpsc::Receiver<String>) {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let (tx, rx) = std::sync::mpsc::channel();
        let status = status.to_string();
        std::thread::spawn(move || {
            let Ok((mut sock, _)) = listener.accept() else {
                return;
            };
            let mut req = Vec::new();
            let mut buf = [0u8; 1024];
            while !req.windows(4).any(|w| w == b"\r\n\r\n") {
                match sock.read(&mut buf) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => req.extend_from_slice(&buf[..n]),
                }
            }
            let _ = tx.send(String::from_utf8_lossy(&req).into_owned());
            let head = format!(
                "HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                body.len()
            );
            let _ = sock.write_all(head.as_bytes());
            let _ = sock.write_all(&body);
        });
        (url, rx)
    }

    #[test]
    fn a_text_is_fetched_with_this_tools_identity() {
        let (url, rx) = serve("200 OK", b"{\"tag_name\":\"v4.4.4\"}".to_vec());
        assert_eq!(
            get_text(&format!("{url}/repos/x/releases/latest")).unwrap(),
            "{\"tag_name\":\"v4.4.4\"}"
        );
        let req = rx.recv().unwrap().to_lowercase();
        assert!(req.starts_with("get /repos/x/releases/latest "), "{req}");
        assert!(req.contains("user-agent: hydranos-update"), "{req}");
    }

    #[test]
    fn an_http_error_is_an_error_with_its_status() {
        let (url, _rx) = serve("404 Not Found", b"{\"message\":\"Not Found\"}".to_vec());
        let e = get_text(&url).unwrap_err();
        assert!(e.contains("HTTP 404") && e.contains(&url), "{e}");
        let (url, _rx) = serve("503 Service Unavailable", Vec::new());
        let e = get_bytes(&url).unwrap_err();
        assert!(e.contains("HTTP 503"), "{e}");
    }

    #[test]
    fn an_archive_comes_back_byte_for_byte() {
        let payload: Vec<u8> = (0..=255u8).cycle().take(5000).collect();
        let (url, _rx) = serve("200 OK", payload.clone());
        assert_eq!(
            get_bytes(&url).unwrap(),
            payload,
            "not text: bytes that are not UTF-8 survive"
        );
    }

    #[test]
    fn a_server_that_is_not_there_is_an_error_naming_the_url() {
        let port = {
            let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            l.local_addr().unwrap().port()
        };
        let url = format!("http://127.0.0.1:{port}/x");
        let e = get_bytes(&url).unwrap_err();
        assert!(e.starts_with(&format!("GET {url}")), "{e}");
        assert!(get_text(&url).is_err());
    }

    // ---- what is installed ----

    #[cfg(unix)]
    fn fake_daemon(dir: &Path, script: &str) {
        use std::os::unix::fs::PermissionsExt;
        let exe = dir.join("hydranos");
        std::fs::write(&exe, script).unwrap();
        std::fs::set_permissions(&exe, std::fs::Permissions::from_mode(0o755)).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn the_installed_version_is_what_the_binary_says_it_is() {
        let dir = scratch("version");
        fake_daemon(&dir, "#!/bin/sh\necho \"hydranos 4.1.7\"\n");
        assert_eq!(installed_version(&dir).as_deref(), Some("4.1.7"));
        fake_daemon(&dir, "#!/bin/sh\necho\n");
        assert_eq!(
            installed_version(&dir),
            None,
            "a binary that says nothing has no version"
        );
        fake_daemon(&dir, "#!/bin/sh\necho hydranos\n");
        assert_eq!(
            installed_version(&dir),
            None,
            "a name with no number is not a version"
        );
        let _ = std::fs::remove_dir_all(&dir);
        assert_eq!(
            installed_version(&scratch("version-none").join("missing")),
            None,
            "no binary there"
        );
        let _ = std::fs::remove_dir_all(scratch("version-none"));
    }

    /// Only a whole sha256 is taken from a `.sha256` file: 64 hex digits,
    /// lowercased, the file name after it ignored.
    #[test]
    fn a_checksum_file_yields_only_a_whole_sha256() {
        let hex = "0123456789ABCDEF".repeat(4);
        assert_eq!(
            sha_from_text(&format!("{hex}  hydranos.tar.gz\n")),
            Some(hex.to_lowercase())
        );
        assert_eq!(sha_from_text(&hex[..63]), None, "one digit short");
        assert_eq!(sha_from_text(&format!("{hex}0")), None, "one digit long");
        assert_eq!(
            sha_from_text(&format!("{}g", &hex[..63])),
            None,
            "a letter that is not hex"
        );
        assert_eq!(sha_from_text(""), None, "an empty file");
    }
}
