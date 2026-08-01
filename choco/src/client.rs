//! HTTP client for `chokofactoryd`'s API (P1-9, design §6.2). One method
//! per endpoint; request bodies are built with `serde_json::json!()` rather
//! than importing `chokofactoryd`'s own request structs — it's a bin-only
//! crate, and depending on the daemon's internal API-layer types here would
//! be backwards coupling.

use std::fmt;

use chokofactory_core::models::{Event, Project, Task};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

/// One page of `GET /tasks/{id}/events`. Defined here rather than imported
/// because the daemon's `EventsPage` lives in its bin-only API layer;
/// `Event` itself does come from `chokofactory-core`.
#[derive(Debug, Serialize, Deserialize)]
pub struct EventsPage {
    pub events: Vec<Event>,
    /// `Some` iff more events may follow — pass it back as `--after`.
    pub next_token: Option<String>,
}

/// Mirrors this repo's hand-rolled `Display`-impl error convention
/// (`ApiError`, `EngineError`, ...) rather than pulling in `anyhow`/`thiserror`.
#[derive(Debug)]
pub enum ClientError {
    /// The request never got a response — connection refused, DNS, TLS,
    /// timeout, etc. Carries the underlying `reqwest::Error` text rather
    /// than discarding it, so a failure that isn't actually "daemon is
    /// down" (e.g. a malformed `--base-url`) still shows its real cause.
    Connect { base_url: String, source: String },
    /// Daemon responded with a 4xx/5xx; carries its `{"error": ...}` body.
    Api(String),
    /// Response body wasn't the JSON shape expected.
    Decode(String),
    /// `--project <name>` matched no project.
    NoSuchProject(String),
    /// `--project <name>` matched several — `projects.name` has no unique
    /// constraint (`0001_init.sql`), so this is a reachable state, not a
    /// theoretical one. Resolved by passing the id instead.
    AmbiguousProject { name: String, ids: Vec<String> },
}

impl fmt::Display for ClientError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ClientError::Connect { base_url, source } => write!(
                f,
                "failed to connect to chokofactoryd at {base_url} (is it running?): {source}"
            ),
            ClientError::Api(message) => write!(f, "{message}"),
            ClientError::Decode(msg) => write!(f, "unexpected response from chokofactoryd: {msg}"),
            ClientError::NoSuchProject(name) => {
                write!(f, "no project named '{name}' (try `choco project list`)")
            }
            ClientError::AmbiguousProject { name, ids } => write!(
                f,
                "'{name}' matches {} projects — pass one of these ids instead: {}",
                ids.len(),
                ids.join(", ")
            ),
        }
    }
}

pub struct Client {
    http: reqwest::Client,
    base_url: String,
}

impl Client {
    pub fn new(base_url: String) -> Self {
        Client {
            http: reqwest::Client::new(),
            base_url,
        }
    }

    async fn send(
        &self,
        builder: reqwest::RequestBuilder,
    ) -> Result<reqwest::Response, ClientError> {
        builder.send().await.map_err(|err| ClientError::Connect {
            base_url: self.base_url.clone(),
            source: err.to_string(),
        })
    }

    /// Turns a non-2xx response into `ClientError::Api`, extracting the
    /// `{"error": "..."}` body `api/error.rs` always sends. `Ok(resp)` is
    /// returned unchanged for 2xx.
    async fn check_status(
        &self,
        resp: reqwest::Response,
    ) -> Result<reqwest::Response, ClientError> {
        if resp.status().is_success() {
            return Ok(resp);
        }
        let status = resp.status();
        let body: Value = resp.json().await.unwrap_or(Value::Null);
        // The daemon always sends `{"error": "..."}` (`api/error.rs`), and
        // that message is self-contained enough to show verbatim. Anything
        // else on this path didn't come from the daemon — a proxy's HTML
        // 502, say — so fall back to the status code rather than a bare
        // "unknown error" that gives the user nothing to act on.
        let message = match body.get("error").and_then(Value::as_str) {
            Some(message) => message.to_string(),
            None => format!("chokofactoryd returned HTTP {status}"),
        };
        Err(ClientError::Api(message))
    }

