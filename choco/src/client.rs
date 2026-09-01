//! HTTP client for `chocofactoryd`'s API (P1-9, design §6.2). One method
//! per endpoint; request bodies are built with `serde_json::json!()` rather
//! than importing `chocofactoryd`'s own request structs — it's a bin-only
//! crate, and depending on the daemon's internal API-layer types here would
//! be backwards coupling.

use std::fmt;

use chocofactory_core::models::{Event, Project, Task};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

/// One page of `GET /tasks/{id}/events`. Defined here rather than imported
/// because the daemon's `EventsPage` lives in its bin-only API layer;
/// `Event` itself does come from `chocofactory-core`.
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
    /// A `--role-*`/`--config` flag couldn't be turned into task config.
    /// Always reported rather than skipped: silently dropping an override
    /// would run the task on a model the caller didn't ask for.
    InvalidConfig(String),
}

impl fmt::Display for ClientError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ClientError::Connect { base_url, source } => write!(
                f,
                "failed to connect to chocofactoryd at {base_url} (is it running?): {source}"
            ),
            ClientError::Api(message) => write!(f, "{message}"),
            ClientError::Decode(msg) => write!(f, "unexpected response from chocofactoryd: {msg}"),
            ClientError::NoSuchProject(name) => {
                write!(f, "no project named '{name}' (try `choco project list`)")
            }
            ClientError::AmbiguousProject { name, ids } => write!(
                f,
                "'{name}' matches {} projects — pass one of these ids instead: {}",
                ids.len(),
                ids.join(", ")
            ),
            ClientError::InvalidConfig(msg) => write!(f, "{msg}"),
        }
    }
}

/// `POST /tasks` inputs, grouped into a struct rather than passed as eight
/// positional parameters (which is what `create_task` used to need an
/// `#[allow(clippy::too_many_arguments)]` for).
pub struct CreateTaskParams<'a> {
    pub project_id: &'a str,
    pub workflow_def: &'a str,
    pub title: &'a str,
    pub prompt: &'a str,
    /// Already-assembled task config, or `None` to send no config at all.
    /// Build it with [`build_task_config`].
    pub config: Option<Value>,
    pub parent_task_id: Option<&'a str>,
}

/// Assembles the task-level `config` object (design §5.5) from the CLI's
/// `--config` blob, the repeatable `--role-*` flags, and `--repo`.
///
/// Layered least- to most-specific, so the typed flags win per *field* and
/// don't clobber sibling fields the `--config` blob set for the same role:
/// `--config` -> `--role-*` -> `--repo`.
///
/// Returns `None` when nothing was supplied, so `choco task create` with no
/// config flags keeps sending `"config": null` rather than an empty object.
///
/// Every malformed input is an error, never a skipped override — running on
/// a model the caller didn't ask for is worse than failing loudly.
pub fn build_task_config(
    overrides: &RoleOverrides<'_>,
    repo: Option<&str>,
) -> Result<Option<Value>, ClientError> {
    let mut config = match overrides.config {
        Some(raw) => {
            let parsed: Value = serde_json::from_str(raw).map_err(|err| {
                ClientError::InvalidConfig(format!("--config is not valid JSON: {err}"))
            })?;
            if !parsed.is_object() {
                return Err(ClientError::InvalidConfig(
                    "--config must be a JSON object, e.g. \
                     '{\"roles\":{\"coder\":{\"model\":\"opus\"}}}'"
                        .to_string(),
                ));
            }
            reject_smuggled_prompt_file(&parsed)?;
            parsed
        }
        None => json!({}),
    };

    // Tracks which (role, field) pairs the typed flags have already set, so a
    // repeated flag is reported instead of silently taking the last value.
    let mut seen: Vec<(String, &str)> = Vec::new();
    let groups: [(&[String], &str, bool); 4] = [
        (overrides.role_cli, "cli", false),
        (overrides.role_model, "model", false),
        (overrides.role_system_prompt, "system_prompt", false),
        (
            overrides.role_system_prompt_file,
            "system_prompt",
            /* from_file */ true,
        ),
    ];

    for (values, field, from_file) in groups {
        let flag = match (field, from_file) {
            ("cli", _) => "--role-cli",
            ("model", _) => "--role-model",
            (_, false) => "--role-system-prompt",
            (_, true) => "--role-system-prompt-file",
        };
        for raw in values {
            let (role, value) = split_role_value(raw, flag)?;
            if seen.iter().any(|(r, f)| r == role && *f == field) {
                return Err(ClientError::InvalidConfig(format!(
                    "'{field}' given more than once for role '{role}' — \
                     pass a single value per role per field"
                )));
            }
            seen.push((role.to_string(), field));

            let value = if from_file {
                std::fs::read_to_string(value).map_err(|err| {
                    ClientError::InvalidConfig(format!(
                        "{flag}: failed to read '{value}' for role '{role}': {err}"
                    ))
                })?
            } else {
                value.to_string()
            };

            // `config` is known to be an object (checked above / built as
            // one), and each level is replaced if it isn't an object, so a
            // `--config` blob with a non-object `roles` can't panic here.
            let roles = config
                .as_object_mut()
                .expect("config is an object")
                .entry("roles")
                .or_insert_with(|| json!({}));
            if !roles.is_object() {
                *roles = json!({});
            }
            let role_entry = roles
                .as_object_mut()
                .expect("roles is an object")
                .entry(role)
                .or_insert_with(|| json!({}));
            if !role_entry.is_object() {
                *role_entry = json!({});
            }
            role_entry
                .as_object_mut()
                .expect("role entry is an object")
                .insert(field.to_string(), Value::String(value));
        }
    }

    if let Some(cwd) = repo {
        config
            .as_object_mut()
            .expect("config is an object")
            .insert("cwd".to_string(), Value::String(cwd.to_string()));
    }

    if config.as_object().is_some_and(|map| map.is_empty()) {
        return Ok(None);
    }
    Ok(Some(config))
}

