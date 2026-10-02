//! `choco task status --live` / `--until` (#84 part 4): polls
//! `GET /tasks/{id}` and renders or waits on the result. The decisions live
//! in a pure core (`changes`, `line_for`, `reached`, `outcome`, `frame`) that
//! the unit tests drive directly; `watch` is the async loop on top of it.

use std::collections::HashSet;
use std::io::{IsTerminal, Write};
use std::process::ExitCode;

use chrono::{DateTime, Utc};
use serde_json::Value;
use tokio::time::Instant;

use crate::cli::{DurationArg, Until};
use crate::client::{Client, ClientError};
use crate::render;

/// A poll that fails to connect this many times in a row ends the watch.
const MAX_CONSECUTIVE_CONNECT_FAILURES: u32 = 3;

/// One `stage_trail` entry (a `stage_entered` event).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TrailEntry {
    pub id: String,
    pub stage: String,
    pub outcome: Option<String>,
    pub created_at: Option<String>,
}

/// The parts of a `GET /tasks/{id}` body the watch cares about.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Snapshot {
    pub status: String,
    pub stuck_reason: Option<String>,
    pub stage: Option<String>,
    pub trail: Vec<TrailEntry>,
}

impl Snapshot {
    /// Fails (rather than defaulting) on a body that lacks `status` or has a
    /// trail entry without an `id`: a snapshot that silently reads as "no
    /// change" would hide a broken daemon from the watcher.
    pub fn from_detail(detail: &Value) -> Result<Snapshot, String> {
        let status = detail
            .get("status")
            .and_then(Value::as_str)
            .ok_or("task has no string `status`")?
            .to_string();
        let stuck_reason = detail
            .get("stuck_reason")
            .and_then(Value::as_str)
            .map(str::to_string);
        let stage = detail
            .get("workflow_state")
            .and_then(|s| s.get("current_stage"))
            .and_then(Value::as_str)
            .map(str::to_string);
        let mut trail = Vec::new();
        if let Some(entries) = detail.get("stage_trail").and_then(Value::as_array) {
            for entry in entries {
                let id = entry
                    .get("id")
                    .and_then(Value::as_str)
                    .ok_or("a stage_trail entry has no string `id`")?
                    .to_string();
                let payload = entry.get("payload");
                trail.push(TrailEntry {
                    id,
                    stage: payload
                        .and_then(|p| p.get("stage"))
                        .and_then(Value::as_str)
                        .unwrap_or("?")
                        .to_string(),
                    outcome: payload
                        .and_then(|p| p.get("outcome"))
                        .and_then(Value::as_str)
                        .map(str::to_string),
                    created_at: entry
                        .get("created_at")
                        .and_then(Value::as_str)
                        .map(str::to_string),
                });
            }
        }
        Ok(Snapshot {
            status,
            stuck_reason,
            stage,
            trail,
        })
    }
}

/// One observable difference between two polls.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Change {
    /// A stage was entered. `at` is the trail entry's own `created_at` when
    /// it came from the trail.
    Stage {
        from: Option<String>,
        to: String,
        outcome: Option<String>,
        at: Option<DateTime<Utc>>,
    },
    Status {
        from: String,
        to: String,
        reason: Option<String>,
    },
    /// Same status, different `stuck_reason`.
    Reason {
        status: String,
        reason: Option<String>,
    },
}

fn parse_time(raw: &Option<String>) -> Option<DateTime<Utc>> {
    raw.as_deref()
        .and_then(|r| DateTime::parse_from_rfc3339(r).ok())
        .map(|t| t.with_timezone(&Utc))
}

