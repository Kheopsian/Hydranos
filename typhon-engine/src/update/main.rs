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
    tag: Option<String>,
    check: bool,
    yes: bool,
}

fn parse() -> Args {
    let mut a = Args { dir: PathBuf::from("."), tag: None, check: false, yes: false };
    let mut it = std::env::args().skip(1);
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--dir" => {
                if let Some(v) = it.next() {
                    a.dir = PathBuf::from(v);
                }
            }
            "--tag" => a.tag = it.next(),
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
  --yes            do not ask before replacing

Nothing is replaced unless the archive downloads AND its SHA-256 matches the
checksum published beside it.";

fn run() -> Result<(), String> {
    let args = parse();
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
    println!("downloading {} ({:.1} MB)", asset.name, asset.size as f64 / 1e6);

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

fn resolve(tag: Option<&str>) -> Result<Release, String> {
    let url = match tag {
        Some(t) => format!("https://api.github.com/repos/{REPO}/releases/tags/{t}"),
        None => format!("https://api.github.com/repos/{REPO}/releases/latest"),
    };
    let body = get_text(&url)?;
    let v: serde_json::Value =
        serde_json::from_str(&body).map_err(|e| format!("the release index is not JSON: {e}"))?;
    if let Some(msg) = v.get("message").and_then(|m| m.as_str()) {
        return Err(format!("GitHub says: {msg}"));
    }
    let tag = v["tag_name"].as_str().ok_or("the release has no tag")?.to_string();
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

/// The archive for THIS platform.
fn pick_asset(r: &Release) -> Result<Asset, String> {
    let (os, arch) = (platform_os(), platform_arch());
    let want_ext = if cfg!(windows) { ".zip" } else { ".tar.gz" };
    r.assets
        .iter()
        .find(|a| {
            a.name.contains(os) && a.name.contains(arch) && a.name.ends_with(want_ext)
        })
        .cloned()
        .ok_or_else(|| {
            format!(
                "release {} publishes nothing for {os}-{arch}{want_ext}\n  it has: {}",
                r.tag,
                r.assets.iter().map(|a| a.name.as_str()).collect::<Vec<_>>().join(", ")
            )
        })
}

fn platform_os() -> &'static str {
    if cfg!(windows) { "windows" } else { "linux" }
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
    let a = r
        .assets
        .iter()
        .find(|a| a.name == name)
        .ok_or_else(|| format!("no {name} published beside the archive -- refusing to install unverified bytes"))?;
    let text = get_text(&a.url)?;
    // "<hex>  <filename>", the sha256sum format.
    text.split_whitespace()
        .next()
        .map(|s| s.to_lowercase())
        .filter(|s| s.len() == 64 && s.chars().all(|c| c.is_ascii_hexdigit()))
        .ok_or_else(|| format!("{name} does not hold a sha256"))
}

// ---------------------------------------------------------------------------
// HTTP
// ---------------------------------------------------------------------------

fn client() -> Result<reqwest::blocking::Client, String> {
    reqwest::blocking::Client::builder()
        .user_agent(UA)
        .timeout(std::time::Duration::from_secs(900))
        .build()
        .map_err(|e| format!("cannot build an HTTP client: {e}"))
}

fn get_text(url: &str) -> Result<String, String> {
    let r = client()?.get(url).send().map_err(|e| format!("GET {url}: {e}"))?;
    if !r.status().is_success() {
        return Err(format!("GET {url}: HTTP {}", r.status()));
    }
    r.text().map_err(|e| format!("reading {url}: {e}"))
}

fn get_bytes(url: &str) -> Result<Vec<u8>, String> {
    let r = client()?.get(url).send().map_err(|e| format!("GET {url}: {e}"))?;
    if !r.status().is_success() {
        return Err(format!("GET {url}: HTTP {}", r.status()));
    }
    Ok(r.bytes().map_err(|e| format!("reading {url}: {e}"))?.to_vec())
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
        "hydranos" | "hydranos.exe" | "hydranos-engine" | "hydranos-engine.exe"
            | "hydranos-update" | "hydranos-update.exe"
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
            let mut f = zip.by_index(i).map_err(|e| format!("reading the zip: {e}"))?;
            let Some(base) = f.name().rsplit(['/', '\\']).next().map(str::to_string) else {
                continue;
            };
            if !is_ours(&base) {
                continue;
            }
            let mut buf = Vec::new();
            f.read_to_end(&mut buf).map_err(|e| format!("extracting {base}: {e}"))?;
            out.push((base, buf));
        }
    } else {
        let gz = flate2::read::GzDecoder::new(bytes);
        let mut tar = tar::Archive::new(gz);
        for entry in tar.entries().map_err(|e| format!("reading the tarball: {e}"))? {
            let mut entry = entry.map_err(|e| format!("reading the tarball: {e}"))?;
            let path = entry.path().map_err(|e| format!("a tar entry has no path: {e}"))?;
            let Some(base) = path.file_name().and_then(|s| s.to_str()).map(str::to_string) else {
                continue;
            };
            if !is_ours(&base) {
                continue;
            }
            let mut buf = Vec::new();
            entry.read_to_end(&mut buf).map_err(|e| format!("extracting {base}: {e}"))?;
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
        // /T so the engine goes with it. No /F: that is the kill this is
        // trying to avoid.
        let _ = std::process::Command::new("taskkill")
            .args(["/IM", "hydranos.exe", "/T"])
            .output();
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
            return Err(format!("writing {}: {e} -- the old binary is back", target.display()));
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
    let exe = dir.join(if cfg!(windows) { "hydranos.exe" } else { "hydranos" });
    let out = std::process::Command::new(&exe).arg("--version").output().ok()?;
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
    Ok(matches!(line.trim().to_lowercase().as_str(), "y" | "yes" | "o" | "oui"))
}