/// Rejects `roles.<name>.system_prompt_file` in a `--config` blob.
///
/// The daemon has no such field by design — task config is the least-trusted
/// of the three layers, so it can't ask the daemon to read a path off disk —
/// which means sending one is a guaranteed silent no-op. Reporting it beats
/// letting someone believe they set a system prompt and never find out they
/// didn't. `--role-system-prompt-file` is the supported spelling: `choco`
/// reads the file here and sends its text.
///
/// Only this one field is checked, deliberately: everything else unknown is
/// left alone, because the daemon's own rule is that unrecognized task-config
/// content means "not overridden", never an error.
fn reject_smuggled_prompt_file(config: &Value) -> Result<(), ClientError> {
    let Some(roles) = config.get("roles").and_then(Value::as_object) else {
        return Ok(());
    };
    for (role, settings) in roles {
        if settings
            .as_object()
            .is_some_and(|s| s.contains_key("system_prompt_file"))
        {
            return Err(ClientError::InvalidConfig(format!(
                "--config sets roles.{role}.system_prompt_file, which the daemon \
                 never reads (task config may not reference files) — use \
                 --role-system-prompt-file {role}=<path> instead"
            )));
        }
    }
    Ok(())
}

/// Splits a `ROLE=VALUE` flag argument on its *first* `=`, so a value may
/// itself contain `=` (a system prompt often will).
fn split_role_value<'a>(raw: &'a str, flag: &str) -> Result<(&'a str, &'a str), ClientError> {
    let (role, value) = raw.split_once('=').ok_or_else(|| {
        ClientError::InvalidConfig(format!(
            "{flag} expects ROLE=VALUE, got '{raw}' (no '=' found)"
        ))
    })?;
    if role.is_empty() {
        return Err(ClientError::InvalidConfig(format!(
            "{flag}: role name is empty in '{raw}'"
        )));
    }
    // An empty value would be forwarded faithfully — `--model ''` on the
    // agent's command line — which is never what `--role-model coder=` meant.
    if value.is_empty() {
        return Err(ClientError::InvalidConfig(format!(
            "{flag}: value is empty in '{raw}' (nothing to set for role '{role}')"
        )));
    }
    Ok((role, value))
}

