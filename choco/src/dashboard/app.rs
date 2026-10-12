//! The dashboard's state and its pure `update` function (#164). Nothing in
//! here does I/O: a key, a tick or a result arrives as a [`Msg`], and the
//! things to go and do come back as [`Effect`]s for `mod.rs` to carry out.

use std::cell::Cell;
use std::collections::HashMap;
use std::time::Duration;

use chocofactory_core::models::{
    Event, PauseOutcome, Project, RetryOutcome, TASK_STATUS_CANCELLED, TASK_STATUS_CLOSED,
    TASK_STATUS_OPEN, TASK_STATUS_PAUSED, TASK_STATUS_PAUSING, TASK_STATUS_STUCK, TaskSummary,
};
use chrono::{DateTime, Utc};
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use serde_json::Value;

/// Most events the detail view keeps; older ones are dropped.
const MAX_EVENTS: usize = 1000;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Section {
    NeedsYou,
    InProgress,
    /// Tasks an operator paused, and the ones still finishing a turn
    /// before they are paused.
    Paused,
    Stuck,
    Closed,
}

impl Section {
    pub const ALL: [Section; 5] = [
        Section::NeedsYou,
        Section::InProgress,
        Section::Paused,
        Section::Stuck,
        Section::Closed,
    ];

    pub fn title(self) -> &'static str {
        match self {
            Section::NeedsYou => "NEEDS YOU",
            Section::InProgress => "IN PROGRESS",
            Section::Paused => "PAUSED",
            Section::Stuck => "STUCK",
            Section::Closed => "RECENTLY CLOSED",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Scope {
    AllProjects,
    Project { name: String },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PromptKind {
    Retry,
    Cancel,
}

/// A confirmation box. It holds the id of the task it was opened on, so a
/// refresh that moves another task under the cursor cannot redirect `y`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Prompt {
    pub kind: PromptKind,
    pub task_id: String,
    pub title: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Level {
    Info,
    Error,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StatusLine {
    pub text: String,
    pub level: Level,
}

/// State of the full-screen detail view.
pub struct Detail {
    pub id: String,
    /// The task's latest summary, kept so the view survives the task
    /// dropping out of the lists (e.g. beyond `--closed`).
    pub snapshot: Option<TaskSummary>,
    /// The last good `GET /tasks/{id}` answer; `None` before one arrived.
    pub data: Option<Value>,
    /// Why the latest `GET /tasks/{id}` failed; cleared by the next success.
    /// With `data` set it is a refresh that failed, never silently dropped.
    pub error: Option<String>,
    /// The expanded event view (`e`) instead of the status view.
    pub expanded: bool,
    pub events: Vec<Event>,
    pub reconnecting: bool,
    pub following: bool,
    /// How many lines above the bottom the event window sits.
    pub scroll_back: usize,
    /// Height of the event pane at the last draw, for paging.
    pub page: Cell<usize>,
}

#[allow(clippy::large_enum_variant)] // one value, held for the whole run
pub enum View {
    List,
    Detail(Detail),
}

/// Where the search popup's task list stands.
pub enum SearchLoad {
    Loading,
    Failed(String),
    Loaded(Vec<TaskSummary>),
}

/// The `/` search popup (#292): an overlay on whatever view is open.
pub struct Search {
    /// Which open of the popup this is; a load result for another one is stale.
    pub generation: u64,
    pub query: String,
    pub load: SearchLoad,
    /// Id of the highlighted result.
    pub selected: Option<String>,
    /// Draw-time feedback: result rows visible, and the scroll offset.
    pub page: Cell<usize>,
    pub offset: Cell<usize>,
}

impl Search {
    /// The loaded tasks that match the query, newest first. Recomputed on
    /// every call so it can never be stale.
    pub fn results(&self) -> Vec<&TaskSummary> {
        match &self.load {
            SearchLoad::Loaded(tasks) => super::search::search(tasks, &self.query),
            SearchLoad::Loading | SearchLoad::Failed(_) => Vec::new(),
        }
    }

    /// Keeps the selected task if it is still a result, else the first one.
    fn reselect(&mut self) {
        let results = self.results();
        let keep = self
            .selected
            .as_ref()
            .is_some_and(|id| results.iter().any(|t| &t.task.id == id));
        if !keep {
            self.selected = results.first().map(|t| t.task.id.clone());
        }
    }

    fn step(&mut self, delta: isize) {
        let ids: Vec<String> = self.results().iter().map(|t| t.task.id.clone()).collect();
        let Some(pos) = self
            .selected
            .as_ref()
            .and_then(|sel| ids.iter().position(|i| i == sel))
        else {
            return;
        };
        let to = pos.saturating_add_signed(delta).min(ids.len() - 1);
        self.selected = Some(ids[to].clone());
    }
}

/// Outcome of one poll.
pub struct ListResult {
    pub at: DateTime<Utc>,
    pub active: Result<Vec<TaskSummary>, String>,
    pub closed: Result<Vec<TaskSummary>, String>,
    /// `Some` only when this poll also fetched the project names.
    pub projects: Option<Result<Vec<Project>, String>>,
    /// The daemon's `GET /server`, read with the lists. `None` when the read
    /// failed: the header keeps the last values it had.
    pub server: Option<chocofactory_core::models::ServerStatus>,
}

pub enum SocketMsg {
    /// A (re)connect succeeded: the backlog about to arrive replaces the buffer.
    Connected,
    Event(Box<Event>),
    Down,
}

pub enum ActionKind {
    Cancel,
    Retry,
    Pause,
    Resume,
}

pub enum ActionOk {
    Cancelled,
    Retried(RetryOutcome),
    Paused(PauseOutcome),
    Resumed,
}

pub enum Msg {
    Key(KeyEvent),
    Tick(DateTime<Utc>),
    List(Box<ListResult>),
    Detail {
        id: String,
        result: Result<Value, String>,
    },
    Socket {
        id: String,
        msg: SocketMsg,
    },
    Action {
        kind: ActionKind,
        id: String,
        result: Result<ActionOk, String>,
    },
    /// The search popup's task list (or why it could not load).
    Search {
        generation: u64,
        result: Result<Vec<TaskSummary>, String>,
    },
    /// Launching the browser failed.
    OpenFailed {
        url: String,
        error: String,
    },
}

#[derive(Debug, PartialEq, Eq)]
pub enum Effect {
    Fetch {
        projects: bool,
    },
    FetchDetail(String),
    OpenSocket(String),
    CloseSocket,
    /// Load every task for the search popup opened as `generation`.
    LoadSearch {
        generation: u64,
    },
    Cancel(String),
    Retry(String),
    /// A graceful pause only: the dashboard has no `--now`.
    Pause(String),
    Resume(String),
    OpenUrl(String),
    Quit,
}

pub struct App {
    pub scope: Scope,
    pub base_url: String,
    pub daemon_version: Option<String>,
    /// The server pool of agent turns, as the last `GET /server` said.
    pub server_pool: Option<chocofactory_core::models::AgentPool>,
    pub interval: Duration,
    pub interval_label: String,
    pub color: bool,
    pub ssh: bool,

    pub active: Vec<TaskSummary>,
    pub closed: Vec<TaskSummary>,
    pub projects: HashMap<String, String>,
    pub now: DateTime<Utc>,
    pub last_refresh: Option<DateTime<Utc>>,
    pub last_poll: Option<DateTime<Utc>>,
    pub fetching: bool,
    pub poll_error: Option<String>,

    pub selected: Option<String>,
    /// Index of the selection in the flat row list, to fall back on when
    /// the selected task disappears.
    pub selected_pos: usize,
    pub view: View,
    pub help: bool,
    pub prompt: Option<Prompt>,
    pub status: Option<StatusLine>,
    pub search: Option<Search>,
    /// Opens of the search popup so far; only ever rises.
    pub search_gen: u64,

    /// Draw-time feedback: rows visible in the list, and the scroll offset.
    pub list_page: Cell<usize>,
    pub list_offset: Cell<usize>,
}

impl App {
    pub fn new(
        scope: Scope,
        base_url: String,
        interval: Duration,
        interval_label: String,
        now: DateTime<Utc>,
    ) -> Self {
        App {
            scope,
            base_url,
            daemon_version: None,
            server_pool: None,
            interval,
            interval_label,
            color: std::env::var_os("NO_COLOR").is_none(),
            ssh: std::env::var_os("SSH_CONNECTION").is_some()
                || std::env::var_os("SSH_TTY").is_some(),
            active: Vec::new(),
            closed: Vec::new(),
            projects: HashMap::new(),
            now,
            last_refresh: None,
            last_poll: None,
            fetching: false,
            poll_error: None,
            selected: None,
            selected_pos: 0,
            view: View::List,
            help: false,
            prompt: None,
            status: None,
            search: None,
            search_gen: 0,
            list_page: Cell::new(10),
            list_offset: Cell::new(0),
        }
    }

    pub fn all_projects(&self) -> bool {
        matches!(self.scope, Scope::AllProjects)
    }

    /// The five sections, each in its display order. Every status in
    /// `ALL_TASK_STATUSES` has its own arm, so a new one cannot land in
    /// none of them unnoticed (a test pins that).
    pub fn sections(&self) -> [Vec<&TaskSummary>; 5] {
        let mut needs: Vec<&TaskSummary> = Vec::new();
        let mut progress: Vec<&TaskSummary> = Vec::new();
        let mut paused: Vec<&TaskSummary> = Vec::new();
        let mut stuck: Vec<&TaskSummary> = Vec::new();
        let mut closed: Vec<&TaskSummary> = Vec::new();
        for t in &self.active {
            match t.task.status.as_str() {
                TASK_STATUS_STUCK => stuck.push(t),
                TASK_STATUS_PAUSING | TASK_STATUS_PAUSED => paused.push(t),
                TASK_STATUS_OPEN => {
                    if t.waiting_on_human {
                        needs.push(t);
                    } else {
                        progress.push(t);
                    }
                }
                // Not fetched into `active`, but classified all the same.
                TASK_STATUS_CLOSED | TASK_STATUS_CANCELLED => closed.push(t),
                _ => {}
            }
        }
        // Oldest entry first; an unknown entry time sorts last.
        let by_entry = |a: &&TaskSummary, b: &&TaskSummary| {
            let key = |t: &TaskSummary| (t.stage_entered_at.is_none(), t.stage_entered_at);
            key(a).cmp(&key(b)).then_with(|| a.task.id.cmp(&b.task.id))
        };
        needs.sort_by(by_entry);
        progress.sort_by(by_entry);
        stuck.sort_by(|a, b| {
            a.task
                .updated_at
                .cmp(&b.task.updated_at)
                .then_with(|| a.task.id.cmp(&b.task.id))
        });
        // Longest-paused last: the one that has waited longest is on top.
        paused.sort_by(|a, b| {
            a.task
                .paused_at
                .cmp(&b.task.paused_at)
                .then_with(|| a.task.id.cmp(&b.task.id))
        });
        closed.extend(self.closed.iter());
        [needs, progress, paused, stuck, closed]
    }

    /// Every task id in display order.
    pub fn flat_ids(&self) -> Vec<String> {
        self.sections()
            .iter()
            .flat_map(|s| s.iter().map(|t| t.task.id.clone()))
            .collect()
    }

    pub fn find(&self, id: &str) -> Option<&TaskSummary> {
        self.active
            .iter()
            .chain(self.closed.iter())
            .find(|t| t.task.id == id)
    }

    /// The name shown for a project: its name, else the id's first 8 chars.
    pub fn project_label(&self, project_id: &str) -> String {
        match (self.projects.get(project_id), &self.scope) {
            (Some(name), _) => name.clone(),
            // One-project mode never fetches the project list.
            (None, Scope::Project { name }) => name.clone(),
            (None, Scope::AllProjects) => project_id.chars().take(8).collect(),
        }
    }

    fn needs_projects(&self) -> bool {
        self.all_projects()
            && self
                .active
                .iter()
                .chain(self.closed.iter())
                .any(|t| !self.projects.contains_key(&t.task.project_id))
    }

    fn set_status(&mut self, text: impl Into<String>, level: Level) {
        self.status = Some(StatusLine {
            text: text.into(),
            level,
        });
    }

    fn detail_id(&self) -> Option<&str> {
        match &self.view {
            View::Detail(d) => Some(&d.id),
            View::List => None,
        }
    }

    /// The task an action applies to: the open detail's, else the selection.
    fn action_target(&self) -> Option<String> {
        self.detail_id()
            .map(str::to_string)
            .or_else(|| self.selected.clone())
    }

    fn select_pos(&mut self, pos: usize) {
        let ids = self.flat_ids();
        if ids.is_empty() {
            self.selected = None;
            self.selected_pos = 0;
            return;
        }
        let pos = pos.min(ids.len() - 1);
        self.selected = Some(ids[pos].clone());
        self.selected_pos = pos;
    }

    /// Re-anchors the selection after the data changed: same task id if it
    /// is still there, else the row now at the old position.
    fn reselect(&mut self) {
        let ids = self.flat_ids();
        if ids.is_empty() {
            self.selected = None;
            self.selected_pos = 0;
            return;
        }
        if let Some(sel) = &self.selected
            && let Some(pos) = ids.iter().position(|i| i == sel)
        {
            self.selected_pos = pos;
            return;
        }
        let pos = self.selected_pos.min(ids.len() - 1);
        self.selected = Some(ids[pos].clone());
        self.selected_pos = pos;
    }

    fn refresh_snapshot(&mut self) {
        let Some(id) = self.detail_id().map(str::to_string) else {
            return;
        };
        let found = self.find(&id).cloned();
        if let (View::Detail(d), Some(found)) = (&mut self.view, found) {
            d.snapshot = Some(found);
        }
    }
}

pub use crate::render::loop_count;

/// Every non-zero loop counter, by stage name.
pub fn laps_by_stage(t: &TaskSummary) -> Vec<(String, u64)> {
    match &t.loop_counters {
        Value::Object(o) => o
            .iter()
            .map(|(k, v)| (k.clone(), loop_count(v)))
            .filter(|(_, n)| *n > 0)
            .collect(),
        _ => Vec::new(),
    }
}

/// The largest loop counter, 0 when there is none.
pub fn max_laps(t: &TaskSummary) -> u64 {
    laps_by_stage(t).iter().map(|(_, n)| *n).max().unwrap_or(0)
}

pub fn update(app: &mut App, msg: Msg) -> Vec<Effect> {
    match msg {
        Msg::Key(key) => on_key(app, key),
        Msg::Tick(now) => {
            app.now = now;
            let due = !app.fetching
                && app
                    .last_poll
                    .is_none_or(|p| (now - p).to_std().is_ok_and(|d| d >= app.interval));
            if !due {
                return Vec::new();
            }
            app.fetching = true;
            let mut effects = vec![Effect::Fetch {
                projects: app.needs_projects(),
            }];
            if let Some(id) = app.detail_id() {
                effects.push(Effect::FetchDetail(id.to_string()));
            }
            effects
        }
        Msg::List(result) => {
            let ListResult {
                at,
                active,
                closed,
                projects,
                server,
            } = *result;
            app.now = at;
            if let Some(server) = server {
                app.daemon_version = Some(server.version);
                app.server_pool = server.agent_pools.into_iter().find(|p| p.pool == "server");
            }
            app.fetching = false;
            app.last_poll = Some(at);
            match projects {
                Some(Ok(list)) => {
                    app.projects = list.into_iter().map(|p| (p.id, p.name)).collect();
                }
                // When the lists failed too, the unreachable line already
                // says so; this one is for a projects-only failure.
                Some(Err(err)) if active.is_ok() && closed.is_ok() => {
                    app.set_status(format!("could not fetch projects: {err}"), Level::Error)
                }
                Some(Err(_)) | None => {}
            }
            match (active, closed) {
                (Ok(active), Ok(closed)) => {
                    app.active = active;
                    app.closed = closed;
                    app.last_refresh = Some(at);
                    app.poll_error = None;
                    app.reselect();
                    app.refresh_snapshot();
                }
                (a, c) => {
                    // Keep what is on screen; say what failed.
                    let err = a.err().or(c.err()).unwrap_or_default();
                    app.poll_error = Some(err);
                }
            }
            Vec::new()
        }
        Msg::Detail { id, result } => {
            if let View::Detail(d) = &mut app.view
                && d.id == id
            {
                match result {
                    Ok(v) => {
                        d.data = Some(v);
                        d.error = None;
                    }
                    Err(e) => d.error = Some(e),
                }
            }
            Vec::new()
        }
        Msg::Socket { id, msg } => {
            if let View::Detail(d) = &mut app.view
                && d.id == id
            {
                match msg {
                    SocketMsg::Connected => {
                        d.events.clear();
                        d.reconnecting = false;
                    }
                    SocketMsg::Event(event) => {
                        if !d.events.iter().any(|e| e.id == event.id) {
                            d.events.push(*event);
                            // Scrolled back: keep the same lines in view.
                            if !d.following {
                                d.scroll_back += 1;
                            }
                            if d.events.len() > MAX_EVENTS {
                                let excess = d.events.len() - MAX_EVENTS;
                                d.events.drain(..excess);
                            }
                        }
                    }
                    SocketMsg::Down => d.reconnecting = true,
                }
            }
            Vec::new()
        }
        Msg::Action { kind, id, result } => {
            let short: String = id.chars().take(8).collect();
            match (kind, result) {
                (ActionKind::Cancel, Ok(_)) => {
                    app.set_status(format!("cancelled {short}"), Level::Info)
                }
                (ActionKind::Retry, Ok(ActionOk::Retried(outcome))) => app.set_status(
                    if outcome.resumed {
                        "retried: resumed"
                    } else {
                        "retried: fresh"
                    },
                    Level::Info,
                ),
                (ActionKind::Retry, Ok(_)) => app.set_status("retried", Level::Info),
                (ActionKind::Pause, Ok(ActionOk::Paused(outcome))) => app.set_status(
                    if outcome.status == "paused" {
                        format!("paused {short}")
                    } else {
                        format!("pausing {short}: waiting for its turn to finish")
                    },
                    Level::Info,
                ),
                (ActionKind::Pause, Ok(_)) => {
                    app.set_status(format!("paused {short}"), Level::Info)
                }
                (ActionKind::Resume, Ok(_)) => {
                    app.set_status(format!("resumed {short}"), Level::Info)
                }
                (ActionKind::Pause, Err(e)) => {
                    app.set_status(format!("pause failed: {e}"), Level::Error);
                    return Vec::new();
                }
                (ActionKind::Resume, Err(e)) => {
                    app.set_status(format!("resume failed: {e}"), Level::Error);
                    return Vec::new();
                }
                (ActionKind::Cancel, Err(e)) => {
                    app.set_status(format!("cancel failed: {e}"), Level::Error);
                    return Vec::new();
                }
                (ActionKind::Retry, Err(e)) => {
                    app.set_status(format!("retry failed: {e}"), Level::Error);
                    return Vec::new();
                }
            }
            // A successful action refreshes right away.
            app.fetching = true;
            vec![Effect::Fetch {
                projects: app.needs_projects(),
            }]
        }
        Msg::Search { generation, result } => {
            // A result for a popup that was closed or reopened since is
            // dropped on purpose: its request just finishes unseen.
            if let Some(s) = &mut app.search
                && s.generation == generation
            {
                s.load = match result {
                    Ok(tasks) => SearchLoad::Loaded(tasks),
                    Err(e) => SearchLoad::Failed(e),
                };
                s.reselect();
            }
            Vec::new()
        }
        Msg::OpenFailed { url, error } => {
            app.set_status(format!("could not open {url}: {error}"), Level::Error);
            Vec::new()
        }
    }
}

fn on_key(app: &mut App, key: KeyEvent) -> Vec<Effect> {
    if key.kind == KeyEventKind::Release {
        return Vec::new();
    }
    if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
        return vec![Effect::Quit];
    }
    // The popup owns every key (it is typed into), and handling one must not
    // clear the board's status line.
    if app.search.is_some() {
        return search_key(app, key);
    }
    app.status = None;

    // A confirmation box swallows the key: only `y` acts.
    if let Some(prompt) = app.prompt.take() {
        if key.code == KeyCode::Char('y') {
            return vec![match prompt.kind {
                PromptKind::Cancel => Effect::Cancel(prompt.task_id),
                PromptKind::Retry => Effect::Retry(prompt.task_id),
            }];
        }
        return Vec::new();
    }
    if app.help {
        match key.code {
            KeyCode::Char('?') | KeyCode::Esc => app.help = false,
            KeyCode::Char('q') => return vec![Effect::Quit],
            _ => {}
        }
        return Vec::new();
    }

    match key.code {
        KeyCode::Char('?') => {
            app.help = true;
            return Vec::new();
        }
        KeyCode::Char('/') => {
            app.search_gen = app.search_gen.wrapping_add(1);
            let generation = app.search_gen;
            app.search = Some(Search {
                generation,
                query: String::new(),
                load: SearchLoad::Loading,
                selected: None,
                page: Cell::new(10),
                offset: Cell::new(0),
            });
            return vec![Effect::LoadSearch { generation }];
        }
        KeyCode::Char('o') => return open_pr(app),
        KeyCode::Char('r') => {
            retry_prompt(app);
            return Vec::new();
        }
        KeyCode::Char('c') => {
            cancel_prompt(app);
            return Vec::new();
        }
        KeyCode::Char('p') => return pause_toggle(app),
        _ => {}
    }

    if matches!(app.view, View::Detail(_)) {
        return detail_key(app, key);
    }
    list_key(app, key)
}

fn search_key(app: &mut App, key: KeyEvent) -> Vec<Effect> {
    let Some(s) = app.search.as_mut() else {
        return Vec::new();
    };
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    let alt = key.modifiers.contains(KeyModifiers::ALT);
    let page = s.page.get().max(1) as isize;
    match key.code {
        KeyCode::Esc => app.search = None,
        KeyCode::Enter => return open_search_result(app),
        KeyCode::Up => s.step(-1),
        KeyCode::Down => s.step(1),
        KeyCode::Char('p') if ctrl => s.step(-1),
        KeyCode::Char('n') if ctrl => s.step(1),
        KeyCode::PageUp => s.step(-page),
        KeyCode::PageDown => s.step(page),
        KeyCode::Backspace => {
            s.query.pop();
            s.reselect();
        }
        KeyCode::Char(c) if !ctrl && !alt => {
            s.query.push(c);
            s.reselect();
        }
        _ => {}
    }
    Vec::new()
}

/// `⏎` in the popup: open the highlighted task's detail view.
fn open_search_result(app: &mut App) -> Vec<Effect> {
    let Some(s) = &app.search else {
        return Vec::new();
    };
    let Some(found) = s
        .selected
        .as_ref()
        .and_then(|id| s.results().into_iter().find(|t| &t.task.id == id))
        .cloned()
    else {
        return Vec::new();
    };
    app.search = None;
    let id = found.task.id.clone();
    if let Some(pos) = app.flat_ids().iter().position(|i| *i == id) {
        app.selected = Some(id.clone());
        app.selected_pos = pos;
    }
    open_detail(app, id, Some(found))
}

fn open_detail(app: &mut App, id: String, snapshot: Option<TaskSummary>) -> Vec<Effect> {
    app.view = View::Detail(Detail {
        id: id.clone(),
        snapshot,
        data: None,
        error: None,
        expanded: false,
        events: Vec::new(),
        reconnecting: false,
        following: true,
        scroll_back: 0,
        page: Cell::new(10),
    });
    vec![Effect::FetchDetail(id.clone()), Effect::OpenSocket(id)]
}

fn open_pr(app: &mut App) -> Vec<Effect> {
    let Some(id) = app.action_target() else {
        return Vec::new();
    };
    let pr = app
        .find(&id)
        .and_then(|t| t.pr.clone())
        .or_else(|| match &app.view {
            View::Detail(d) => d.snapshot.as_ref().and_then(|t| t.pr.clone()),
            View::List => None,
        });
    let Some(pr) = pr else {
        app.set_status("no PR yet", Level::Info);
        return Vec::new();
    };
    if app.ssh {
        app.set_status(format!("PR #{}: {}", pr.number, pr.url), Level::Info);
        return Vec::new();
    }
    vec![Effect::OpenUrl(pr.url)]
}

fn target_summary(app: &App) -> Option<TaskSummary> {
    let id = app.action_target()?;
    app.find(&id).cloned().or_else(|| match &app.view {
        View::Detail(d) => d.snapshot.clone(),
        View::List => None,
    })
}

fn retry_prompt(app: &mut App) {
    let Some(t) = target_summary(app) else {
        return;
    };
    if matches!(
        t.task.status.as_str(),
        TASK_STATUS_PAUSING | TASK_STATUS_PAUSED
    ) {
        app.set_status(
            format!("task is {}; press p to resume", t.task.status),
            Level::Info,
        );
        return;
    }
    if t.task.status != "stuck" {
        app.set_status(
            format!("retry: task is {}, not stuck", t.task.status),
            Level::Info,
        );
        return;
    }
    app.prompt = Some(Prompt {
        kind: PromptKind::Retry,
        task_id: t.task.id,
        title: t.task.title,
    });
}

/// `p`: pauses an `open` task (graceful) and resumes a `pausing` or
/// `paused` one. No confirmation: both are undone by the other.
fn pause_toggle(app: &mut App) -> Vec<Effect> {
    let Some(t) = target_summary(app) else {
        return Vec::new();
    };
    match t.task.status.as_str() {
        TASK_STATUS_OPEN => vec![Effect::Pause(t.task.id)],
        TASK_STATUS_PAUSING | TASK_STATUS_PAUSED => vec![Effect::Resume(t.task.id)],
        other => {
            app.set_status(
                format!("pause: task is {other}, not open or paused"),
                Level::Info,
            );
            Vec::new()
        }
    }
}

fn cancel_prompt(app: &mut App) {
    let Some(t) = target_summary(app) else {
        return;
    };
    if !matches!(
        t.task.status.as_str(),
        TASK_STATUS_OPEN | TASK_STATUS_PAUSING | TASK_STATUS_PAUSED | TASK_STATUS_STUCK
    ) {
        app.set_status(
            format!(
                "cancel: task is {}, not open, paused or stuck",
                t.task.status
            ),
            Level::Info,
        );
        return;
    }
    app.prompt = Some(Prompt {
        kind: PromptKind::Cancel,
        task_id: t.task.id,
        title: t.task.title,
    });
}

fn list_key(app: &mut App, key: KeyEvent) -> Vec<Effect> {
    let page = app.list_page.get().max(1);
    let last = app.flat_ids().len().saturating_sub(1);
    let pos = app.selected_pos;
    match key.code {
        KeyCode::Char('q') => return vec![Effect::Quit],
        KeyCode::Up | KeyCode::Char('k') => app.select_pos(pos.saturating_sub(1)),
        KeyCode::Down | KeyCode::Char('j') => app.select_pos((pos + 1).min(last)),
        KeyCode::PageUp => app.select_pos(pos.saturating_sub(page)),
        KeyCode::PageDown => app.select_pos((pos + page).min(last)),
        KeyCode::Char('g') | KeyCode::Home => app.select_pos(0),
        KeyCode::Char('G') | KeyCode::End => app.select_pos(last),
        KeyCode::Tab => jump_section(app, true),
        KeyCode::BackTab => jump_section(app, false),
        KeyCode::Enter => {
            if let Some(id) = app.selected.clone() {
                let snapshot = app.find(&id).cloned();
                return open_detail(app, id, snapshot);
            }
        }
        _ => {}
    }
    Vec::new()
}

/// Moves to the first row of the next (or previous) non-empty section.
fn jump_section(app: &mut App, forward: bool) {
    let sections = app.sections();
    let sizes: Vec<usize> = sections.iter().map(Vec::len).collect();
    let starts: Vec<usize> = sizes
        .iter()
        .scan(0, |acc, n| {
            let s = *acc;
            *acc += n;
            Some(s)
        })
        .collect();
    let Some(current) = app.selected.as_ref().and_then(|id| {
        sections
            .iter()
            .position(|s| s.iter().any(|t| &t.task.id == id))
    }) else {
        return;
    };
    let n = sizes.len();
    for step in 1..=n {
        let idx = if forward {
            (current + step) % n
        } else {
            (current + n - step) % n
        };
        if sizes[idx] > 0 {
            app.select_pos(starts[idx]);
            return;
        }
    }
}

fn detail_key(app: &mut App, key: KeyEvent) -> Vec<Effect> {
    let View::Detail(d) = &mut app.view else {
        return Vec::new();
    };
    let page = d.page.get().max(1);
    let max_back = d.events.len().saturating_sub(1);
    match key.code {
        KeyCode::Char('q') => {
            app.view = View::List;
            return vec![Effect::CloseSocket];
        }
        KeyCode::Esc if !d.expanded => {
            app.view = View::List;
            return vec![Effect::CloseSocket];
        }
        KeyCode::Esc => d.expanded = false,
        KeyCode::Char('e') => {
            d.expanded = !d.expanded;
            if d.expanded {
                d.following = true;
                d.scroll_back = 0;
            }
        }
        // The status view has no scrolling.
        _ if !d.expanded => {}
        KeyCode::PageUp => {
            d.following = false;
            d.scroll_back = (d.scroll_back + page).min(max_back);
        }
        KeyCode::PageDown => d.scroll_back = d.scroll_back.saturating_sub(page),
        KeyCode::Up | KeyCode::Char('k') => {
            d.following = false;
            d.scroll_back = (d.scroll_back + 1).min(max_back);
        }
        KeyCode::Down | KeyCode::Char('j') => d.scroll_back = d.scroll_back.saturating_sub(1),
        KeyCode::End | KeyCode::Char('G') => {
            d.scroll_back = 0;
            d.following = true;
        }
        _ => {}
    }
    Vec::new()
}

/// `45s`, `12m`, `2h05m`, `3d04h`; whole days from 100 on (`123d`), so the
/// time column's width holds.
pub fn fmt_duration(d: chrono::Duration) -> String {
    let s = d.num_seconds().max(0);
    if s < 60 {
        format!("{s}s")
    } else if s < 3600 {
        format!("{}m", s / 60)
    } else if s < 86_400 {
        format!("{}h{:02}m", s / 3600, (s % 3600) / 60)
    } else if s >= 100 * 86_400 {
        format!("{}d", s / 86_400)
    } else {
        format!("{}d{:02}h", s / 86_400, (s % 86_400) / 3600)
    }
}
