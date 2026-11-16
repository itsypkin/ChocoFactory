//! The read-only `get_context` tool both agent CLIs offer: one fixed-task,
//! fixed-anchor window onto `GET /tasks/{id}/context` (see `docs/context.md`).
//!
//! Everything the two adapters must agree on lives here — the launch
//! binding, the argument rules, the tool definition, the HTTP client and how
//! its answers become a tool result — so `choco mcp-serve` (Claude) and the
//! omp driver cannot drift apart. The daemon keeps ownership of history
//! traversal; this module only asks it one question per call.
//!
//! The model never chooses where the question goes. The task, the session
//! and the anchor (the newest stage result that existed when the turn was
//! launched, or the explicit absence of one) come from the [`ContextBinding`]
//! the daemon fixed at launch; the tool's arguments only say *what* to read.
//!
//! A call is read-only: one GET, no retry, no state of its own. A tool result
//! is never a report, so nothing here can complete a turn.

use std::time::Duration;

use reqwest::{StatusCode, Url};
use serde_json::{Value, json};

use crate::context::{ContextResponse, DEPARTING_KINDS, UnavailableReason};
use crate::mcp::GET_CONTEXT_TOOL_NAME;

/// How long one request may take, connection to last body byte.
pub const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

/// The `mcp-serve` flags a binding travels in. Startup-only: nothing reads
/// them from an environment variable, a lock file or a default port.
pub const TASK_ID_FLAG: &str = "--context-task-id";
pub const SESSION_ID_FLAG: &str = "--context-session-id";
pub const BASE_URL_FLAG: &str = "--context-base-url";
pub const ANCHOR_ID_FLAG: &str = "--context-anchor-id";
pub const NO_ANCHOR_FLAG: &str = "--context-no-anchor";

/// What a launch fixes for its `get_context` tool.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContextBinding {
    pub task_id: String,
    /// The session this launch created or resumed. Launch metadata only: it
    /// is never sent as a lookup selector.
    pub session_id: String,
    /// The newest stage result at launch. `None` is the explicit boundary
    /// "no result had been recorded yet", not "the newest at call time".
    pub anchor_id: Option<String>,
    pub base_url: Url,
}

fn nonblank(what: &str, value: &str) -> Result<(), String> {
    if value.trim().is_empty() {
        Err(format!("{what} must not be blank"))
    } else {
        Ok(())
    }
}

impl ContextBinding {
    pub fn new(
        task_id: &str,
        session_id: &str,
        anchor_id: Option<&str>,
        base_url: &str,
    ) -> Result<Self, String> {
        nonblank("the context task id", task_id)?;
        nonblank("the context session id", session_id)?;
        if let Some(anchor) = anchor_id {
            nonblank("the context anchor id", anchor)?;
        }
        let base_url = Url::parse(base_url)
            .map_err(|err| format!("the context base URL is not an absolute URL: {err}"))?;
        if !matches!(base_url.scheme(), "http" | "https") || base_url.host().is_none() {
            return Err("the context base URL must be an absolute http(s) URL with a host".into());
        }
        if base_url.query().is_some() || base_url.fragment().is_some() {
            return Err("the context base URL must not carry a query or a fragment".into());
        }
        Ok(Self {
            task_id: task_id.to_string(),
            session_id: session_id.to_string(),
            anchor_id: anchor_id.map(str::to_string),
            base_url,
        })
    }

    /// Builds a binding from `choco mcp-serve`'s flags. No binding flag at
    /// all is `Ok(None)` (manual, report-only mode); any one of them demands
    /// all the rest, including an explicit choice between an anchor and no
    /// anchor. Never guesses a missing piece.
    pub fn from_flags(
        task_id: Option<&str>,
        session_id: Option<&str>,
        base_url: Option<&str>,
        anchor_id: Option<&str>,
        no_anchor: bool,
    ) -> Result<Option<Self>, String> {
        if task_id.is_none()
            && session_id.is_none()
            && base_url.is_none()
            && anchor_id.is_none()
            && !no_anchor
        {
            return Ok(None);
        }
        if anchor_id.is_some() && no_anchor {
            return Err(format!(
                "{ANCHOR_ID_FLAG} and {NO_ANCHOR_FLAG} are mutually exclusive"
            ));
        }
        let missing: Vec<&str> = [
            (TASK_ID_FLAG, task_id.is_none()),
            (SESSION_ID_FLAG, session_id.is_none()),
            (BASE_URL_FLAG, base_url.is_none()),
            (
                "one of --context-anchor-id or --context-no-anchor",
                anchor_id.is_none() && !no_anchor,
            ),
        ]
        .into_iter()
        .filter_map(|(flag, absent)| absent.then_some(flag))
        .collect();
        if !missing.is_empty() {
            return Err(format!(
                "an incomplete context binding: missing {}",
                missing.join(", ")
            ));
        }
        Self::new(
            task_id.unwrap_or_default(),
            session_id.unwrap_or_default(),
            anchor_id,
            base_url.unwrap_or_default(),
        )
        .map(Some)
    }

    /// The `mcp-serve` arguments that carry this binding.
    pub fn to_args(&self) -> Vec<String> {
        let mut args = vec![
            TASK_ID_FLAG.to_string(),
            self.task_id.clone(),
            SESSION_ID_FLAG.to_string(),
            self.session_id.clone(),
            BASE_URL_FLAG.to_string(),
            self.base_url.to_string(),
        ];
        match &self.anchor_id {
            Some(anchor) => {
                args.push(ANCHOR_ID_FLAG.to_string());
                args.push(anchor.clone());
            }
            None => args.push(NO_ANCHOR_FLAG.to_string()),
        }
        args
    }
}

