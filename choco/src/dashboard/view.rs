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

use super::app::{
    App, Detail, Level, PromptKind, Scope, Section, View, fmt_duration, laps_by_stage, max_laps,
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
}

fn cols_for(app: &App, width: u16) -> Cols {
    Cols {
        project: app.all_projects() && width >= 60,
        stage: width >= 60,
        pr: width >= 80,
        laps: width >= 80,
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

fn draw_detail(frame: &mut Frame, app: &App, d: &Detail, area: Rect) {
    let width = area.width as usize;
    let mut head: Vec<(String, Style)> = Vec::new();
    let plain = Style::default();

    match &d.snapshot {
        None => head.push(("(task is no longer listed)".into(), plain)),
        Some(t) => {
            let right = "Esc back · ? help";
            let title = pad(
                &t.task.title,
                width.saturating_sub(right.chars().count() + 2),
            );
            head.push((
                format!("{title}  {right}"),
                Style::default().add_modifier(Modifier::BOLD),
            ));
            let project = app.project_label(&t.task.project_id);
            head.extend(
                wrap_chars(
                    &format!(
                        "id {} · project {} · workflow {} · status {}",
                        t.task.id, project, t.task.workflow_def, t.task.status
                    ),
                    width,
                )
                .into_iter()
                .map(|l| (l, plain)),
            );
            let mut stage = format!(
                "stage {} for {}",
                t.current_stage.as_deref().unwrap_or("-"),
                age(app, t.stage_entered_at)
            );
            let laps = laps_by_stage(t);
            if !laps.is_empty() {
                let parts: Vec<String> = laps.iter().map(|(s, n)| format!("{s} ×{n}")).collect();
                stage.push_str(&format!(" · laps {}", parts.join(", ")));
            }
            match &t.pr {
                Some(pr) => stage.push_str(&format!(" · PR {}", pr.url)),
                None => stage.push_str(" · PR none yet"),
            }
            head.extend(wrap_chars(&stage, width).into_iter().map(|l| (l, plain)));
            if let Some(reason) = &t.task.stuck_reason {
                head.extend(
                    wrap_chars(&format!("stuck: {reason}"), width)
                        .into_iter()
                        .map(|l| (l, error_style(app))),
                );
            }
        }
    }

    // History: the last few wrapped lines of the trail.
    let (hist, hist_style): (Vec<String>, Style) = match &d.trail {
        None => (vec!["loading…".into()], plain),
        Some(Err(e)) => (
            wrap_chars(&format!("could not load history: {e}"), width),
            error_style(app),
        ),
        Some(Ok(steps)) if steps.is_empty() => (vec!["(none)".into()], plain),
        Some(Ok(steps)) => {
            let text = steps
                .iter()
                .map(|s| match &s.outcome {
                    Some(o) => format!("{} (via {o})", s.stage),
                    None => s.stage.clone(),
                })
                .collect::<Vec<_>>()
                .join(" → ");
            let mut lines = wrap_chars(&text, width.saturating_sub(2));
            if lines.len() > 3 {
                lines = lines.split_off(lines.len() - 3);
                lines[0] = format!("…{}", lines[0]);
            }
            (lines, plain)
        }
    };

    let fixed = head.len() + 1 + hist.len() + 1 + 1;
    let ev_h = (area.height as usize).saturating_sub(fixed);
    d.page.set(ev_h.max(1));

    let n = d.events.len();
    let back = d.scroll_back.min(n.saturating_sub(ev_h));
    let end = n - back;
    let start = end.saturating_sub(ev_h);

    let mut lines: Vec<Line> = Vec::new();
    for (text, style) in &head {
        lines.push(Line::styled(fit(text, width), *style));
    }
    lines.push(Line::raw(separator("stage history", width)));
    for l in &hist {
        lines.push(Line::styled(
            format!("  {}", fit(l, width.saturating_sub(2))),
            hist_style,
        ));
    }
    let ev_title = if d.reconnecting {
        "events: reconnecting"
    } else if d.following {
        "events (following)"
    } else {
        "events (scrolled · End follows)"
    };
    lines.push(Line::styled(
        separator(ev_title, width),
        if d.reconnecting {
            error_style(app)
        } else {
            plain
        },
    ));
    for e in &d.events[start..end] {
        let time = e.created_at.with_timezone(&Local).format("%H:%M:%S");
        let text = format!(
            "  {time}  {:<14} {}",
            e.event_type,
            crate::render::event_summary(e)
        );
        lines.push(Line::raw(fit(&text, width)));
    }
    let body_h = area.height.saturating_sub(1);
    frame.render_widget(
        Paragraph::new(lines),
        Rect::new(area.x, area.y, area.width, body_h),
    );
    draw_bottom(
        frame,
        app,
        Rect::new(area.x, area.y + area.height - 1, area.width, 1),
        "PgUp/PgDn scroll  End follow  o PR  r retry  c cancel  Esc back",
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
    ];
    let r = centered(area, 52, lines.len() as u16 + 2);
    frame.render_widget(Clear, r);
    frame.render_widget(
        Paragraph::new(lines).block(Block::default().borders(Borders::ALL).title(" keys ")),
        r,
    );
}
