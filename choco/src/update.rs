//! `choco update` (#84 part 3): replaces `choco` and `chocofactoryd` in the
//! directory `install.sh` put them in, with a release from GitHub.
//!
//! Order, and what a failure at each step leaves behind:
//! 1. marker / manifest / in-flight checks: read-only, nothing changed.
//! 2. download, checksum, extract, version check into `<dir>/.choco-update-<pid>/`:
//!    only that temp dir exists, and it is removed on every exit path.
//! 3. stop the daemon (only one running from `<dir>`): the old binaries are
//!    still in place; a failed stop leaves them untouched.
//! 4. rename `chocofactoryd`, then `choco`, into `<dir>` (each an atomic
//!    replace). A failure of the second leaves new daemon + old choco.
//! 5. rewrite the marker (temp file + rename), then restart the daemon on its
//!    old port from `<dir>`.

use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::Duration;

use chocofactory_core::daemon_lock::{LockInfo, LockState, read_lock};
use chocofactory_core::paths::config_root;
use chocofactory_core::version::VERSION;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use crate::client::Client;
use crate::server::{Failure, start_daemon, stop_daemon};

const DEFAULT_RELEASES: &str = "https://github.com/itsypkin/ChocoFactory/releases";
const HTTP_TIMEOUT: Duration = Duration::from_secs(120);
const STATUS_TIMEOUT: Duration = Duration::from_secs(5);
const EXIT_REFUSED: u8 = 3;

pub async fn run(check: bool, version: Option<String>, force: bool) -> ExitCode {
    match update(check, version, force).await {
        Ok(code) => ExitCode::from(code),
        Err(msg) => {
            eprintln!("error: {msg}");
            ExitCode::FAILURE
        }
    }
}

struct Marker {
    dir: PathBuf,
    target: String,
    raw: Value,
}

/// Removes the temp directory on every exit path.
struct TempDir(PathBuf);

impl Drop for TempDir {
    fn drop(&mut self) {
        // Best effort in a destructor: there is no caller left to tell.
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn read_marker(path: &Path) -> Option<Marker> {
    let text = std::fs::read_to_string(path).ok()?;
    let raw: Value = serde_json::from_str(&text).ok()?;
    let dir = PathBuf::from(raw.get("dir")?.as_str()?);
    let target = raw.get("target")?.as_str()?.to_string();
    raw.get("version")?.as_str()?;
    Some(Marker { dir, target, raw })
}

fn canon(p: &Path) -> PathBuf {
    std::fs::canonicalize(p).unwrap_or_else(|_| p.to_path_buf())
}

/// The refusal text when this choco is not an `install.sh` install, or
/// `None` when the marker names this choco's own directory.
fn refusal(marker: Option<&Marker>, exe: &Path, releases: &str) -> Option<String> {
    let exe_dir = exe.parent().map(canon).unwrap_or_default();
    if let Some(m) = marker
        && canon(&m.dir) == exe_dir
    {
        return None;
    }
    let shown = exe.display().to_string();
    if shown.contains("/target/debug/") || shown.contains("/target/release/") {
        return Some(format!(
            "this choco is a source build ({shown}); update it with `git pull && cargo build --workspace`, then `choco server restart`"
        ));
    }
    let cargo_bin = std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".cargo").join("bin"));
    if let Some(cb) = cargo_bin
        && (exe.starts_with(&cb) || exe.starts_with(canon(&cb)))
    {
        return Some(
            "this choco was installed with cargo; update with: cargo install --git https://github.com/itsypkin/ChocoFactory --locked --force choco chocofactoryd"
                .to_string(),
        );
    }
    if let Some(m) = marker {
        return Some(format!(
            "this choco is at {}, but the installed copy is at {}; run {}/choco update",
            exe_dir.display(),
            m.dir.display(),
            m.dir.display()
        ));
    }
    Some(format!(
        "choco wasn't installed by install.sh, so it can't update itself; reinstall with: curl -fsSL {releases}/latest/download/install.sh | sh"
    ))
}

async fn get(http: &reqwest::Client, url: &str) -> Result<Vec<u8>, Failure> {
    let resp = http
        .get(url)
        .send()
        .await
        .map_err(|e| format!("could not fetch {url}: {e}"))?;
    let status = resp.status();
    if !status.is_success() {
        return Err(format!("could not fetch {url}: HTTP {status}"));
    }
    resp.bytes()
        .await
        .map(|b| b.to_vec())
        .map_err(|e| format!("could not read {url}: {e}"))
}