/// What changed from `prev` to `next`: new trail entries in trail order,
/// then a current-stage change the trail doesn't explain, then status /
/// stuck-reason changes. Timestamps and loop counters never count.
pub fn changes(prev: &Snapshot, next: &Snapshot) -> Vec<Change> {
    let seen: HashSet<&str> = prev.trail.iter().map(|e| e.id.as_str()).collect();
    let mut out = Vec::new();
    for (i, entry) in next.trail.iter().enumerate() {
        if seen.contains(entry.id.as_str()) {
            continue;
        }
        out.push(Change::Stage {
            from: i
                .checked_sub(1)
                .and_then(|p| next.trail.get(p))
                .map(|e| e.stage.clone()),
            to: entry.stage.clone(),
            outcome: entry.outcome.clone(),
            at: parse_time(&entry.created_at),
        });
    }
    // The current stage moved but the trail shows nothing new (e.g. pruned
    // by retention): still a change, so report it.
    if prev.stage != next.stage
        && let Some(to) = &next.stage
        && !out
            .iter()
            .any(|c| matches!(c, Change::Stage { to: t, .. } if t == to))
    {
        out.push(Change::Stage {
            from: prev.stage.clone(),
            to: to.clone(),
            outcome: None,
            at: None,
        });
    }
    if prev.status != next.status {
        out.push(Change::Status {
            from: prev.status.clone(),
            to: next.status.clone(),
            reason: next.stuck_reason.clone(),
        });
    } else if prev.stuck_reason != next.stuck_reason {
        out.push(Change::Reason {
            status: next.status.clone(),
            reason: next.stuck_reason.clone(),
        });
    }
    out
}

fn clock(at: DateTime<Utc>) -> String {
    at.format("%H:%M:%S").to_string()
}

/// `HH:MM:SS  text`, with the text collapsed to one line (stage names and
/// stuck reasons are free-form and may contain newlines).
fn stamped(at: DateTime<Utc>, text: &str) -> String {
    format!("{}  {}", clock(at), render::single_line(text))
}

/// One log line for a change. Stage lines carry the entry's own time;
/// status lines the time of the poll (now).
pub fn line_for(change: &Change) -> String {
    match change {
        Change::Stage {
            from,
            to,
            outcome,
            at,
        } => {
            let from = from.as_deref().unwrap_or("…");
            let text = match outcome {
                Some(outcome) => format!("{from} --[{outcome}]--> {to}"),
                None => format!("{from} --> {to}"),
            };
            stamped(at.unwrap_or_else(Utc::now), &text)
        }
        Change::Status { from, to, reason } => {
            let mut text = format!("status {from} -> {to}");
            if let Some(reason) = reason.as_deref().filter(|_| to == "stuck") {
                text.push_str(": ");
                text.push_str(reason);
            }
            stamped(Utc::now(), &text)
        }
        Change::Reason { status, reason } => stamped(
            Utc::now(),
            &format!("{status}: {}", reason.as_deref().unwrap_or("(no reason)")),
        ),
    }
}

/// The first line of a plain-text watch.
pub fn start_line(id: &str, snap: &Snapshot, now: DateTime<Utc>) -> String {
    let stage = snap
        .stage
        .as_deref()
        .map(|s| format!(", stage {s}"))
        .unwrap_or_default();
    stamped(now, &format!("watching task {id}: {}{stage}", snap.status))
}

/// Whether `target` holds. `first` is the first poll: a trail entry for a
/// stage counts only if it wasn't already there then, so a stage the task
/// had left before the watch began doesn't satisfy `stage:<name>`.
pub fn reached(target: &Until, first: &Snapshot, now: &Snapshot) -> bool {
    match target {
        Until::Status(s) => now.status == *s,
        Until::Stage(name) => {
            now.stage.as_deref() == Some(name.as_str())
                || now
                    .trail
                    .iter()
                    .any(|e| e.stage == *name && !first.trail.iter().any(|f| f.id == e.id))
        }
    }
}

/// How a watch ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WatchEnd {
    Reached,
    Stuck,
    Cancelled,
    TimedOut,
    ClosedWithoutTarget,
}

impl WatchEnd {
    pub fn code(self) -> u8 {
        match self {
            WatchEnd::Reached => 0,
            WatchEnd::Stuck => 3,
            WatchEnd::Cancelled => 4,
            WatchEnd::TimedOut => 5,
            WatchEnd::ClosedWithoutTarget => 6,
        }
    }

