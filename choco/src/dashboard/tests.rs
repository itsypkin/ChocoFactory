//! Dashboard tests (#164). No real terminal: `update` is driven directly,
//! frames go to ratatui's `TestBackend`, and the end-to-end test runs the
//! real loop against an in-process fake daemon.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::Router;
use axum::extract::ws::{Message, WebSocketUpgrade};
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::routing::{get, post};
use chocofactory_core::models::{
    Project, PullRequestRef, RetryOutcome, Task, TaskSummary, UsageTotal,
};
use chrono::{DateTime, TimeZone, Utc};
use ratatui::Terminal;
use ratatui::backend::TestBackend;
use ratatui::buffer::Buffer;
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use serde_json::{Value, json};
use tokio::sync::mpsc;

use super::app::*;
use super::view::draw;
use super::{Handover, LoopConfig, PanicSignal, run_loop};
use crate::client::Client;

fn now() -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 1, 1, 12, 0, 0).unwrap()
}

fn mins_ago(m: i64) -> DateTime<Utc> {
    now() - chrono::Duration::minutes(m)
}

fn summary(
    id: &str,
    title: &str,
    status: &str,
    stage: Option<&str>,
    entered_mins_ago: Option<i64>,
    project_id: &str,
) -> TaskSummary {
    TaskSummary {
        queued: Vec::new(),
        task: Task {
            id: id.to_string(),
            project_id: project_id.to_string(),
            workflow_def: "coding-task".to_string(),
            title: title.to_string(),
            status: status.to_string(),
            config: json!({}),
            worktree_repo: None,
            worktree_project: None,
            stuck_reason: None,
            paused_at: None,
            kept_work: false,
            workflow_path: None,
            workflow_sha256: None,
            base_ref: None,
            base_commit: None,
            closes: vec![],
            created_at: mins_ago(500),
            updated_at: mins_ago(60),
        },
        current_stage: stage.map(str::to_string),
        stage_entered_at: entered_mins_ago.map(mins_ago),
        loop_counters: json!({}),
        pr: None,
        waiting_on_human: false,
        usage_total: None,
        branch_progress: None,
    }
}

fn waiting(mut t: TaskSummary) -> TaskSummary {
    t.waiting_on_human = true;
    t
}

fn with_pr(mut t: TaskSummary, n: u64) -> TaskSummary {
    t.pr = Some(PullRequestRef {
        number: n,
        url: format!("https://github.com/o/r/pull/{n}"),
    });
    t
}

fn stuck(mut t: TaskSummary, reason: &str, updated_mins_ago: i64) -> TaskSummary {
    t.task.stuck_reason = Some(reason.to_string());
    t.task.updated_at = mins_ago(updated_mins_ago);
    t
}

fn new_app(scope: Scope) -> App {
    let mut app = App::new(
        scope,
        "127.0.0.1:4141".to_string(),
        Duration::from_secs(2),
        "2s".to_string(),
        now(),
    );
    app.color = false;
    app.ssh = false;
    app.editor = None;
    app.daemon_version = Some(chocofactory_core::version::VERSION.to_string());
    app
}

fn list_ok(active: Vec<TaskSummary>, closed: Vec<TaskSummary>) -> Msg {
    Msg::List(Box::new(ListResult {
        server: None,
        at: now(),
        active: Ok(active),
        closed: Ok(closed),
        projects: None,
    }))
}

fn load(app: &mut App, active: Vec<TaskSummary>, closed: Vec<TaskSummary>) {
    update(app, list_ok(active, closed));
}

fn press(app: &mut App, code: KeyCode) -> Vec<Effect> {
    update(app, Msg::Key(KeyEvent::new(code, KeyModifiers::NONE)))
}

fn ch(app: &mut App, c: char) -> Vec<Effect> {
    press(app, KeyCode::Char(c))
}

fn ids(v: &[&TaskSummary]) -> Vec<String> {
    v.iter().map(|t| t.task.id.clone()).collect()
}

fn screen(buf: &Buffer) -> String {
    let mut out = String::new();
    for y in 0..buf.area.height {
        for x in 0..buf.area.width {
            out.push_str(buf[(x, y)].symbol());
        }
        out.push('\n');
    }
    out
}

fn render(app: &App, w: u16, h: u16) -> String {
    let mut terminal = Terminal::new(TestBackend::new(w, h)).unwrap();
    terminal.draw(|f| draw(f, app)).unwrap();
    screen(terminal.backend().buffer())
}

fn board() -> App {
    let mut app = new_app(Scope::AllProjects);
    app.projects.insert("p-alpha".into(), "chocofactory".into());
    app.projects.insert("p-beta".into(), "webshop".into());
    let mut review = with_pr(
        waiting(summary(
            "3f2a91c0-aaaa",
            "Interactive terminal dashboard (#164)",
            "open",
            Some("awaiting_human_review"),
            Some(125),
            "p-alpha",
        )),
        171,
    );
    review.loop_counters = json!({});
    let mut busy = summary(
        "9c03aa17-bbbb",
        "Per-kind stage execution (#55)",
        "open",
        Some("internal_review"),
        Some(12),
        "p-alpha",
    );
    busy.loop_counters = json!({"internal_review": {"count": 2}});
    let waiting = waiting(summary(
        "b81e0d44-cccc",
        "Checkout totals rounding (#42)",
        "open",
        Some("escalate_to_human"),
        Some(14),
        "p-beta",
    ));
    let stuck_task = stuck(
        summary(
            "7d22e1a8-dddd",
            "Flaky lock test (#98)",
            "stuck",
            Some("revising"),
            Some(41),
            "p-alpha",
        ),
        "interrupted by a usage limit\nsecond line",
        41,
    );
    let mut done = with_pr(
        summary(
            "61b0f2e3-eeee",
            "Release 0.2.2",
            "closed",
            None,
            None,
            "p-alpha",
        ),
        163,
    );
    done.task.updated_at = mins_ago(30 * 60);
    load(
        &mut app,
        vec![review, busy, waiting, stuck_task],
        vec![done],
    );
    app
}

// ---- grouping and ordering ----------------------------------------------

#[test]
fn sections_group_and_order_tasks() {
    let mut app = new_app(Scope::AllProjects);
    let active = vec![
        waiting(summary(
            "a",
            "a",
            "open",
            Some("awaiting_human_review"),
            Some(10),
            "p",
        )),
        waiting(summary(
            "b",
            "b",
            "open",
            Some("my_custom_gate"),
            Some(120),
            "p",
        )),
        // An open task at the old gate name that the daemon does not
        // report as waiting is In progress.
        summary(
            "i",
            "i",
            "open",
            Some("awaiting_human_review"),
            Some(2),
            "p",
        ),
        stuck(
            waiting(summary(
                "c",
                "c",
                "stuck",
                Some("awaiting_human_review"),
                Some(1),
                "p",
            )),
            "boom",
            5,
        ),
        summary("d", "d", "open", Some("coding"), Some(3), "p"),
        summary("e", "e", "open", Some("coding"), Some(30), "p"),
        stuck(
            summary("f", "f", "stuck", Some("coding"), Some(1), "p"),
            "x",
            60,
        ),
        waiting(summary(
            "g",
            "g",
            "open",
            Some("escalate_to_human"),
            None,
            "p",
        )),
        summary("h", "h", "open", None, None, "p"),
    ];
    let closed = vec![
        summary("z1", "z1", "closed", None, None, "p"),
        summary("z2", "z2", "cancelled", None, None, "p"),
    ];
    load(&mut app, active, closed);
    let [needs, progress, _paused, stuck_s, closed_s] = app.sections();
    // Oldest first; an unknown entry time last.
    assert_eq!(ids(&needs), ["b", "a", "g"]);
    // A stuck task at awaiting_human_review is Stuck, not Needs you; an
    // open task at any other stage (or none) is In progress.
    assert_eq!(ids(&progress), ["e", "d", "i", "h"]);
    // Longest stuck first.
    assert_eq!(ids(&stuck_s), ["f", "c"]);
    assert_eq!(ids(&closed_s), ["z1", "z2"]);
}

#[test]
fn a_task_at_the_planned_workflows_question_gate_needs_you() {
    let mut app = new_app(Scope::AllProjects);
    let active = vec![
        waiting(summary(
            "q",
            "q",
            "open",
            Some("spec_questions"),
            Some(5),
            "p",
        )),
        summary("w", "w", "open", Some("coding"), Some(5), "p"),
    ];
    load(&mut app, active, vec![]);
    let [needs, progress, _, _, _] = app.sections();
    assert_eq!(ids(&needs), ["q"]);
    assert_eq!(ids(&progress), ["w"]);
}

// ---- keys ---------------------------------------------------------------

#[test]
fn movement_crosses_sections_and_clamps() {
    let mut app = board();
    // Needs you (2) -> In progress (1) -> Stuck (1) -> Closed (1)
    assert_eq!(app.selected.as_deref(), Some("3f2a91c0-aaaa"));
    ch(&mut app, 'k');
    assert_eq!(app.selected_pos, 0, "clamped at the top");
    for _ in 0..2 {
        ch(&mut app, 'j');
    }
    assert_eq!(
        app.selected.as_deref(),
        Some("9c03aa17-bbbb"),
        "crossed a section"
    );
    for _ in 0..10 {
        press(&mut app, KeyCode::Down);
    }
    assert_eq!(
        app.selected.as_deref(),
        Some("61b0f2e3-eeee"),
        "clamped at the end"
    );
    ch(&mut app, 'g');
    assert_eq!(app.selected_pos, 0);
    ch(&mut app, 'G');
    assert_eq!(app.selected_pos, 4);
    press(&mut app, KeyCode::Home);
    press(&mut app, KeyCode::Tab);
    assert_eq!(app.selected.as_deref(), Some("9c03aa17-bbbb"));
    press(&mut app, KeyCode::Tab);
    assert_eq!(app.selected.as_deref(), Some("7d22e1a8-dddd"));
    press(&mut app, KeyCode::BackTab);
    assert_eq!(app.selected.as_deref(), Some("9c03aa17-bbbb"));
    press(&mut app, KeyCode::PageDown);
    assert_eq!(app.selected_pos, 4);
    press(&mut app, KeyCode::PageUp);
    assert_eq!(app.selected_pos, 0);
}

#[test]
fn selection_follows_the_task_id_across_a_resort() {
    let mut app = board();
    ch(&mut app, 'G');
    press(&mut app, KeyCode::Up);
    press(&mut app, KeyCode::Up);
    assert_eq!(app.selected.as_deref(), Some("9c03aa17-bbbb"));
    // It becomes stuck and moves into another section, below the others.
    let mut active = app.active.clone();
    let t = active
        .iter_mut()
        .find(|t| t.task.id == "9c03aa17-bbbb")
        .unwrap();
    t.task.status = "stuck".into();
    t.task.updated_at = mins_ago(500);
    let closed = app.closed.clone();
    load(&mut app, active, closed);
    assert_eq!(app.selected.as_deref(), Some("9c03aa17-bbbb"));
    assert_eq!(app.flat_ids()[app.selected_pos], "9c03aa17-bbbb");

    // A task that is gone: the selection moves to the row now at its place.
    let pos = app.selected_pos;
    let active: Vec<_> = app
        .active
        .iter()
        .filter(|t| t.task.id != "9c03aa17-bbbb")
        .cloned()
        .collect();
    let closed = app.closed.clone();
    load(&mut app, active, closed);
    assert_eq!(
        app.selected.as_deref(),
        Some(app.flat_ids()[pos.min(3)].as_str())
    );
}

#[test]
fn cancel_prompt_only_y_acts_and_uses_the_captured_id() {
    let mut app = board();
    let first = app.selected.clone().unwrap();
    // `n` and Esc send nothing.
    assert!(ch(&mut app, 'c').is_empty());
    assert!(app.prompt.is_some());
    assert!(ch(&mut app, 'n').is_empty());
    assert!(app.prompt.is_none());
    ch(&mut app, 'c');
    assert!(press(&mut app, KeyCode::Esc).is_empty());
    assert!(app.prompt.is_none());

    // `y` cancels the task the prompt was opened on, even though a refresh
    // put another task under the cursor's row in the meantime.
    ch(&mut app, 'c');
    let mut active = app.active.clone();
    active.retain(|t| t.task.id != first);
    active.push(waiting(summary(
        "0000-new",
        "New",
        "open",
        Some("awaiting_human_review"),
        Some(9999),
        "p-alpha",
    )));
    let closed = app.closed.clone();
    load(&mut app, active, closed);
    assert_ne!(app.flat_ids()[0], first);
    let effects = ch(&mut app, 'y');
    assert_eq!(effects, vec![Effect::Cancel(first)]);
    assert!(app.prompt.is_none());
}

#[test]
fn retry_only_applies_to_a_stuck_task() {
    let mut app = board();
    assert!(ch(&mut app, 'r').is_empty());
    assert_eq!(
        app.status.as_ref().unwrap().text,
        "retry: task is open, not stuck"
    );
    assert!(app.prompt.is_none());

    ch(&mut app, 'G');
    press(&mut app, KeyCode::Up); // the stuck task
    assert_eq!(app.selected.as_deref(), Some("7d22e1a8-dddd"));
    assert!(ch(&mut app, 'r').is_empty());
    assert_eq!(app.prompt.as_ref().unwrap().kind, PromptKind::Retry);
    assert_eq!(
        ch(&mut app, 'y'),
        vec![Effect::Retry("7d22e1a8-dddd".into())]
    );

    let effects = update(
        &mut app,
        Msg::Action {
            kind: ActionKind::Retry,
            id: "7d22e1a8-dddd".into(),
            result: Ok(ActionOk::Retried(RetryOutcome {
                stage: "revising".into(),
                resumed: true,
                adapter_session_id: None,
                fresh_reason: None,
                rewatched: false,
                branches: Vec::new(),
                queued: None,
            })),
        },
    );
    assert_eq!(app.status.as_ref().unwrap().text, "retried: resumed");
    assert_eq!(effects, vec![Effect::Fetch { projects: false }]);
}

#[test]
fn a_failed_action_shows_the_daemons_error_and_does_not_refresh() {
    let mut app = board();
    let effects = update(
        &mut app,
        Msg::Action {
            kind: ActionKind::Retry,
            id: "x".into(),
            result: Err("task is 'open', not 'stuck', so it cannot be retried".into()),
        },
    );
    assert!(effects.is_empty());
    let s = app.status.as_ref().unwrap();
    assert_eq!(s.level, Level::Error);
    assert_eq!(
        s.text,
        "retry failed: task is 'open', not 'stuck', so it cannot be retried"
    );
}

#[test]
fn open_pr_without_a_pr_over_ssh_and_normally() {
    let mut app = board();
    ch(&mut app, 'j'); // b81e…, no PR
    assert!(ch(&mut app, 'o').is_empty());
    assert_eq!(app.status.as_ref().unwrap().text, "no PR yet");

    ch(&mut app, 'k'); // 3f2a… has PR #171
    app.ssh = true;
    assert!(ch(&mut app, 'o').is_empty());
    assert_eq!(
        app.status.as_ref().unwrap().text,
        "PR #171: https://github.com/o/r/pull/171"
    );
    app.ssh = false;
    assert_eq!(
        ch(&mut app, 'o'),
        vec![Effect::OpenUrl("https://github.com/o/r/pull/171".into())]
    );
}

#[test]
fn quit_keys() {
    let mut app = board();
    assert_eq!(ch(&mut app, 'q'), vec![Effect::Quit]);
    let ctrl_c = Msg::Key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL));
    assert_eq!(update(&mut app, ctrl_c), vec![Effect::Quit]);
}

#[test]
fn polling_cadence_and_project_refetch() {
    let mut app = new_app(Scope::AllProjects);
    assert_eq!(
        update(&mut app, Msg::Tick(now())),
        vec![Effect::Fetch { projects: false }]
    );
    assert!(
        update(&mut app, Msg::Tick(now())).is_empty(),
        "one poll in flight"
    );
    // A row names a project missing from the map: the next poll re-fetches.
    load(
        &mut app,
        vec![summary("a", "a", "open", Some("coding"), Some(1), "p-new")],
        vec![],
    );
    assert!(update(&mut app, Msg::Tick(now() + chrono::Duration::seconds(1))).is_empty());
    assert_eq!(
        update(&mut app, Msg::Tick(now() + chrono::Duration::seconds(2))),
        vec![Effect::Fetch { projects: true }]
    );
}

// ---- drawing ------------------------------------------------------------

/// Gives the board's task `id` a recorded cost.
fn give_usage(app: &mut App, id: &str, cost: f64, label: &str) {
    let t = app
        .active
        .iter_mut()
        .chain(app.closed.iter_mut())
        .find(|t| t.task.id == id)
        .expect("task on the board");
    t.usage_total = Some(UsageTotal {
        cost_usd: Some(cost),
        tokens: Some(405),
        billing_label: label.to_string(),
        turns_without_cost: 0,
    });
}

#[test]
fn wide_screen_shows_headers_counts_pr_durations_and_laps() {
    let mut app = board();
    give_usage(&mut app, "9c03aa17-bbbb", 1.234, "api_equivalent");
    let s = render(&app, 120, 30);
    for want in [
        "NEEDS YOU (2)",
        "IN PROGRESS (1)",
        "STUCK (1)",
        "RECENTLY CLOSED (1)",
        "#171",
        "2h05m",
        "×2",
        "all projects",
        "refreshed 0s ago",
        "interrupted by a usage limit",
        "1d06h ago",
        "cost",
        "≈$1.23",
        "no data",
    ] {
        assert!(s.contains(want), "missing {want:?} in:\n{s}");
    }
    assert!(
        !s.contains("second line"),
        "only the first line of the reason"
    );
    assert!(s.contains("▶"));
}

#[test]
fn medium_screen_drops_pr_and_laps() {
    let app = board();
    let s = render(&app, 70, 20);
    assert!(!s.contains("cost"), "{s}");
    assert!(!s.contains("#171"), "{s}");
    assert!(!s.contains("×2"), "{s}");
    // One grid: project and stage shrink (cut with `…`) before titles do,
    // and every section's rows share the same columns.
    assert!(s.contains("project"), "{s}");
    assert!(s.contains("awaiting_hum"), "{s}");
    assert!(s.contains("chocof…"), "{s}");
    assert!(s.contains("Interactive termina"), "{s}");
    let col = |needle: &str| {
        s.lines()
            .filter_map(|l| l.find(needle).map(|i| l[..i].chars().count()))
            .collect::<Vec<_>>()
    };
    let title_cols: Vec<usize> = ["Interactive", "Checkout", "Per-kind", "Flaky", "Release"]
        .iter()
        .flat_map(|t| col(t))
        .collect();
    assert_eq!(title_cols.len(), 5, "{s}");
    assert!(title_cols.windows(2).all(|w| w[0] == w[1]), "{s}");
}

#[test]
fn narrow_screen_drops_stage_and_project() {
    let mut app = board();
    give_usage(&mut app, "9c03aa17-bbbb", 1.234, "api_equivalent");
    let s = render(&app, 55, 15);
    assert!(!s.contains("cost") && !s.contains("≈$"), "{s}");
    assert!(!s.contains("awaiting_human_review"), "{s}");
    assert!(!s.contains("chocofactory  "), "{s}");
    assert!(!s.contains("webshop"), "{s}");
    assert!(s.contains("3f2a91c0"), "{s}");
}

#[test]
fn tiny_screen_says_it_is_too_small() {
    let app = board();
    assert!(render(&app, 30, 8).contains("terminal too small"));
    assert!(render(&app, 120, 9).contains("terminal too small"));
    assert!(render(&app, 40, 10).contains("NEEDS YOU"));
}

#[test]
fn a_failed_poll_keeps_the_rows_and_shows_the_red_line() {
    let mut app = board();
    update(&mut app, Msg::Tick(now() + chrono::Duration::seconds(34)));
    update(
        &mut app,
        Msg::List(Box::new(ListResult {
            server: None,
            at: now() + chrono::Duration::seconds(34),
            active: Err("connection refused".into()),
            closed: Err("connection refused".into()),
            projects: None,
        })),
    );
    assert_eq!(app.active.len(), 4, "the data stays");
    let s = render(&app, 120, 30);
    assert!(s.contains("Interactive terminal dashboard"), "{s}");
    assert!(
        s.contains("daemon unreachable: connection refused; retrying every 2s (data 34s old)"),
        "{s}"
    );
    // And the next good poll clears it.
    let (a, c) = (app.active.clone(), app.closed.clone());
    load(&mut app, a, c);
    assert!(!render(&app, 120, 30).contains("daemon unreachable"));
}

#[test]
fn all_projects_mode_shows_a_project_column() {
    let app = board();
    let s = render(&app, 120, 30);
    assert!(s.contains("choco dashboard · all projects"), "{s}");
    assert!(s.contains("chocofactory"), "{s}");
    assert!(s.contains("webshop"), "{s}");
}

#[test]
fn an_unknown_project_shows_its_short_id_and_is_refetched() {
    let mut app = new_app(Scope::AllProjects);
    load(
        &mut app,
        vec![summary(
            "a",
            "Some task",
            "open",
            Some("coding"),
            Some(1),
            "deadbeef-1234-5678",
        )],
        vec![],
    );
    let s = render(&app, 120, 30);
    assert!(s.contains("deadbeef"), "{s}");
    assert!(!s.contains("deadbeef-1234"), "{s}");
    let effects = update(&mut app, Msg::Tick(now() + chrono::Duration::seconds(5)));
    assert_eq!(effects, vec![Effect::Fetch { projects: true }]);
    // Names arrive with the next poll.
    let active = app.active.clone();
    update(
        &mut app,
        Msg::List(Box::new(ListResult {
            server: None,
            at: now() + chrono::Duration::seconds(5),
            active: Ok(active),
            closed: Ok(vec![]),
            projects: Some(Ok(vec![Project {
                id: "deadbeef-1234-5678".into(),
                name: "fresh".into(),
                repo_path: None,
                created_at: now(),
                max_agents: None,
            }])),
        })),
    );
    assert!(render(&app, 120, 30).contains("fresh"));
}

#[test]
fn a_failed_projects_fetch_goes_to_the_status_line_and_keeps_the_board() {
    let mut app = board();
    let (active, closed) = (app.active.clone(), app.closed.clone());
    update(
        &mut app,
        Msg::List(Box::new(ListResult {
            server: None,
            at: now(),
            active: Ok(active),
            closed: Ok(closed),
            projects: Some(Err("nope".into())),
        })),
    );
    let s = render(&app, 120, 30);
    assert!(s.contains("could not fetch projects: nope"), "{s}");
    assert!(s.contains("Flaky lock test"), "{s}");
    assert_eq!(app.projects.len(), 2, "names are kept");
}

#[test]
fn long_project_names_are_cut_to_14_characters() {
    let mut app = new_app(Scope::AllProjects);
    app.projects
        .insert("p".into(), "averyveryverylongprojectname".into());
    load(
        &mut app,
        vec![summary("a", "Task", "open", Some("coding"), Some(1), "p")],
        vec![],
    );
    let s = render(&app, 120, 30);
    assert!(s.contains("averyveryvery…"), "{s}");
    assert!(!s.contains("averyveryveryl"), "{s}");
}

#[test]
fn one_project_mode_has_no_project_column() {
    let mut app = new_app(Scope::Project {
        name: "chocofactory".into(),
    });
    load(
        &mut app,
        vec![summary(
            "a",
            "Task",
            "open",
            Some("coding"),
            Some(1),
            "p-alpha",
        )],
        vec![],
    );
    let s = render(&app, 120, 30);
    assert!(s.contains("choco dashboard · project chocofactory"), "{s}");
    assert_eq!(
        s.matches("chocofactory").count(),
        1,
        "only in the header:\n{s}"
    );
    assert!(!s.contains("all projects"));
}

#[test]
fn header_drops_parts_as_it_runs_out_of_room() {
    let mut app = board();
    app.daemon_version = Some("0.0.1".into());
    let wide = render(&app, 120, 12);
    assert!(wide.contains("127.0.0.1:4141"));
    assert!(
        wide.contains("daemon 0.0.1 (choco"),
        "the skew is marked:\n{wide}"
    );
    let first = |s: &str| s.lines().next().unwrap().to_string();
    let no_url = first(&render(&app, 90, 12));
    assert!(
        !no_url.contains("127.0.0.1") && no_url.contains("daemon 0.0.1"),
        "{no_url}"
    );
    let no_ver = first(&render(&app, 66, 12));
    assert!(
        !no_ver.contains("daemon") && no_ver.contains("? help"),
        "{no_ver}"
    );
    let no_help = first(&render(&app, 44, 12));
    assert!(
        !no_help.contains("? help") && no_help.contains("refreshed"),
        "{no_help}"
    );
}

