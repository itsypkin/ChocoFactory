//! `get_context` against a private daemon with real stored history (#369).
//!
//! The history comes from a real router over a real database (`TestServer`,
//! an ephemeral port, never :4141): gate replies departing a gate stage,
//! recorded as stage results. The Claude surface is the built `choco
//! mcp-serve` binary driven over JSON-RPC stdio; the omp surface is the
//! production omp driver against `fake_omp.py` (see `adapter::omp_tests`).
//! Both are shared by the same fixtures and assertions, so a divergence
//! between the two surfaces fails here. These are isolated fixtures, not runs
//! of the real CLIs.

use std::path::PathBuf;
use std::process::Stdio;
use std::time::Duration;

use chocofactory_core::context_tool::ContextBinding;
use chocofactory_core::models::EventType;
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::Command;

use crate::api::tests::TestServer;

pub(crate) const CONTEXT_FLOW: &str = r#"
name: ctxflow
stages:
  gate:
    kind: human_gate
    capture: text
    markers:
      - line: /again
        then: again
      - line: /approve
        then: approved
    on: { again: gate, approved: finished }
  finished:
    kind: terminal
"#;

/// More than the 2,048 bytes a truncating reader would keep.
const PADDING: &str = "0123456789abcdefghij0123456789abcdefghij0123456789abcdefghij\
0123456789abcdefghij0123456789abcdefghij0123456789abcdefghij";

/// A task on the private daemon and what it has recorded so far.
pub(crate) struct TaskFacts {
    pub id: String,
    pub title: String,
    pub input: String,
    /// The stage results, oldest first.
    pub results: Vec<String>,
    /// Each correction's expected captured text, oldest first.
    pub captures: Vec<String>,
    pub tag: String,
}

impl TaskFacts {
    /// A correction longer than 2 KiB, ending in a sentinel only the whole
    /// text carries.
    fn correction(&self, n: usize) -> String {
        let mut text = format!("Correction {n} of {}.\n", self.tag);
        for _ in 0..40 {
            text.push_str(PADDING);
            text.push('\n');
        }
        text.push_str(&format!("END-{}-CORRECTION-{n}", self.tag));
        text
    }

    /// Replies to the gate: `again` loops it, `approved` leaves it. Each is
    /// one departure, hence one stage result.
    pub(crate) async fn reply(&mut self, server: &TestServer, outcome: &str) {
        let n = self.results.len() + 1;
        let text = if outcome == "again" {
            self.correction(n)
        } else {
            format!("Ship it, round {n} of {}.", self.tag)
        };
        let marker = if outcome == "again" {
            "/again"
        } else {
            "/approve"
        };
        let response = server
            .post(
                &format!("/tasks/{}/messages", self.id),
                json!({ "text": format!("{text}\n{marker}") }),
            )
            .await;
        assert_eq!(response.status(), 202, "{}", response.json());
        self.captures.push(text);
        let results = stage_result_ids(server, &self.id).await;
        assert_eq!(results.len(), n, "one stage result per reply");
        self.results = results;
    }
}

async fn stage_result_ids(server: &TestServer, task_id: &str) -> Vec<String> {
    crate::db::events::list_for_task(server.pool(), task_id)
        .await
        .unwrap()
        .into_iter()
        .filter(|event| event.event_type == EventType::StageResult)
        .map(|event| event.id)
        .collect()
}

/// A task parked at its gate, with nothing recorded yet.
pub(crate) async fn new_task(
    server: &TestServer,
    project: &str,
    tag: &str,
    closes: &[u64],
) -> TaskFacts {
    let title = format!("Title of {tag}");
    let input = format!("Private brief of {tag}: do the thing & only that.");
    let created: Value = server
        .post(
            "/tasks",
            json!({ "project_id": project, "workflow_def": "ctxflow", "title": title,
                    "prompt": input, "closes": closes, "config": { "cwd": "." } }),
        )
        .await
        .json();
    TaskFacts {
        id: created["id"].as_str().unwrap().to_string(),
        title,
        input,
        results: Vec::new(),
        captures: Vec::new(),
        tag: tag.to_string(),
    }
}

pub(crate) async fn server_with_flow() -> (TestServer, String) {
    let server = TestServer::start().await;
    server.write_workflow("ctxflow", CONTEXT_FLOW);
    let project: Value = server
        .post("/projects", json!({ "name": "demo" }))
        .await
        .json();
    let project = project["id"].as_str().unwrap().to_string();
    (server, project)
}

/// One surface's answer to one call.
#[derive(Debug, Clone)]
pub(crate) struct ToolReply {
    pub is_error: bool,
    pub text: String,
}

impl ToolReply {
    pub(crate) fn json(&self) -> Value {
        serde_json::from_str(&self.text)
            .unwrap_or_else(|err| panic!("not JSON ({err}): {}", self.text))
    }
}

pub(crate) fn choco_binary() -> PathBuf {
    let mut path = std::env::current_exe().expect("test binary has no path");
    path.pop();
    if path.ends_with("deps") {
        path.pop();
    }
    let path = path.join("choco");
    assert!(
        path.exists(),
        "{} is missing: run `cargo build --workspace --all-targets` first",
        path.display()
    );
    path
}

/// A `choco mcp-serve` child over stdio, as Claude would talk to it.
pub(crate) struct McpChild {
    pub child: tokio::process::Child,
    stdin: tokio::process::ChildStdin,
    lines: tokio::io::Lines<BufReader<tokio::process::ChildStdout>>,
    next_id: u64,
}