    async fn decode<T: serde::de::DeserializeOwned>(
        &self,
        resp: reqwest::Response,
    ) -> Result<T, ClientError> {
        resp.json()
            .await
            .map_err(|err| ClientError::Decode(err.to_string()))
    }

    pub async fn create_project(&self, name: &str) -> Result<Project, ClientError> {
        let resp = self
            .send(
                self.http
                    .post(format!("{}/projects", self.base_url))
                    .json(&json!({ "name": name })),
            )
            .await?;
        let resp = self.check_status(resp).await?;
        self.decode(resp).await
    }

    pub async fn list_projects(&self) -> Result<Vec<Project>, ClientError> {
        let resp = self
            .send(self.http.get(format!("{}/projects", self.base_url)))
            .await?;
        let resp = self.check_status(resp).await?;
        self.decode(resp).await
    }

    /// Resolves a user-supplied `--project` value, which may be either an
    /// id or a name, to an id.
    ///
    /// Tries id first so an exact id always wins, then falls back to an
    /// exact name match. `projects.name` carries no unique constraint, so a
    /// name matching several projects is an error naming the candidates
    /// rather than an arbitrary pick.
    pub async fn resolve_project(&self, name_or_id: &str) -> Result<String, ClientError> {
        let projects = self.list_projects().await?;
        if projects.iter().any(|p| p.id == name_or_id) {
            return Ok(name_or_id.to_string());
        }
        let matches: Vec<&Project> = projects.iter().filter(|p| p.name == name_or_id).collect();
        match matches.as_slice() {
            [only] => Ok(only.id.clone()),
            [] => Err(ClientError::NoSuchProject(name_or_id.to_string())),
            many => Err(ClientError::AmbiguousProject {
                name: name_or_id.to_string(),
                ids: many.iter().map(|p| p.id.clone()).collect(),
            }),
        }
    }

    pub async fn list_events(
        &self,
        id: &str,
        limit: Option<usize>,
        after: Option<&str>,
    ) -> Result<EventsPage, ClientError> {
        let mut query: Vec<(&str, String)> = Vec::new();
        if let Some(limit) = limit {
            query.push(("limit", limit.to_string()));
        }
        if let Some(after) = after {
            query.push(("after", after.to_string()));
        }
        let resp = self
            .send(
                self.http
                    .get(format!("{}/tasks/{id}/events", self.base_url))
                    .query(&query),
            )
            .await?;
        let resp = self.check_status(resp).await?;
        self.decode(resp).await
    }

    /// Builds the `POST /tasks` request body — split out from
    /// [`Self::create_task`] so the `--repo` -> `config.cwd` mapping is
    /// unit-testable without a network round trip.
    fn create_task_request(
        &self,
        project_id: &str,
        workflow_def: &str,
        title: &str,
        prompt: &str,
        repo: Option<&str>,
        parent_task_id: Option<&str>,
    ) -> reqwest::RequestBuilder {
        let config = repo.map(|cwd| json!({ "cwd": cwd }));
        self.http
            .post(format!("{}/tasks", self.base_url))
            .json(&json!({
                "project_id": project_id,
                "workflow_def": workflow_def,
                "title": title,
                "prompt": prompt,
                "config": config,
                "parent_task_id": parent_task_id,
            }))
    }

    #[allow(clippy::too_many_arguments)]
    pub async fn create_task(
        &self,
        project_id: &str,
        workflow_def: &str,
        title: &str,
        prompt: &str,
        repo: Option<&str>,
        parent_task_id: Option<&str>,
    ) -> Result<Task, ClientError> {
        let resp = self
            .send(self.create_task_request(
                project_id,
                workflow_def,
                title,
                prompt,
                repo,
                parent_task_id,
            ))
            .await?;
        let resp = self.check_status(resp).await?;
        self.decode(resp).await
    }