    pub fn exit_code(self) -> ExitCode {
        ExitCode::from(self.code())
    }
}

/// Whether the task's state ends the watch. Call [`reached`] first for
/// `stage:` targets: this decides the "gave up on it" cases, and treats a
/// status target equal to the status as reached.
pub fn outcome(target: Option<&Until>, now: &Snapshot) -> Option<WatchEnd> {
    if let Some(Until::Status(s)) = target
        && now.status == *s
    {
        return Some(WatchEnd::Reached);
    }
    match (now.status.as_str(), target) {
        ("closed", None) => Some(WatchEnd::Reached),
        ("closed", Some(_)) => Some(WatchEnd::ClosedWithoutTarget),
        ("cancelled", _) => Some(WatchEnd::Cancelled),
        // `--live` alone keeps watching through stuck: a human may retry.
        ("stuck", Some(_)) => Some(WatchEnd::Stuck),
        _ => None,
    }
}

/// The redraw for a terminal: clear, home, the usual status view, footer.
pub fn frame(detail: &Value, interval: &str, now: DateTime<Utc>) -> String {
    format!(
        "\x1b[2J\x1b[H{}\n\nwatching every {interval} · updated {} UTC · Ctrl-C to stop",
        render::task_detail(detail),
        clock(now)
    )
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Mode {
    Json,
    Tty,
    Plain,
}

pub struct WatchArgs<'a> {
    pub id: &'a str,
    pub until: Option<&'a Until>,
    pub interval: &'a DurationArg,
    pub timeout: Option<&'a DurationArg>,
    pub json: bool,
}

fn describe(snap: &Snapshot) -> String {
    match &snap.stage {
        Some(stage) => format!("it is {}, in stage '{stage}'", snap.status),
        None => format!("it is {}", snap.status),
    }
}

fn last_seen(snap: &Snapshot) -> String {
    format!(
        "last seen: {}, stage {}",
        snap.status,
        snap.stage.as_deref().unwrap_or("-")
    )
}

fn end_message(
    end: WatchEnd,
    id: &str,
    snap: &Snapshot,
    target: &str,
    timeout: Option<&DurationArg>,
) -> Option<String> {
    let stage = snap.stage.as_deref().unwrap_or("-");
    let msg = match end {
        WatchEnd::Reached => return None,
        WatchEnd::Stuck => format!(
            "task {id} is stuck in stage '{stage}': {}",
            snap.stuck_reason
                .as_deref()
                .unwrap_or("(no reason recorded)")
        ),
        WatchEnd::Cancelled => format!("task {id} was cancelled (last stage '{stage}')"),
        WatchEnd::ClosedWithoutTarget => {
            format!("task {id} closed without reaching {target} (last stage '{stage}')")
        }
        WatchEnd::TimedOut => format!(
            "timed out after {} waiting for task {id} to reach {target} ({})",
            timeout.map(|t| t.raw.as_str()).unwrap_or("?"),
            describe(snap)
        ),
    };
    Some(render::single_line(&msg))
}

/// Writes one line to stdout. A closed pipe (`| head`) ends the process
/// quietly with the error code; any other write failure is reported the same
/// way rather than dropped.
fn out_line(text: &str) {
    let mut out = std::io::stdout().lock();
    if let Err(e) = writeln!(out, "{text}").and_then(|_| out.flush()) {
        if e.kind() != std::io::ErrorKind::BrokenPipe {
            eprintln!("error: failed writing to stdout: {e}");
        }
        std::process::exit(1);
    }
}

fn emit(mode: Mode, detail: &Value, interval: &str) {
    match mode {
        Mode::Json => {
            out_line(&serde_json::to_string(detail).expect("a JSON value always serializes"))
        }
        Mode::Tty => out_line(&frame(detail, interval, Utc::now())),
        Mode::Plain => {}
    }
}

