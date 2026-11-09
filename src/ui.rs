//! Drawing. Each frame is drawn into a ratatui buffer; ratatui compares it
//! with the previous frame and writes only the cells that changed, so a
//! redraw never flickers. Steps replace the whole screen.

use crate::app::{
    AddIssueFlow, AddIssueStep, App, Choice, ChoiceKind, Screen, Step, DEFAULT_PRIORITY_INDEX,
    ISSUE_TYPES, PRIORITIES,
};
use crate::model::{Issue, IssueGraph};
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::Frame;
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

const SEPARATOR: &str = " │ ";
const HIGHLIGHT_BACKGROUND: Color = Color::Indexed(238);
/// Marks an issue that has a live agent pane.
const PANE_MARKER: &str = "● ";
const PANE_COLOR: Color = Color::Cyan;

/// One line of text with a single style. `fill` paints the style across
/// the whole width, for highlighted rows.
struct Line {
    text: String,
    style: Style,
    fill: bool,
}

impl Line {
    fn new(text: impl Into<String>, style: Style) -> Self {
        Self {
            text: text.into(),
            style,
            fill: false,
        }
    }

    fn plain(text: impl Into<String>) -> Self {
        Self::new(text, Style::new())
    }

    fn blank() -> Self {
        Self::plain("")
    }

    fn filled(mut self) -> Self {
        self.fill = true;
        self
    }
}

pub fn draw(app: &mut App, frame: &mut Frame) {
    let area = frame.area();
    let buffer = frame.buffer_mut();
    if area.width == 0 || area.height == 0 {
        return;
    }
    if let Some(step) = app.step().cloned() {
        draw_step(app, &step, buffer, area);
        return;
    }
    match app.screen() {
        Screen::Tree => draw_tree(app, buffer, area),
        Screen::Detail => draw_detail(app, buffer, area),
    }
}

fn header_style() -> Style {
    Style::new()
        .bg(HIGHLIGHT_BACKGROUND)
        .add_modifier(Modifier::BOLD)
}

fn dim() -> Style {
    Style::new().add_modifier(Modifier::DIM)
}

fn bold() -> Style {
    Style::new().add_modifier(Modifier::BOLD)
}

/// Writes `text` at (`x`, `y`), cut to `width` columns; returns the column
/// after it.
fn put(buffer: &mut Buffer, x: u16, y: u16, text: &str, width: usize, style: Style) -> u16 {
    buffer.set_stringn(x, y, text, width, style).0
}

fn put_line(buffer: &mut Buffer, area: Rect, y: u16, line: &Line) {
    if line.fill {
        buffer.set_style(Rect::new(area.x, y, area.width, 1), line.style);
    }
    put(
        buffer,
        area.x,
        y,
        &truncate(&line.text, area.width as usize),
        area.width as usize,
        line.style,
    );
}

/// The last row: the status message if there is one, otherwise the hints.
fn draw_footer(buffer: &mut Buffer, area: Rect, status: Option<&str>, hints: &str, style: Style) {
    let y = area.bottom() - 1;
    let line = match status {
        Some(status) => Line::new(
            status,
            Style::new().fg(Color::Yellow).add_modifier(Modifier::BOLD),
        ),
        None => Line::new(hints, style),
    };
    put_line(buffer, area, y, &line);
}