    /// Returns the raw `TaskDetail` JSON (`Task` fields flattened +
    /// `workflow_state`) rather than a typed struct — `TaskDetail` isn't
    /// exported from `chokofactory-core`, and the raw shape is exactly what
    /// a delegating agent needs to poll `workflow_state.current_stage`.
    pub async fn get_task(&self, id: &str) -> Result<Value, ClientError> {
        let resp = self
            .send(self.http.get(format!("{}/tasks/{id}", self.base_url)))
            .await?;
        let resp = self.check_status(resp).await?;
        self.decode(resp).await
    }

    /// Builds the `GET /tasks` query pairs — omitting a filter entirely
    /// rather than sending it as the literal string `"null"` — split out
    /// from [`Self::list_tasks`] so it's unit-testable directly.
    fn list_tasks_query<'a>(
        project_id: Option<&'a str>,
        status: Option<&'a str>,
    ) -> Vec<(&'static str, &'a str)> {
        let mut query = Vec::new();
        if let Some(project_id) = project_id {
            query.push(("project_id", project_id));
        }
        if let Some(status) = status {
            query.push(("status", status));
        }
        query
    }

    pub async fn list_tasks(
        &self,
        project_id: Option<&str>,
        status: Option<&str>,
    ) -> Result<Vec<Task>, ClientError> {
        let query = Self::list_tasks_query(project_id, status);
        let resp = self
            .send(
                self.http
                    .get(format!("{}/tasks", self.base_url))
                    .query(&query),
            )
            .await?;
        let resp = self.check_status(resp).await?;
        self.decode(resp).await
    }

    pub async fn send_message(&self, id: &str, text: &str) -> Result<(), ClientError> {
        let resp = self
            .send(
                self.http
                    .post(format!("{}/tasks/{id}/messages", self.base_url))
                    .json(&json!({ "text": text })),
            )
            .await?;
        self.check_status(resp).await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn client() -> Client {
        Client::new("http://127.0.0.1:4141".to_string())
    }

    #[test]
    fn create_task_includes_config_cwd_when_repo_is_given() {
        let c = client();
        let req = c
            .create_task_request("p", "chat", "t", "hi", Some("/repo"), None)
            .build()
            .unwrap();
        let body: Value = serde_json::from_slice(req.body().unwrap().as_bytes().unwrap()).unwrap();
        assert_eq!(body["config"]["cwd"], "/repo");
    }

    #[test]
    fn create_task_sends_null_config_when_repo_is_absent() {
        let c = client();
        let req = c
            .create_task_request("p", "chat", "t", "hi", None, None)
            .build()
            .unwrap();
        let body: Value = serde_json::from_slice(req.body().unwrap().as_bytes().unwrap()).unwrap();
        assert!(body["config"].is_null());
    }

    #[test]
    fn create_task_includes_parent_task_id_when_given() {
        let c = client();
        let req = c
            .create_task_request("p", "chat", "t", "hi", None, Some("parent-1"))
            .build()
            .unwrap();
        let body: Value = serde_json::from_slice(req.body().unwrap().as_bytes().unwrap()).unwrap();
        assert_eq!(body["parent_task_id"], "parent-1");
    }

    #[test]
    fn list_tasks_query_omits_absent_filters_rather_than_sending_null() {
        assert_eq!(Client::list_tasks_query(None, None), Vec::new());
        assert_eq!(
            Client::list_tasks_query(Some("p"), None),
            vec![("project_id", "p")]
        );
        assert_eq!(
            Client::list_tasks_query(Some("p"), Some("open")),
            vec![("project_id", "p"), ("status", "open")]
        );
        assert_eq!(
            Client::list_tasks_query(None, Some("open")),
            vec![("status", "open")]
        );
    }
}