#[test]
fn selection_stays_visible_when_the_list_scrolls() {
    let mut app = new_app(Scope::AllProjects);
    let active: Vec<_> = (0..40)
        .map(|i| {
            summary(
                &format!("id{i:02}"),
                &format!("Task number {i:02}"),
                "open",
                Some("coding"),
                Some(i + 1),
                "p",
            )
        })
        .collect();
    load(&mut app, active, vec![]);
    ch(&mut app, 'G');
    let s = render(&app, 80, 12);
    assert!(s.contains("▶ id"), "{s}");
    let marked = s.lines().find(|l| l.contains('▶')).unwrap();
    assert!(marked.contains("id00"), "the newest entry sorts last:\n{s}");
    assert_eq!(app.selected_pos, 39);
}

#[test]
fn overlays_draw() {
    let mut app = board();
    ch(&mut app, 'c');
    let s = render(&app, 120, 30);
    assert!(
        s.contains("Cancel \"Interactive terminal dashboard (#164)\"?"),
        "{s}"
    );
    assert!(s.contains("cannot be undone."), "{s}");
    ch(&mut app, 'n');
    ch(&mut app, '?');
    assert!(render(&app, 120, 30).contains("this help"));
}

#[test]
fn detail_events_follow_until_scrolled() {
    let mut app = board();
    press(&mut app, KeyCode::Enter);
    ch(&mut app, 'e');
    let id = app.selected.clone().unwrap();
    let ev = |n: usize| Msg::Socket {
        id: id.clone(),
        msg: SocketMsg::Event(Box::new(chocofactory_core::models::Event {
            id: format!("e{n}"),
            task_id: id.clone(),
            session_id: None,
            event_type: chocofactory_core::models::EventType::HumanMessage,
            payload: json!({"text": format!("message {n}")}),
            created_at: now(),
        })),
    };
    update(
        &mut app,
        Msg::Socket {
            id: id.clone(),
            msg: SocketMsg::Connected,
        },
    );
    for n in 0..60 {
        update(&mut app, ev(n));
    }
    let s = render(&app, 80, 20);
    assert!(
        s.contains("message 59") && !s.contains("message 0\n"),
        "{s}"
    );
    assert!(s.contains("events (following)"));
    press(&mut app, KeyCode::PageUp);
    update(&mut app, ev(60));
    let s = render(&app, 80, 20);
    assert!(!s.contains("message 60"), "scrolled view stays put:\n{s}");
    assert!(s.contains("End follows"));
    press(&mut app, KeyCode::End);
    assert!(render(&app, 80, 20).contains("message 60"));
    // A reconnect replaces the buffer with the new backlog.
    update(
        &mut app,
        Msg::Socket {
            id: id.clone(),
            msg: SocketMsg::Connected,
        },
    );
    update(&mut app, ev(100));
    let View::Detail(d) = &app.view else { panic!() };
    assert_eq!(d.events.len(), 1);
}

// ---- end to end against a fake daemon -----------------------------------

/// A handover that does nothing (the tests have no real terminal).
struct NoHandover;

impl Handover for NoHandover {
    fn suspend(&mut self) -> std::io::Result<()> {
        Ok(())
    }
    fn resume(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

#[derive(Clone, Default)]
struct Fake {
    tasks: Arc<Vec<TaskSummary>>,
    cancels: Arc<Mutex<Vec<String>>>,
    /// The `keep` field of each cancel request body, in order.
    keeps: Arc<Mutex<Vec<Value>>>,
    /// When set, the detail, cancel and retry endpoints never answer.
    hang: bool,
    /// The query string of each `GET /tasks`, in order.
    queries: Arc<Mutex<Vec<HashMap<String, String>>>>,
    /// Each `POST /tasks/{id}/messages` as `(id, text)`.
    messages: Arc<Mutex<Vec<(String, String)>>>,
    /// How that route answers.
    reply: Reply,
    /// `"list"` for each `GET /tasks`, `"post"` for each message, in order.
    events: Arc<Mutex<Vec<&'static str>>>,
}

#[derive(Clone, Default)]
enum Reply {
    #[default]
    Accept,
    Refuse(String),
    Hang,
}

async fn fake_server(state: Fake) -> String {
    async fn server() -> axum::Json<Value> {
        axum::Json(json!({
            "version": chocofactory_core::version::VERSION, "commit": null, "pid": 1,
            "port": 1, "started_at": "2026-01-01T00:00:00Z", "config_root": "/x",
            "exe": "/x", "exe_replaced": false, "choco_binary": "choco",
            "choco_binary_found": true, "tasks": {}, "in_flight": []
        }))
    }
    async fn projects() -> axum::Json<Value> {
        axum::Json(json!([
            {"id": "p-alpha", "name": "alpha-project", "repo_path": null, "created_at": "2026-01-01T00:00:00Z"},
            {"id": "p-beta", "name": "beta-project", "repo_path": null, "created_at": "2026-01-01T00:00:00Z"},
        ]))
    }
    async fn tasks(
        State(s): State<Fake>,
        Query(q): Query<HashMap<String, String>>,
    ) -> axum::Json<Value> {
        s.queries.lock().unwrap().push(q.clone());
        s.events.lock().unwrap().push("list");
        let statuses: Vec<&str> = q
            .get("status")
            .map(|s| s.split(',').collect())
            .unwrap_or_default();
        let mut rows: Vec<&TaskSummary> = s
            .tasks
            .iter()
            .filter(|t| statuses.is_empty() || statuses.contains(&t.task.status.as_str()))
            .filter(|t| q.get("project_id").is_none_or(|p| &t.task.project_id == p))
            .collect();
        if q.get("order").is_some_and(|o| o == "updated_desc") {
            rows.sort_by_key(|a| std::cmp::Reverse(a.task.updated_at));
        }
        if let Some(limit) = q.get("limit").and_then(|l| l.parse::<usize>().ok()) {
            rows.truncate(limit);
        }
        axum::Json(serde_json::to_value(rows).unwrap())
    }
    async fn task(State(s): State<Fake>, Path(id): Path<String>) -> axum::Json<Value> {
        if s.hang {
            std::future::pending::<()>().await;
        }
        let t = s.tasks.iter().find(|t| t.task.id == id).unwrap();
        let mut v = serde_json::to_value(t).unwrap();
        v["workflow_state"] = json!({"current_stage": t.current_stage, "loop_counters": {}});
        v["stage_trail"] = json!([{
            "created_at": "2026-01-01T09:00:00Z",
            "payload": {"stage": "coding", "outcome": null}
        }]);
        axum::Json(v)
    }
    async fn retry(State(s): State<Fake>) -> StatusCode {
        if s.hang {
            std::future::pending::<()>().await;
        }
        StatusCode::OK
    }
    async fn cancel(
        State(s): State<Fake>,
        Path(id): Path<String>,
        axum::Json(body): axum::Json<Value>,
    ) -> StatusCode {
        if s.hang {
            std::future::pending::<()>().await;
        }
        s.keeps.lock().unwrap().push(body["keep"].clone());
        s.cancels.lock().unwrap().push(id);
        StatusCode::ACCEPTED
    }
    async fn message(
        State(s): State<Fake>,
        Path(id): Path<String>,
        axum::Json(body): axum::Json<Value>,
    ) -> axum::response::Response {
        use axum::response::IntoResponse;
        s.messages
            .lock()
            .unwrap()
            .push((id, body["text"].as_str().unwrap_or_default().to_string()));
        s.events.lock().unwrap().push("post");
        match &s.reply {
            Reply::Accept => StatusCode::ACCEPTED.into_response(),
            Reply::Refuse(text) => {
                (StatusCode::CONFLICT, axum::Json(json!({ "error": text }))).into_response()
            }
            Reply::Hang => std::future::pending().await,
        }
    }
    async fn live(Path(id): Path<String>, ws: WebSocketUpgrade) -> axum::response::Response {
        ws.on_upgrade(move |mut socket| async move {
            for (n, text) in ["hello streamed", "second streamed"].iter().enumerate() {
                let event = json!({
                    "id": format!("ev{n}"), "task_id": id, "session_id": null,
                    "event_type": "human_message", "payload": {"text": text},
                    "created_at": "2026-01-01T10:00:00Z"
                });
                if socket
                    .send(Message::Text(event.to_string().into()))
                    .await
                    .is_err()
                {
                    return;
                }
            }
            while socket.recv().await.is_some() {}
        })
    }
    let app = Router::new()
        .route("/server", get(server))
        .route("/projects", get(projects))
        .route("/tasks", get(tasks))
        .route("/tasks/{id}", get(task))
        .route("/tasks/{id}/cancel", post(cancel))
        .route("/tasks/{id}/retry", post(retry))
        .route("/tasks/{id}/messages", post(message))
        .route("/tasks/{id}/events/live", get(live))
        .with_state(state);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    format!("http://{addr}")
}

async fn wait_for(screen: &Arc<Mutex<String>>, what: &str) {
    for _ in 0..300 {
        if screen.lock().unwrap().contains(what) {
            return;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!(
        "never saw {what:?}; last screen:\n{}",
        screen.lock().unwrap()
    );
}

/// Whether some screen line's whitespace-split tokens contain `tokens`
/// consecutively (the progress table pads its columns, so exact spacing is
/// not worth asserting in a loose check).
fn row_has(screen: &str, tokens: &[&str]) -> bool {
    screen.lines().any(|l| {
        let t: Vec<&str> = l.split_whitespace().collect();
        t.windows(tokens.len()).any(|w| w == tokens)
    })
}

async fn wait_for_row(screen: &Arc<Mutex<String>>, tokens: &[&str]) {
    for _ in 0..300 {
        if row_has(&screen.lock().unwrap(), tokens) {
            return;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!(
        "never saw a row {tokens:?}; last screen:\n{}",
        screen.lock().unwrap()
    );
}

#[tokio::test]
async fn the_real_loop_against_a_fake_daemon() {
    let tasks = vec![
        summary(
            "aaaa1111-0",
            "Fix the login",
            "open",
            Some("awaiting_human_review"),
            Some(125),
            "p-alpha",
        ),
        summary(
            "bbbb2222-0",
            "Speed up checkout",
            "open",
            Some("coding"),
            Some(3),
            "p-beta",
        ),
        summary("cccc3333-0", "Old release", "closed", None, None, "p-alpha"),
    ];
    let fake = Fake {
        tasks: Arc::new(tasks),
        cancels: Arc::new(Mutex::new(Vec::new())),
        keeps: Arc::new(Mutex::new(Vec::new())),
        hang: false,
        queries: Arc::default(),
        ..Default::default()
    };
    let base_url = fake_server(fake.clone()).await;
    let client = Arc::new(Client::new(base_url.clone()).without_version_check());

    let mut app = App::new(
        Scope::AllProjects,
        base_url,
        Duration::from_secs(1),
        "1s".into(),
        Utc::now(),
    );
    app.color = false;
    app.ssh = false;
    let config = LoopConfig {
        tick: Duration::from_millis(50),
        closed: 10,
        project_id: None,
        timeout: Duration::from_secs(30),
        poll_timeout: Duration::from_secs(30),
        panicked: Arc::default(),
        shell: "/bin/sh".into(),
        message_dir: std::env::temp_dir(),
    };
    let (ktx, mut krx) = mpsc::unbounded_channel();
    let latest = Arc::new(Mutex::new(String::new()));
    let sink = Arc::clone(&latest);
    let mut terminal = Terminal::new(TestBackend::new(120, 30)).unwrap();

    let driver = async {
        let k = |c| {
            ktx.send(KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE))
                .unwrap()
        };
        // The board, with project names.
        wait_for(&latest, "Fix the login").await;
        wait_for(&latest, "alpha-project").await;
        wait_for(&latest, "beta-project").await;
        wait_for(&latest, "Old release").await;
        // `c` then `n`: nothing is sent.
        k('c');
        wait_for(&latest, "Cancel \"Fix the login\"?").await;
        k('n');
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert!(fake.cancels.lock().unwrap().is_empty());
        assert!(!latest.lock().unwrap().contains("Cancel \""));
        // Next row, `c` then `y`: one cancel, for that task.
        k('j');
        k('c');
        wait_for(&latest, "Cancel \"Speed up checkout\"?").await;
        k('y');
        wait_for(&latest, "cancelled bbbb2222").await;
        assert_eq!(
            *fake.cancels.lock().unwrap(),
            vec!["bbbb2222-0".to_string()]
        );
        // Enter: streamed events.
        ktx.send(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
            .unwrap();
        // The status view: the daemon's progress line, and the last events.
        wait_for_row(&latest, &["1", "start", "coding"]).await;
        wait_for(&latest, "hello streamed").await;
        wait_for(&latest, "second streamed").await;
        // `e` expands to the full stream.
        k('e');
        wait_for(&latest, "events (following)").await;
        // Esc returns to the status view, a second one to the list.
        ktx.send(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE))
            .unwrap();
        wait_for(&latest, "last events (e expands)").await;
        ktx.send(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE))
            .unwrap();
        wait_for(&latest, "RECENTLY CLOSED").await;
        k('q');
    };
    let mut on_frame = |buf: &Buffer| *sink.lock().unwrap() = screen(buf);
    let mut handover = NoHandover;
    let run = run_loop(
        &mut terminal,
        &mut app,
        client,
        &config,
        &mut krx,
        &mut handover,
        &mut on_frame,
    );
    let (result, ()) =
        tokio::time::timeout(Duration::from_secs(60), async { tokio::join!(run, driver) })
            .await
            .expect("the loop should end on q");
    result.unwrap();
    assert_eq!(fake.cancels.lock().unwrap().len(), 1);
    // The dashboard never keeps work: `keep` is sent, and it is false.
    assert_eq!(*fake.keeps.lock().unwrap(), vec![json!(false)]);
}

#[tokio::test]
async fn an_unreachable_daemon_shows_the_error_and_the_loop_still_quits() {
    // Nothing listens on this port.
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    drop(listener);
    let client = Arc::new(Client::new(format!("http://{addr}")).without_version_check());
    let mut app = new_app(Scope::AllProjects);
    app.now = Utc::now();
    let config = LoopConfig {
        tick: Duration::from_millis(50),
        closed: 10,
        project_id: None,
        timeout: Duration::from_secs(30),
        poll_timeout: Duration::from_secs(30),
        panicked: Arc::default(),
        shell: "/bin/sh".into(),
        message_dir: std::env::temp_dir(),
    };
    let (ktx, mut krx) = mpsc::unbounded_channel();
    let latest = Arc::new(Mutex::new(String::new()));
    let sink = Arc::clone(&latest);
    let mut terminal = Terminal::new(TestBackend::new(120, 30)).unwrap();
    let driver = async {
        wait_for(&latest, "daemon unreachable").await;
        wait_for(&latest, "retrying every 2s").await;
        ktx.send(KeyEvent::new(KeyCode::Char('q'), KeyModifiers::NONE))
            .unwrap();
    };
    let mut on_frame = |buf: &Buffer| *sink.lock().unwrap() = screen(buf);
    let mut handover = NoHandover;
    let run = run_loop(
        &mut terminal,
        &mut app,
        client,
        &config,
        &mut krx,
        &mut handover,
        &mut on_frame,
    );
    let (result, ()) = tokio::join!(run, driver);
    result.unwrap();
}

#[test]
fn a_failed_cancel_shows_the_daemons_error_and_does_not_refresh() {
    let mut app = board();
    let effects = update(
        &mut app,
        Msg::Action {
            kind: ActionKind::Cancel,
            id: "x".into(),
            result: Err("409 task is closed".into()),
        },
    );
    assert!(effects.is_empty());
    let s = app.status.as_ref().unwrap();
    assert_eq!(s.level, Level::Error);
    assert_eq!(s.text, "cancel failed: 409 task is closed");
}

#[test]
fn a_failed_open_shows_the_error_and_the_url() {
    let mut app = board();
    let effects = update(
        &mut app,
        Msg::OpenFailed {
            url: "https://github.com/o/r/pull/9".into(),
            error: "no such file".into(),
        },
    );
    assert!(effects.is_empty());
    let s = app.status.as_ref().unwrap();
    assert_eq!(s.level, Level::Error);
    assert!(s.text.contains("no such file") && s.text.contains("/pull/9"));
}

#[test]
fn cancel_on_a_closed_task_says_why_and_opens_no_prompt() {
    let mut app = new_app(Scope::AllProjects);
    load(
        &mut app,
        vec![],
        vec![summary("cccc3333-0", "Old", "closed", None, None, "p")],
    );
    let effects = ch(&mut app, 'c');
    assert!(effects.is_empty());
    assert!(app.prompt.is_none());
    assert_eq!(
        app.status.as_ref().unwrap().text,
        "cancel: task is closed, not open, paused or stuck"
    );
}

#[test]
fn a_half_failed_poll_keeps_the_rows_and_reports_the_error() {
    let mut app = board();
    let before = app.active.len();
    let closed_before = app.closed.len();
    update(
        &mut app,
        Msg::List(Box::new(ListResult {
            server: None,
            at: now(),
            active: Ok(vec![]),
            closed: Err("boom".into()),
            projects: None,
        })),
    );
    assert_eq!(app.active.len(), before);
    assert_eq!(app.closed.len(), closed_before);
    assert!(render(&app, 120, 30).contains("daemon unreachable: boom"));
}

#[test]
fn a_poll_error_is_not_hidden_by_an_info_status() {
    let mut app = board();
    update(
        &mut app,
        Msg::Action {
            kind: ActionKind::Cancel,
            id: "bbbb2222-0".into(),
            result: Ok(ActionOk::Cancelled),
        },
    );
    update(
        &mut app,
        Msg::List(Box::new(ListResult {
            server: None,
            at: now(),
            active: Err("refused".into()),
            closed: Err("refused".into()),
            projects: None,
        })),
    );
    assert!(render(&app, 120, 30).contains("daemon unreachable: refused"));
}

#[test]
fn a_version_mismatch_is_marked_in_the_header() {
    let mut app = board();
    app.daemon_version = Some("0.0.1-other".into());
    let s = render(&app, 120, 30);
    assert!(
        s.contains(&format!(
            "daemon 0.0.1-other (choco {})",
            chocofactory_core::version::VERSION
        )),
        "{s}"
    );
}

/// A daemon that accepts connections and never answers.
async fn hung_daemon() -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move {
        let mut held = Vec::new();
        while let Ok((sock, _)) = listener.accept().await {
            held.push(sock);
        }
    });
    url
}

#[tokio::test]
async fn a_hung_daemon_does_not_stop_the_first_load_from_quitting() {
    let client = Arc::new(Client::new(hung_daemon().await).without_version_check());
    let mut app = new_app(Scope::AllProjects);
    let config = LoopConfig {
        tick: Duration::from_millis(50),
        closed: 10,
        project_id: None,
        timeout: Duration::from_secs(30),
        poll_timeout: Duration::from_secs(30),
        panicked: Arc::default(),
        shell: "/bin/sh".into(),
        message_dir: std::env::temp_dir(),
    };
    let (ktx, mut krx) = mpsc::unbounded_channel();
    ktx.send(KeyEvent::new(KeyCode::Char('q'), KeyModifiers::NONE))
        .unwrap();
    let mut terminal = Terminal::new(TestBackend::new(120, 30)).unwrap();
    let mut on_frame = |_: &Buffer| {};
    let mut handover = NoHandover;
    let result = tokio::time::timeout(
        Duration::from_secs(10),
        run_loop(
            &mut terminal,
            &mut app,
            client,
            &config,
            &mut krx,
            &mut handover,
            &mut on_frame,
        ),
    )
    .await
    .expect("q must quit while the first load hangs");
    result.unwrap();
}

#[tokio::test]
async fn a_request_that_times_out_is_shown_as_unreachable() {
    let client = Arc::new(Client::new(hung_daemon().await).without_version_check());
    let mut app = new_app(Scope::AllProjects);
    let config = LoopConfig {
        tick: Duration::from_millis(50),
        closed: 10,
        project_id: None,
        timeout: Duration::from_millis(300),
        poll_timeout: Duration::from_millis(300),
        panicked: Arc::default(),
        shell: "/bin/sh".into(),
        message_dir: std::env::temp_dir(),
    };
    let (ktx, mut krx) = mpsc::unbounded_channel();
    let latest = Arc::new(Mutex::new(String::new()));
    let sink = Arc::clone(&latest);
    let mut terminal = Terminal::new(TestBackend::new(120, 30)).unwrap();
    let driver = async {
        wait_for(&latest, "daemon unreachable: request timed out").await;
        ktx.send(KeyEvent::new(KeyCode::Char('q'), KeyModifiers::NONE))
            .unwrap();
    };
    let mut on_frame = |buf: &Buffer| *sink.lock().unwrap() = screen(buf);
    let mut handover = NoHandover;
    let run = run_loop(
        &mut terminal,
        &mut app,
        client,
        &config,
        &mut krx,
        &mut handover,
        &mut on_frame,
    );
    let (result, ()) =
        tokio::time::timeout(Duration::from_secs(20), async { tokio::join!(run, driver) })
            .await
            .expect("the loop should report the timeout and quit");
    result.unwrap();
}

#[tokio::test]
async fn detail_cancel_and_retry_requests_time_out_visibly() {
    let fake = Fake {
        tasks: Arc::new(vec![
            summary(
                "aaaa1111-0",
                "Busy one",
                "open",
                Some("coding"),
                Some(3),
                "p-alpha",
            ),
            summary(
                "bbbb2222-0",
                "Stuck one",
                "stuck",
                Some("coding"),
                Some(9),
                "p-alpha",
            ),
        ]),
        cancels: Arc::new(Mutex::new(Vec::new())),
        keeps: Arc::new(Mutex::new(Vec::new())),
        hang: true,
        queries: Arc::default(),
        ..Default::default()
    };
    let base_url = fake_server(fake).await;
    let client = Arc::new(Client::new(base_url).without_version_check());
    let mut app = new_app(Scope::AllProjects);
    app.now = Utc::now();
    app.ssh = false;
    let config = LoopConfig {
        tick: Duration::from_millis(50),
        closed: 10,
        project_id: None,
        timeout: Duration::from_millis(300),
        poll_timeout: Duration::from_millis(300),
        panicked: Arc::default(),
        shell: "/bin/sh".into(),
        message_dir: std::env::temp_dir(),
    };
    let (ktx, mut krx) = mpsc::unbounded_channel();
    let latest = Arc::new(Mutex::new(String::new()));
    let sink = Arc::clone(&latest);
    let mut terminal = Terminal::new(TestBackend::new(120, 30)).unwrap();
    let driver = async {
        let k = |c| {
            ktx.send(KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE))
                .unwrap()
        };
        wait_for(&latest, "Busy one").await;
        k('c');
        wait_for(&latest, "Cancel \"Busy one\"?").await;
        k('y');
        wait_for(&latest, "cancel failed: request timed out").await;
        k('j');
        k('r');
        wait_for(&latest, "Retry \"Stuck one\"?").await;
        k('y');
        wait_for(&latest, "retry failed: request timed out").await;
        ktx.send(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
            .unwrap();
        wait_for(&latest, "could not load the task: request timed out").await;
        ktx.send(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE))
            .unwrap();
        wait_for(&latest, "RECENTLY CLOSED").await;
        k('q');
    };
    let mut on_frame = |buf: &Buffer| *sink.lock().unwrap() = screen(buf);
    let mut handover = NoHandover;
    let run = run_loop(
        &mut terminal,
        &mut app,
        client,
        &config,
        &mut krx,
        &mut handover,
        &mut on_frame,
    );
    let (result, ()) =
        tokio::time::timeout(Duration::from_secs(40), async { tokio::join!(run, driver) })
            .await
            .expect("every hung request should time out and the loop should quit");
    result.unwrap();
}

#[tokio::test]
async fn a_panic_signal_stops_the_loop_with_an_error() {
    let client = Arc::new(Client::new(hung_daemon().await).without_version_check());
    let mut app = new_app(Scope::AllProjects);
    let config = LoopConfig {
        tick: Duration::from_millis(50),
        closed: 10,
        project_id: None,
        timeout: Duration::from_millis(300),
        poll_timeout: Duration::from_millis(300),
        panicked: Arc::default(),
        shell: "/bin/sh".into(),
        message_dir: std::env::temp_dir(),
    };
    let (_ktx, mut krx) = mpsc::unbounded_channel();
    let latest = Arc::new(Mutex::new(String::new()));
    let sink = Arc::clone(&latest);
    let mut terminal = Terminal::new(TestBackend::new(120, 30)).unwrap();
    let signal = Arc::clone(&config.panicked);
    let driver = async {
        // Once the board is up, a task elsewhere "panics".
        wait_for(&latest, "daemon unreachable").await;
        signal.trip();
    };
    let mut on_frame = |buf: &Buffer| *sink.lock().unwrap() = screen(buf);
    let mut handover = NoHandover;
    let run = run_loop(
        &mut terminal,
        &mut app,
        client,
        &config,
        &mut krx,
        &mut handover,
        &mut on_frame,
    );
    let (result, ()) =
        tokio::time::timeout(Duration::from_secs(20), async { tokio::join!(run, driver) })
            .await
            .expect("the loop must stop on a panic");
    assert!(result.unwrap_err().contains("panicked"));
}

/// A daemon whose list endpoint answers after `delay`, and never once `hang`
/// is set. Everything else answers at once.
async fn slow_daemon(delay: Duration, hang: Arc<std::sync::atomic::AtomicBool>) -> String {
    #[derive(Clone)]
    struct S {
        delay: Duration,
        hang: Arc<std::sync::atomic::AtomicBool>,
    }
    async fn server() -> axum::Json<Value> {
        axum::Json(json!({
            "version": chocofactory_core::version::VERSION, "commit": null, "pid": 1,
            "port": 1, "started_at": "2026-01-01T00:00:00Z", "config_root": "/x",
            "exe": "/x", "exe_replaced": false, "choco_binary": "choco",
            "choco_binary_found": true, "tasks": {}, "in_flight": []
        }))
    }
    async fn projects() -> axum::Json<Value> {
        axum::Json(json!([]))
    }
    async fn tasks(State(s): State<S>) -> axum::Json<Value> {
        if s.hang.load(std::sync::atomic::Ordering::SeqCst) {
            std::future::pending::<()>().await;
        }
        tokio::time::sleep(s.delay).await;
        let t = summary(
            "aaaa1111-0",
            "Slow but there",
            "open",
            Some("coding"),
            Some(3),
            "p-alpha",
        );
        axum::Json(json!([t]))
    }
    let app = Router::new()
        .route("/server", get(server))
        .route("/projects", get(projects))
        .route("/tasks", get(tasks))
        .with_state(S { delay, hang });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    format!("http://{addr}")
}

/// Runs the loop against `url` until `driver` is done; `driver` sees the
/// latest screen and quits through the key channel.
async fn run_against<F, Fut>(url: String, config: LoopConfig, driver: F)
where
    F: FnOnce(Arc<Mutex<String>>, mpsc::UnboundedSender<KeyEvent>) -> Fut,
    Fut: std::future::Future<Output = ()>,
{
    let client = Arc::new(Client::new(url).without_version_check());
    let mut app = new_app(Scope::AllProjects);
    let (ktx, mut krx) = mpsc::unbounded_channel();
    let latest = Arc::new(Mutex::new(String::new()));
    let sink = Arc::clone(&latest);
    let mut terminal = Terminal::new(TestBackend::new(120, 30)).unwrap();
    let mut on_frame = |buf: &Buffer| *sink.lock().unwrap() = screen(buf);
    let mut handover = NoHandover;
    let run = run_loop(
        &mut terminal,
        &mut app,
        client,
        &config,
        &mut krx,
        &mut handover,
        &mut on_frame,
    );
    let (result, ()) = tokio::time::timeout(Duration::from_secs(30), async {
        tokio::join!(run, driver(latest, ktx))
    })
    .await
    .expect("the loop should end on q");
    result.unwrap();
}

fn quit(ktx: &mpsc::UnboundedSender<KeyEvent>) {
    ktx.send(KeyEvent::new(KeyCode::Char('q'), KeyModifiers::NONE))
        .unwrap();
}

/// A daemon whose board requests (with a `status`) work, but whose search
/// request (no `status`) either never answers or fails with a 500.
async fn broken_search_daemon(hang: bool) -> String {
    async fn server() -> axum::Json<Value> {
        axum::Json(json!({
            "version": chocofactory_core::version::VERSION, "commit": null, "pid": 1,
            "port": 1, "started_at": "2026-01-01T00:00:00Z", "config_root": "/x",
            "exe": "/x", "exe_replaced": false, "choco_binary": "choco",
            "choco_binary_found": true, "tasks": {}, "in_flight": []
        }))
    }
    async fn projects() -> axum::Json<Value> {
        axum::Json(json!([]))
    }
    async fn tasks(
        State(hang): State<bool>,
        Query(q): Query<HashMap<String, String>>,
    ) -> (StatusCode, axum::Json<Value>) {
        if q.contains_key("status") {
            let t = summary(
                "aaaa1111-0",
                "Board row",
                "open",
                Some("coding"),
                Some(3),
                "p-alpha",
            );
            return (StatusCode::OK, axum::Json(json!([t])));
        }
        if hang {
            std::future::pending::<()>().await;
        }
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            axum::Json(json!({"error": "search exploded"})),
        )
    }
    let app = Router::new()
        .route("/server", get(server))
        .route("/projects", get(projects))
        .route("/tasks", get(tasks))
        .with_state(hang);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    format!("http://{addr}")
}