fn sha256_hex(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

fn checksum_for(sums: &str, asset: &str) -> Option<String> {
    sums.lines().find_map(|line| {
        let mut parts = line.split_whitespace();
        let hash = parts.next()?;
        let name = parts.next()?;
        (name == asset || name.strip_prefix('*') == Some(asset)).then(|| hash.to_string())
    })
}

fn write_atomically(path: &Path, text: &str) -> Result<(), Failure> {
    let tmp = path.with_extension(format!("json.{}", std::process::id()));
    std::fs::write(&tmp, text).map_err(|e| format!("cannot write {}: {e}", tmp.display()))?;
    std::fs::rename(&tmp, path)
        .map_err(|e| format!("cannot rename {} to {}: {e}", tmp.display(), path.display()))
}

/// Fetches, verifies and unpacks the release into `tmp`; returns the
/// directory holding the new binaries.
async fn fetch_release(
    http: &reqwest::Client,
    releases: &str,
    version: &str,
    target: &str,
    tmp: &Path,
) -> Result<PathBuf, Failure> {
    let asset = format!("chocofactory-{target}.tar.gz");
    let base = format!("{releases}/download/v{version}");
    let archive_url = format!("{base}/{asset}");
    let sums_url = format!("{base}/SHA256SUMS");
    let archive = get(http, &archive_url).await?;
    let sums = String::from_utf8(get(http, &sums_url).await?)
        .map_err(|_| format!("{sums_url} is not text"))?;
    let expected = checksum_for(&sums, &asset)
        .ok_or_else(|| format!("no checksum for {asset} in {sums_url}"))?;
    let actual = sha256_hex(&archive);
    if expected != actual {
        return Err(format!(
            "checksum mismatch for {archive_url} (expected {expected}, got {actual})"
        ));
    }
    std::fs::create_dir(tmp).map_err(|e| format!("cannot create {}: {e}", tmp.display()))?;
    let archive_path = tmp.join(&asset);
    std::fs::write(&archive_path, &archive)
        .map_err(|e| format!("cannot write {}: {e}", archive_path.display()))?;
    let out = tmp.join("x");
    std::fs::create_dir(&out).map_err(|e| format!("cannot create {}: {e}", out.display()))?;
    let status = std::process::Command::new("tar")
        .arg("-xzf")
        .arg(&archive_path)
        .arg("-C")
        .arg(&out)
        .status()
        .map_err(|e| format!("cannot run tar on {}: {e}", archive_path.display()))?;
    if !status.success() {
        return Err(format!(
            "could not extract {} ({status})",
            archive_path.display()
        ));
    }
    let src = out.join(format!("chocofactory-{target}"));
    for b in ["choco", "chocofactoryd"] {
        if !src.join(b).is_file() {
            return Err(format!(
                "the downloaded archive has no {b} (looked for {})",
                src.join(b).display()
            ));
        }
    }
    let reported = std::process::Command::new(src.join("chocofactoryd"))
        .arg("--version")
        .output()
        .map_err(|e| {
            format!(
                "cannot run {} --version: {e}",
                src.join("chocofactoryd").display()
            )
        })?;
    let reported = String::from_utf8_lossy(&reported.stdout).trim().to_string();
    if !reported.contains(version) {
        return Err(format!(
            "the downloaded chocofactoryd reports {reported}, expected {version}"
        ));
    }
    Ok(src)
}

fn running_from(dir: &Path, info: &LockInfo) -> bool {
    Path::new(&info.exe).parent().map(canon).as_deref() == Some(dir)
}

async fn update(check: bool, want: Option<String>, force: bool) -> Result<u8, Failure> {
    let root = config_root().ok_or("HOME is not set")?;
    let marker_path = root.join("install.json");
    let exe = canon(&std::env::current_exe().map_err(|e| format!("cannot locate choco: {e}"))?);
    let releases = std::env::var("CHOCO_RELEASES_URL")
        .unwrap_or_else(|_| DEFAULT_RELEASES.to_string())
        .trim_end_matches('/')
        .to_string();
    let marker = read_marker(&marker_path);
    if let Some(msg) = refusal(marker.as_ref(), &exe, &releases) {
        eprintln!("error: {msg}");
        return Ok(1);
    }
    let marker = marker.ok_or("internal error: no marker")?;
    let dir = canon(&marker.dir);

    if let Some(v) = &want
        && (v.is_empty()
            || !v
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || ".-+".contains(c)))
    {
        return Err(format!("invalid --version '{v}'"));
    }
    let http = reqwest::Client::builder()
        .timeout(HTTP_TIMEOUT)
        .build()
        .map_err(|e| format!("cannot build an HTTP client: {e}"))?;
    let manifest_url = match &want {
        Some(v) => format!("{releases}/download/v{v}/manifest.json"),
        None => format!("{releases}/latest/download/manifest.json"),
    };
    let body = get(&http, &manifest_url).await?;
    let manifest: Value = serde_json::from_slice(&body)
        .map_err(|e| format!("{manifest_url} is not valid JSON: {e}"))?;
    let new_version = manifest
        .get("version")
        .and_then(Value::as_str)
        .ok_or_else(|| format!("{manifest_url} has no \"version\""))?
        .to_string();

    let same = new_version == VERSION;
    if check {
        if same {
            println!("choco is up to date ({VERSION})");
        } else {
            println!("update available: {VERSION} → {new_version}");
        }
        return Ok(0);
    }
    if same && !force {
        println!("choco is up to date ({VERSION})");
        return Ok(0);
    }

    let lock = read_lock(&root).map_err(|e| format!("could not read the daemon lock file: {e}"))?;
    let mut daemon_here = false;
    if let LockState::Running(info) = &lock {
        if running_from(&dir, info) {
            daemon_here = true;
            if !force {
                let server = Client::new(format!("http://127.0.0.1:{}", info.port))
                    .server_status(STATUS_TIMEOUT)
                    .await
                    .map_err(|e| {
                        format!(
                            "chocofactoryd (pid {}) holds the lock but isn't answering on port {}: {e}; pass --force to update anyway",
                            info.pid, info.port
                        )
                    })?;
                if !server.in_flight.is_empty() {
                    let mut text = String::from(
                        "error: chocofactoryd is in the middle of work that updating would interrupt:\n",
                    );
                    for f in &server.in_flight {
                        text.push_str(&format!(
                            "  {}  {} ({})  {}\n",
                            f.task_id,
                            f.stage,
                            f.kind,
                            crate::render::single_line(&f.title)
                        ));
                    }
                    text.push_str(
                        "Tasks waiting on a poll or a human are not affected. Wait for these to move on\n\
                         (`choco task status <id>`), or pass --force to update anyway: they are marked stuck,\n\
                         and `choco task retry <id>` continues them (an agent turn resumes its session).",
                    );
                    println!("{text}");
                    return Ok(EXIT_REFUSED);
                }
            }
        } else {
            println!(
                "note: the running chocofactoryd is {} (not in {}); leaving it alone",
                info.exe,
                dir.display()
            );
        }
    }

    let tmp_path = dir.join(format!(".choco-update-{}", std::process::id()));
    let _guard = TempDir(tmp_path.clone());
    let src = fetch_release(&http, &releases, &new_version, &marker.target, &tmp_path).await?;

    // Stop the daemon with the same code as `choco server stop --force`. Re-read the
    // lock: the daemon may have gone, or been replaced, since the check above.
    let mut old_port = None;
    if daemon_here {
        let fresh =
            read_lock(&root).map_err(|e| format!("could not read the daemon lock file: {e}"))?;
        if let LockState::Running(info) = fresh
            && running_from(&dir, &info)
        {
            let port = info.port;
            let code = stop_daemon(&root, info, true).await?;
            if code != 0
                && matches!(
                    read_lock(&root)
                        .map_err(|e| format!("could not read the daemon lock file: {e}"))?,
                    LockState::Running(_)
                )
            {
                return Err("chocofactoryd could not be stopped; nothing was replaced".to_string());
            }
            old_port = Some(port);
        }
    }

    let new_daemon = dir.join("chocofactoryd");
    let replaced = replace_binaries(&src, &dir, &new_version);
    // Whatever happened, a daemon we stopped comes back (the old one if nothing was replaced).
    let marker_result = if replaced.is_ok() {
        let mut raw = marker.raw.clone();
        raw["version"] = json!(new_version);
        write_atomically(&marker_path, &raw.to_string())
    } else {
        Ok(())
    };
    let restart_result = match old_port {
        Some(port) => start_daemon(&root, &new_daemon, Some(port))
            .await
            .map(|_| ()),
        None => Ok(()),
    };
    replaced?;
    marker_result?;
    restart_result.map_err(|e| format!("updated, but chocofactoryd did not restart: {e}"))?;
    println!("updated {VERSION} → {new_version}");
    Ok(0)
}

fn replace_binaries(src: &Path, dir: &Path, new_version: &str) -> Result<(), Failure> {
    let daemon_to = dir.join("chocofactoryd");
    std::fs::rename(src.join("chocofactoryd"), &daemon_to).map_err(|e| {
        format!(
            "cannot replace {}: {e}; nothing was replaced",
            daemon_to.display()
        )
    })?;
    let choco_to = dir.join("choco");
    std::fs::rename(src.join("choco"), &choco_to).map_err(|e| {
        format!(
            "chocofactoryd is now {new_version}, choco is still {VERSION}; rerun `choco update --force` (cannot replace {}: {e})",
            choco_to.display()
        )
    })
}
