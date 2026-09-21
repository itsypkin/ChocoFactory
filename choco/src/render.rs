//! Human-readable rendering of API responses (design Q12: `choco` is both
//! human-scriptable and agent-callable — this is the human half, `--json`
//! is the machine half). No colour/ANSI: output is routinely piped, and
//! this repo ships no terminal-styling dependency.

use chocofactory_core::models::{Event, EventType, Project, RetryOutcome, Task};
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
        ("Repo", p.repo_path.as_deref().unwrap_or("-").to_string()),
        ("Created", timestamp(&p.created_at)),
    ])
}

pub fn projects(list: &[Project]) -> String {
    if list.is_empty() {
        return "No projects yet. Create one with `choco project create <name>`.".to_string();
    }
    let rows: Vec<Vec<String>> = list
        .iter()
        .map(|p| {
            vec![
                one_line(&p.name),
                p.id.clone(),
                p.repo_path.as_deref().unwrap_or("-").to_string(),
                timestamp(&p.created_at),
            ]
        })
        .collect();
    table(&["NAME", "ID", "REPO", "CREATED"], &rows)
}

/// `choco project init-workflows`'s result (issue #88).
pub fn init_workflows(result: &crate::client::InitWorkflowsResult) -> String {
    let mut out = format!("Seeded {}\n", result.dir);
    for path in &result.created {
        out.push_str(&format!("  created   {path}\n"));
    }
    for path in &result.existing {
        out.push_str(&format!("  existing  {path}\n"));
    }
    out.push_str(
        "\nCommit this directory so the team shares it: \
         git add .chocofactory/ && git commit",
    );
    out
}

pub fn task(t: &Task) -> String {
    let mut pairs = vec![
        ("Title", single_line(&t.title)),
        ("ID", t.id.clone()),
        ("Project", t.project_id.clone()),
        ("Workflow", t.workflow_def.clone()),
        ("Status", t.status.clone()),
    ];
    // Right after Status, so the reason for a stuck task (X-4, #61) reads
    // next to the status value that explains it needs one.
    if t.status == "stuck"
        && let Some(reason) = &t.stuck_reason
    {
        pairs.push(("Stuck", single_line(reason)));
    }
    if let Some(cwd) = t.config.get("cwd").and_then(Value::as_str) {
        pairs.push(("Repo", cwd.to_string()));
    }
    // Without this, `task create --role-model coder=opus` and
    // `task reconfigure` would both print nothing about the override that
    // was just applied, leaving `--json` as the only way to confirm it.
    for (role, settings) in role_summaries(&t.config) {
        pairs.push(("Role", format!("{role}: {settings}")));
    }
    pairs.push(("Created", timestamp(&t.created_at)));
    fields(&pairs)
}