/// One validated `get_context` request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ContextOperation {
    /// `{"source":"task"}`
    Task,
    /// `{"source":"stage","stage":S}` with an optional `outcome`.
    StageByName {
        stage: String,
        outcome: Option<String>,
    },
    /// `{"source":"stage","result_id":R}`
    StageById { result_id: String },
}

const ALLOWED_KEYS: [&str; 4] = ["source", "stage", "outcome", "result_id"];

/// Checks a call's arguments. Selectors are opaque: blankness is detected
/// with `trim` but the value that is used is exactly what was sent.
pub fn parse_arguments(arguments: &Value) -> Result<ContextOperation, String> {
    let Some(object) = arguments.as_object() else {
        return Err(format!(
            "get_context arguments must be a JSON object such as {{\"source\":\"task\"}}; got {}",
            json_kind(arguments)
        ));
    };
    for key in object.keys() {
        if !ALLOWED_KEYS.contains(&key.as_str()) {
            return Err(format!(
                "unknown argument '{key}': get_context takes only source, stage, outcome and \
                 result_id. The task, session and daemon are fixed for this turn."
            ));
        }
    }
    let text = |key: &str| -> Result<Option<String>, String> {
        match object.get(key) {
            None => Ok(None),
            Some(Value::String(value)) if !value.trim().is_empty() => Ok(Some(value.clone())),
            Some(Value::String(_)) => Err(format!("'{key}' must not be blank")),
            Some(other) => Err(format!(
                "'{key}' must be a string; got {}",
                json_kind(other)
            )),
        }
    };
    let Some(source) = text("source")? else {
        return Err("'source' is required: \"task\" or \"stage\"".to_string());
    };
    let stage = text("stage")?;
    let outcome = text("outcome")?;
    let result_id = text("result_id")?;
    match source.as_str() {
        "task" => {
            if let Some(key) = ["stage", "outcome", "result_id"]
                .into_iter()
                .find(|key| object.contains_key(*key))
            {
                return Err(format!("source \"task\" takes no '{key}'"));
            }
            Ok(ContextOperation::Task)
        }
        "stage" => match (stage, result_id) {
            (Some(_), Some(_)) => Err("give either 'stage' or 'result_id', not both".to_string()),
            (None, None) => {
                Err("source \"stage\" needs 'stage' (the latest result) or 'result_id'".to_string())
            }
            (Some(stage), None) => Ok(ContextOperation::StageByName { stage, outcome }),
            (None, Some(_)) if outcome.is_some() => {
                Err("'outcome' goes with 'stage', not with 'result_id'".to_string())
            }
            (None, Some(result_id)) => Ok(ContextOperation::StageById { result_id }),
        },
        _ => Err("'source' must be \"task\" or \"stage\"".to_string()),
    }
}

fn json_kind(value: &Value) -> &'static str {
    match value {
        Value::Null => "null",
        Value::Bool(_) => "a boolean",
        Value::Number(_) => "a number",
        Value::String(_) => "a string",
        Value::Array(_) => "an array",
        Value::Object(_) => "an object",
    }
}

/// The tool as the model sees it.
pub fn tool_definition() -> Value {
    json!({
        "name": GET_CONTEXT_TOOL_NAME,
        "description": "Read-only lookup of ONE recorded fact about THIS task; it changes nothing, \
            does not report an outcome and never ends your turn by itself (it is not \
            report_outcome). source=\"task\" returns the task's original title, input and closing \
            issues. source=\"stage\" with `stage` returns the full recorded result of the latest \
            time the task left that stage (add `outcome` to require that exact outcome); with \
            `result_id` it returns exactly that result. Stage results are limited to what the \
            task had recorded when this turn started: later ones are not visible, and a turn that \
            started before any result existed sees none. Use it to read the governing request or \
            an earlier reviewer's or coder's own words when your prompt only summarises them. The \
            reply is one JSON object with a `provenance` field (task, stage_result, snapshot or \
            unavailable); `unavailable` says why a value could not be returned and never means \
            that no earlier review or message existed.",
        "inputSchema": {
            "type": "object",
            "properties": {
                "source": {
                    "type": "string",
                    "enum": ["task", "stage"],
                    "description": "\"task\" for the task's own title/input/closing issues; \"stage\" for a recorded stage result.",
                },
                "stage": {
                    "type": "string",
                    "description": "With source=stage: the name of the stage whose latest recorded result you want. Matched exactly.",
                },
                "outcome": {
                    "type": "string",
                    "description": "With stage: only a result whose applied outcome is exactly this.",
                },
                "result_id": {
                    "type": "string",
                    "description": "With source=stage, instead of `stage`: the exact ID of one earlier result of this task.",
                },
            },
            "required": ["source"],
            "additionalProperties": false,
        },
    })
}

/// A call's result as the tool layer returns it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContextOutput {
    pub text: String,
    pub is_error: bool,
}

impl ContextOutput {
    fn error(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            is_error: true,
        }
    }
}

/// One reusable HTTP client plus the binding it answers for.
#[derive(Debug, Clone)]
pub struct ContextClient {
    http: reqwest::Client,
    binding: ContextBinding,
}

impl ContextClient {
    pub fn new(binding: ContextBinding) -> Result<Self, String> {
        Self::with_timeout(binding, REQUEST_TIMEOUT)
    }