fn draw_tree(app: &mut App, buffer: &mut Buffer, area: Rect) {
    let width = area.width as usize;
    // The pane column is there whenever agent panes are possible, so the
    // table does not shift when a pane opens or closes.
    let gutter = if app.tmux() { PANE_MARKER.width() } else { 0 };
    let id_width = app
        .rows
        .iter()
        .map(|row| row.issue_id.width())
        .max()
        .unwrap_or(2);
    let widths = table_widths(width.saturating_sub(gutter), id_width);
    buffer.set_style(Rect::new(area.x, area.y, area.width, 1), header_style());
    write_table_row(
        buffer,
        area.x + gutter as u16,
        area.y,
        &["ID", "PRIORITY", "STATUS", "TYPE", "NAME"],
        widths,
        None,
    );

    let list_height = area.height.saturating_sub(2) as usize;
    app.viewport = list_height.max(1);
    if app.rows.is_empty() {
        // Only a search can empty the rows; otherwise the create entry is there.
        let message = format!(
            "No issue IDs match /{}",
            app.search_query().unwrap_or_default()
        );
        if area.height > 2 {
            put_line(buffer, area, area.y + 1, &Line::new(message, dim()));
        }
    } else if list_height > 0 {
        if app.scroll + list_height > app.rows.len() {
            app.scroll = app.rows.len().saturating_sub(list_height);
        }
        if app.cursor < app.scroll {
            app.scroll = app.cursor;
        }
        if app.cursor >= app.scroll + list_height {
            app.scroll = app.cursor + 1 - list_height;
        }

        let visible = list_height.min(app.rows.len().saturating_sub(app.scroll));
        for line in 0..visible {
            let position = app.scroll + line;
            let y = area.y + line as u16 + 1;
            let selected = position == app.cursor;
            let row = &app.rows[position];
            if selected {
                buffer.set_style(
                    Rect::new(area.x, y, area.width, 1),
                    Style::new().bg(HIGHLIGHT_BACKGROUND),
                );
            }
            if row.is_create_entry() {
                put(
                    buffer,
                    area.x,
                    y,
                    "+ Create New",
                    width,
                    Style::new().fg(Color::Green).add_modifier(Modifier::BOLD),
                );
                continue;
            }
            let Some(issue) = app.graph.issue(&row.issue_id) else {
                continue;
            };
            if gutter > 0 && app.pane(&issue.id).is_some() {
                put(
                    buffer,
                    area.x,
                    y,
                    PANE_MARKER,
                    gutter,
                    Style::new().fg(PANE_COLOR),
                );
            }
            let marker = if app.search_query().is_some() {
                "  "
            } else if row.cycle {
                "↻ "
            } else if app.row_has_children(row) {
                if app.row_is_expanded(row) {
                    "▾ "
                } else {
                    "▸ "
                }
            } else {
                "  "
            };
            let name = format!("{}{}{}", row.prefix, marker, issue.title);
            let priority = format!("P{}", issue.priority);
            let issue_type = short_type(&issue.issue_type);
            write_table_row(
                buffer,
                area.x + gutter as u16,
                y,
                &[&issue.id, &priority, &issue.status, &issue_type, &name],
                widths,
                Some(issue),
            );
        }
    }

    let (hints, style) = if let Some(query) = app.search_query() {
        (
            format!(
                "/{query}  · {} match{} · ↑/↓ select · Enter open · Esc cancel",
                app.rows.len(),
                if app.rows.len() == 1 { "" } else { "es" }
            ),
            bold(),
        )
    } else {
        (
            format!(
                "{} issue{} · + add child · / go to · j/k move · h/l fold · Enter open · e edit · s status · p priority · x close{} · q quit",
                app.graph.len(),
                if app.graph.len() == 1 { "" } else { "s" },
                if app.tmux() { " · a agent · f focus" } else { "" }
            ),
            dim(),
        )
    };
    if area.height > 1 {
        draw_footer(buffer, area, app.status_message(), &hints, style);
    }
}

fn draw_detail(app: &mut App, buffer: &mut Buffer, area: Rect) {
    let width = area.width as usize;
    let Some(issue) = app.current_detail_issue().cloned() else {
        return;
    };
    let selected_dependency = app
        .detail_frame()
        .map(|frame| frame.dependency_cursor)
        .unwrap_or(0);
    let pane = app.tmux().then(|| app.pane(&issue.id).map(str::to_string));
    let (lines, selected_line) = detail_lines(&app.graph, &issue, pane, width, selected_dependency);
    let visible = area.height.saturating_sub(1) as usize;
    let max_scroll = lines.len().saturating_sub(visible);
    let mut scroll = app.detail_frame().map(|frame| frame.scroll).unwrap_or(0);
    scroll = scroll.min(max_scroll);
    if let Some(selected) = selected_line {
        if selected < scroll {
            scroll = selected;
        } else if visible > 0 && selected >= scroll + visible {
            scroll = selected + 1 - visible;
        }
    }
    if let Some(frame) = app.detail_frame_mut() {
        frame.scroll = scroll;
    }

    for (screen_line, line) in lines.iter().skip(scroll).take(visible).enumerate() {
        put_line(buffer, area, area.y + screen_line as u16, line);
    }

    let hints = format!(
        "+ add child · j/k dependency · Enter open · e edit · et title · s status · p priority{}{} · Backspace back · Esc tree · q quit",
        if app.can_close_current_issue() { " · x close" } else { "" },
        if app.tmux() { " · a agent · f focus" } else { "" }
    );
    if area.height > 1 {
        draw_footer(buffer, area, app.status_message(), &hints, dim());
    }
}

