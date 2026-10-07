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

/// Labels of the rows [`task_fields`] produces. The dashboard matches on
/// these (to drop rows on a short screen), so a rename here can't silently
/// break it.
pub const LABEL_TITLE: &str = "Title";
pub const LABEL_ID: &str = "ID";
pub const LABEL_PROJECT: &str = "Project";
pub const LABEL_WORKFLOW: &str = "Workflow";
pub const LABEL_WORKFLOW_FILE: &str = "Workflow file";
pub const LABEL_STATUS: &str = "Status";
pub const LABEL_STUCK: &str = "Stuck";
pub const LABEL_KEPT_WORKTREE: &str = "Kept worktree";
pub const LABEL_KEPT_BRANCH: &str = "Kept branch";
pub const LABEL_REPO: &str = "Repo";
pub const LABEL_ROLE: &str = "Role";
pub const LABEL_CREATED: &str = "Created";
pub const LABEL_STAGE: &str = "Stage";

/// The field rows `choco task status` prints for the daemon's `TaskDetail`
/// JSON, as `(label, value)` pairs: the order and wording are the CLI's, and
/// the dashboard's status view starts from the same rows.
pub fn task_fields(detail: &Value) -> Vec<(&'static str, String)> {
    let get = |key: &str| detail.get(key).and_then(Value::as_str).unwrap_or("-");

    let mut pairs = vec![
        (LABEL_TITLE, single_line(get("title"))),
        (LABEL_ID, get("id").to_string()),
        (LABEL_PROJECT, get("project_id").to_string()),
        (LABEL_WORKFLOW, get("workflow_def").to_string()),
    ];
    // Only present for a task created after issue #88 — a legacy task
    // (`workflow_path: null`) shows no such line, exactly as if this field
    // didn't exist. Where the file changed or has gone missing since the
    // task started, that's appended right onto this line rather than as a
    // separate one — it's a qualifier on *this* fact, not a fact of its
    // own.
    if let Some(path) = detail.get("workflow_path").and_then(Value::as_str) {
        let mut line = path.to_string();
        let builtin = path.starts_with("builtin:");
        match detail.get("workflow_file_status").and_then(Value::as_str) {
            Some("changed") if builtin => line.push_str(" (built-in updated since task start)"),
            Some("changed") => line.push_str(" (changed since task start)"),
            Some("missing") => line.push_str(" (missing)"),
            _ => {}
        }
        if let Some(sha) = detail.get("workflow_sha256").and_then(Value::as_str) {
            line.push_str(&format!("  [{}]", &sha[..sha.len().min(12)]));
        }
        pairs.push((
            if builtin {
                LABEL_WORKFLOW
            } else {
                LABEL_WORKFLOW_FILE
            },
            line,
        ));
    }
    pairs.push((LABEL_STATUS, get("status").to_string()));
    // Right after Status, so the reason for a stuck task (X-4, #61) reads
    // next to the status value that explains it needs one.
    if get("status") == "stuck"
        && let Some(reason) = detail.get("stuck_reason").and_then(Value::as_str)
    {
        pairs.push((LABEL_STUCK, single_line(reason)));
    }
    // A task cancelled with `--keep` (#102) handed its worktree and branch to
    // a person; say where, or the work is as good as lost.
    if let Some(kept) = detail.get("kept").filter(|k| !k.is_null()) {
        if let Some(path) = kept.get("worktree_path").and_then(Value::as_str) {
            pairs.push((LABEL_KEPT_WORKTREE, path.to_string()));
        }
        if let Some(branch) = kept.get("branch").and_then(Value::as_str) {
            pairs.push((LABEL_KEPT_BRANCH, branch.to_string()));
        }
    }
    // Same per-role lines `task` renders: `task status` is where an existing
    // task gets inspected, so leaving them out would mean `--json` was the
    // only way to see what `task reconfigure` actually did. `TaskDetail`
    // flattens the `Task`, so `config` is a top-level key here.
    if let Some(config) = detail.get("config") {
        if let Some(cwd) = config.get("cwd").and_then(Value::as_str) {
            pairs.push((LABEL_REPO, cwd.to_string()));
        }
        for (role, settings) in role_summaries(config) {
            pairs.push((LABEL_ROLE, format!("{role}: {settings}")));
        }
    }
    pairs.push((LABEL_CREATED, timestamp_str(get("created_at"))));

    if let Some(current) = detail_stage(detail) {
        pairs.push((LABEL_STAGE, current.to_string()));
    }
    pairs
}

/// The stage named by the detail's `workflow_state`, when it has one.
pub fn detail_stage(detail: &Value) -> Option<&str> {
    detail
        .get("workflow_state")
        .and_then(|s| s.get("current_stage"))
        .and_then(Value::as_str)
}

/// The detail's progress table (see [`stage_progress_table`]); `None` when
/// the task has no workflow state yet.
pub fn detail_progress(
    detail: &Value,
    now: DateTime<Utc>,
    width: Option<usize>,
) -> Option<ProgressTable> {
    detail.get("workflow_state").filter(|s| !s.is_null())?;
    // The trail is a sibling of `workflow_state`, not a field inside
    // it: X-3 moved it out of `stage_history` and into the events
    // timeline, which the daemon re-exposes here as `stage_trail`.
    let trail = detail
        .get("stage_trail")
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or_default();
    Some(stage_progress_table(
        trail,
        detail_stage(detail),
        now,
        width,
    ))
}

/// The loop count of one `loop_counters` value: a bare number, or the
/// engine's `{count: N, ...}` object.
pub fn loop_count(v: &Value) -> u64 {
    match v {
        Value::Number(n) => n.as_u64().unwrap_or(0),
        Value::Object(o) => o.get("count").and_then(Value::as_u64).unwrap_or(0),
        _ => 0,
    }
}