fn search_config(timeout: Duration) -> LoopConfig {
    LoopConfig {
        tick: Duration::from_millis(50),
        closed: 10,
        project_id: None,
        timeout,
        poll_timeout: Duration::from_secs(30),
        panicked: Arc::default(),
        shell: "/bin/sh".into(),
        message_dir: std::env::temp_dir(),
    }
}

#[tokio::test]
async fn a_search_load_times_out_at_the_request_timeout() {
    let url = broken_search_daemon(true).await;
    run_against(
        url,
        search_config(Duration::from_millis(300)),
        |latest, ktx| async move {
            wait_for(&latest, "Board row").await;
            ktx.send(KeyEvent::new(KeyCode::Char('/'), KeyModifiers::NONE))
                .unwrap();
            wait_for(&latest, "could not load tasks: request timed out").await;
            ktx.send(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE))
                .unwrap();
            wait_for(&latest, "Board row").await;
            assert!(!latest.lock().unwrap().contains("could not load tasks"));
            quit(&ktx);
        },
    )
    .await;
}

#[tokio::test]
async fn a_search_load_error_reaches_the_popup() {
    let url = broken_search_daemon(false).await;
    run_against(
        url,
        search_config(Duration::from_secs(5)),
        |latest, ktx| async move {
            wait_for(&latest, "Board row").await;
            ktx.send(KeyEvent::new(KeyCode::Char('/'), KeyModifiers::NONE))
                .unwrap();
            wait_for(&latest, "could not load tasks:").await;
            wait_for(&latest, "search exploded").await;
            ktx.send(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE))
                .unwrap();
            quit(&ktx);
        },
    )
    .await;
}

#[test]
fn moving_with_nothing_selected_does_nothing() {
    let mut app = board();
    let g = open_search(&mut app);
    update(
        &mut app,
        Msg::Search {
            generation: g,
            result: Ok(vec![summary(
                "aaaa1111-0",
                "One",
                "open",
                None,
                None,
                "p-alpha",
            )]),
        },
    );
    for c in "zz".chars() {
        ch(&mut app, c);
    }
    assert!(press(&mut app, KeyCode::Down).is_empty());
    assert!(press(&mut app, KeyCode::PageDown).is_empty());
    assert_eq!(app.search.as_ref().unwrap().selected, None);
}

#[tokio::test]
async fn list_polls_time_out_at_the_poll_timeout_not_the_request_timeout() {
    // The first load works; every poll after it hangs.
    let hang = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let url = slow_daemon(Duration::ZERO, Arc::clone(&hang)).await;
    let config = LoopConfig {
        tick: Duration::from_millis(50),
        closed: 10,
        project_id: None,
        timeout: Duration::from_secs(30),
        poll_timeout: Duration::from_millis(200),
        panicked: Arc::default(),
        shell: "/bin/sh".into(),
        message_dir: std::env::temp_dir(),
    };
    run_against(url, config, |latest, ktx| async move {
        wait_for(&latest, "Slow but there").await;
        hang.store(true, std::sync::atomic::Ordering::SeqCst);
        wait_for(&latest, "daemon unreachable: request timed out").await;
        quit(&ktx);
    })
    .await;
}

#[tokio::test]
async fn the_first_load_waits_for_a_slow_daemon_past_the_poll_timeout() {
    let url = slow_daemon(
        Duration::from_millis(300),
        Arc::new(std::sync::atomic::AtomicBool::new(false)),
    )
    .await;
    let config = LoopConfig {
        tick: Duration::from_millis(50),
        closed: 10,
        project_id: None,
        timeout: Duration::from_secs(5),
        poll_timeout: Duration::from_millis(100),
        panicked: Arc::default(),
        shell: "/bin/sh".into(),
        message_dir: std::env::temp_dir(),
    };
    run_against(url, config, |latest, ktx| async move {
        wait_for(&latest, "Slow but there").await;
        // The first frame already has the rows, not the outage line.
        assert!(
            !latest.lock().unwrap().contains("daemon unreachable"),
            "{}",
            latest.lock().unwrap()
        );
        quit(&ktx);
    })
    .await;
}

#[test]
fn the_poll_timeout_is_the_interval_between_ten_and_thirty_seconds() {
    let t = |s| super::poll_timeout_for(Duration::from_secs(s));
    assert_eq!(t(1), Duration::from_secs(10));
    assert_eq!(t(20), Duration::from_secs(20));
    assert_eq!(t(600), Duration::from_secs(30));
}

#[test]
fn a_panic_on_any_thread_trips_the_signal_through_the_hook() {
    let signal: Arc<PanicSignal> = Arc::default();
    let prev = super::install_panic_hook_with(Arc::clone(&signal), || {});
    let joined = std::thread::spawn(|| panic!("boom from a spawned thread")).join();
    super::restore_panic_hook(prev);
    assert!(joined.is_err());
    assert!(signal.is_tripped());
}

// ---- the status view (#178) ---------------------------------------------

const BUSY: &str = "9c03aa17-bbbb";
const SHA: &str = "0123456789abcdef0123456789abcdef";

/// The `GET /tasks/{id}` answer for the in-progress task of `board()`.
fn busy_detail() -> Value {
    json!({
        "id": BUSY, "project_id": "p-alpha", "workflow_def": "coding-task",
        "title": "Per-kind stage execution (#55)", "status": "open",
        "stuck_reason": null,
        "config": {"cwd": "/home/dev/chocofactory", "roles": {"coder": {"model": "opus"}}},
        "created_at": "2026-01-01T03:40:00Z",
        "workflow_path": "builtin:coding-task", "workflow_sha256": SHA,
        "workflow_file_status": "unchanged",
        "workflow_state": {
            "current_stage": "internal_review",
            "loop_counters": {"internal_review": {"count": 2}}
        },
        "stage_trail": [
            {"created_at": "2026-01-01T03:41:00Z", "payload": {"stage": "coding", "outcome": null}},
            {"created_at": "2026-01-01T04:10:00Z", "payload": {"stage": "internal_review", "outcome": "done"}},
            {"created_at": "2026-01-01T04:50:00Z", "payload": {"stage": "revising", "outcome": "changes_requested"}},
            {"created_at": "2026-01-01T11:48:00Z", "payload": {"stage": "internal_review", "outcome": "done"}},
        ]
    })
}

fn open_detail(app: &mut App, id: &str) {
    app.selected = Some(id.to_string());
    press(app, KeyCode::Enter);
}

fn answer(app: &mut App, id: &str, result: Result<Value, String>) {
    update(
        app,
        Msg::Detail {
            id: id.to_string(),
            result,
        },
    );
}

fn event_at(id: &str, n: usize) -> chocofactory_core::models::Event {
    chocofactory_core::models::Event {
        id: format!("e{n}"),
        task_id: id.to_string(),
        session_id: None,
        event_type: chocofactory_core::models::EventType::HumanMessage,
        payload: json!({"text": format!("message {n}")}),
        created_at: now() + chrono::Duration::seconds(n as i64),
    }
}

fn push_events(app: &mut App, id: &str, n: usize) {
    update(
        app,
        Msg::Socket {
            id: id.to_string(),
            msg: SocketMsg::Connected,
        },
    );
    for i in 0..n {
        update(
            app,
            Msg::Socket {
                id: id.to_string(),
                msg: SocketMsg::Event(Box::new(event_at(id, i))),
            },
        );
    }
}

/// Local-time text of the `n`th test event, as the view prints it.
fn at(n: usize) -> String {
    (now() + chrono::Duration::seconds(n as i64))
        .with_timezone(&chrono::Local)
        .format("%H:%M:%S")
        .to_string()
}

/// Compares whole screens, ignoring trailing spaces and trailing blank lines.
fn assert_screen(actual: &str, expected: &str) {
    let norm = |s: &str| {
        let mut v: Vec<String> = s.lines().map(|l| l.trim_end().to_string()).collect();
        while v.last().is_some_and(String::is_empty) {
            v.pop();
        }
        v.join("\n")
    };
    assert_eq!(norm(actual), norm(expected), "\nactual:\n{actual}");
}

fn busy_view() -> App {
    let mut app = board();
    open_detail(&mut app, BUSY);
    answer(&mut app, BUSY, Ok(busy_detail()));
    push_events(&mut app, BUSY, 7);
    app
}

/// `─ title ─…` filled to `w` columns.
fn sep(title: &str, w: usize) -> String {
    let head = format!("─ {title} ");
    format!("{head}{}", "─".repeat(w - head.chars().count()))
}

fn title_row(title: &str, right: &str, w: usize) -> String {
    format!(
        "{title:<width$}  {right}",
        width = w - right.chars().count() - 2
    )
}

#[test]
fn the_status_view_shows_fields_progress_counters_and_the_event_tail() {
    let app = busy_view();
    let expected = [
        title_row(
            "Per-kind stage execution (#55)",
            "e events · Esc back · ? help",
            100,
        ),
        "ID        9c03aa17-bbbb".into(),
        "Project   chocofactory".into(),
        "Workflow  coding-task".into(),
        "Workflow  builtin:coding-task  [0123456789ab]".into(),
        "Status    open".into(),
        "Repo      /home/dev/chocofactory".into(),
        "Role      coder: model=opus".into(),
        "Created   2026-01-01 03:40:00 UTC".into(),
        "Stage     internal_review for 12m".into(),
        "Cost      no data".into(),
        sep("progress", 100),
        "  #  from             outcome            to               at (UTC)".into(),
        "  1                   start              coding           03:41:00".into(),
        "  2  coding           done               internal_review  04:10:00".into(),
        "  3  internal_review  changes_requested  revising         04:50:00".into(),
        "  4  revising         done               internal_review  11:48:00  ◀ current".into(),
        "Loop counters  internal_review=2".into(),
        sep("last events (e expands)", 100),
        format!("  {}  human_message message 0", at(0)),
        format!("  {}  human_message message 1", at(1)),
        format!("  {}  human_message message 2", at(2)),
        format!("  {}  human_message message 3", at(3)),
        format!("  {}  human_message message 4", at(4)),
        format!("  {}  human_message message 5", at(5)),
        format!("  {}  human_message message 6", at(6)),
        String::new(),
        String::new(),
        String::new(),
        "e events  o PR  r retry  p pause  s send  c cancel  Esc back".into(),
    ]
    .join("\n");
    assert_screen(&render(&app, 100, 30), &expected);
}

#[test]
fn the_dashboard_status_view_leaves_out_the_closes_row() {
    let mut app = board();
    open_detail(&mut app, BUSY);
    let mut detail = busy_detail();
    detail["closes"] = json!([152, 126]);
    assert!(
        crate::render::task_fields(&detail)
            .iter()
            .any(|(l, _)| *l == crate::render::LABEL_CLOSES)
    );
    answer(&mut app, BUSY, Ok(detail));
    let screen = render(&app, 100, 30);
    assert!(screen.contains("Status    open"), "{screen}");
    assert!(
        !screen.contains("Closes") && !screen.contains("#152"),
        "{screen}"
    );
}

fn with_detail_status(mut v: Value, status: &str, stage: &str) -> Value {
    v["status"] = json!(status);
    v["workflow_state"]["current_stage"] = json!(stage);
    v
}

#[test]
fn a_task_waiting_on_you_shows_the_pr_and_what_it_waits_for() {
    let mut app = board();
    let id = "3f2a91c0-aaaa";
    open_detail(&mut app, id);
    answer(
        &mut app,
        id,
        Ok(with_detail_status(
            busy_detail(),
            "open",
            "awaiting_human_review",
        )),
    );
    let s = render(&app, 100, 30);
    let want_stage = "Stage        awaiting_human_review for 2h05m";
    let want_pr = "PR           #171 https://github.com/o/r/pull/171";
    let want_wait =
        "Waiting for  your verdict: a PR comment with /approve or /request-changes on its own line";
    let lines: Vec<&str> = s.lines().map(str::trim_end).collect();
    let i = lines.iter().position(|l| *l == want_stage).expect(&s);
    assert_eq!(lines[i + 1], want_pr, "{s}");
    assert_eq!(lines[i + 2], want_wait, "{s}");

    {
        let (id, stage) = ("b81e0d44-cccc", "escalate_to_human");
        let want = "Waiting for  a note to resume it: choco task send b81e0d44-cccc --text \"…\"";
        let mut app = board();
        open_detail(&mut app, id);
        answer(
            &mut app,
            id,
            Ok(with_detail_status(busy_detail(), "open", stage)),
        );
        let s = render(&app, 120, 30);
        assert!(s.lines().any(|l| l.trim_end() == want), "{s}");
    }

    // `spec_questions` needs a task at that stage.
    let mut app = new_app(Scope::AllProjects);
    let t = waiting(summary(
        "5e5e5e5e-ffff",
        "Spec it",
        "open",
        Some("spec_questions"),
        Some(5),
        "p-alpha",
    ));
    load(&mut app, vec![t], vec![]);
    open_detail(&mut app, "5e5e5e5e-ffff");
    let s = render(&app, 120, 30);
    let want = "Waiting for  your answers to the spec check's questions: choco task send 5e5e5e5e-ffff --text \"…\"";
    assert!(s.lines().any(|l| l.trim_end() == want), "{s}");
    // No PR yet: no PR row.
    assert!(!s.lines().any(|l| l.starts_with("PR ")), "{s}");
}

#[test]
fn a_task_that_is_not_waiting_has_no_waiting_for_row() {
    let app = busy_view();
    assert!(!render(&app, 100, 30).contains("Waiting for"));
    // Nor does a stuck or closed task at a waiting stage.
    let mut app = new_app(Scope::AllProjects);
    let t = waiting(summary(
        "5e5e5e5e-ffff",
        "Spec it",
        "closed",
        Some("spec_questions"),
        Some(5),
        "p-alpha",
    ));
    load(&mut app, vec![], vec![t]);
    open_detail(&mut app, "5e5e5e5e-ffff");
    assert!(!render(&app, 100, 30).contains("Waiting for"));
}

#[test]
fn an_open_task_the_daemon_does_not_report_waiting_has_no_waiting_for_row() {
    let mut app = new_app(Scope::AllProjects);
    let t = summary(
        "6f6f6f6f-aaaa",
        "Not a gate here",
        "open",
        Some("awaiting_human_review"),
        Some(6),
        "p-alpha",
    );
    load(&mut app, vec![], vec![t]);
    open_detail(&mut app, "6f6f6f6f-aaaa");
    assert!(!render(&app, 100, 30).contains("Waiting for"));
}

#[test]
fn a_stuck_task_shows_the_reason_wrapped_in_the_error_style() {
    let mut app = board();
    app.color = true;
    let id = "7d22e1a8-dddd";
    app.active
        .iter_mut()
        .find(|t| t.task.id == id)
        .unwrap()
        .task
        .stuck_reason = Some(
        "the agent hit a usage limit and could not continue; resume it once the limit resets overnight please"
            .into(),
    );
    open_detail(&mut app, id);
    let mut terminal = Terminal::new(TestBackend::new(60, 20)).unwrap();
    terminal.draw(|f| draw(f, &app)).unwrap();
    let buf = terminal.backend().buffer().clone();
    let s = screen(&buf);
    let lines: Vec<&str> = s.lines().map(str::trim_end).collect();
    let i = lines
        .iter()
        .position(|l| l.starts_with("Stuck "))
        .expect(&s);
    assert_eq!(
        lines[i],
        "Stuck     the agent hit a usage limit and could not"
    );
    assert!(
        lines[i + 1].starts_with("          ") && !lines[i + 1].starts_with("           "),
        "{s}"
    );
    assert!(lines[i + 1].trim().len() > 5, "{s}");
    assert_eq!(buf[(0, i as u16)].fg, ratatui::style::Color::Red);
    assert_eq!(buf[(12, i as u16 + 1)].fg, ratatui::style::Color::Red);
    assert!(s.contains("r retry"), "{s}");
}

#[test]
fn a_closed_task_says_how_long_ago_it_closed() {
    let mut app = board();
    let id = "61b0f2e3-eeee";
    {
        let t = app.closed.iter_mut().find(|t| t.task.id == id).unwrap();
        t.task.updated_at = now() - chrono::Duration::minutes(27 * 60);
        t.current_stage = Some("done".into());
    }
    open_detail(&mut app, id);
    answer(
        &mut app,
        id,
        Ok(with_detail_status(busy_detail(), "closed", "done")),
    );
    push_events(&mut app, id, 2);
    let s = render(&app, 100, 30);
    let lines: Vec<&str> = s.lines().map(str::trim_end).collect();
    let i = lines
        .iter()
        .position(|l| *l == "Stage     done (closed 1d03h ago)")
        .expect(&s);
    assert_eq!(
        lines[i + 1],
        "PR        #163 https://github.com/o/r/pull/163",
        "{s}"
    );
    assert!(
        s.contains(&format!("  {}  human_message message 1", at(1))),
        "{s}"
    );
}

#[test]
fn the_expanded_view_has_the_full_stream_and_its_own_footer() {
    let mut app = busy_view();
    ch(&mut app, 'e');
    let mut expected = vec![
        title_row(
            "Per-kind stage execution (#55)",
            "e/Esc status · ? help",
            100,
        ),
        sep("events (following)", 100),
    ];
    for n in 0..7 {
        expected.push(format!("  {}  human_message message {n}", at(n)));
    }
    expected.extend(std::iter::repeat_n(String::new(), 20));
    expected.push(
        "PgUp/PgDn scroll  End follow  e/Esc status  o PR  r retry  p pause  s send  c cancel"
            .into(),
    );
    assert_screen(&render(&app, 100, 30), &expected.join("\n"));
    // Scrolling still works there.
    push_events(&mut app, BUSY, 60);
    let s = render(&app, 80, 20);
    assert!(
        s.contains("message 59") && !s.contains("message 0\n"),
        "{s}"
    );
    press(&mut app, KeyCode::PageUp);
    let s = render(&app, 80, 20);
    assert!(
        s.contains("End follows") && !s.contains("message 59"),
        "{s}"
    );
    press(&mut app, KeyCode::End);
    assert!(render(&app, 80, 20).contains("events (following)"));
}

fn detail_state(app: &App) -> (bool, bool, usize) {
    let View::Detail(d) = &app.view else {
        panic!("not in the detail view")
    };
    (d.expanded, d.following, d.scroll_back)
}

