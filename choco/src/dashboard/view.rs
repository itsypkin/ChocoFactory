//! Drawing (#164). `draw` reads the [`App`] and paints one frame; the only
//! thing it writes back is the visible list height and scroll offset, which
//! `update` needs for paging.

use chocofactory_core::models::TaskSummary;
use chrono::{DateTime, Local};
use ratatui::Frame;
use ratatui::layout::{Alignment, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::Line;
use ratatui::widgets::{Block, Borders, Clear, Paragraph};

use super::app::{App, Detail, Level, PromptKind, Scope, Section, View, fmt_duration, max_laps};

pub const MIN_WIDTH: u16 = 40;
pub const MIN_HEIGHT: u16 = 10;

pub fn draw(frame: &mut Frame, app: &App) {
    let area = frame.area();
    if area.width < MIN_WIDTH || area.height < MIN_HEIGHT {
        let text = "terminal too small (need 40×10)";
        let lines: Vec<Line> = if area.width as usize >= text.chars().count() {
            vec![Line::from(text)]
        } else {
            vec![Line::from("terminal too small"), Line::from("(need 40×10)")]
        };
        frame.render_widget(Paragraph::new(lines), area);
        return;
    }
    match &app.view {
        View::List => draw_list(frame, app, area),
        View::Detail(d) => draw_detail(frame, app, d, area),
    }
    if let Some(prompt) = &app.prompt {
        draw_prompt(frame, app, prompt, area);
    }
    if app.help {
        draw_help(frame, area);
    }
}

// ---- styles -------------------------------------------------------------

fn colored(app: &App, color: Color) -> Style {
    if app.color {
        Style::default().fg(color)
    } else {
        Style::default()
    }
}

fn section_style(app: &App, s: Section) -> Style {
    let c = match s {
        Section::NeedsYou => Color::Yellow,
        Section::InProgress => Color::Cyan,
        Section::Stuck => Color::Red,
        Section::Closed => Color::DarkGray,
    };
    colored(app, c).add_modifier(Modifier::BOLD)
}

fn error_style(app: &App) -> Style {
    if app.color {
        Style::default().fg(Color::Red)
    } else {
        Style::default().add_modifier(Modifier::BOLD)
    }
}

// ---- text helpers -------------------------------------------------------

/// Cuts `s` to `w` characters, ending in `…` when something was dropped.
pub fn fit(s: &str, w: usize) -> String {
    let s: String = s
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect();
    if s.chars().count() <= w {
        return s;
    }
    if w == 0 {
        return String::new();
    }
    let mut out: String = s.chars().take(w - 1).collect();
    out.push('…');
    out
}

fn pad(s: &str, w: usize) -> String {
    let mut out = fit(s, w);
    let n = out.chars().count();
    out.extend(std::iter::repeat_n(' ', w.saturating_sub(n)));
    out
}

fn wrap_chars(s: &str, w: usize) -> Vec<String> {
    let w = w.max(1);
    let chars: Vec<char> = s.chars().collect();
    if chars.is_empty() {
        return vec![String::new()];
    }
    chars.chunks(w).map(|c| c.iter().collect()).collect()
}

fn age(app: &App, from: Option<DateTime<chrono::Utc>>) -> String {
    match from {
        Some(t) => fmt_duration(app.now - t),
        None => "?".to_string(),
    }
}

fn first_line(s: &str) -> &str {
    s.lines().next().unwrap_or("")
}

// ---- list ---------------------------------------------------------------

#[derive(Clone, Copy)]
struct Cols {
    project: bool,
    stage: bool,
    pr: bool,
    laps: bool,
    cost: bool,
}

fn cols_for(app: &App, width: u16) -> Cols {
    Cols {
        project: app.all_projects() && width >= 60,
        stage: width >= 60,
        pr: width >= 80,
        laps: width >= 80,
        // 80 plus the column and its separator, so the title never has less
        // room than it has at 80.
        cost: width >= 91,
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Key {
    Id,
    Project,
    Title,
    Stage,
    Reason,
    Pr,
    Time,
    Laps,
    Status,
    Cost,
}

struct Col {
    key: Key,
    w: usize,
}

/// The widths of the whole board's columns, computed once from every row so
/// each section lines up with the others.
struct Grid {
    cols: Cols,
    project_w: usize,
    stage_w: usize,
    /// Left after every other column; titles are cut first.
    title_w: usize,
}

const ID_W: usize = 8;
const PR_W: usize = 6;
const TIME_W: usize = 10;
const LAPS_W: usize = 5;
const COST_W: usize = 9;
/// Project and stage shrink before the title drops below this.
const TITLE_MIN: usize = 20;

fn grid_for(app: &App, sections: &[Vec<&TaskSummary>; 4], width: usize) -> Grid {
    let cols = cols_for(app, width as u16);
    let mut project_w = if cols.project {
        sections
            .iter()
            .flatten()
            .map(|t| app.project_label(&t.task.project_id).chars().count())
            .max()
            .unwrap_or(7)
            .clamp(7, 14)
    } else {
        0
    };
    let mut stage_w = if cols.stage {
        sections
            .iter()
            .zip(Section::ALL)
            .flat_map(|(ts, s)| ts.iter().map(move |t| (s, t)))
            .filter_map(|(s, t)| match s {
                Section::NeedsYou | Section::InProgress => {
                    Some(t.current_stage.as_deref().unwrap_or("-").chars().count())
                }
                Section::Closed => Some(t.task.status.chars().count()),
                Section::Stuck => None,
            })
            .max()
            .unwrap_or(5)
            .clamp(6, 22)
    } else {
        0
    };
    let mut fixed_cols = vec![ID_W, TIME_W];
    fixed_cols.extend([PR_W].into_iter().filter(|_| cols.pr));
    fixed_cols.extend([LAPS_W].into_iter().filter(|_| cols.laps));
    fixed_cols.extend([COST_W].into_iter().filter(|_| cols.cost));
    let n = fixed_cols.len() + 1 + usize::from(cols.project) + usize::from(cols.stage);
    let rest = |project_w: usize, stage_w: usize| {
        let used: usize = fixed_cols.iter().sum::<usize>() + project_w + stage_w + (n - 1) * 2;
        width.saturating_sub(2 + used)
    };
    // Shrink the project column, then the stage column, before the title.
    let mut title_w = rest(project_w, stage_w);
    if title_w < TITLE_MIN && cols.project {
        let give = (TITLE_MIN - title_w).min(project_w.saturating_sub(7));
        project_w -= give;
        title_w = rest(project_w, stage_w);
    }
    if title_w < TITLE_MIN && cols.stage {
        let give = (TITLE_MIN - title_w).min(stage_w.saturating_sub(8));
        stage_w -= give;
        title_w = rest(project_w, stage_w);
    }
    Grid {
        cols,
        project_w,
        stage_w,
        title_w,
    }
}

/// The columns of one section's rows. The id, project, title, PR, time and
/// laps columns sit at the same place in every section; a stuck task's
/// reason takes the stage and PR columns' room.
fn columns(section: Section, g: &Grid) -> Vec<Col> {
    let c = |key, w| Col { key, w };
    let mut v = vec![c(Key::Id, ID_W)];
    if g.cols.project {
        v.push(c(Key::Project, g.project_w));
    }
    let mut title_w = g.title_w;
    let mut mid = None;
    let mut pr = g.cols.pr;
    match section {
        Section::NeedsYou | Section::InProgress => {
            if g.cols.stage {
                mid = Some(c(Key::Stage, g.stage_w));
            }
        }
        Section::Closed => {
            if g.cols.stage {
                mid = Some(c(Key::Status, g.stage_w));
            }
        }
        Section::Stuck => {
            if g.cols.stage {
                let w = g.stage_w + if pr { 2 + PR_W } else { 0 };
                mid = Some(c(Key::Reason, w));
                pr = false;
            } else {
                // Narrow: no stage column to borrow, so share the title's room.
                let reason = (title_w.saturating_sub(2) / 2).min(24);
                title_w -= reason + 2;
                mid = Some(c(Key::Reason, reason));
            }
        }
    }
    v.push(c(Key::Title, title_w));
    v.extend(mid);
    if pr {
        v.push(c(Key::Pr, PR_W));
    }
    v.push(c(Key::Time, TIME_W));
    if g.cols.laps {
        v.push(c(Key::Laps, LAPS_W));
    }
    if g.cols.cost {
        v.push(c(Key::Cost, COST_W));
    }
    v
}

fn cell(app: &App, section: Section, t: &TaskSummary, key: Key) -> String {
    match key {
        Key::Id => t.task.id.chars().take(8).collect(),
        Key::Project => app.project_label(&t.task.project_id),
        Key::Title => t.task.title.clone(),
        Key::Stage => t.current_stage.clone().unwrap_or_else(|| "-".into()),
        Key::Reason => first_line(t.task.stuck_reason.as_deref().unwrap_or("")).to_string(),
        Key::Pr => {
            t.pr.as_ref()
                .map(|p| format!("#{}", p.number))
                .unwrap_or_default()
        }
        Key::Time => match section {
            Section::NeedsYou | Section::InProgress => age(app, t.stage_entered_at),
            Section::Stuck => fmt_duration(app.now - t.task.updated_at),
            Section::Closed => format!("{} ago", fmt_duration(app.now - t.task.updated_at)),
        },
        Key::Laps => match max_laps(t) {
            0 => String::new(),
            n => format!("×{n}"),
        },
        Key::Status => t.task.status.clone(),
        Key::Cost => list_cost(t),
    }
}

/// The list's cost cell: `≈$1.23`, `no data` for a task with no recorded
/// turns, `unknown` when turns exist but none reported a cost.
fn list_cost(t: &TaskSummary) -> String {
    match &t.usage_total {
        None => "no data".to_string(),
        Some(u) => match u.cost_usd {
            None => "unknown".to_string(),
            Some(c) => format!("≈${c:.2}"),
        },
    }
}

fn task_line(app: &App, section: Section, t: &TaskSummary, cols: &[Col]) -> String {
    let mut out = String::new();
    for (i, col) in cols.iter().enumerate() {
        if i > 0 {
            out.push_str("  ");
        }
        let text = if col.key == Key::Laps && section != Section::InProgress {
            String::new()
        } else {
            cell(app, section, t, col.key)
        };
        out.push_str(&pad(&text, col.w));
    }
    out.trim_end().to_string()
}

/// The column headings, once for the whole board.
fn heading_line(g: &Grid) -> String {
    // Any non-stuck section has the full set of columns.
    let cols = columns(Section::NeedsYou, g);
    let mut out = String::new();
    for (i, col) in cols.iter().enumerate() {
        if i > 0 {
            out.push_str("  ");
        }
        let text = match col.key {
            Key::Id => "id",
            Key::Project => "project",
            Key::Title => "title",
            Key::Stage => "stage",
            Key::Pr => "PR",
            Key::Laps => "laps",
            Key::Cost => "cost",
            _ => "",
        };
        out.push_str(&pad(text, col.w));
    }
    out.trim_end().to_string()
}

/// The section's title on its own; the labels that only apply to this
/// section sit at their columns, and only where they clear the title.
fn header_line(section: Section, count: usize, cols: &[Col]) -> String {
    let title = format!("{} ({count})", section.title());
    let mut line: Vec<char> = title.chars().collect();
    let mut x = 2;
    for (i, col) in cols.iter().enumerate() {
        if i > 0 {
            x += 2;
        }
        let text = match col.key {
            Key::Reason => "reason",
            Key::Status => "status",
            Key::Time => match section {
                Section::NeedsYou => "waiting",
                Section::InProgress => "in stage",
                Section::Stuck => "stuck for",
                Section::Closed => "closed",
            },
            _ => "",
        };
        if !text.is_empty() && x >= line.len() + 2 {
            line.resize(x, ' ');
            line.extend(fit(text, col.w).chars());
        }
        x += col.w;
    }
    line.into_iter().collect()
}

enum Row<'a> {
    Header(Section, usize),
    Task(Section, &'a TaskSummary),
}

fn draw_list(frame: &mut Frame, app: &App, area: Rect) {
    let width = area.width as usize;
    let list_area = Rect::new(area.x, area.y + 2, area.width, area.height - 3);

    draw_header(
        frame,
        app,
        Rect::new(area.x, area.y, area.width, 1),
        "choco dashboard",
    );

    let sections = app.sections();
    let grid = grid_for(app, &sections, width);
    frame.render_widget(
        Paragraph::new(Line::styled(
            fit(&format!("  {}", heading_line(&grid)), width),
            colored(app, Color::DarkGray),
        )),
        Rect::new(area.x, area.y + 1, area.width, 1),
    );

    let mut rows: Vec<Row> = Vec::new();
    for (s, tasks) in Section::ALL.iter().zip(&sections) {
        rows.push(Row::Header(*s, tasks.len()));
        rows.extend(tasks.iter().map(|t| Row::Task(*s, t)));
    }

    let h = list_area.height as usize;
    let sel = rows
        .iter()
        .position(|r| matches!(r, Row::Task(_, t) if Some(&t.task.id) == app.selected.as_ref()));
    let mut off = app.list_offset.get();
    if let Some(i) = sel {
        let top = if i > 0 && matches!(rows[i - 1], Row::Header(..)) {
            i - 1
        } else {
            i
        };
        if top < off {
            off = top;
        }
        if i >= off + h {
            off = i + 1 - h;
        }
    }
    off = off.min(rows.len().saturating_sub(h));
    app.list_offset.set(off);
    app.list_page.set(h);

    let lines: Vec<Line> = rows
        .iter()
        .skip(off)
        .take(h)
        .map(|row| match row {
            Row::Header(s, n) => {
                let c = columns(*s, &grid);
                Line::styled(header_line(*s, *n, &c), section_style(app, *s))
            }
            Row::Task(s, t) => {
                let c = columns(*s, &grid);
                let selected = Some(&t.task.id) == app.selected.as_ref();
                let text = format!(
                    "{}{}",
                    if selected { "▶ " } else { "  " },
                    task_line(app, *s, t, &c)
                );
                let style = if selected {
                    Style::default().add_modifier(Modifier::REVERSED)
                } else {
                    Style::default()
                };
                Line::styled(fit(&text, width), style)
            }
        })
        .collect();
    frame.render_widget(Paragraph::new(lines), list_area);

    draw_bottom(
        frame,
        app,
        Rect::new(area.x, area.y + area.height - 1, area.width, 1),
        "↑↓ move  ⏎ open  o PR  r retry  c cancel  ? help  q quit",
    );
}

fn version_text(app: &App) -> String {
    match &app.daemon_version {
        None => "daemon ?".to_string(),
        Some(v) if v == chocofactory_core::version::VERSION => format!("daemon {v}"),
        Some(v) => format!("daemon {v} (choco {})", chocofactory_core::version::VERSION),
    }
}

fn draw_header(frame: &mut Frame, app: &App, area: Rect, name: &str) {
    let width = area.width as usize;
    let mode = match &app.scope {
        Scope::AllProjects => "all projects".to_string(),
        Scope::Project { name } => format!("project {name}"),
    };
    let base = format!("{name} · {mode}");
    let refreshed = match app.last_refresh {
        Some(t) => format!("refreshed {} ago", fmt_duration(app.now - t)),
        None => "not refreshed yet".to_string(),
    };
    let ver = version_text(app);
    let lefts = [
        format!("{base} · {} · {ver}", app.base_url),
        format!("{base} · {ver}"),
        base.clone(),
    ];
    let rights = [format!("{refreshed} · ? help"), refreshed.clone()];
    let candidates = [
        (&lefts[0], &rights[0]),
        (&lefts[1], &rights[0]),
        (&lefts[2], &rights[0]),
        (&lefts[2], &rights[1]),
    ];
    let len = |s: &str| s.chars().count();
    let text = candidates
        .iter()
        .find(|(l, r)| len(l) + 2 + len(r) <= width)
        .map(|(l, r)| format!("{l}{}{r}", " ".repeat(width - len(l) - len(r))))
        .unwrap_or_else(|| {
            let r = &rights[1];
            if width > len(r) + 12 {
                format!("{}  {r}", pad(&lefts[2], width - len(r) - 2))
            } else {
                fit(&lefts[2], width)
            }
        });
    frame.render_widget(
        Paragraph::new(Line::styled(
            text,
            Style::default().add_modifier(Modifier::BOLD),
        )),
        area,
    );
}

fn draw_bottom(frame: &mut Frame, app: &App, area: Rect, hints: &str) {
    let width = area.width as usize;
    // A failed poll outranks an informational status (e.g. "cancelled x"),
    // so an unreachable daemon is never hidden until the next key press.
    let shown = app
        .status
        .as_ref()
        .filter(|s| s.level == Level::Error || app.poll_error.is_none());
    let line = if let Some(status) = shown {
        let style = match status.level {
            Level::Error => error_style(app),
            Level::Info => Style::default(),
        };
        Line::styled(fit(&status.text, width), style)
    } else if let Some(err) = &app.poll_error {
        let data = match app.last_refresh {
            Some(t) => format!("data {} old", fmt_duration(app.now - t)),
            None => "no data yet".to_string(),
        };
        // The error is the long, variable part: cut it, never the rest.
        let head = "daemon unreachable: ";
        let tail = format!("; retrying every {} ({data})", app.interval_label);
        let room = width.saturating_sub(head.chars().count() + tail.chars().count());
        Line::styled(
            format!("{head}{}{tail}", fit(err, room.max(8))),
            error_style(app),
        )
    } else {
        Line::styled(fit(hints, width), colored(app, Color::DarkGray))
    };
    frame.render_widget(Paragraph::new(line), area);
}

// ---- detail -------------------------------------------------------------

fn separator(title: &str, width: usize) -> String {
    let head = format!("─ {title} ");
    let n = head.chars().count();
    format!("{head}{}", "─".repeat(width.saturating_sub(n)))
}

/// Most events the status view's tail shows.
const STATUS_TAIL: usize = 5;
/// Lines a wrapping field (`Stuck`, `Waiting for`) may take.
const WRAP_MAX: usize = 3;

/// One field row of the status view.
struct FRow {
    label: &'static str,
    value: String,
    error: bool,
    wrap: bool,
    /// Most screen lines a wrapped row may take (cut with `…` beyond).
    cap: usize,
    /// Which cut drops this row when the screen is short (1 = first to go).
    drop: u8,
}

impl FRow {
    fn new(label: &'static str, value: String) -> Self {
        FRow {
            label,
            value,
            error: false,
            wrap: false,
            cap: WRAP_MAX,
            drop: 0,
        }
    }
}

fn waiting_text(stage: &str, id: &str) -> Option<String> {
    match stage {
        "awaiting_human_review" => Some(
            "your verdict: a PR comment with /approve or /request-changes on its own line"
                .to_string(),
        ),
        "escalate_to_human" => Some(format!(
            "a note to resume it: choco task send {id} --text \"…\""
        )),
        "spec_questions" => Some(format!(
            "your answers to the spec check's questions: choco task send {id} --text \"…\""
        )),
        _ => None,
    }
}

/// The rows of the fields block: `choco task status`'s rows from the last
/// answer when there is one, adjusted for the dashboard; else what the
/// snapshot can give.
fn field_rows(app: &App, d: &Detail) -> Vec<FRow> {
    let snap = d.snapshot.as_ref();
    let mut rows: Vec<FRow> = Vec::new();
    let mut paths = 0;
    if let Some(v) = &d.data {
        for (label, value) in crate::render::task_fields(v) {
            let mut row = FRow::new(label, value);
            match label {
                "Title" => continue,
                "Project" => {
                    let pid = snap
                        .map(|t| t.task.project_id.as_str())
                        .or_else(|| v.get("project_id").and_then(|p| p.as_str()));
                    if let Some(pid) = pid {
                        row.value = app.project_label(pid);
                    }
                }
                "Workflow" | "Workflow file" => {
                    paths += 1;
                    if label == "Workflow file" || paths == 2 {
                        row.drop = 4;
                    }
                }
                "Status" => {
                    if let Some(t) = snap {
                        row.value = t.task.status.clone();
                    }
                }
                // The snapshot is fresher: rebuilt after Status below.
                "Stuck" if snap.is_some() => continue,
                "Stuck" => {
                    row.error = true;
                    row.wrap = true;
                }
                "Role" => row.drop = 1,
                "Created" => row.drop = 2,
                "Repo" => row.drop = 3,
                _ => {}
            }
            rows.push(row);
        }
    } else if let Some(t) = snap {
        rows.push(FRow::new("ID", t.task.id.clone()));
        rows.push(FRow::new("Project", app.project_label(&t.task.project_id)));
        rows.push(FRow::new("Workflow", t.task.workflow_def.clone()));
        rows.push(FRow::new("Status", t.task.status.clone()));
    }
    if let Some(t) = snap
        && let Some(pos) = rows.iter().position(|r| r.label == "Status")
        && t.task.status == "stuck"
        && let Some(reason) = &t.task.stuck_reason
    {
        let mut row = FRow::new("Stuck", crate::render::single_line(reason));
        row.error = true;
        row.wrap = true;
        rows.insert(pos + 1, row);
    }

    // Stage, with the time in it (or how long ago it ended), then PR and
    // what the task waits for.
    let stage_name = snap
        .and_then(|t| t.current_stage.as_deref())
        .or_else(|| d.data.as_ref().and_then(crate::render::detail_stage));
    let stage_value = match (snap, stage_name) {
        (Some(t), name) => {
            let name = name.unwrap_or("-");
            match t.task.status.as_str() {
                "closed" | "cancelled" => format!(
                    "{name} ({} {} ago)",
                    t.task.status,
                    fmt_duration(app.now - t.task.updated_at)
                ),
                _ => match t.stage_entered_at {
                    Some(at) => format!("{name} for {}", fmt_duration(app.now - at)),
                    None => name.to_string(),
                },
            }
        }
        (None, Some(name)) => name.to_string(),
        (None, None) => String::new(),
    };
    let at = match rows.iter().position(|r| r.label == "Stage") {
        Some(pos) => {
            rows[pos].value = stage_value;
            pos
        }
        None => {
            if snap.is_some() || !stage_value.is_empty() {
                rows.push(FRow::new("Stage", stage_value));
            }
            rows.len().saturating_sub(1)
        }
    };
    let mut extra = Vec::new();
    if let Some(t) = snap {
        if let Some(pr) = &t.pr {
            extra.push(FRow::new("PR", format!("#{} {}", pr.number, pr.url)));
        }
        if t.task.status == "open"
            && let Some(stage) = t.current_stage.as_deref()
            && t.waiting_on_human
            && let Some(text) = waiting_text(stage, &t.task.id)
        {
            let mut row = FRow::new("Waiting for", text);
            row.wrap = true;
            extra.push(row);
        }
    }
    let at = (at + 1).min(rows.len());
    let after = at + extra.len();
    rows.splice(at..at, extra);
    if let Some(value) = cost_value(d) {
        let mut row = FRow::new("Cost", value);
        row.drop = 1;
        let after = after.min(rows.len());
        rows.insert(after, row);
    }
    rows
}

/// The `Cost` row's value: the full answer's totals when it has arrived,
/// else the list snapshot's cost; `None` when there is neither.
fn cost_value(d: &Detail) -> Option<String> {
    use crate::render::cost_with_label;
    if let Some(v) = &d.data {
        let Some(usage) = v.get("usage").filter(|u| u.is_object()) else {
            return Some("no data".to_string());
        };
        let label = usage
            .get("billing_label")
            .and_then(|l| l.as_str())
            .unwrap_or("estimated");
        let duration = |key: &str| {
            usage
                .get(key)
                .and_then(|ms| ms.as_i64())
                .map(|ms| fmt_duration(chrono::Duration::milliseconds(ms)))
                .unwrap_or_else(|| "no data".to_string())
        };
        return Some(format!(
            "{} · wall {} · active {}",
            cost_with_label(usage.get("cost_usd").and_then(|c| c.as_f64()), label),
            duration("wall_time_ms"),
            duration("active_time_ms"),
        ));
    }
    let snap = d.snapshot.as_ref()?;
    Some(match &snap.usage_total {
        Some(u) => cost_with_label(u.cost_usd, &u.billing_label),
        None => "no data".to_string(),
    })
}

/// A row as screen lines: cut to the width, or wrapped for `wrap` rows.
fn row_lines(row: &FRow, label_w: usize, width: usize) -> Vec<String> {
    let head = format!("{:<label_w$}  ", row.label);
    if !row.wrap {
        return vec![fit(format!("{head}{}", row.value).trim_end(), width)];
    }
    let room = width.saturating_sub(label_w + 2).max(1);
    let mut parts = wrap_chars(&crate::render::single_line(&row.value), room);
    if parts.len() > row.cap {
        parts.truncate(row.cap);
        let last = parts.last_mut().expect("wrap keeps a line");
        *last = fit(&format!("{last}…"), room);
    }
    let pad = " ".repeat(label_w + 2);
    parts
        .into_iter()
        .enumerate()
        .map(|(i, p)| {
            if i == 0 {
                fit(&format!("{head}{p}"), width)
            } else {
                fit(&format!("{pad}{p}"), width)
            }
        })
        .collect()
}

fn event_line(e: &chocofactory_core::models::Event, width: usize) -> String {
    let time = e.created_at.with_timezone(&Local).format("%H:%M:%S");
    let text = format!(
        "  {time}  {:<14} {}",
        e.event_type,
        crate::render::event_summary(e)
    );
    fit(&text, width)
}

fn title_line(app: &App, d: &Detail, width: usize) -> Line<'static> {
    let right = if d.expanded {
        "e status · Esc back · ? help"
    } else {
        "e events · Esc back · ? help"
    };
    let title = match (&d.snapshot, &d.data) {
        (Some(t), _) => t.task.title.clone(),
        (None, Some(v)) => v
            .get("title")
            .and_then(|t| t.as_str())
            .unwrap_or("-")
            .to_string(),
        (None, None) => "(task is no longer listed)".to_string(),
    };
    let _ = app;
    let title = pad(&title, width.saturating_sub(right.chars().count() + 2));
    Line::styled(
        format!("{title}  {right}"),
        Style::default().add_modifier(Modifier::BOLD),
    )
}

fn draw_detail(frame: &mut Frame, app: &App, d: &Detail, area: Rect) {
    let width = area.width as usize;
    let body_h = area.height.saturating_sub(1) as usize;
    let plain = Style::default();
    let mut lines: Vec<Line> = vec![title_line(app, d, width)];

    let ev_title = if d.reconnecting {
        "events: reconnecting"
    } else if d.following {
        "events (following)"
    } else {
        "events (scrolled · End follows)"
    };
    let ev_sep_style = if d.reconnecting {
        error_style(app)
    } else {
        plain
    };

    if d.expanded {
        let ev_h = body_h.saturating_sub(2);
        d.page.set(ev_h.max(1));
        let n = d.events.len();
        let back = d.scroll_back.min(n.saturating_sub(ev_h));
        let end = n - back;
        let start = end.saturating_sub(ev_h);
        lines.push(Line::styled(separator(ev_title, width), ev_sep_style));
        for e in &d.events[start..end] {
            lines.push(Line::raw(event_line(e, width)));
        }
        draw_detail_body(frame, app, lines, area, EXPANDED_HINTS);
        return;
    }

    // ---- status view ----
    let mut rows = field_rows(app, d);
    let widest = |rows: &[FRow]| {
        rows.iter()
            .map(|r| r.label.chars().count())
            .max()
            .unwrap_or(0)
    };
    let label_w_cell = std::cell::Cell::new(widest(&rows));
    let has_task = d.snapshot.is_some() || d.data.is_some();

    // Progress section: lines, and the separator's title.
    let mut prog_title = "progress".to_string();
    let mut prog_err = false;
    let mut prog: Vec<(String, Style)> = Vec::new();
    let mut cuttable = true;
    match (&d.data, &d.error) {
        (None, None) => prog.push(("  loading…".into(), plain)),
        (None, Some(e)) => {
            // An error is never "cut to earlier steps": it keeps its head
            // and the bottom clip takes the rest.
            cuttable = false;
            prog.extend(
                wrap_chars(
                    &format!("could not load the task: {e}"),
                    width.saturating_sub(2).max(1),
                )
                .into_iter()
                .map(|l| (format!("  {l}"), error_style(app))),
            )
        }
        (Some(v), err) => {
            if let Some(e) = err {
                prog_title = format!("progress (refresh failed: {e})");
                prog_err = true;
            }
            match crate::render::detail_progress(v) {
                Some(l) => prog.extend(l.into_iter().map(|l| (l, plain))),
                None => prog.push((
                    "  (no workflow state — the task has not started)".into(),
                    plain,
                )),
            }
        }
    }
    let counters = d.data.as_ref().and_then(crate::render::loop_counters_line);

    let events_n = d.events.len();
    let want_tail = events_n.clamp(1, STATUS_TAIL);
    let row_h = |rows: &[FRow]| -> usize {
        rows.iter()
            .map(|r| row_lines(r, label_w_cell.get(), width).len())
            .sum()
    };
    let block = |rows: &[FRow], prog_h: usize| -> usize {
        1 + if has_task { row_h(rows) } else { 0 } + 1 + prog_h + usize::from(counters.is_some())
    };

    let mut keep_prog = prog.len();
    let mut tail = 0;
    if !has_task {
        rows.clear();
    }
    if block(&rows, keep_prog) + 1 + want_tail <= body_h {
        tail = want_tail;
    } else {
        // The tail shrinks first, one event at a time; without room for an
        // event line its separator goes too.
        let avail = body_h.saturating_sub(block(&rows, keep_prog));
        if avail >= 2 {
            tail = (avail - 1).min(want_tail);
        }
        if tail == 0 {
            // Then the status block: older progress lines first...
            let fits = |m: usize, rows: &[FRow]| {
                let h = if m < prog.len() { m + 1 } else { m };
                block(rows, h) <= body_h
            };
            if cuttable {
                keep_prog = (1..=prog.len())
                    .rev()
                    .find(|&m| m != prog.len().saturating_sub(1) && fits(m, &rows))
                    .unwrap_or(if prog.len() <= 2 { prog.len() } else { 1 });
            }
            // ...then the Role, Created, Repo and workflow path rows.
            for cut in 1..=4u8 {
                let h = if keep_prog < prog.len() {
                    keep_prog + 1
                } else {
                    keep_prog
                };
                if block(&rows, h) <= body_h {
                    break;
                }
                rows.retain(|r| r.drop != cut);
            }
            // Still too tall: wrapped rows shrink a line at a time, then
            // trailing rows go, so the progress separator and one progress
            // (or error) line stay on screen. Extra error lines and the
            // loop counters are left to the bottom clip.
            let tight = |rows: &[FRow]| {
                let h = if keep_prog < prog.len() {
                    keep_prog + 1
                } else {
                    prog.len().min(if cuttable { 2 } else { 1 })
                };
                block(rows, h) - usize::from(counters.is_some())
            };
            const KEEP: [&str; 5] = ["ID", "Project", "Workflow", "Status", "Stage"];
            while tight(&rows) > body_h {
                if let Some(i) = rows.iter().position(|r| r.wrap && r.cap > 1) {
                    rows[i].cap -= 1;
                    continue;
                }
                match rows.iter().rposition(|r| !KEEP.contains(&r.label)) {
                    Some(i) => {
                        rows.remove(i);
                    }
                    None => break,
                }
            }
            label_w_cell.set(widest(&rows));
        }
    }

    if has_task {
        for r in &rows {
            let style = if r.error { error_style(app) } else { plain };
            for l in row_lines(r, label_w_cell.get(), width) {
                lines.push(Line::styled(l, style));
            }
        }
    }
    lines.push(Line::styled(
        fit(&separator(&prog_title, width), width),
        if prog_err { error_style(app) } else { plain },
    ));
    let hidden = prog.len() - keep_prog;
    if hidden > 0 {
        lines.push(Line::raw(fit(
            // Never 1: replacing a single line with the "earlier" line saves
            // nothing, so the cut skips it.
            &format!("  … {hidden} earlier steps"),
            width,
        )));
    }
    for (l, st) in &prog[hidden..] {
        lines.push(Line::styled(fit(l, width), *st));
    }
    if let Some(c) = &counters {
        lines.push(Line::raw(fit(c, width)));
    }
    if tail > 0 {
        lines.push(Line::styled(
            separator(
                if d.reconnecting {
                    "events: reconnecting"
                } else {
                    "last events (e expands)"
                },
                width,
            ),
            ev_sep_style,
        ));
        if events_n == 0 {
            lines.push(Line::raw("  (no events yet)"));
        }
        for e in &d.events[events_n.saturating_sub(tail)..] {
            lines.push(Line::raw(event_line(e, width)));
        }
    }
    draw_detail_body(frame, app, lines, area, STATUS_HINTS);
}

const STATUS_HINTS: &str = "e events  o PR  r retry  c cancel  Esc back";
const EXPANDED_HINTS: &str = "PgUp/PgDn scroll  End follow  e status  o PR  r retry  c cancel";

fn draw_detail_body(frame: &mut Frame, app: &App, lines: Vec<Line>, area: Rect, hints: &str) {
    let body_h = area.height.saturating_sub(1);
    frame.render_widget(
        Paragraph::new(lines),
        Rect::new(area.x, area.y, area.width, body_h),
    );
    draw_bottom(
        frame,
        app,
        Rect::new(area.x, area.y + area.height - 1, area.width, 1),
        hints,
    );
}

// ---- overlays -----------------------------------------------------------

fn centered(area: Rect, w: u16, h: u16) -> Rect {
    let w = w.min(area.width);
    let h = h.min(area.height);
    Rect::new(
        area.x + (area.width - w) / 2,
        area.y + (area.height - h) / 2,
        w,
        h,
    )
}

fn draw_prompt(frame: &mut Frame, app: &App, prompt: &super::app::Prompt, area: Rect) {
    let (verb, body, yes) = match prompt.kind {
        PromptKind::Cancel => (
            "Cancel",
            "This kills its agent and deletes its worktree and branch. It cannot be undone.",
            "y = cancel",
        ),
        PromptKind::Retry => ("Retry", "Its stage runs again.", "y = retry"),
    };
    let w = area.width.min(66);
    let inner = (w as usize).saturating_sub(4);
    let mut lines = vec![Line::styled(
        format!(
            "{verb} \"{}\"?",
            fit(&prompt.title, inner.saturating_sub(verb.len() + 4))
        ),
        Style::default().add_modifier(Modifier::BOLD),
    )];
    lines.extend(wrap_words(body, inner).into_iter().map(Line::from));
    lines.push(Line::from(format!("[y/N]  {yes}   N = keep")).alignment(Alignment::Right));
    let r = centered(area, w, lines.len() as u16 + 2);
    frame.render_widget(Clear, r);
    frame.render_widget(
        Paragraph::new(lines).block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(colored(app, Color::Yellow)),
        ),
        r,
    );
}

fn wrap_words(s: &str, w: usize) -> Vec<String> {
    let mut lines = Vec::new();
    let mut cur = String::new();
    for word in s.split_whitespace() {
        if !cur.is_empty() && cur.chars().count() + 1 + word.chars().count() > w {
            lines.push(std::mem::take(&mut cur));
        }
        if !cur.is_empty() {
            cur.push(' ');
        }
        cur.push_str(word);
    }
    if !cur.is_empty() {
        lines.push(cur);
    }
    lines
}

fn draw_help(frame: &mut Frame, area: Rect) {
    let lines = vec![
        Line::from("↑ k / ↓ j   move            ⏎     open detail"),
        Line::from("PgUp PgDn   page            Esc   back"),
        Line::from("g / G       top / bottom    o     open PR"),
        Line::from("⇥ / ⇧⇥      next section    r     retry stuck"),
        Line::from("?           this help       c     cancel"),
        Line::from("q  Ctrl-C   quit"),
        Line::from("e           events / status"),
    ];
    let r = centered(area, 52, lines.len() as u16 + 2);
    frame.render_widget(Clear, r);
    frame.render_widget(
        Paragraph::new(lines).block(Block::default().borders(Borders::ALL).title(" keys ")),
        r,
    );
}