/// `Loop counters  internal_review=2 revising=1`, or `None` when there are
/// no counters.
pub fn loop_counters_line(detail: &Value) -> Option<String> {
    let counters = detail
        .get("workflow_state")
        .and_then(|s| s.get("loop_counters"))
        .and_then(Value::as_object)
        .filter(|c| !c.is_empty())?;
    let rendered: Vec<String> = counters
        .iter()
        .map(|(stage, count)| format!("{stage}={}", loop_count(count)))
        .collect();
    Some(format!("Loop counters  {}", rendered.join(" ")))
}

/// Renders the daemon's `TaskDetail` (a `Task` flattened alongside
/// `workflow_state`) from raw JSON — it has no exported Rust type.
pub fn task_detail(detail: &Value, now: DateTime<Utc>) -> String {
    let mut out = fields(&task_fields(detail));
    match detail_progress(detail, now, None) {
        Some(table) => {
            out.push_str("\n\nProgress\n");
            for line in table.header.iter().chain(&table.rows) {
                out.push_str(line);
                out.push('\n');
            }
            out.pop();
            if let Some(line) = loop_counters_line(detail) {
                out.push_str(&format!("\n\n{line}"));
            }
        }
        None => out.push_str("\n\n(no workflow state — the task has not started)"),
    }
    out.push_str("\n\n");
    out.push_str(&cost_and_time(detail));
    out
}

/// Money to two decimals, always marked approximate: `≈ $1.23`.
pub fn money(cost: f64) -> String {
    format!("≈ ${cost:.2}")
}

/// What a billing label means for the reader.
pub fn billing_text(label: &str) -> &'static str {
    if label == "api_equivalent" {
        "API-equivalent"
    } else {
        "estimated"
    }
}

/// `≈ $1.23 (API-equivalent)`, or `cost unknown` when no turn's cost is known.
pub fn cost_with_label(cost: Option<f64>, label: &str) -> String {
    match cost {
        Some(cost) => format!("{} ({})", money(cost), billing_text(label)),
        None => "cost unknown".to_string(),
    }
}

/// What makes a task's total partial, as trailing notes: sessions that
/// ended without recording a turn, and turns whose cost is unknown (their
/// cost is left out of the total).
pub fn partial_notes(usage: &Value) -> String {
    let count = |key: &str| usage.get(key).and_then(Value::as_u64).unwrap_or(0);
    let mut out = String::new();
    match count("sessions_without_data") {
        0 => {}
        1 => out.push_str("  (1 session without data)"),
        n => out.push_str(&format!("  ({n} sessions without data)")),
    }
    match count("turns_without_cost") {
        0 => {}
        1 => out.push_str("  (1 turn without a cost)"),
        n => out.push_str(&format!("  ({n} turns without a cost)")),
    }
    out
}

fn token_count(v: Option<&Value>) -> String {
    v.and_then(Value::as_u64)
        .map_or_else(|| "?".to_string(), |n| n.to_string())
}

/// `input 30 · output 15 · cache read 300 · cache write 60`, unknowns as
/// `?`; `short` abbreviates the first two words (`in`, `out`).
fn token_line(tokens: &Value, short: bool) -> String {
    let (input, output) = if short {
        ("in", "out")
    } else {
        ("input", "output")
    };
    format!(
        "{input} {} · {output} {} · cache read {} · cache write {}",
        token_count(tokens.get("input")),
        token_count(tokens.get("output")),
        token_count(tokens.get("cache_read")),
        token_count(tokens.get("cache_write")),
    )
}

/// One breakdown row's value: cost and tokens, or `no data` for a group
/// none of whose sessions recorded a turn.
fn usage_group_value(entry: &Value) -> String {
    let Some(tokens) = entry.get("tokens").filter(|t| t.is_object()) else {
        return "no data".to_string();
    };
    let cost = entry
        .get("cost_usd")
        .and_then(Value::as_f64)
        .map_or_else(|| "cost unknown".to_string(), money);
    format!("{cost}  ({})", token_line(tokens, true))
}

fn usage_breakdown(
    out: &mut String,
    heading: &str,
    entries: &[Value],
    name: impl Fn(&Value) -> String,
) {
    if entries.is_empty() {
        return;
    }
    out.push_str(&format!("\n  {heading}"));
    let names: Vec<String> = entries.iter().map(&name).collect();
    let width = names
        .iter()
        .map(|n| n.chars().count())
        .max()
        .unwrap_or(0)
        .max(14)
        + 1;
    for (entry, name) in entries.iter().zip(names) {
        out.push_str(&format!("\n    {name:<width$}{}", usage_group_value(entry)));
    }
}

