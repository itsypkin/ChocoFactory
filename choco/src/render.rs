//! Human-readable rendering of API responses (design Q12: `choco` is both
//! human-scriptable and agent-callable — this is the human half, `--json`
//! is the machine half). No colour/ANSI: output is routinely piped, and
//! this repo ships no terminal-styling dependency.

use chokofactory_core::models::{Event, EventType, Project, Task};
use chrono::{DateTime, Utc};
use serde_json::Value;

use crate::client::EventsPage;

/// `2026-08-01 11:55:07 UTC` — RFC3339 with the sub-second precision and
/// `T`/`Z` punctuation dropped, which is unreadable at a glance and never
/// what a human is scanning for.
fn timestamp(at: &DateTime<Utc>) -> String {
    at.format("%Y-%m-%d %H:%M:%S UTC").to_string()
}

/// Parses one of `created_at`'s RFC3339 strings back out of raw JSON (used
/// for `task status`, which passes the daemon's `TaskDetail` through
/// untyped). Falls back to the raw string when it isn't a timestamp.
fn timestamp_str(raw: &str) -> String {
    DateTime::parse_from_rfc3339(raw)
        .map(|at| timestamp(&at.with_timezone(&Utc)))
        .unwrap_or_else(|_| raw.to_string())
}

/// Left-aligned columns padded to the widest cell, skipping trailing
/// padding on the last column so lines don't carry invisible whitespace.
fn table(headers: &[&str], rows: &[Vec<String>]) -> String {
    let mut widths: Vec<usize> = headers.iter().map(|h| h.chars().count()).collect();
    for row in rows {
        for (i, cell) in row.iter().enumerate() {
            if i < widths.len() {
                widths[i] = widths[i].max(cell.chars().count());
            }
        }
    }

    let render_row = |cells: &[String]| -> String {
        let mut line = String::new();
        for (i, cell) in cells.iter().enumerate() {
            if i + 1 == cells.len() {
                line.push_str(cell);
            } else {
                // `.get` rather than `widths[i]`: a row longer than the
                // header list would otherwise panic mid-render.
                let width = widths.get(i).copied().unwrap_or(0);
                line.push_str(&format!("{:<width$}  ", cell, width = width));
            }
        }
        // An empty final cell (e.g. an event with no renderable payload)
        // would otherwise leave the separator dangling at end of line.
        line.trim_end().to_string()
    };

    let header: Vec<String> = headers.iter().map(|h| h.to_string()).collect();
    let mut out = vec![render_row(&header)];
    out.extend(rows.iter().map(|r| render_row(r)));
    out.join("\n")
}

/// `key   value` pairs aligned on the value column.
fn fields(pairs: &[(&str, String)]) -> String {
    let width = pairs
        .iter()
        .map(|(k, _)| k.chars().count())
        .max()
        .unwrap_or(0);
    pairs
        .iter()
        .map(|(k, v)| {
            format!("{:<width$}  {}", k, v, width = width)
                .trim_end()
                .to_string()
        })
        .collect::<Vec<_>>()
        .join("\n")
}

pub fn project(p: &Project) -> String {
    fields(&[
        ("Name", single_line(&p.name)),
        ("ID", p.id.clone()),
        ("Created", timestamp(&p.created_at)),
    ])
}

pub fn projects(list: &[Project]) -> String {
    if list.is_empty() {
        return "No projects yet. Create one with `choco project create <name>`.".to_string();
    }
    let rows: Vec<Vec<String>> = list
        .iter()
        .map(|p| vec![one_line(&p.name), p.id.clone(), timestamp(&p.created_at)])
        .collect();
    table(&["NAME", "ID", "CREATED"], &rows)
}

pub fn task(t: &Task) -> String {
    let mut pairs = vec![
        ("Title", single_line(&t.title)),
        ("ID", t.id.clone()),
        ("Project", t.project_id.clone()),
        ("Workflow", t.workflow_def.clone()),
        ("Status", t.status.clone()),
    ];
    if let Some(parent) = &t.parent_task_id {
        pairs.push(("Parent task", parent.clone()));
    }
    pairs.push(("Created", timestamp(&t.created_at)));
    fields(&pairs)
}