/// The task view's lines, and the index of the selected dependency's line.
/// `pane` is `None` outside tmux, where agent panes do not exist.
fn detail_lines(
    graph: &IssueGraph,
    issue: &Issue,
    pane: Option<Option<String>>,
    width: usize,
    selected_dependency: usize,
) -> (Vec<Line>, Option<usize>) {
    let mut lines = vec![
        Line::new(
            format!(
                "{}  [P{}]  {}  {}",
                issue.id,
                issue.priority,
                issue.status,
                short_type(&issue.issue_type)
            ),
            Style::new().fg(Color::Cyan).add_modifier(Modifier::BOLD),
        ),
        Line::blank(),
    ];
    lines.extend(
        wrap(&issue.title, width)
            .into_iter()
            .map(|text| Line::new(text, bold())),
    );

    if !issue.description.trim().is_empty() {
        lines.push(Line::blank());
        lines.extend(wrap(&issue.description, width).into_iter().map(Line::plain));
    }

    lines.push(Line::blank());
    lines.push(Line::new(
        format!("Created:  {}", display_timestamp(&issue.created_at)),
        dim(),
    ));
    lines.push(Line::new(
        format!("Updated:  {}", display_timestamp(&issue.updated_at)),
        dim(),
    ));
    match pane {
        Some(Some(pane_id)) => lines.push(Line::new(
            format!("Agent:    {PANE_MARKER}pane {pane_id} · f focus"),
            Style::new().fg(PANE_COLOR),
        )),
        Some(None) => lines.push(Line::new("Agent:    no pane · a start", dim())),
        None => {}
    }
    lines.push(Line::blank());
    lines.push(Line::new("Dependencies:", bold()));

    let mut selected_line = None;
    if issue.dependencies.is_empty() {
        lines.push(Line::new("  none", dim()));
    } else {
        for (index, dependency) in issue.dependencies.iter().enumerate() {
            let dep_type = if dependency.dependency_type.is_empty() {
                String::new()
            } else {
                format!(" ({})", dependency.dependency_type)
            };
            // `bd list` gives bare edges, so the title and status come from
            // the linked issue itself.
            let (status, title) = match graph.issue(&dependency.id) {
                Some(target) => (&target.status, &target.title),
                None => (&dependency.status, &dependency.title),
            };
            let text = format!("  → {} [{status}] {title}{dep_type}", dependency.id);
            let line = Line::new(text, Style::new().fg(Color::Cyan));
            if index == selected_dependency {
                selected_line = Some(lines.len());
                lines.push(
                    Line {
                        style: line.style.bg(HIGHLIGHT_BACKGROUND),
                        ..line
                    }
                    .filled(),
                );
            } else {
                lines.push(line);
            }
        }
    }
    (lines, selected_line)
}

