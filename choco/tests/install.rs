//! Release, install and update (#84 part 3), against the real scripts and the
//! real `choco`/`chocofactoryd` in `target/debug/`.
//!
//! Safety rules, per test: its own temporary `HOME`; an explicit
//! `CHOCO_INSTALL_DIR` inside it (asserted before `install.sh` runs); every
//! command that could fetch gets `CHOCO_RELEASES_URL` pointing at the
//! in-test server (or an unreachable local port), never github.com; daemons
//! use `--port 0` and `mock-claude`/a fixture, and are signalled only by the
//! pid in the temp lock.

use std::collections::HashMap;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use axum::Router;
use axum::extract::State;
use axum::http::{StatusCode, Uri};
use axum::response::{IntoResponse, Response};
use chocofactory_core::daemon_lock::{LockInfo, LockState, read_lock};
use chocofactory_core::version::VERSION;
use serde_json::Value;
use sha2::{Digest, Sha256};

static UNIQUE: AtomicU64 = AtomicU64::new(0);
const DEADLINE: Duration = Duration::from_secs(60);
/// Never reachable: stands in for "no server" without ever touching the network.
const NO_SERVER: &str = "http://127.0.0.1:1";

fn target_dir() -> PathBuf {
    let mut path = std::env::current_exe().unwrap();
    path.pop();
    if path.ends_with("deps") {
        path.pop();
    }
    path
}

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .to_path_buf()
}

fn host_target() -> &'static str {
    match (std::env::consts::OS, std::env::consts::ARCH) {
        ("macos", "aarch64") => "aarch64-apple-darwin",
        ("macos", "x86_64") => "x86_64-apple-darwin",
        ("linux", "x86_64") => "x86_64-unknown-linux-musl",
        ("linux", "aarch64") => "aarch64-unknown-linux-musl",
        other => panic!("unsupported host {other:?}"),
    }
}

fn text(out: &Output) -> String {
    format!(
        "status: {:?}\nstdout: {}\nstderr: {}",
        out.status.code(),
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    )
}

fn stdout(out: &Output) -> String {
    String::from_utf8_lossy(&out.stdout).into_owned()
}

fn stderr(out: &Output) -> String {
    String::from_utf8_lossy(&out.stderr).into_owned()
}