#[test]
fn e_toggles_the_expanded_view_and_esc_and_q_leave_as_documented() {
    let mut app = board();
    open_detail(&mut app, BUSY);
    assert!(!detail_state(&app).0, "Enter opens the status view");
    assert!(ch(&mut app, 'e').is_empty());
    assert!(detail_state(&app).0);
    assert!(ch(&mut app, 'e').is_empty());
    assert!(!detail_state(&app).0);
    ch(&mut app, 'e');
    assert!(press(&mut app, KeyCode::Esc).is_empty());
    assert!(!detail_state(&app).0, "Esc in the expanded view -> status");
    assert_eq!(press(&mut app, KeyCode::Esc), vec![Effect::CloseSocket]);
    assert!(matches!(app.view, View::List));
    // `q` leaves from either view.
    open_detail(&mut app, BUSY);
    assert_eq!(ch(&mut app, 'q'), vec![Effect::CloseSocket]);
    assert!(matches!(app.view, View::List));
    open_detail(&mut app, BUSY);
    ch(&mut app, 'e');
    assert_eq!(ch(&mut app, 'q'), vec![Effect::CloseSocket]);
    assert!(matches!(app.view, View::List));
}

#[test]
fn the_status_view_does_not_scroll_and_expanding_starts_following() {
    let mut app = board();
    open_detail(&mut app, BUSY);
    push_events(&mut app, BUSY, 30);
    for code in [
        KeyCode::PageUp,
        KeyCode::Up,
        KeyCode::Char('k'),
        KeyCode::Home,
    ] {
        assert!(press(&mut app, code).is_empty());
    }
    assert_eq!(detail_state(&app), (false, true, 0));
    ch(&mut app, 'e');
    press(&mut app, KeyCode::PageUp);
    assert!(!detail_state(&app).1);
    ch(&mut app, 'e');
    ch(&mut app, 'e');
    assert_eq!(detail_state(&app), (true, true, 0));
}

#[test]
fn s_without_an_editor_says_so_in_both_views_and_help_lists_it() {
    let mut app = board();
    open_detail(&mut app, BUSY);
    for expanded in [false, true] {
        if expanded {
            ch(&mut app, 'e');
        }
        assert!(ch(&mut app, 's').is_empty());
        let shown = render(&app, 100, 30);
        assert!(shown.contains("s: set $VISUAL or $EDITOR"), "{shown}");
    }
    ch(&mut app, '?');
    let help = render(&app, 100, 30);
    assert!(
        help.contains("e           events / status s     send a message"),
        "{help}"
    );
}

#[test]
fn a_short_screen_shrinks_the_events_tail_then_cuts_the_status_block() {
    let app = busy_view();
    // 22 rows: the whole status block, and two of the events.
    let s = render(&app, 100, 22);
    for want in ["Role      coder", "Loop counters", "message 5", "message 6"] {
        assert!(s.contains(want), "{want}\n{s}");
    }
    assert!(row_has(&s, &["1", "start", "coding"]), "{s}");
    assert!(!s.contains("message 4"), "{s}");
    assert!(s.contains("last events (e expands)"));
    // 19 rows: no room for an event line, so no tail at all.
    let s = render(&app, 100, 19);
    assert!(!s.contains("last events") && !s.contains("message"), "{s}");
    assert!(
        s.contains("Role      coder") && row_has(&s, &["1", "start", "coding"]),
        "{s}"
    );

    // 12 rows: no events; the path row goes, and the progress list keeps its
    // header and its newest line.
    let expected = [
        title_row(
            "Per-kind stage execution (#55)",
            "e events · Esc back · ? help",
            100,
        ),
        "ID        9c03aa17-bbbb".into(),
        "Project   chocofactory".into(),
        "Workflow  coding-task".into(),
        "Status    open".into(),
        "Stage     internal_review for 12m".into(),
        sep("progress", 100),
        "  #  from             outcome            to               at (UTC)".into(),
        "  … 3 earlier steps".into(),
        "  4  revising         done               internal_review  11:48:00  ◀ current".into(),
        "Loop counters  internal_review=2".into(),
        "e events  o PR  r retry  p pause  s send  c cancel  Esc back".into(),
    ]
    .join("\n");
    assert_screen(&render(&app, 100, 12), &expected);

    // 40x10: the rows a typical task needs.
    let s = render(&app, 40, 10);
    for want in [
        "Per-kind …  e events",
        "ID        9c03aa17-bbbb",
        "Project   chocofactory",
        "Workflow  coding-task",
        "Status    open",
        "Stage     internal_review for 12m",
        "◀",
    ] {
        assert!(s.contains(want), "{want}\n{s}");
    }
}

#[test]
fn a_cut_progress_list_leaves_room_before_rows_go() {
    // Rows are dropped Role, Created, Repo, path - and only as far as needed,
    // with every progress step (and its header row) still counted.
    let app = busy_view();
    let s = render(&app, 100, 17);
    assert!(!s.contains("Role") && s.contains("Created"), "{s}");
    let s = render(&app, 100, 16);
    assert!(!s.contains("Created") && s.contains("Repo"), "{s}");
    let s = render(&app, 100, 15);
    assert!(
        !s.contains("Repo") && s.contains("builtin:coding-task"),
        "{s}"
    );
    let s = render(&app, 100, 14);
    assert!(
        !s.contains("builtin:coding-task") && s.contains("Stage"),
        "{s}"
    );
}

#[test]
fn there_is_a_line_when_there_are_no_events_yet() {
    let mut app = board();
    open_detail(&mut app, BUSY);
    answer(&mut app, BUSY, Ok(busy_detail()));
    let s = render(&app, 100, 30);
    assert!(s.contains("  (no events yet)"), "{s}");
    update(
        &mut app,
        Msg::Socket {
            id: BUSY.into(),
            msg: SocketMsg::Down,
        },
    );
    assert!(render(&app, 100, 30).contains("─ events: reconnecting ─"));
}

#[test]
fn before_the_answer_the_view_loads_from_the_snapshot_and_the_answer_replaces_it() {
    let mut app = board();
    open_detail(&mut app, BUSY);
    let s = render(&app, 100, 30);
    for want in [
        "ID        9c03aa17-bbbb",
        "Project   chocofactory",
        "Workflow  coding-task",
        "Status    open",
        "Stage     internal_review for 12m",
        "  loading…",
    ] {
        assert!(s.contains(want), "{want}\n{s}");
    }
    assert!(!s.contains("Repo"), "{s}");
    answer(&mut app, BUSY, Ok(busy_detail()));
    let s = render(&app, 100, 30);
    assert!(
        !s.contains("loading…") && row_has(&s, &["1", "start", "coding"]),
        "{s}"
    );
    assert!(s.contains("Repo      /home/dev/chocofactory"), "{s}");
}

#[test]
fn a_failed_first_answer_is_shown_and_the_next_success_replaces_it() {
    let mut app = board();
    app.color = true;
    open_detail(&mut app, BUSY);
    answer(&mut app, BUSY, Err("request timed out".into()));
    let mut terminal = Terminal::new(TestBackend::new(100, 30)).unwrap();
    terminal.draw(|f| draw(f, &app)).unwrap();
    let buf = terminal.backend().buffer().clone();
    let s = screen(&buf);
    let i = s
        .lines()
        .position(|l| l.starts_with("  could not load the task: request timed out"))
        .expect(&s);
    assert_eq!(buf[(4, i as u16)].fg, ratatui::style::Color::Red);
    assert!(!s.contains("loading…"));
    answer(&mut app, BUSY, Ok(busy_detail()));
    let s = render(&app, 100, 30);
    assert!(
        !s.contains("could not load") && row_has(&s, &["1", "start", "coding"]),
        "{s}"
    );
}

#[test]
fn a_failed_refresh_keeps_the_rows_and_says_so_on_the_separator() {
    let mut app = board();
    open_detail(&mut app, BUSY);
    answer(&mut app, BUSY, Ok(busy_detail()));
    answer(&mut app, BUSY, Err("request timed out".into()));
    let s = render(&app, 100, 30);
    assert!(
        s.contains("─ progress (refresh failed: request timed out) ─"),
        "{s}"
    );
    assert!(
        row_has(&s, &["1", "start", "coding"]) && s.contains("Repo      /home/dev"),
        "{s}"
    );
    answer(&mut app, BUSY, Ok(busy_detail()));
    let s = render(&app, 100, 30);
    assert!(
        !s.contains("refresh failed") && s.contains("─ progress ─"),
        "{s}"
    );
}

#[test]
fn an_answer_for_a_task_the_view_has_left_is_ignored() {
    let mut app = board();
    open_detail(&mut app, BUSY);
    press(&mut app, KeyCode::Esc);
    open_detail(&mut app, "b81e0d44-cccc");
    answer(&mut app, BUSY, Ok(busy_detail()));
    answer(&mut app, BUSY, Err("late failure".into()));
    let View::Detail(d) = &app.view else { panic!() };
    assert!(d.data.is_none() && d.error.is_none());
}

#[test]
fn a_task_that_left_the_lists_keeps_its_last_answer() {
    let mut app = board();
    open_detail(&mut app, BUSY);
    answer(&mut app, BUSY, Ok(busy_detail()));
    load(&mut app, vec![], vec![]);
    app.view = match std::mem::replace(&mut app.view, View::List) {
        View::Detail(mut d) => {
            d.snapshot = None;
            View::Detail(d)
        }
        v => v,
    };
    let s = render(&app, 100, 30);
    assert!(s.contains("Per-kind stage execution (#55)"), "{s}");
    assert!(s.contains("ID        9c03aa17-bbbb"), "{s}");
    assert!(s.contains("Stage     internal_review"), "{s}");
    assert!(
        s.contains("Repo      /home/dev/chocofactory") && row_has(&s, &["1", "start", "coding"]),
        "{s}"
    );
    // With neither a snapshot nor an answer there is only the notice.
    let mut app = board();
    open_detail(&mut app, BUSY);
    load(&mut app, vec![], vec![]);
    app.view = match std::mem::replace(&mut app.view, View::List) {
        View::Detail(mut d) => {
            d.snapshot = None;
            View::Detail(d)
        }
        v => v,
    };
    assert!(render(&app, 100, 30).contains("(task is no longer listed)"));
}

#[test]
fn a_workflow_without_state_says_the_task_has_not_started() {
    let mut app = board();
    open_detail(&mut app, BUSY);
    let mut v = busy_detail();
    v["workflow_state"] = Value::Null;
    answer(&mut app, BUSY, Ok(v));
    let s = render(&app, 100, 30);
    assert!(
        s.contains("(no workflow state — the task has not started)"),
        "{s}"
    );
}

#[test]
fn a_changed_workflow_and_kept_work_show_as_in_task_status() {
    let mut app = board();
    open_detail(&mut app, BUSY);
    let mut v = busy_detail();
    v["workflow_file_status"] = json!("changed");
    v["kept"] = json!({"worktree_path": "/wt/x", "branch": "task/x"});
    answer(&mut app, BUSY, Ok(v));
    let s = render(&app, 100, 30);
    assert!(
        s.contains("builtin:coding-task (built-in updated since task start)  [0123456789ab]"),
        "{s}"
    );
    assert!(
        s.contains("Kept worktree  /wt/x") && s.contains("Kept branch    task/x"),
        "{s}"
    );
}

// ---- #176: the time column, the poll timeout ----------------------------

#[test]
fn durations_of_a_hundred_days_or_more_are_whole_days() {
    let d = |days: i64, hours: i64, mins: i64| {
        fmt_duration(chrono::Duration::minutes(days * 1440 + hours * 60 + mins))
    };
    assert_eq!(d(99, 23, 59), "99d23h");
    assert_eq!(d(100, 0, 0), "100d");
    assert_eq!(d(123, 4, 0), "123d");
    assert_eq!(d(3, 4, 0), "3d04h");
}

#[test]
fn a_closed_row_123_days_old_shows_123d_ago_uncut() {
    let mut app = board();
    app.closed[0].task.updated_at =
        now() - chrono::Duration::days(123) - chrono::Duration::hours(4);
    let s = render(&app, 120, 30);
    assert!(s.contains("123d ago"), "{s}");
    assert!(!s.contains("123d04h"), "{s}");
}

#[test]
fn a_failed_first_answer_stays_visible_on_a_short_screen() {
    let mut app = board();
    open_detail(&mut app, BUSY);
    answer(
        &mut app,
        BUSY,
        Err("error sending request for url (http://127.0.0.1:7777/tasks/9c03aa17-bbbb): connection refused".into()),
    );
    let s = render(&app, 40, 10);
    assert!(s.contains("could not load the task"), "{s}");
    assert!(!s.contains("earlier step"), "{s}");
}

#[test]
fn a_waiting_task_keeps_its_progress_or_error_on_a_tiny_screen() {
    let id = "3f2a91c0-aaaa";
    let mut app = board();
    open_detail(&mut app, id);
    answer(&mut app, id, Err("connection refused".into()));
    let s = render(&app, 40, 10);
    assert!(s.contains("─ progress"), "{s}");
    assert!(s.contains("could not load the task"), "{s}");
    assert!(s.contains("Stage"), "{s}");

    let mut app = board();
    open_detail(&mut app, id);
    let mut data = busy_detail();
    data["id"] = id.into();
    answer(&mut app, id, Ok(data));
    let s = render(&app, 40, 10);
    assert!(s.contains("─ progress"), "{s}");
    assert!(row_has(&s, &["4", "revi…", "done", "inte…"]), "{s}");
}

#[test]
fn an_open_task_without_a_stage_time_shows_the_bare_stage() {
    let mut app = board();
    app.active
        .iter_mut()
        .find(|t| t.task.id == BUSY)
        .unwrap()
        .stage_entered_at = None;
    open_detail(&mut app, BUSY);
    let s = render(&app, 100, 30);
    assert!(
        s.lines()
            .any(|l| l.trim_end() == "Stage     internal_review"),
        "{s}"
    );
}

#[test]
fn a_very_long_stuck_reason_is_cut_to_three_lines_with_an_ellipsis() {
    let mut app = board();
    let id = "7d22e1a8-dddd";
    app.active
        .iter_mut()
        .find(|t| t.task.id == id)
        .unwrap()
        .task
        .stuck_reason = Some("word ".repeat(60));
    open_detail(&mut app, id);
    let s = render(&app, 60, 30);
    let lines: Vec<&str> = s.lines().map(str::trim_end).collect();
    let i = lines
        .iter()
        .position(|l| l.starts_with("Stuck "))
        .expect(&s);
    assert!(lines[i + 2].ends_with('…'), "{s}");
    assert!(!lines[i + 3].starts_with("          word"), "{s}");
}

#[test]
fn tiny_screens_shrink_wrapped_rows_a_line_at_a_time_and_keep_the_stage_time() {
    let id = "3f2a91c0-aaaa";
    let mut app = board();
    open_detail(&mut app, id);
    answer(&mut app, id, Err("connection refused".into()));
    let s = render(&app, 40, 11);
    let ls: Vec<&str> = s.lines().map(|l| l.trim_end()).collect();
    let i = ls
        .iter()
        .position(|l| l.starts_with("Waiting for"))
        .unwrap_or_else(|| panic!("{s}"));
    assert!(ls[i].ends_with('…'), "{s}");
    assert!(ls[i + 1].starts_with("─ progress"), "{s}");

    // One more row of room: two lines, not one.
    let s = render(&app, 40, 12);
    let ls: Vec<&str> = s.lines().map(|l| l.trim_end()).collect();
    let i = ls
        .iter()
        .position(|l| l.starts_with("Waiting for"))
        .unwrap();
    assert!(!ls[i + 1].starts_with("─ progress"), "{s}");

    // At 40x10 the label column shrinks once Waiting for is gone.
    let s = render(&app, 40, 10);
    assert!(s.contains("awaiting_human_review for 2h"), "{s}");
    assert!(!s.contains("for …"), "{s}");
}

#[test]
fn a_stuck_reason_survives_a_long_error_on_a_tiny_screen() {
    let mut app = board();
    let id = "7d22e1a8-dddd";
    app.active
        .iter_mut()
        .find(|t| t.task.id == id)
        .unwrap()
        .task
        .stuck_reason = Some("word ".repeat(60));
    open_detail(&mut app, id);
    answer(
        &mut app,
        id,
        Err("connection refused: a very long error from the daemon that wraps".into()),
    );
    let s = render(&app, 40, 10);
    assert!(s.contains("Stuck"), "{s}");
    assert!(s.contains("could not load the task"), "{s}");
}

#[test]
fn a_waiting_row_survives_next_to_loop_counters_at_40x12() {
    let id = "3f2a91c0-aaaa";
    let mut app = board();
    open_detail(&mut app, id);
    let mut data = busy_detail();
    data["id"] = id.into();
    answer(&mut app, id, Ok(data));
    let s = render(&app, 40, 12);
    assert!(s.contains("Waiting for"), "{s}");
}

fn busy_with_trail(n: usize) -> Value {
    let mut d = busy_detail();
    let t = d["stage_trail"].as_array().unwrap()[..n].to_vec();
    d["stage_trail"] = Value::Array(t);
    d
}

#[test]
fn a_two_step_progress_list_is_never_cut_to_one_hidden_step() {
    for (w, h) in [(40, 10), (40, 13)] {
        let mut app = board();
        open_detail(&mut app, BUSY);
        answer(&mut app, BUSY, Ok(busy_with_trail(2)));
        let s = render(&app, w, h);
        assert!(!s.contains("earlier step"), "{s}");
        if h == 13 {
            assert!(row_has(&s, &["1", "start", "codi…"]), "{s}");
        }
    }
}

#[test]
fn a_three_step_list_cut_to_one_line_says_two_earlier_steps() {
    let mut app = board();
    open_detail(&mut app, BUSY);
    let mut d = busy_with_trail(3);
    d["workflow_state"]["current_stage"] = "revising".into();
    answer(&mut app, BUSY, Ok(d));
    let s = render(&app, 40, 10);
    assert!(s.contains("  … 2 earlier steps"), "{s}");
    assert!(!s.contains("1 earlier"), "{s}");
}

#[test]
fn a_two_step_list_next_to_wrapped_rows_keeps_the_newest_step() {
    for id in ["3f2a91c0-aaaa", "7d22e1a8-dddd"] {
        for (w, h) in [(40, 10), (40, 11), (40, 12)] {
            let mut app = board();
            open_detail(&mut app, id);
            let mut d = busy_with_trail(2);
            d["id"] = id.into();
            answer(&mut app, id, Ok(d));
            let s = render(&app, w, h);
            assert!(
                row_has(&s, &["2", "codi…", "done", "inte…"]),
                "{id} {w}x{h}\n{s}"
            );
            assert!(!s.contains("earlier step"), "{id} {w}x{h}\n{s}");
        }
    }
}

// ---- cost -----------------------------------------------------------------

#[test]
fn the_cost_column_shows_only_from_width_91_and_leaves_the_other_columns() {
    let mut app = board();
    give_usage(&mut app, "9c03aa17-bbbb", 1.234, "api_equivalent");
    for w in [90, 80] {
        let s = render(&app, w, 30);
        assert!(!s.contains("cost") && !s.contains("≈$"), "{w}\n{s}");
        // Everything present at 80 before the cost column existed.
        for want in [
            "id", "project", "title", "stage", "PR", "laps", "#171", "×2", "2h05m",
        ] {
            assert!(s.contains(want), "{w}: missing {want:?}\n{s}");
        }
    }
    let s = render(&app, 91, 30);
    assert!(s.contains("cost") && s.contains("≈$1.23"), "{s}");
}

#[test]
fn the_cost_cell_says_no_data_or_unknown() {
    let mut app = board();
    let s = render(&app, 120, 30);
    assert!(!s.contains("≈$"), "{s}");
    assert_eq!(s.matches("no data").count(), 5, "{s}");
    app.active[0].usage_total = Some(UsageTotal {
        cost_usd: None,
        tokens: None,
        billing_label: "estimated".into(),
        turns_without_cost: 0,
    });
    let s = render(&app, 120, 30);
    assert!(s.contains("unknown"), "{s}");
}

#[test]
fn every_width_keeps_id_title_and_time_and_the_title_never_loses_room_to_cost() {
    let mut app = board();
    for t in app.active.iter_mut().chain(app.closed.iter_mut()) {
        t.task.title = "Q".repeat(150);
    }
    for id in [
        "3f2a91c0-aaaa",
        "9c03aa17-bbbb",
        "b81e0d44-cccc",
        "7d22e1a8-dddd",
        "61b0f2e3-eeee",
    ] {
        give_usage(&mut app, id, 1.234, "api_equivalent");
    }
    let title_chars = |app: &App, w: u16| -> usize {
        let s = render(app, w, 30);
        let line = s
            .lines()
            .find(|l| l.contains("3f2a91c0"))
            .unwrap_or_else(|| panic!("no row at {w}\n{s}"));
        line.matches('Q').count()
    };
    let at_80 = title_chars(&app, 80);
    for w in 40..=140u16 {
        let s = render(&app, w, 30);
        for line in s.lines() {
            assert!(line.trim_end().chars().count() <= w as usize, "{w}\n{s}");
        }
        assert_eq!(s.contains("cost"), w >= 91, "{w}\n{s}");
        assert_eq!(s.contains("≈$1.23"), w >= 91, "{w}\n{s}");
        for want in [
            "3f2a91c0", "9c03aa17", "b81e0d44", "7d22e1a8", "61b0f2e3", "Q", "2h05m",
        ] {
            assert!(s.contains(want), "{w}: missing {want:?}\n{s}");
        }
        let q = title_chars(&app, w);
        if w >= 91 {
            assert!(q >= at_80, "{w}: title {q} < {at_80}");
        }
    }
}

fn usage_data(id: &str) -> Value {
    let mut d = busy_detail();
    d["id"] = id.into();
    d["usage"] = json!({
        "cost_usd": 0.09, "billing_label": "api_equivalent",
        "tokens": {"input": 30, "output": 15, "cache_read": 300, "cache_write": 60},
        "wall_time_ms": 7_500_000, "active_time_ms": 4_200_000,
        "sessions_without_data": 0,
        "by_stage": [], "by_role": [], "by_lap": [], "by_model": [],
    });
    d
}

#[test]
fn the_detail_view_shows_the_cost_row() {
    let mut app = board();
    open_detail(&mut app, BUSY);
    answer(&mut app, BUSY, Ok(usage_data(BUSY)));
    let s = render(&app, 100, 30);
    assert!(
        s.lines()
            .any(|l| l.trim_end()
                == "Cost      ≈ $0.09 (API-equivalent) · wall 2h05m · active 1h10m"),
        "{s}"
    );

    let mut app = board();
    open_detail(&mut app, BUSY);
    let mut d = usage_data(BUSY);
    d["usage"]["active_time_ms"] = Value::Null;
    d["usage"]["billing_label"] = "estimated".into();
    answer(&mut app, BUSY, Ok(d));
    let s = render(&app, 100, 30);
    assert!(
        s.contains("≈ $0.09 (estimated) · wall 2h05m · active no data"),
        "{s}"
    );

    let mut app = board();
    open_detail(&mut app, BUSY);
    answer(&mut app, BUSY, Ok(busy_detail()));
    let s = render(&app, 100, 30);
    assert!(
        s.lines().any(|l| l.trim_end() == "Cost      no data"),
        "{s}"
    );
}

#[test]
fn partial_totals_are_marked_in_the_dashboard() {
    // Detail: sessions without data and turns without a cost.
    let mut app = board();
    open_detail(&mut app, BUSY);
    let mut d = usage_data(BUSY);
    d["usage"]["sessions_without_data"] = 2.into();
    d["usage"]["turns_without_cost"] = 1.into();
    answer(&mut app, BUSY, Ok(d));
    let s = render(&app, 140, 30);
    assert!(
        s.contains("(API-equivalent)  (2 sessions without data)  (1 turn without a cost) · wall"),
        "{s}"
    );
    // Snapshot-only detail and the list cell mark unknown-cost turns.
    let mut app = board();
    give_usage(&mut app, BUSY, 1.234, "estimated");
    app.active
        .iter_mut()
        .find(|t| t.task.id == BUSY)
        .unwrap()
        .usage_total
        .as_mut()
        .unwrap()
        .turns_without_cost = 3;
    let s = render(&app, 120, 30);
    assert!(s.contains("≈$1.23+"), "{s}");
    open_detail(&mut app, BUSY);
    let s = render(&app, 140, 30);
    assert!(s.contains("(3 turns without a cost)"), "{s}");
}