pub fn tasks(list: &[Task]) -> String {
    if list.is_empty() {
        return "No tasks matched.".to_string();
    }
    let rows: Vec<Vec<String>> = list
        .iter()
        .map(|t| {
            vec![
                one_line(&t.title),
                t.id.clone(),
                t.status.clone(),
                t.workflow_def.clone(),
                timestamp(&t.created_at),
            ]
        })
        .collect();
    table(&["TITLE", "ID", "STATUS", "WORKFLOW", "CREATED"], &rows)
}

/// Renders the daemon's `TaskDetail` (a `Task` flattened alongside
/// `workflow_state`) from raw JSON — it has no exported Rust type.
pub fn task_detail(detail: &Value) -> String {
    let get = |key: &str| detail.get(key).and_then(Value::as_str).unwrap_or("-");

    let mut pairs = vec![
        ("Title", single_line(get("title"))),
        ("ID", get("id").to_string()),
        ("Project", get("project_id").to_string()),
        ("Workflow", get("workflow_def").to_string()),
        ("Status", get("status").to_string()),
    ];
    if let Some(parent) = detail.get("parent_task_id").and_then(Value::as_str) {
        pairs.push(("Parent task", parent.to_string()));
    }
    pairs.push(("Created", timestamp_str(get("created_at"))));

    let state = detail.get("workflow_state");
    let current = state
        .and_then(|s| s.get("current_stage"))
        .and_then(Value::as_str);
    if let Some(current) = current {
        pairs.push(("Stage", current.to_string()));
    }

    let mut out = fields(&pairs);

    if let Some(state) = state.filter(|s| !s.is_null()) {
        // The trail is a sibling of `workflow_state`, not a field inside
        // it: X-3 moved it out of `stage_history` and into the events
        // timeline, which the daemon re-exposes here as `stage_trail`.
        let trail = detail
            .get("stage_trail")
            .and_then(Value::as_array)
            .map(Vec::as_slice)
            .unwrap_or_default();
        out.push_str("\n\nProgress\n");
        out.push_str(&stage_progress(trail, current));

        let counters = state.get("loop_counters");
        if let Some(counters) = counters
            .and_then(Value::as_object)
            .filter(|c| !c.is_empty())
        {
            let rendered: Vec<String> = counters
                .iter()
                .map(|(stage, count)| format!("{stage}={count}"))
                .collect();
            out.push_str(&format!("\n\nLoop counters  {}", rendered.join(" ")));
        }
    } else {
        out.push_str("\n\n(no workflow state — the task has not started)");
    }

    out
}