fn draw_step(app: &App, step: &Step, buffer: &mut Buffer, area: Rect) {
    let (title, lines, hints) = match step {
        Step::Choose(choice) => choice_step(app, choice),
        Step::Close { issue_id } => (
            format!("Close issue · {issue_id}"),
            vec![
                issue_title_line(app, issue_id),
                Line::blank(),
                Line::plain(format!("Close {issue_id}?")),
                Line::blank(),
                Line::new("[y] Yes    [n/Esc] No", bold()),
            ],
            "y close · n/Esc cancel".to_string(),
        ),
        Step::AddIssue(flow) => add_issue_step(flow, area.width as usize),
        Step::Command { issue_id, input } => {
            let mut lines = vec![
                issue_title_line(app, issue_id),
                Line::blank(),
                Line::plain(format!(
                    "Command to run in a new tmux pane next to bew. The pane is bound to {issue_id}."
                )),
                Line::new("bew does not change the issue; the command does what it needs.", dim()),
                Line::blank(),
            ];
            lines.extend(
                hard_wrap(&format!("> {input}▏"), area.width as usize)
                    .into_iter()
                    .map(|text| Line::new(text, bold())),
            );
            (
                format!("Start agent · {issue_id}"),
                lines,
                "Edit or paste the command · Enter run · Ctrl-U clear · Esc cancel".to_string(),
            )
        }
    };

    put_line(
        buffer,
        area,
        area.y,
        &Line::new(format!(" {title}"), header_style()).filled(),
    );
    let body_height = area.height.saturating_sub(3) as usize;
    for (index, line) in lines.iter().take(body_height).enumerate() {
        put_line(buffer, area, area.y + 2 + index as u16, line);
    }
    if area.height > 1 {
        draw_footer(buffer, area, app.status_message(), &hints, dim());
    }
}

fn issue_title_line(app: &App, issue_id: &str) -> Line {
    let title = app
        .graph
        .issue(issue_id)
        .map(|issue| issue.title.as_str())
        .unwrap_or_default();
    Line::new(title, bold())
}

fn choice_step(app: &App, choice: &Choice) -> (String, Vec<Line>, String) {
    let issue = app.graph.issue(&choice.issue_id);
    let (title, current) = match choice.kind {
        ChoiceKind::Status => (
            "Set status",
            issue.map(|issue| issue.status.clone()).unwrap_or_default(),
        ),
        ChoiceKind::Priority => (
            "Set priority",
            issue
                .map(|issue| format!("P{}", issue.priority))
                .unwrap_or_default(),
        ),
    };
    let mut lines = vec![
        issue_title_line(app, &choice.issue_id),
        Line::new(format!("Now: {current}"), dim()),
        Line::blank(),
    ];
    for (index, option) in choice.options.iter().enumerate() {
        let color = match choice.kind {
            ChoiceKind::Status => status_color(option),
            ChoiceKind::Priority => priority_color(PRIORITIES.get(index).copied().unwrap_or(4)),
        };
        lines.push(option_line(option, index == choice.index, color));
    }
    (
        format!("{title} · {}", choice.issue_id),
        lines,
        "j/k or ↑/↓ select · Enter apply · Esc cancel".to_string(),
    )
}

/// One option of a list; `›` and the highlight mark the selected one.
fn option_line(option: &str, selected: bool, color: Color) -> Line {
    let text = format!("{} {option}", if selected { "›" } else { " " });
    if selected {
        Line::new(text, Style::new().fg(color).bg(HIGHLIGHT_BACKGROUND)).filled()
    } else {
        Line::new(text, Style::new().fg(color))
    }
}