/// The `--role-*`/`--config` flag values [`build_task_config`] reads.
///
/// A borrowed view rather than `cli::RoleOverrideArgs` itself, so this
/// module stays independent of the `clap` layer (and the unit tests below can
/// build cases without constructing a parser).
#[derive(Default)]
pub struct RoleOverrides<'a> {
    pub role_cli: &'a [String],
    pub role_model: &'a [String],
    pub role_system_prompt: &'a [String],
    pub role_system_prompt_file: &'a [String],
    pub config: Option<&'a str>,
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
            None => format!("chocofactoryd returned HTTP {status}"),
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
    /// [`Self::create_task`] so the flag -> `config` mapping is
    /// unit-testable without a network round trip.
    fn create_task_request(&self, params: &CreateTaskParams<'_>) -> reqwest::RequestBuilder {
        self.http
            .post(format!("{}/tasks", self.base_url))
            .json(&json!({
                "project_id": params.project_id,
                "workflow_def": params.workflow_def,
                "title": params.title,
                "prompt": params.prompt,
                "config": params.config,
                "parent_task_id": params.parent_task_id,
            }))
    }

    pub async fn create_task(&self, params: &CreateTaskParams<'_>) -> Result<Task, ClientError> {
        let resp = self.send(self.create_task_request(params)).await?;
        let resp = self.check_status(resp).await?;
        self.decode(resp).await
    }

    fn update_task_config_request(&self, id: &str, config: &Value) -> reqwest::RequestBuilder {
        self.http
            .patch(format!("{}/tasks/{id}", self.base_url))
            .json(&json!({ "config": config }))
    }

    /// `PATCH /tasks/{id}` — merges `config` into the task's existing config.
    pub async fn update_task_config(&self, id: &str, config: &Value) -> Result<Task, ClientError> {
        let resp = self
            .send(self.update_task_config_request(id, config))
            .await?;
        let resp = self.check_status(resp).await?;
        self.decode(resp).await
    }

    /// Returns the raw `TaskDetail` JSON (`Task` fields flattened +
    /// `workflow_state`) rather than a typed struct — `TaskDetail` isn't
    /// exported from `chocofactory-core`, and the raw shape is exactly what
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

    /// Cancels a task (#69). No request body — the task id in the path is
    /// the entire request — and the daemon answers `202` with no body, so
    /// there is nothing to decode.
    pub async fn cancel_task(&self, id: &str) -> Result<(), ClientError> {
        let resp = self
            .send(
                self.http
                    .post(format!("{}/tasks/{id}/cancel", self.base_url)),
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

    fn params<'a>(config: Option<Value>, parent_task_id: Option<&'a str>) -> CreateTaskParams<'a> {
        CreateTaskParams {
            project_id: "p",
            workflow_def: "chat",
            title: "t",
            prompt: "hi",
            config,
            parent_task_id,
        }
    }

    fn body_of(req: reqwest::RequestBuilder) -> Value {
        let req = req.build().unwrap();
        serde_json::from_slice(req.body().unwrap().as_bytes().unwrap()).unwrap()
    }

    /// `strs(&["a=b"])` — the `Vec<String>` shape clap produces.
    fn strs(values: &[&str]) -> Vec<String> {
        values.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn create_task_includes_config_cwd_when_repo_is_given() {
        let config = build_task_config(&RoleOverrides::default(), Some("/repo")).unwrap();
        let body = body_of(client().create_task_request(&params(config, None)));
        assert_eq!(body["config"]["cwd"], "/repo");
    }

    #[test]
    fn create_task_sends_null_config_when_repo_is_absent() {
        let config = build_task_config(&RoleOverrides::default(), None).unwrap();
        assert!(config.is_none());
        let body = body_of(client().create_task_request(&params(config, None)));
        assert!(body["config"].is_null());
    }

    #[test]
    fn create_task_includes_parent_task_id_when_given() {
        let body = body_of(client().create_task_request(&params(None, Some("parent-1"))));
        assert_eq!(body["parent_task_id"], "parent-1");
    }

    #[test]
    fn update_task_config_patches_the_task_with_the_config_wrapped() {
        let config = json!({ "roles": { "coder": { "model": "opus" } } });
        let req = client()
            .update_task_config_request("task-1", &config)
            .build()
            .unwrap();
        assert_eq!(req.method(), "PATCH");
        assert!(req.url().path().ends_with("/tasks/task-1"));
        let body: Value = serde_json::from_slice(req.body().unwrap().as_bytes().unwrap()).unwrap();
        assert_eq!(body["config"], config);
    }

    // --- build_task_config: the P2-6 multi-role surface ---

    /// The headline case for #17: two roles configured independently in one
    /// invocation, each landing under its own key.
    #[test]
    fn build_config_sets_more_than_one_role_at_once() {
        let overrides = RoleOverrides {
            role_model: &strs(&["coder=opus", "reviewer=sonnet"]),
            role_cli: &strs(&["reviewer=claude"]),
            ..RoleOverrides::default()
        };

        let config = build_task_config(&overrides, Some("/repo"))
            .unwrap()
            .unwrap();

        assert_eq!(config["roles"]["coder"]["model"], "opus");
        assert_eq!(config["roles"]["reviewer"]["model"], "sonnet");
        assert_eq!(config["roles"]["reviewer"]["cli"], "claude");
        assert_eq!(config["cwd"], "/repo");
        // `coder` got no --role-cli, so nothing is invented for it.
        assert!(config["roles"]["coder"].get("cli").is_none());
    }

    /// A typed flag overrides only the field it names, leaving the rest of
    /// that role's `--config` object intact — the "typed flags win per field"
    /// rule, not "per role".
    #[test]
    fn build_config_typed_flag_beats_config_blob_without_erasing_siblings() {
        let overrides = RoleOverrides {
            role_model: &strs(&["coder=opus"]),
            config: Some(
                r#"{"roles":{"coder":{"model":"sonnet","cli":"claude"},
                             "reviewer":{"model":"haiku"}},"extra":1}"#,
            ),
            ..RoleOverrides::default()
        };

        let config = build_task_config(&overrides, None).unwrap().unwrap();

        assert_eq!(config["roles"]["coder"]["model"], "opus");
        assert_eq!(config["roles"]["coder"]["cli"], "claude");
        assert_eq!(config["roles"]["reviewer"]["model"], "haiku");
        assert_eq!(config["extra"], 1);
    }

    #[test]
    fn build_config_repo_overrides_a_cwd_from_the_config_blob() {
        let overrides = RoleOverrides {
            config: Some(r#"{"cwd":"/from-blob"}"#),
            ..RoleOverrides::default()
        };
        let config = build_task_config(&overrides, Some("/from-flag"))
            .unwrap()
            .unwrap();
        assert_eq!(config["cwd"], "/from-flag");
    }

    #[test]
    fn build_config_reads_a_system_prompt_from_a_file_client_side() {
        let dir = std::env::temp_dir().join(format!("choco-sp-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("coder-system.md");
        std::fs::write(&path, "be careful").unwrap();

        let overrides = RoleOverrides {
            role_system_prompt_file: &strs(&[&format!("coder={}", path.display())]),
            ..RoleOverrides::default()
        };
        let config = build_task_config(&overrides, None).unwrap().unwrap();

        // Sent as inline text: the daemon is never handed a path, because
        // task config is the least-trusted config layer.
        assert_eq!(config["roles"]["coder"]["system_prompt"], "be careful");
        assert!(
            config["roles"]["coder"].get("system_prompt_file").is_none(),
            "must not forward a path to the daemon"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn build_config_splits_on_the_first_equals_so_values_may_contain_one() {
        let overrides = RoleOverrides {
            role_system_prompt: &strs(&["coder=x = y = z"]),
            ..RoleOverrides::default()
        };
        let config = build_task_config(&overrides, None).unwrap().unwrap();
        assert_eq!(config["roles"]["coder"]["system_prompt"], "x = y = z");
    }

    /// A `--config` blob whose `roles` (or a role entry) isn't an object
    /// must not panic the index-and-insert path below it.
    #[test]
    fn build_config_replaces_a_non_object_roles_from_the_config_blob() {
        for blob in [r#"{"roles":7}"#, r#"{"roles":{"coder":"nope"}}"#] {
            let overrides = RoleOverrides {
                role_model: &strs(&["coder=opus"]),
                config: Some(blob),
                ..RoleOverrides::default()
            };
            let config = build_task_config(&overrides, None).unwrap().unwrap();
            assert_eq!(
                config["roles"]["coder"]["model"], "opus",
                "blob {blob} produced {config}"
            );
        }
    }

    // --- error cases: every bad input is reported, never silently dropped ---

    fn expect_err(overrides: &RoleOverrides<'_>) -> String {
        match build_task_config(overrides, None) {
            Err(ClientError::InvalidConfig(msg)) => msg,
            other => panic!("expected InvalidConfig, got {other:?}"),
        }
    }

    #[test]
    fn build_config_rejects_a_flag_without_an_equals() {
        let msg = expect_err(&RoleOverrides {
            role_model: &strs(&["opus"]),
            ..RoleOverrides::default()
        });
        assert!(msg.contains("--role-model"), "{msg}");
        assert!(msg.contains("ROLE=VALUE"), "{msg}");
    }

    #[test]
    fn build_config_rejects_an_empty_role_name() {
        let msg = expect_err(&RoleOverrides {
            role_model: &strs(&["=opus"]),
            ..RoleOverrides::default()
        });
        assert!(msg.contains("role name is empty"), "{msg}");
    }

    #[test]
    fn build_config_rejects_the_same_role_and_field_twice() {
        let msg = expect_err(&RoleOverrides {
            role_model: &strs(&["coder=opus", "coder=sonnet"]),
            ..RoleOverrides::default()
        });
        assert!(msg.contains("more than once"), "{msg}");
        assert!(msg.contains("coder"), "{msg}");
    }

    /// Both system-prompt flags target the same field, so asking for both is
    /// a conflict rather than a silent winner.
    #[test]
    fn build_config_rejects_inline_and_file_system_prompt_for_one_role() {
        let msg = expect_err(&RoleOverrides {
            role_system_prompt: &strs(&["coder=inline"]),
            role_system_prompt_file: &strs(&["coder=/some/path"]),
            ..RoleOverrides::default()
        });
        assert!(msg.contains("more than once"), "{msg}");
    }

    #[test]
    fn build_config_rejects_an_unreadable_system_prompt_file() {
        let msg = expect_err(&RoleOverrides {
            role_system_prompt_file: &strs(&["coder=/definitely/not/here.md"]),
            ..RoleOverrides::default()
        });
        assert!(msg.contains("/definitely/not/here.md"), "{msg}");
        assert!(msg.contains("coder"), "{msg}");
    }

    #[test]
    fn build_config_rejects_an_empty_value() {
        for raw in ["coder=", "coder"] {
            let msg = expect_err(&RoleOverrides {
                role_model: &strs(&[raw]),
                ..RoleOverrides::default()
            });
            assert!(msg.contains("--role-model"), "{raw} gave: {msg}");
        }
        // Specifically the empty-value case, which would otherwise be
        // forwarded as `--model ''` to the agent.
        let msg = expect_err(&RoleOverrides {
            role_model: &strs(&["coder="]),
            ..RoleOverrides::default()
        });
        assert!(msg.contains("value is empty"), "{msg}");
    }

    /// `system_prompt_file` has no meaning at the task level by design, so
    /// smuggling one through `--config` would be a silent no-op. Reported
    /// instead, pointing at the flag that does work.
    #[test]
    fn build_config_rejects_a_system_prompt_file_smuggled_through_config() {
        let msg = expect_err(&RoleOverrides {
            config: Some(r#"{"roles":{"coder":{"system_prompt_file":"/etc/passwd"}}}"#),
            ..RoleOverrides::default()
        });
        assert!(msg.contains("system_prompt_file"), "{msg}");
        assert!(msg.contains("--role-system-prompt-file"), "{msg}");

        // Other unrecognized keys are left alone: the daemon's own rule is
        // that unknown task-config content means "not overridden", not an
        // error, and `choco` shouldn't be stricter than the thing it calls.
        let config = build_task_config(
            &RoleOverrides {
                config: Some(r#"{"roles":{"coder":{"future_field":"x"}},"unknown":1}"#),
                ..RoleOverrides::default()
            },
            None,
        )
        .unwrap()
        .unwrap();
        assert_eq!(config["roles"]["coder"]["future_field"], "x");
        assert_eq!(config["unknown"], 1);
    }

    #[test]
    fn build_config_rejects_malformed_and_non_object_config_json() {
        let msg = expect_err(&RoleOverrides {
            config: Some("{not json"),
            ..RoleOverrides::default()
        });
        assert!(msg.contains("not valid JSON"), "{msg}");

        for blob in ["[1,2]", "\"text\"", "7", "null"] {
            let msg = expect_err(&RoleOverrides {
                config: Some(blob),
                ..RoleOverrides::default()
            });
            assert!(
                msg.contains("must be a JSON object"),
                "blob {blob} gave: {msg}"
            );
        }
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