/// The stage trail as a timeline, ending at the current stage.
///
/// Entries are `stage_entered` events (X-3), so each names a stage the task
/// *entered* and the outcome that selected it — the previous
/// `stage_history` shape named the stage it *departed* and where it was
/// headed. The hop arrow is reconstructed by pairing each entry with its
/// predecessor, which reads the same as before while gaining the entry
/// stage: `stage_history` only ever appended on the way out, so the stage a
/// task started in was never in the trail at all.
///
/// A task that ran before X-3 has no `stage_entered` events and no
/// backfill, so its trail is legitimately empty and renders as "no
/// transitions yet" rather than being reconstructed from data that isn't
/// there.
fn stage_progress(trail: &[Value], current: Option<&str>) -> String {
    let stage_of = |entry: &Value| {
        entry
            .get("payload")
            .and_then(|p| p.get("stage"))
            .and_then(Value::as_str)
            .unwrap_or("?")
            .to_string()
    };

    let mut lines = Vec::new();
    for (i, entry) in trail.iter().enumerate() {
        let step = i + 1;
        let stage = stage_of(entry);
        let at = entry
            .get("created_at")
            .and_then(Value::as_str)
            .map(timestamp_str)
            .map(|at| format!("   {at}"))
            .unwrap_or_default();

        // A null `outcome` — not a missing predecessor — is what marks a
        // starting point: the engine writes `entered_via: None` only for a
        // stage nothing transitioned into. Keying on the predecessor
        // instead would label the *first surviving* entry "(start)" on a
        // trail whose head has been truncated, inventing a beginning that
        // never happened and discarding the recorded outcome with it.
        // Retention prunes `stage_entered` rows like any other event, and
        // the entry-stage append is best-effort, so a trail that opens
        // mid-flight is reachable, not hypothetical.
        let hop = match (
            entry
                .get("payload")
                .and_then(|p| p.get("outcome"))
                .and_then(Value::as_str),
            i.checked_sub(1).and_then(|p| trail.get(p)),
        ) {
            (Some(outcome), Some(previous)) => {
                format!("{} --[{outcome}]--> {stage}", stage_of(previous))
            }
            // Something carried the task here, but whatever it departed is
            // no longer on record — say so rather than guessing or dropping
            // the outcome.
            (Some(outcome), None) => format!("… --[{outcome}]--> {stage}"),
            (None, _) => format!("{stage} (start)"),
        };
        lines.push(format!("  {step}. {hop}{at}"));
    }

    match current {
        Some(current) if lines.is_empty() => {
            format!("  → {current} (current, no transitions yet)")
        }
        // The last entry *is* the current stage — `enter_stage` records on
        // entry — so this marks it in place rather than repeating it on a
        // trailing arrow line. It's still worth stating: a mismatch means
        // the trail was truncated by retention, and silently rendering a
        // stale last hop as "where the task is" would be a lie.
        Some(current) => {
            if let Some(last) = lines.last_mut()
                && trail.last().map(stage_of).as_deref() == Some(current)
            {
                last.push_str("   (current)");
            } else {
                lines.push(format!("  → {current} (current)"));
            }
            lines.join("\n")
        }
        None if lines.is_empty() => "  (none)".to_string(),
        None => lines.join("\n"),
    }
}

/// One line per event: time, kind, and the payload's salient field.
pub fn events(page: &EventsPage) -> String {
    if page.events.is_empty() {
        return "No events recorded for this task yet.".to_string();
    }

    let rows: Vec<Vec<String>> = page
        .events
        .iter()
        .map(|e| {
            vec![
                timestamp(&e.created_at),
                e.event_type.to_string(),
                event_summary(e),
            ]
        })
        .collect();
    let mut out = table(&["TIME", "KIND", "DETAIL"], &rows);

    if let Some(token) = &page.next_token {
        out.push_str(&format!(
            "\n\nMore events available — continue with `--after {token}`"
        ));
    }
    out
}