#[test]
fn before_the_detail_arrives_the_cost_row_comes_from_the_snapshot() {
    let mut app = board();
    give_usage(&mut app, BUSY, 1.234, "estimated");
    open_detail(&mut app, BUSY);
    let s = render(&app, 100, 30);
    assert!(s.contains("Cost      ≈ $1.23 (estimated)"), "{s}");

    let mut app = board();
    open_detail(&mut app, BUSY);
    let s = render(&app, 100, 30);
    assert!(s.contains("Cost      no data"), "{s}");
}

#[test]
fn the_cost_row_is_dropped_with_the_role_row_at_every_height() {
    let mut app = board();
    open_detail(&mut app, BUSY);
    answer(&mut app, BUSY, Ok(usage_data(BUSY)));
    push_events(&mut app, BUSY, 7);
    for h in 10..=40u16 {
        let s = render(&app, 100, h);
        assert_eq!(s.contains("Role "), s.contains("Cost "), "height {h}\n{s}");
    }
    assert!(render(&app, 100, 40).contains("Cost "));
    assert!(!render(&app, 100, 14).contains("Cost "));
    for w in [40, 60, 100] {
        for h in 10..=40u16 {
            let _ = render(&app, w, h);
        }
    }
    // A long, mixed-width trail and a long event stream, at every size.
    let long = long_view(34, 60, false);
    for w in [40, 60, 100] {
        for h in 10..=40u16 {
            assert_detail_invariants(&long, w, h, false);
        }
    }
}

// ---- the progress table, accent colour and polish (#211, #187) -----------

/// A detail whose trail has `n` steps with mixed-width stage names. Steps
/// 1..=5 are dated the day before `now()`, the rest are today. When `stale`,
/// the current stage is not the trail's last one.
fn long_detail(n: usize, stale: bool) -> Value {
    const STAGES: [&str; 5] = [
        "coding",
        "internal_review",
        "awaiting_human_review",
        "ci",
        "revising",
    ];
    let mut d = busy_detail();
    let trail: Vec<Value> = (1..=n)
        .map(|i| {
            json!({
                "created_at": step_iso(i),
                "payload": {
                    "stage": STAGES[(i - 1) % 5],
                    "outcome": if i == 1 { Value::Null } else { json!("done") },
                },
            })
        })
        .collect();
    d["stage_trail"] = Value::Array(trail);
    d["workflow_state"]["current_stage"] = json!(if stale {
        "somewhere_else"
    } else {
        STAGES[(n - 1) % 5]
    });
    d
}

fn step_iso(i: usize) -> String {
    if i <= 5 {
        format!("2025-12-31T{:02}:00:00Z", 18 + i)
    } else {
        format!("2026-01-01T01:{:02}:00Z", i % 60)
    }
}

/// The time cell the table prints for step `i` of [`long_detail`].
fn step_time(i: usize) -> String {
    if i <= 5 {
        format!("2025-12-31 {:02}:00:00", 18 + i)
    } else {
        format!("01:{:02}:00", i % 60)
    }
}

fn long_view(n: usize, events: usize, stale: bool) -> App {
    let mut app = board();
    open_detail(&mut app, BUSY);
    answer(&mut app, BUSY, Ok(long_detail(n, stale)));
    push_events(&mut app, BUSY, events);
    app
}

/// The progress table's step rows on a screen: the lines whose first token
/// is a step number or the `→` of a stale trail.
fn step_rows(s: &str) -> Vec<&str> {
    s.lines()
        .map(str::trim_end)
        .filter(|l| {
            l.split_whitespace().next().is_some_and(|t| {
                t == "→" || (t.chars().all(|c| c.is_ascii_digit()) && l.starts_with("  "))
            })
        })
        .collect()
}

fn render_buf(app: &App, w: u16, h: u16) -> Buffer {
    let mut terminal = Terminal::new(TestBackend::new(w, h)).unwrap();
    terminal.draw(|f| draw(f, app)).unwrap();
    terminal.backend().buffer().clone()
}

fn buf_lines(buf: &Buffer) -> Vec<String> {
    screen(buf)
        .lines()
        .map(|l| l.trim_end().to_string())
        .collect()
}

fn line_index(lines: &[String], starts: &str) -> usize {
    lines
        .iter()
        .position(|l| l.starts_with(starts))
        .unwrap_or_else(|| panic!("no line starting {starts:?}:\n{}", lines.join("\n")))
}

#[test]
fn the_80x24_status_view_shows_the_aligned_table() {
    let app = busy_view();
    let expected = [
        title_row(
            "Per-kind stage execution (#55)",
            "e events · Esc back · ? help",
            80,
        ),
        "ID        9c03aa17-bbbb".into(),
        "Project   chocofactory".into(),
        "Workflow  coding-task".into(),
        "Workflow  builtin:coding-task  [0123456789ab]".into(),
        "Status    open".into(),
        "Repo      /home/dev/chocofactory".into(),
        "Role      coder: model=opus".into(),
        "Created   2026-01-01 03:40:00 UTC".into(),
        "Stage     internal_review for 12m".into(),
        "Cost      no data".into(),
        sep("progress", 80),
        "  #  from             outcome            to               at (UTC)".into(),
        "  1                   start              coding           03:41:00".into(),
        "  2  coding           done               internal_review  04:10:00".into(),
        "  3  internal_review  changes_requested  revising         04:50:00".into(),
        "  4  revising         done               internal_review  11:48:00  ◀ current".into(),
        "Loop counters  internal_review=2".into(),
        sep("last events (e expands)", 80),
        format!("  {}  human_message message 3", at(3)),
        format!("  {}  human_message message 4", at(4)),
        format!("  {}  human_message message 5", at(5)),
        format!("  {}  human_message message 6", at(6)),
        "e events  o PR  r retry  p pause  s send  c cancel  Esc back".into(),
    ]
    .join("\n");
    assert_screen(&render(&app, 80, 24), &expected);
}

/// #187: the tail used to stop at five events and leave the rest blank.
#[test]
fn the_event_tail_fills_a_tall_screen() {
    let app = long_view(4, 40, false);
    let s = render(&app, 120, 40);
    let lines: Vec<&str> = s.lines().collect();
    let i = lines
        .iter()
        .position(|l| l.starts_with("─ last events"))
        .expect(&s);
    let shown = &lines[i + 1..lines.len() - 1];
    assert!(shown.len() > 5, "{s}");
    for (n, l) in shown.iter().enumerate() {
        assert!(l.contains("human_message message"), "line {n}: {l:?}\n{s}");
    }
    // The newest event is the last line above the footer.
    assert!(shown.last().unwrap().contains("message 39"), "{s}");
}

#[test]
fn at_80x16_progress_wins_over_role_and_created() {
    let app = long_view(14, 30, false);
    let s = render(&app, 80, 16);
    let lines: Vec<&str> = s.lines().map(str::trim_end).collect();
    assert!(
        !lines
            .iter()
            .any(|l| l.starts_with("Role ") || l.starts_with("Created ")),
        "{s}"
    );
    // Fields if Role and Created were kept: 10 rows (ID, Project, two
    // Workflow, Status, Repo, Role, Created, Stage, Cost).
    let old_budget = 15 - 1 - 10 - 3;
    assert!(step_rows(&s).len() > old_budget, "{s}");
    assert!(lines.iter().any(|l| l.contains("earlier steps")), "{s}");
    // No blank body line: the counters line sits right above the footer,
    // and the `◀ current` row right above it.
    assert!(lines[14].starts_with("Loop counters"), "{s}");
    assert!(lines[13].ends_with("◀ current"), "{s}");
}

#[test]
fn at_60_columns_stage_names_are_cut_but_every_time_stays() {
    let app = long_view(8, 0, false);
    let s = render(&app, 60, 40);
    let rows = step_rows(&s);
    assert_eq!(rows.len(), 8, "{s}");
    assert!(rows.iter().any(|r| r.contains('…')), "{s}");
    for r in rows {
        let n: usize = r.split_whitespace().next().unwrap().parse().unwrap();
        assert!(r.contains(&step_time(n)), "{r:?}\n{s}");
    }
}

#[test]
fn section_headers_share_one_accent_and_nothing_else_does() {
    use ratatui::style::{Color, Modifier};
    let mut app = busy_view();
    app.color = true;
    let accent_rows = |lines: &[String], starts: &[&str]| -> Vec<usize> {
        starts.iter().map(|p| line_index(lines, p)).collect()
    };

    // Status view.
    let buf = render_buf(&app, 100, 30);
    let lines = buf_lines(&buf);
    let accent = accent_rows(&lines, &["Per-kind", "─ progress", "─ last events"]);
    for &y in &accent {
        for x in 0..100u16 {
            assert_eq!(buf[(x, y as u16)].fg, Color::Cyan, "cell {x},{y}");
        }
    }
    assert!(buf[(0, 0)].modifier.contains(Modifier::BOLD));
    let hy = lines.iter().position(|l| l.contains("at (UTC)")).unwrap();
    for x in 0..100u16 {
        let cell = &buf[(x, hy as u16)];
        if cell.symbol() != " " {
            assert_eq!(cell.fg, Color::DarkGray, "header cell {x}");
        }
    }
    for y in 0..30u16 {
        for x in 0..100u16 {
            if !accent.contains(&(y as usize)) {
                assert_ne!(buf[(x, y)].fg, Color::Cyan, "stray accent at {x},{y}");
            }
        }
    }

    // Expanded view.
    ch(&mut app, 'e');
    let buf = render_buf(&app, 100, 30);
    let lines = buf_lines(&buf);
    let accent = accent_rows(&lines, &["Per-kind", "─ events (following)"]);
    for &y in &accent {
        for x in 0..100u16 {
            assert_eq!(buf[(x, y as u16)].fg, Color::Cyan, "cell {x},{y}");
        }
    }
    for y in 0..30u16 {
        for x in 0..100u16 {
            if !accent.contains(&(y as usize)) {
                assert_ne!(buf[(x, y)].fg, Color::Cyan, "stray accent at {x},{y}");
            }
        }
    }
}

#[test]
fn without_colour_no_cell_of_the_detail_view_has_a_foreground() {
    use ratatui::style::Color;
    let mut app = busy_view();
    app.color = false;
    for expanded in [false, true] {
        if expanded {
            ch(&mut app, 'e');
        }
        let buf = render_buf(&app, 100, 30);
        for y in 0..30u16 {
            for x in 0..100u16 {
                assert_eq!(buf[(x, y)].fg, Color::Reset, "cell {x},{y}");
            }
        }
    }
}

#[test]
fn error_separators_are_red_not_accent() {
    use ratatui::style::Color;
    let check = |buf: &Buffer, starts: &str| {
        let lines = buf_lines(buf);
        let y = line_index(&lines, starts) as u16;
        for x in 0..buf.area.width {
            assert_eq!(buf[(x, y)].fg, Color::Red, "{starts} cell {x}");
        }
    };
    let mut app = busy_view();
    app.color = true;
    update(
        &mut app,
        Msg::Socket {
            id: BUSY.into(),
            msg: SocketMsg::Down,
        },
    );
    check(&render_buf(&app, 100, 30), "─ events: reconnecting");
    ch(&mut app, 'e');
    check(&render_buf(&app, 100, 30), "─ events: reconnecting");

    let mut app = busy_view();
    app.color = true;
    answer(&mut app, BUSY, Err("request timed out".into()));
    check(&render_buf(&app, 100, 30), "─ progress (refresh failed");
}

#[test]
fn stuck_reasons_wrap_at_word_boundaries() {
    let reason =
        "the agent hit 'overloaded_error' and the retry budget is exhausted after three attempts";
    let mut app = board();
    let id = "7d22e1a8-dddd";
    app.active
        .iter_mut()
        .find(|t| t.task.id == id)
        .unwrap()
        .task
        .stuck_reason = Some(reason.into());
    open_detail(&mut app, id);
    let s = render(&app, 60, 24);
    let lines: Vec<&str> = s.lines().map(str::trim_end).collect();
    let i = lines
        .iter()
        .position(|l| l.starts_with("Stuck "))
        .expect(&s);
    let mut words = Vec::new();
    let mut n = 0;
    for l in &lines[i..] {
        if n > 0 && !l.starts_with("          ") {
            break;
        }
        words.extend(l.split_whitespace().skip(usize::from(n == 0)));
        n += 1;
    }
    assert!(n >= 2, "the reason should wrap here:\n{s}");
    assert_eq!(words, reason.split_whitespace().collect::<Vec<_>>(), "{s}");
}

#[test]
fn a_token_wider_than_the_line_is_cut_into_consecutive_chunks() {
    let token = "abcdefghij".repeat(15);
    let mut app = board();
    let id = "7d22e1a8-dddd";
    app.active
        .iter_mut()
        .find(|t| t.task.id == id)
        .unwrap()
        .task
        .stuck_reason = Some(token.clone());
    open_detail(&mut app, id);
    let s = render(&app, 80, 24);
    let lines: Vec<&str> = s.lines().map(str::trim_end).collect();
    let i = lines
        .iter()
        .position(|l| l.starts_with("Stuck "))
        .expect(&s);
    let mut joined = String::new();
    for (k, l) in lines[i..].iter().enumerate() {
        if k > 0 && !l.starts_with("          ") {
            break;
        }
        joined.push_str(&l[10..]);
    }
    let joined = joined.strip_suffix('…').unwrap_or(&joined);
    assert!(joined.len() > 68, "should span several lines:\n{s}");
    assert_eq!(joined, &token[..joined.len()], "{s}");
}

#[test]
fn the_expanded_view_says_esc_goes_back_to_the_status_view() {
    let mut app = busy_view();
    ch(&mut app, 'e');
    for w in [80u16, 100, 140] {
        let lines = buf_lines(&render_buf(&app, w, 24));
        assert!(lines[0].contains("e/Esc status"), "{}", lines[0]);
        assert!(!lines[0].contains("Esc back"), "{}", lines[0]);
        assert!(lines[23].contains("Esc status"), "{}", lines[23]);
    }
}

/// What every size must show: the progress separator, the current row, and
/// nothing wider than the terminal; times in full from 60 columns up.
fn assert_detail_invariants(app: &App, w: u16, h: u16, stale: bool) {
    assert_detail_invariants_for(app, w, h, stale, 34);
}

/// [`assert_detail_invariants`] for a trail of `last` steps.
fn assert_detail_invariants_for(app: &App, w: u16, h: u16, stale: bool, last_step: usize) {
    let s = render(app, w, h);
    let lines: Vec<&str> = s.lines().map(str::trim_end).collect();
    let at = format!("{w}x{h}\n{s}");
    assert!(lines.iter().any(|l| l.starts_with("─ progress")), "{at}");
    let rows = step_rows(&s);
    let last = rows.last().unwrap_or_else(|| panic!("no step rows {at}"));
    let first = last.split_whitespace().next().unwrap();
    let want = last_step.to_string();
    assert_eq!(first, if stale { "→" } else { &want }, "{at}");
    if w >= 60 {
        // Narrower than that a dated row can outgrow the screen, and the
        // right edge (the marker) is cut.
        assert!(last.contains('◀'), "{at}");
    }
    for l in &lines {
        assert!(l.chars().count() <= w as usize, "too wide: {l:?}\n{at}");
    }
    if w >= 60 {
        for r in rows.iter().filter(|r| !r.trim_start().starts_with('→')) {
            let n: usize = r.split_whitespace().next().unwrap().parse().unwrap();
            assert!(r.contains(&step_time(n)), "{r:?}\n{at}");
        }
    }
}

#[test]
fn the_detail_view_keeps_its_invariants_at_every_size() {
    for stale in [false, true] {
        let app = long_view(34, 60, stale);
        for w in 40..=140u16 {
            for h in 10..=40u16 {
                assert_detail_invariants(&app, w, h, stale);
            }
        }
    }
}

/// Dropping Role (with Cost) can free several lines at once; the freed
/// lines go to the events tail rather than to blank space.
#[test]
fn lines_freed_by_dropping_rows_go_to_the_event_tail() {
    let mut app = board();
    open_detail(&mut app, BUSY);
    let mut d = busy_detail();
    d["config"]["roles"] = json!({"coder": {"model": "opus"}, "reviewer": {"model": "sonnet"}});
    answer(&mut app, BUSY, Ok(d));
    push_events(&mut app, BUSY, 7);
    // Everything: title 1 + 11 field rows (two Role rows) + separator +
    // header + 4 steps + counters = 19 lines, plus the footer.
    let s = render(&app, 100, 20);
    assert!(s.contains("Role      reviewer"), "{s}");
    assert!(!s.contains("last events"), "{s}");
    // One line short: Role x2 and Cost (3 lines) go, and two of the freed
    // lines become the tail's separator and one event.
    let s = render(&app, 100, 19);
    assert!(!s.contains("Role ") && !s.contains("Cost "), "{s}");
    assert!(s.contains("Created"), "{s}");
    let lines: Vec<&str> = s.lines().collect();
    let i = lines
        .iter()
        .position(|l| l.starts_with("─ last events"))
        .expect(&s);
    assert!(lines[i + 1].contains("message 6"), "{s}");
    assert!(lines[i + 2].starts_with("e events"), "{s}");
}

#[test]
fn a_long_token_after_words_starts_its_own_line() {
    let token = "abcdefghij".repeat(10);
    let mut app = board();
    let id = "7d22e1a8-dddd";
    app.active
        .iter_mut()
        .find(|t| t.task.id == id)
        .unwrap()
        .task
        .stuck_reason = Some(format!("failed at {token} again"));
    open_detail(&mut app, id);
    let s = render(&app, 80, 24);
    let lines: Vec<&str> = s.lines().map(str::trim_end).collect();
    let i = lines
        .iter()
        .position(|l| l.starts_with("Stuck "))
        .expect(&s);
    assert_eq!(lines[i], "Stuck     failed at", "{s}");
    assert_eq!(lines[i + 1], format!("          {}", &token[..70]), "{s}");
    // The last chunk is the current line, so the next word joins it.
    assert_eq!(
        lines[i + 2],
        format!("          {} again", &token[70..]),
        "{s}"
    );
}

/// Review of #211: with loop counters, a short screen drops the header row
/// before it hides a step, and the counters line stays.
#[test]
fn a_short_screen_drops_the_header_row_and_keeps_the_counters() {
    let mut app = board();
    open_detail(&mut app, BUSY);
    answer(&mut app, BUSY, Ok(busy_with_trail(4)));
    let s = render(&app, 40, 11);
    assert!(!s.contains("at (UTC)"), "{s}");
    assert!(s.contains("  … 3 earlier steps"), "{s}");
    assert!(s.contains("Loop counters  internal_review=2"), "{s}");
    assert!(row_has(&s, &["4", "revi…", "done"]), "{s}");
}

/// Review of #211: a two-step trail without counters shows both steps at
/// every height, never "… 1 earlier steps".
#[test]
fn a_two_step_list_without_counters_is_never_cut() {
    for h in 10..=14 {
        let mut app = board();
        open_detail(&mut app, BUSY);
        let mut d = busy_with_trail(2);
        d["workflow_state"]["loop_counters"] = json!({});
        answer(&mut app, BUSY, Ok(d));
        let s = render(&app, 40, h);
        assert!(!s.contains("earlier step"), "40x{h}\n{s}");
        assert!(row_has(&s, &["2", "codi…", "done"]), "40x{h}\n{s}");
        assert!(row_has(&s, &["1", "start"]), "40x{h}\n{s}");
    }
}

#[test]
fn the_120x40_status_view_shows_the_aligned_table() {
    let app = busy_view();
    let mut expected = vec![
        title_row(
            "Per-kind stage execution (#55)",
            "e events · Esc back · ? help",
            120,
        ),
        "ID        9c03aa17-bbbb".to_string(),
        "Project   chocofactory".into(),
        "Workflow  coding-task".into(),
        "Workflow  builtin:coding-task  [0123456789ab]".into(),
        "Status    open".into(),
        "Repo      /home/dev/chocofactory".into(),
        "Role      coder: model=opus".into(),
        "Created   2026-01-01 03:40:00 UTC".into(),
        "Stage     internal_review for 12m".into(),
        "Cost      no data".into(),
        sep("progress", 120),
        "  #  from             outcome            to               at (UTC)".into(),
        "  1                   start              coding           03:41:00".into(),
        "  2  coding           done               internal_review  04:10:00".into(),
        "  3  internal_review  changes_requested  revising         04:50:00".into(),
        "  4  revising         done               internal_review  11:48:00  ◀ current".into(),
        "Loop counters  internal_review=2".into(),
        sep("last events (e expands)", 120),
    ];
    for n in 0..7 {
        expected.push(format!("  {}  human_message message {n}", at(n)));
    }
    let s = render(&app, 120, 40);
    let lines: Vec<&str> = s.lines().map(str::trim_end).collect();
    for (i, e) in expected.iter().enumerate() {
        assert_eq!(&lines[i], e, "line {i}\n{s}");
    }
    assert_eq!(
        lines[39], "e events  o PR  r retry  p pause  s send  c cancel  Esc back",
        "{s}"
    );
}

#[test]
fn a_stuck_row_with_an_empty_reason_still_renders() {
    let mut app = board();
    let id = "7d22e1a8-dddd";
    app.active
        .iter_mut()
        .find(|t| t.task.id == id)
        .unwrap()
        .task
        .stuck_reason = Some(String::new());
    open_detail(&mut app, id);
    let s = render(&app, 80, 24);
    assert!(s.lines().any(|l| l.starts_with("Stuck")), "{s}");
}

// ---- parallel groups (#257, PG1-7) ---------------------------------------

fn progress_of(settled: u32, total: u32) -> Option<chocofactory_core::models::BranchProgress> {
    Some(chocofactory_core::models::BranchProgress { settled, total })
}

#[test]
fn the_stage_cell_reads_settled_over_total_in_a_group() {
    let mut app = board();
    app.active
        .iter_mut()
        .find(|t| t.task.id == BUSY)
        .unwrap()
        .branch_progress = progress_of(2, 3);
    for w in [80u16, 100, 140] {
        let s = render(&app, w, 24);
        assert!(s.contains("internal_review 2/3"), "{w}\n{s}");
    }
}

#[test]
fn the_stage_cell_is_the_plain_stage_without_branch_progress() {
    let app = board();
    let s = render(&app, 100, 24);
    assert!(s.contains("internal_review"), "{s}");
    assert!(!s.contains("internal_review 0/"), "{s}");
    assert!(!s.contains("internal_review 2/"), "{s}");
}

#[test]
fn a_summary_from_an_older_daemon_has_no_branch_progress() {
    let mut v =
        serde_json::to_value(summary("t1", "T", "open", Some("coding"), Some(5), "p")).unwrap();
    v.as_object_mut().unwrap().remove("branch_progress");
    let t: TaskSummary = serde_json::from_value(v).unwrap();
    assert_eq!(t.branch_progress, None);
    let mut app = new_app(Scope::AllProjects);
    load(&mut app, vec![t], vec![]);
    let s = render(&app, 100, 24);
    assert!(s.contains("coding"), "{s}");
}

/// `long_detail` made a parallel group's detail: three branches.
fn group_detail(n: usize) -> Value {
    let mut d = long_detail(n, false);
    d["workflow_state"]["branches"] = json!([
        {"name": "security_review", "kind": "agent_turn", "state": "running",
         "result": null, "reason": null, "entry": 2,
         "started_at": "2026-01-01T11:50:00Z", "ended_at": null},
        {"name": "architecture_review", "kind": "agent_turn", "state": "done",
         "result": "blocking", "reason": null, "entry": 2,
         "started_at": "2026-01-01T11:50:00Z", "ended_at": "2026-01-01T11:55:00Z"},
        {"name": "tests", "kind": "agent_turn", "state": "failed",
         "result": null, "reason": "the suite did not compile", "entry": 2,
         "started_at": "2026-01-01T11:50:00Z", "ended_at": "2026-01-01T11:52:00Z"},
    ]);
    d
}

fn group_view(n: usize, events: usize) -> App {
    let mut app = board();
    open_detail(&mut app, BUSY);
    answer(&mut app, BUSY, Ok(group_detail(n)));
    push_events(&mut app, BUSY, events);
    app
}