/// The "Cost & time" block of `choco task status`, from the detail's
/// `usage` object (null when the task has no recorded turns).
pub fn cost_and_time(detail: &Value) -> String {
    let Some(usage) = detail.get("usage").filter(|u| u.is_object()) else {
        return "Cost & time  no data".to_string();
    };
    let label = usage
        .get("billing_label")
        .and_then(Value::as_str)
        .unwrap_or("estimated");
    let mut total = cost_with_label(usage.get("cost_usd").and_then(Value::as_f64), label);
    total.push_str(&partial_notes(usage));
    let duration = |key: &str| {
        usage
            .get(key)
            .and_then(Value::as_i64)
            .map(|ms| crate::dashboard::app::fmt_duration(chrono::Duration::milliseconds(ms)))
            .unwrap_or_else(|| "no data".to_string())
    };
    let mut out = String::from("Cost & time");
    out.push_str(&format!("\n  Total        {total}"));
    out.push_str(&format!(
        "\n  Tokens       {}",
        token_line(usage.get("tokens").unwrap_or(&Value::Null), false)
    ));
    out.push_str(&format!("\n  Wall time    {}", duration("wall_time_ms")));
    out.push_str(&format!("\n  Active time  {}", duration("active_time_ms")));
    let list = |key: &str| {
        usage
            .get(key)
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default()
    };
    let text = |entry: &Value, key: &str| {
        entry
            .get(key)
            .and_then(Value::as_str)
            .unwrap_or("?")
            .to_string()
    };
    usage_breakdown(&mut out, "By stage", &list("by_stage"), |e| {
        text(e, "stage")
    });
    usage_breakdown(&mut out, "By role", &list("by_role"), |e| text(e, "role"));
    usage_breakdown(&mut out, "By lap", &list("by_lap"), |e| {
        let lap = e
            .get("lap")
            .and_then(Value::as_i64)
            .map_or_else(|| "?".to_string(), |n| n.to_string());
        format!("{} #{lap}", text(e, "stage"))
    });
    usage_breakdown(&mut out, "By model", &list("by_model"), |e| {
        text(e, "model")
    });
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
///
/// Rendered as an aligned table (#211): `#`, `from`, `outcome`, `to`, `at
/// (UTC)`, then a `◀ current` marker. The header comes back separately so
/// the dashboard can style it and keep it out of its "earlier steps" cut.
/// Column widths are computed over the whole trail and the header titles, so
/// they don't depend on which rows a caller shows.
///
/// `width`: `None` never truncates. With `Some(w)` the `from`/`outcome`/`to`
/// columns shrink (widest first, leftmost on a tie, to a floor of five
/// characters) until the widest line fits; failing that `◀ current` becomes
/// `◀`. Lines still wider than `w` are the caller's to cut.
pub struct ProgressTable {
    /// Column titles; `None` when there is no table (no trail).
    pub header: Option<String>,
    /// One line per step (or the single no-table line).
    pub rows: Vec<String>,
}

/// One row's cells, before layout.
struct StepCells {
    num: String,
    from: String,
    outcome: String,
    to: String,
    at: String,
    current: bool,
}

const MARKER: &str = "◀ current";
const MARKER_SHORT: &str = "◀";

/// Cuts `s` to `w` characters, ending in `…` when something was dropped.
fn cut_cell(s: &str, w: usize) -> String {
    if s.chars().count() <= w {
        return s.to_string();
    }
    if w == 0 {
        return String::new();
    }
    let mut out: String = s.chars().take(w - 1).collect();
    out.push('…');
    out
}

/// Lays the cells out at the given column widths `[num, from, outcome, to,
/// at]`. The `at` column is padded only on a row carrying the marker, so
/// the marker lines up and nothing else has trailing spaces.
fn layout_row(cells: [&str; 5], widths: &[usize; 5], marker: Option<&str>) -> String {
    let mut line = format!("  {:>w$}", cells[0], w = widths[0]);
    for i in 1..4 {
        line.push_str(&format!(
            "  {:<w$}",
            cut_cell(cells[i], widths[i]),
            w = widths[i]
        ));
    }
    line.push_str("  ");
    line.push_str(cells[4]);
    if let Some(marker) = marker {
        let pad = widths[4].saturating_sub(cells[4].chars().count());
        line.push_str(&" ".repeat(pad));
        line.push_str("  ");
        line.push_str(marker);
    }
    line.trim_end().to_string()
}

pub fn stage_progress_table(
    trail: &[Value],
    current: Option<&str>,
    now: DateTime<Utc>,
    width: Option<usize>,
) -> ProgressTable {
    let stage_of = |entry: &Value| {
        single_line(
            entry
                .get("payload")
                .and_then(|p| p.get("stage"))
                .and_then(Value::as_str)
                .unwrap_or("?"),
        )
    };

    let mut steps: Vec<StepCells> = Vec::new();
    for (i, entry) in trail.iter().enumerate() {
        let stage = stage_of(entry);
        // A missing or unparseable time is an empty cell, by design: the
        // step is still real, only its time is unknown.
        let at = entry
            .get("created_at")
            .and_then(Value::as_str)
            .and_then(|raw| DateTime::parse_from_rfc3339(raw).ok())
            .map(|t| t.with_timezone(&Utc))
            .map(|t| {
                if t.date_naive() == now.date_naive() {
                    t.format("%H:%M:%S").to_string()
                } else {
                    t.format("%Y-%m-%d %H:%M:%S").to_string()
                }
            })
            .unwrap_or_default();

        // A null `outcome` — not a missing predecessor — is what marks a
        // starting point: the engine writes `entered_via: None` only for a
        // stage nothing transitioned into. Keying on the predecessor
        // instead would label the *first surviving* entry "start" on a
        // trail whose head has been truncated, inventing a beginning that
        // never happened and discarding the recorded outcome with it.
        // Retention prunes `stage_entered` rows like any other event, and
        // the entry-stage append is best-effort, so a trail that opens
        // mid-flight is reachable, not hypothetical.
        let (from, outcome) = match (
            entry
                .get("payload")
                .and_then(|p| p.get("outcome"))
                .and_then(Value::as_str),
            i.checked_sub(1).and_then(|p| trail.get(p)),
        ) {
            (Some(outcome), Some(previous)) => (stage_of(previous), single_line(outcome)),
            // Something carried the task here, but whatever it departed is
            // no longer on record — say so rather than guessing or dropping
            // the outcome.
            (Some(outcome), None) => ("…".to_string(), single_line(outcome)),
            (None, _) => (String::new(), "start".to_string()),
        };
        steps.push(StepCells {
            num: (i + 1).to_string(),
            from,
            outcome,
            to: stage,
            at,
            current: false,
        });
    }

    if steps.is_empty() {
        let line = match current {
            Some(current) => format!("  → {current} (current, no transitions yet)"),
            None => "  (none)".to_string(),
        };
        return ProgressTable {
            header: None,
            rows: vec![line],
        };
    }

    if let Some(current) = current {
        // The last entry *is* the current stage — `enter_stage` records on
        // entry — so this marks it in place rather than repeating it on a
        // trailing arrow row. It's still worth stating: a mismatch means
        // the trail was truncated by retention, and silently rendering a
        // stale last hop as "where the task is" would be a lie.
        if trail.last().map(stage_of).as_deref() == Some(single_line(current).as_str()) {
            if let Some(last) = steps.last_mut() {
                last.current = true;
            }
        } else {
            steps.push(StepCells {
                num: "→".to_string(),
                from: String::new(),
                outcome: String::new(),
                to: single_line(current),
                at: String::new(),
                current: true,
            });
        }
    }

    let titles = ["#", "from", "outcome", "to", "at (UTC)"];
    let cells_of = |s: &StepCells| -> [String; 5] {
        [
            s.num.clone(),
            s.from.clone(),
            s.outcome.clone(),
            s.to.clone(),
            s.at.clone(),
        ]
    };
    let all: Vec<[String; 5]> = steps.iter().map(cells_of).collect();
    let mut natural = [0usize; 5];
    for (i, t) in titles.iter().enumerate() {
        natural[i] = t.chars().count();
    }
    for row in &all {
        for (i, c) in row.iter().enumerate() {
            natural[i] = natural[i].max(c.chars().count());
        }
    }

    let render = |widths: &[usize; 5], marker: &str| -> (String, Vec<String>) {
        let header = layout_row(titles, widths, None);
        let rows = steps
            .iter()
            .zip(&all)
            .map(|(s, c)| {
                let cells = [
                    c[0].as_str(),
                    c[1].as_str(),
                    c[2].as_str(),
                    c[3].as_str(),
                    c[4].as_str(),
                ];
                layout_row(cells, widths, s.current.then_some(marker))
            })
            .collect();
        (header, rows)
    };
    let widest = |(header, rows): &(String, Vec<String>)| {
        rows.iter()
            .chain(std::iter::once(header))
            .map(|l| l.chars().count())
            .max()
            .unwrap_or(0)
    };

    let mut widths = natural;
    let mut marker = MARKER;
    let mut laid = render(&widths, marker);
    if let Some(limit) = width {
        let floor = |i: usize| natural[i].min(5);
        while widest(&laid) > limit {
            // Widest of from/outcome/to; the leftmost of a tie.
            let pick = (1..=3)
                .filter(|&i| widths[i] > floor(i))
                .fold(None::<usize>, |best, i| match best {
                    Some(b) if widths[b] >= widths[i] => Some(b),
                    _ => Some(i),
                });
            let Some(i) = pick else { break };
            widths[i] -= 1;
            laid = render(&widths, marker);
        }
        if widest(&laid) > limit {
            marker = MARKER_SHORT;
            laid = render(&widths, marker);
        }
    }
    ProgressTable {
        header: Some(laid.0),
        rows: laid.1,
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
pub(crate) fn event_summary(event: &Event) -> String {
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
pub fn single_line(text: &str) -> String {
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

/// `3h12m`, `5m`, `42s`: how long the daemon has been up.
fn uptime(secs: i64) -> String {
    let secs = secs.max(0);
    let (d, h, m) = (secs / 86400, secs % 86400 / 3600, secs % 3600 / 60);
    if d > 0 {
        format!("{d}d{h}h")
    } else if h > 0 {
        format!("{h}h{m}m")
    } else if m > 0 {
        format!("{m}m")
    } else {
        format!("{secs}s")
    }
}

/// `choco server status`'s human output (#84): one line per item, then any
/// warnings.
pub fn server_status(
    s: &chocofactory_core::models::ServerStatus,
    log: &std::path::Path,
    now: DateTime<Utc>,
) -> String {
    use chocofactory_core::version::{VERSION, long_version};
    let commit = |c: Option<&str>| c.unwrap_or("dev build").to_string();
    let mut out = vec![
        format!(
            "chocofactoryd {} ({})  running  pid {}  port {}  up {}",
            s.version,
            commit(s.commit.as_deref()),
            s.pid,
            s.port,
            uptime((now - s.started_at).num_seconds())
        ),
        format!("choco         {}", long_version()),
        format!("binary        {}", s.exe),
        format!("agents' choco {}", s.choco_binary),
    ];
    let tasks: Vec<String> = s
        .tasks
        .iter()
        .filter(|(_, n)| **n > 0)
        .map(|(status, n)| format!("{n} {status}"))
        .collect();
    out.push(format!(
        "tasks         {}",
        if tasks.is_empty() {
            "none".to_string()
        } else {
            tasks.join(", ")
        }
    ));
    if s.in_flight.is_empty() {
        out.push("in flight     none".to_string());
    } else {
        for f in &s.in_flight {
            out.push(format!(
                "in flight     {}  {} ({})  {}",
                f.task_id,
                f.stage,
                f.kind,
                single_line(&f.title)
            ));
        }
    }
    out.push(format!("log           {}", log.display()));
    if s.version != VERSION {
        out.push(format!(
            "warning: choco {VERSION} is talking to chocofactoryd {}; run `choco server restart` (or `choco update`)",
            s.version
        ));
    }
    if s.exe_replaced == Some(true) {
        out.push("warning: the chocofactoryd binary changed on disk since this daemon started; `choco server restart` picks it up".to_string());
    }
    if !s.choco_binary_found {
        out.push(format!(
            "warning: agents' report_outcome tool will not work: {} not found",
            s.choco_binary
        ));
    }
    out.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_status() -> chocofactory_core::models::ServerStatus {
        use chocofactory_core::models::{InFlight, ServerStatus};
        let now = Utc::now();
        ServerStatus {
            version: "0.0.0".into(),
            commit: Some("abc1234".into()),
            pid: 42,
            port: 4141,
            started_at: now - chrono::Duration::seconds(3 * 3600 + 12 * 60),
            config_root: "/root".into(),
            exe: "/bin/chocofactoryd".into(),
            exe_replaced: Some(true),
            choco_binary: "/bin/choco".into(),
            choco_binary_found: false,
            tasks: [
                ("open".to_string(), 2),
                ("stuck".to_string(), 1),
                ("done".to_string(), 0),
            ]
            .into_iter()
            .collect(),
            in_flight: vec![
                InFlight {
                    task_id: "t1".into(),
                    title: "two\nlines".into(),
                    stage: "work".into(),
                    kind: "agent_turn".into(),
                },
                InFlight {
                    task_id: "t2".into(),
                    title: "x".into(),
                    stage: "build".into(),
                    kind: "shell".into(),
                },
            ],
        }
    }

    #[test]
    fn server_status_renders_items_and_warnings() {
        let s = sample_status();
        let out = server_status(
            &s,
            std::path::Path::new("/log"),
            s.started_at + chrono::Duration::seconds(3 * 3600 + 12 * 60),
        );
        assert!(
            out.contains("chocofactoryd 0.0.0 (abc1234)  running  pid 42  port 4141  up 3h12m"),
            "{out}"
        );
        assert!(out.contains("tasks         2 open, 1 stuck"), "{out}");
        assert!(
            out.contains("in flight     t1  work (agent_turn)  two lines"),
            "{out}"
        );
        assert!(out.contains("in flight     t2  build (shell)  x"), "{out}");
        assert!(out.contains("log           /log"), "{out}");
        assert!(out.contains("warning: choco "), "{out}");
        assert!(out.contains("binary changed on disk"), "{out}");
        assert!(
            out.contains("report_outcome tool will not work: /bin/choco not found"),
            "{out}"
        );
    }

    #[test]
    fn server_status_quiet_case_has_no_warnings() {
        let mut s = sample_status();
        s.version = chocofactory_core::version::VERSION.to_string();
        s.exe_replaced = Some(false);
        s.choco_binary_found = true;
        s.tasks.clear();
        s.in_flight.clear();
        let out = server_status(&s, std::path::Path::new("/log"), s.started_at);
        assert!(!out.contains("warning:"), "{out}");
        assert!(out.contains("tasks         none"), "{out}");
        assert!(out.contains("in flight     none"), "{out}");
    }

    #[test]
    fn uptime_units() {
        assert_eq!(uptime(5), "5s");
        assert_eq!(uptime(125), "2m");
        assert_eq!(uptime(3700), "1h1m");
        assert_eq!(uptime(90000), "1d1h");
        assert_eq!(uptime(-3), "0s");
    }

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

    fn test_now() -> DateTime<Utc> {
        DateTime::parse_from_rfc3339("2026-08-02T10:00:00Z")
            .unwrap()
            .with_timezone(&Utc)
    }

    fn table_of(trail: &[Value], current: Option<&str>, width: Option<usize>) -> ProgressTable {
        stage_progress_table(trail, current, test_now(), width)
    }

    /// Header and rows, as the CLI prints them.
    fn lines_of(t: &ProgressTable) -> Vec<String> {
        t.header.iter().chain(&t.rows).cloned().collect()
    }

    /// Character offset of `needle` in `line`.
    fn col(line: &str, needle: &str) -> usize {
        let byte = line
            .find(needle)
            .unwrap_or_else(|| panic!("{needle:?} in {line:?}"));
        line[..byte].chars().count()
    }

    fn long_trail() -> Vec<Value> {
        let stages = ["coding", "internal_review", "awaiting_human_review", "ci"];
        let mut trail = vec![stage_entry("coding", Value::Null, "2026-08-01T08:00:00Z")];
        for i in 1..12 {
            trail.push(stage_entry(
                stages[i % 4],
                json!(["done", "changes_requested", "green"][i % 3]),
                &format!("2026-08-01T08:{i:02}:00Z"),
            ));
        }
        trail
    }

    /// The `to` and `at` cells of every row start at the header's offsets,
    /// and the `#` cells end where the header's `#` does.
    fn assert_aligned(t: &ProgressTable) {
        let header = t.header.as_deref().unwrap();
        let to = col(header, "to");
        let at = col(header, "at (UTC)");
        let num_end = col(header, "#") + 1;
        for row in &t.rows {
            // A row with an empty time cell is trimmed short.
            let mut chars: Vec<char> = row.chars().collect();
            chars.resize(chars.len().max(at + 1), ' ');
            assert_eq!(chars[to - 2..to], [' ', ' '], "to column: {row:?}");
            assert_eq!(chars[at - 2..at], [' ', ' '], "at column: {row:?}");
            // The `#` cell is right-aligned: a digit (or arrow) right before
            // the two-space gutter that ends at the `from` column.
            assert_ne!(chars[num_end - 1], ' ', "# column: {row:?}");
            assert_eq!(chars[num_end..num_end + 2], [' ', ' '], "# gutter: {row:?}");
        }
    }

    /// `task status` prints the helper's header and rows, unshrunk, right
    /// under `Progress`.
    #[test]
    fn task_detail_prints_the_progress_table_under_the_heading() {
        let trail = long_trail();
        let detail = json!({
            "id": "t1", "title": "x", "project_id": "p", "workflow_def": "chat",
            "status": "open", "created_at": "2026-08-01T12:00:00Z",
            "workflow_state": {
                "task_id": "t1", "current_stage": "ci", "loop_counters": {},
                "payload": {}, "updated_at": "2026-08-01T12:00:00Z",
            },
            "stage_trail": trail,
        });
        let table = stage_progress_table(&trail, Some("ci"), test_now(), None);
        let expected = format!("Progress\n{}", lines_of(&table).join("\n"));
        assert!(
            task_detail(&detail, test_now()).contains(&expected),
            "{}",
            task_detail(&detail, test_now())
        );
    }

    #[test]
    fn the_table_aligns_columns_for_mixed_width_stages_and_two_digit_steps() {
        let mut trail = long_trail();
        trail.push(stage_entry("ci", json!("green"), "2026-08-01T09:00:00Z"));
        let t = table_of(&trail, Some("ci"), None);
        assert_eq!(t.rows.len(), 13);
        assert_aligned(&t);
        assert!(t.header.as_deref().unwrap().ends_with("at (UTC)"));
        for line in lines_of(&t) {
            assert_eq!(line, line.trim_end(), "trailing space");
        }
    }

    #[test]
    fn a_start_row_has_no_from_and_says_start() {
        let trail = [stage_entry("chatting", Value::Null, "2026-08-02T09:00:00Z")];
        let t = table_of(&trail, Some("chatting"), None);
        let tokens: Vec<&str> = t.rows[0].split_whitespace().collect();
        assert_eq!(
            tokens,
            ["1", "start", "chatting", "09:00:00", "◀", "current"]
        );
        assert!(!t.rows[0].contains('?'), "no phantom predecessor");
    }

    #[test]
    fn a_hop_names_the_previous_stage_the_outcome_and_the_stage() {
        let trail = [
            stage_entry("gate", Value::Null, "2026-08-02T09:00:00Z"),
            stage_entry("review", json!("resumed"), "2026-08-02T09:01:00Z"),
        ];
        let t = table_of(&trail, Some("review"), None);
        let tokens: Vec<&str> = t.rows[1].split_whitespace().collect();
        assert_eq!(
            tokens,
            ["2", "gate", "resumed", "review", "09:01:00", "◀", "current"]
        );
        // The marker is on the last row only, in place — no trailing arrow.
        assert!(!t.rows[0].contains('◀'));
        assert_eq!(t.rows.len(), 2);
    }

    /// Retention prunes `stage_entered` rows like any other event, so the
    /// first surviving entry can be one the task was transitioned into.
    /// Labelling it "start" would assert a beginning that never happened and
    /// throw away the recorded outcome, so the outcome decides.
    #[test]
    fn a_pruned_trail_head_is_not_claimed_to_be_the_start() {
        let trail = [
            stage_entry("review", json!("changes_requested"), "2026-08-02T09:00:00Z"),
            stage_entry("gate", json!("rejected"), "2026-08-02T09:01:00Z"),
        ];
        let t = table_of(&trail, Some("gate"), None);
        let head: Vec<&str> = t.rows[0].split_whitespace().collect();
        assert_eq!(head[..4], ["1", "…", "changes_requested", "review"]);
        assert!(!t.rows[0].contains("start"), "{}", t.rows[0]);
        let next: Vec<&str> = t.rows[1].split_whitespace().collect();
        assert_eq!(next[..4], ["2", "review", "rejected", "gate"]);
    }

    /// Retention ages events out, so a stale last hop must not be rendered
    /// as "where the task is" when it disagrees with `current_stage`.
    #[test]
    fn a_stale_trail_gets_an_arrow_row_for_the_current_stage() {
        let trail = [stage_entry("gate", Value::Null, "2026-08-02T09:00:00Z")];
        let t = table_of(&trail, Some("done"), None);
        assert_eq!(t.rows.len(), 2);
        assert!(!t.rows[0].contains('◀'), "{}", t.rows[0]);
        let tokens: Vec<&str> = t.rows[1].split_whitespace().collect();
        assert_eq!(tokens, ["→", "done", "◀", "current"]);
        assert_aligned(&t);
    }

    /// A task that ran before X-3 has no `stage_entered` events and no
    /// backfill, so an absent trail must read as "nothing recorded".
    #[test]
    fn an_empty_trail_has_no_table() {
        let t = table_of(&[], Some("chatting"), None);
        assert!(t.header.is_none());
        assert_eq!(t.rows, ["  → chatting (current, no transitions yet)"]);
        let t = table_of(&[], None, None);
        assert!(t.header.is_none());
        assert_eq!(t.rows, ["  (none)"]);
    }

    #[test]
    fn a_current_stage_with_odd_whitespace_matches_the_last_row() {
        let trail = [stage_entry(
            "two  words",
            Value::Null,
            "2026-08-02T09:00:00Z",
        )];
        let t = table_of(&trail, Some("two  words"), None);
        assert_eq!(t.rows.len(), 1, "{:?}", t.rows);
        assert!(t.rows[0].ends_with("◀ current"));
    }

    #[test]
    fn a_trail_without_a_current_stage_marks_nothing() {
        let trail = [stage_entry("gate", Value::Null, "2026-08-02T09:00:00Z")];
        let t = table_of(&trail, None, None);
        assert_eq!(t.rows.len(), 1);
        assert!(!t.rows[0].contains('◀'));
    }

    #[test]
    fn times_are_clock_only_today_and_dated_otherwise() {
        let trail = [
            stage_entry("a", Value::Null, "2026-08-02T09:00:00Z"),
            stage_entry("b", json!("done"), "2026-08-01T08:00:00Z"),
            stage_entry("c", json!("done"), "2026-08-01T23:59:59Z"),
            stage_entry("d", json!("done"), "2026-08-02T11:30:00+02:00"),
        ];
        let t = table_of(&trail, None, None);
        assert!(t.rows[0].ends_with("  09:00:00"), "{}", t.rows[0]);
        assert!(t.rows[1].ends_with("2026-08-01 08:00:00"), "{}", t.rows[1]);
        assert!(t.rows[2].ends_with("2026-08-01 23:59:59"), "{}", t.rows[2]);
        // An offset time is converted to UTC before comparing dates.
        assert!(t.rows[3].ends_with("  09:30:00"), "{}", t.rows[3]);
        assert!(!t.rows[3].contains("2026-08-02"), "{}", t.rows[3]);
    }

    #[test]
    fn a_missing_or_unparseable_time_is_an_empty_cell() {
        let mut no_time = stage_entry("b", json!("done"), "x");
        no_time.as_object_mut().unwrap().remove("created_at");
        let trail = [
            stage_entry("a", Value::Null, "not-a-time"),
            no_time,
            stage_entry("c", json!("done"), "2026-08-02T09:00:00Z"),
        ];
        let t = table_of(&trail, Some("c"), None);
        assert_eq!(t.rows.len(), 3);
        assert_eq!(t.rows[0].split_whitespace().last(), Some("a"));
        assert!(!t.rows[0].contains("not-a-time"));
        assert_eq!(t.rows[1].split_whitespace().last(), Some("b"));
        assert_aligned(&t);
    }

    fn wide_trail() -> Vec<Value> {
        let a = "a_very_long_stage_name_one";
        let b = "a_very_long_stage_name_two";
        let c = "a_very_long_stage_name_three";
        vec![
            stage_entry(a, Value::Null, "2026-08-01T08:00:00Z"),
            stage_entry(b, json!("outcome_that_is_long_too"), "2026-08-01T09:00:00Z"),
            stage_entry(
                c,
                json!("another_long_outcome_here"),
                "2026-08-01T10:00:00Z",
            ),
        ]
    }

    #[test]
    fn a_width_shrinks_stage_columns_but_keeps_every_time() {
        let trail = wide_trail();
        let t = table_of(&trail, Some("a_very_long_stage_name_three"), Some(60));
        for line in lines_of(&t) {
            assert!(line.chars().count() <= 60, "{line:?}");
        }
        assert!(t.rows.iter().any(|r| r.contains('…')), "{:?}", t.rows);
        for (row, time) in t.rows.iter().zip(["08:00:00", "09:00:00", "10:00:00"]) {
            assert!(row.contains(&format!("2026-08-01 {time}")), "{row}");
        }
        assert!(t.rows[2].ends_with("◀ current"), "{}", t.rows[2]);
        assert_aligned(&t);
    }

    #[test]
    fn no_width_never_truncates() {
        let trail = wide_trail();
        let t = table_of(&trail, Some("a_very_long_stage_name_three"), None);
        assert!(t.rows.iter().all(|r| !r.contains('…')), "{:?}", t.rows);
        assert!(t.rows.iter().any(|r| r.chars().count() > 60));
    }

    #[test]
    fn shrinking_takes_from_the_widest_column_leftmost_first() {
        let trail = [
            stage_entry("aaaaaaaaaa", Value::Null, "2026-08-01T08:00:00Z"),
            stage_entry("bbbbbbbbbb", json!("cccccccccc"), "2026-08-01T09:00:00Z"),
        ];
        let full = table_of(&trail, None, None);
        let natural = full.rows[1].chars().count();
        // One character too wide: the leftmost of the tied columns (from)
        // loses it, not outcome or to.
        let t = table_of(&trail, None, Some(natural - 1));
        assert!(t.rows[1].contains("aaaaaaaa…"), "{}", t.rows[1]);
        assert!(t.rows[1].contains("cccccccccc"), "{}", t.rows[1]);
        assert!(t.rows[1].contains("bbbbbbbbbb"), "{}", t.rows[1]);
    }

    #[test]
    fn the_marker_shortens_only_once_the_columns_are_at_their_floors() {
        let trail = wide_trail();
        let cur = Some("a_very_long_stage_name_three");
        // Floors: from/outcome/to at 5 each.
        let floors = table_of(&trail, cur, Some(0));
        let floor_marker = floors.rows[2].chars().count();
        assert!(floors.rows[2].ends_with("◀"), "{}", floors.rows[2]);
        assert!(!floors.rows[2].ends_with("current"), "{}", floors.rows[2]);
        // With room for the long marker once shrunk, it stays whole.
        let fits = table_of(&trail, cur, Some(floor_marker + "◀ current".len() - 1 + 1));
        assert!(fits.rows[2].ends_with("◀ current"), "{}", fits.rows[2]);
        assert_aligned(&floors);
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
        let rendered = task_detail(&detail, test_now());
        let row = rendered
            .lines()
            .find(|l| l.contains("resumed"))
            .unwrap_or_else(|| panic!("no hop row: {rendered}"));
        let tokens: Vec<&str> = row.split_whitespace().collect();
        assert_eq!(
            tokens,
            [
                "2",
                "gate",
                "resumed",
                "review",
                "2026-08-01",
                "11:58:18",
                "◀",
                "current"
            ]
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
        let rendered = task_detail(&detail, test_now());
        assert!(rendered.contains("has not started"), "{rendered}");
        assert!(!rendered.contains("Stage "), "{rendered}");
    }

    #[test]
    fn task_detail_names_the_kept_worktree_and_branch() {
        let detail = json!({
            "id": "t1", "title": "x", "status": "cancelled", "kept_work": true,
            "created_at": "2030-01-01T00:00:00Z", "stage_trail": [],
            "kept": {"worktree_path": "/work/demo-wt-t1", "branch": "task/t1"},
        });
        let rendered = task_detail(&detail, test_now());
        assert!(rendered.contains("/work/demo-wt-t1"), "{rendered}");
        assert!(rendered.contains("task/t1"), "{rendered}");
        let plain = json!({
            "id": "t1", "title": "x", "status": "cancelled", "kept_work": false,
            "created_at": "2030-01-01T00:00:00Z", "stage_trail": [], "kept": null,
        });
        assert!(!task_detail(&plain, test_now()).contains("Kept"));
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
            kept_work: false,
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

        let rendered = task_detail(&detail, test_now());

        assert!(rendered.contains("/src/app"), "{rendered}");
        assert!(
            rendered.contains("coder: cli=claude, model=opus"),
            "{rendered}"
        );
        assert!(rendered.contains("reviewer: model=sonnet"), "{rendered}");
    }

    /// Loop counters print as counts, from an object or a bare number.
    #[test]
    fn task_detail_renders_loop_counters_as_counts() {
        let detail = json!({
            "id": "t1", "title": "T", "workflow_state": {
                "current_stage": "revising",
                "loop_counters": {"internal_review": {"count": 2}, "revising": 1}
            }
        });
        let rendered = task_detail(&detail, test_now());
        assert!(
            rendered.contains("\n\nLoop counters  internal_review=2 revising=1\n\nCost & time"),
            "{rendered}"
        );
        let none = json!({"id": "t1", "workflow_state": {"loop_counters": {}}});
        assert!(!task_detail(&none, test_now()).contains("Loop counters"));
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
        let rendered = task_detail(&stuck, test_now());
        assert!(rendered.contains("Stuck"), "{rendered}");
        assert!(rendered.contains("stage 'run': it broke"), "{rendered}");

        // An open task with no stuck_reason at all gets no such line.
        let open = json!({
            "id": "t1", "title": "x", "project_id": "p", "workflow_def": "chat",
            "status": "open", "created_at": "2026-08-01T12:00:00Z",
            "workflow_state": null,
        });
        let rendered = task_detail(&open, test_now());
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
        let rendered = task_detail(&unchanged, test_now());
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
        let rendered = task_detail(&changed, test_now());
        assert!(rendered.contains("changed since task start"), "{rendered}");

        // missing: a different suffix, not "changed since task start".
        let mut missing = base.clone();
        missing["workflow_file_status"] = json!("missing");
        let rendered = task_detail(&missing, test_now());
        assert!(rendered.contains("(missing)"), "{rendered}");
        assert!(!rendered.contains("changed since task start"), "{rendered}");
    }

    #[test]
    fn task_detail_labels_a_builtin_and_flags_an_update() {
        let detail = json!({
            "id": "t1", "title": "x", "project_id": "p", "workflow_def": "chat",
            "status": "open", "created_at": "2026-08-01T12:00:00Z",
            "workflow_state": null,
            "workflow_path": "builtin:chat@1.0.0",
            "workflow_sha256": "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcd",
            "workflow_file_status": "changed",
        });
        let rendered = task_detail(&detail, test_now());
        assert!(!rendered.contains("Workflow file"), "{rendered}");
        assert!(rendered.contains("builtin:chat@1.0.0"), "{rendered}");
        assert!(
            rendered.contains("(built-in updated since task start)"),
            "{rendered}"
        );
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
        let rendered = task_detail(&detail, test_now());
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

        let rendered = task_detail(&detail, test_now());

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
            kept_work: false,
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

    fn usage_detail(label: &str, without_data: u64) -> Value {
        json!({
            "id": "t1", "title": "x", "project_id": "p", "workflow_def": "chat",
            "status": "open", "created_at": "2026-08-01T12:00:00Z",
            "workflow_state": null,
            "usage": {
                "cost_usd": 0.09, "billing_label": label,
                "tokens": {"input": 30, "output": 15, "cache_read": 300, "cache_write": null},
                "wall_time_ms": 7_500_000, "active_time_ms": 4_200_000,
                "sessions_without_data": without_data,
                "by_stage": [
                    {"stage": "implement", "cost_usd": 0.05,
                     "tokens": {"input": 20, "output": 10, "cache_read": 200, "cache_write": 40}},
                    {"stage": "review", "cost_usd": null, "tokens": null},
                ],
                "by_role": [
                    {"role": "coder", "cost_usd": 0.05,
                     "tokens": {"input": 20, "output": 10, "cache_read": 200, "cache_write": 40}},
                ],
                "by_lap": [
                    {"stage": "implement", "lap": 1, "cost_usd": 0.03,
                     "tokens": {"input": 1, "output": 2, "cache_read": 3, "cache_write": 4}},
                    {"stage": "implement", "lap": null, "cost_usd": null, "tokens": null},
                ],
                "by_model": [
                    {"model": "mock-model", "cost_usd": 0.09,
                     "tokens": {"input": 30, "output": 15, "cache_read": 300, "cache_write": 60}},
                ],
            },
        })
    }

    #[test]
    fn task_detail_renders_the_cost_and_time_block() {
        let rendered = task_detail(&usage_detail("api_equivalent", 0), test_now());
        let block = rendered.split("Cost & time").nth(1).expect("block present");
        for expected in [
            "  Total        ≈ $0.09 (API-equivalent)\n",
            "  Tokens       input 30 · output 15 · cache read 300 · cache write ?\n",
            "  Wall time    2h05m\n",
            "  Active time  1h10m\n",
            "  By stage\n",
            "    implement      ≈ $0.05  (in 20 · out 10 · cache read 200 · cache write 40)\n",
            "    review         no data\n",
            "  By role\n",
            "    coder          ≈ $0.05  (in 20",
            "  By lap\n",
            "    implement #1   ≈ $0.03  (in 1 · out 2 · cache read 3 · cache write 4)\n",
            "    implement #?   no data",
            "  By model\n",
            "    mock-model     ≈ $0.09  (in 30",
        ] {
            assert!(block.contains(expected), "missing {expected:?} in {block}");
        }
        assert!(!block.contains("without data)"), "{block}");
    }

    #[test]
    fn an_estimated_total_says_so() {
        let rendered = task_detail(&usage_detail("estimated", 0), test_now());
        assert!(
            rendered.contains("Total        ≈ $0.09 (estimated)"),
            "{rendered}"
        );
    }

    #[test]
    fn turns_without_a_cost_are_counted_on_the_total_line() {
        let mut d = serde_json::json!({"usage": {
            "cost_usd": 0.09, "billing_label": "estimated",
            "tokens": {"input": 1, "output": 1, "cache_read": 1, "cache_write": 1},
            "wall_time_ms": 1000, "active_time_ms": null,
            "sessions_without_data": 0, "turns_without_cost": 2,
            "by_stage": [], "by_role": [], "by_lap": [], "by_model": []}});
        let block = cost_and_time(&d);
        assert!(
            block.contains("(estimated)  (2 turns without a cost)"),
            "{block}"
        );
        d["usage"]["turns_without_cost"] = 1.into();
        assert!(cost_and_time(&d).contains("(1 turn without a cost)"));
        d["usage"]["turns_without_cost"] = 0.into();
        assert!(!cost_and_time(&d).contains("without a cost"));
    }

    #[test]
    fn sessions_without_data_are_counted_on_the_total_line() {
        let one = task_detail(&usage_detail("estimated", 1), test_now());
        assert!(
            one.contains("(estimated)  (1 session without data)"),
            "{one}"
        );
        let two = task_detail(&usage_detail("estimated", 2), test_now());
        assert!(two.contains("(2 sessions without data)"), "{two}");
    }

    #[test]
    fn a_task_without_usage_says_no_data_on_one_line() {
        let mut detail = usage_detail("estimated", 0);
        detail["usage"] = Value::Null;
        let rendered = task_detail(&detail, test_now());
        assert!(rendered.ends_with("\n\nCost & time  no data"), "{rendered}");
    }

    #[test]
    fn a_null_active_time_prints_no_data_and_unknown_cost_says_so() {
        let mut detail = usage_detail("estimated", 0);
        detail["usage"]["active_time_ms"] = Value::Null;
        detail["usage"]["cost_usd"] = Value::Null;
        let rendered = task_detail(&detail, test_now());
        assert!(rendered.contains("Active time  no data"), "{rendered}");
        assert!(rendered.contains("Total        cost unknown"), "{rendered}");
    }
}