/// Pulls the field worth showing for each event kind, falling back to the
/// whole payload so an unrecognized shape still renders something real
/// rather than being silently blanked.
///
/// Payload shapes come from `AgentEvent::payload` (`adapter/mod.rs`) and
/// the engine's own `HumanMessage` events:
/// `text` for human/assistant/thinking, `session_id` for session_meta,
/// `message` for error, and `{tool_use_id, tool, input|output}` for the two
/// tool kinds — which carry no single "the interesting bit" field, so they
/// get composed rather than probed. Tool events dominate a real coding
/// transcript, so dumping their raw JSON here would defeat the point of
/// this view. The engine's own task-scoped events (`stage_entered`,
/// `shell_output`) are composed for the same reason.
fn event_summary(event: &Event) -> String {
    let payload = &event.payload;

    match event.event_type {
        // `{stage, outcome}` has no "text"-ish field for the fallback below
        // to find, so without this arm the timeline would show a raw JSON
        // object for every stage transition.
        EventType::StageEntered => {
            let stage = payload.get("stage").and_then(Value::as_str).unwrap_or("?");
            match payload.get("outcome").and_then(Value::as_str) {
                Some(outcome) => format!("{stage}  (via {outcome})"),
                None => stage.to_string(),
            }
        }
        // Likewise `{stage, command, exit_code, …}` (P2-1): the fallback
        // would find no "text"-ish key and dump the raw object, when what
        // a reader wants is the command and whether it worked.
        EventType::ShellOutput => {
            let command = payload
                .get("command")
                .and_then(Value::as_str)
                .unwrap_or("?");
            let status = if payload
                .get("timed_out")
                .and_then(Value::as_bool)
                .unwrap_or(false)
            {
                "timed out".to_string()
            } else {
                match payload.get("exit_code").and_then(Value::as_i64) {
                    Some(code) => format!("exit {code}"),
                    // No exit code and no timeout: killed by a signal, or
                    // never started at all (`note` carries the reason).
                    None => "did not exit cleanly".to_string(),
                }
            };
            let mut summary = format!("$ {command}  →  {status}");
            // Whatever explains a surprising result — the spawn failure, an
            // uncaptured oversized output, JSON that wouldn't parse —
            // outranks the command's own chatter.
            for key in ["note", "stderr_tail", "stdout_tail"] {
                match payload.get(key).and_then(Value::as_str) {
                    Some(detail) if !detail.is_empty() => {
                        summary.push_str("  ");
                        summary.push_str(detail);
                        break;
                    }
                    _ => {}
                }
            }
            one_line(&summary)
        }
        EventType::ToolCall => {
            let tool = payload.get("tool").and_then(Value::as_str).unwrap_or("?");
            match payload.get("input") {
                Some(input) if !input.is_null() => {
                    one_line(&format!("{tool}  {}", value_text(input)))
                }
                _ => tool.to_string(),
            }
        }
        EventType::ToolResult => {
            let tool = payload.get("tool").and_then(Value::as_str).unwrap_or("?");
            let failed = payload
                .get("is_error")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            let tool = if failed {
                format!("{tool} [error]")
            } else {
                tool.to_string()
            };
            match payload.get("output") {
                Some(output) if !output.is_null() => {
                    one_line(&format!("{tool}  {}", value_text(output)))
                }
                _ => tool,
            }
        }
        _ => {
            for key in ["text", "message", "session_id"] {
                if let Some(value) = payload.get(key).and_then(Value::as_str) {
                    return one_line(value);
                }
            }
            match payload {
                Value::Null => String::new(),
                Value::Object(map) if map.is_empty() => String::new(),
                other => one_line(&value_text(other)),
            }
        }
    }
}

/// A JSON value as display text: strings unquoted (the common case for
/// tool output), everything else compact JSON.
fn value_text(value: &Value) -> String {
    match value.as_str() {
        Some(text) => text.to_string(),
        None => value.to_string(),
    }
}

