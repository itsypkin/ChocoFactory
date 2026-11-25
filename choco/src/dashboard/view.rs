//! Drawing (#164). `draw` reads the [`App`] and paints one frame; the only
//! thing it writes back is the visible list height and scroll offset, which
//! `update` needs for paging.

use chocofactory_core::models::{TASK_STATUS_OPEN, TaskSummary};
use chrono::{DateTime, Local};
use ratatui::Frame;
use ratatui::buffer::CellWidth;
use ratatui::layout::{Alignment, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph};

use crate::render::{
    LABEL_BASE, LABEL_CLOSES, LABEL_CREATED, LABEL_ID, LABEL_PAUSED, LABEL_PROJECT, LABEL_REPO,
    LABEL_REVIEW, LABEL_ROLE, LABEL_STAGE, LABEL_STATUS, LABEL_STUCK, LABEL_TITLE, LABEL_WORKFLOW,
    LABEL_WORKFLOW_FILE,
};

use super::app::{
    App, Detail, Level, PromptKind, Scope, Search, SearchLoad, Section, View, fmt_duration,
    max_laps,
};

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
    if let Some(search) = &app.search {
        draw_search(frame, app, search, area);
    }
    if let Some(prompt) = &app.prompt {
        draw_prompt(frame, app, prompt, area);
    }
    if app.help {
        draw_help(frame, area);
    }
}

// ---- styles -------------------------------------------------------------

/// The one accent colour of the detail view's section headers (#211).
const ACCENT: Color = Color::Cyan;

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
        Section::Paused => Color::Blue,
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

/// `s` with each control character as the space it is shown as.
fn printable(s: &str) -> String {
    s.chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect()
}

/// The terminal cells one grapheme takes, as ratatui draws it.
fn grapheme_cells(grapheme: &str) -> usize {
    usize::from(grapheme.cell_width())
}

/// The terminal cells `s` takes: not its character count, since a wide
/// character takes two and a combining mark none.
fn cell_len(s: &str) -> usize {
    // One cell each, without the cost of splitting into graphemes: this runs
    // for every field of every frame.
    if s.is_ascii() {
        return s.len();
    }
    Span::raw(s)
        .styled_graphemes(Style::default())
        .map(|g| grapheme_cells(g.symbol))
        .sum()
}

/// `s` in pieces of at most `w` cells, cut between graphemes. A grapheme
/// wider than `w` is a piece of its own.
fn chunk_cells(s: &str, w: usize) -> Vec<String> {
    let mut chunks = Vec::new();
    let mut cur = String::new();
    let mut used = 0;
    for g in Span::raw(s).styled_graphemes(Style::default()) {
        let n = grapheme_cells(g.symbol);
        if used + n > w && !cur.is_empty() {
            chunks.push(std::mem::take(&mut cur));
            used = 0;
        }
        cur.push_str(g.symbol);
        used += n;
    }
    if !cur.is_empty() {
        chunks.push(cur);
    }
    chunks
}

/// `s` made printable and cut to `w` cells, ending in `…` when something was
/// dropped.
pub fn fit(s: &str, w: usize) -> String {
    clip(&printable(s), w)
}

/// [`fit`] without rewriting any of `s`.
fn clip(s: &str, w: usize) -> String {
    if cell_len(s) <= w {
        return s.to_string();
    }
    if w == 0 {
        return String::new();
    }
    let mut used = 0;
    let span = Span::raw(s);
    let mut out: String = span
        .styled_graphemes(Style::default())
        .map_while(|g| {
            used += grapheme_cells(g.symbol);
            (used < w).then_some(g.symbol)
        })
        .collect();
    out.push('…');
    out
}

fn pad(s: &str, w: usize) -> String {
    let mut out = fit(s, w);
    let n = cell_len(&out);
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
    review: bool,
    laps: bool,
    cost: bool,
}