/// Polls the task until the watch ends. Prints the end reason (if any) to
/// stderr itself; the caller maps the result to an exit code. Errors on the
/// first poll are fatal; afterwards a connection error is retried until
/// three in a row, and any other error is fatal at once.
pub async fn watch(client: &Client, args: WatchArgs<'_>) -> Result<WatchEnd, ClientError> {
    let deadline = args.timeout.map(|t| Instant::now() + t.duration);
    let interval = args.interval.duration;
    let target_name = args
        .until
        .map(|u| u.to_string())
        .unwrap_or_else(|| "closed".to_string());
    let mode = if args.json {
        Mode::Json
    } else if std::io::stdout().is_terminal() {
        Mode::Tty
    } else {
        Mode::Plain
    };
    let decode = |detail: &Value| {
        Snapshot::from_detail(detail)
            .map_err(|e| ClientError::Decode(format!("task {}: {e}", args.id)))
    };

    let detail = client.get_task(args.id).await?;
    let first = decode(&detail)?;
    match mode {
        Mode::Plain => out_line(&start_line(args.id, &first, Utc::now())),
        _ => emit(mode, &detail, &args.interval.raw),
    }
    let mut prev = first.clone();
    let mut strikes = 0u32;

    let check = |snap: &Snapshot| -> Option<WatchEnd> {
        if let Some(target) = args.until
            && reached(target, &first, snap)
        {
            return Some(WatchEnd::Reached);
        }
        outcome(args.until, snap)
    };
    let finish = |end: WatchEnd, snap: &Snapshot| {
        if let Some(msg) = end_message(end, args.id, snap, &target_name, args.timeout) {
            eprintln!("{msg}");
        }
        Ok(end)
    };

    if let Some(end) = check(&first) {
        return finish(end, &first);
    }
    loop {
        let sleep_for = match deadline {
            Some(deadline) => {
                let left = deadline.saturating_duration_since(Instant::now());
                if left.is_zero() {
                    return finish(WatchEnd::TimedOut, &prev);
                }
                interval.min(left)
            }
            None => interval,
        };
        tokio::time::sleep(sleep_for).await;

        // The deadline also bounds a poll that never answers.
        let polled = match deadline {
            Some(deadline) => {
                match tokio::time::timeout_at(deadline, client.get_task(args.id)).await {
                    Ok(result) => result,
                    Err(_) => return finish(WatchEnd::TimedOut, &prev),
                }
            }
            None => client.get_task(args.id).await,
        };
        match polled {
            Ok(detail) => {
                strikes = 0;
                let snap = decode(&detail)?;
                let diff = changes(&prev, &snap);
                match mode {
                    Mode::Plain => diff.iter().for_each(|c| out_line(&line_for(c))),
                    // NDJSON prints only on change; the terminal redraws every poll.
                    Mode::Json if diff.is_empty() => {}
                    _ => emit(mode, &detail, &args.interval.raw),
                }
                prev = snap;
                if let Some(end) = check(&prev) {
                    return finish(end, &prev);
                }
            }
            Err(ClientError::Connect { base_url, source }) => {
                strikes += 1;
                if strikes >= MAX_CONSECUTIVE_CONNECT_FAILURES {
                    return Err(ClientError::LostContact(format!(
                        "lost contact with chocofactoryd at {base_url} while watching task {} ({}): {source}",
                        args.id,
                        last_seen(&prev)
                    )));
                }
            }
            Err(other) => return Err(other),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn entry(id: &str, stage: &str, outcome: Option<&str>, at: &str) -> TrailEntry {
        TrailEntry {
            id: id.into(),
            stage: stage.into(),
            outcome: outcome.map(Into::into),
            created_at: Some(at.into()),
        }
    }

    fn snap(status: &str, stage: Option<&str>, trail: Vec<TrailEntry>) -> Snapshot {
        Snapshot {
            status: status.into(),
            stuck_reason: None,
            stage: stage.map(Into::into),
            trail,
        }
    }

    fn base() -> Snapshot {
        snap(
            "open",
            Some("coding"),
            vec![entry("1", "coding", None, "2026-08-01T14:22:31Z")],
        )
    }

    #[test]
    fn status_change() {
        let mut next = base();
        next.status = "stuck".into();
        next.stuck_reason = Some("boom".into());
        assert_eq!(
            changes(&base(), &next),
            vec![Change::Status {
                from: "open".into(),
                to: "stuck".into(),
                reason: Some("boom".into())
            }]
        );
    }

    #[test]
    fn two_hops_between_polls_give_two_changes_in_order() {
        let mut next = base();
        next.stage = Some("review".into());
        next.trail.push(entry(
            "2",
            "internal_review",
            Some("done"),
            "2026-08-01T14:22:55Z",
        ));
        next.trail
            .push(entry("3", "review", Some("ok"), "2026-08-01T14:22:56Z"));
        let got = changes(&base(), &next);
        assert_eq!(got.len(), 2);
        assert!(
            matches!(&got[0], Change::Stage { from: Some(f), to, outcome: Some(o), .. }
            if f == "coding" && to == "internal_review" && o == "done")
        );
        assert!(matches!(&got[1], Change::Stage { from: Some(f), to, .. }
            if f == "internal_review" && to == "review"));
    }

    #[test]
    fn stage_move_without_trail_entry_is_still_a_change() {
        let mut next = base();
        next.stage = Some("review".into());
        assert_eq!(changes(&base(), &next).len(), 1);
    }

    #[test]
    fn stuck_reason_alone_is_a_change() {
        let mut a = base();
        a.status = "stuck".into();
        a.stuck_reason = Some("a".into());
        let mut b = a.clone();
        b.stuck_reason = Some("b".into());
        assert_eq!(changes(&a, &b).len(), 1);
    }

    #[test]
    fn timestamps_and_counters_are_not_changes() {
        let a = Snapshot::from_detail(&json!({
            "status": "open", "updated_at": "1",
            "workflow_state": {"current_stage": "x", "loop_counters": {"x": 1}},
            "stage_trail": []
        }))
        .unwrap();
        let b = Snapshot::from_detail(&json!({
            "status": "open", "updated_at": "2",
            "workflow_state": {"current_stage": "x", "loop_counters": {"x": 2}},
            "stage_trail": []
        }))
        .unwrap();
        assert!(changes(&a, &b).is_empty());
    }

    #[test]
    fn decode_failures_are_errors() {
        assert!(Snapshot::from_detail(&json!({"title": "x"})).is_err());
        assert!(
            Snapshot::from_detail(&json!({"status": "open", "stage_trail": [{"payload": {}}]}))
                .is_err()
        );
    }

    #[test]
    fn stage_line_uses_the_entrys_time() {
        let c = Change::Stage {
            from: Some("coding".into()),
            to: "internal_review".into(),
            outcome: Some("done".into()),
            at: parse_time(&Some("2026-08-01T14:22:55Z".into())),
        };
        assert_eq!(line_for(&c), "14:22:55  coding --[done]--> internal_review");
    }

    #[test]
    fn outcome_less_and_reason_lines_have_exact_text() {
        let at = "2026-08-01T14:22:55Z".parse().ok();
        let stage = |from: Option<&str>| Change::Stage {
            from: from.map(Into::into),
            to: "b".into(),
            outcome: None,
            at,
        };
        assert_eq!(line_for(&stage(Some("a"))), "14:22:55  a --> b");
        assert_eq!(line_for(&stage(None)), "14:22:55  … --> b");
        let line = line_for(&Change::Reason {
            status: "stuck".into(),
            reason: Some("x".into()),
        });
        assert!(line.ends_with("  stuck: x"), "{line}");
    }

    #[test]
    fn stuck_line_is_one_line() {
        let c = Change::Status {
            from: "open".into(),
            to: "stuck".into(),
            reason: Some("stage 'revising':\nthe agent's\n turn".into()),
        };
        let line = line_for(&c);
        assert!(!line.contains('\n'));
        assert!(
            line.ends_with("status open -> stuck: stage 'revising': the agent's turn"),
            "{line}"
        );
        let closed = Change::Status {
            from: "open".into(),
            to: "closed".into(),
            reason: Some("ignored".into()),
        };
        assert!(line_for(&closed).ends_with("status open -> closed"));
    }

    #[test]
    fn reached_by_status() {
        let s = |st: &str| snap(st, None, vec![]);
        for st in ["closed", "cancelled", "stuck"] {
            let t = Until::Status(st.into());
            assert!(reached(&t, &s("open"), &s(st)));
            assert!(!reached(&t, &s("open"), &s("open")));
        }
    }

    #[test]
    fn reached_by_stage() {
        let t = Until::Stage("review".into());
        let first = base();
        // current stage
        let mut now = base();
        now.stage = Some("review".into());
        assert!(reached(&t, &first, &now));
        // entered and left between polls
        let mut now = base();
        now.stage = Some("finished".into());
        now.trail
            .push(entry("2", "review", Some("done"), "2026-08-01T14:23:00Z"));
        assert!(reached(&t, &first, &now));
        // already in the trail at the first poll, and not current
        let mut first = base();
        first
            .trail
            .push(entry("2", "review", Some("done"), "2026-08-01T14:23:00Z"));
        assert!(!reached(&t, &first, &first.clone()));
    }

    #[test]
    fn outcome_table() {
        let s = |st: &str| snap(st, Some("x"), vec![]);
        let st = |n: &str| Some(Until::Status(n.into()));
        let stage = Some(Until::Stage("x".into()));
        // --live alone
        assert_eq!(outcome(None, &s("closed")), Some(WatchEnd::Reached));
        assert_eq!(outcome(None, &s("cancelled")), Some(WatchEnd::Cancelled));
        assert_eq!(outcome(None, &s("stuck")), None);
        assert_eq!(outcome(None, &s("open")), None);
        // status targets
        let o = |t: Option<Until>, status: &str| outcome(t.as_ref(), &s(status));
        assert_eq!(o(st("closed"), "closed"), Some(WatchEnd::Reached));
        assert_eq!(o(st("stuck"), "stuck"), Some(WatchEnd::Reached));
        assert_eq!(o(st("cancelled"), "cancelled"), Some(WatchEnd::Reached));
        assert_eq!(o(st("closed"), "stuck"), Some(WatchEnd::Stuck));
        assert_eq!(o(st("closed"), "cancelled"), Some(WatchEnd::Cancelled));
        assert_eq!(
            o(st("stuck"), "closed"),
            Some(WatchEnd::ClosedWithoutTarget)
        );
        assert_eq!(
            o(st("cancelled"), "closed"),
            Some(WatchEnd::ClosedWithoutTarget)
        );
        assert_eq!(o(st("closed"), "open"), None);
        // stage target
        assert_eq!(
            o(stage.clone(), "closed"),
            Some(WatchEnd::ClosedWithoutTarget)
        );
        assert_eq!(o(stage.clone(), "stuck"), Some(WatchEnd::Stuck));
        assert_eq!(o(stage.clone(), "cancelled"), Some(WatchEnd::Cancelled));
        assert_eq!(o(stage, "open"), None);
    }

    #[test]
    fn exit_codes_match_the_table() {
        assert_eq!(WatchEnd::Reached.code(), 0);
        assert_eq!(WatchEnd::Stuck.code(), 3);
        assert_eq!(WatchEnd::Cancelled.code(), 4);
        assert_eq!(WatchEnd::TimedOut.code(), 5);
        assert_eq!(WatchEnd::ClosedWithoutTarget.code(), 6);
    }

    #[test]
    fn frame_has_clear_detail_and_footer() {
        let detail = json!({"title": "t", "id": "abc", "status": "open"});
        let now = DateTime::parse_from_rfc3339("2026-08-01T14:22:55Z")
            .unwrap()
            .with_timezone(&Utc);
        let f = frame(&detail, "2s", now);
        assert!(f.starts_with("\x1b[2J\x1b[H"));
        assert!(f.contains(&render::task_detail(&detail)));
        assert!(f.ends_with("watching every 2s · updated 14:22:55 UTC · Ctrl-C to stop"));
    }
}