/// Collapses any internal newline/whitespace run to a single space. Titles
/// and project names are free-form and reach the API unvalidated (`POST
/// /tasks` takes any string, and a shell can pass `--title $'a\nb'`), so a
/// raw one would otherwise split a row across lines and break the layout.
fn single_line(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Events carry whole agent messages; unbounded multi-line text would wreck
/// a table, so collapse newlines and cap the width.
fn one_line(text: &str) -> String {
    const MAX: usize = 100;
    let flat = single_line(text);
    if flat.chars().count() > MAX {
        let kept: String = flat.chars().take(MAX).collect();
        format!("{kept}…")
    } else {
        flat
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    /// One `stage_entered` event per entry, shaped as the daemon serializes
    /// them, with the hop arrow reconstructed from consecutive entries.
    fn stage_entry(stage: &str, outcome: Value, at: &str) -> Value {
        json!({
            "id": format!("e-{stage}-{at}"),
            "task_id": "t1",
            "task_run_id": Value::Null,
            "event_type": "stage_entered",
            "payload": {"stage": stage, "outcome": outcome},
            "created_at": at,
        })
    }

    #[test]
    fn stage_progress_reconstructs_each_hop_from_consecutive_entries() {
        let trail = [
            stage_entry("gate", Value::Null, "2026-08-01T11:58:00Z"),
            stage_entry("review", json!("resumed"), "2026-08-01T11:58:18.972857783Z"),
        ];
        let rendered = stage_progress(&trail, Some("review"));
        assert!(
            rendered.contains("gate --[resumed]--> review"),
            "{rendered}"
        );
        assert!(rendered.contains("2026-08-01 11:58:18 UTC"), "{rendered}");
        // The last entry *is* the current stage, so it's marked in place
        // rather than repeated on a trailing arrow line.
        assert!(rendered.contains("(current)"), "{rendered}");
        assert!(!rendered.contains("→ review"), "duplicated: {rendered}");
    }

    /// The stage a task *starts* in was never in `stage_history`, which only
    /// appended on the way out. It has no predecessor and no outcome, so it
    /// must render as a starting point rather than an arrow from "?".
    #[test]
    fn stage_progress_shows_the_entry_stage_that_stage_history_never_had() {
        let trail = [stage_entry("chatting", Value::Null, "2026-08-01T11:58:00Z")];
        let rendered = stage_progress(&trail, Some("chatting"));
        assert!(rendered.contains("1. chatting (start)"), "{rendered}");
        assert!(
            !rendered.contains("?"),
            "no phantom predecessor: {rendered}"
        );
    }

    /// The other end of the same truncation. Retention prunes
    /// `stage_entered` rows like any other event, so the first *surviving*
    /// entry can be one the task was transitioned into. Labelling it
    /// "(start)" would assert a beginning that never happened and throw
    /// away the recorded outcome, so the outcome — not the presence of a
    /// predecessor — decides.
    #[test]
    fn stage_progress_does_not_claim_a_truncated_trail_head_is_the_start() {
        let trail = [
            stage_entry("review", json!("changes_requested"), "2026-08-01T11:58:00Z"),
            stage_entry("gate", json!("rejected"), "2026-08-01T11:59:00Z"),
        ];
        let rendered = stage_progress(&trail, Some("gate"));
        assert!(
            !rendered.contains("(start)"),
            "nothing here started the task: {rendered}"
        );
        assert!(
            rendered.contains("changes_requested"),
            "the outcome that carried it here was dropped: {rendered}"
        );
        assert!(
            rendered.contains("… --[changes_requested]--> review"),
            "{rendered}"
        );
        // The hop that *does* have its predecessor still renders normally.
        assert!(
            rendered.contains("review --[rejected]--> gate"),
            "{rendered}"
        );
    }

    /// Retention ages events out, so a long-lived task's trail can lose its
    /// head. Rendering the surviving last hop as "where the task is" would
    /// be a lie whenever it disagrees with `current_stage`.
    #[test]
    fn stage_progress_still_names_the_current_stage_when_the_trail_is_stale() {
        let trail = [stage_entry("gate", Value::Null, "2026-08-01T11:58:00Z")];
        let rendered = stage_progress(&trail, Some("done"));
        assert!(rendered.contains("→ done (current)"), "{rendered}");
    }

    /// A task that ran before X-3 has no `stage_entered` events and no
    /// backfill, so an absent trail must read as "nothing recorded" rather
    /// than being invented.
    #[test]
    fn stage_progress_reports_a_task_that_has_not_transitioned_yet() {
        let rendered = stage_progress(&[], Some("chatting"));
        assert!(rendered.contains("no transitions yet"), "{rendered}");
    }

    /// The trail is a sibling of `workflow_state`, not a field inside it —
    /// reading it from the wrong place would silently render every task as
    /// having no history at all.
    #[test]
    fn task_detail_reads_the_trail_from_the_top_level_not_from_workflow_state() {
        let detail = json!({
            "id": "t1", "title": "x", "project_id": "p", "workflow_def": "chat",
            "status": "open", "created_at": "2026-08-01T12:00:00Z",
            "workflow_state": {
                "task_id": "t1", "current_stage": "review",
                "loop_counters": {}, "payload": {},
                "updated_at": "2026-08-01T12:00:00Z",
            },
            "stage_trail": [
                stage_entry("gate", Value::Null, "2026-08-01T11:58:00Z"),
                stage_entry("review", json!("resumed"), "2026-08-01T11:58:18Z"),
            ],
        });
        let rendered = task_detail(&detail);
        assert!(
            rendered.contains("gate --[resumed]--> review"),
            "{rendered}"
        );
        assert!(
            !rendered.contains("no transitions yet"),
            "trail was not found: {rendered}"
        );
    }

    #[test]
    fn task_detail_without_workflow_state_does_not_claim_a_stage() {
        let detail = json!({
            "id": "t1", "title": "x", "project_id": "p", "workflow_def": "chat",
            "status": "open", "created_at": "2026-08-01T12:00:00Z",
            "workflow_state": null,
        });
        let rendered = task_detail(&detail);
        assert!(rendered.contains("has not started"), "{rendered}");
        assert!(!rendered.contains("Stage "), "{rendered}");
    }

    fn event(event_type: EventType, payload: Value) -> Event {
        Event {
            id: "e1".to_string(),
            task_id: "t1".to_string(),
            task_run_id: Some("r1".to_string()),
            event_type,
            payload,
            created_at: Utc::now(),
        }
    }

    /// Covers every payload shape `AgentEvent::payload` (`adapter/mod.rs`)
    /// and the engine's `HumanMessage` actually produce. The tool kinds
    /// carry no single "interesting" field, and they dominate a real
    /// coding transcript, so a regression here would make the events view
    /// useless exactly where it matters most.
    #[test]
    fn event_summary_renders_every_real_payload_shape() {
        let cases = [
            (
                EventType::HumanMessage,
                json!({"text": "do the thing"}),
                "do the thing",
            ),
            (
                EventType::AssistantMessage,
                json!({"text": "on it"}),
                "on it",
            ),
            (EventType::Thinking, json!({"text": "hmm"}), "hmm"),
            (
                EventType::SessionMeta,
                json!({"session_id": "abc-123"}),
                "abc-123",
            ),
            (EventType::Error, json!({"message": "boom"}), "boom"),
            // A stage transition carries neither text nor a session, so
            // without its own arm it would render as raw JSON.
            (
                EventType::StageEntered,
                json!({"stage": "review", "outcome": "approved"}),
                "review  (via approved)",
            ),
            (
                EventType::StageEntered,
                json!({"stage": "gate", "outcome": null}),
                "gate",
            ),
            // Same for a shell stage's output: the command and whether it
            // worked, not the raw payload object.
            (
                EventType::ShellOutput,
                json!({"stage": "open_pr", "command": "gh pr create --fill",
                       "exit_code": 0, "timed_out": false, "duration_ms": 840,
                       "stdout_tail": "", "stderr_tail": ""}),
                "$ gh pr create --fill → exit 0",
            ),
            (
                EventType::ShellOutput,
                json!({"stage": "open_pr", "command": "gh pr create --fill",
                       "exit_code": 1, "timed_out": false, "duration_ms": 840,
                       "stdout_tail": "", "stderr_tail": "no commits between"}),
                "$ gh pr create --fill → exit 1 no commits between",
            ),
            (
                EventType::ShellOutput,
                json!({"stage": "checks", "command": "sleep 600",
                       "exit_code": null, "timed_out": true, "duration_ms": 300000,
                       "stdout_tail": "", "stderr_tail": ""}),
                "$ sleep 600 → timed out",
            ),
            // A `note` outranks the command's own chatter — it's the part
            // that explains a surprising result.
            (
                EventType::ShellOutput,
                json!({"stage": "run", "command": "./deploy.sh",
                       "exit_code": null, "timed_out": false, "duration_ms": 0,
                       "stdout_tail": "", "stderr_tail": "",
                       "note": "failed to start command: permission denied"}),
                "$ ./deploy.sh → did not exit cleanly failed to start command: permission denied",
            ),
        ];
        for (event_type, payload, expected) in cases {
            assert_eq!(event_summary(&event(event_type, payload)), expected);
        }

        // Tool events: the tool name leads, then its input/output — never
        // the opaque `tool_use_id`, which would eat the width budget.
        let call = event_summary(&event(
            EventType::ToolCall,
            json!({"tool_use_id": "toolu_01ABCDEFGHIJKLMNOPQRSTUV", "tool": "Bash",
                   "input": {"command": "ls -la"}}),
        ));
        assert!(call.starts_with("Bash"), "{call}");
        assert!(call.contains("ls -la"), "{call}");
        assert!(!call.contains("toolu_01"), "id should not be shown: {call}");

        let result = event_summary(&event(
            EventType::ToolResult,
            json!({"tool_use_id": "toolu_01ABC", "tool": "Bash",
                   "output": "total 0", "is_error": false}),
        ));
        assert!(result.starts_with("Bash"), "{result}");
        assert!(result.contains("total 0"), "{result}");
        assert!(!result.contains("[error]"), "{result}");

        let failed = event_summary(&event(
            EventType::ToolResult,
            json!({"tool_use_id": "t", "tool": "Bash", "output": "nope", "is_error": true}),
        ));
        assert!(failed.contains("[error]"), "{failed}");
    }

    #[test]
    fn event_summary_survives_an_unrecognized_or_empty_payload() {
        assert_eq!(event_summary(&event(EventType::Error, json!({}))), "");
        // An unexpected shape still shows something real rather than blank.
        let odd = event_summary(&event(EventType::Error, json!({"unexpected": 42})));
        assert!(odd.contains("42"), "{odd}");
    }

    #[test]
    fn events_table_has_no_trailing_whitespace_when_a_summary_is_empty() {
        let page = EventsPage {
            events: vec![event(EventType::Error, json!({}))],
            next_token: None,
        };
        for line in events(&page).lines() {
            assert_eq!(line, line.trim_end(), "trailing space: {line:?}");
        }
    }

    #[test]
    fn event_summary_collapses_multiline_text_and_caps_length() {
        let long = "a ".repeat(200);
        assert_eq!(one_line("hello\n  there"), "hello there");
        assert!(one_line(&long).ends_with('…'));
        assert!(one_line(&long).chars().count() <= 101);
    }

    /// Titles and project names reach the API unvalidated, so a newline in
    /// one would otherwise split a row across lines and wreck the layout.
    #[test]
    fn a_title_containing_a_newline_does_not_break_the_table() {
        let task = Task {
            id: "t1".to_string(),
            project_id: "p1".to_string(),
            parent_task_id: None,
            workflow_def: "chat".to_string(),
            title: "first line\nsecond line".to_string(),
            status: "open".to_string(),
            config: json!({}),
            created_at: Utc::now(),
            updated_at: Utc::now(),
        };

        let rendered = tasks(std::slice::from_ref(&task));
        assert_eq!(
            rendered.lines().count(),
            2,
            "header + one row expected, got: {rendered}"
        );
        assert!(rendered.contains("first line second line"), "{rendered}");

        // The single-task view collapses it too, but keeps the full text.
        let detail = super::task(&task);
        assert!(detail.contains("first line second line"), "{detail}");
    }

    #[test]
    fn table_pads_columns_without_trailing_whitespace() {
        let rows = vec![
            vec!["a".to_string(), "1".to_string()],
            vec!["longer".to_string(), "2".to_string()],
        ];
        let rendered = table(&["NAME", "N"], &rows);
        for line in rendered.lines() {
            assert_eq!(line, line.trim_end(), "line has trailing space: {line:?}");
        }
        assert!(rendered.contains("longer  2"), "{rendered}");
    }
}