    /// [`Self::new`] with another whole-request timeout, for tests.
    pub fn with_timeout(binding: ContextBinding, timeout: Duration) -> Result<Self, String> {
        let http = reqwest::Client::builder()
            .timeout(timeout)
            // The daemon is local; a proxy variable in the agent's
            // environment must not reroute (or leak) a loopback request.
            .no_proxy()
            // The binding fixes where the question goes: a redirect would
            // answer from somewhere else, and the 3xx status itself must
            // be what the status check sees.
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .map_err(|err| format!("could not build the context HTTP client: {err}"))?;
        Ok(Self { http, binding })
    }

    pub fn binding(&self) -> &ContextBinding {
        &self.binding
    }

    /// Answers one `get_context` call. A valid daemon answer — including an
    /// `unavailable` one — is a success; a bad argument, a transport failure
    /// or an answer the daemon should not give is an error the model sees.
    pub async fn call(&self, arguments: &Value) -> ContextOutput {
        let operation = match parse_arguments(arguments) {
            Ok(operation) => operation,
            Err(message) => return ContextOutput::error(message),
        };
        match self.answer(&operation).await {
            Ok(response) => match serde_json::to_string(&response) {
                Ok(text) => ContextOutput {
                    text,
                    is_error: false,
                },
                Err(err) => ContextOutput::error(format!("could not encode the answer: {err}")),
            },
            Err(message) => ContextOutput::error(message),
        }
    }

    async fn answer(&self, operation: &ContextOperation) -> Result<ContextResponse, String> {
        let anchor = self.binding.anchor_id.as_deref();
        let mut query: Vec<(&str, &str)> = Vec::new();
        match (operation, anchor) {
            (ContextOperation::Task, _) => query.push(("source", "task")),
            // A launch with no earlier result: the boundary is fixed and
            // empty. An unanchored stage query would start at the task's
            // *current* result and drift to results written after launch,
            // so it is never sent. Only the task itself is asked for, so a
            // deleted task or an unreachable daemon is still an error.
            (_, None) => {
                self.fetch(&[("source", "task")]).await?;
                return Ok(ContextResponse::unavailable(
                    &self.binding.task_id,
                    None,
                    UnavailableReason::NotFoundInRecordedHistory,
                ));
            }
            (ContextOperation::StageByName { stage, outcome }, Some(anchor)) => {
                query.push(("source", "stage"));
                query.push(("stage", stage));
                if let Some(outcome) = outcome {
                    query.push(("outcome", outcome));
                }
                query.push(("anchor_id", anchor));
            }
            (ContextOperation::StageById { result_id }, Some(anchor)) => {
                query.push(("source", "stage"));
                query.push(("result_id", result_id));
                query.push(("anchor_id", anchor));
            }
        }
        self.fetch(&query).await
    }

    /// One GET. HTTP 200 carries any answer; 404 is an answer only when it
    /// is the typed `unavailable` body (a missing or foreign result ID, a
    /// missing anchor) — an unknown task's plain `{"error": …}` 404 is not.
    async fn fetch(&self, query: &[(&str, &str)]) -> Result<ContextResponse, String> {
        let mut url = self.binding.base_url.clone();
        url.path_segments_mut()
            .map_err(|_| "the context base URL cannot carry a path".to_string())?
            .pop_if_empty()
            .extend(["tasks", self.binding.task_id.as_str(), "context"]);
        url.query_pairs_mut().extend_pairs(query);
        let response = self
            .http
            .get(url)
            .send()
            .await
            .map_err(|err| format!("could not read the task context from the daemon: {err}"))?;
        let status = response.status();
        let body = response.bytes().await.map_err(|err| {
            format!("the daemon's task-context answer was cut short ({status}): {err}")
        })?;
        match status {
            StatusCode::OK => parse_answer(&body, &self.binding.task_id).map_err(|err| {
                format!("the daemon's task-context answer is not a valid answer: {err}")
            }),
            StatusCode::NOT_FOUND => match parse_answer(&body, &self.binding.task_id) {
                Ok(answer @ ContextResponse::Unavailable { .. }) => Ok(answer),
                _ => Err(format!(
                    "the daemon could not find this task's context (404): {}",
                    snippet(&body)
                )),
            },
            other => Err(format!(
                "the daemon failed to answer the task-context request ({other}): {}",
                snippet(&body)
            )),
        }
    }
}