fn wait_for(what: &str, mut f: impl FnMut() -> bool) {
    let end = Instant::now() + DEADLINE;
    while Instant::now() < end {
        if f() {
            return;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    panic!("timed out waiting for {what}");
}

fn sha(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

fn package(target: &str, bin_dir: &Path, out_dir: &Path) -> Output {
    Command::new("sh")
        .arg(repo_root().join("scripts/package-release.sh"))
        .args([target])
        .arg(bin_dir)
        .arg(out_dir)
        .output()
        .unwrap()
}

/// The archive of the current `target/debug` binaries, built once.
fn archive_bytes() -> &'static Vec<u8> {
    static ARCHIVE: OnceLock<Vec<u8>> = OnceLock::new();
    ARCHIVE.get_or_init(|| {
        let out_dir =
            std::env::temp_dir().join(format!("choco-install-archive-{}", std::process::id()));
        let out = package(host_target(), &target_dir(), &out_dir);
        assert!(out.status.success(), "{}", text(&out));
        let bytes =
            std::fs::read(out_dir.join(format!("chocofactory-{}.tar.gz", host_target()))).unwrap();
        std::fs::remove_dir_all(&out_dir).unwrap();
        bytes
    })
}

fn asset() -> String {
    format!("chocofactory-{}.tar.gz", host_target())
}

fn sums_for(archive: &[u8]) -> Vec<u8> {
    format!("{}  {}\n", sha(archive), asset()).into_bytes()
}

// ---- the fixture release server ------------------------------------------

#[derive(Clone, Default)]
struct Shared {
    files: Arc<Mutex<HashMap<String, Vec<u8>>>>,
    log: Arc<Mutex<Vec<String>>>,
}

async fn handle(State(st): State<Shared>, uri: Uri) -> Response {
    let path = uri.path().to_string();
    st.log.lock().unwrap().push(path.clone());
    match st.files.lock().unwrap().get(&path) {
        Some(bytes) => bytes.clone().into_response(),
        None => StatusCode::NOT_FOUND.into_response(),
    }
}

struct Server {
    url: String,
    shared: Shared,
}

impl Server {
    fn new() -> Self {
        let shared = Shared::default();
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let port = listener.local_addr().unwrap().port();
        let app = Router::new().fallback(handle).with_state(shared.clone());
        std::thread::spawn(move || {
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap()
                .block_on(async {
                    let listener = tokio::net::TcpListener::from_std(listener).unwrap();
                    axum::serve(listener, app).await.unwrap();
                });
        });
        Server {
            url: format!("http://127.0.0.1:{port}"),
            shared,
        }
    }

    fn put(&self, path: &str, bytes: Vec<u8>) {
        self.shared
            .files
            .lock()
            .unwrap()
            .insert(path.to_string(), bytes);
    }

    /// Serves a release under `/latest/download/` (`tag` None) or `/download/v<tag>/`.
    fn release(&self, tag: Option<&str>, archive: &[u8], sums: Vec<u8>, manifest_version: &str) {
        let base = match tag {
            Some(v) => format!("/download/v{v}"),
            None => "/latest/download".to_string(),
        };
        self.put(&format!("{base}/{}", asset()), archive.to_vec());
        self.put(&format!("{base}/SHA256SUMS"), sums);
        self.put(
            &format!("{base}/manifest.json"),
            format!(r#"{{"version":"{manifest_version}","commit":"abc1234"}}"#).into_bytes(),
        );
    }

    fn requests(&self) -> Vec<String> {
        self.shared.log.lock().unwrap().clone()
    }
}

// ---- a temp HOME with an install dir --------------------------------------

struct Env {
    home: PathBuf,
    bin: PathBuf,
    claude: PathBuf,
}

impl Env {
    fn new() -> Self {
        let n = UNIQUE.fetch_add(1, Ordering::Relaxed);
        let home =
            std::env::temp_dir().join(format!("choco-install-test-{}-{n}", std::process::id()));
        std::fs::create_dir_all(&home).unwrap();
        let home = std::fs::canonicalize(home).unwrap();
        let bin = home.join("bin");
        Env {
            home,
            bin,
            claude: target_dir().join("mock-claude"),
        }
    }

    fn root(&self) -> PathBuf {
        self.home.join(".config").join("chocofactory")
    }

    fn marker(&self) -> PathBuf {
        self.root().join("install.json")
    }

    fn base_cmd(&self, program: impl AsRef<std::ffi::OsStr>, url: &str) -> Command {
        let mut cmd = Command::new(program);
        cmd.env("HOME", &self.home)
            .env("CHOCO_RELEASES_URL", url)
            .env("CHOCOFACTORY_CLAUDE_BINARY", &self.claude)
            .env("RUST_LOG", "error")
            .env_remove("CHOCO_BASE_URL")
            .env_remove("CHOCO_VERSION")
            .env_remove("CHOCO_INSTALL_ARCHIVE")
            .env_remove("CHOCO_INSTALL_DIR");
        cmd
    }

    fn install_cmd(&self, url: &str) -> Command {
        assert!(
            self.bin.starts_with(&self.home),
            "install dir must be inside the temp HOME"
        );
        let mut cmd = self.base_cmd("sh", url);
        cmd.arg(repo_root().join("install.sh"))
            .env("CHOCO_INSTALL_DIR", &self.bin);
        cmd
    }

    fn install_from_archive(&self) -> Output {
        let archive = self.home.join("archive.tar.gz");
        std::fs::write(&archive, archive_bytes()).unwrap();
        let out = self
            .install_cmd(NO_SERVER)
            .env("CHOCO_INSTALL_ARCHIVE", &archive)
            .output()
            .unwrap();
        assert!(out.status.success(), "{}", text(&out));
        out
    }

    fn choco(&self, url: &str, args: &[&str]) -> Output {
        self.base_cmd(self.bin.join("choco"), url)
            .args(args)
            .output()
            .unwrap()
    }

    fn inodes(&self) -> (u64, u64) {
        (
            std::fs::metadata(self.bin.join("choco")).unwrap().ino(),
            std::fs::metadata(self.bin.join("chocofactoryd"))
                .unwrap()
                .ino(),
        )
    }

    fn leftovers(&self) -> Vec<String> {
        std::fs::read_dir(&self.bin)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .filter(|n| n.starts_with(".choco-"))
            .collect()
    }

    fn lock(&self) -> LockState {
        read_lock(&self.root()).unwrap()
    }

    fn running(&self) -> LockInfo {
        match self.lock() {
            LockState::Running(info) => info,
            other => panic!("expected a running daemon, got {other:?}"),
        }
    }

    fn start_daemon(&self) -> LockInfo {
        let out = self.choco(NO_SERVER, &["server", "start", "--port", "0"]);
        assert!(out.status.success(), "{}", text(&out));
        let info = self.running();
        assert_ne!(info.port, 4141, "test daemon must not use the real port");
        info
    }

    fn json(&self, args: &[&str]) -> Value {
        let base = format!("http://127.0.0.1:{}", self.running().port);
        let mut a = vec!["--json", "--base-url", base.as_str()];
        a.extend_from_slice(args);
        let out = self.choco(NO_SERVER, &a);
        assert!(out.status.success(), "{args:?} failed: {}", text(&out));
        serde_json::from_slice(&out.stdout).unwrap()
    }

    fn set_marker_version(&self, version: &str) {
        let mut m: Value =
            serde_json::from_str(&std::fs::read_to_string(self.marker()).unwrap()).unwrap();
        m["version"] = Value::from(version);
        std::fs::write(self.marker(), m.to_string()).unwrap();
    }
}

impl Drop for Env {
    fn drop(&mut self) {
        if let Ok(LockState::Running(info)) = read_lock(&self.root()) {
            unsafe { libc::kill(info.pid as i32, libc::SIGKILL) };
        }
        let _ = std::fs::remove_dir_all(&self.home);
    }
}

// ---- 1. package-release.sh -------------------------------------------------

#[test]
fn package_release_lists_exactly_the_five_files_with_executable_binaries() {
    let tmp = Env::new();
    let out_dir = tmp.home.join("out");
    let out = package("t-1", &target_dir(), &out_dir);
    assert!(out.status.success(), "{}", text(&out));
    let archive = out_dir.join("chocofactory-t-1.tar.gz");
    let list = Command::new("tar")
        .arg("-tzf")
        .arg(&archive)
        .output()
        .unwrap();
    let mut entries: Vec<String> = stdout(&list)
        .lines()
        .filter(|l| !l.ends_with('/'))
        .map(str::to_string)
        .collect();
    entries.sort();
    let mut want: Vec<String> = [
        "choco",
        "chocofactoryd",
        "README.md",
        "LICENSE-MIT",
        "LICENSE-APACHE",
    ]
    .iter()
    .map(|f| format!("chocofactory-t-1/{f}"))
    .collect();
    want.sort();
    assert_eq!(entries, want);
    let x = tmp.home.join("x");
    std::fs::create_dir(&x).unwrap();
    let st = Command::new("tar")
        .arg("-xzf")
        .arg(&archive)
        .arg("-C")
        .arg(&x)
        .status()
        .unwrap();
    assert!(st.success());
    for b in ["choco", "chocofactoryd"] {
        let mode = std::fs::metadata(x.join("chocofactory-t-1").join(b))
            .unwrap()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o755, "{b}");
    }
}

#[test]
fn package_release_fails_when_a_binary_or_license_is_missing() {
    let tmp = Env::new();
    // A binary dir without `choco`.
    let bins = tmp.home.join("bins");
    std::fs::create_dir(&bins).unwrap();
    std::fs::copy(
        target_dir().join("chocofactoryd"),
        bins.join("chocofactoryd"),
    )
    .unwrap();
    let out = package("t-2", &bins, &tmp.home.join("out"));
    assert!(!out.status.success(), "{}", text(&out));
    assert!(stderr(&out).contains("choco"), "{}", text(&out));
    // A repo copy without LICENSE-APACHE.
    let fake = tmp.home.join("fake");
    std::fs::create_dir_all(fake.join("scripts")).unwrap();
    std::fs::copy(
        repo_root().join("scripts/package-release.sh"),
        fake.join("scripts/package-release.sh"),
    )
    .unwrap();
    for f in ["README.md", "LICENSE-MIT"] {
        std::fs::copy(repo_root().join(f), fake.join(f)).unwrap();
    }
    let out = Command::new("sh")
        .arg(fake.join("scripts/package-release.sh"))
        .args(["t-3"])
        .arg(target_dir())
        .arg(tmp.home.join("out2"))
        .output()
        .unwrap();
    assert!(!out.status.success(), "{}", text(&out));
    assert!(stderr(&out).contains("LICENSE-APACHE"), "{}", text(&out));
}

// ---- 2-7. install.sh -------------------------------------------------------

#[test]
fn install_from_the_server_puts_both_binaries_side_by_side() {
    let env = Env::new();
    let server = Server::new();
    let archive = archive_bytes();
    server.release(None, archive, sums_for(archive), VERSION);
    let out = env.install_cmd(&server.url).output().unwrap();
    assert!(out.status.success(), "{}", text(&out));
    assert!(
        stdout(&out).contains(&format!(
            "installed chocofactory {VERSION} to {}",
            env.bin.display()
        )),
        "{}",
        text(&out)
    );
    for b in ["choco", "chocofactoryd"] {
        let mode = std::fs::metadata(env.bin.join(b)).unwrap().mode();
        assert_eq!(mode & 0o111, 0o111, "{b} must be executable");
    }
    let marker: Value =
        serde_json::from_str(&std::fs::read_to_string(env.marker()).unwrap()).unwrap();
    assert_eq!(marker["channel"], "github-release");
    assert_eq!(marker["dir"], env.bin.to_str().unwrap());
    assert_eq!(marker["version"], VERSION);
    assert_eq!(marker["target"], host_target());
    assert!(
        server
            .requests()
            .iter()
            .any(|p| p == &format!("/latest/download/{}", asset()))
    );
}

#[test]
fn install_with_a_corrupted_checksum_touches_nothing() {
    let env = Env::new();
    let server = Server::new();
    let archive = archive_bytes();
    server.release(
        None,
        archive,
        format!("{}  {}\n", "0".repeat(64), asset()).into_bytes(),
        VERSION,
    );
    let out = env.install_cmd(&server.url).output().unwrap();
    assert!(!out.status.success(), "{}", text(&out));
    assert!(stderr(&out).contains("checksum mismatch"), "{}", text(&out));
    assert!(!env.bin.exists(), "install dir must not be created");
    assert!(!env.marker().exists());
}

#[test]
fn install_with_a_missing_checksum_line_touches_nothing() {
    let env = Env::new();
    let server = Server::new();
    server.release(
        None,
        archive_bytes(),
        b"abc  some-other-file.tar.gz\n".to_vec(),
        VERSION,
    );
    let out = env.install_cmd(&server.url).output().unwrap();
    assert!(!out.status.success(), "{}", text(&out));
    assert!(stderr(&out).contains("no checksum"), "{}", text(&out));
    assert!(!env.bin.exists());
}

#[test]
fn pinned_version_requests_the_versioned_download_path() {
    let env = Env::new();
    let server = Server::new();
    let archive = archive_bytes();
    server.release(Some("0.2.0"), archive, sums_for(archive), VERSION);
    let out = env
        .install_cmd(&server.url)
        .env("CHOCO_VERSION", "0.2.0")
        .output()
        .unwrap();
    assert!(out.status.success(), "{}", text(&out));
    let log = server.requests();
    assert!(
        log.contains(&format!("/download/v0.2.0/{}", asset())),
        "{log:?}"
    );
    assert!(
        log.contains(&"/download/v0.2.0/SHA256SUMS".to_string()),
        "{log:?}"
    );
    assert!(log.iter().all(|p| !p.contains("latest")), "{log:?}");
}

#[test]
fn unsupported_platform_is_refused() {
    let env = Env::new();
    let fake = env.home.join("fakebin");
    std::fs::create_dir_all(&fake).unwrap();
    let uname = fake.join("uname");
    std::fs::write(
        &uname,
        "#!/bin/sh\ncase \"$1\" in -s) echo FreeBSD ;; -m) echo amd64 ;; esac\n",
    )
    .unwrap();
    std::fs::set_permissions(&uname, std::fs::Permissions::from_mode(0o755)).unwrap();
    let out = env
        .install_cmd(NO_SERVER)
        .env("PATH", format!("{}:/usr/bin:/bin", fake.display()))
        .output()
        .unwrap();
    assert!(!out.status.success(), "{}", text(&out));
    assert!(
        stderr(&out).contains("unsupported platform: FreeBSD amd64"),
        "{}",
        text(&out)
    );
    assert!(!env.bin.exists());
}

#[test]
fn reinstall_replaces_by_rename_and_leaves_no_temp_files() {
    let env = Env::new();
    env.install_from_archive();
    let before = env.inodes();
    env.install_from_archive();
    let after = env.inodes();
    assert_ne!(before.0, after.0, "choco was overwritten in place");
    assert_ne!(before.1, after.1, "chocofactoryd was overwritten in place");
    assert_eq!(env.leftovers(), Vec::<String>::new());
}

#[test]
fn symlinked_install_dir_on_path_gets_no_path_hint() {
    let env = Env::new();
    let real = env.home.join("real-bin");
    std::fs::create_dir_all(&real).unwrap();
    let link = env.home.join("link-bin");
    std::os::unix::fs::symlink(&real, &link).unwrap();
    assert!(link.starts_with(&env.home));
    let archive = env.home.join("archive.tar.gz");
    std::fs::write(&archive, archive_bytes()).unwrap();
    let path = format!("{}:{}", link.display(), std::env::var("PATH").unwrap());
    let out = env
        .install_cmd(NO_SERVER)
        .env("CHOCO_INSTALL_ARCHIVE", &archive)
        .env("CHOCO_INSTALL_DIR", &link)
        .env("PATH", path)
        .output()
        .unwrap();
    assert!(out.status.success(), "{}", text(&out));
    assert!(!text(&out).contains("not on your PATH"), "{}", text(&out));
}

#[test]
fn install_from_a_local_archive_needs_no_server() {
    let env = Env::new();
    let out = env.install_from_archive();
    assert!(
        stdout(&out).contains(&format!("installed chocofactory {VERSION}")),
        "{}",
        text(&out)
    );
    assert!(env.bin.join("choco").is_file() && env.bin.join("chocofactoryd").is_file());
}

// ---- 8-12. choco update ----------------------------------------------------

#[test]
fn update_refuses_without_a_marker() {
    let env = Env::new();
    std::fs::create_dir_all(&env.bin).unwrap();
    for b in ["choco", "chocofactoryd"] {
        std::fs::copy(target_dir().join(b), env.bin.join(b)).unwrap();
    }
    let out = env.choco(NO_SERVER, &["update"]);
    assert_eq!(out.status.code(), Some(1), "{}", text(&out));
    assert!(
        stderr(&out).contains("wasn't installed by install.sh"),
        "{}",
        text(&out)
    );
    assert!(stderr(&out).contains(&format!(
        "curl -fsSL {NO_SERVER}/latest/download/install.sh | sh"
    )));
}

#[test]
fn update_refuses_from_a_source_build() {
    let env = Env::new();
    let out = env
        .base_cmd(target_dir().join("choco"), NO_SERVER)
        .arg("update")
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(1), "{}", text(&out));
    assert!(stderr(&out).contains("is a source build"), "{}", text(&out));
    assert!(
        stderr(&out).contains("git pull && cargo build --workspace"),
        "{}",
        text(&out)
    );
}

#[test]
fn update_refuses_when_the_marker_names_a_different_dir() {
    let env = Env::new();
    env.install_from_archive();
    let other = env.home.join("other");
    std::fs::create_dir_all(&other).unwrap();
    let mut m: Value =
        serde_json::from_str(&std::fs::read_to_string(env.marker()).unwrap()).unwrap();
    m["dir"] = Value::from(other.to_str().unwrap());
    std::fs::write(env.marker(), m.to_string()).unwrap();
    let out = env.choco(NO_SERVER, &["update"]);
    assert_eq!(out.status.code(), Some(1), "{}", text(&out));
    let err = stderr(&out);
    assert!(err.contains("but the installed copy is at"), "{err}");
    assert!(
        err.contains(&format!("run {}/choco update", other.display())),
        "{err}"
    );
}

#[test]
fn update_to_the_same_version_and_check_change_nothing() {
    let env = Env::new();
    env.install_from_archive();
    let server = Server::new();
    server.put(
        "/latest/download/manifest.json",
        format!(r#"{{"version":"{VERSION}","commit":"x"}}"#).into_bytes(),
    );
    let before = env.inodes();
    let out = env.choco(&server.url, &["update"]);
    assert!(out.status.success(), "{}", text(&out));
    assert!(
        stdout(&out).contains(&format!("choco is up to date ({VERSION})")),
        "{}",
        text(&out)
    );
    assert_eq!(env.inodes(), before);

    server.put(
        "/latest/download/manifest.json",
        br#"{"version":"9.9.9","commit":"x"}"#.to_vec(),
    );
    let out = env.choco(&server.url, &["update", "--check"]);
    assert!(out.status.success(), "{}", text(&out));
    assert!(
        stdout(&out).contains(&format!("update available: {VERSION} → 9.9.9")),
        "{}",
        text(&out)
    );
    assert_eq!(env.inodes(), before);
    assert!(
        server.requests().iter().all(|p| !p.ends_with(".tar.gz")),
        "{:?}",
        server.requests()
    );
}

#[test]
fn forced_update_replaces_binaries_rewrites_the_marker_and_restarts_the_daemon() {
    let env = Env::new();
    env.install_from_archive();
    let server = Server::new();
    let archive = archive_bytes();
    server.release(Some(VERSION), archive, sums_for(archive), VERSION);
    server.put(
        "/latest/download/manifest.json",
        format!(r#"{{"version":"{VERSION}","commit":"x"}}"#).into_bytes(),
    );
    let old = env.start_daemon();
    env.set_marker_version("0.0.1");
    let before = env.inodes();
    let out = env.choco(&server.url, &["update", "--force"]);
    assert!(out.status.success(), "{}", text(&out));
    assert!(
        stdout(&out).contains(&format!("updated {VERSION} → {VERSION}")),
        "{}",
        text(&out)
    );
    let after = env.inodes();
    assert_ne!(before.0, after.0);
    assert_ne!(before.1, after.1);
    let marker: Value =
        serde_json::from_str(&std::fs::read_to_string(env.marker()).unwrap()).unwrap();
    assert_eq!(marker["version"], VERSION);
    let new = env.running();
    assert_ne!(new.pid, old.pid);
    assert_eq!(new.port, old.port);
    assert_eq!(
        std::fs::canonicalize(&new.exe).unwrap(),
        env.bin.join("chocofactoryd")
    );
    assert_eq!(env.leftovers(), Vec::<String>::new());
    assert!(std::fs::read_dir(&env.bin).unwrap().all(|e| {
        !e.unwrap()
            .file_name()
            .to_string_lossy()
            .starts_with(".choco-update")
    }));
}

fn blocking_claude(home: &Path) -> PathBuf {
    let path = home.join("blocking-claude.sh");
    std::fs::write(&path, "#!/bin/sh\nexec sleep 600\n").unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    path
}

const AGENT_WF: &str = r#"
name: blocking
roles:
  worker:
    cli: claude
    model: claude-sonnet-5-5
stages:
  work:
    kind: agent_turn
    role: worker
    on: { ok: done }
  done:
    kind: terminal
"#;

fn start_turn_in_flight(env: &Env) {
    let wf = env.home.join("wf.yaml");
    std::fs::write(&wf, AGENT_WF).unwrap();
    let repo = env.home.join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    env.json(&[
        "project",
        "create",
        "demo",
        "--repo",
        repo.to_str().unwrap(),
    ]);
    env.json(&[
        "task",
        "create",
        "--project",
        "demo",
        "--workflow",
        wf.to_str().unwrap(),
        "--title",
        "t",
        "--prompt",
        "go",
    ]);
    wait_for("an in-flight turn", || {
        let out = env.choco(NO_SERVER, &["--json", "server", "status"]);
        serde_json::from_slice::<Value>(&out.stdout)
            .map(|v| {
                !v["daemon"]["in_flight"]
                    .as_array()
                    .is_none_or(|a| a.is_empty())
            })
            .unwrap_or(false)
    });
}

#[test]
fn update_with_work_in_flight_is_refused_before_any_download_unless_forced() {
    let mut env = Env::new();
    env.claude = blocking_claude(&env.home);
    env.install_from_archive();
    let server = Server::new();
    let archive = archive_bytes();
    server.put(
        "/latest/download/manifest.json",
        br#"{"version":"9.9.9","commit":"x"}"#.to_vec(),
    );
    env.start_daemon();
    start_turn_in_flight(&env);
    let before = env.inodes();
    let pid = env.running().pid;

    let out = env.choco(&server.url, &["update"]);
    assert_eq!(out.status.code(), Some(3), "{}", text(&out));
    assert!(
        stdout(&out).contains("pass --force to update anyway"),
        "{}",
        text(&out)
    );
    assert!(
        server.requests().iter().all(|p| !p.ends_with(".tar.gz")),
        "{:?}",
        server.requests()
    );
    assert_eq!(env.inodes(), before);
    assert_eq!(env.running().pid, pid);

    // --force with the same version proceeds, and the daemon comes back.
    server.release(Some(VERSION), archive, sums_for(archive), VERSION);
    server.put(
        "/latest/download/manifest.json",
        format!(r#"{{"version":"{VERSION}","commit":"x"}}"#).into_bytes(),
    );
    let out = env.choco(&server.url, &["update", "--force"]);
    assert!(out.status.success(), "{}", text(&out));
    assert_ne!(env.inodes(), before);
    assert_ne!(env.running().pid, pid);
}

#[test]
fn update_to_a_version_the_archive_does_not_hold_changes_nothing() {
    let env = Env::new();
    env.install_from_archive();
    let server = Server::new();
    let archive = archive_bytes();
    server.release(Some("9.9.9"), archive, sums_for(archive), "9.9.9");
    server.put(
        "/latest/download/manifest.json",
        br#"{"version":"9.9.9","commit":"x"}"#.to_vec(),
    );
    let old = env.start_daemon();
    let before = env.inodes();
    let out = env.choco(&server.url, &["update"]);
    assert_eq!(out.status.code(), Some(1), "{}", text(&out));
    assert!(stderr(&out).contains("expected 9.9.9"), "{}", text(&out));
    assert!(
        stderr(&out).contains("reports chocofactoryd"),
        "{}",
        text(&out)
    );
    assert_eq!(env.inodes(), before);
    assert_eq!(env.running().pid, old.pid, "the daemon must keep running");
    assert_eq!(env.leftovers(), Vec::<String>::new());
    assert!(std::fs::read_dir(&env.bin).unwrap().all(|e| {
        !e.unwrap()
            .file_name()
            .to_string_lossy()
            .starts_with(".choco-update")
    }));
}

#[test]
fn update_with_a_checksum_mismatch_replaces_nothing() {
    let env = Env::new();
    env.install_from_archive();
    let server = Server::new();
    server.release(
        Some("9.9.9"),
        archive_bytes(),
        format!("{}  {}\n", "f".repeat(64), asset()).into_bytes(),
        "9.9.9",
    );
    server.put(
        "/latest/download/manifest.json",
        br#"{"version":"9.9.9","commit":"x"}"#.to_vec(),
    );
    let before = env.inodes();
    let out = env.choco(&server.url, &["update"]);
    assert_eq!(out.status.code(), Some(1), "{}", text(&out));
    assert!(stderr(&out).contains("checksum mismatch"), "{}", text(&out));
    assert_eq!(env.inodes(), before);
    assert!(std::fs::read_dir(&env.bin).unwrap().all(|e| {
        !e.unwrap()
            .file_name()
            .to_string_lossy()
            .starts_with(".choco-update")
    }));
}

#[test]
fn update_names_the_manifest_url_when_it_cannot_be_fetched() {
    let env = Env::new();
    env.install_from_archive();
    let server = Server::new();
    let out = env.choco(&server.url, &["update"]);
    assert_eq!(out.status.code(), Some(1), "{}", text(&out));
    assert!(
        stderr(&out).contains(&format!("{}/latest/download/manifest.json", server.url)),
        "{}",
        text(&out)
    );
}

// ---- 13. release-smoke.sh --------------------------------------------------

fn no_update_dirs(env: &Env) -> bool {
    std::fs::read_dir(&env.bin).unwrap().all(|e| {
        !e.unwrap()
            .file_name()
            .to_string_lossy()
            .starts_with(".choco-update")
    })
}

#[test]
fn update_with_a_version_reads_the_versioned_manifest_not_latest() {
    let env = Env::new();
    env.install_from_archive();
    let server = Server::new();
    let archive = archive_bytes();
    server.release(Some(VERSION), archive, sums_for(archive), VERSION);
    let before = env.inodes();
    let out = env.choco(&server.url, &["update", "--version", VERSION, "--force"]);
    assert!(out.status.success(), "{}", text(&out));
    let reqs = server.requests();
    assert!(
        reqs.contains(&format!("/download/v{VERSION}/manifest.json")),
        "{reqs:?}"
    );
    assert!(reqs.iter().all(|p| !p.contains("latest")), "{reqs:?}");
    assert_ne!(env.inodes(), before);

    // An invalid version is refused before any request.
    let n = server.requests().len();
    let out = env.choco(&server.url, &["update", "--version", "a/b"]);
    assert_eq!(out.status.code(), Some(1), "{}", text(&out));
    assert!(stderr(&out).contains("invalid --version"), "{}", text(&out));
    assert_eq!(server.requests().len(), n);
}

#[test]
fn update_leaves_a_daemon_running_from_another_dir_alone() {
    let env = Env::new();
    env.install_from_archive();
    let other = env.home.join("other");
    std::fs::create_dir_all(&other).unwrap();
    for b in ["choco", "chocofactoryd"] {
        std::fs::copy(target_dir().join(b), other.join(b)).unwrap();
    }
    let out = env
        .base_cmd(other.join("choco"), NO_SERVER)
        .args(["server", "start", "--port", "0"])
        .output()
        .unwrap();
    assert!(out.status.success(), "{}", text(&out));
    let old = env.running();
    assert_ne!(old.port, 4141);
    let server = Server::new();
    let archive = archive_bytes();
    server.release(Some(VERSION), archive, sums_for(archive), VERSION);
    server.put(
        "/latest/download/manifest.json",
        format!(r#"{{"version":"{VERSION}","commit":"x"}}"#).into_bytes(),
    );
    let out = env.choco(&server.url, &["update", "--force"]);
    assert!(out.status.success(), "{}", text(&out));
    assert!(stdout(&out).contains("leaving it alone"), "{}", text(&out));
    assert_eq!(env.running().pid, old.pid);
}

#[test]
fn install_prints_the_path_and_running_daemon_hints() {
    let env = Env::new();
    env.install_from_archive();
    env.start_daemon();
    let archive = env.home.join("archive.tar.gz");
    let out = env
        .install_cmd(NO_SERVER)
        .env("CHOCO_INSTALL_ARCHIVE", &archive)
        .output()
        .unwrap();
    assert!(out.status.success(), "{}", text(&out));
    let so = stdout(&out);
    assert!(
        so.contains("restart the running daemon to use the new version: choco server restart"),
        "{so}"
    );
    assert!(
        so.contains(&format!("{} is not on your PATH", env.bin.display())),
        "{so}"
    );
    // With the dir on PATH there is no PATH hint.
    let path = format!(
        "{}:{}",
        env.bin.display(),
        std::env::var("PATH").unwrap_or_default()
    );
    let out = env
        .install_cmd(NO_SERVER)
        .env("CHOCO_INSTALL_ARCHIVE", &archive)
        .env("PATH", path)
        .output()
        .unwrap();
    assert!(out.status.success(), "{}", text(&out));
    assert!(!stdout(&out).contains("not on your PATH"), "{}", text(&out));
}

#[test]
fn update_refuses_a_cargo_install() {
    let env = Env::new();
    let cb = env.home.join(".cargo").join("bin");
    std::fs::create_dir_all(&cb).unwrap();
    std::fs::copy(target_dir().join("choco"), cb.join("choco")).unwrap();
    let out = env
        .base_cmd(cb.join("choco"), NO_SERVER)
        .arg("update")
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(1), "{}", text(&out));
    assert!(
        stderr(&out).contains("installed with cargo"),
        "{}",
        text(&out)
    );
}

#[test]
fn update_reports_bad_manifests_missing_checksums_and_up_to_date_checks() {
    let env = Env::new();
    env.install_from_archive();
    let server = Server::new();
    let before = env.inodes();

    server.put("/latest/download/manifest.json", b"not json".to_vec());
    let out = env.choco(&server.url, &["update"]);
    assert_eq!(out.status.code(), Some(1), "{}", text(&out));
    assert!(stderr(&out).contains("not valid JSON"), "{}", text(&out));

    server.put(
        "/latest/download/manifest.json",
        br#"{"commit":"x"}"#.to_vec(),
    );
    let out = env.choco(&server.url, &["update"]);
    assert_eq!(out.status.code(), Some(1), "{}", text(&out));
    assert!(
        stderr(&out).contains("has no \"version\""),
        "{}",
        text(&out)
    );

    // A checksum file with no line for this asset.
    server.release(
        Some("9.9.9"),
        archive_bytes(),
        b"abc  other.tar.gz\n".to_vec(),
        "9.9.9",
    );
    server.put(
        "/latest/download/manifest.json",
        br#"{"version":"9.9.9","commit":"x"}"#.to_vec(),
    );
    let out = env.choco(&server.url, &["update"]);
    assert_eq!(out.status.code(), Some(1), "{}", text(&out));
    assert!(stderr(&out).contains("no checksum for"), "{}", text(&out));
    assert!(no_update_dirs(&env));

    server.put(
        "/latest/download/manifest.json",
        format!(r#"{{"version":"{VERSION}","commit":"x"}}"#).into_bytes(),
    );
    let out = env.choco(&server.url, &["update", "--check"]);
    assert!(out.status.success(), "{}", text(&out));
    assert!(
        stdout(&out).contains(&format!("choco is up to date ({VERSION})")),
        "{}",
        text(&out)
    );
    assert_eq!(env.inodes(), before);
}

#[test]
fn release_smoke_passes_on_a_packaged_archive() {
    let env = Env::new();
    let archive = env.home.join("smoke.tar.gz");
    std::fs::write(&archive, archive_bytes()).unwrap();
    let out = Command::new("sh")
        .arg(repo_root().join("scripts/release-smoke.sh"))
        .arg(&archive)
        .env("HOME", &env.home)
        .env("TMPDIR", &env.home)
        .env("EXPECT_VERSION", VERSION)
        .env_remove("CHOCO_BASE_URL")
        .output()
        .unwrap();
    assert!(out.status.success(), "{}", text(&out));
    assert!(stdout(&out).contains("release-smoke: ok"), "{}", text(&out));
}

// ---- 14. release.yml -------------------------------------------------------

#[test]
fn release_workflow_has_the_expected_shape() {
    let text = std::fs::read_to_string(repo_root().join(".github/workflows/release.yml")).unwrap();
    let wf: serde_yaml::Value = serde_yaml::from_str(&text).unwrap();
    let on = &wf["on"];
    assert_eq!(on["push"]["tags"][0].as_str(), Some("v*"));
    assert!(on.get("workflow_dispatch").is_some());
    let jobs = wf["jobs"].as_mapping().unwrap();
    let names: Vec<&str> = jobs.keys().map(|k| k.as_str().unwrap()).collect();
    assert_eq!(names, ["verify", "test", "build", "publish"]);
    let needs = |j: &str| wf["jobs"][j]["needs"].as_str().map(str::to_string);
    assert_eq!(needs("test").as_deref(), Some("verify"));
    assert_eq!(needs("build").as_deref(), Some("test"));
    assert_eq!(needs("publish").as_deref(), Some("build"));
    let mut targets: Vec<String> = wf["jobs"]["build"]["strategy"]["matrix"]["include"]
        .as_sequence()
        .unwrap()
        .iter()
        .map(|e| e["target"].as_str().unwrap().to_string())
        .collect();
    targets.sort();
    assert_eq!(
        targets,
        [
            "aarch64-apple-darwin",
            "aarch64-unknown-linux-musl",
            "x86_64-apple-darwin",
            "x86_64-unknown-linux-musl"
        ]
    );
    assert_eq!(
        wf["jobs"]["publish"]["permissions"]["contents"].as_str(),
        Some("write")
    );
    for j in ["verify", "test", "build"] {
        assert!(
            wf["jobs"][j].get("permissions").is_none(),
            "{j} must not have write permissions"
        );
    }
    assert!(wf.get("permissions").is_none());
    assert!(
        wf["jobs"]["publish"]["if"]
            .as_str()
            .unwrap()
            .contains("refs/tags/")
    );
    // Tag must equal v + workspace version; prerelease flag for hyphenated tags.
    assert!(text.contains(r#"[ "$GITHUB_REF_NAME" != "v$version" ]"#));
    assert!(text.contains(r#"case "$TAG" in *-*) prerelease="--prerelease""#));
    // musl builds need an explicit C compiler name.
    assert!(text.contains("musl-gcc") && text.contains("CC_"));
}

// ---- 15. license -----------------------------------------------------------

#[test]
fn every_crate_inherits_the_dual_license() {
    let root = repo_root();
    let manifest = std::fs::read_to_string(root.join("Cargo.toml")).unwrap();
    assert!(manifest.contains(r#"license = "MIT OR Apache-2.0""#));
    let out = Command::new(std::env::var("CARGO").unwrap_or_else(|_| "cargo".into()))
        .args(["metadata", "--no-deps", "--format-version", "1"])
        .current_dir(&root)
        .output()
        .unwrap();
    assert!(out.status.success(), "{}", text(&out));
    let meta: Value = serde_json::from_slice(&out.stdout).unwrap();
    let packages = meta["packages"].as_array().unwrap();
    assert_eq!(packages.len(), 4);
    for p in packages {
        assert_eq!(p["license"], "MIT OR Apache-2.0", "{}", p["name"]);
    }
    for f in ["LICENSE-MIT", "LICENSE-APACHE"] {
        assert!(std::fs::metadata(root.join(f)).unwrap().len() > 0, "{f}");
    }
}

#[test]
fn install_rejects_an_unrepresentable_dir_before_creating_it() {
    let env = Env::new();
    let archive = env.home.join("archive.tar.gz");
    std::fs::write(&archive, archive_bytes()).unwrap();
    for name in ["a\"b", "a\\b", "a\tb", "a\nb"] {
        let dir = env.home.join(name);
        assert!(dir.starts_with(&env.home));
        let out = env
            .install_cmd(NO_SERVER)
            .env("CHOCO_INSTALL_ARCHIVE", &archive)
            .env("CHOCO_INSTALL_DIR", &dir)
            .output()
            .unwrap();
        assert!(!out.status.success(), "{}", text(&out));
        assert!(text(&out).contains("cannot hold"), "{}", text(&out));
        assert!(!dir.exists(), "{name:?} was created");
    }
}
