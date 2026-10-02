//! `GET /server` (#84): what this daemon is, and what a restart would
//! strand. Static facts are captured once at startup in [`ServerInfo`];
//! everything else is read per request.

use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use axum::Json;
use axum::extract::State;
use chocofactory_core::models::ServerStatus;
use chrono::{DateTime, Utc};

use super::{ApiError, AppState};
use crate::db::tasks;

/// The daemon executable as it was when the daemon started: enough to tell
/// later whether the file on disk has been replaced underneath it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExeStamp {
    pub path: PathBuf,
    pub inode: u64,
    pub modified: SystemTime,
}

impl ExeStamp {
    pub fn capture() -> std::io::Result<ExeStamp> {
        let path = std::env::current_exe()?;
        let meta = std::fs::metadata(&path)?;
        Ok(ExeStamp {
            inode: meta.ino(),
            modified: meta.modified()?,
            path,
        })
    }

    /// `true` if the file is gone or its inode or mtime changed. A stat
    /// error other than "not found" is reported as replaced, with a warning
    /// naming it, rather than hidden.
    pub fn replaced(&self) -> bool {
        match std::fs::metadata(&self.path) {
            Ok(meta) => {
                meta.ino() != self.inode
                    || match meta.modified() {
                        Ok(modified) => modified != self.modified,
                        Err(err) => {
                            tracing::warn!(path = %self.path.display(), %err, "could not read the daemon executable's mtime");
                            true
                        }
                    }
            }
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => true,
            Err(err) => {
                tracing::warn!(path = %self.path.display(), %err, "could not stat the daemon executable");
                true
            }
        }
    }
}

/// Facts fixed at startup.
#[derive(Debug, Clone)]
pub struct ServerInfo {
    pub pid: u32,
    pub port: u16,
    pub started_at: DateTime<Utc>,
    pub config_root: PathBuf,
    /// `None` when the stamp could not be taken at startup.
    pub exe: Option<ExeStamp>,
    pub choco_binary: String,
}

pub async fn get(State(state): State<AppState>) -> Result<Json<ServerStatus>, ApiError> {
    let info = &state.server;
    let tasks = tasks::count_by_status(&state.pool).await?;
    let in_flight = state.engine.in_flight().await?;
    Ok(Json(ServerStatus {
        version: chocofactory_core::version::VERSION.to_string(),
        commit: chocofactory_core::version::BUILD_COMMIT.map(str::to_string),
        pid: info.pid,
        port: info.port,
        started_at: info.started_at,
        config_root: info.config_root.display().to_string(),
        exe: info
            .exe
            .as_ref()
            .map(|e| e.path.display().to_string())
            .unwrap_or_default(),
        exe_replaced: info.exe.as_ref().map(ExeStamp::replaced),
        choco_binary: info.choco_binary.clone(),
        choco_binary_found: Path::new(&info.choco_binary).is_file(),
        tasks,
        in_flight,
    }))
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::api::tests::TestServer;

    const SHELL_WORKFLOW: &str = r#"
name: sh
stages:
  run:
    kind: shell
    command: "sleep 600"
    on:
      done: finished
  finished:
    kind: terminal
"#;

    async fn make_task(server: &TestServer, workflow: &str) -> String {
        let dir = server.temp_dir();
        let project = server
            .post(
                "/projects",
                json!({"name": "p", "repo_path": dir.display().to_string()}),
            )
            .await;
        assert_eq!(project.status(), 201, "{:?}", project.json());
        let task = server
            .post(
                "/tasks",
                json!({"project_id": project.json()["id"], "workflow_def": workflow, "title": "T", "prompt": "go"}),
            )
            .await;
        assert_eq!(task.status(), 201, "{:?}", task.json());
        task.json()["id"].as_str().unwrap().to_string()
    }

    #[tokio::test]
    async fn get_server_reports_identity_counts_and_choco_binary() {
        let existing = std::env::current_exe().unwrap().display().to_string();
        let server = TestServer::start_with_binaries("fake_claude.py", &existing).await;
        server.write_workflow("sh", SHELL_WORKFLOW);
        let id = make_task(&server, "sh").await;

        let resp = server.get("/server").await;
        assert_eq!(resp.status(), 200);
        let body = resp.json();
        assert_eq!(body["version"], chocofactory_core::version::VERSION);
        assert_eq!(body["pid"], std::process::id());
        assert!(body["port"].as_u64().unwrap() > 0);
        assert_eq!(body["tasks"]["open"], 1);
        assert_eq!(body["choco_binary"], existing);
        assert_eq!(body["choco_binary_found"], true);
        assert_eq!(body["exe_replaced"], false);
        assert_eq!(body["in_flight"][0]["task_id"], id);
        assert_eq!(body["in_flight"][0]["stage"], "run");
        assert_eq!(body["in_flight"][0]["kind"], "shell");
        // The wire shape deserializes into the shared type.
        let _: ServerStatus = serde_json::from_value(body).unwrap();

        let missing = TestServer::start_with_binaries("fake_claude.py", "/no/such/choco").await;
        assert_eq!(
            missing.get("/server").await.json()["choco_binary_found"],
            false
        );
        let bare = TestServer::start_with_binaries("fake_claude.py", "choco").await;
        assert_eq!(
            bare.get("/server").await.json()["choco_binary_found"],
            false
        );
    }

    #[tokio::test]
    async fn every_response_carries_the_version_header() {
        let server = TestServer::start().await;
        let want = Some(chocofactory_core::version::VERSION.to_string());
        let ok = server.get("/server").await;
        assert_eq!(ok.status(), 200);
        assert_eq!(ok.header("x-chocofactory-version"), want);
        let not_found = server.get("/no/such/route").await;
        assert_eq!(not_found.status(), 404);
        assert_eq!(not_found.header("x-chocofactory-version"), want);
        let api_error = server.get("/tasks/nope").await;
        assert!(api_error.status() >= 400);
        assert_eq!(api_error.header("x-chocofactory-version"), want);
    }

    #[test]
    fn exe_stamp_detects_a_replaced_file() {
        let dir = std::env::temp_dir().join(format!("choco-exe-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("bin");
        std::fs::write(&path, "one").unwrap();
        let meta = std::fs::metadata(&path).unwrap();
        let stamp = ExeStamp {
            path: path.clone(),
            inode: meta.ino(),
            modified: meta.modified().unwrap(),
        };
        assert!(!stamp.replaced());
        // A new file at the same path has a new inode.
        std::fs::write(dir.join("other"), "two").unwrap();
        std::fs::rename(dir.join("other"), &path).unwrap();
        assert!(stamp.replaced());
        std::fs::remove_file(&path).unwrap();
        assert!(stamp.replaced());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