/// Decodes a daemon answer and checks the facts the daemon always writes.
/// Plain deserialization would invent a `null` for a nullable field the
/// body left out and accept a result that contradicts itself; neither is a
/// recorded fact, so both are refused. So is an answer about another task
/// than the bound one, or whose own parts disagree about which task it is.
fn parse_answer(body: &[u8], bound_task_id: &str) -> Result<ContextResponse, String> {
    let raw: Value = serde_json::from_slice(body).map_err(|err| err.to_string())?;
    let present = |value: &Value, key: &str| -> Result<(), String> {
        if value.get(key).is_none() {
            return Err(format!("`{key}` is missing"));
        }
        Ok(())
    };
    present(&raw, "anchor_id")?;
    match raw.get("provenance").and_then(Value::as_str) {
        Some("stage_result") => {
            let result = raw.get("result").unwrap_or(&Value::Null);
            present(result, "previous_result_id")?;
        }
        Some("task") => present(raw.get("task").unwrap_or(&Value::Null), "input")?,
        _ => {}
    }
    let answer: ContextResponse = serde_json::from_value(raw).map_err(|err| err.to_string())?;
    let (outer_task_id, anchor_id) = match &answer {
        ContextResponse::Task {
            task_id, anchor_id, ..
        }
        | ContextResponse::StageResult {
            task_id, anchor_id, ..
        }
        | ContextResponse::Snapshot {
            task_id, anchor_id, ..
        }
        | ContextResponse::Unavailable {
            task_id, anchor_id, ..
        } => (task_id, anchor_id),
    };
    if outer_task_id != bound_task_id {
        return Err(format!(
            "the answer is about task {outer_task_id:?}, not the bound task"
        ));
    }
    // The daemon never anchors a task or a snapshot answer.
    if matches!(
        answer,
        ContextResponse::Task { .. } | ContextResponse::Snapshot { .. }
    ) && anchor_id.is_some()
    {
        return Err("a task or snapshot answer carries an anchor_id".into());
    }
    match &answer {
        ContextResponse::StageResult { result, .. } => {
            if result.task_id != *outer_task_id {
                return Err("the result belongs to a different task than the answer".into());
            }
            if result.source_stage.is_empty() || result.destination_stage.is_empty() {
                return Err("a stage name is empty".into());
            }
            if !DEPARTING_KINDS.contains(&result.source_kind.as_str()) {
                return Err(format!(
                    "source_kind {:?} is not a stage kind that departs",
                    result.source_kind
                ));
            }
            if result.previous_result_id.as_deref() == Some("") {
                return Err("previous_result_id is empty".into());
            }
            if !result.capture_present && !result.capture.is_null() {
                return Err("capture is set but capture_present is false".into());
            }
        }
        ContextResponse::Snapshot {
            capture_present,
            capture,
            ..
        } => {
            if !capture_present && !capture.is_null() {
                return Err("capture is set but capture_present is false".into());
            }
        }
        ContextResponse::Task { task, .. } => {
            if task.input_available != task.input.is_some() {
                return Err("input_available does not match whether input is present".into());
            }
        }
        ContextResponse::Unavailable { .. } => {}
    }
    Ok(answer)
}

/// The start of a failure body, enough to say what the daemon said.
fn snippet(body: &[u8]) -> String {
    let text = String::from_utf8_lossy(body);
    let mut shown: String = text.chars().take(300).collect();
    if shown.len() < text.len() {
        shown.push('…');
    }
    shown
}

#[cfg(test)]
mod tests {
    use super::*;

    const URL: &str = "http://127.0.0.1:4999";

    fn valid(arguments: Value) -> ContextOperation {
        parse_arguments(&arguments).unwrap_or_else(|err| panic!("{arguments}: {err}"))
    }

    fn refused(arguments: Value) -> String {
        match parse_arguments(&arguments) {
            Ok(op) => panic!("{arguments} was accepted as {op:?}"),
            Err(err) => err,
        }
    }

    #[test]
    fn the_three_operations_parse_and_selectors_stay_exact() {
        assert_eq!(valid(json!({"source": "task"})), ContextOperation::Task);
        assert_eq!(
            valid(json!({"source": "stage", "stage": " Code & review? "})),
            ContextOperation::StageByName {
                stage: " Code & review? ".into(),
                outcome: None
            }
        );
        assert_eq!(
            valid(json!({"source": "stage", "stage": "s", "outcome": "a=b&c"})),
            ContextOperation::StageByName {
                stage: "s".into(),
                outcome: Some("a=b&c".into())
            }
        );
        assert_eq!(
            valid(json!({"source": "stage", "result_id": "r#1 &x=y"})),
            ContextOperation::StageById {
                result_id: "r#1 &x=y".into()
            }
        );
    }

    #[test]
    fn malformed_arguments_are_refused() {
        // Not an object at all.
        for arguments in [
            Value::Null,
            json!([]),
            json!([{"source": "task"}]),
            json!("task"),
            json!(7),
            json!(true),
        ] {
            refused(arguments);
        }
        // Source: absent, null, wrong type, blank, unknown.
        for source in [
            None,
            Some(json!(null)),
            Some(json!(1)),
            Some(json!(["task"])),
            Some(json!("")),
            Some(json!("  ")),
            Some(json!("history")),
            Some(json!("Task")),
        ] {
            let mut arguments = json!({"stage": "s"});
            if let Some(source) = source {
                arguments["source"] = source;
            }
            refused(arguments);
        }
        // Selectors: null, wrong type, blank — each on a call that is
        // otherwise valid.
        for bad in [json!(null), json!(7), json!(true), json!(""), json!(" \t")] {
            refused(json!({"source": "stage", "stage": bad}));
            refused(json!({"source": "stage", "result_id": bad}));
            refused(json!({"source": "stage", "stage": "s", "outcome": bad}));
        }
        // Missing selector, and the forms that must not mix.
        refused(json!({"source": "stage"}));
        refused(json!({"source": "stage", "outcome": "o"}));
        refused(json!({"source": "stage", "stage": "s", "result_id": "r"}));
        refused(json!({"source": "stage", "result_id": "r", "outcome": "o"}));
        for key in ["stage", "outcome", "result_id"] {
            refused(json!({"source": "task", key: "x"}));
            // Even a null selector on a task call is not ignored.
            refused(json!({"source": "task", key: null}));
        }
    }

    #[test]
    fn a_routing_override_is_refused_on_every_valid_form() {
        let forms = [
            json!({"source": "task"}),
            json!({"source": "stage", "stage": "s"}),
            json!({"source": "stage", "stage": "s", "outcome": "o"}),
            json!({"source": "stage", "result_id": "r"}),
        ];
        for key in [
            "task_id",
            "anchor_id",
            "session_id",
            "daemon_url",
            "base_url",
            "stagee",
        ] {
            for form in &forms {
                let mut arguments = form.clone();
                arguments[key] = json!("other");
                let error = refused(arguments);
                assert!(error.contains(key), "{error}");
            }
        }
    }

