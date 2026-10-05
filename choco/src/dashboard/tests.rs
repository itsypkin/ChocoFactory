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
use chocofactory_core::models::{Project, PullRequestRef, RetryOutcome, Task, TaskSummary};
use chrono::{DateTime, TimeZone, Utc};
use ratatui::Terminal;
use ratatui::backend::TestBackend;
use ratatui::buffer::Buffer;
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use serde_json::{Value, json};
use tokio::sync::mpsc;

use super::app::*;
use super::view::draw;
use super::{LoopConfig, run_loop};
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
    }
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
        summary(
            "3f2a91c0-aaaa",
            "Interactive terminal dashboard (#164)",
            "open",
            Some("awaiting_human_review"),
            Some(125),
            "p-alpha",
        ),
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
    let waiting = summary(
        "b81e0d44-cccc",
        "Checkout totals rounding (#42)",
        "open",
        Some("escalate_to_human"),
        Some(14),
        "p-beta",
    );
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
        summary(
            "a",
            "a",
            "open",
            Some("awaiting_human_review"),
            Some(10),
            "p",
        ),
        summary(
            "b",
            "b",
            "open",
            Some("awaiting_human_review"),
            Some(120),
            "p",
        ),
        stuck(
            summary(
                "c",
                "c",
                "stuck",
                Some("awaiting_human_review"),
                Some(1),
                "p",
            ),
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
        summary("g", "g", "open", Some("escalate_to_human"), None, "p"),
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
    assert_eq!(ids(&progress), ["e", "d", "h"]);
    // Longest stuck first.
    assert_eq!(ids(&stuck_s), ["f", "c"]);
    assert_eq!(ids(&closed_s), ["z1", "z2"]);
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
    active.push(summary(
        "0000-new",
        "New",
        "open",
        Some("awaiting_human_review"),
        Some(9999),
        "p-alpha",
    ));
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

#[test]
fn wide_screen_shows_headers_counts_pr_durations_and_laps() {
    let app = board();
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
    assert!(!s.contains("#171"), "{s}");
    assert!(!s.contains("×2"), "{s}");
    assert!(s.contains("awaiting_human_review"), "{s}");
    assert!(s.contains("chocofactory"), "{s}");
}

#[test]
fn narrow_screen_drops_stage_and_project() {
    let app = board();
    let s = render(&app, 55, 15);
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
fn detail_view_shows_header_trail_and_events() {
    let mut app = board();
    ch(&mut app, 'G');
    press(&mut app, KeyCode::Up);
    let effects = press(&mut app, KeyCode::Enter);
    assert_eq!(
        effects,
        vec![
            Effect::FetchDetail("7d22e1a8-dddd".into()),
            Effect::OpenSocket("7d22e1a8-dddd".into())
        ]
    );
    update(
        &mut app,
        Msg::Detail {
            id: "7d22e1a8-dddd".into(),
            result: Ok(json!({"stage_trail": [
                {"payload": {"stage": "coding", "outcome": null}},
                {"payload": {"stage": "revising", "outcome": "changes_requested"}},
            ]})),
        },
    );
    update(
        &mut app,
        Msg::Socket {
            id: "7d22e1a8-dddd".into(),
            msg: SocketMsg::Down,
        },
    );
    let s = render(&app, 100, 24);
    for want in [
        "Flaky lock test (#98)",
        "7d22e1a8-dddd",
        "project chocofactory",
        "workflow coding-task",
        "stuck: interrupted by a usage limit",
        "coding → revising (via changes_requested)",
        "events: reconnecting",
        "Esc back",
    ] {
        assert!(s.contains(want), "missing {want:?} in:\n{s}");
    }
    assert_eq!(press(&mut app, KeyCode::Esc), vec![Effect::CloseSocket]);
    assert!(matches!(app.view, View::List));
}

#[test]
fn detail_events_follow_until_scrolled() {
    let mut app = board();
    press(&mut app, KeyCode::Enter);
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
        let t = s.tasks.iter().find(|t| t.task.id == id).unwrap();
        let mut v = serde_json::to_value(t).unwrap();
        v["stage_trail"] = json!([{"payload": {"stage": "coding", "outcome": null}}]);
        axum::Json(v)
    }
    async fn cancel(State(s): State<Fake>, Path(id): Path<String>) -> StatusCode {
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
        wait_for(&latest, "hello streamed").await;
        wait_for(&latest, "second streamed").await;
        wait_for(&latest, "events (following)").await;
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