/// One `field=value` summary per configured role in `config.roles`, sorted by
/// role name so repeated runs render identically.
///
/// Skips anything that isn't shaped like a role object instead of erroring:
/// `config` is free-form JSON the daemon stores verbatim (and deliberately
/// does not validate), so a display path is the wrong place to be strict.
fn role_summaries(config: &Value) -> Vec<(String, String)> {
    let Some(roles) = config.get("roles").and_then(Value::as_object) else {
        return Vec::new();
    };
    let mut names: Vec<&String> = roles.keys().collect();
    names.sort();
    names
        .into_iter()
        .filter_map(|name| {
            let settings = roles.get(name)?.as_object()?;
            let mut keys: Vec<&String> = settings.keys().collect();
            keys.sort();
            let rendered: Vec<String> = keys
                .into_iter()
                .filter_map(|key| {
                    let value = settings.get(key)?;
                    Some(match value.as_str() {
                        // A system prompt is arbitrary multi-line prose;
                        // summarize rather than dumping it into a field list.
                        Some(_) if key == "system_prompt" => format!("{key}=<text>"),
                        Some(text) => format!("{key}={}", one_line(text)),
                        // Not a string: the daemon doesn't constrain these, so
                        // show the raw JSON rather than hiding the field.
                        None => format!("{key}={value}"),
                    })
                })
                .collect();
            (!rendered.is_empty()).then(|| (name.clone(), rendered.join(", ")))
        })
        .collect()
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

/// Says which of the two things `task retry` did (#92). Named in the first
/// sentence rather than left to the timeline: resuming a session and
/// starting a new one lead to very different next few minutes, and an
/// operator who asked for one and got the other should not have to go
/// looking for that.
pub fn retried(task_id: &str, outcome: &RetryOutcome) -> String {
    let what = match &outcome.adapter_session_id {
        Some(adapter_session_id) if outcome.resumed => format!(
            "Retrying stage '{}' by resuming its interrupted session ({adapter_session_id}) — it \
             picks up where it left off, with its working tree untouched.",
            outcome.stage
        ),
        // `resumed` without a session id is not something the daemon
        // produces; reported plainly rather than claiming a session that
        // isn't named.
        _ if outcome.resumed => format!(
            "Retrying stage '{}' by resuming its interrupted session.",
            outcome.stage
        ),
        // Why it started over, when the daemon said: an operator who
        // expected a resume is owed the reason, not left to guess.
        _ => match &outcome.fresh_reason {
            Some(why) => format!(
                "Retrying stage '{}' from scratch, in a fresh session: {why}.",
                outcome.stage
            ),
            None => format!(
                "Retrying stage '{}' from scratch, in a fresh session.",
                outcome.stage
            ),
        },
    };
    // The same "where to look next" pointer `cancel` and `send` end with.
    format!("{what} See `choco task status {task_id}`.")
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
    ];
    // Only present for a task created after issue #88 — a legacy task
    // (`workflow_path: null`) shows no such line, exactly as if this field
    // didn't exist. Where the file changed or has gone missing since the
    // task started, that's appended right onto this line rather than as a
    // separate one — it's a qualifier on *this* fact, not a fact of its
    // own.
    if let Some(path) = detail.get("workflow_path").and_then(Value::as_str) {
        let mut line = path.to_string();
        match detail.get("workflow_file_status").and_then(Value::as_str) {
            Some("changed") => line.push_str(" (changed since task start)"),
            Some("missing") => line.push_str(" (missing)"),
            _ => {}
        }
        if let Some(sha) = detail.get("workflow_sha256").and_then(Value::as_str) {
            line.push_str(&format!("  [{}]", &sha[..sha.len().min(12)]));
        }
        pairs.push(("Workflow file", line));
    }
    pairs.push(("Status", get("status").to_string()));
    // Right after Status, so the reason for a stuck task (X-4, #61) reads
    // next to the status value that explains it needs one.
    if get("status") == "stuck"
        && let Some(reason) = detail.get("stuck_reason").and_then(Value::as_str)
    {
        pairs.push(("Stuck", single_line(reason)));
    }
    // Same per-role lines `task` renders: `task status` is where an existing
    // task gets inspected, so leaving them out would mean `--json` was the
    // only way to see what `task reconfigure` actually did. `TaskDetail`
    // flattens the `Task`, so `config` is a top-level key here.
    if let Some(config) = detail.get("config") {
        if let Some(cwd) = config.get("cwd").and_then(Value::as_str) {
            pairs.push(("Repo", cwd.to_string()));
        }
        for (role, settings) in role_summaries(config) {
            pairs.push(("Role", format!("{role}: {settings}")));
        }
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
/// `text` for human/assistant/thinking, `adapter_session_id` for session_meta,
/// `message` for error, and `{tool_use_id, tool, input|output}` for the two
/// tool kinds — which carry no single "the interesting bit" field, so they
/// get composed rather than probed. Tool events dominate a real coding
/// transcript, so dumping their raw JSON here would defeat the point of
/// this view. The engine's own task-scoped events (`stage_entered`,
/// `shell_output`) are composed for the same reason.
fn event_summary(event: &Event) -> String {
    let summary = event_summary_body(event);
    // #90: output from a sub-agent, or arriving after the turn had already
    // completed, is recorded on the same session as the main agent's. Marked
    // so a reader of the timeline can't take either for the turn's own answer.
    let mut markers = String::new();
    if event
        .payload
        .get("parent_tool_use_id")
        .is_some_and(|v| !v.is_null())
    {
        markers.push_str("[sub-agent] ");
    }
    if event
        .payload
        .get("after_completion")
        .and_then(Value::as_bool)
        == Some(true)
    {
        markers.push_str("[late] ");
    }
    format!("{markers}{summary}")
}

fn event_summary_body(event: &Event) -> String {
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
        // `{stage, capture, outcome, applied, note}` (#45) — same reasoning as
        // the two arms above. The `note` is the point of the entry when it's
        // present: it's what says a reply wasn't the JSON the stage asked for
        // and the outcome fell back to `done`. `applied: false` marks an
        // outcome that was computed but deliberately not taken, which is the
        // park a reviewer stage relies on — without it the line would read as
        // a transition that happened.
        EventType::TurnOutcome => {
            let stage = payload.get("stage").and_then(Value::as_str).unwrap_or("?");
            let outcome = payload
                .get("outcome")
                .and_then(Value::as_str)
                .unwrap_or("no outcome");
            let applied = payload
                .get("applied")
                .and_then(Value::as_bool)
                .unwrap_or(true);
            let arrow = if applied { "→" } else { "⨯" };
            let mut summary = format!("{stage} turn  {arrow}  {outcome}");
            if let Some(note) = payload.get("note").and_then(Value::as_str)
                && !note.is_empty()
            {
                summary.push_str("  ");
                summary.push_str(note);
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
        // `{is_error}` — the CLI's own end-of-turn marker (#70). Without
        // this arm the catch-all's empty-object fallback renders a blank
        // cell, which reads as missing data rather than as its own event.
        EventType::TurnCompleted => {
            let is_error = payload
                .get("is_error")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            if is_error {
                "turn complete (error)".to_string()
            } else {
                "turn complete".to_string()
            }
        }
        // `{kind, message}` (#90): the daemon nudged, gave up on, or killed
        // an agent turn. The kind leads, so a scan of the timeline can tell a
        // routine nudge apart from the kill that parked the task.
        EventType::SessionNote => {
            let kind = payload.get("kind").and_then(Value::as_str).unwrap_or("?");
            let message = payload
                .get("message")
                .and_then(Value::as_str)
                .unwrap_or_default();
            one_line(&format!("[{kind}] {message}"))
        }
        _ => {
            for key in ["text", "message", "adapter_session_id"] {
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

    #[test]
    fn retried_says_whether_the_session_was_resumed() {
        let resumed = super::retried(
            "task-1",
            &RetryOutcome {
                stage: "coding".to_string(),
                resumed: true,
                adapter_session_id: Some("sess-123".to_string()),
                fresh_reason: None,
            },
        );
        assert!(
            resumed.contains("coding") && resumed.contains("sess-123"),
            "{resumed}"
        );
        assert!(resumed.contains("resuming"), "{resumed}");
        assert!(resumed.contains("choco task status task-1"), "{resumed}");

        // A fresh start says why, so an operator who expected a resume
        // isn't left guessing.
        let fresh = super::retried(
            "task-1",
            &RetryOutcome {
                stage: "coding".to_string(),
                resumed: false,
                adapter_session_id: None,
                fresh_reason: Some("its turn ended 'no_report'".to_string()),
            },
        );
        assert!(
            fresh.contains("fresh session") && fresh.contains("no_report"),
            "{fresh}"
        );
    }
    use serde_json::json;

    use super::*;

    /// One `stage_entered` event per entry, shaped as the daemon serializes
    /// them, with the hop arrow reconstructed from consecutive entries.
    fn stage_entry(stage: &str, outcome: Value, at: &str) -> Value {
        json!({
            "id": format!("e-{stage}-{at}"),
            "task_id": "t1",
            "session_id": Value::Null,
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

    fn task_with_config(config: Value) -> Task {
        Task {
            id: "t1".to_string(),
            project_id: "p".to_string(),
            workflow_def: "coding-task".to_string(),
            title: "x".to_string(),
            status: "open".to_string(),
            config,
            worktree_repo: None,
            worktree_project: None,
            stuck_reason: None,
            workflow_path: None,
            workflow_sha256: None,
            created_at: Utc::now(),
            updated_at: Utc::now(),
        }
    }

    /// X-4 (#61): the single-task view renders `Stuck` only when the
    /// status is actually `stuck` and a reason is present.
    #[test]
    fn task_renders_the_stuck_line_only_when_stuck() {
        let mut stuck = task_with_config(json!({}));
        stuck.status = "stuck".to_string();
        stuck.stuck_reason = Some("stage 'run': it broke".to_string());
        let rendered = task(&stuck);
        assert!(rendered.contains("Stuck"), "{rendered}");
        assert!(rendered.contains("stage 'run': it broke"), "{rendered}");

        let open = task_with_config(json!({}));
        let rendered = task(&open);
        assert!(!rendered.contains("Stuck"), "{rendered}");
    }

    /// P2-6: a task can configure several roles independently, so human
    /// output has to name each one rather than showing the first or none.
    #[test]
    fn task_lists_every_configured_role_and_the_repo() {
        let rendered = task(&task_with_config(json!({
            "cwd": "/src/app",
            "roles": {
                "reviewer": { "model": "sonnet" },
                "coder": { "model": "opus", "cli": "claude" }
            }
        })));

        assert!(rendered.contains("/src/app"), "{rendered}");
        // Sorted by role name, so output is stable across runs.
        let coder = rendered.find("coder: ").expect("coder missing");
        let reviewer = rendered.find("reviewer: ").expect("reviewer missing");
        assert!(coder < reviewer, "{rendered}");
        assert!(rendered.contains("cli=claude, model=opus"), "{rendered}");
        assert!(rendered.contains("reviewer: model=sonnet"), "{rendered}");
    }

    /// A long or multi-line system prompt must not be dumped into the field
    /// list, and a non-object `roles` must not panic the renderer.
    #[test]
    fn task_summarizes_a_system_prompt_and_tolerates_odd_config_shapes() {
        let rendered = task(&task_with_config(json!({
            "roles": { "coder": { "system_prompt": "line one\nline two" } }
        })));
        assert!(rendered.contains("system_prompt=<text>"), "{rendered}");
        assert!(!rendered.contains("line two"), "{rendered}");

        for odd in [
            json!({}),
            json!({ "roles": 7 }),
            json!({ "roles": { "coder": "nope" } }),
            json!({ "roles": { "coder": {} } }),
        ] {
            let rendered = task(&task_with_config(odd.clone()));
            assert!(rendered.contains("Workflow"), "{odd} -> {rendered}");
        }
    }

    /// `task status` is where an existing task gets inspected, so it has to
    /// show per-role config too — otherwise `task reconfigure`'s effect is
    /// only visible under `--json`.
    #[test]
    fn task_detail_lists_every_configured_role_and_the_repo() {
        let detail = json!({
            "id": "t1", "title": "x", "project_id": "p", "workflow_def": "two-role",
            "status": "open", "created_at": "2026-08-01T12:00:00Z",
            "config": {
                "cwd": "/src/app",
                "roles": {
                    "reviewer": { "model": "sonnet" },
                    "coder": { "model": "opus", "cli": "claude" }
                }
            },
            "workflow_state": null,
        });

        let rendered = task_detail(&detail);

        assert!(rendered.contains("/src/app"), "{rendered}");
        assert!(
            rendered.contains("coder: cli=claude, model=opus"),
            "{rendered}"
        );
        assert!(rendered.contains("reviewer: model=sonnet"), "{rendered}");
    }

    /// X-4 (#61): `task_detail` renders the `Stuck` line only when the
    /// status is actually `stuck` and a reason is present.
    #[test]
    fn task_detail_renders_the_stuck_line_only_when_stuck() {
        let stuck = json!({
            "id": "t1", "title": "x", "project_id": "p", "workflow_def": "chat",
            "status": "stuck", "stuck_reason": "stage 'run': it broke",
            "created_at": "2026-08-01T12:00:00Z",
            "workflow_state": null,
        });
        let rendered = task_detail(&stuck);
        assert!(rendered.contains("Stuck"), "{rendered}");
        assert!(rendered.contains("stage 'run': it broke"), "{rendered}");

        // An open task with no stuck_reason at all gets no such line.
        let open = json!({
            "id": "t1", "title": "x", "project_id": "p", "workflow_def": "chat",
            "status": "open", "created_at": "2026-08-01T12:00:00Z",
            "workflow_state": null,
        });
        let rendered = task_detail(&open);
        assert!(!rendered.contains("Stuck"), "{rendered}");
    }

    /// Issue #88: `task status` shows the "Workflow file" line only when
    /// `workflow_path` is present, appends the drift suffix from
    /// `workflow_file_status`, and includes a short hash prefix.
    #[test]
    fn task_detail_renders_the_workflow_file_line_with_status_and_hash() {
        let base = json!({
            "id": "t1", "title": "x", "project_id": "p", "workflow_def": "chat",
            "status": "open", "created_at": "2026-08-01T12:00:00Z",
            "workflow_state": null,
            "workflow_path": "/repo/.chocofactory/workflows/chat.yaml",
            "workflow_sha256": "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcd",
        });

        // unchanged: the path and short hash show, with no drift suffix.
        let mut unchanged = base.clone();
        unchanged["workflow_file_status"] = json!("unchanged");
        let rendered = task_detail(&unchanged);
        assert!(
            rendered.contains("Workflow file"),
            "expected a Workflow file line: {rendered}"
        );
        assert!(
            rendered.contains("/repo/.chocofactory/workflows/chat.yaml"),
            "{rendered}"
        );
        assert!(rendered.contains("[0123456789ab]"), "{rendered}");
        assert!(!rendered.contains("changed since task start"), "{rendered}");
        assert!(!rendered.contains("(missing)"), "{rendered}");

        // changed: the drift suffix is appended to the same line.
        let mut changed = base.clone();
        changed["workflow_file_status"] = json!("changed");
        let rendered = task_detail(&changed);
        assert!(rendered.contains("changed since task start"), "{rendered}");

        // missing: a different suffix, not "changed since task start".
        let mut missing = base.clone();
        missing["workflow_file_status"] = json!("missing");
        let rendered = task_detail(&missing);
        assert!(rendered.contains("(missing)"), "{rendered}");
        assert!(!rendered.contains("changed since task start"), "{rendered}");
    }

    /// A legacy task (predating issue #88) has `workflow_path: null` and no
    /// `workflow_file_status` at all — no "Workflow file" line at all,
    /// exactly as if the field didn't exist.
    #[test]
    fn task_detail_omits_the_workflow_file_line_for_a_legacy_task() {
        let detail = json!({
            "id": "t1", "title": "x", "project_id": "p", "workflow_def": "chat",
            "status": "open", "created_at": "2026-08-01T12:00:00Z",
            "workflow_state": null,
            "workflow_path": null,
        });
        let rendered = task_detail(&detail);
        assert!(!rendered.contains("Workflow file"), "{rendered}");
    }

    /// A task with no config at all must render exactly as before.
    #[test]
    fn task_detail_without_config_adds_no_role_lines() {
        let detail = json!({
            "id": "t1", "title": "x", "project_id": "p", "workflow_def": "chat",
            "status": "open", "created_at": "2026-08-01T12:00:00Z",
            "workflow_state": null,
        });

        let rendered = task_detail(&detail);

        assert!(!rendered.contains("Role"), "{rendered}");
        assert!(!rendered.contains("Repo"), "{rendered}");
    }

    fn event(event_type: EventType, payload: Value) -> Event {
        Event {
            id: "e1".to_string(),
            task_id: "t1".to_string(),
            session_id: Some("r1".to_string()),
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
                json!({"adapter_session_id": "abc-123"}),
                "abc-123",
            ),
            (
                EventType::TurnCompleted,
                json!({"is_error": false}),
                "turn complete",
            ),
            (
                EventType::TurnCompleted,
                json!({"is_error": true}),
                "turn complete (error)",
            ),
            (EventType::Error, json!({"message": "boom"}), "boom"),
            (
                EventType::SessionNote,
                json!({"kind": "nudge", "message": "asked the agent to report"}),
                "[nudge] asked the agent to report",
            ),
            (
                EventType::AssistantMessage,
                json!({"text": "Now the CLI side.", "parent_tool_use_id": "toolu_agent"}),
                "[sub-agent] Now the CLI side.",
            ),
            (
                EventType::AssistantMessage,
                json!({"text": "one more thing", "after_completion": true}),
                "[late] one more thing",
            ),
            (
                EventType::AssistantMessage,
                json!({"text": "hi", "parent_tool_use_id": null}),
                "hi",
            ),
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
            // The one that matters most: a timed-out command whose process
            // group may have survived. The whole clause has to fit inside
            // `one_line`'s budget alongside a realistic command, or the
            // operator never sees the part that tells them something is
            // still running.
            (
                EventType::ShellOutput,
                json!({"stage": "build", "command": "make build",
                       "exit_code": null, "timed_out": true, "escaped": true,
                       "duration_ms": 300000, "stdout_tail": "", "stderr_tail": "",
                       "note": "could not confirm the process group exited — something may still be running"}),
                "$ make build → timed out could not confirm the process group exited — something may still be running",
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
            // A capturing turn (#45): the verdict it produced, and whether
            // the graph actually moved on it.
            (
                EventType::TurnOutcome,
                json!({"stage": "review", "capture": "json", "outcome": "approved",
                       "applied": true, "note": null}),
                "review turn → approved",
            ),
            // The park a reviewer stage relies on: an outcome computed but
            // deliberately not taken. It must not read as a transition.
            (
                EventType::TurnOutcome,
                json!({"stage": "review", "capture": "json", "outcome": "done",
                       "applied": false,
                       "note": "no 'outcome' key; parked"}),
                "review turn ⨯ done no 'outcome' key; parked",
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
            workflow_def: "chat".to_string(),
            title: "first line\nsecond line".to_string(),
            status: "open".to_string(),
            config: json!({}),
            worktree_repo: None,
            worktree_project: None,
            stuck_reason: None,
            workflow_path: None,
            workflow_sha256: None,
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