    #[test]
    fn flags_make_a_binding_only_when_complete() {
        assert_eq!(
            ContextBinding::from_flags(None, None, None, None, false),
            Ok(None)
        );
        let binding =
            ContextBinding::from_flags(Some("t"), Some("s"), Some(URL), Some("r1"), false)
                .unwrap()
                .unwrap();
        assert_eq!(binding.anchor_id.as_deref(), Some("r1"));
        let empty = ContextBinding::from_flags(Some("t"), Some("s"), Some(URL), None, true)
            .unwrap()
            .unwrap();
        assert_eq!(empty.anchor_id, None);
        // Any single missing piece, each alone.
        for (task, session, url, anchor, none) in [
            (None, Some("s"), Some(URL), Some("r"), false),
            (Some("t"), None, Some(URL), Some("r"), false),
            (Some("t"), Some("s"), None, Some("r"), false),
            (Some("t"), Some("s"), Some(URL), None, false),
            (Some("t"), Some("s"), Some(URL), Some("r"), true),
            (None, None, None, None, true),
            (None, None, None, Some("r"), false),
        ] {
            assert!(
                ContextBinding::from_flags(task, session, url, anchor, none).is_err(),
                "{task:?} {session:?} {url:?} {anchor:?} {none}"
            );
        }
        // Explicit-but-unusable components.
        for (task, session, url, anchor) in [
            ("", "s", URL, Some("r")),
            ("t", "  ", URL, Some("r")),
            ("t", "s", URL, Some("")),
            ("t", "s", "", Some("r")),
            ("t", "s", "127.0.0.1:4141", Some("r")),
            ("t", "s", "/relative", Some("r")),
            ("t", "s", "ftp://127.0.0.1", Some("r")),
            ("t", "s", "file:///tmp/x", Some("r")),
            ("t", "s", "http://127.0.0.1:4141/?a=b", Some("r")),
            ("t", "s", "http://127.0.0.1:4141/#x", Some("r")),
        ] {
            assert!(
                ContextBinding::from_flags(Some(task), Some(session), Some(url), anchor, false)
                    .is_err(),
                "{task:?} {session:?} {url:?} {anchor:?}"
            );
        }
    }

    #[test]
    fn a_binding_round_trips_through_its_flags() {
        for anchor in [Some("r-1"), None] {
            let binding = ContextBinding::new("task one", "sess", anchor, URL).unwrap();
            let args = binding.to_args();
            let value_after = |flag: &str| {
                args.iter()
                    .position(|arg| arg == flag)
                    .map(|index| args[index + 1].as_str())
            };
            let rebuilt = ContextBinding::from_flags(
                value_after(TASK_ID_FLAG),
                value_after(SESSION_ID_FLAG),
                value_after(BASE_URL_FLAG),
                value_after(ANCHOR_ID_FLAG),
                args.iter().any(|arg| arg == NO_ANCHOR_FLAG),
            )
            .unwrap()
            .unwrap();
            assert_eq!(rebuilt, binding);
        }
    }

    #[test]
    fn the_definition_offers_exactly_the_documented_arguments() {
        let definition = tool_definition();
        assert_eq!(definition["name"], GET_CONTEXT_TOOL_NAME);
        let schema = &definition["inputSchema"];
        assert_eq!(schema["required"], json!(["source"]));
        assert_eq!(schema["additionalProperties"], json!(false));
        let mut keys: Vec<&str> = schema["properties"]
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect();
        keys.sort();
        assert_eq!(keys, ["outcome", "result_id", "source", "stage"]);
    }

    // -- the client, against a private daemon ---------------------------------

    use std::sync::{Arc, Mutex};

    type Respond = Arc<dyn Fn(&axum::http::Uri) -> (u16, String) + Send + Sync>;

    struct FakeDaemon {
        base: String,
        requests: Arc<Mutex<Vec<axum::http::Uri>>>,
    }

    impl FakeDaemon {
        async fn start(respond: Respond, delay: Duration) -> Self {
            let requests = Arc::new(Mutex::new(Vec::new()));
            let seen = Arc::clone(&requests);
            let app = axum::Router::new().fallback(move |uri: axum::http::Uri| {
                let respond = Arc::clone(&respond);
                let seen = Arc::clone(&seen);
                async move {
                    seen.lock().unwrap().push(uri.clone());
                    tokio::time::sleep(delay).await;
                    let (status, body) = respond(&uri);
                    (
                        axum::http::StatusCode::from_u16(status).unwrap(),
                        [("content-type", "application/json")],
                        body,
                    )
                }
            });
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let base = format!("http://{}", listener.local_addr().unwrap());
            tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
            Self { base, requests }
        }

        /// Every request's decoded `(path, query pairs)`.
        fn seen(&self) -> Vec<(String, Vec<(String, String)>)> {
            self.requests
                .lock()
                .unwrap()
                .iter()
                .map(|uri| {
                    let url = Url::parse(&format!("http://x{uri}")).unwrap();
                    (
                        url.path().to_string(),
                        url.query_pairs()
                            .map(|(k, v)| (k.into_owned(), v.into_owned()))
                            .collect(),
                    )
                })
                .collect()
        }
    }