fn add_issue_step(flow: &AddIssueFlow, width: usize) -> (String, Vec<Line>, String) {
    let title = if flow.parent_id.is_some() {
        format!("Add child issue · {}/4", flow.step.number())
    } else {
        format!("Add issue · {}/4", flow.step.number())
    };
    if flow.is_confirming_cancel() {
        return (
            title,
            vec![
                Line::new("Discard new issue?", bold()),
                Line::blank(),
                Line::plain("The issue has not been created."),
                Line::blank(),
                Line::new("[y] Discard    [n/Esc] Keep editing", bold()),
            ],
            "y discard · n/Esc keep editing".to_string(),
        );
    }

    let parent = flow
        .parent_id
        .as_deref()
        .unwrap_or("none · new top-level issue");
    let mut lines = vec![Line::new(format!("Parent: {parent}"), dim()), Line::blank()];
    let hints = match flow.step {
        AddIssueStep::Title => {
            lines.push(Line::new("Title", bold()));
            lines.extend(
                hard_wrap(&format!("> {}▏", flow.title), width)
                    .into_iter()
                    .map(Line::plain),
            );
            "Type or paste a title · Enter continue · e $EDITOR (empty title) · Esc cancel"
        }
        AddIssueStep::Description => {
            lines.push(Line::new(format!("Title: {}", flow.title), dim()));
            lines.push(Line::blank());
            lines.push(Line::new("Description", bold()));
            let text = format!("{}▏", flow.description);
            for (index, paragraph) in text.split('\n').enumerate() {
                let prefix = if index == 0 { "> " } else { "  " };
                lines.extend(
                    hard_wrap(&format!("{prefix}{paragraph}"), width)
                        .into_iter()
                        .map(Line::plain),
                );
            }
            "Type or paste a description · Enter continue · e $EDITOR (empty description) · Esc cancel"
        }
        AddIssueStep::IssueType => {
            lines.push(Line::new(format!("Title: {}", flow.title), dim()));
            lines.push(Line::blank());
            lines.push(Line::new("Issue type", bold()));
            for (index, issue_type) in ISSUE_TYPES.iter().enumerate() {
                lines.push(option_line(
                    issue_type,
                    index == flow.issue_type_index,
                    type_color(issue_type),
                ));
            }
            "j/k or ↑/↓ select · Enter continue · Backspace previous · Esc cancel"
        }
        AddIssueStep::Priority => {
            lines.push(Line::new(
                format!("Title: {} · {}", flow.title, flow.issue_type()),
                dim(),
            ));
            lines.push(Line::blank());
            lines.push(Line::new("Priority", bold()));
            for (index, priority) in PRIORITIES.iter().enumerate() {
                let default = if index == DEFAULT_PRIORITY_INDEX {
                    "  default"
                } else {
                    ""
                };
                lines.push(option_line(
                    &format!("P{priority}{default}"),
                    index == flow.priority_index,
                    priority_color(*priority),
                ));
            }
            "j/k or ↑/↓ select · Enter create · Backspace previous · Esc cancel"
        }
    };
    (title, lines, hints.to_string())
}

/// Writes the five table cells from `x`. `issue` gives the cell colors;
/// `None` writes the header.
fn write_table_row(
    buffer: &mut Buffer,
    mut x: u16,
    y: u16,
    cells: &[&str; 5],
    widths: [usize; 5],
    issue: Option<&Issue>,
) {
    let header = issue.is_none();
    for index in 0..5 {
        let mut style = if header { bold() } else { Style::new() };
        if let Some(issue) = issue {
            match index {
                1 => style = style.fg(priority_color(issue.priority)),
                2 => style = style.fg(status_color(&issue.status)),
                3 => style = style.fg(type_color(&issue.issue_type)),
                _ => {}
            }
        }
        x = put(
            buffer,
            x,
            y,
            &pad(cells[index], widths[index]),
            widths[index],
            style,
        );
        if index < 4 {
            x = put(
                buffer,
                x,
                y,
                SEPARATOR,
                SEPARATOR.width(),
                Style::new().fg(Color::DarkGray).add_modifier(Modifier::DIM),
            );
        }
    }
}

fn table_widths(width: usize, id_hint: usize) -> [usize; 5] {
    let usable = width.saturating_sub(SEPARATOR.width() * 4);
    let mut id = id_hint.clamp(8, 28);
    let mut priority = 8;
    let mut status = 12;
    let issue_type = 4;
    let minimum_name = 12;

    while id + priority + status + issue_type + minimum_name > usable && id > 6 {
        id -= 1;
    }
    while id + priority + status + issue_type + minimum_name > usable && status > 6 {
        status -= 1;
    }
    while id + priority + status + issue_type + minimum_name > usable && priority > 4 {
        priority -= 1;
    }
    if id + priority + status + issue_type > usable {
        let base = usable / 5;
        return [base, base, base, base, usable.saturating_sub(base * 4)];
    }
    [
        id,
        priority,
        status,
        issue_type,
        usable - id - priority - status - issue_type,
    ]
}