impl McpChild {
    pub(crate) fn spawn(extra_args: &[String], env: &[(&str, &str)]) -> Self {
        let mut command = Command::new(choco_binary());
        command
            .arg("mcp-serve")
            .args(extra_args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        for (key, value) in env {
            command.env(key, value);
        }
        let mut child = command.spawn().expect("spawn choco mcp-serve");
        let stdin = child.stdin.take().unwrap();
        let lines = BufReader::new(child.stdout.take().unwrap()).lines();
        Self {
            child,
            stdin,
            lines,
            next_id: 0,
        }
    }

    pub(crate) async fn send(&mut self, request: Value) {
        self.stdin
            .write_all(format!("{request}\n").as_bytes())
            .await
            .unwrap();
        self.stdin.flush().await.unwrap();
    }

    pub(crate) async fn next_line(&mut self) -> Value {
        let line = tokio::time::timeout(Duration::from_secs(60), self.lines.next_line())
            .await
            .expect("choco mcp-serve did not answer")
            .unwrap()
            .expect("choco mcp-serve closed its output");
        serde_json::from_str(&line).unwrap()
    }

    /// One request and its response.
    pub(crate) async fn rpc(&mut self, method: &str, params: Value) -> Value {
        self.next_id += 1;
        self.send(
            json!({"jsonrpc": "2.0", "id": self.next_id, "method": method,
                         "params": params}),
        )
        .await;
        let response = self.next_line().await;
        assert_eq!(response["id"], json!(self.next_id));
        response
    }

    pub(crate) async fn initialize(&mut self) {
        let response = self
            .rpc("initialize", json!({"protocolVersion": "2025-06-18"}))
            .await;
        assert!(response["result"]["serverInfo"].is_object(), "{response}");
        self.send(json!({"jsonrpc": "2.0", "method": "notifications/initialized"}))
            .await;
    }

    pub(crate) async fn call(&mut self, name: &str, arguments: Option<Value>) -> ToolReply {
        let mut params = json!({ "name": name });
        if let Some(arguments) = arguments {
            params["arguments"] = arguments;
        }
        let response = self.rpc("tools/call", params).await;
        let result = &response["result"];
        ToolReply {
            is_error: result["isError"]
                .as_bool()
                .unwrap_or_else(|| panic!("{response}")),
            text: result["content"][0]["text"].as_str().unwrap().to_string(),
        }
    }

    #[allow(dead_code)]
    pub(crate) async fn tool_names(&mut self) -> Vec<String> {
        let response = self.rpc("tools/list", json!({})).await;
        response["result"]["tools"]
            .as_array()
            .unwrap()
            .iter()
            .map(|tool| tool["name"].as_str().unwrap().to_string())
            .collect()
    }

    /// Closes stdin as a client going away does, and waits for the exit.
    pub(crate) async fn finish(mut self) -> std::process::ExitStatus {
        drop(self.stdin);
        tokio::time::timeout(Duration::from_secs(30), self.child.wait())
            .await
            .expect("choco mcp-serve did not exit")
            .unwrap()
    }
}

/// Every call of `calls` through a bound `choco mcp-serve`.
pub(crate) async fn via_choco(binding: &ContextBinding, calls: &[Value]) -> Vec<ToolReply> {
    let mut child = McpChild::spawn(
        &[
            vec!["--outcome".to_string(), "done".to_string()],
            binding.to_args(),
        ]
        .concat(),
        &[],
    );
    child.initialize().await;
    let mut replies = Vec::new();
    for call in calls {
        replies.push(child.call("get_context", Some(call.clone())).await);
    }
    assert!(child.finish().await.success());
    replies
}

/// The calls both surfaces are given, and what each must come back with.
/// `a` has results `r1, r2, r3` (again, again, approved); the binding's
/// anchor is `anchor`; `foreign` is another task's result.
pub(crate) fn full_result_calls(a: &TaskFacts, foreign: &str) -> Vec<Value> {
    vec![
        json!({"source": "task"}),
        json!({"source": "stage", "stage": "gate"}),
        json!({"source": "stage", "stage": "gate", "outcome": "again"}),
        json!({"source": "stage", "stage": "gate", "outcome": "approved"}),
        json!({"source": "stage", "result_id": a.results[0]}),
        json!({"source": "stage", "result_id": a.results[2]}),
        json!({"source": "stage", "result_id": foreign}),
        // Selectors with punctuation stay exactly what they are.
        json!({"source": "stage", "stage": "ga&te=1"}),
        json!({"source": "stage", "stage": "gate", "outcome": "again&stage=gate"}),
        json!({"source": "stage", "result_id": format!("{}&anchor_id={}", a.results[2], a.results[2])}),
        // Attempts to steer the tool elsewhere.
        json!({"source": "task", "task_id": foreign}),
        json!({"source": "stage", "stage": "gate", "anchor_id": a.results[2]}),
    ]
}

/// The assertions on `full_result_calls`' replies for a binding whose anchor
/// is `a.results[0]` (the earliest result), with later ones already recorded.
pub(crate) fn assert_full_results_at_first_result(
    a: &TaskFacts,
    replies: &[ToolReply],
    surface: &str,
) {
    let anchor = &a.results[0];
    let ok = |index: usize| {
        assert!(
            !replies[index].is_error,
            "{surface} call {index}: {}",
            replies[index].text
        );
        replies[index].json()
    };
    // The task's own facts.
    let task = ok(0);
    assert_eq!(task["provenance"], "task", "{surface}");
    assert_eq!(task["task_id"], json!(a.id));
    assert_eq!(task["task"]["title"], json!(a.title));
    assert_eq!(task["task"]["input"], json!(a.input));
    assert_eq!(task["task"]["closes"], json!([369]));
    // The latest `gate` at or before the anchor is the first correction, not
    // the approval recorded after the launch.
    let latest = ok(1);
    assert_eq!(latest["provenance"], "stage_result", "{surface}");
    assert_eq!(latest["anchor_id"], json!(anchor));
    let result = &latest["result"];
    assert_eq!(result["task_id"], json!(a.id));
    assert_eq!(result["result_id"], json!(anchor));
    assert_eq!(result["source_stage"], "gate");
    assert_eq!(result["source_kind"], "human_gate");
    assert_eq!(result["outcome"], "again");
    assert_eq!(result["destination_stage"], "gate");
    assert_eq!(result["capture_present"], json!(true));
    assert_eq!(result["previous_result_id"], Value::Null);
    assert!(result["created_at"].is_string());
    // The whole correction, however long, down to its last characters.
    let capture = result["capture"].as_str().unwrap();
    assert!(capture.len() > 2048, "{surface}: {} bytes", capture.len());
    assert_eq!(capture, a.captures[0], "{surface}");
    assert!(capture.ends_with(&format!("END-{}-CORRECTION-1", a.tag)));
    // An outcome filter agrees; one that never happened before the anchor
    // is unavailable, not the later approval.
    assert_eq!(ok(2), latest, "{surface}");
    assert_eq!(
        ok(3),
        json!({"provenance": "unavailable", "task_id": a.id, "anchor_id": anchor,
               "reason": "not_found_in_recorded_history"}),
        "{surface}"
    );
    // A result recorded after the launch is refused, with the full chain
    // present (the walk from the anchor was read to its end)...
    assert_eq!(
        ok(5),
        json!({"provenance": "unavailable", "task_id": a.id, "anchor_id": anchor,
               "reason": "not_at_or_before_anchor"}),
        "{surface}: a result newer than the launch's anchor"
    );
    // ...while the same-task exact result at or before it is returned.
    assert_eq!(ok(4), latest, "{surface}");
    // Another task's result: the same non-disclosing answer as a missing ID.
    assert_eq!(
        ok(6),
        json!({"provenance": "unavailable", "task_id": a.id, "anchor_id": anchor,
               "reason": "missing_result"}),
        "{surface}"
    );
    // Punctuation is data, not syntax: nothing is selected by accident.
    for index in [7, 8] {
        assert_eq!(
            ok(index)["reason"],
            "not_found_in_recorded_history",
            "{surface} {index}"
        );
    }
    assert_eq!(ok(9)["reason"], "missing_result", "{surface}");
    // Overrides are refused before any request is made.
    for index in [10, 11] {
        assert!(
            replies[index].is_error,
            "{surface} {index}: {}",
            replies[index].text
        );
    }
}

#[tokio::test]
async fn choco_returns_the_bound_tasks_full_results_not_newer_or_foreign_ones() {
    let (server, project) = server_with_flow().await;
    let mut a = new_task(&server, &project, "A", &[369]).await;
    let mut b = new_task(&server, &project, "B", &[]).await;
    a.reply(&server, "again").await;
    b.reply(&server, "again").await;
    // Bound now, at A's first result; everything after is newer.
    let binding =
        ContextBinding::new(&a.id, "session-a", Some(&a.results[0]), server.base_url()).unwrap();
    a.reply(&server, "again").await;
    a.reply(&server, "approved").await;
    assert_eq!(a.results.len(), 3);

    let calls = full_result_calls(&a, &b.results[0]);
    let replies = via_choco(&binding, &calls).await;
    assert_full_results_at_first_result(&a, &replies, "choco");
}

#[tokio::test]
async fn an_intervening_result_is_found_by_name_and_by_outcome_from_a_later_anchor() {
    let (server, project) = server_with_flow().await;
    let mut a = new_task(&server, &project, "A", &[]).await;
    for outcome in ["again", "again", "approved"] {
        a.reply(&server, outcome).await;
    }
    let binding = ContextBinding::new(&a.id, "s", Some(&a.results[1]), server.base_url()).unwrap();
    let replies = via_choco(
        &binding,
        &[
            json!({"source": "stage", "stage": "gate"}),
            json!({"source": "stage", "stage": "gate", "outcome": "again"}),
            json!({"source": "stage", "result_id": a.results[0]}),
            json!({"source": "stage", "result_id": a.results[2]}),
        ],
    )
    .await;
    let second = replies[0].json();
    // Not the approval that is the task's newest result.
    assert_eq!(second["result"]["result_id"], json!(a.results[1]));
    assert_eq!(second["result"]["capture"], json!(a.captures[1]));
    assert_eq!(second["result"]["previous_result_id"], json!(a.results[0]));
    assert_eq!(replies[1].json(), second);
    assert_eq!(replies[2].json()["result"]["capture"], json!(a.captures[0]));
    assert_eq!(replies[3].json()["reason"], "not_at_or_before_anchor");
}

/// A launch with no earlier result sees none later, and never the stored
/// snapshot a task from before results was recorded may hold.
#[tokio::test]
async fn an_empty_boundary_never_floats_to_later_results_or_a_stored_snapshot() {
    let (server, project) = server_with_flow().await;
    let mut c = new_task(&server, &project, "C", &[7]).await;
    // A pre-results capture with its own content, and no pointer.
    sqlx::query("UPDATE workflow_state SET payload = ? WHERE task_id = ?")
        .bind(
            json!({"task": {"input": c.input, "title": c.title},
                   "stages": {"gate": "LEGACY-SNAPSHOT-OF-C"}})
            .to_string(),
        )
        .bind(&c.id)
        .execute(server.pool())
        .await
        .unwrap();
    let c_id = c.id.clone();
    let raw = |query: &str| {
        let url = format!("{}/tasks/{c_id}/context?{query}", server.base_url());
        async move {
            reqwest::get(url)
                .await
                .unwrap()
                .json::<Value>()
                .await
                .unwrap()
        }
    };
    // The daemon's own lookup still answers from the snapshot.
    let before = raw("source=stage&stage=gate").await;
    assert_eq!(before["provenance"], "snapshot");
    assert_eq!(before["capture"], "LEGACY-SNAPSHOT-OF-C");

    let binding = ContextBinding::new(&c.id, "s", None, server.base_url()).unwrap();
    // Results are recorded after the launch.
    c.reply(&server, "again").await;
    c.reply(&server, "approved").await;
    let raw_now = raw("source=stage&stage=gate").await;
    assert_eq!(
        raw_now["provenance"], "stage_result",
        "later results do exist"
    );

    let replies = via_choco(
        &binding,
        &[
            json!({"source": "task"}),
            json!({"source": "stage", "stage": "gate"}),
            json!({"source": "stage", "stage": "gate", "outcome": "again"}),
            json!({"source": "stage", "result_id": c.results[0]}),
        ],
    )
    .await;
    assert_eq!(replies[0].json()["task"]["closes"], json!([7]));
    assert_eq!(replies[0].json()["task"]["input"], json!(c.input));
    let unavailable = json!({"provenance": "unavailable", "task_id": c.id, "anchor_id": null,
                             "reason": "not_found_in_recorded_history"});
    for reply in &replies[1..] {
        assert!(!reply.is_error, "{}", reply.text);
        assert_eq!(reply.json(), unavailable);
        assert!(!reply.text.contains("LEGACY-SNAPSHOT"));
        assert!(!reply.text.contains(&c.captures[0]));
    }

    // The task's existence is still checked: a deleted task is an error,
    // not a local "unavailable".
    for table in ["events", "sessions", "workflow_state"] {
        sqlx::query(sqlx::AssertSqlSafe(format!(
            "DELETE FROM {table} WHERE task_id = '{}'",
            c.id
        )))
        .execute(server.pool())
        .await
        .unwrap();
    }
    assert!(
        crate::db::tasks::delete(server.pool(), &c.id)
            .await
            .unwrap()
    );
    let replies = via_choco(
        &binding,
        &[
            json!({"source": "task"}),
            json!({"source": "stage", "stage": "gate"}),
        ],
    )
    .await;
    assert!(replies.iter().all(|reply| reply.is_error), "{replies:?}");
}

#[tokio::test]
async fn a_pruned_anchor_and_a_pruned_link_are_named_not_invented() {
    let (server, project) = server_with_flow().await;
    let mut a = new_task(&server, &project, "A", &[]).await;
    for outcome in ["again", "again", "approved"] {
        a.reply(&server, outcome).await;
    }
    let at_latest =
        ContextBinding::new(&a.id, "s", Some(&a.results[2]), server.base_url()).unwrap();
    let at_missing =
        ContextBinding::new(&a.id, "s", Some("no-such-result"), server.base_url()).unwrap();
    // Retention pruned the first result: a link needed to reach it is gone.
    sqlx::query("DELETE FROM events WHERE id = ?")
        .bind(&a.results[0])
        .execute(server.pool())
        .await
        .unwrap();
    let gap = via_choco(
        &at_latest,
        &[json!({"source": "stage", "stage": "gate", "outcome": "nothing"})],
    )
    .await;
    assert_eq!(gap[0].json()["reason"], "history_gap");
    assert!(!gap[0].is_error);
    let missing = via_choco(
        &at_missing,
        &[
            json!({"source": "stage", "stage": "gate"}),
            json!({"source": "stage", "result_id": a.results[1]}),
        ],
    )
    .await;
    for reply in &missing {
        assert!(!reply.is_error, "{}", reply.text);
        assert_eq!(reply.json()["reason"], "missing_anchor");
        assert_eq!(reply.json()["anchor_id"], "no-such-result");
    }
}

/// A fixture daemon that answers every context request with valid but
/// different content, under the same task and result IDs it was asked for.
/// Stands in for a guessed or default address.
pub(crate) async fn decoy_daemon() -> String {
    use axum::extract::{Path, Query};
    let app = axum::Router::new().route(
        "/tasks/{id}/context",
        axum::routing::get(
            |Path(id): Path<String>,
             Query(query): Query<std::collections::HashMap<String, String>>| async move {
                let body = if query.get("source").map(String::as_str) == Some("task") {
                    json!({"provenance": "task", "task_id": id, "anchor_id": null,
                           "task": {"title": "DECOY TITLE", "input_available": true,
                                    "input": "DECOY INPUT", "closes": [1]}})
                } else {
                    json!({"provenance": "stage_result", "task_id": id,
                           "anchor_id": query.get("anchor_id"),
                           "result": {"task_id": id,
                                      "result_id": query.get("result_id").cloned()
                                          .unwrap_or_else(|| "decoy-result".to_string()),
                                      "source_stage": "gate", "source_kind": "human_gate",
                                      "outcome": "again", "destination_stage": "gate",
                                      "capture_present": true, "capture": "DECOY CAPTURE",
                                      "previous_result_id": null,
                                      "created_at": "2026-11-15T10:00:00Z"}})
                };
                axum::Json(body)
            },
        ),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    base
}

#[tokio::test]
async fn the_decoy_daemon_gives_different_content_under_the_same_ids() {
    // The control for every wrong-address check: it is a valid answer, so a
    // client pointed at it is told apart by content, not by an error.
    let (server, project) = server_with_flow().await;
    let mut a = new_task(&server, &project, "A", &[]).await;
    a.reply(&server, "again").await;
    let decoy = decoy_daemon().await;
    let real = ContextBinding::new(&a.id, "s", Some(&a.results[0]), server.base_url()).unwrap();
    let wrong = ContextBinding::new(&a.id, "s", Some(&a.results[0]), &decoy).unwrap();
    let call = [
        json!({"source": "task"}),
        json!({"source": "stage", "stage": "gate"}),
    ];
    let right = via_choco(&real, &call).await;
    let other = via_choco(&wrong, &call).await;
    assert_eq!(right[0].json()["task"]["title"], json!(a.title));
    assert_eq!(other[0].json()["task"]["title"], "DECOY TITLE");
    assert_eq!(right[1].json()["result"]["capture"], json!(a.captures[0]));
    assert_eq!(other[1].json()["result"]["capture"], "DECOY CAPTURE");
}

// ---------------------------------------------------------------------------
// Opt-in probes with the real CLIs (paid; never part of the default suite)
// ---------------------------------------------------------------------------

/// One real turn: the agent reads the human's private correction with
/// `get_context`, quotes its last line in its report. The history is
/// recorded by a private in-process daemon on an ephemeral port with its own
/// in-memory database; the CLI keeps the operator's real HOME and login.
///
/// `sandboxed` is the workflow's `worktree:` (a disposable checkout the CLI
/// may act freely in) — false is the "real checkout" mode that leaves the
/// CLI's own permission prompts on.
///
/// Run with `CHOCO_REAL_PROBE=claude`, `omp` or `omp-unsandboxed`:
/// `cargo test -p chocofactoryd --lib real_probe -- --ignored --nocapture`
async fn real_probe(cli: &str, model: &str, sandboxed: bool) {
    use std::sync::Arc;
    let choco = choco_binary();
    let server = TestServer::start_with_adapters(
        |dir| {
            let claude: Arc<dyn crate::adapter::AgentAdapter> = Arc::new(
                crate::adapter::ClaudeAdapter::new().with_choco_binary(choco.display().to_string()),
            );
            let omp: Arc<dyn crate::adapter::AgentAdapter> =
                Arc::new(crate::adapter::OmpAdapter::new(dir.join("omp-state")));
            vec![claude, omp]
        },
        &choco.display().to_string(),
    )
    .await;
    // A disposable git repository the read-only turn runs in.
    let repo = server.temp_dir();
    for args in [
        vec!["init", "-q"],
        vec![
            "-c",
            "user.email=a@b.c",
            "-c",
            "user.name=probe",
            "commit",
            "-q",
            "--allow-empty",
            "-m",
            "init",
        ],
    ] {
        let status = std::process::Command::new("git")
            .arg("-C")
            .arg(&repo)
            .args(&args)
            .status()
            .unwrap();
        assert!(status.success());
    }
    let sentinel = format!("PROBE-SENTINEL-{}", uuid::Uuid::new_v4().simple());
    server.write_workflow(
        "probe",
        &format!(
            "name: probe\nworktree: {sandboxed}\nroles:\n  prober:\n    cli: {cli}\n    model: {model}\n    read_only: {sandboxed}\n    disallowed_tools: [edit, write, notebook_edit]\nstages:\n  gate:\n    kind: human_gate\n    capture: text\n    markers:\n      - line: /fix\n        then: fixed\n    on: {{ fixed: probing }}\n  probing:\n    kind: agent_turn\n    role: prober\n    prompt_file: probe.md\n    on: {{ done: finished }}\n  finished:\n    kind: terminal\n"
        ),
    );
    std::fs::write(
        server
            .builtin_workflow_path("probe")
            .with_file_name("probe.md"),
        "A human left a private correction for this task at the stage named `gate`; it is not in \
         this prompt. Use the get_context tool to read it (source `stage`, stage `gate`). Then \
         call report_outcome with outcome `done` and a summary that quotes the correction's \
         last line exactly. Use no other tools.",
    )
    .unwrap();
    let project: Value = server
        .post("/projects", json!({"name": "probe"}))
        .await
        .json();
    let created: Value = server
        .post(
            "/tasks",
            json!({"project_id": project["id"], "workflow_def": "probe", "title": "probe",
                   "prompt": "probe", "config": {"cwd": repo.to_string_lossy()}}),
        )
        .await
        .json();
    let id = created["id"].as_str().unwrap().to_string();
    let correction = format!("Change nothing but the retry loop.\n{PADDING}\n{sentinel}\n/fix");
    let response = server
        .post(
            &format!("/tasks/{id}/messages"),
            json!({"text": correction}),
        )
        .await;
    assert_eq!(response.status(), 202, "{}", response.json());

    // Up to ten minutes for the real turn.
    let deadline = std::time::Instant::now() + Duration::from_secs(600);
    let status = loop {
        let task = server.get(&format!("/tasks/{id}")).await.json();
        let status = task["status"].as_str().unwrap_or("").to_string();
        if matches!(status.as_str(), "closed" | "stuck" | "cancelled") {
            break status;
        }
        if std::time::Instant::now() > deadline {
            let _ = server.post_empty(&format!("/tasks/{id}/cancel")).await;
            panic!("the real turn did not finish in ten minutes");
        }
        tokio::time::sleep(Duration::from_secs(2)).await;
    };
    let events = crate::db::events::list_for_task(server.pool(), &id)
        .await
        .unwrap();
    for event in &events {
        if matches!(
            event.event_type,
            EventType::ToolCall | EventType::ToolResult
        ) {
            let text = event.payload.to_string();
            println!("{} {}", event.event_type, &text[..text.len().min(400)]);
        }
    }
    let qualified = chocofactory_core::mcp::qualified_get_context_tool_name();
    let call = events
        .iter()
        .find(|e| e.event_type == EventType::ToolCall && e.payload["tool"] == json!(qualified))
        .expect("the agent never called get_context");
    println!("get_context input: {}", call.payload["input"]);
    let result = events
        .iter()
        .find(|e| {
            e.event_type == EventType::ToolResult
                && e.payload["tool_use_id"] == call.payload["tool_use_id"]
        })
        .expect("no result for the get_context call");
    assert_eq!(
        result.payload["is_error"],
        json!(false),
        "{}",
        result.payload
    );
    assert!(
        result.payload["output"]
            .as_str()
            .unwrap()
            .contains(&sentinel)
    );
    let report = events
        .iter()
        .find(|e| {
            e.event_type == EventType::ToolCall
                && e.payload["tool"]
                    == json!(chocofactory_core::mcp::qualified_report_outcome_tool_name())
        })
        .expect("the agent never reported");
    assert!(
        report.payload["input"]["summary"]
            .as_str()
            .unwrap_or("")
            .contains(&sentinel),
        "{}",
        report.payload
    );
    assert_eq!(status, "closed");
}

#[tokio::test]
#[ignore = "paid: one real Claude turn; run with CHOCO_REAL_PROBE=claude"]
async fn real_probe_claude() {
    if std::env::var("CHOCO_REAL_PROBE").as_deref() != Ok("claude") {
        return;
    }
    real_probe("claude", "claude-sonnet-5-5:medium", true).await;
}

#[tokio::test]
#[ignore = "paid: one real omp turn; run with CHOCO_REAL_PROBE=omp"]
async fn real_probe_omp() {
    let sandboxed = match std::env::var("CHOCO_REAL_PROBE").as_deref() {
        Ok("omp") => true,
        Ok("omp-unsandboxed") => false,
        _ => return,
    };
    real_probe("omp", "openai-codex/gpt-6.1-sol:high", sandboxed).await;
}

/// A daemon whose `GET /tasks/{id}/context` runs the real lookup against
/// `pool` but holds the lookup's read transaction open after the anchor was
/// read, until released. The pool the tests use has one connection, so while
/// the read is held every other statement waits on it: a write that
/// completes afterwards shows the connection was really given back.
pub(crate) struct HeldDaemon {
    pub base: String,
    /// Lookups that reached the held point (their transaction is open).
    pub held: std::sync::Arc<std::sync::atomic::AtomicUsize>,
    release: std::sync::Arc<tokio::sync::Notify>,
    pool: sqlx::SqlitePool,
    stop: std::sync::Arc<tokio::sync::Notify>,
    server: tokio::task::JoinHandle<()>,
}

impl HeldDaemon {
    /// Asks the private daemon to shut down gracefully and requires it to
    /// finish: a request or worker still held by an abandoned read would
    /// keep a graceful shutdown waiting.
    pub(crate) async fn shut_down(self) {
        self.stop.notify_one();
        tokio::time::timeout(Duration::from_secs(30), self.server)
            .await
            .expect("the private daemon did not shut down: something is still held")
            .expect("the private daemon's server task failed");
    }

    /// Lets the held lookup finish.
    pub(crate) fn release(&self) {
        self.release.notify_one();
    }

    /// Waits until the held lookup's connection is back in the pool, which is
    /// the transaction's release.
    pub(crate) async fn wait_until_the_read_is_released(&self) {
        let pool = self.pool.clone();
        crate::test_support::wait_until("the read transaction to be released", || async {
            if pool.num_idle() == pool.size() as usize {
                Ok(())
            } else {
                Err(format!("{} of {} idle", pool.num_idle(), pool.size()))
            }
        })
        .await;
    }

    pub(crate) async fn wait_until_held(&self) {
        let held = std::sync::Arc::clone(&self.held);
        crate::test_support::wait_until("the lookup to hold its transaction", || async {
            if held.load(std::sync::atomic::Ordering::SeqCst) == 1 {
                Ok(())
            } else {
                Err("not yet".to_string())
            }
        })
        .await;
    }
}

pub(crate) async fn held_lookup_daemon(pool: sqlx::SqlitePool) -> HeldDaemon {
    use axum::extract::{Path, Query, State};
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    let held = Arc::new(AtomicUsize::new(0));
    let release = Arc::new(tokio::sync::Notify::new());
    #[derive(Clone)]
    struct Shared {
        pool: sqlx::SqlitePool,
        held: Arc<AtomicUsize>,
        release: Arc<tokio::sync::Notify>,
    }
    let shared = Shared {
        pool: pool.clone(),
        held: Arc::clone(&held),
        release: Arc::clone(&release),
    };
    async fn handler(
        State(shared): State<Shared>,
        Path(id): Path<String>,
        Query(pairs): Query<Vec<(String, String)>>,
    ) -> (axum::http::StatusCode, axum::Json<Value>) {
        use crate::context::Lookup;
        let request = crate::api::tasks::parse_context_query(&pairs)
            .unwrap_or_else(|_| panic!("a malformed query reached the daemon: {pairs:?}"));
        let (held, release) = (Arc::clone(&shared.held), Arc::clone(&shared.release));
        let looked_up =
            crate::context::lookup_with_pause(&shared.pool, &id, &request, || async move {
                held.fetch_add(1, Ordering::SeqCst);
                release.notified().await;
            })
            .await
            .unwrap();
        match looked_up {
            Lookup::Found(answer) => (
                axum::http::StatusCode::OK,
                axum::Json(serde_json::to_value(answer).unwrap()),
            ),
            Lookup::UnknownId(answer) => (
                axum::http::StatusCode::NOT_FOUND,
                axum::Json(serde_json::to_value(answer).unwrap()),
            ),
            Lookup::NoSuchTask => (
                axum::http::StatusCode::NOT_FOUND,
                axum::Json(json!({"error": "no such task"})),
            ),
        }
    }
    let app = axum::Router::new()
        .route("/tasks/{id}/context", axum::routing::get(handler))
        .with_state(shared);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let stop = Arc::new(tokio::sync::Notify::new());
    let stopped = Arc::clone(&stop);
    let server = tokio::spawn(async move {
        axum::serve(listener, app)
            .with_graceful_shutdown(async move { stopped.notified().await })
            .await
            .unwrap()
    });
    HeldDaemon {
        base,
        held,
        release,
        pool,
        stop,
        server,
    }
}

/// Serves the real `GET /tasks/{id}/context` lookup over `pool` on an
/// already-bound listener (so an engine can be told its address first) and
/// counts every request it answers.
pub(crate) fn serve_context(
    listener: tokio::net::TcpListener,
    pool: sqlx::SqlitePool,
) -> std::sync::Arc<std::sync::atomic::AtomicUsize> {
    serve_context_with_decoy(listener, pool, Default::default()).0
}

/// [`serve_context`] with a switch: while it is set, every answer keeps its
/// task, result IDs and shape but carries other content (a different
/// capture and title). Switched on, the listener stands for a daemon at the
/// wrong address: a launch sent there reads valid, different content under
/// the very IDs it asked for.
pub(crate) fn serve_context_with_decoy(
    listener: tokio::net::TcpListener,
    pool: sqlx::SqlitePool,
    decoy: std::sync::Arc<std::sync::atomic::AtomicBool>,
) -> (
    std::sync::Arc<std::sync::atomic::AtomicUsize>,
    std::sync::Arc<std::sync::atomic::AtomicBool>,
) {
    use axum::extract::{Path, Query, State};
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    #[derive(Clone)]
    struct Shared {
        pool: sqlx::SqlitePool,
        hits: Arc<AtomicUsize>,
        decoy: Arc<AtomicBool>,
    }
    async fn handler(
        State(shared): State<Shared>,
        Path(id): Path<String>,
        Query(pairs): Query<Vec<(String, String)>>,
    ) -> (axum::http::StatusCode, axum::Json<Value>) {
        use crate::context::Lookup;
        shared.hits.fetch_add(1, Ordering::SeqCst);
        let request = crate::api::tasks::parse_context_query(&pairs)
            .unwrap_or_else(|_| panic!("a malformed query reached the daemon: {pairs:?}"));
        let (status, mut body) = match crate::context::lookup(&shared.pool, &id, &request)
            .await
            .unwrap()
        {
            Lookup::Found(answer) => (
                axum::http::StatusCode::OK,
                serde_json::to_value(answer).unwrap(),
            ),
            Lookup::UnknownId(answer) => (
                axum::http::StatusCode::NOT_FOUND,
                serde_json::to_value(answer).unwrap(),
            ),
            Lookup::NoSuchTask => (
                axum::http::StatusCode::NOT_FOUND,
                json!({"error": "no such task"}),
            ),
        };
        if shared.decoy.load(Ordering::SeqCst) {
            if body["result"].is_object() && !body["result"]["capture"].is_null() {
                body["result"]["capture"] = json!("DECOY-CAPTURE");
            }
            if body["task"].is_object() {
                body["task"]["title"] = json!("DECOY-TITLE");
            }
        }
        (status, axum::Json(body))
    }
    let hits = Arc::new(AtomicUsize::new(0));
    let app = axum::Router::new()
        .route("/tasks/{id}/context", axum::routing::get(handler))
        .with_state(Shared {
            pool,
            hits: Arc::clone(&hits),
            decoy: Arc::clone(&decoy),
        });
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (hits, decoy)
}

/// A read abandoned by Ctrl-C while the daemon's lookup holds its read
/// transaction changes nothing on the daemon and holds nothing: the
/// transaction is released, a later read and a later write both complete,
/// and nothing is left that a shutdown would wait for.
#[tokio::test(flavor = "multi_thread")]
async fn an_abandoned_read_changes_nothing_and_blocks_neither_a_read_nor_a_write() {
    use std::os::unix::process::ExitStatusExt;
    let (server, project) = server_with_flow().await;
    let mut a = new_task(&server, &project, "A", &[]).await;
    a.reply(&server, "again").await;
    let held = held_lookup_daemon(server.pool().clone()).await;
    let through_held = ContextBinding::new(&a.id, "s", Some(&a.results[0]), &held.base).unwrap();
    // Every row the daemon keeps, once the fixture's own writes are done.
    let before = crate::test_support::dump_tables(server.pool()).await;

    let mut child = McpChild::spawn(&through_held.to_args(), &[]);
    child.initialize().await;
    child
        .send(json!({"jsonrpc": "2.0", "id": 90, "method": "tools/call",
                     "params": {"name": "get_context",
                                "arguments": {"source": "stage", "stage": "gate"}}}))
        .await;
    // The daemon is inside the lookup, holding its read transaction (the
    // pool's only connection).
    held.wait_until_held().await;
    assert_eq!(server.pool().num_idle(), 0, "the read holds the connection");
    let pid = child.child.id().unwrap() as libc::pid_t;
    assert_eq!(unsafe { libc::kill(pid, libc::SIGINT) }, 0);
    let status = tokio::time::timeout(Duration::from_secs(30), child.child.wait())
        .await
        .expect("the abandoned server did not exit")
        .unwrap();
    assert_eq!(status.signal(), Some(libc::SIGINT), "{status:?}");
    // A lookup that outlived its client finishes when let go; either way
    // the transaction must end, so the connection is back in the pool.
    held.release();
    held.wait_until_the_read_is_released().await;

    assert_eq!(
        crate::test_support::dump_tables(server.pool()).await,
        before,
        "an abandoned read wrote nothing"
    );
    // A later read, straight to the daemon...
    let direct = ContextBinding::new(&a.id, "s", Some(&a.results[0]), server.base_url()).unwrap();
    let again = via_choco(&direct, &[json!({"source": "stage", "stage": "gate"})]).await;
    assert_eq!(again[0].json()["result"]["capture"], json!(a.captures[0]));
    // ...and a write: the gate takes another reply and records its result.
    a.reply(&server, "approved").await;
    assert_eq!(a.results.len(), 2);
    // Finally the private daemon is asked to shut down and must be able to.
    held.shut_down().await;
}

// ---------------------------------------------------------------------------
// The launch path end to end: what the engine hands a turn reads the right data
// ---------------------------------------------------------------------------

const LAUNCH_FLOW: &str = "name: launchflow\nworktree: false\nroles:\n  coder:\n    cli: claude\n    model: sonnet\nstages:\n  gate:\n    kind: human_gate\n    capture: text\n    markers:\n      - line: /go\n        then: go\n    on: { go: coding }\n  coding:\n    kind: agent_turn\n    role: coder\n    prompt_file: launch.md\n    on: { done: finished }\n  finished:\n    kind: terminal\n";

/// The `mcp-serve` arguments the engine really gave a launched `claude`
/// (echoed back by `fake_claude_echo_args.py`).
async fn launched_mcp_args(server: &TestServer, task_id: &str) -> Vec<String> {
    let session = crate::test_support::wait_until("the coding session's reply", || async {
        let sessions = crate::db::sessions::list_for_task(server.pool(), task_id)
            .await
            .map_err(|err| err.to_string())?;
        let session = sessions
            .into_iter()
            .find(|s| s.stage == "coding")
            .ok_or("no coding session yet".to_string())?;
        let text = crate::db::events::final_assistant_text_for_session(server.pool(), &session.id)
            .await
            .map_err(|err| err.to_string())?;
        if text.contains("|mcp_config=") {
            Ok(text)
        } else {
            Err(format!("reply so far: {text:?}"))
        }
    })
    .await;
    let config = session
        .split("|mcp_config=")
        .nth(1)
        .and_then(|rest| rest.split("|strict_mcp_config=").next())
        .expect("the stand-in echoes its mcp config");
    let config: Value = serde_json::from_str(config).unwrap();
    config["mcpServers"]["chocofactory"]["args"]
        .as_array()
        .unwrap()
        .iter()
        .map(|arg| arg.as_str().unwrap().to_string())
        .collect()
}

/// Two tasks go through the same workflow on one daemon, one after the other.
/// Each task's launched turn, running the tool exactly as it was launched,
/// reads its own task's facts and its own gate reply: neither the other
/// task's, nor the daemon's newest, nor another address's.
#[tokio::test]
async fn a_launched_turn_reads_its_own_task_and_its_own_gate_reply_from_the_daemon_that_ran_it() {
    let choco = choco_binary();
    let echo =
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/fake_claude_echo_args.py");
    let server = TestServer::start_with_adapters(
        |_| {
            let adapter: std::sync::Arc<dyn crate::adapter::AgentAdapter> = std::sync::Arc::new(
                crate::adapter::ClaudeAdapter::with_binary(echo.display().to_string())
                    .with_choco_binary(choco.display().to_string()),
            );
            vec![adapter]
        },
        &choco.display().to_string(),
    )
    .await;
    server.write_workflow("launchflow", LAUNCH_FLOW);
    std::fs::write(
        server
            .builtin_workflow_path("launchflow")
            .with_file_name("launch.md"),
        "Work on it.",
    )
    .unwrap();
    let project: Value = server
        .post("/projects", json!({"name": "demo"}))
        .await
        .json();
    let decoy = decoy_daemon().await;

    let mut launched = Vec::new();
    for tag in ["A", "B"] {
        let created: Value = server
            .post(
                "/tasks",
                json!({"project_id": project["id"], "workflow_def": "launchflow",
                       "title": format!("Title of {tag}"), "prompt": format!("brief {tag}"),
                       "closes": [if tag == "A" { 1 } else { 2 }], "config": {"cwd": "."}}),
            )
            .await
            .json();
        let id = created["id"].as_str().unwrap().to_string();
        let reply = format!("Reply for {tag}: change {tag} only.\n/go");
        let response = server
            .post(&format!("/tasks/{id}/messages"), json!({"text": reply}))
            .await;
        assert_eq!(response.status(), 202);
        let args = launched_mcp_args(&server, &id).await;
        launched.push((tag, id, args));
    }

    for (tag, id, args) in &launched {
        let flag = |name: &str| args[args.iter().position(|a| a == name).unwrap() + 1].clone();
        let mut mcp = McpChild::spawn(&args[1..], &[]);
        mcp.initialize().await;
        let task = mcp
            .call("get_context", Some(json!({"source": "task"})))
            .await;
        assert!(!task.is_error, "{}", task.text);
        assert_eq!(
            task.json()["task"]["title"],
            json!(format!("Title of {tag}")),
            "{tag}"
        );
        assert_eq!(task.json()["task_id"], json!(id));
        let gate = mcp
            .call(
                "get_context",
                Some(json!({"source": "stage", "stage": "gate"})),
            )
            .await;
        assert!(!gate.is_error, "{}", gate.text);
        assert_eq!(
            gate.json()["result"]["capture"],
            json!(format!("Reply for {tag}: change {tag} only.")),
            "{tag}"
        );
        assert_eq!(gate.json()["result"]["task_id"], json!(id));
        assert!(mcp.finish().await.success());
        // And the flags themselves name this task and this daemon.
        assert_eq!(&flag("--context-task-id"), id);
        assert_eq!(
            flag("--context-base-url"),
            format!("{}/", server.base_url())
        );
        assert_ne!(flag("--context-base-url"), format!("{decoy}/"));
    }
}