/// The branch block of a screen: `None` when absent, else the index of its
/// separator, after checking its header and rows follow it in order.
fn branch_block_at(s: &str) -> Option<usize> {
    let lines: Vec<&str> = s.lines().map(str::trim_end).collect();
    let progress = lines.iter().position(|l| l.starts_with("─ progress"))?;
    let Some(at) = lines.iter().position(|l| l.starts_with("─ branches")) else {
        assert!(
            !lines.iter().any(|l| l.contains("security_review")
                || l.contains("architecture_review")
                || l.trim_start().starts_with("branch ")),
            "part of the branch block is on screen:\n{s}"
        );
        return None;
    };
    assert!(at + 4 < progress, "block must sit above progress:\n{s}");
    assert!(lines[at + 1].trim_start().starts_with("branch "), "{s}");
    assert!(lines[at + 2].contains("security_review"), "{s}");
    assert!(lines[at + 3].contains("architecture_review"), "{s}");
    assert!(lines[at + 4].trim_start().starts_with("tests "), "{s}");
    Some(at)
}

#[test]
fn the_detail_view_shows_the_branch_table_above_progress() {
    let app = group_view(6, 3);
    let s = render(&app, 120, 40);
    assert!(branch_block_at(&s).is_some(), "{s}");
    assert!(s.contains("the suite did not compile"), "{s}");
    assert!(s.contains("5m"), "{s}");
}

#[test]
fn the_detail_view_keeps_its_invariants_with_a_parallel_group() {
    let app = group_view(16, 60);
    let mut seen_present = false;
    for w in 40..=140u16 {
        for h in 10..=40u16 {
            assert_detail_invariants_for(&app, w, h, false, 16);
            seen_present |= branch_block_at(&render(&app, w, h)).is_some();
        }
    }
    assert!(seen_present);
    assert!(
        branch_block_at(&render(&app, 120, 40)).is_some(),
        "the block is present at 120x40"
    );
}

#[test]
fn the_branch_table_goes_before_any_older_progress_step() {
    let app = group_view(16, 0);
    let mut dropped_with_all_steps = false;
    for h in 10..=40u16 {
        let s = render(&app, 120, h);
        let steps = step_rows(&s).len();
        if branch_block_at(&s).is_some() {
            assert_eq!(
                steps, 16,
                "the block stayed while steps were cut at h={h}\n{s}"
            );
        } else if steps == 16 {
            dropped_with_all_steps = true;
        }
    }
    assert!(dropped_with_all_steps, "no height drops only the block");
}

#[test]
fn a_stuck_groups_stage_cell_does_not_shrink_the_title_column() {
    let plain = render(&board(), 80, 24);
    let mut app = board();
    app.active
        .iter_mut()
        .find(|t| t.task.id == "7d22e1a8-dddd")
        .unwrap()
        .branch_progress = progress_of(2, 3);
    let grouped = render(&app, 80, 24);
    let busy = |s: &str| {
        s.lines()
            .find(|l| l.contains("9c03aa17"))
            .unwrap_or_else(|| panic!("{s}"))
            .to_string()
    };
    assert_eq!(busy(&plain), busy(&grouped));
}

// ---- search popup (#292) -------------------------------------------------

const ANCIENT: &str = "ffff9999-0";

/// The board's tasks plus one old closed task that is not on the board.
fn search_tasks(app: &App) -> Vec<TaskSummary> {
    let mut all: Vec<TaskSummary> = app
        .active
        .iter()
        .chain(app.closed.iter())
        .cloned()
        .collect();
    let mut old = with_pr(
        summary(
            ANCIENT,
            "Ancient migration fix",
            "closed",
            None,
            None,
            "p-alpha",
        ),
        77,
    );
    old.task.created_at = mins_ago(90_000);
    all.push(old);
    for (i, t) in all.iter_mut().enumerate().take(5) {
        t.task.created_at = mins_ago(100 + i as i64 * 10);
    }
    all
}

fn key_with(app: &mut App, code: KeyCode, mods: KeyModifiers) -> Vec<Effect> {
    update(app, Msg::Key(KeyEvent::new(code, mods)))
}

fn open_search(app: &mut App) -> u64 {
    match ch(app, '/').as_slice() {
        [Effect::LoadSearch { generation }] => *generation,
        other => panic!("expected LoadSearch, got {other:?}"),
    }
}

fn deliver(app: &mut App, generation: u64, tasks: Vec<TaskSummary>) {
    update(
        app,
        Msg::Search {
            generation,
            result: Ok(tasks),
        },
    );
}

fn opened(app: &mut App) -> u64 {
    let tasks = search_tasks(app);
    let g = open_search(app);
    deliver(app, g, tasks);
    g
}

fn typed(app: &mut App, s: &str) {
    for c in s.chars() {
        assert!(ch(app, c).is_empty(), "{c:?} must only be typed");
    }
}

fn query(app: &App) -> String {
    app.search.as_ref().unwrap().query.clone()
}

fn sel(app: &App) -> Option<String> {
    app.search.as_ref().unwrap().selected.clone()
}

fn found(app: &App) -> Vec<String> {
    ids(&app.search.as_ref().unwrap().results())
}

fn detail_id(app: &App) -> Option<String> {
    match &app.view {
        View::Detail(d) => Some(d.id.clone()),
        View::List => None,
    }
}

fn into_detail(app: &mut App) {
    press(app, KeyCode::Enter);
    assert!(detail_id(app).is_some());
}

#[test]
fn slash_opens_the_popup_and_esc_closes_it_unchanged_from_the_list() {
    let mut app = board();
    ch(&mut app, 'j');
    ch(&mut app, 'j');
    let (selected, pos) = (app.selected.clone(), app.selected_pos);
    let effects = ch(&mut app, '/');
    assert_eq!(effects, vec![Effect::LoadSearch { generation: 1 }]);
    let s = app.search.as_ref().unwrap();
    assert!(matches!(s.load, SearchLoad::Loading));
    assert!(matches!(app.view, View::List));
    assert!(press(&mut app, KeyCode::Esc).is_empty());
    assert!(app.search.is_none());
    assert!(matches!(app.view, View::List));
    assert_eq!((app.selected.clone(), app.selected_pos), (selected, pos));
}

#[test]
fn slash_then_esc_in_the_detail_view_keeps_the_view_and_the_socket() {
    let mut app = board();
    into_detail(&mut app);
    let id = detail_id(&app);
    assert_eq!(ch(&mut app, '/').len(), 1);
    assert!(press(&mut app, KeyCode::Esc).is_empty());
    assert_eq!(detail_id(&app), id);
    assert!(app.search.is_none());
}

#[test]
fn esc_in_the_popup_keeps_the_boards_status_line() {
    let mut app = board();
    ch(&mut app, '/');
    // A status set while the popup is open (a failed browser launch).
    update(
        &mut app,
        Msg::OpenFailed {
            url: "u".into(),
            error: "e".into(),
        },
    );
    let status = app.status.clone();
    assert!(status.is_some());
    ch(&mut app, 'x');
    press(&mut app, KeyCode::Esc);
    assert_eq!(app.status, status);
}

#[test]
fn slash_does_nothing_in_help_and_only_closes_a_confirmation() {
    let mut app = board();
    ch(&mut app, '?');
    assert!(ch(&mut app, '/').is_empty());
    assert!(app.search.is_none());
    assert!(app.help);
    ch(&mut app, '?');

    let mut app = board();
    // Select the stuck task and ask to cancel it.
    ch(&mut app, 'G');
    ch(&mut app, 'k');
    ch(&mut app, 'c');
    assert!(app.prompt.is_some());
    assert!(ch(&mut app, '/').is_empty());
    assert!(app.prompt.is_none());
    assert!(app.search.is_none());
}

#[test]
fn printable_keys_are_typed_not_acted_on_over_the_list_and_the_detail() {
    for in_detail in [false, true] {
        let mut app = board();
        // A stuck task with a PR selected.
        app.active
            .iter_mut()
            .find(|t| t.task.id == "7d22e1a8-dddd")
            .unwrap()
            .pr = Some(PullRequestRef {
            number: 5,
            url: "https://x/pull/5".into(),
        });
        ch(&mut app, 'G');
        ch(&mut app, 'k');
        if in_detail {
            into_detail(&mut app);
        }
        let before = (
            app.selected.clone(),
            app.selected_pos,
            detail_id(&app),
            matches!(&app.view, View::Detail(d) if d.expanded),
        );
        ch(&mut app, '/');
        let text = "qorcjkgG?e";
        typed(&mut app, text);
        assert_eq!(query(&app), text);
        assert!(app.prompt.is_none() && !app.help);
        let after = (
            app.selected.clone(),
            app.selected_pos,
            detail_id(&app),
            matches!(&app.view, View::Detail(d) if d.expanded),
        );
        assert_eq!(before, after);
        assert_eq!(
            key_with(&mut app, KeyCode::Char('c'), KeyModifiers::CONTROL),
            vec![Effect::Quit]
        );
    }
}

#[test]
fn ctrl_chords_other_than_navigation_and_other_keys_are_ignored() {
    let mut app = board();
    opened(&mut app);
    typed(&mut app, "re");
    for (code, mods) in [
        (KeyCode::Char('x'), KeyModifiers::CONTROL),
        (KeyCode::Char('x'), KeyModifiers::ALT),
        (KeyCode::Tab, KeyModifiers::NONE),
        (KeyCode::BackTab, KeyModifiers::SHIFT),
        (KeyCode::Home, KeyModifiers::NONE),
        (KeyCode::End, KeyModifiers::NONE),
        (KeyCode::Left, KeyModifiers::NONE),
        (KeyCode::Right, KeyModifiers::NONE),
        (KeyCode::Delete, KeyModifiers::NONE),
    ] {
        assert!(key_with(&mut app, code, mods).is_empty());
    }
    assert_eq!(query(&app), "re");
    // Shift is part of typing a capital.
    key_with(&mut app, KeyCode::Char('R'), KeyModifiers::SHIFT);
    assert_eq!(query(&app), "reR");
}

#[test]
fn a_poll_result_does_not_touch_the_popup() {
    let mut app = board();
    opened(&mut app);
    typed(&mut app, "st");
    let g = (query(&app), found(&app), sel(&app));
    assert!(g.1.len() >= 2);
    key_with(&mut app, KeyCode::Down, KeyModifiers::NONE);
    let moved = sel(&app);
    load(
        &mut app,
        vec![summary(
            "zzzz",
            "Brand new",
            "open",
            Some("coding"),
            Some(1),
            "p-beta",
        )],
        vec![],
    );
    assert_eq!((query(&app), found(&app)), (g.0, g.1));
    assert_eq!(sel(&app), moved);
}

#[test]
fn a_stale_search_result_changes_nothing() {
    let mut app = board();
    let tasks = search_tasks(&app);
    let first = open_search(&mut app);
    press(&mut app, KeyCode::Esc);
    let second = open_search(&mut app);
    assert!(second > first);
    deliver(&mut app, first, tasks.clone());
    let s = app.search.as_ref().unwrap();
    assert!(matches!(s.load, SearchLoad::Loading));
    assert_eq!(s.selected, None);

    press(&mut app, KeyCode::Esc);
    let board_sel = (app.selected.clone(), app.selected_pos);
    deliver(&mut app, second, tasks);
    assert!(app.search.is_none());
    assert_eq!((app.selected.clone(), app.selected_pos), board_sel);
}

#[test]
fn a_failed_load_shows_the_error_and_the_board_still_works() {
    let mut app = board();
    let g = open_search(&mut app);
    update(
        &mut app,
        Msg::Search {
            generation: g,
            result: Err("boom".into()),
        },
    );
    let screen = render(&app, 100, 24);
    assert!(screen.contains("could not load tasks: boom"), "{screen}");
    assert!(!screen.contains("no matching tasks"));
    assert!(press(&mut app, KeyCode::Enter).is_empty());
    assert!(app.search.is_some());
    press(&mut app, KeyCode::Esc);
    assert!(app.search.is_none());
    let before = app.selected_pos;
    ch(&mut app, 'j');
    assert_eq!(app.selected_pos, before + 1);
    let again = open_search(&mut app);
    assert!(again > g);
}

#[test]
fn enter_on_a_task_outside_the_closed_window_opens_its_detail() {
    let mut app = board();
    let before = (app.selected.clone(), app.selected_pos);
    opened(&mut app);
    typed(&mut app, "ancient");
    assert_eq!(found(&app), [ANCIENT]);
    assert_eq!(sel(&app).as_deref(), Some(ANCIENT));
    assert_eq!(
        press(&mut app, KeyCode::Enter),
        vec![
            Effect::FetchDetail(ANCIENT.into()),
            Effect::OpenSocket(ANCIENT.into())
        ]
    );
    assert!(app.search.is_none());
    match &app.view {
        View::Detail(d) => assert_eq!(d.snapshot.as_ref().unwrap().task.id, ANCIENT),
        View::List => panic!("expected the detail view"),
    }
    assert_eq!((app.selected.clone(), app.selected_pos), before);
    assert_eq!(
        ch(&mut app, 'o'),
        vec![Effect::OpenUrl("https://github.com/o/r/pull/77".into())]
    );
}

#[test]
fn enter_on_a_task_on_the_board_selects_it_there() {
    let mut app = board();
    opened(&mut app);
    typed(&mut app, "Release");
    assert_eq!(found(&app), ["61b0f2e3-eeee"]);
    press(&mut app, KeyCode::Enter);
    assert_eq!(app.selected.as_deref(), Some("61b0f2e3-eeee"));
    assert_eq!(app.selected_pos, 4);
    assert_eq!(detail_id(&app).as_deref(), Some("61b0f2e3-eeee"));
}

#[test]
fn enter_from_a_detail_replaces_it() {
    let mut app = board();
    into_detail(&mut app);
    opened(&mut app);
    typed(&mut app, "ancient");
    press(&mut app, KeyCode::Enter);
    assert_eq!(detail_id(&app).as_deref(), Some(ANCIENT));
}

#[test]
fn enter_with_no_match_or_while_loading_does_nothing() {
    let mut app = board();
    open_search(&mut app);
    assert!(press(&mut app, KeyCode::Enter).is_empty());
    assert!(app.search.is_some());
    assert!(matches!(app.view, View::List));

    let mut app = board();
    opened(&mut app);
    typed(&mut app, "no such thing");
    assert_eq!(sel(&app), None);
    assert!(press(&mut app, KeyCode::Enter).is_empty());
    assert!(app.search.is_some());
}

#[test]
fn selection_moves_clamps_pages_and_follows_the_query() {
    let mut app = board();
    opened(&mut app);
    let all = found(&app);
    assert_eq!(all.len(), 6);
    assert_eq!(sel(&app).as_ref(), Some(&all[0]));
    // Up at the top clamps; Down, Ctrl-N, Ctrl-P move by one.
    press(&mut app, KeyCode::Up);
    assert_eq!(sel(&app).as_ref(), Some(&all[0]));
    press(&mut app, KeyCode::Down);
    assert_eq!(sel(&app).as_ref(), Some(&all[1]));
    key_with(&mut app, KeyCode::Char('n'), KeyModifiers::CONTROL);
    assert_eq!(sel(&app).as_ref(), Some(&all[2]));
    key_with(&mut app, KeyCode::Char('p'), KeyModifiers::CONTROL);
    assert_eq!(sel(&app).as_ref(), Some(&all[1]));
    // Paging uses the drawn page height, and clamps.
    app.search.as_ref().unwrap().page.set(4);
    press(&mut app, KeyCode::PageDown);
    assert_eq!(sel(&app).as_ref(), Some(&all[5]));
    press(&mut app, KeyCode::Down);
    assert_eq!(sel(&app).as_ref(), Some(&all[5]));
    press(&mut app, KeyCode::PageUp);
    assert_eq!(sel(&app).as_ref(), Some(&all[1]));
    press(&mut app, KeyCode::PageUp);
    assert_eq!(sel(&app).as_ref(), Some(&all[0]));
    // Still a result after typing: stays. Filtered out: the first result.
    press(&mut app, KeyCode::Down);
    let keep = sel(&app).unwrap();
    let title = app
        .search
        .as_ref()
        .unwrap()
        .results()
        .into_iter()
        .find(|t| t.task.id == keep)
        .unwrap()
        .task
        .title
        .clone();
    typed(&mut app, &title[..4]);
    assert_eq!(sel(&app).as_ref(), Some(&keep));
    typed(&mut app, "zz");
    assert_eq!(sel(&app), None);
    // Backspace to a matching query: from none, the first result.
    press(&mut app, KeyCode::Backspace);
    press(&mut app, KeyCode::Backspace);
    assert_eq!(sel(&app).as_ref(), found(&app).first());
    // Backspace on an empty query is a no-op.
    for _ in 0..10 {
        press(&mut app, KeyCode::Backspace);
    }
    assert_eq!(query(&app), "");
    assert_eq!(sel(&app).as_ref(), Some(&keep));
}

#[test]
fn selection_is_the_first_result_when_the_load_arrives_after_typing() {
    let mut app = board();
    let tasks = search_tasks(&app);
    let g = open_search(&mut app);
    typed(&mut app, "re");
    assert_eq!(sel(&app), None);
    deliver(&mut app, g, tasks);
    assert_eq!(sel(&app).as_ref(), found(&app).first());
    assert!(sel(&app).is_some());
}

fn long_fixture() -> Vec<TaskSummary> {
    let statuses = chocofactory_core::models::ALL_TASK_STATUSES;
    (0..12)
        .map(|i| {
            let mut t = summary(
                &format!("id{i:05}-aaaa"),
                &format!("Long title {i} {}", "lorem ipsum dolor ".repeat(8)),
                statuses[i % statuses.len()],
                None,
                None,
                if i % 2 == 0 { "p-alpha" } else { "p-beta" },
            );
            if i % 3 == 0 {
                t = with_pr(t, 1000 + i as u64);
            }
            t.task.created_at = mins_ago(i as i64);
            t
        })
        .collect()
}

#[test]
fn the_popup_fits_its_box_and_shows_the_right_columns_at_every_size() {
    for all_projects in [true, false] {
        for last in [false, true] {
            for h in [10u16, 24, 40] {
                for w in 40u16..=120 {
                    let mut app = new_app(if all_projects {
                        Scope::AllProjects
                    } else {
                        Scope::Project {
                            name: "chocofactory".into(),
                        }
                    });
                    app.projects.insert("p-alpha".into(), "chocofactory".into());
                    app.projects.insert("p-beta".into(), "webshop".into());
                    app.search_gen = 1;
                    let mut s = Search {
                        generation: 1,
                        query: String::new(),
                        load: SearchLoad::Loaded(long_fixture()),
                        selected: None,
                        page: std::cell::Cell::new(10),
                        offset: std::cell::Cell::new(0),
                    };
                    let want = if last { s.results().len() - 1 } else { 0 };
                    s.selected = Some(s.results()[want].task.id.clone());
                    app.search = Some(s);
                    let text = render(&app, w, h);
                    let ctx = format!("{w}x{h} all={all_projects} last={last}\n{text}");
                    let lines: Vec<Vec<char>> = text.lines().map(|l| l.chars().collect()).collect();
                    let top = lines
                        .iter()
                        .position(|l| {
                            l.contains(&'┌') && l.iter().collect::<String>().contains("search")
                        })
                        .unwrap_or_else(|| panic!("no box\n{ctx}"));
                    let x0 = lines[top].iter().position(|c| *c == '┌').unwrap();
                    let x1 = lines[top].iter().position(|c| *c == '┐').unwrap();
                    let bottom = (top + 1..lines.len())
                        .find(|y| lines[*y][x0] == '└')
                        .unwrap_or_else(|| panic!("no bottom\n{ctx}"));
                    for row in &lines[top + 1..bottom] {
                        assert_eq!((row[x0], row[x1]), ('│', '│'), "{ctx}");
                    }
                    // Result rows only: skip input, count and hint lines.
                    let rows: String = lines[top + 3..bottom - 1]
                        .iter()
                        .map(|r| r[x0 + 1..x1].iter().collect::<String>())
                        .collect::<Vec<_>>()
                        .join("\n");
                    assert!(rows.contains("▶ "), "{ctx}");
                    assert_eq!(rows.contains('#'), w >= 80, "{ctx}");
                    let status = ["open", "stuck", "closed", "cancelled"]
                        .iter()
                        .any(|s| rows.contains(s));
                    assert_eq!(status, w >= 60, "{ctx}");
                    let project = rows.contains("chocofactory") || rows.contains("webshop");
                    assert_eq!(project, w >= 60 && all_projects, "{ctx}");
                }
            }
        }
    }
}

#[test]
fn the_popup_says_what_it_is_doing() {
    let mut app = board();
    let tasks = search_tasks(&app);
    let g = open_search(&mut app);
    assert!(render(&app, 100, 24).contains("loading…"));
    deliver(&mut app, g, tasks);
    let screen = render(&app, 100, 24);
    assert!(screen.contains("6 of 6 tasks"), "{screen}");
    typed(&mut app, "st");
    assert!(render(&app, 100, 24).contains("2 of 6 tasks"));
    assert!(render(&app, 100, 24).contains("/ st_"));
    typed(&mut app, "qqqq");
    let screen = render(&app, 100, 24);
    assert!(screen.contains("no matching tasks"), "{screen}");
    assert!(screen.contains("0 of 6 tasks"));
}

#[test]
fn a_long_query_shows_its_tail_and_the_cursor() {
    let mut app = board();
    opened(&mut app);
    typed(&mut app, &"x".repeat(80));
    typed(&mut app, "END");
    let screen = render(&app, 40, 12);
    assert!(screen.contains("…xxx"), "{screen}");
    assert!(screen.contains("xxxEND_"), "{screen}");
}

#[test]
fn help_and_the_board_hints_mention_search() {
    let mut app = board();
    let hint = render(&app, 100, 24);
    assert!(hint.contains("/ search"), "{hint}");
    ch(&mut app, '?');
    let help = render(&app, 100, 24);
    assert!(help.contains("/           search"), "{help}");
    // The 10-row help box still fits the smallest terminal.
    let small = render(&app, 40, 10);
    assert!(
        small.contains("search") && small.contains("keys"),
        "{small}"
    );
}

#[tokio::test]
async fn search_finds_a_task_beyond_the_closed_window_through_the_real_loop() {
    let mut recent = summary(
        "cccc3333-0",
        "Recent release",
        "closed",
        None,
        None,
        "p-alpha",
    );
    recent.task.updated_at = mins_ago(10);
    let mut old = summary(
        "dddd4444-0",
        "Ancient migration fix",
        "closed",
        None,
        None,
        "p-alpha",
    );
    old.task.updated_at = mins_ago(10_000);
    let live = summary(
        "aaaa1111-0",
        "Fix the login",
        "open",
        Some("coding"),
        Some(3),
        "p-alpha",
    );
    let fake = Fake {
        tasks: Arc::new(vec![live, recent, old]),
        cancels: Arc::new(Mutex::new(Vec::new())),
        keeps: Arc::new(Mutex::new(Vec::new())),
        hang: false,
        queries: Arc::default(),
        ..Default::default()
    };
    let base_url = fake_server(fake.clone()).await;
    let client = Arc::new(Client::new(base_url.clone()).without_version_check());
    let mut app = App::new(
        Scope::AllProjects,
        base_url,
        Duration::from_secs(1),
        "1s".into(),
        Utc::now(),
    );
    app.color = false;
    app.ssh = false;
    let config = LoopConfig {
        tick: Duration::from_millis(50),
        closed: 1,
        project_id: None,
        timeout: Duration::from_secs(30),
        poll_timeout: Duration::from_secs(30),
        panicked: Arc::default(),
        shell: "/bin/sh".into(),
        message_dir: std::env::temp_dir(),
    };
    let (ktx, mut krx) = mpsc::unbounded_channel();
    let latest = Arc::new(Mutex::new(String::new()));
    let sink = Arc::clone(&latest);
    let mut terminal = Terminal::new(TestBackend::new(120, 30)).unwrap();
    let driver = async {
        let k = |c| {
            ktx.send(KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE))
                .unwrap()
        };
        wait_for(&latest, "Recent release").await;
        assert!(!latest.lock().unwrap().contains("Ancient migration fix"));
        k('/');
        wait_for(&latest, "3 of 3 tasks").await;
        {
            let queries = fake.queries.lock().unwrap();
            let search = queries
                .iter()
                .find(|q| !q.contains_key("status"))
                .expect("a request without a status filter");
            assert!(!search.contains_key("limit"));
            assert!(!search.contains_key("project_id"));
        }
        for c in "ancient".chars() {
            k(c);
        }
        wait_for(&latest, "1 of 3 tasks").await;
        wait_for(&latest, "Ancient migration fix").await;
        ktx.send(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
            .unwrap();
        wait_for(&latest, "last events").await;
        k('q');
        wait_for(&latest, "RECENTLY CLOSED").await;
        k('q');
    };
    let mut on_frame = |buf: &Buffer| *sink.lock().unwrap() = screen(buf);
    let mut handover = NoHandover;
    let run = run_loop(
        &mut terminal,
        &mut app,
        client,
        &config,
        &mut krx,
        &mut handover,
        &mut on_frame,
    );
    let (result, ()) =
        tokio::time::timeout(Duration::from_secs(30), async { tokio::join!(run, driver) })
            .await
            .expect("the loop should end on q");
    result.unwrap();
}