fn priority_color(priority: i32) -> Color {
    match priority {
        0 => Color::Red,
        1 => Color::Yellow,
        2 => Color::Green,
        3 => Color::Blue,
        _ => Color::DarkGray,
    }
}

fn status_color(status: &str) -> Color {
    match status {
        "open" => Color::Green,
        "in_progress" | "in-progress" => Color::Yellow,
        "blocked" => Color::Red,
        "closed" => Color::DarkGray,
        "deferred" => Color::Blue,
        _ => Color::White,
    }
}

fn type_color(issue_type: &str) -> Color {
    match issue_type {
        "feature" | "feat" => Color::Cyan,
        "epic" => Color::Magenta,
        "bug" => Color::Red,
        "task" => Color::Blue,
        "chore" => Color::Yellow,
        "merge-request" => Color::Green,
        _ => Color::DarkGray,
    }
}

pub fn short_type(issue_type: &str) -> String {
    match issue_type {
        "feature" | "feat" => "feat".to_string(),
        "epic" => "epic".to_string(),
        "bug" => "bug".to_string(),
        "task" => "task".to_string(),
        "chore" => "chor".to_string(),
        "merge-request" => "mr".to_string(),
        other => truncate(other, 4),
    }
}

fn display_timestamp(timestamp: &str) -> String {
    if timestamp.len() >= 16 && timestamp.as_bytes().get(10) == Some(&b'T') {
        format!("{} {}", &timestamp[..10], &timestamp[11..16])
    } else if timestamp.is_empty() {
        "—".to_string()
    } else {
        timestamp.to_string()
    }
}

fn pad(value: &str, width: usize) -> String {
    let value = truncate(value, width);
    let used = value.width();
    format!("{value}{}", " ".repeat(width.saturating_sub(used)))
}

pub fn truncate(value: &str, width: usize) -> String {
    if width == 0 {
        return String::new();
    }
    if value.width() <= width {
        return value.to_string();
    }
    if width == 1 {
        return "…".to_string();
    }
    let mut output = String::new();
    let mut used = 0;
    for character in value.chars() {
        let character_width = character.width().unwrap_or(0);
        if used + character_width > width - 1 {
            break;
        }
        output.push(character);
        used += character_width;
    }
    output.push('…');
    output
}

/// Breaks `value` into lines of at most `width` columns at any character,
/// keeping every space, for text the user is typing.
fn hard_wrap(value: &str, width: usize) -> Vec<String> {
    let width = width.max(1);
    let mut lines = vec![String::new()];
    let mut used = 0;
    for character in value.chars() {
        let character_width = character.width().unwrap_or(0);
        if used + character_width > width {
            lines.push(String::new());
            used = 0;
        }
        lines
            .last_mut()
            .expect("lines is never empty")
            .push(character);
        used += character_width;
    }
    lines
}

