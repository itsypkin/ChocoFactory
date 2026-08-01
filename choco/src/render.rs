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
        out.push_str("\n\nProgress\n");
        out.push_str(&stage_progress(state, current));

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
/// Entries are objects (`{stage, outcome, to, at}`) as of the engine change
/// that records why and when each hop happened; entries written before that
/// are bare stage-name strings, and both shapes can coexist in one array,
/// so each is rendered for what it is rather than assumed.
fn stage_progress(state: &Value, current: Option<&str>) -> String {
    let history = state
        .get("stage_history")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();

    let mut lines = Vec::new();
    for (i, entry) in history.iter().enumerate() {
        let step = i + 1;
        match entry {
            Value::String(stage) => lines.push(format!("  {step}. {stage}")),
            Value::Object(_) => {
                let field = |key: &str| entry.get(key).and_then(Value::as_str);
                let stage = field("stage").unwrap_or("?");
                let to = field("to").unwrap_or("?");
                let outcome = field("outcome").unwrap_or("?");
                let at = field("at").map(timestamp_str).unwrap_or_default();
                let at = if at.is_empty() {
                    String::new()
                } else {
                    format!("   {at}")
                };
                lines.push(format!("  {step}. {stage} --[{outcome}]--> {to}{at}"));
            }
            other => lines.push(format!("  {step}. {other}")),
        }
    }

    match current {
        Some(current) if lines.is_empty() => {
            format!("  → {current} (current, no transitions yet)")
        }
        Some(current) => {
            lines.push(format!("  → {current} (current)"));
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
/// this view.
fn event_summary(event: &Event) -> String {
    let payload = &event.payload;

    match event.event_type {
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

    #[test]
    fn stage_progress_renders_the_new_object_entries_with_outcome_and_target() {
        let state = json!({
            "current_stage": "review",
            "stage_history": [
                {"stage": "gate", "outcome": "resumed", "to": "review",
                 "at": "2026-08-01T11:58:18.972857783Z"}
            ],
        });
        let rendered = stage_progress(&state, Some("review"));
        assert!(
            rendered.contains("gate --[resumed]--> review"),
            "{rendered}"
        );
        assert!(rendered.contains("2026-08-01 11:58:18 UTC"), "{rendered}");
        assert!(rendered.contains("→ review (current)"), "{rendered}");
    }

    /// Entries written before the engine started recording outcomes are
    /// bare strings; they must still render rather than showing "?" noise
    /// or being dropped.
    #[test]
    fn stage_progress_still_renders_legacy_string_entries() {
        let state = json!({
            "current_stage": "done",
            "stage_history": ["gate", {"stage": "review", "outcome": "approved",
                                       "to": "done", "at": "2026-08-01T12:00:00Z"}],
        });
        let rendered = stage_progress(&state, Some("done"));
        assert!(rendered.contains("1. gate"), "{rendered}");
        assert!(
            rendered.contains("2. review --[approved]--> done"),
            "{rendered}"
        );
    }

    #[test]
    fn stage_progress_reports_a_task_that_has_not_transitioned_yet() {
        let state = json!({"current_stage": "chatting", "stage_history": []});
        let rendered = stage_progress(&state, Some("chatting"));
        assert!(rendered.contains("no transitions yet"), "{rendered}");
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
            task_run_id: "r1".to_string(),
            seq: 1,
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