#[tokio::test]
async fn search_in_one_project_mode_asks_for_that_project_only() {
    let fake = Fake {
        tasks: Arc::new(vec![
            summary(
                "aaaa1111-0",
                "Alpha task",
                "open",
                Some("coding"),
                Some(3),
                "p-alpha",
            ),
            summary(
                "bbbb2222-0",
                "Beta task",
                "open",
                Some("coding"),
                Some(3),
                "p-beta",
            ),
        ]),
        cancels: Arc::new(Mutex::new(Vec::new())),
        keeps: Arc::new(Mutex::new(Vec::new())),
        hang: false,
        queries: Arc::default(),
        ..Default::default()
    };
    let base_url = fake_server(fake.clone()).await;
    let client = Arc::new(Client::new(base_url.clone()).without_version_check());
    let mut app = App::new(
        Scope::Project {
            name: "alpha-project".into(),
        },
        base_url,
        Duration::from_secs(1),
        "1s".into(),
        Utc::now(),
    );
    app.color = false;
    app.ssh = false;
    let config = LoopConfig {
        tick: Duration::from_millis(50),
        closed: 10,
        project_id: Some("p-alpha".into()),
        timeout: Duration::from_secs(30),
        poll_timeout: Duration::from_secs(30),
        panicked: Arc::default(),
        shell: "/bin/sh".into(),
        message_dir: std::env::temp_dir(),
    };
    let (ktx, mut krx) = mpsc::unbounded_channel();
    let latest = Arc::new(Mutex::new(String::new()));
    let sink = Arc::clone(&latest);
    let mut terminal = Terminal::new(TestBackend::new(120, 30)).unwrap();
    let driver = async {
        wait_for(&latest, "Alpha task").await;
        ktx.send(KeyEvent::new(KeyCode::Char('/'), KeyModifiers::NONE))
            .unwrap();
        wait_for(&latest, "1 of 1 tasks").await;
        {
            let queries = fake.queries.lock().unwrap();
            let search = queries
                .iter()
                .find(|q| !q.contains_key("status"))
                .expect("a request without a status filter");
            assert_eq!(
                search.get("project_id").map(String::as_str),
                Some("p-alpha")
            );
            assert!(!search.contains_key("limit"));
        }
        ktx.send(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL))
            .unwrap();
    };
    let mut on_frame = |buf: &Buffer| *sink.lock().unwrap() = screen(buf);
    let mut handover = NoHandover;
    let run = run_loop(
        &mut terminal,
        &mut app,
        client,
        &config,
        &mut krx,
        &mut handover,
        &mut on_frame,
    );
    let (result, ()) =
        tokio::time::timeout(Duration::from_secs(30), async { tokio::join!(run, driver) })
            .await
            .expect("Ctrl-C quits from the popup");
    result.unwrap();
}

// ---- queued agent turns -----------------------------------------------------

fn queued_turn(stage: &str, position: u32) -> chocofactory_core::models::QueuedTurn {
    chocofactory_core::models::QueuedTurn {
        stage: stage.to_string(),
        group: None,
        position,
        pool: "server".to_string(),
        pool_name: "server".to_string(),
        limit: Some(2),
        running: 2,
        unlimited_because: None,
        queued_at: now(),
    }
}

#[test]
fn a_queued_stage_reads_its_place_in_line() {
    let mut app = board();
    app.active
        .iter_mut()
        .find(|t| t.task.id == BUSY)
        .unwrap()
        .queued = vec![queued_turn("internal_review", 2)];
    for w in [80u16, 100, 140] {
        let s = render(&app, w, 24);
        assert!(s.contains("internal_review · queued 2"), "{w}\n{s}");
    }
}

#[test]
fn a_group_with_waiting_branches_counts_them() {
    let mut app = board();
    let task = app.active.iter_mut().find(|t| t.task.id == BUSY).unwrap();
    task.branch_progress = progress_of(1, 3);
    task.queued = vec![queued_turn("security", 1)];
    let s = render(&app, 120, 24);
    assert!(s.contains("internal_review 1/3 · 1 queued"), "{s}");
    // Without waiting branches the cell is the plain progress.
    app.active
        .iter_mut()
        .find(|t| t.task.id == BUSY)
        .unwrap()
        .queued
        .clear();
    let s = render(&app, 120, 24);
    assert!(s.contains("internal_review 1/3"), "{s}");
    assert!(!s.contains("queued"), "{s}");
}

fn pool(limit: Option<u32>, running: u32, queued: u32) -> chocofactory_core::models::AgentPool {
    chocofactory_core::models::AgentPool {
        pool: "server".into(),
        name: "server".into(),
        limit,
        running,
        queued,
    }
}

fn server_status_with(
    pools: Vec<chocofactory_core::models::AgentPool>,
) -> chocofactory_core::models::ServerStatus {
    chocofactory_core::models::ServerStatus {
        version: "0.0.1".into(),
        commit: None,
        pid: 1,
        port: 4141,
        started_at: now(),
        config_root: "/r".into(),
        exe: "/e".into(),
        exe_replaced: None,
        choco_binary: "/c".into(),
        choco_binary_found: true,
        tasks: Default::default(),
        in_flight: vec![],
        agent_pools: pools,
        queued: vec![],
        queue_error: None,
    }
}

fn list_with_server(server: Option<chocofactory_core::models::ServerStatus>) -> Msg {
    Msg::List(Box::new(ListResult {
        server,
        at: now(),
        active: Ok(vec![]),
        closed: Ok(vec![]),
        projects: None,
    }))
}

#[test]
fn the_header_shows_the_server_pools_slots_and_follows_each_refresh() {
    let mut app = board();
    assert!(!render(&app, 140, 12).contains("agents"));
    update(
        &mut app,
        list_with_server(Some(server_status_with(vec![pool(Some(2), 2, 3)]))),
    );
    let first = render(&app, 140, 12).lines().next().unwrap().to_string();
    assert!(first.contains("agents 2/2 · 3 queued"), "{first}");
    // The next refresh brings new counts; the header follows.
    update(
        &mut app,
        list_with_server(Some(server_status_with(vec![pool(Some(2), 1, 0)]))),
    );
    let second = render(&app, 140, 12).lines().next().unwrap().to_string();
    assert!(
        second.contains("agents 1/2") && !second.contains("queued"),
        "{second}"
    );
    // A failed read keeps the last values.
    update(&mut app, list_with_server(None));
    let third = render(&app, 140, 12).lines().next().unwrap().to_string();
    assert!(third.contains("agents 1/2"), "{third}");
    // No limit reads as the running count alone.
    update(
        &mut app,
        list_with_server(Some(server_status_with(vec![pool(None, 4, 0)]))),
    );
    let fourth = render(&app, 140, 12).lines().next().unwrap().to_string();
    assert!(
        fourth.contains("agents 4") && !fourth.contains("agents 4/"),
        "{fourth}"
    );
}

#[test]
fn a_project_pool_does_not_show_in_the_server_header() {
    let mut app = board();
    let mut project_pool = pool(Some(1), 1, 5);
    project_pool.pool = "project:p1".into();
    update(
        &mut app,
        list_with_server(Some(server_status_with(vec![project_pool]))),
    );
    assert!(!render(&app, 140, 12).contains("agents"));
}

#[test]
fn a_summary_from_an_older_daemon_has_no_queued_turns() {
    let mut v =
        serde_json::to_value(summary("t1", "T", "open", Some("coding"), Some(5), "p")).unwrap();
    v.as_object_mut().unwrap().remove("queued");
    let t: TaskSummary = serde_json::from_value(v).unwrap();
    assert!(t.queued.is_empty());
}

// ---- pause and resume (#295) -------------------------------------------------

fn paused_summary(id: &str, status: &str, mins: i64) -> TaskSummary {
    let mut t = summary(id, "Held", status, Some("coding"), Some(30), "p");
    t.task.paused_at = Some(mins_ago(mins));
    t
}

/// Every status on the list lands in exactly one section; a status added to
/// the list without a section arm fails here.
#[test]
fn every_listed_status_lands_in_exactly_one_section() {
    use chocofactory_core::models::ALL_TASK_STATUSES;
    let mut app = new_app(Scope::AllProjects);
    let make = |status: &str| paused_summary(&format!("t-{status}"), status, 5);
    let active: Vec<TaskSummary> = ["open", "pausing", "paused", "stuck"]
        .into_iter()
        .map(make)
        .collect();
    let closed: Vec<TaskSummary> = ["closed", "cancelled"].into_iter().map(make).collect();
    load(&mut app, active, closed);
    let sections = app.sections();
    for status in ALL_TASK_STATUSES {
        let id = format!("t-{status}");
        let homes: Vec<usize> = sections
            .iter()
            .enumerate()
            .filter(|(_, s)| s.iter().any(|t| t.task.id == id))
            .map(|(i, _)| i)
            .collect();
        assert_eq!(homes.len(), 1, "'{status}' is in sections {homes:?}");
    }
    let names: Vec<&str> = Section::ALL.iter().map(|s| s.title()).collect();
    assert_eq!(
        names,
        [
            "NEEDS YOU",
            "IN PROGRESS",
            "PAUSED",
            "STUCK",
            "RECENTLY CLOSED"
        ]
    );
    // Pausing and paused tasks share the Paused section, between In
    // progress and Stuck.
    assert_eq!(
        ids(&sections[2]),
        ["t-paused", "t-pausing"].map(String::from)
    );
    assert_eq!(ids(&sections[1]), ["t-open"].map(String::from));
}

#[test]
fn the_paused_section_reads_paused_for_and_marks_a_pausing_stage() {
    let mut app = new_app(Scope::AllProjects);
    load(
        &mut app,
        vec![
            paused_summary("aaaa1111-0", "paused", 7),
            paused_summary("bbbb2222-0", "pausing", 2),
        ],
        vec![],
    );
    let screen = render(&app, 120, 24);
    assert!(screen.contains("PAUSED (2)"), "{screen}");
    assert!(screen.contains("paused for"), "{screen}");
    assert!(screen.contains("coding · pausing"), "{screen}");
}

#[test]
fn p_pauses_an_open_task_and_resumes_a_pausing_or_paused_one() {
    let mut app = new_app(Scope::AllProjects);
    load(
        &mut app,
        vec![
            summary(
                "open0000-0",
                "Running",
                "open",
                Some("coding"),
                Some(1),
                "p",
            ),
            paused_summary("held0000-0", "paused", 3),
            paused_summary("slow0000-0", "pausing", 3),
            summary(
                "stuck000-0",
                "Broken",
                "stuck",
                Some("coding"),
                Some(1),
                "p",
            ),
        ],
        vec![],
    );
    let on = |app: &mut App, id: &str| {
        app.selected = Some(id.to_string());
        ch(app, 'p')
    };
    assert_eq!(
        on(&mut app, "open0000-0"),
        vec![Effect::Pause("open0000-0".into())]
    );
    assert_eq!(
        on(&mut app, "held0000-0"),
        vec![Effect::Resume("held0000-0".into())]
    );
    assert_eq!(
        on(&mut app, "slow0000-0"),
        vec![Effect::Resume("slow0000-0".into())]
    );
    assert!(on(&mut app, "stuck000-0").is_empty());
    assert_eq!(
        app.status.as_ref().unwrap().text,
        "pause: task is stuck, not open or paused"
    );
}

#[test]
fn r_on_a_paused_task_says_to_press_p_and_c_is_allowed() {
    let mut app = new_app(Scope::AllProjects);
    load(
        &mut app,
        vec![
            paused_summary("held0000-0", "paused", 3),
            paused_summary("slow0000-0", "pausing", 3),
        ],
        vec![],
    );
    for (id, status) in [("held0000-0", "paused"), ("slow0000-0", "pausing")] {
        app.selected = Some(id.to_string());
        assert!(ch(&mut app, 'r').is_empty());
        assert!(app.prompt.is_none());
        assert_eq!(
            app.status.as_ref().unwrap().text,
            format!("task is {status}; press p to resume")
        );
        ch(&mut app, 'c');
        assert!(app.prompt.is_some(), "cancel is allowed on {status}");
        press(&mut app, KeyCode::Esc);
    }
}

#[test]
fn the_detail_view_shows_the_paused_row_and_how_long_ago() {
    let mut app = new_app(Scope::AllProjects);
    load(
        &mut app,
        vec![paused_summary("held0000-0", "paused", 3)],
        vec![],
    );
    app.selected = Some("held0000-0".to_string());
    press(&mut app, KeyCode::Enter);
    let screen = render(&app, 120, 30);
    assert!(screen.contains("Paused"), "{screen}");
    assert!(screen.contains("choco task resume held0000-0"), "{screen}");
    assert!(screen.contains("(paused 3m ago)"), "{screen}");
}

// ---- `s`: reply through the editor (#167) -----------------------------------

use std::os::unix::process::ExitStatusExt;
use std::path::PathBuf;
use std::process::ExitStatus;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use super::{KeySource, spawn_key_reader};

fn status(raw: i32) -> ExitStatus {
    ExitStatus::from_raw(raw)
}

fn two_tasks() -> App {
    let mut app = new_app(Scope::AllProjects);
    app.editor = Some("ed".into());
    load(
        &mut app,
        vec![
            summary("aaaa1111-0", "Task A", "open", Some("coding"), Some(3), "p"),
            summary("bbbb2222-0", "Task B", "open", Some("coding"), Some(3), "p"),
        ],
        vec![],
    );
    app
}

fn edited(id: &str, outcome: EditOutcome) -> Msg {
    Msg::Edited {
        id: id.into(),
        path: PathBuf::from("/tmp/m.md"),
        outcome,
    }
}

fn exited(raw: i32, text: &str) -> EditOutcome {
    EditOutcome::Exited {
        status: status(raw),
        content: Ok(text.into()),
        exists: true,
    }
}

#[test]
fn send_goes_to_the_task_captured_when_s_was_pressed() {
    // The cursor moves on after `s`.
    let mut app = two_tasks();
    assert_eq!(
        ch(&mut app, 's'),
        vec![Effect::Compose {
            id: "aaaa1111-0".into()
        }]
    );
    ch(&mut app, 'j');
    assert_eq!(app.selected.as_deref(), Some("bbbb2222-0"));
    let fx = update(&mut app, edited("aaaa1111-0", exited(0, "hi\n")));
    assert_eq!(
        fx,
        vec![Effect::Send {
            id: "aaaa1111-0".into(),
            path: "/tmp/m.md".into(),
            text: "hi".into()
        }]
    );
    // The detail view's task differs from the selection.
    let mut app = two_tasks();
    press(&mut app, KeyCode::Enter);
    let fx = ch(&mut app, 's');
    assert_eq!(
        fx,
        vec![Effect::Compose {
            id: "aaaa1111-0".into()
        }]
    );
    app.selected = Some("bbbb2222-0".into());
    let fx = update(&mut app, edited("aaaa1111-0", exited(0, "hi\n")));
    assert!(matches!(&fx[0], Effect::Send { id, .. } if id == "aaaa1111-0"));
}

#[test]
fn whitespace_only_sends_nothing() {
    for text in ["", "  \n\t\n"] {
        let mut app = two_tasks();
        let fx = update(&mut app, edited("aaaa1111-0", exited(0, text)));
        assert_eq!(
            fx,
            vec![Effect::RemoveFile {
                path: "/tmp/m.md".into()
            }]
        );
        assert_eq!(
            app.status.as_ref().unwrap().full(),
            "nothing sent: the message was empty"
        );
    }
    let mut app = two_tasks();
    let fx = update(&mut app, edited("aaaa1111-0", exited(0, " x \n")));
    assert!(matches!(&fx[0], Effect::Send { text, .. } if text == " x"));
}

#[test]
fn editor_resolution() {
    use super::app::resolve_editor;
    assert_eq!(resolve_editor(Some("v"), Some("e")).as_deref(), Some("v"));
    assert_eq!(resolve_editor(Some("  "), Some("e")).as_deref(), Some("e"));
    assert_eq!(resolve_editor(Some(""), Some("e")).as_deref(), Some("e"));
    assert_eq!(
        resolve_editor(None, Some(" e -w ")).as_deref(),
        Some("e -w")
    );
    assert_eq!(resolve_editor(Some(" v "), None).as_deref(), Some("v"));
    assert_eq!(resolve_editor(None, None), None);
    assert_eq!(resolve_editor(Some(" "), Some("\t")), None);

    let mut app = two_tasks();
    app.editor = None;
    assert!(ch(&mut app, 's').is_empty());
    assert_eq!(
        app.status.as_ref().unwrap().full(),
        "s: set $VISUAL or $EDITOR to write a message here, or use choco task send aaaa1111-0"
    );
    assert_eq!(app.status.as_ref().unwrap().level, Level::Info);
}

#[test]
fn s_key_routing() {
    // Typed into the search query.
    let mut app = two_tasks();
    ch(&mut app, '/');
    assert!(ch(&mut app, 's').is_empty());
    assert_eq!(app.search.as_ref().unwrap().query, "s");
    // Swallowed by a prompt.
    let mut app = two_tasks();
    ch(&mut app, 'c');
    assert!(app.prompt.is_some());
    assert!(ch(&mut app, 's').is_empty());
    assert!(app.status.is_none());
    // Swallowed by help.
    let mut app = two_tasks();
    ch(&mut app, '?');
    assert!(ch(&mut app, 's').is_empty());
    assert!(app.help);
    // Detail views use the detail's task, not the selection.
    let mut app = two_tasks();
    press(&mut app, KeyCode::Enter);
    app.selected = Some("bbbb2222-0".into());
    let want = vec![Effect::Compose {
        id: "aaaa1111-0".into(),
    }];
    assert_eq!(ch(&mut app, 's'), want);
    ch(&mut app, 'e');
    assert_eq!(ch(&mut app, 's'), want);
    // An empty list.
    let mut app = new_app(Scope::AllProjects);
    app.editor = Some("ed".into());
    assert!(ch(&mut app, 's').is_empty());
}

#[test]
fn exit_report_lists_kept_and_unanswered_files() {
    let mut app = two_tasks();
    app.unanswered_sends
        .push(("bbbb2222-0".into(), "/tmp/two.md".into()));
    app.kept_messages
        .push(("aaaa1111-0".into(), "/tmp/one.md".into()));
    assert_eq!(
        super::app::exit_report(&app),
        vec![
            "unsent message for task aaaa1111: /tmp/one.md".to_string(),
            "message for task bbbb2222 got no answer before exit (it may have been delivered): /tmp/two.md"
                .to_string(),
        ]
    );
}

#[test]
fn the_kept_path_is_never_cut() {
    let path = "/tmp/choco-message-aaaa1111-1-1.md";
    for w in [60u16, 80, 100] {
        let mut app = two_tasks();
        update(
            &mut app,
            Msg::Sent {
                id: "aaaa1111-0".into(),
                path: path.into(),
                result: Err(SendFailure::Failed("x".repeat(300))),
            },
        );
        let out = render(&app, w, 20);
        let last = out.lines().last().unwrap().to_string();
        // The reason has priority, so the directory may go; the file name stays.
        assert!(
            last.contains("choco-message-aaaa1111-1-1.md"),
            "{w}: {last:?}"
        );
        if w >= 100 {
            assert!(
                last.contains(&format!("; your text is in {path}")),
                "{last:?}"
            );
        }
        assert!(last.starts_with("send f"), "{last:?}");
    }
}

// -- the key reader --

struct TimedSource {
    calls: Arc<Mutex<Vec<(std::time::Instant, std::time::Instant)>>>,
    stop: Arc<AtomicBool>,
}

impl KeySource for TimedSource {
    fn poll(&mut self, _: Duration) -> std::io::Result<bool> {
        let start = std::time::Instant::now();
        std::thread::sleep(Duration::from_millis(30));
        self.calls
            .lock()
            .unwrap()
            .push((start, std::time::Instant::now()));
        if self.stop.load(Ordering::SeqCst) {
            return Err(std::io::Error::other("stop"));
        }
        Ok(false)
    }
    fn read(&mut self) -> std::io::Result<ratatui::crossterm::event::Event> {
        Err(std::io::Error::other("no events"))
    }
}

#[test]
fn key_reader_makes_no_terminal_calls_while_paused() {
    let calls = Arc::new(Mutex::new(Vec::new()));
    let stop = Arc::new(AtomicBool::new(false));
    let (_rx, control) = spawn_key_reader(TimedSource {
        calls: Arc::clone(&calls),
        stop: Arc::clone(&stop),
    });
    std::thread::sleep(Duration::from_millis(200));
    control.pause();
    let paused = std::time::Instant::now();
    std::thread::sleep(Duration::from_millis(200));
    let resumed = std::time::Instant::now();
    control.resume();
    std::thread::sleep(Duration::from_millis(200));
    stop.store(true, Ordering::SeqCst);
    let calls = calls.lock().unwrap().clone();
    assert!(calls.iter().any(|(s, _)| *s < paused), "none before");
    assert!(calls.iter().any(|(s, _)| *s >= resumed), "none after");
    let inside: Vec<_> = calls
        .iter()
        .filter(|(s, e)| *e > paused && *s < resumed)
        .collect();
    assert!(inside.is_empty(), "calls while paused: {inside:?}");
}

struct DyingSource;

impl KeySource for DyingSource {
    fn poll(&mut self, _: Duration) -> std::io::Result<bool> {
        Ok(true)
    }
    fn read(&mut self) -> std::io::Result<ratatui::crossterm::event::Event> {
        Err(std::io::Error::other("gone"))
    }
}

#[test]
fn pause_returns_when_the_reader_has_exited() {
    let (_rx, control) = spawn_key_reader(DyingSource);
    std::thread::sleep(Duration::from_millis(200));
    let (done_tx, done_rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        control.pause();
        let _ = done_tx.send(());
    });
    done_rx
        .recv_timeout(Duration::from_secs(2))
        .expect("pause hung on an exited reader");
}

// -- the signal guard --

/// Editor tests and the signal guard test share process-wide signal
/// dispositions, so they run one at a time.
static EDITOR_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

fn sigint_is_ignored() -> (bool, usize) {
    // SAFETY: reads the current disposition only.
    unsafe {
        let mut old: libc::sigaction = std::mem::zeroed();
        assert_eq!(libc::sigaction(libc::SIGINT, std::ptr::null(), &mut old), 0);
        (old.sa_sigaction == libc::SIG_IGN, old.sa_sigaction)
    }
}

#[test]
fn sigint_ignore_nests() {
    let _serial = EDITOR_LOCK.blocking_lock();
    let (was_ignored, before) = sigint_is_ignored();
    let a = super::SigintIgnore::acquire().unwrap();
    let b = super::SigintIgnore::acquire().unwrap();
    assert!(sigint_is_ignored().0);
    a.release().unwrap();
    assert!(sigint_is_ignored().0, "released too early");
    b.release().unwrap();
    let (now_ignored, now) = sigint_is_ignored();
    assert_eq!((now_ignored, now), (was_ignored, before));
}

// -- run_loop with a stub editor --

struct Scratch(PathBuf);