/// Breaks `value` into lines of at most `width` columns at word boundaries.
fn wrap(value: &str, width: usize) -> Vec<String> {
    if value.is_empty() {
        return vec![String::new()];
    }
    let width = width.max(1);
    let mut result = Vec::new();
    for paragraph in value.lines() {
        if paragraph.trim().is_empty() {
            result.push(String::new());
            continue;
        }
        let mut line = String::new();
        for word in paragraph.split_whitespace() {
            let next_width = if line.is_empty() {
                word.width()
            } else {
                line.width() + 1 + word.width()
            };
            if next_width <= width {
                if !line.is_empty() {
                    line.push(' ');
                }
                line.push_str(word);
            } else {
                if !line.is_empty() {
                    result.push(line);
                }
                line = word.to_string();
                while line.width() > width {
                    let mut piece = String::new();
                    let mut used = 0;
                    let mut consumed = 0;
                    for character in line.chars() {
                        let character_width = character.width().unwrap_or(0);
                        if consumed > 0 && used + character_width > width {
                            break;
                        }
                        piece.push(character);
                        used += character_width;
                        consumed += 1;
                        if used >= width {
                            break;
                        }
                    }
                    result.push(piece);
                    line = line.chars().skip(consumed).collect();
                }
            }
        }
        if !line.is_empty() {
            result.push(line);
        }
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Issue, IssueGraph};
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;
    use std::collections::HashMap;

    fn app() -> App {
        App::new(IssueGraph::new(
            vec![Issue {
                id: "task-1".into(),
                title: "A task".into(),
                status: "open".into(),
                ..Issue::default()
            }],
            vec![],
        ))
    }

    fn press(app: &mut App, code: KeyCode) {
        app.handle_key(KeyEvent::new(code, KeyModifiers::NONE));
    }

    /// Draws `app` and returns the screen buffer.
    fn render_buffer(app: &mut App, width: u16, height: u16) -> Buffer {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        terminal.draw(|frame| draw(app, frame)).unwrap();
        terminal.backend().buffer().clone()
    }

    /// Draws `app` and returns the screen as text, one line per row.
    fn render(app: &mut App, width: u16, height: u16) -> String {
        let buffer = render_buffer(app, width, height);
        (0..height)
            .map(|y| {
                (0..width)
                    .map(|x| buffer[(x, y)].symbol())
                    .collect::<String>()
                    .trim_end()
                    .to_string()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn close_confirmation_replaces_the_screen() {
        let mut app = app();
        press(&mut app, KeyCode::Char('x'));

        let screen = render(&mut app, 80, 20);
        assert!(screen.starts_with(" Close issue · task-1"));
        assert!(screen.contains("Close task-1?"));
        assert!(screen.contains("[y] Yes    [n/Esc] No"));
        assert!(
            !screen.contains("PRIORITY"),
            "the tree is not drawn behind it"
        );
    }

    #[test]
    fn add_issue_and_discard_confirmation_are_full_screen_steps() {
        let mut app = app();
        press(&mut app, KeyCode::Char('+'));

        let screen = render(&mut app, 100, 24);
        assert!(screen.starts_with(" Add child issue · 1/4"));
        assert!(screen.contains("Parent: task-1"));
        assert!(screen.contains("e $EDITOR"));

        press(&mut app, KeyCode::Esc);
        let screen = render(&mut app, 100, 24);
        assert!(screen.contains("Discard new issue?"));
        assert!(screen.contains("[y] Discard"));
    }

    #[test]
    fn tree_renders_the_create_entry_and_the_status_step() {
        let mut app = app();
        assert!(render(&mut app, 80, 20).contains("+ Create New"));

        press(&mut app, KeyCode::Char('s'));
        let screen = render(&mut app, 80, 24);
        assert!(screen.starts_with(" Set status · task-1"));
        assert!(screen.contains("› open"));
        assert!(screen.contains("  in_progress"));
        assert!(screen.contains("Enter apply"));
    }

    #[test]
    fn command_step_shows_the_typed_command() {
        let mut app = app();
        app.set_tmux(true);
        press(&mut app, KeyCode::Char('a'));
        app.handle_paste(" --verbose");

        let screen = render(&mut app, 80, 20);
        assert!(screen.starts_with(" Start agent · task-1"));
        assert!(screen.contains("> claude --effort medium \"/ship 1\" --verbose▏"));
    }

    #[test]
    fn a_live_pane_shows_in_the_tree_and_the_task_view() {
        let mut app = app();
        app.set_tmux(true);
        let screen = render(&mut app, 200, 10);
        assert!(!screen.contains('●'));
        assert!(screen.contains("a agent · f focus"));

        app.set_panes(HashMap::from([("task-1".to_string(), "%9".to_string())]));
        let screen = render(&mut app, 80, 10);
        assert!(screen.lines().nth(2).unwrap().starts_with("● task-1"));

        press(&mut app, KeyCode::Enter);
        assert!(render(&mut app, 80, 20).contains("pane %9 · f focus"));
    }

    #[test]
    fn task_view_shows_each_link_with_the_linked_issue_title_and_status() {
        // `bd list` edges carry only the target ID and the link type.
        let edge = |id: &str, kind: &str| crate::model::Dependency {
            id: id.into(),
            dependency_type: kind.into(),
            ..Default::default()
        };
        let blocked = Issue {
            id: "b".into(),
            title: "Menu bar".into(),
            dependencies: vec![edge("a", "blocks"), edge("gone", "related")],
            ..Issue::default()
        };
        let blocker = Issue {
            id: "a".into(),
            title: "Window chrome".into(),
            status: "in_progress".into(),
            ..Issue::default()
        };
        let mut app = App::new(IssueGraph::new(vec![blocked, blocker], vec![]));
        press(&mut app, KeyCode::Enter);

        let screen = render(&mut app, 80, 20);
        assert!(screen.contains("→ a [in_progress] Window chrome (blocks)"));
        assert!(screen.contains("→ gone [?] (not found) (related)"));
    }

    #[test]
    fn outside_tmux_there_is_no_pane_column_or_agent_hint() {
        let mut app = app();
        let screen = render(&mut app, 120, 10);
        assert!(screen.lines().nth(2).unwrap().starts_with("task-1"));
        assert!(!screen.contains("a agent"));
    }

    #[test]
    fn status_message_replaces_the_footer_hints() {
        let mut app = app();
        app.set_status("bd close failed: boom".to_string());

        let screen = render(&mut app, 80, 20);
        assert!(screen.contains("bd close failed: boom"));
        assert!(!screen.contains("q quit"));
    }

    #[test]
    fn highlights_use_a_background_and_keep_label_colors() {
        let mut issue_app = App::new(IssueGraph::new(
            vec![Issue {
                id: "issue-1".into(),
                priority: 0,
                status: "open".into(),
                issue_type: "feature".into(),
                ..Issue::default()
            }],
            vec![],
        ));
        let buffer = render_buffer(&mut issue_app, 80, 5);
        let header = &buffer[(0, 0)];
        assert_eq!(header.bg, HIGHLIGHT_BACKGROUND);
        assert!(header.modifier.contains(Modifier::BOLD));

        // Row 2 is the selected issue: ID, then priority after one separator.
        let id = &buffer[(0, 2)];
        assert_eq!(id.bg, HIGHLIGHT_BACKGROUND);
        assert!(!id.modifier.contains(Modifier::REVERSED));
        let priority_x = (8 + SEPARATOR.width()) as u16;
        assert_eq!(buffer[(priority_x, 2)].fg, priority_color(0));
        assert_eq!(buffer[(priority_x, 2)].bg, HIGHLIGHT_BACKGROUND);
        assert_eq!(
            buffer[(79, 2)].bg,
            HIGHLIGHT_BACKGROUND,
            "the highlight fills the row"
        );
    }

    #[test]
    fn type_labels_are_never_wider_than_four() {
        for issue_type in ["feature", "epic", "bug", "task", "chore", "molecule"] {
            assert!(short_type(issue_type).width() <= 4);
        }
    }

    #[test]
    fn primary_types_have_distinct_colors() {
        let colors = ["feature", "epic", "bug", "task"].map(type_color);
        for (index, color) in colors.iter().enumerate() {
            assert!(!colors[..index].contains(color));
        }
    }

    #[test]
    fn truncation_respects_unicode_width() {
        assert_eq!(truncate("abcdef", 4), "abc…");
        assert_eq!(truncate("界界界", 5), "界界…");
    }

    #[test]
    fn wraps_description_to_terminal_width() {
        assert_eq!(wrap("one two three", 7), vec!["one two", "three"]);
        assert_eq!(hard_wrap("ab  cd", 3), vec!["ab ", " cd"]);
    }

    #[test]
    fn tiny_terminals_do_not_panic() {
        let mut app = app();
        for (width, height) in [(1, 1), (3, 2), (10, 3)] {
            render(&mut app, width, height);
            press(&mut app, KeyCode::Enter);
            render(&mut app, width, height);
            press(&mut app, KeyCode::Esc);
            press(&mut app, KeyCode::Char('s'));
            render(&mut app, width, height);
            press(&mut app, KeyCode::Esc);
        }
    }
}
