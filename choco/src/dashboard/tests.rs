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
use super::{LoopConfig, PanicSignal, run_loop};
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
            kept_work: false,
            workflow_path: None,
            workflow_sha256: None,
            created_at: mins_ago(500),
            updated_at: mins_ago(60),
        },
        current_stage: stage.map(str::to_string),
        stage_entered_at: entered_mins_ago.map(mins_ago),
        loop_counters: json!({}),
        pr: None,
        waiting_on_human: false,
        usage_total: None,
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
    app.daemon_version = Some(chocofactory_core::version::VERSION.to_string());
    app
}

fn list_ok(active: Vec<TaskSummary>, closed: Vec<TaskSummary>) -> Msg {
    Msg::List(Box::new(ListResult {
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
    let [needs, progress, stuck_s, closed_s] = app.sections();
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
    let [needs, progress, _, _] = app.sections();
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
            at: now() + chrono::Duration::seconds(5),
            active: Ok(active),
            closed: Ok(vec![]),
            projects: Some(Ok(vec![Project {
                id: "deadbeef-1234-5678".into(),
                name: "fresh".into(),
                repo_path: None,
                created_at: now(),
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

#[derive(Clone)]
struct Fake {
    tasks: Arc<Vec<TaskSummary>>,
    cancels: Arc<Mutex<Vec<String>>>,
    /// The `keep` field of each cancel request body, in order.
    keeps: Arc<Mutex<Vec<Value>>>,
    /// When set, the detail, cancel and retry endpoints never answer.
    hang: bool,
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
        let statuses: Vec<&str> = q
            .get("status")
            .map(|s| s.split(',').collect())
            .unwrap_or_default();
        let rows: Vec<&TaskSummary> = s
            .tasks
            .iter()
            .filter(|t| statuses.is_empty() || statuses.contains(&t.task.status.as_str()))
            .filter(|t| q.get("project_id").is_none_or(|p| &t.task.project_id == p))
            .collect();
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
    let run = run_loop(
        &mut terminal,
        &mut app,
        client,
        &config,
        &mut krx,
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
    let run = run_loop(
        &mut terminal,
        &mut app,
        client,
        &config,
        &mut krx,
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
        "cancel: task is closed, not open or stuck"
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
    };
    let (ktx, mut krx) = mpsc::unbounded_channel();
    ktx.send(KeyEvent::new(KeyCode::Char('q'), KeyModifiers::NONE))
        .unwrap();
    let mut terminal = Terminal::new(TestBackend::new(120, 30)).unwrap();
    let mut on_frame = |_: &Buffer| {};
    let result = tokio::time::timeout(
        Duration::from_secs(10),
        run_loop(
            &mut terminal,
            &mut app,
            client,
            &config,
            &mut krx,
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
    let run = run_loop(
        &mut terminal,
        &mut app,
        client,
        &config,
        &mut krx,
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
    let run = run_loop(
        &mut terminal,
        &mut app,
        client,
        &config,
        &mut krx,
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
    let run = run_loop(
        &mut terminal,
        &mut app,
        client,
        &config,
        &mut krx,
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
    let run = run_loop(
        &mut terminal,
        &mut app,
        client,
        &config,
        &mut krx,
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
        "e events  o PR  r retry  c cancel  Esc back".into(),
    ]
    .join("\n");
    assert_screen(&render(&app, 100, 30), &expected);
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
    expected.push("PgUp/PgDn scroll  End follow  e/Esc status  o PR  r retry  c cancel".into());
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
fn s_does_nothing_in_either_view_or_the_help() {
    let mut app = board();
    open_detail(&mut app, BUSY);
    let before = render(&app, 100, 30);
    assert!(ch(&mut app, 's').is_empty());
    assert_eq!(render(&app, 100, 30), before);
    ch(&mut app, 'e');
    let before = render(&app, 100, 30);
    assert!(ch(&mut app, 's').is_empty());
    assert_eq!(render(&app, 100, 30), before);
    assert_eq!(detail_state(&app), (true, true, 0));
    ch(&mut app, '?');
    let help = render(&app, 100, 30);
    assert!(help.contains("e           events / status"), "{help}");
    assert!(!help.contains(" s "), "{help}");
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
        "e events  o PR  r retry  c cancel  Esc back".into(),
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
        "e events  o PR  r retry  c cancel  Esc back".into(),
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
    let s = render(app, w, h);
    let lines: Vec<&str> = s.lines().map(str::trim_end).collect();
    let at = format!("{w}x{h}\n{s}");
    assert!(lines.iter().any(|l| l.starts_with("─ progress")), "{at}");
    let rows = step_rows(&s);
    let last = rows.last().unwrap_or_else(|| panic!("no step rows {at}"));
    let first = last.split_whitespace().next().unwrap();
    assert_eq!(first, if stale { "→" } else { "34" }, "{at}");
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
        lines[39], "e events  o PR  r retry  c cancel  Esc back",
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