impl Scratch {
    fn new(name: &str) -> Self {
        let p = std::env::temp_dir().join(format!("choco-dash-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&p);
        std::fs::create_dir_all(&p).unwrap();
        Scratch(p)
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        use std::os::unix::fs::PermissionsExt;
        if let Ok(rd) = std::fs::read_dir(&self.0) {
            for e in rd.flatten() {
                let _ = std::fs::set_permissions(e.path(), std::fs::Permissions::from_mode(0o700));
            }
        }
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// `$1` log, `$2` mode, `$3` extra, `$4` the message file.
const STUB: &str = r#"
log="$1"; mode="$2"; extra="$3"; f="$4"
echo editor >> "$log"
echo "$f" > "$log.path"
case "$mode" in
  text) printf ' x \n' > "$f" ;;
  empty) : ;;
  fail) printf 'hello\n' > "$f"; exit 1 ;;
  bin) printf '\377\376' > "$f" ;;
  ctrlc) kill -INT "$extra"; printf 'hello\n' > "$f" ;;
  selfint) printf 'hello\n' > "$f"; kill -INT $$ ;;
  chmod) chmod 500 "$(dirname "$f")" ;;
esac
"#;

/// A backend that never answers a cursor-position query, like a terminal
/// that ignores `ESC[6n`.
struct NoCursor(TestBackend);

impl ratatui::backend::Backend for NoCursor {
    type Error = std::io::Error;
    fn draw<'a, I>(&mut self, content: I) -> std::io::Result<()>
    where
        I: Iterator<Item = (u16, u16, &'a ratatui::buffer::Cell)>,
    {
        self.0.draw(content).map_err(|e| match e {})
    }
    fn hide_cursor(&mut self) -> std::io::Result<()> {
        self.0.hide_cursor().map_err(|e| match e {})
    }
    fn show_cursor(&mut self) -> std::io::Result<()> {
        self.0.show_cursor().map_err(|e| match e {})
    }
    fn get_cursor_position(&mut self) -> std::io::Result<ratatui::layout::Position> {
        Err(std::io::Error::other("the terminal did not answer"))
    }
    fn set_cursor_position<P: Into<ratatui::layout::Position>>(
        &mut self,
        position: P,
    ) -> std::io::Result<()> {
        self.0.set_cursor_position(position).map_err(|e| match e {})
    }
    fn clear(&mut self) -> std::io::Result<()> {
        self.0.clear().map_err(|e| match e {})
    }
    fn clear_region(&mut self, clear_type: ratatui::backend::ClearType) -> std::io::Result<()> {
        self.0.clear_region(clear_type).map_err(|e| match e {})
    }
    fn size(&self) -> std::io::Result<ratatui::layout::Size> {
        self.0.size().map_err(|e| match e {})
    }
    fn window_size(&mut self) -> std::io::Result<ratatui::backend::WindowSize> {
        self.0.window_size().map_err(|e| match e {})
    }
    fn flush(&mut self) -> std::io::Result<()> {
        self.0.flush().map_err(|e| match e {})
    }
}

struct Scenario {
    mode: &'static str,
    extra: String,
    shell: &'static str,
    reply: Reply,
    timeout: Duration,
    suspend_err: Option<&'static str>,
    resume_err: Option<&'static str>,
    message_dir: Option<PathBuf>,
    wait: Wait,
}

#[derive(Clone, Copy)]
enum Wait {
    Screen(&'static str),
    Posted,
    Nothing,
}

impl Scenario {
    fn new(mode: &'static str, wait: Wait) -> Self {
        Scenario {
            mode,
            extra: "-".into(),
            shell: "/bin/sh",
            reply: Reply::Accept,
            timeout: Duration::from_secs(30),
            suspend_err: None,
            resume_err: None,
            message_dir: None,
            wait,
        }
    }
}

struct RecHandover {
    log: PathBuf,
    frames: Arc<AtomicUsize>,
    counts: Arc<Mutex<Vec<(&'static str, usize)>>>,
    suspend_err: Option<&'static str>,
    resume_err: Option<&'static str>,
}

impl RecHandover {
    fn note(&self, what: &'static str) {
        use std::io::Write;
        let mut f = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.log)
            .unwrap();
        writeln!(f, "{what}").unwrap();
        self.counts
            .lock()
            .unwrap()
            .push((what, self.frames.load(Ordering::SeqCst)));
    }
}

impl Handover for RecHandover {
    fn suspend(&mut self) -> std::io::Result<()> {
        self.note("suspend");
        self.suspend_err
            .map_or(Ok(()), |e| Err(std::io::Error::other(e)))
    }
    fn resume(&mut self) -> std::io::Result<()> {
        self.note("resume");
        self.resume_err
            .map_or(Ok(()), |e| Err(std::io::Error::other(e)))
    }
}

struct Outcome {
    result: Result<(), String>,
    screen: String,
    fake: Fake,
    log: Vec<String>,
    counts: Vec<(&'static str, usize)>,
    dir: Scratch,
    msgs: PathBuf,
    app: App,
}

impl Outcome {
    fn posts(&self) -> Vec<(String, String)> {
        self.fake.messages.lock().unwrap().clone()
    }
    /// The path the stub was given.
    fn path(&self) -> PathBuf {
        let p = std::fs::read_to_string(self.dir.0.join("log.path")).unwrap();
        PathBuf::from(p.trim_end())
    }
}

async fn scenario(name: &str, sc: Scenario) -> Outcome {
    let _serial = EDITOR_LOCK.lock().await;
    let dir = Scratch::new(name);
    let stub = dir.0.join("stub.sh");
    std::fs::write(&stub, STUB).unwrap();
    let log = dir.0.join("log");
    let msgs = sc.message_dir.clone().unwrap_or_else(|| dir.0.join("msgs"));
    if sc.message_dir.is_none() {
        std::fs::create_dir_all(&msgs).unwrap();
    }
    let fake = Fake {
        tasks: Arc::new(vec![summary(
            "aaaa1111-0",
            "Fix the login",
            "open",
            Some("coding"),
            Some(3),
            "p-alpha",
        )]),
        reply: sc.reply.clone(),
        ..Default::default()
    };
    let base_url = fake_server(fake.clone()).await;
    let client = Arc::new(Client::new(base_url.clone()).without_version_check());
    let mut app = new_app(Scope::AllProjects);
    app.now = Utc::now();
    app.editor = Some(format!(
        "sh {} {} {} {}",
        stub.display(),
        log.display(),
        sc.mode,
        sc.extra
    ));
    let config = LoopConfig {
        tick: Duration::from_millis(50),
        closed: 10,
        project_id: None,
        timeout: sc.timeout,
        poll_timeout: sc.timeout,
        panicked: Arc::default(),
        shell: sc.shell.into(),
        message_dir: msgs.clone(),
    };
    let frames = Arc::new(AtomicUsize::new(0));
    let counts = Arc::new(Mutex::new(Vec::new()));
    let mut handover = RecHandover {
        log: log.clone(),
        frames: Arc::clone(&frames),
        counts: Arc::clone(&counts),
        suspend_err: sc.suspend_err,
        resume_err: sc.resume_err,
    };
    let (ktx, mut krx) = mpsc::unbounded_channel();
    let latest = Arc::new(Mutex::new(String::new()));
    let sink = Arc::clone(&latest);
    let shot = Arc::new(Mutex::new(String::new()));
    let mut terminal = Terminal::new(NoCursor(TestBackend::new(320, 30))).unwrap();
    let driver = async {
        wait_for(&latest, "Fix the login").await;
        ktx.send(KeyEvent::new(KeyCode::Char('s'), KeyModifiers::NONE))
            .unwrap();
        match sc.wait {
            Wait::Screen(t) => wait_for(&latest, t).await,
            Wait::Posted => {
                for _ in 0..300 {
                    if !fake.messages.lock().unwrap().is_empty() {
                        break;
                    }
                    tokio::time::sleep(Duration::from_millis(50)).await;
                }
                // Let the loop process what followed the post.
                tokio::time::sleep(Duration::from_millis(150)).await;
            }
            Wait::Nothing => {}
        }
        *shot.lock().unwrap() = latest.lock().unwrap().clone();
        let _ = ktx.send(KeyEvent::new(KeyCode::Char('q'), KeyModifiers::NONE));
    };
    let mut on_frame = |buf: &Buffer| {
        frames.fetch_add(1, Ordering::SeqCst);
        *sink.lock().unwrap() = screen(buf);
    };
    let run = run_loop(
        &mut terminal,
        &mut app,
        client,
        &config,
        &mut krx,
        &mut handover,
        &mut on_frame,
    );
    let (result, ()) =
        tokio::time::timeout(Duration::from_secs(60), async { tokio::join!(run, driver) })
            .await
            .expect("the loop should end");
    let screen_text = shot.lock().unwrap().clone();
    let log_lines = std::fs::read_to_string(&log)
        .unwrap_or_default()
        .lines()
        .map(str::to_string)
        .collect();
    let counts = counts.lock().unwrap().clone();
    Outcome {
        result,
        screen: screen_text,
        fake,
        log: log_lines,
        counts,
        dir,
        msgs,
        app,
    }
}

fn files_in(dir: &std::path::Path) -> Vec<PathBuf> {
    std::fs::read_dir(dir)
        .map(|rd| rd.flatten().map(|e| e.path()).collect())
        .unwrap_or_default()
}

#[tokio::test]
async fn the_terminal_is_handed_over_and_restored_on_every_way_back() {
    let cases = [
        ("ok", Scenario::new("text", Wait::Screen("sent to"))),
        (
            "ws",
            Scenario::new("empty", Wait::Screen("the message was empty")),
        ),
        (
            "fail",
            Scenario::new("fail", Wait::Screen("exit status: 1")),
        ),
        ("spawn", {
            let mut s = Scenario::new("text", Wait::Screen("could not start the editor"));
            s.shell = "/nonexistent/sh";
            s
        }),
        ("refused", {
            let mut s = Scenario::new("text", Wait::Screen("send failed"));
            s.reply = Reply::Refuse("nope".into());
            s
        }),
    ];
    for (name, sc) in cases {
        let spawn_fail = name == "spawn";
        let o = scenario(&format!("every-{name}"), sc).await;
        o.result.clone().unwrap();
        let want: Vec<&str> = if spawn_fail {
            vec!["suspend", "resume"]
        } else {
            vec!["suspend", "editor", "resume"]
        };
        assert_eq!(o.log, want, "{name}");
        let (a, b) = (o.counts[0].1, o.counts[1].1);
        assert_eq!(a, b, "{name}: drew while the editor ran");
    }
}

#[tokio::test]
async fn a_non_zero_editor_exit_sends_nothing_and_keeps_the_text() {
    let o = scenario(
        "nonzero",
        Scenario::new("fail", Wait::Screen("exit status: 1")),
    )
    .await;
    let path = o.path();
    assert!(o.posts().is_empty());
    assert_eq!(std::fs::read_to_string(&path).unwrap(), "hello\n");
    assert!(
        o.screen
            .contains("nothing sent: the editor exited with exit status: 1")
    );
    assert!(
        o.screen
            .contains(&format!("; your text is in {}", path.display()))
    );
    assert_eq!(o.app.kept_messages.len(), 1);
}

#[tokio::test]
async fn a_failed_send_keeps_the_text_and_says_where() {
    let mut sc = Scenario::new("text", Wait::Screen("send failed"));
    sc.reply = Reply::Refuse("marker text zzz".into());
    let o = scenario("refused", sc).await;
    let path = o.path();
    assert!(
        o.screen.contains("send failed: marker text zzz"),
        "{}",
        o.screen
    );
    assert!(
        o.screen
            .contains(&format!("; your text is in {}", path.display()))
    );
    assert_eq!(std::fs::read_to_string(&path).unwrap(), " x \n");
    assert_eq!(o.app.kept_messages.len(), 1);

    let mut sc = Scenario::new("text", Wait::Screen("send timed out"));
    sc.reply = Reply::Hang;
    sc.timeout = Duration::from_millis(300);
    let o = scenario("timeout", sc).await;
    let path = o.path();
    assert!(
        o.screen.contains(
            "send timed out: the daemon may still have taken it (check the task's events)"
        ),
        "{}",
        o.screen
    );
    assert!(
        o.screen
            .contains(&format!("; your text is in {}", path.display()))
    );
    assert_eq!(std::fs::read_to_string(&path).unwrap(), " x \n");
}

#[tokio::test]
async fn a_sent_message_removes_its_file_and_refreshes() {
    let o = scenario(
        "sent",
        Scenario::new("text", Wait::Screen("sent to aaaa1111")),
    )
    .await;
    assert_eq!(
        o.posts(),
        vec![("aaaa1111-0".to_string(), " x".to_string())]
    );
    assert!(files_in(&o.msgs).is_empty());
    let events = o.fake.events.lock().unwrap().clone();
    let post = events.iter().position(|e| *e == "post").unwrap();
    assert!(events[post..].contains(&"list"), "{events:?}");
}

#[tokio::test]
async fn ctrl_c_while_the_editor_runs_does_not_stop_the_dashboard() {
    let mut sc = Scenario::new("ctrlc", Wait::Screen("sent to aaaa1111"));
    sc.extra = std::process::id().to_string();
    let o = scenario("ctrlc", sc).await;
    o.result.clone().unwrap();
    assert_eq!(o.posts().len(), 1);
}

#[tokio::test]
async fn the_editor_gets_default_signal_handling() {
    let o = scenario(
        "selfint",
        Scenario::new("selfint", Wait::Screen("nothing sent")),
    )
    .await;
    assert!(o.posts().is_empty());
    // macOS /bin/sh reports the signal; dash (Linux CI) exits 128+2.
    assert!(
        o.screen.contains("signal: 2") || o.screen.contains("exit status: 130"),
        "{}",
        o.screen
    );
    eprintln!(
        "ExitStatus text for a SIGINT-ended editor: {}",
        o.screen
            .lines()
            .find(|l| l.contains("signal: 2") || l.contains("exit status: 130"))
            .unwrap_or_default()
            .trim()
    );
}

#[tokio::test]
async fn the_file_path_is_passed_as_an_argument() {
    let root = Scratch::new("quote-root");
    let weird = root.0.join("a b'c");
    std::fs::create_dir_all(&weird).unwrap();
    let mut sc = Scenario::new("fail", Wait::Screen("exit status: 1"));
    sc.message_dir = Some(weird.clone());
    let o = scenario("quote", sc).await;
    let files = files_in(&weird);
    assert_eq!(files.len(), 1);
    assert_eq!(o.path(), files[0]);
}

#[tokio::test]
async fn a_message_file_that_cannot_be_created_leaves_the_terminal_alone() {
    let mut sc = Scenario::new("text", Wait::Screen("could not create a message file:"));
    sc.message_dir = Some(PathBuf::from("/nonexistent/choco-dir"));
    let o = scenario("nocreate", sc).await;
    assert!(o.log.is_empty());
    assert!(o.posts().is_empty());
}

#[tokio::test]
async fn a_failed_handover_starts_no_editor_and_still_restores() {
    let mut sc = Scenario::new(
        "text",
        Wait::Screen("could not hand the terminal to the editor: boom; nothing sent"),
    );
    sc.suspend_err = Some("boom");
    let o = scenario("handover", sc).await;
    assert_eq!(o.log, vec!["suspend", "resume"]);
    assert!(files_in(&o.msgs).is_empty());
    assert!(o.posts().is_empty());
}

#[tokio::test]
async fn a_failed_take_back_ends_the_loop_and_names_the_text() {
    let mut sc = Scenario::new("text", Wait::Nothing);
    sc.resume_err = Some("tty gone");
    let o = scenario("takeback", sc).await;
    let path = o.path();
    let err = o.result.unwrap_err();
    assert!(
        err.contains("could not take the terminal back: tty gone"),
        "{err}"
    );
    assert!(
        err.contains(&format!("your unsent message is in {}", path.display())),
        "{err}"
    );
    assert_eq!(std::fs::read_to_string(&path).unwrap(), " x \n");
    assert!(o.fake.messages.lock().unwrap().is_empty());

    let mut sc = Scenario::new("empty", Wait::Nothing);
    sc.resume_err = Some("tty gone");
    let o = scenario("takeback-empty", sc).await;
    let err = o.result.unwrap_err();
    assert!(err.contains("could not take the terminal back: tty gone"));
    assert!(!err.contains("unsent message"), "{err}");
    assert!(files_in(&o.msgs).is_empty());

    // The file cannot be read but exists: kept and named, not deleted.
    let mut sc = Scenario::new("bin", Wait::Nothing);
    sc.resume_err = Some("tty gone");
    let o = scenario("takeback-bin", sc).await;
    let path = o.path();
    let err = o.result.unwrap_err();
    assert!(
        err.contains(&format!("your unsent message is in {}", path.display())),
        "{err}"
    );
    assert!(path.exists());

    // An empty file that cannot be removed: the error says so.
    // SAFETY: geteuid has no preconditions.
    if unsafe { libc::geteuid() } != 0 {
        let mut sc = Scenario::new("chmod", Wait::Nothing);
        sc.resume_err = Some("tty gone");
        let o = scenario("takeback-rmfail", sc).await;
        let path = o.path();
        let err = o.result.unwrap_err();
        assert!(
            err.contains(&format!("; could not remove {}:", path.display())),
            "{err}"
        );
    }
}

#[test]
fn a_wait_failure_keeps_text_and_removes_nothing_worth_keeping() {
    let wait = |text: &str| EditOutcome::WaitFailed {
        error: "boom".into(),
        content: Ok(text.into()),
        exists: true,
    };
    let mut app = two_tasks();
    let fx = update(&mut app, edited("aaaa1111-0", wait("x\n")));
    assert!(fx.is_empty());
    assert_eq!(app.kept_messages.len(), 1);
    assert!(
        app.status
            .as_ref()
            .unwrap()
            .full()
            .starts_with("nothing sent: could not wait for the editor: boom")
    );
    let mut app = two_tasks();
    let fx = update(&mut app, edited("aaaa1111-0", wait(" \n")));
    assert!(matches!(&fx[..], [Effect::RemoveFile { .. }]));
}

#[test]
fn a_wait_failure_with_an_unreadable_file_keeps_it_and_names_the_cause() {
    let mut app = two_tasks();
    let fx = update(
        &mut app,
        edited(
            "aaaa1111-0",
            EditOutcome::WaitFailed {
                error: "boom".into(),
                content: Err("bad utf8".into()),
                exists: true,
            },
        ),
    );
    assert!(fx.is_empty());
    assert_eq!(app.kept_messages.len(), 1);
    let full = app.status.as_ref().unwrap().full();
    assert!(full.contains("boom") && full.contains("bad utf8"), "{full}");
}

#[test]
fn a_failed_editor_with_an_empty_file_removes_it() {
    let mut app = two_tasks();
    let fx = update(&mut app, edited("aaaa1111-0", exited(256, "\n")));
    assert!(matches!(&fx[..], [Effect::RemoveFile { .. }]));
    assert!(app.kept_messages.is_empty());
}

#[test]
fn a_long_kept_path_shows_its_file_name() {
    let path = "/var/folders/nb/abcdefghijklmnopqrstuvwxyz0123/T/choco-message-aaaa1111-12345-7.md";
    let mut app = two_tasks();
    update(
        &mut app,
        Msg::Sent {
            id: "aaaa1111-0".into(),
            path: path.into(),
            result: Err(SendFailure::Failed("refused".into())),
        },
    );
    let out = render(&app, 60, 20);
    let last = out.lines().last().unwrap().to_string();
    assert!(
        last.contains("choco-message-aaaa1111-12345-7.md"),
        "{last:?}"
    );
    assert!(last.starts_with("send failed:"), "{last:?}");
    // A realistic refusal stays whole beside the file name at 80 columns.
    let mut app = two_tasks();
    update(
        &mut app,
        Msg::Sent {
            id: "aaaa1111-0".into(),
            path: path.into(),
            result: Err(SendFailure::Failed("task is not at a gate".into())),
        },
    );
    let out = render(&app, 80, 20);
    let last = out.lines().last().unwrap().to_string();
    assert!(
        last.contains("send failed: task is not at a gate"),
        "{last:?}"
    );
    assert!(
        last.contains("choco-message-aaaa1111-12345-7.md"),
        "{last:?}"
    );
    assert!(!last.contains("……"), "{last:?}");
}

#[tokio::test]
async fn an_unreadable_message_is_kept() {
    let o = scenario(
        "unreadable",
        Scenario::new("bin", Wait::Screen("could not read the message:")),
    )
    .await;
    let path = o.path();
    assert!(o.screen.contains("; nothing sent"), "{}", o.screen);
    assert!(o.screen.contains(&path.display().to_string()));
    assert!(path.exists());
    assert!(o.posts().is_empty());
}

#[tokio::test]
async fn a_failed_removal_is_reported() {
    // SAFETY: geteuid has no preconditions.
    if unsafe { libc::geteuid() } == 0 {
        eprintln!("skipped: root can remove files from a read-only directory");
        return;
    }
    let o = scenario(
        "rmfail",
        Scenario::new("chmod", Wait::Screen("could not remove")),
    )
    .await;
    let path = o.path();
    assert!(
        o.screen.contains(&format!(
            "nothing sent: the message was empty; could not remove {}:",
            path.display()
        )),
        "{}",
        o.screen
    );
}

#[tokio::test]
async fn quitting_with_a_send_in_flight_keeps_its_file_and_lists_it() {
    let mut sc = Scenario::new("text", Wait::Posted);
    sc.reply = Reply::Hang;
    let o = scenario("inflight", sc).await;
    o.result.clone().unwrap();
    let path = o.path();
    assert!(path.exists());
    assert_eq!(
        o.app.unanswered_sends,
        vec![("aaaa1111-0".to_string(), path)]
    );
}

#[tokio::test]
async fn a_terminal_that_ignores_the_cursor_query_is_still_taken_back() {
    // `scenario` draws to a backend whose cursor query always fails.
    let o = scenario("nocursor", Scenario::new("text", Wait::Screen("sent to"))).await;
    o.result.clone().unwrap();
    assert_eq!(o.posts().len(), 1);
}

#[tokio::test]
async fn the_message_file_is_created_new_and_private() {
    use std::os::unix::fs::PermissionsExt;
    let _serial = EDITOR_LOCK.lock().await;
    let planted = Scratch::new("planted");
    let next = super::MESSAGE_FILES.load(Ordering::Relaxed);
    let mut stale = Vec::new();
    for n in next..next + 5 {
        let p = planted.0.join(format!(
            "choco-message-aaaa1111-{}-{n}.md",
            std::process::id()
        ));
        std::fs::write(&p, "stale").unwrap();
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o644)).unwrap();
        stale.push(p);
    }
    drop(_serial);
    let mut sc = Scenario::new("fail", Wait::Screen("exit status: 1"));
    sc.message_dir = Some(planted.0.clone());
    let o = scenario("fresh", sc).await;
    let path = o.path();
    assert!(!stale.contains(&path), "reused {path:?}");
    for p in &stale {
        assert_eq!(std::fs::read_to_string(p).unwrap(), "stale");
    }
    let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode, 0o600);
}

fn not_sent_texts(app: &mut App, msg: Msg, w: u16) -> String {
    update(app, msg);
    let mut hay = render(app, w, 20);
    for l in super::app::exit_report(app) {
        hay.push('\n');
        hay.push_str(&l);
    }
    hay
}

#[test]
fn the_full_cause_and_path_stay_readable_when_nothing_is_sent() {
    let refusal = "stage 'awaiting_human_review' needs exactly one /approve or /request-changes \
        marker line: put /request-changes or /approve alone on its own line and send again";
    assert!(refusal.chars().count() > 130);
    let path = "/var/folders/zz/abcdefghijklmnopqrstuvwxyz0123456789abcdefghijkl/T/\
        choco-message-aaaa1111-12345-7.md";
    assert!(path.chars().count() > 95);
    for w in [80u16, 120] {
        let mut app = two_tasks();
        let hay = not_sent_texts(
            &mut app,
            Msg::Sent {
                id: "aaaa1111-0".into(),
                path: path.into(),
                result: Err(SendFailure::Failed(refusal.into())),
            },
            w,
        );
        assert!(hay.contains(refusal), "{w}: refusal\n{hay}");
        assert!(hay.contains(path), "{w}: path\n{hay}");

        let mut app = two_tasks();
        let hay = not_sent_texts(
            &mut app,
            Msg::Edited {
                id: "aaaa1111-0".into(),
                path: path.into(),
                outcome: EditOutcome::Exited {
                    status: status(1 << 8),
                    content: Ok("hello\n".into()),
                    exists: true,
                },
            },
            w,
        );
        assert!(
            hay.contains("nothing sent: the editor exited with exit status: 1"),
            "{w}: status\n{hay}"
        );
        assert!(hay.contains(path), "{w}: path\n{hay}");
    }
}