fn cols_for(app: &App, width: u16) -> Cols {
    Cols {
        project: app.all_projects() && width >= 60,
        stage: width >= 60,
        review: width >= 80,
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
    Review,
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
const REVIEW_W: usize = 6;
const TIME_W: usize = 10;
const LAPS_W: usize = 5;
const COST_W: usize = 9;
/// Project and stage shrink before the title drops below this.
const TITLE_MIN: usize = 20;

fn grid_for(app: &App, sections: &[Vec<&TaskSummary>; 5], width: usize) -> Grid {
    let cols = cols_for(app, width as u16);
    let mut project_w = if cols.project {
        sections
            .iter()
            .flatten()
            .map(|t| cell_len(&app.project_label(&t.task.project_id)))
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
                Section::NeedsYou | Section::InProgress | Section::Paused => {
                    Some(cell_len(&stage_cell(t)))
                }
                Section::Closed => Some(t.task.status.chars().count()),
                Section::Stuck => None,
            })
            .max()
            .unwrap_or(5)
            // Room for a waiting turn's `· queued N` suffix, only when some
            // row has one.
            .clamp(
                6,
                if sections.iter().flatten().any(|t| !t.queued.is_empty()) {
                    30
                } else {
                    22
                },
            )
    } else {
        0
    };
    let mut fixed_cols = vec![ID_W, TIME_W];
    fixed_cols.extend([REVIEW_W].into_iter().filter(|_| cols.review));
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
        // A group's `settled/total` suffix is the point of the cell, so the
        // column keeps room for it even if the title gets less.
        let floor = sections
            .iter()
            .zip(Section::ALL)
            .filter(|(_, s)| matches!(s, Section::NeedsYou | Section::InProgress | Section::Paused))
            .flat_map(|(ts, _)| ts.iter())
            .filter(|t| t.branch_progress.is_some() || !t.queued.is_empty())
            .map(|t| {
                // A waiting turn's `· queued N` is the point of its cell.
                let most = if t.queued.is_empty() { 22 } else { 30 };
                cell_len(&stage_cell(t)).clamp(6, most)
            })
            .max()
            .unwrap_or(0)
            .max(8);
        let give = (TITLE_MIN - title_w).min(stage_w.saturating_sub(floor));
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

/// The columns of one section's rows. The id, project, title, review, time
/// and laps columns sit at the same place in every section; a stuck task's
/// reason takes the stage and review columns' room.
fn columns(section: Section, g: &Grid) -> Vec<Col> {
    let c = |key, w| Col { key, w };
    let mut v = vec![c(Key::Id, ID_W)];
    if g.cols.project {
        v.push(c(Key::Project, g.project_w));
    }
    let mut title_w = g.title_w;
    let mut mid = None;
    let mut review = g.cols.review;
    match section {
        Section::NeedsYou | Section::InProgress | Section::Paused => {
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
                let w = g.stage_w + if review { 2 + REVIEW_W } else { 0 };
                mid = Some(c(Key::Reason, w));
                review = false;
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
    if review {
        v.push(c(Key::Review, REVIEW_W));
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

/// The list's stage cell: the stage, with `settled/total` after it while a
/// parallel group is current.
fn stage_cell(t: &TaskSummary) -> String {
    let base = base_stage_cell(t);
    match t.task.status.as_str() {
        // A pause was asked for and has not yet settled to `paused`.
        "pausing" => format!("{base} · pausing"),
        _ => base,
    }
}

fn base_stage_cell(t: &TaskSummary) -> String {
    match (&t.current_stage, &t.branch_progress) {
        // A group with branches waiting for a slot: how many.
        (Some(stage), Some(p)) if !t.queued.is_empty() => {
            format!(
                "{stage} {}/{} · {} queued",
                p.settled,
                p.total,
                t.queued.len()
            )
        }
        (Some(stage), Some(p)) => format!("{stage} {}/{}", p.settled, p.total),
        // A stage waiting for a slot: its place in line.
        (Some(stage), None) if !t.queued.is_empty() => {
            format!("{stage} · queued {}", t.queued[0].position)
        }
        (Some(stage), None) => stage.clone(),
        (None, _) => "-".to_string(),
    }
}

fn cell(app: &App, section: Section, t: &TaskSummary, key: Key) -> String {
    match key {
        Key::Id => t.task.id.chars().take(8).collect(),
        Key::Project => app.project_label(&t.task.project_id),
        Key::Title => t.task.title.clone(),
        Key::Stage => stage_cell(t),
        Key::Reason => first_line(t.task.stuck_reason.as_deref().unwrap_or("")).to_string(),
        Key::Review => t
            .reviews
            .first()
            .map(|r| r.label.clone())
            .unwrap_or_default(),
        Key::Time => match section {
            Section::NeedsYou | Section::InProgress => age(app, t.stage_entered_at),
            // "paused for", from when the pause was asked for.
            Section::Paused => {
                fmt_duration(app.now - t.task.paused_at.unwrap_or(t.task.updated_at))
            }
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
            // A `+` marks a total that leaves out turns of unknown cost.
            Some(c) if u.turns_without_cost > 0 => format!("≈${c:.2}+"),
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
            Key::Review => LABEL_REVIEW,
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
                Section::Paused => "paused for",
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
        "↑↓ move  ⏎ open  / search  o review  r retry  p pause  s send  c cancel  ? help  q quit",
    );
}

fn version_text(app: &App) -> String {
    match &app.daemon_version {
        None => "daemon ?".to_string(),
        Some(v) if v == chocofactory_core::version::VERSION => format!("daemon {v}"),
        Some(v) => format!("daemon {v} (choco {})", chocofactory_core::version::VERSION),
    }
}

/// `agents 2/2 · 3 queued` for the server pool; `None` until the daemon has
/// answered `GET /server` once.
fn agents_text(app: &App) -> Option<String> {
    let pool = app.server_pool.as_ref()?;
    let mut text = match pool.limit {
        Some(limit) => format!("agents {}/{limit}", pool.running),
        None => format!("agents {}", pool.running),
    };
    if pool.queued > 0 {
        text.push_str(&format!(" · {} queued", pool.queued));
    }
    Some(text)
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
    let mut ver = version_text(app);
    if let Some(agents) = agents_text(app) {
        ver = format!("{ver} · {agents}");
    }
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

/// Splits a status line into its shown text and shown tail for `width`.
/// The reason (text) has priority: the full tail is shown while the text keeps
/// up to `WANT` characters beside it; otherwise the tail shrinks to
/// `; kept: …/<file name>` and the text takes the rest. Only when even that
/// leaves the text under `FLOOR` characters is the tail cut from its left.
fn status_parts(text: &str, tail: &str, width: usize) -> (String, String) {
    const WANT: usize = 40;
    const FLOOR: usize = 16;
    let text_len = text.chars().count();
    let tail_len = tail.chars().count();
    if text_len.min(WANT) + tail_len <= width {
        return (fit(text, width.saturating_sub(tail_len)), tail.to_string());
    }
    let compact = tail
        .strip_prefix("; your text is in ")
        .and_then(|p| p.rsplit('/').next())
        .map(|file| format!("; kept: …/{file}"))
        .filter(|c| c.chars().count() < tail_len)
        .unwrap_or_else(|| tail.to_string());
    let compact_len = compact.chars().count();
    let room = width.saturating_sub(compact_len);
    if room >= text_len.min(FLOOR) {
        return (fit(text, room), compact);
    }
    // Very narrow: keep FLOOR characters of text, cut the tail from its left.
    let text_room = text_len.min(FLOOR).min(width);
    let tail_room = width - text_room;
    let shown_tail = if tail_room == 0 {
        String::new()
    } else {
        let keep: String = compact
            .chars()
            .skip(compact_len.saturating_sub(tail_room - 1))
            .collect();
        format!("…{keep}")
    };
    (fit(text, text_room), shown_tail)
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
        let (shown_text, shown_tail) = status_parts(&status.text, &status.tail, width);
        let whole = format!("{shown_text}{shown_tail}");
        Line::styled(fit(&whole, width), style)
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

/// Lines a wrapping field (`Stuck`, `Waiting for`) may take.
const WRAP_MAX: usize = 3;

/// One field row of the status view.
struct FRow {
    label: &'static str,
    value: String,
    error: bool,
    wrap: bool,
    /// The value holds text to show as written (a gate's marker lines): a
    /// wrapped row breaks it only at a lone space and rewrites nothing.
    verbatim: bool,
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
            verbatim: false,
            cap: WRAP_MAX,
            drop: 0,
        }
    }
}

/// The hint for a gate with these marker lines, before the send command: each
/// line exactly as configured, between backticks. The daemon accepts a marker
/// only exactly, so the row is `verbatim`.
fn marker_hint(lines: &[&str]) -> String {
    let shown: Vec<String> = lines.iter().map(|l| format!("`{l}`")).collect();
    format!("a marker on its own line ({})", shown.join(" or "))
}

/// The `Waiting for` row's value: how to reply to the open human gate the
/// task is at; `None` when it is at none.
///
/// The status comes from the loaded detail when there is one, since the list's
/// snapshot can be older; else from the snapshot. The stage and "waiting"
/// facts come from the snapshot when there is one, else from the loaded
/// detail. The marker lines come only from the detail's `gate_markers`, and
/// only while it is at the same stage; anything else that isn't a list of
/// strings (not loaded yet, `null`, missing, the wrong type) reads as
/// unavailable, never as a gate without markers.
fn waiting_text(d: &Detail) -> Option<String> {
    let data = d.data.as_ref();
    let state = data.and_then(|v| v.get("workflow_state"));
    let text = |v: Option<&'_ serde_json::Value>, key: &str| -> Option<String> {
        v?.get(key)?.as_str().map(str::to_string)
    };
    let status = match data {
        Some(_) => text(data, "status")?,
        None => d.snapshot.as_ref()?.task.status.clone(),
    };
    if status != TASK_STATUS_OPEN {
        return None;
    }
    let detail_stage = text(state, "current_stage");
    let (at_gate, same_stage) = match &d.snapshot {
        Some(t) => (
            t.waiting_on_human,
            detail_stage.is_some() && detail_stage == t.current_stage,
        ),
        None => (
            text(state, "stage_kind").as_deref() == Some("human_gate"),
            true,
        ),
    };
    if !at_gate {
        return None;
    }
    let markers: Option<Vec<&str>> = data
        .filter(|_| same_stage)
        .and_then(|v| v.get("gate_markers"))
        .and_then(serde_json::Value::as_array)
        .and_then(|lines| lines.iter().map(serde_json::Value::as_str).collect());
    let send = format!("choco task send {} --text \"…\"", d.id);
    Some(match markers {
        None => {
            format!("marker instructions unavailable, so a reply may be refused. Reply with {send}")
        }
        Some(lines) if lines.is_empty() => format!("a note: {send}"),
        Some(lines) => format!("{}: {send}", marker_hint(&lines)),
    })
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
                LABEL_TITLE | LABEL_CLOSES => continue,
                LABEL_PROJECT => {
                    let pid = snap
                        .map(|t| t.task.project_id.as_str())
                        .or_else(|| v.get("project_id").and_then(|p| p.as_str()));
                    if let Some(pid) = pid {
                        row.value = app.project_label(pid);
                    }
                }
                LABEL_WORKFLOW | LABEL_WORKFLOW_FILE => {
                    paths += 1;
                    if label == LABEL_WORKFLOW_FILE || paths == 2 {
                        row.drop = 4;
                    }
                }
                // The snapshot is fresher: rebuilt after Status below.
                LABEL_STUCK | LABEL_PAUSED if snap.is_some() => continue,
                LABEL_STUCK => {
                    row.error = true;
                    row.wrap = true;
                }
                LABEL_ROLE => row.drop = 1,
                LABEL_CREATED => row.drop = 2,
                LABEL_REPO | LABEL_BASE => row.drop = 3,
                _ => {}
            }
            rows.push(row);
        }
    } else if let Some(t) = snap {
        rows.push(FRow::new(LABEL_ID, t.task.id.clone()));
        rows.push(FRow::new(
            LABEL_PROJECT,
            app.project_label(&t.task.project_id),
        ));
        rows.push(FRow::new(LABEL_WORKFLOW, t.task.workflow_def.clone()));
        rows.push(FRow::new(LABEL_STATUS, t.task.status.clone()));
    }
    if let Some(t) = snap
        && let Some(pos) = rows.iter().position(|r| r.label == LABEL_STATUS)
        && t.task.status == "stuck"
        && let Some(reason) = &t.task.stuck_reason
    {
        let mut row = FRow::new(LABEL_STUCK, crate::render::single_line(reason));
        row.error = true;
        row.wrap = true;
        rows.insert(pos + 1, row);
    }
    if let Some(t) = snap
        && let Some(pos) = rows.iter().position(|r| r.label == LABEL_STATUS)
        && let Some(value) = crate::render::paused_value(
            &t.task.status,
            t.task.paused_at.map(|at| at.to_rfc3339()).as_deref(),
            &t.task.id,
            t.current_stage.as_deref(),
        )
    {
        rows.insert(pos + 1, FRow::new(LABEL_PAUSED, value));
    }

    // Stage, with the time in it (or how long ago it ended), then the review
    // and what the task waits for. The review row is taken out so it can go
    // back right after Stage wherever Stage ends up.
    let review_row = rows
        .iter()
        .position(|r| r.label == LABEL_REVIEW)
        .map(|pos| rows.remove(pos));
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
                "paused" => format!(
                    "{name} (paused {} ago)",
                    fmt_duration(app.now - t.task.paused_at.unwrap_or(t.task.updated_at))
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
    let at = match rows.iter().position(|r| r.label == LABEL_STAGE) {
        Some(pos) => {
            rows[pos].value = stage_value;
            pos
        }
        None => {
            if snap.is_some() || !stage_value.is_empty() {
                rows.push(FRow::new(LABEL_STAGE, stage_value));
            }
            rows.len().saturating_sub(1)
        }
    };
    let mut extra = Vec::new();
    // Once the detail has loaded, `task_fields` alone decides whether there
    // is a review row; the snapshot only stands in until then.
    extra.extend(match (&d.data, snap) {
        (None, Some(t)) => t
            .reviews
            .first()
            .map(|review| FRow::new(LABEL_REVIEW, crate::render::review_value(review))),
        _ => review_row,
    });
    if let Some(text) = waiting_text(d) {
        let mut row = FRow::new("Waiting for", text);
        row.wrap = true;
        row.verbatim = true;
        extra.push(row);
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
        let cost = format!(
            "{}{}",
            cost_with_label(usage.get("cost_usd").and_then(|c| c.as_f64()), label),
            crate::render::partial_notes(usage)
        );
        return Some(format!(
            "{cost} · wall {} · active {}",
            duration("wall_time_ms"),
            duration("active_time_ms"),
        ));
    }
    let snap = d.snapshot.as_ref()?;
    Some(match &snap.usage_total {
        Some(u) => {
            let mut s = cost_with_label(u.cost_usd, &u.billing_label);
            if u.cost_usd.is_some() {
                s.push_str(&crate::render::partial_notes(
                    &serde_json::json!({ "turns_without_cost": u.turns_without_cost }),
                ));
            }
            s
        }
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
    let mut parts = if row.verbatim {
        wrap_tokens(lone_space_words(&row.value), room)
    } else {
        wrap_words(&crate::render::single_line(&row.value), room)
    };
    // `clip`, not `fit`: the wrapped parts are final. Prose has been made
    // printable already, and verbatim text must stay as it is.
    if parts.len() > row.cap {
        parts.truncate(row.cap);
        let last = parts.last_mut().expect("wrap keeps a line");
        *last = clip(&format!("{last}…"), room);
    }
    let pad = " ".repeat(label_w + 2);
    parts
        .into_iter()
        .enumerate()
        .map(|(i, p)| {
            if i == 0 {
                clip(&format!("{head}{p}"), width)
            } else {
                clip(&format!("{pad}{p}"), width)
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
        "e/Esc status · ? help"
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
    let title = pad(&title, width.saturating_sub(right.chars().count() + 2));
    Line::styled(
        format!("{title}  {right}"),
        colored(app, ACCENT).add_modifier(Modifier::BOLD),
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
        colored(app, ACCENT)
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
    let mut prog_header: Option<String> = None;
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
            match crate::render::detail_progress(v, app.now, Some(width)) {
                Some(t) => {
                    prog_header = t.header;
                    prog.extend(t.rows.into_iter().map(|l| (l, plain)));
                }
                None => prog.push((
                    "  (no workflow state — the task has not started)".into(),
                    plain,
                )),
            }
        }
    }
    let counters = d.data.as_ref().and_then(crate::render::loop_counters_line);
    // The parallel group's branch table: all or nothing, between the field
    // rows and the progress separator. Its height is 0 once it is dropped.
    let branches = d
        .data
        .as_ref()
        .and_then(|v| crate::render::detail_branches(v, app.now, Some(width)));
    let branch_h = std::cell::Cell::new(branches.as_ref().map_or(0, |b| 2 + b.rows.len()));

    let events_n = d.events.len();
    // The tail used to be capped at five events; it now takes whatever
    // height is left once the fields and the progress block fit (#187).
    let want_tail = events_n.max(1);
    let row_h = |rows: &[FRow]| -> usize {
        rows.iter()
            .map(|r| row_lines(r, label_w_cell.get(), width).len())
            .sum()
    };
    // Height of the status block (without the tail) when `m` of the steps
    // are shown, with or without the progress header row.
    let block = |rows: &[FRow], m: usize, header: bool| -> usize {
        let prog_h = if m < prog.len() { m + 1 } else { m };
        1 + if has_task { row_h(rows) } else { 0 }
            + branch_h.get()
            + 1
            + usize::from(header)
            + prog_h
            + usize::from(counters.is_some())
    };
    // What the tail gets from `spare` lines: its separator plus events.
    let tail_for = |spare: usize| -> usize {
        if spare >= 2 {
            (spare - 1).min(want_tail)
        } else {
            0
        }
    };

    let mut show_header = prog_header.is_some();
    let mut keep_prog = prog.len();
    let mut tail = 0;
    if !has_task {
        rows.clear();
    }
    if block(&rows, keep_prog, show_header) <= body_h {
        tail = tail_for(body_h - block(&rows, keep_prog, show_header));
    } else {
        // The tail is already gone. Rows drop first, with every progress
        // step still counted: the progress path is what the view is for.
        for cut in 1..=4u8 {
            if block(&rows, keep_prog, show_header) <= body_h {
                break;
            }
            rows.retain(|r| r.drop != cut);
            label_w_cell.set(widest(&rows));
        }
        // After the row drops, the whole branch table goes before any
        // progress step does.
        if block(&rows, keep_prog, show_header) > body_h {
            branch_h.set(0);
        }
        if block(&rows, keep_prog, show_header) <= body_h {
            // The drops can free several lines at once: the tail gets them.
            tail = tail_for(body_h - block(&rows, keep_prog, show_header));
        } else {
            // Then older progress lines go, the header row kept...
            let largest = |rows: &[FRow], header: bool| {
                (1..prog.len())
                    .rev()
                    .find(|&m| block(rows, m, header) <= body_h)
            };
            if cuttable {
                keep_prog = largest(&rows, show_header)
                    .or_else(|| {
                        // ...and when even one step doesn't fit with it, the
                        // header goes before anything else is given up.
                        let m = (1..=prog.len())
                            .rev()
                            .find(|&m| block(&rows, m, false) <= body_h);
                        if m.is_some() {
                            show_header = false;
                        }
                        m
                    })
                    .unwrap_or(if prog.len() <= 2 { prog.len() } else { 1 });
            }
            // Last resort: the header row first, then wrapped rows shrink a
            // line at a time, then trailing rows go, so the progress
            // separator and one progress (or error) line stay on screen.
            // Extra error lines and the loop counters are left to the
            // bottom clip.
            let tight = |rows: &[FRow], header: bool| {
                let m = if keep_prog < prog.len() {
                    keep_prog
                } else {
                    prog.len().min(if cuttable { 2 } else { 1 })
                };
                let prog_h = if keep_prog < prog.len() { m + 1 } else { m };
                1 + if has_task { row_h(rows) } else { 0 }
                    + branch_h.get()
                    + 1
                    + usize::from(header)
                    + prog_h
            };
            const KEEP: [&str; 5] = [
                LABEL_ID,
                LABEL_PROJECT,
                LABEL_WORKFLOW,
                LABEL_STATUS,
                LABEL_STAGE,
            ];
            if tight(&rows, show_header) > body_h {
                show_header = false;
            }
            while tight(&rows, show_header) > body_h {
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
    if let Some(b) = branches.as_ref().filter(|_| branch_h.get() > 0) {
        lines.push(Line::styled(
            fit(&separator("branches", width), width),
            colored(app, ACCENT),
        ));
        lines.push(Line::styled(
            fit(&b.header, width),
            colored(app, Color::DarkGray),
        ));
        for row in &b.rows {
            lines.push(Line::raw(fit(row, width)));
        }
    }
    lines.push(Line::styled(
        fit(&separator(&prog_title, width), width),
        if prog_err {
            error_style(app)
        } else {
            colored(app, ACCENT)
        },
    ));
    if show_header && let Some(h) = &prog_header {
        lines.push(Line::styled(fit(h, width), colored(app, Color::DarkGray)));
    }
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

const STATUS_HINTS: &str = "e events  o review  r retry  p pause  s send  c cancel  Esc back";
const EXPANDED_HINTS: &str =
    "PgUp/PgDn scroll  End follow  e/Esc status  o review  r retry  p pause  s send  c cancel";

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

/// The `/` search popup (#292): input line, count line, result rows, hints.
fn draw_search(frame: &mut Frame, app: &App, search: &Search, area: Rect) {
    let box_area = centered(
        area,
        area.width.saturating_sub(4).min(100),
        area.height.saturating_sub(2),
    );
    frame.render_widget(Clear, box_area);
    let block = Block::default().borders(Borders::ALL).title(" search ");
    let inner_area = block.inner(box_area);
    frame.render_widget(block, box_area);
    let inner = inner_area.width as usize;

    // Line 1: the query, its tail when too long, then the cursor mark.
    let avail = inner.saturating_sub(3);
    let q: Vec<char> = search.query.chars().collect();
    let shown: String = if q.len() > avail {
        let tail: String = q[q.len() - avail.saturating_sub(1)..].iter().collect();
        format!("…{tail}")
    } else {
        search.query.clone()
    };
    let input = fit(&format!("/ {shown}_"), inner);

    let results = search.results();
    let (status, status_style) = match &search.load {
        SearchLoad::Loading => ("loading…".to_string(), Style::default()),
        SearchLoad::Failed(err) => (format!("could not load tasks: {err}"), error_style(app)),
        SearchLoad::Loaded(all) => (
            format!("{} of {} tasks", results.len(), all.len()),
            Style::default(),
        ),
    };

    let mut lines = vec![
        Line::from(input),
        Line::styled(fit(&status, inner), status_style),
    ];

    // Result rows fill what is left between the count line and the hints.
    let rows_h = (inner_area.height as usize).saturating_sub(3);
    if matches!(search.load, SearchLoad::Loaded(_)) && results.is_empty() && rows_h > 0 {
        lines.push(Line::from("no matching tasks"));
    }
    let sel = search
        .selected
        .as_ref()
        .and_then(|id| results.iter().position(|t| &t.task.id == id));
    let mut off = search.offset.get();
    if let Some(i) = sel {
        if i < off {
            off = i;
        }
        if i >= off + rows_h {
            off = i + 1 - rows_h;
        }
    }
    off = off.min(results.len().saturating_sub(rows_h));
    search.offset.set(off);
    search.page.set(rows_h);

    let cols = cols_for(app, area.width);
    // Sized over every loaded task, so the title column doesn't shift as the
    // query narrows the results.
    let loaded: &[TaskSummary] = match &search.load {
        SearchLoad::Loaded(all) => all,
        _ => &[],
    };
    let proj_w = loaded
        .iter()
        .map(|t| cell_len(&app.project_label(&t.task.project_id)))
        .max()
        .unwrap_or(0)
        .clamp(7, 14);
    let fixed = ID_W
        + if cols.project { proj_w + 2 } else { 0 }
        + if cols.stage { 9 + 2 } else { 0 }
        + if cols.review { REVIEW_W + 2 } else { 0 }
        + 2;
    let title_w = inner.saturating_sub(2 + fixed).max(1);
    for (i, t) in results.iter().enumerate().skip(off).take(rows_h) {
        let mut parts = vec![pad(&t.task.id, ID_W)];
        if cols.project {
            parts.push(pad(&app.project_label(&t.task.project_id), proj_w));
        }
        parts.push(pad(&t.task.title, title_w));
        if cols.stage {
            parts.push(pad(&t.task.status, 9));
        }
        if cols.review {
            let label = t.reviews.first().map_or("-", |r| r.label.as_str());
            parts.push(pad(label, REVIEW_W));
        }
        let selected = Some(i) == sel;
        let text = format!("{}{}", if selected { "▶ " } else { "  " }, parts.join("  "));
        let style = if selected {
            Style::default().add_modifier(Modifier::REVERSED)
        } else {
            Style::default()
        };
        lines.push(Line::styled(fit(&text, inner), style));
    }

    frame.render_widget(Paragraph::new(lines), inner_area);
    if inner_area.height > 0 {
        frame.render_widget(
            Paragraph::new(fit("↑↓ move  ⏎ open  Esc back", inner)),
            Rect::new(
                inner_area.x,
                inner_area.y + inner_area.height - 1,
                inner_area.width,
                1,
            ),
        );
    }
}

/// Word wrap to `w` cells. A token longer than a line ends the current line
/// and is split into `w`-cell chunks, the last of which later words may join.
/// Empty input is one empty line.
fn wrap_words(s: &str, w: usize) -> Vec<String> {
    // Measured as it will be shown, control characters as spaces.
    wrap_tokens(printable(s).split_whitespace(), w)
}

/// `s` cut only at lone spaces, those with no whitespace beside them. Every
/// other space, tab or other whitespace stays in its word, so wrapping these
/// words can't collapse or rewrite any of it.
fn lone_space_words(s: &str) -> Vec<&str> {
    let blank = |c: Option<char>| c.is_some_and(char::is_whitespace);
    let mut words = Vec::new();
    let mut start = 0;
    for (i, _) in s.match_indices(' ') {
        if !blank(s[..i].chars().next_back()) && !blank(s[i + 1..].chars().next()) {
            words.push(&s[start..i]);
            start = i + 1;
        }
    }
    words.push(&s[start..]);
    words
}

/// [`wrap_words`] for words already split: each is kept as it is.
fn wrap_tokens<'a>(words: impl IntoIterator<Item = &'a str>, w: usize) -> Vec<String> {
    let w = w.max(1);
    let mut lines = Vec::new();
    let mut cur = String::new();
    for word in words {
        let n = cell_len(word);
        if n > w {
            if !cur.is_empty() {
                lines.push(std::mem::take(&mut cur));
            }
            let mut chunks = chunk_cells(word, w);
            cur = chunks.pop().unwrap_or_default();
            lines.extend(chunks);
            continue;
        }
        if !cur.is_empty() && cell_len(&cur) + 1 + n > w {
            lines.push(std::mem::take(&mut cur));
        }
        if !cur.is_empty() {
            cur.push(' ');
        }
        cur.push_str(word);
    }
    if !cur.is_empty() || lines.is_empty() {
        lines.push(cur);
    }
    lines
}

fn draw_help(frame: &mut Frame, area: Rect) {
    let lines = vec![
        Line::from("↑ k / ↓ j   move            ⏎     open detail"),
        Line::from("PgUp PgDn   page            Esc   back"),
        Line::from("g / G       top / bottom    o     open review"),
        Line::from("⇥ / ⇧⇥      next section    r     retry stuck"),
        Line::from("?           this help       c     cancel"),
        Line::from("q  Ctrl-C   quit            p     pause / resume"),
        Line::from("e           events / status s     send a message"),
        Line::from("/           search"),
    ];
    let r = centered(area, 52, lines.len() as u16 + 2);
    frame.render_widget(Clear, r);
    frame.render_widget(
        Paragraph::new(lines).block(Block::default().borders(Borders::ALL).title(" keys ")),
        r,
    );
}

#[cfg(test)]
mod wrap_tests {
    use super::{FRow, fit, lone_space_words, row_lines, wrap_words};

    #[test]
    fn fit_shows_a_control_character_in_ordinary_text_as_a_space() {
        assert_eq!(fit("a\tb", 5), "a b");
    }

    #[test]
    fn a_verbatim_row_cut_at_its_last_line_is_not_made_printable() {
        let mut row = FRow::new("W", "a\tb c d".to_string());
        row.wrap = true;
        row.verbatim = true;
        row.cap = 1;
        assert_eq!(row_lines(&row, 1, 8), ["W  a\tb c…"]);
    }

    #[test]
    fn only_a_space_with_no_whitespace_beside_it_ends_a_word() {
        assert_eq!(
            lone_space_words("a b  c \td \u{a0}e f\tg"),
            ["a", "b  c \td \u{a0}e", "f\tg"]
        );
    }

    #[test]
    fn empty_input_is_one_empty_line() {
        assert_eq!(wrap_words("", 10), [""]);
        assert_eq!(wrap_words("   ", 0), [""]);
    }

    #[test]
    fn lines_are_filled_by_terminal_cells_not_characters() {
        // Two cells each: five characters and a space would take eleven.
        assert_eq!(wrap_words("界界界 界界", 10), ["界界界", "界界"]);
        // A long token is cut between characters, none of them split.
        assert_eq!(wrap_words("界界界界界 x", 5), ["界界", "界界", "界 x"]);
        // A character wider than the line still ends up somewhere.
        assert_eq!(wrap_words("界界", 1), ["界", "界"]);
        // A combining mark takes no cell.
        let e = "e\u{301}";
        let word = e.repeat(4);
        assert_eq!(
            wrap_words(&format!("{word} {word}"), 9),
            [format!("{word} {word}")]
        );
    }

    #[test]
    fn a_control_character_is_measured_as_the_space_it_is_shown_as() {
        assert_eq!(wrap_words("ab\u{1b}cd", 4), ["ab", "cd"]);
    }
}