    fn pairs(items: &[(&str, &str)]) -> Vec<(String, String)> {
        items
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    fn client(daemon: &FakeDaemon, anchor: Option<&str>) -> ContextClient {
        let binding = ContextBinding::new("task/1 &x", "sess-1", anchor, &daemon.base).unwrap();
        ContextClient::new(binding).unwrap()
    }

    fn fixed(status: u16, body: Value) -> Respond {
        let body = body.to_string();
        Arc::new(move |_| (status, body.clone()))
    }

    fn stage_result_body(capture: &str) -> Value {
        json!({"provenance": "stage_result", "task_id": "task/1 &x", "anchor_id": "a1",
               "result": {"task_id": "task/1 &x", "result_id": "r1", "source_stage": "review",
                          "source_kind": "human_gate", "outcome": "changes_requested",
                          "destination_stage": "coding", "capture_present": true,
                          "capture": capture, "previous_result_id": null,
                          "created_at": "2026-11-15T10:00:00Z"}})
    }

    #[tokio::test]
    async fn an_anchored_stage_request_sends_the_fixed_anchor_and_the_selectors_exactly() {
        let capture = format!("{}END-SENTINEL", "x".repeat(5000));
        let daemon =
            FakeDaemon::start(fixed(200, stage_result_body(&capture)), Duration::ZERO).await;
        let client = client(&daemon, Some("a1"));

        let by_name = client
            .call(&json!({"source": "stage", "stage": "re view&x=1", "outcome": "a b"}))
            .await;
        assert!(!by_name.is_error, "{}", by_name.text);
        // The whole capture, trailing sentinel included, reaches the model.
        let body: Value = serde_json::from_str(&by_name.text).unwrap();
        assert_eq!(body["result"]["capture"], json!(capture));

        let by_id = client
            .call(&json!({"source": "stage", "result_id": "r#1 &anchor_id=zz"}))
            .await;
        assert!(!by_id.is_error);
        let task = client.call(&json!({"source": "task"})).await;
        assert!(!task.is_error);

        let task_path = "/tasks/task%2F1%20&x/context";
        assert_eq!(
            daemon.seen(),
            vec![
                (
                    task_path.to_string(),
                    pairs(&[
                        ("source", "stage"),
                        ("stage", "re view&x=1"),
                        ("outcome", "a b"),
                        ("anchor_id", "a1"),
                    ])
                ),
                (
                    task_path.to_string(),
                    pairs(&[
                        ("source", "stage"),
                        ("result_id", "r#1 &anchor_id=zz"),
                        ("anchor_id", "a1"),
                    ])
                ),
                // A task request carries no anchor at all.
                (task_path.to_string(), pairs(&[("source", "task")])),
            ]
        );
    }

    #[tokio::test]
    async fn a_fixed_empty_boundary_asks_only_for_the_task_and_answers_unavailable() {
        // The daemon would happily answer a stage query with a snapshot or a
        // newer result; the tool must never ask.
        let respond: Respond = Arc::new(|uri| {
            if uri.query() == Some("source=task") {
                (
                    200,
                    json!({"provenance": "task", "task_id": "task/1 &x", "anchor_id": null,
                           "task": {"title": "T", "input_available": true, "input": "I",
                                    "closes": [369]}})
                    .to_string(),
                )
            } else {
                (
                    200,
                    json!({"provenance": "snapshot", "task_id": "task/1 &x", "anchor_id": null,
                           "stage": "review", "capture_present": true,
                           "capture": "LEAKED"})
                    .to_string(),
                )
            }
        });
        let daemon = FakeDaemon::start(respond, Duration::ZERO).await;
        let client = client(&daemon, None);
        for arguments in [
            json!({"source": "stage", "stage": "review"}),
            json!({"source": "stage", "stage": "review", "outcome": "o"}),
            json!({"source": "stage", "result_id": "r1"}),
        ] {
            let output = client.call(&arguments).await;
            assert!(!output.is_error, "{}", output.text);
            assert_eq!(
                serde_json::from_str::<Value>(&output.text).unwrap(),
                json!({"provenance": "unavailable", "task_id": "task/1 &x", "anchor_id": null,
                       "reason": "not_found_in_recorded_history"})
            );
        }
        assert!(
            daemon
                .seen()
                .iter()
                .all(|(_, query)| query == &pairs(&[("source", "task")])),
            "{:?}",
            daemon.seen()
        );
        assert_eq!(daemon.seen().len(), 3);
        // The task's own facts still come back.
        let task = client.call(&json!({"source": "task"})).await;
        assert!(task.text.contains("\"closes\":[369]"), "{}", task.text);
    }

    #[tokio::test]
    async fn a_fixed_empty_boundary_still_reports_a_missing_task_or_daemon() {
        let missing =
            FakeDaemon::start(fixed(404, json!({"error": "no such task"})), Duration::ZERO).await;
        let output = client(&missing, None)
            .call(&json!({"source": "stage", "stage": "review"}))
            .await;
        assert!(output.is_error, "{}", output.text);

        // Nothing listens on port 1.
        let offline =
            ContextClient::new(ContextBinding::new("t", "s", None, "http://127.0.0.1:1").unwrap())
                .unwrap();
        let output = offline
            .call(&json!({"source": "stage", "stage": "review"}))
            .await;
        assert!(output.is_error, "{}", output.text);
    }

    #[tokio::test]
    async fn typed_unavailable_answers_succeed_and_every_other_failure_is_an_error() {
        let unavailable = |reason: &str| {
            json!({"provenance": "unavailable", "task_id": "task/1 &x", "anchor_id": "a1",
                   "reason": reason})
        };
        let ask = json!({"source": "stage", "result_id": "r9"});
        for (status, body) in [
            (200, unavailable("history_gap")),
            (404, unavailable("missing_result")),
            (404, unavailable("missing_anchor")),
        ] {
            let daemon = FakeDaemon::start(fixed(status, body.clone()), Duration::ZERO).await;
            let output = client(&daemon, Some("a1")).call(&ask).await;
            assert!(!output.is_error, "{status} {body}: {}", output.text);
            assert_eq!(serde_json::from_str::<Value>(&output.text).unwrap(), body);
        }
        let not_an_answer = json!({"hello": "world"});
        for (status, body) in [
            // The ordinary unknown-task error body.
            (404, json!({"error": "no such task"})),
            // A typed answer that is not `unavailable` on a 404.
            (404, stage_result_body("x")),
            (400, json!({"error": "bad query"})),
            (500, json!({"error": "database is locked"})),
            (200, not_an_answer.clone()),
            (200, json!({"provenance": "unavailable", "task_id": "t"})),
            (200, json!(null)),
        ] {
            let daemon = FakeDaemon::start(fixed(status, body.clone()), Duration::ZERO).await;
            let output = client(&daemon, Some("a1")).call(&ask).await;
            assert!(output.is_error, "{status} {body}: {}", output.text);
        }
        // Not JSON at all.
        let daemon = FakeDaemon::start(Arc::new(|_| (200, "<html>".into())), Duration::ZERO).await;
        assert!(client(&daemon, Some("a1")).call(&ask).await.is_error);
    }

    #[tokio::test]
    async fn a_refused_connection_and_a_slow_daemon_are_errors() {
        let refused = ContextClient::new(
            ContextBinding::new("t", "s", Some("a"), "http://127.0.0.1:1").unwrap(),
        )
        .unwrap();
        let output = refused.call(&json!({"source": "task"})).await;
        assert!(output.is_error, "{}", output.text);

        let slow = FakeDaemon::start(
            fixed(200, stage_result_body("x")),
            Duration::from_millis(600),
        )
        .await;
        let binding = ContextBinding::new("t", "s", Some("a"), &slow.base).unwrap();
        let client = ContextClient::with_timeout(binding, Duration::from_millis(100)).unwrap();
        let started = std::time::Instant::now();
        let output = client.call(&json!({"source": "task"})).await;
        assert!(output.is_error, "{}", output.text);
        assert!(started.elapsed() < Duration::from_millis(550));
    }

    #[tokio::test]
    async fn a_refused_argument_never_reaches_the_daemon() {
        let daemon = FakeDaemon::start(fixed(200, stage_result_body("x")), Duration::ZERO).await;
        let client = client(&daemon, Some("a1"));
        for arguments in [
            Value::Null,
            json!({}),
            json!({"source": "task", "task_id": "other"}),
            json!({"source": "stage", "stage": "s", "anchor_id": "zz"}),
        ] {
            assert!(client.call(&arguments).await.is_error);
        }
        assert!(daemon.seen().is_empty());
    }

    #[tokio::test]
    async fn a_daemon_that_hangs_up_mid_answer_is_an_error() {
        // Accepts the connection and drops it without a byte of answer: a
        // socket disconnect, not a refusal and not a timeout.
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let accepted = tokio::spawn(async move {
            let (socket, _) = listener.accept().await.unwrap();
            drop(socket);
        });
        let binding = ContextBinding::new("t", "s", Some("a"), &base).unwrap();
        let output = ContextClient::new(binding)
            .unwrap()
            .call(&json!({"source": "task"}))
            .await;
        accepted.await.unwrap();
        assert!(output.is_error, "{}", output.text);
    }

    #[tokio::test]
    async fn a_redirect_is_an_error_and_never_reaches_the_other_address() {
        for status in [301u16, 302, 303, 307, 308] {
            let decoy = FakeDaemon::start(
                fixed(200, stage_result_body("DECOY CONTENT")),
                Duration::ZERO,
            )
            .await;
            let target = format!("{}/elsewhere", decoy.base);
            let redirecting: Respond = Arc::new(move |_| (status, String::new()));
            let daemon = {
                // A daemon that answers every request with a redirect.
                let requests = Arc::new(Mutex::new(Vec::new()));
                let seen = Arc::clone(&requests);
                let app = axum::Router::new().fallback(move |uri: axum::http::Uri| {
                    let respond = Arc::clone(&redirecting);
                    let seen = Arc::clone(&seen);
                    let target = target.clone();
                    async move {
                        seen.lock().unwrap().push(uri.clone());
                        let (status, _) = respond(&uri);
                        (
                            axum::http::StatusCode::from_u16(status).unwrap(),
                            [("location", target)],
                            String::new(),
                        )
                    }
                });
                let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
                let base = format!("http://{}", listener.local_addr().unwrap());
                tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
                FakeDaemon { base, requests }
            };
            let anchored = client(&daemon, Some("a1"));
            let empty = client(&daemon, None);
            for (label, client, arguments) in [
                ("task", &anchored, json!({"source": "task"})),
                ("stage", &anchored, json!({"source": "stage", "stage": "s"})),
                (
                    "id",
                    &anchored,
                    json!({"source": "stage", "result_id": "r1"}),
                ),
                (
                    "empty boundary",
                    &empty,
                    json!({"source": "stage", "stage": "s"}),
                ),
            ] {
                let output = client.call(&arguments).await;
                assert!(output.is_error, "{status} {label}: {}", output.text);
                assert!(!output.text.contains("DECOY"), "{status} {label}");
            }
            assert!(decoy.seen().is_empty(), "{status}: {:?}", decoy.seen());
            assert_eq!(daemon.seen().len(), 4, "{status}: one request per call");
        }
    }

    #[tokio::test]
    async fn an_answer_that_leaves_out_or_contradicts_a_recorded_fact_is_an_error() {
        let ask = json!({"source": "stage", "result_id": "r1"});
        let without = |mut body: Value, path: &[&str]| {
            let (last, parents) = path.split_last().unwrap();
            let mut at = &mut body;
            for key in parents {
                at = at.get_mut(*key).unwrap();
            }
            at.as_object_mut().unwrap().remove(*last);
            body
        };
        let with = |mut body: Value, path: &[&str], value: Value| {
            let (last, parents) = path.split_last().unwrap();
            let mut at = &mut body;
            for key in parents {
                at = at.get_mut(*key).unwrap();
            }
            at.as_object_mut().unwrap().insert(last.to_string(), value);
            body
        };
        let task = json!({"provenance": "task", "task_id": "task/1 &x", "anchor_id": null,
                          "task": {"title": "T", "input_available": true, "input": "I",
                                   "closes": []}});
        let snapshot = json!({"provenance": "snapshot", "task_id": "task/1 &x",
                              "anchor_id": null, "stage": "s", "capture_present": true,
                              "capture": "C"});
        let unavailable = json!({"provenance": "unavailable", "task_id": "task/1 &x",
                                 "anchor_id": "a1", "reason": "missing_result"});
        let bad: Vec<(&str, u16, Value)> = vec![
            (
                "stage result without anchor_id",
                200,
                without(stage_result_body("x"), &["anchor_id"]),
            ),
            (
                "stage result without previous_result_id",
                200,
                without(stage_result_body("x"), &["result", "previous_result_id"]),
            ),
            (
                "unknown source_kind",
                200,
                with(
                    stage_result_body("x"),
                    &["result", "source_kind"],
                    json!("bogus"),
                ),
            ),
            (
                "capture without capture_present",
                200,
                with(
                    with(
                        stage_result_body("x"),
                        &["result", "capture_present"],
                        json!(false),
                    ),
                    &["result", "capture"],
                    json!("stray"),
                ),
            ),
            (
                "empty previous_result_id",
                200,
                with(
                    stage_result_body("x"),
                    &["result", "previous_result_id"],
                    json!(""),
                ),
            ),
            (
                "task without anchor_id",
                200,
                without(task.clone(), &["anchor_id"]),
            ),
            (
                "task without input",
                200,
                without(task.clone(), &["task", "input"]),
            ),
            (
                "task input contradicts input_available",
                200,
                with(task.clone(), &["task", "input_available"], json!(false)),
            ),
            (
                "snapshot capture without capture_present",
                200,
                with(
                    with(snapshot.clone(), &["capture_present"], json!(false)),
                    &["capture"],
                    json!("stray"),
                ),
            ),
            (
                "snapshot without anchor_id",
                200,
                without(snapshot.clone(), &["anchor_id"]),
            ),
            (
                "typed 404 without anchor_id",
                404,
                without(unavailable.clone(), &["anchor_id"]),
            ),
            (
                "empty source_stage only",
                200,
                with(
                    stage_result_body("x"),
                    &["result", "source_stage"],
                    json!(""),
                ),
            ),
            (
                "empty destination_stage only",
                200,
                with(
                    stage_result_body("x"),
                    &["result", "destination_stage"],
                    json!(""),
                ),
            ),
            (
                "result of another task inside this task's answer",
                200,
                with(
                    stage_result_body("x"),
                    &["result", "task_id"],
                    json!("ANOTHER-TASK"),
                ),
            ),
            (
                "answer about another task than the bound one",
                200,
                with(stage_result_body("x"), &["task_id"], json!("ANOTHER-TASK")),
            ),
            (
                "typed 404 about another task",
                404,
                with(unavailable.clone(), &["task_id"], json!("ANOTHER-TASK")),
            ),
            (
                "task answer with an anchor",
                200,
                with(task.clone(), &["anchor_id"], json!("a1")),
            ),
            (
                "snapshot answer with an anchor",
                200,
                with(snapshot.clone(), &["anchor_id"], json!("a1")),
            ),
        ];
        for (label, status, body) in bad {
            let daemon = FakeDaemon::start(fixed(status, body.clone()), Duration::ZERO).await;
            let output = client(&daemon, Some("a1")).call(&ask).await;
            assert!(output.is_error, "{label}: {}", output.text);
        }
        // The same shapes with every nullable field explicitly null are valid.
        let good = [
            stage_result_body("x"),
            task,
            snapshot,
            with(
                with(
                    stage_result_body("x"),
                    &["result", "capture_present"],
                    json!(false),
                ),
                &["result", "capture"],
                Value::Null,
            ),
            with(stage_result_body("x"), &["result", "capture"], Value::Null),
        ];
        for body in good {
            let daemon = FakeDaemon::start(fixed(200, body.clone()), Duration::ZERO).await;
            let output = client(&daemon, Some("a1")).call(&ask).await;
            assert!(!output.is_error, "{body}: {}", output.text);
            assert_eq!(serde_json::from_str::<Value>(&output.text).unwrap(), body);
        }
    }

    #[tokio::test]
    async fn a_body_cut_short_after_the_headers_is_a_cut_short_error() {
        // Promises more bytes than it sends, then closes: the headers
        // arrive, the body does not.
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let served = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut buf = [0u8; 2048];
            let _ = socket.read(&mut buf).await.unwrap();
            socket
                .write_all(
                    b"HTTP/1.1 200 OK\r\ncontent-type: application/json\r\n\
                      content-length: 5000\r\n\r\n{\"provenance\":\"task\"",
                )
                .await
                .unwrap();
            socket.shutdown().await.unwrap();
        });
        let binding = ContextBinding::new("t", "s", Some("a"), &base).unwrap();
        let output = ContextClient::new(binding)
            .unwrap()
            .call(&json!({"source": "task"}))
            .await;
        served.await.unwrap();
        assert!(output.is_error, "{}", output.text);
        assert!(output.text.contains("cut short"), "{}", output.text);
    }
}
