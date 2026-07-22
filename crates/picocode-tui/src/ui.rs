//! ratatui rendering: transcript, input box, status bar, approval modal.

use ratatui::Frame;
use ratatui::layout::{Alignment, Constraint, Layout, Position, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Clear, Paragraph};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use crate::app::{
    AddModelForm, App, EntryKind, ModelPicker, PendingApproval, PendingQuestion, SessionPicker,
};

/// Column width of the setting names in the `/config` dialog.
const SETTING_NAME_COL: usize = 12;

const SPINNER: &[&str] = &["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

pub fn draw(f: &mut Frame, app: &mut App) {
    // The input box grows with the number of lines being written (up to 8).
    let input_lines = app.input.split('\n').count().clamp(1, 8) as u16;
    let [transcript, input, status] = Layout::vertical([
        Constraint::Min(1),
        Constraint::Length(input_lines + 2),
        Constraint::Length(1),
    ])
    .areas(f.area());

    draw_transcript(f, app, transcript);
    draw_input(f, app, input);
    draw_status(f, app, status);

    if app.pending.is_none()
        && app.session_picker.is_none()
        && app.model_picker.is_none()
        && app.add_model.is_none()
        && app.settings.is_none()
        && app.question.is_none()
    {
        let matches = app.completions();
        if !matches.is_empty() {
            draw_completions(f, app, &matches, input);
        }
    }
    if let Some(picker) = &app.session_picker {
        draw_session_picker(f, picker);
    }
    if let Some(picker) = &app.model_picker {
        draw_model_picker(f, picker);
    }
    if let Some(form) = &app.add_model {
        draw_add_model(f, form);
    }
    if app.settings.is_some() {
        draw_settings(f, app);
    }
    if let Some(q) = &app.question {
        draw_question(f, q);
    }
    if let Some(pending) = &app.pending {
        draw_approval(f, pending);
    }
}

// ----- command completion popup --------------------------------------------

fn draw_completions(f: &mut Frame, app: &App, matches: &[(String, String)], input_area: Rect) {
    // Wide enough for the longest fill text (argument completions carry the
    // whole command line), min 8 for the bare command names.
    let cmd_col = matches
        .iter()
        .map(|(cmd, _)| cmd.chars().count())
        .max()
        .unwrap_or(8)
        .max(8);
    let height = (matches.len() as u16 + 2).min(input_area.y);
    if height < 3 {
        return;
    }
    let inner_width = matches
        .iter()
        .map(|(_, desc)| cmd_col + desc.len() + 3)
        .max()
        .unwrap_or(20) as u16;
    let width = (inner_width + 2).min(f.area().width.saturating_sub(2));
    let area = Rect {
        x: input_area.x + 1,
        y: input_area.y.saturating_sub(height),
        width,
        height,
    };

    let selected = app.comp_selected.min(matches.len() - 1);
    let lines: Vec<Line> = matches
        .iter()
        .enumerate()
        .map(|(i, (cmd, desc))| {
            let text = format!(" {cmd:<cmd_col$} {desc}");
            if i == selected {
                Line::from(Span::styled(
                    text,
                    Style::new().fg(Color::Black).bg(Color::Cyan),
                ))
            } else {
                Line::from(vec![
                    Span::styled(format!(" {cmd:<cmd_col$}"), Style::new().fg(Color::Cyan)),
                    Span::styled(format!(" {desc}"), Style::new().fg(Color::DarkGray)),
                ])
            }
        })
        .collect();

    let block = Block::bordered()
        .title(" Tab: complete · ↑↓: select ")
        .border_style(Style::new().fg(Color::DarkGray));
    f.render_widget(Clear, area);
    f.render_widget(Paragraph::new(lines).block(block), area);
}

// ----- transcript ----------------------------------------------------------

fn draw_transcript(f: &mut Frame, app: &mut App, area: Rect) {
    let width = area.width.saturating_sub(1) as usize;
    if width < 4 {
        return;
    }
    let lines = transcript_lines(app, width);
    let height = area.height as usize;
    let total = lines.len();

    // Record layout for the scroll key handlers.
    app.last_total_lines = total;
    app.last_view_height = height;

    let max_top = total.saturating_sub(height);
    let top = if app.follow {
        max_top
    } else {
        app.top_line.min(max_top)
    };
    app.top_line = top;

    let end = (top + height).min(total);
    let visible: Vec<Line> = lines[top..end].to_vec();
    f.render_widget(Paragraph::new(visible), area);
}

fn transcript_lines(app: &App, width: usize) -> Vec<Line<'static>> {
    let mut lines: Vec<Line<'static>> = Vec::new();
    for entry in &app.entries {
        match entry.kind {
            EntryKind::User => {
                lines.push(Line::default());
                push_wrapped(&mut lines, &entry.text, width.saturating_sub(2), |i, s| {
                    if i == 0 {
                        Line::from(vec![
                            Span::styled("❯ ", Style::new().fg(Color::Cyan).bold()),
                            Span::styled(s, Style::new().bold()),
                        ])
                    } else {
                        Line::from(Span::styled(format!("  {s}"), Style::new().bold()))
                    }
                });
                for name in &entry.attachments {
                    lines.push(Line::from(Span::styled(
                        format!("  📎 {name}"),
                        Style::new().fg(Color::DarkGray),
                    )));
                }
            }
            EntryKind::Assistant => {
                lines.push(Line::default());
                lines.extend(crate::markdown::render(&entry.text, width));
            }
            EntryKind::Reasoning => {
                lines.push(Line::default());
                let dim = Style::new()
                    .fg(Color::DarkGray)
                    .add_modifier(Modifier::ITALIC);
                if app.show_reasoning {
                    lines.push(Line::from(Span::styled(
                        "∴ thinking (Ctrl+T to collapse)".to_string(),
                        dim,
                    )));
                    push_wrapped(&mut lines, &entry.text, width.saturating_sub(2), |_, s| {
                        Line::from(Span::styled(format!("  {s}"), dim))
                    });
                } else {
                    // Collapsed: one dim line with a live-updating size.
                    let n = entry.text.lines().count();
                    let plural = if n == 1 { "" } else { "s" };
                    lines.push(Line::from(Span::styled(
                        format!("∴ thinking … {n} line{plural} (Ctrl+T to expand)"),
                        dim,
                    )));
                }
            }
            EntryKind::Tool => {
                lines.push(Line::default());
                push_wrapped(&mut lines, &entry.text, width.saturating_sub(2), |i, s| {
                    let prefix = if i == 0 { "⚙ " } else { "  " };
                    Line::from(Span::styled(
                        format!("{prefix}{s}"),
                        Style::new().fg(Color::Yellow),
                    ))
                });
            }
            EntryKind::ToolOut => {
                push_wrapped(&mut lines, &entry.text, width.saturating_sub(4), |_, s| {
                    Line::from(Span::styled(
                        format!("    {s}"),
                        Style::new().fg(Color::DarkGray),
                    ))
                });
            }
            EntryKind::Diff => {
                // Syntax-highlighted foregrounds; +/- rows are marked by the
                // background, extended to the full line width (delta-style).
                let token = entry.lang.as_deref().unwrap_or("");
                for (bg, span_line) in crate::highlight::diff_spans(&entry.text, token) {
                    for chunk in crate::highlight::wrap_spans(&span_line, width.saturating_sub(2)) {
                        let mut used = 2usize;
                        let mut spans = vec![Span::raw("  ")];
                        for (st, s) in chunk {
                            used += s.width();
                            spans.push(Span::styled(s, st));
                        }
                        if let Some(bg) = bg
                            && used < width
                        {
                            spans.push(Span::styled(" ".repeat(width - used), Style::new().bg(bg)));
                        }
                        lines.push(Line::from(spans));
                    }
                }
            }
            EntryKind::Notice => {
                push_wrapped(&mut lines, &entry.text, width.saturating_sub(2), |_, s| {
                    Line::from(Span::styled(format!("· {s}"), Style::new().fg(Color::Blue)))
                });
            }
            EntryKind::Warning => {
                push_wrapped(&mut lines, &entry.text, width.saturating_sub(2), |i, s| {
                    let prefix = if i == 0 { "⚠ " } else { "  " };
                    Line::from(Span::styled(
                        format!("{prefix}{s}"),
                        Style::new().fg(Color::Yellow).bold(),
                    ))
                });
            }
            EntryKind::Summary => {
                lines.push(Line::default());
                push_wrapped(&mut lines, &entry.text, width.saturating_sub(2), |_, s| {
                    Line::from(Span::styled(format!("  {s}"), Style::new().fg(Color::Gray)))
                });
            }
            EntryKind::Context => {
                match picocode_core::context::Breakdown::decode(&entry.text) {
                    Some(breakdown) => lines.extend(context_lines(&breakdown, width)),
                    // A breakdown from a newer version we can't parse.
                    None => {
                        push_wrapped(&mut lines, &entry.text, width.saturating_sub(2), |_, s| {
                            Line::from(Span::styled(
                                format!("  {s}"),
                                Style::new().fg(Color::DarkGray),
                            ))
                        })
                    }
                }
            }
            EntryKind::Error => {
                push_wrapped(&mut lines, &entry.text, width.saturating_sub(2), |i, s| {
                    let prefix = if i == 0 { "✗ " } else { "  " };
                    Line::from(Span::styled(
                        format!("{prefix}{s}"),
                        Style::new().fg(Color::Red),
                    ))
                });
            }
            EntryKind::Logo => {
                // Verbatim, unwrapped: block art would fall apart if wrapped
                // (overflow is clipped on narrow terminals).
                for raw in entry.text.lines() {
                    lines.push(Line::from(Span::styled(
                        raw.to_string(),
                        Style::new().fg(Color::White),
                    )));
                }
                lines.push(Line::default());
            }
        }
    }
    lines
}

/// Color for one context-breakdown segment.
fn context_color(kind: picocode_core::context::ContextKind) -> Color {
    use picocode_core::context::ContextKind::*;
    match kind {
        System => Color::Blue,
        Instructions => Color::Cyan,
        User => Color::Green,
        Assistant => Color::Magenta,
        Tools => Color::Yellow,
        Media => Color::LightRed,
        Overhead => Color::DarkGray,
    }
}

/// The /status context block: a segmented colored bar over the window,
/// then one legend line per non-empty segment.
fn context_lines(
    breakdown: &picocode_core::context::Breakdown,
    width: usize,
) -> Vec<Line<'static>> {
    let window = breakdown.window.max(1);
    let pct = |v: u64| (v as f64 / window as f64 * 100.0).round() as u64;
    let mut lines = vec![
        Line::default(),
        Line::from(Span::styled(
            format!(
                "  context breakdown — {} of {} tokens ({}%){}",
                breakdown.used(),
                breakdown.window,
                pct(breakdown.used()),
                if breakdown.reported == 0 {
                    " — estimated"
                } else {
                    ""
                },
            ),
            Style::new().bold(),
        )),
    ];

    // The bar: segments scaled to the window; every non-empty segment gets
    // at least one cell so tiny ones stay visible.
    let bar_width = width.saturating_sub(4).min(60).max(10);
    let mut spans = vec![Span::raw("  ")];
    let mut cells_used = 0usize;
    for (kind, tokens) in &breakdown.segments {
        if *tokens == 0 {
            continue;
        }
        let cells = ((*tokens as f64 / window as f64 * bar_width as f64).round() as usize)
            .max(1)
            .min(bar_width - cells_used.min(bar_width));
        if cells == 0 {
            continue;
        }
        cells_used += cells;
        spans.push(Span::styled(
            "█".repeat(cells),
            Style::new().fg(context_color(*kind)),
        ));
    }
    if cells_used < bar_width {
        spans.push(Span::styled(
            "░".repeat(bar_width - cells_used),
            Style::new().fg(Color::DarkGray),
        ));
    }
    lines.push(Line::from(spans));

    // Legend: one colored line per non-empty segment, plus free space.
    for (kind, tokens) in &breakdown.segments {
        if *tokens == 0 {
            continue;
        }
        lines.push(Line::from(vec![
            Span::styled("  ■ ", Style::new().fg(context_color(*kind))),
            Span::raw(format!("{:<24}", kind.label())),
            Span::raw(format!("{:>8} tokens ({}%)", tokens, pct(*tokens))),
        ]));
    }
    lines.push(Line::from(vec![
        Span::styled("  □ ", Style::new().fg(Color::DarkGray)),
        Span::styled(format!("{:<24}", "free"), Style::new().fg(Color::DarkGray)),
        Span::styled(
            format!(
                "{:>8} tokens ({}%)",
                breakdown.free(),
                pct(breakdown.free())
            ),
            Style::new().fg(Color::DarkGray),
        ),
    ]));
    lines
}

fn push_wrapped(
    lines: &mut Vec<Line<'static>>,
    text: &str,
    width: usize,
    mut make_line: impl FnMut(usize, String) -> Line<'static>,
) {
    let width = width.max(4);
    let mut i = 0;
    for raw in text.split('\n') {
        if raw.is_empty() {
            lines.push(make_line(i, String::new()));
            i += 1;
            continue;
        }
        for piece in textwrap::wrap(raw, width) {
            lines.push(make_line(i, piece.into_owned()));
            i += 1;
        }
    }
}

// ----- input ---------------------------------------------------------------

fn draw_input(f: &mut Frame, app: &App, area: Rect) {
    // A leading `!` means the input is a direct shell command and a leading
    // `/` a slash command; recolor the box so the mode is obvious while
    // typing.
    let (title, border) = if app.input.starts_with('!') {
        (" shell ", Style::new().fg(Color::Yellow))
    } else if app.input.starts_with('/') {
        (" command ", Style::new().fg(Color::Cyan))
    } else {
        (" picocode ", Style::new().fg(Color::DarkGray))
    };
    let block = Block::bordered()
        .title(title)
        .title(Span::styled(
            format!(" {} ", app.workdir_label()),
            Style::new().fg(Color::DarkGray),
        ))
        .border_style(border);
    let inner_width = area.width.saturating_sub(2) as usize;
    let inner_height = (area.height.saturating_sub(2) as usize).max(1);
    let dialog_open = app.pending.is_some()
        || app.session_picker.is_some()
        || app.model_picker.is_some()
        || app.add_model.is_some()
        || app.settings.is_some()
        || app.question.is_some();

    if app.input.is_empty() {
        let hint = Paragraph::new(Span::styled(
            "Type a message",
            Style::new().fg(Color::DarkGray),
        ))
        .block(block);
        f.render_widget(hint, area);
        if !dialog_open {
            f.set_cursor_position(Position::new(area.x + 1, area.y + 1));
        }
        return;
    }

    let (cursor_row, cursor_col) = crate::input::line_col(&app.input, app.cursor);
    let lines: Vec<&str> = app.input.split('\n').collect();
    // Vertical window: keep the cursor's row visible (relevant only when
    // there are more lines than the box's growth cap).
    let top = cursor_row.saturating_sub(inner_height - 1);

    let mut visible: Vec<Line> = Vec::new();
    let mut cursor_pos = None;
    for (i, line) in lines.iter().enumerate().skip(top).take(inner_height) {
        if i == cursor_row {
            // Horizontal scroll on the cursor's line: drop chars from the
            // left until the cursor fits. Other lines are simply clipped.
            let chars: Vec<char> = line.chars().collect();
            let width = |from: usize, to: usize| -> usize {
                chars[from..to].iter().map(|c| c.width().unwrap_or(0)).sum()
            };
            let mut start = 0usize;
            while start < cursor_col && width(start, cursor_col) >= inner_width.saturating_sub(1) {
                start += 1;
            }
            cursor_pos = Some((width(start, cursor_col), i - top));
            visible.push(Line::raw(chars[start..].iter().collect::<String>()));
        } else {
            visible.push(Line::raw((*line).to_string()));
        }
    }

    f.render_widget(Paragraph::new(visible).block(block), area);
    if !dialog_open && let Some((x, y)) = cursor_pos {
        f.set_cursor_position(Position::new(area.x + 1 + x as u16, area.y + 1 + y as u16));
    }
}

// ----- status bar ----------------------------------------------------------

fn draw_status(f: &mut Frame, app: &App, area: Rect) {
    let indicator = if app.running > 0 {
        // Waiting for the API to start answering vs. tokens streaming in.
        if app.waiting {
            Span::styled(
                format!("{} waiting", SPINNER[app.spinner % SPINNER.len()]),
                Style::new().fg(Color::Yellow),
            )
        } else {
            Span::styled(
                format!("{} running", SPINNER[app.spinner % SPINNER.len()]),
                Style::new().fg(Color::Green),
            )
        }
    } else {
        Span::styled("● idle", Style::new().fg(Color::DarkGray))
    };
    let mode = app.mode();
    let mode_style = match mode {
        picocode_core::config::Mode::ReadOnly => Style::new().fg(Color::Cyan),
        picocode_core::config::Mode::Edit => Style::new().fg(Color::Yellow),
        picocode_core::config::Mode::Plan => Style::new().fg(Color::Blue),
        picocode_core::config::Mode::Bypass => Style::new().fg(Color::Red).bold(),
    };
    let mut left = vec![
        Span::raw(" "),
        Span::styled(format!("[{}]", mode.label()), mode_style),
        Span::raw("  "),
        indicator,
    ];
    if app.running > 0 {
        left.push(Span::styled("  Esc stop", Style::new().fg(Color::DarkGray)));
    }
    if app.background_jobs > 0 {
        left.push(Span::styled(
            format!(" {} bg", app.background_jobs),
            Style::new().fg(Color::Yellow),
        ));
    }
    if !app.follow {
        left.push(Span::styled(
            "  ⇡ scrolled (PgDn to bottom)",
            Style::new().fg(Color::Yellow),
        ));
    }

    // Context-window usage: a flat tqdm-style bar (eighth-block resolution),
    // colored by pressure.
    const GAUGE_CELLS: usize = 10;
    const PARTIALS: [&str; 8] = ["", "▏", "▎", "▍", "▌", "▋", "▊", "▉"];
    let ratio = app.context_ratio();
    let eighths = (ratio.min(1.0) * (GAUGE_CELLS * 8) as f64).round() as usize;
    let (full, rem) = (eighths / 8, eighths % 8);
    let bar = format!("{}{}", "█".repeat(full), PARTIALS[rem]);
    let rest = " ".repeat(GAUGE_CELLS - full - usize::from(rem > 0));
    let gauge_color = if ratio >= 0.85 {
        Color::Red
    } else if ratio >= 0.6 {
        Color::Yellow
    } else {
        Color::Green
    };
    let dim = Style::new().fg(Color::DarkGray);

    // Right block: [↑ prefill ↓ decode while running] gauge % model.
    let mut right = vec![
        // Leading gap so a truncated left side never touches the right block.
        Span::raw(" "),
    ];
    if app.running > 0 {
        right.push(Span::styled(
            format!("↑ {} ↓ {}  ", app.ctx_tokens, app.turn_out + app.delta_est),
            dim,
        ));
    }
    right.push(Span::styled(bar, Style::new().fg(gauge_color)));
    right.push(Span::styled(rest, dim));
    right.push(Span::styled("▏", dim));
    right.push(Span::styled(
        format!("{:>3}%", (ratio * 100.0).round().min(999.0) as u64),
        dim,
    ));
    right.push(Span::raw("  "));
    right.push(Span::styled(
        app.model_label.clone(),
        Style::new().fg(Color::Magenta),
    ));
    right.push(Span::raw(" "));
    let right = Line::from(right);
    let right_width = (right.width() as u16).min(area.width);
    let [left_area, right_area] =
        Layout::horizontal([Constraint::Min(0), Constraint::Length(right_width)]).areas(area);
    f.render_widget(Paragraph::new(Line::from(left)), left_area);
    f.render_widget(
        Paragraph::new(right).alignment(Alignment::Right),
        right_area,
    );
}

// ----- dialog building blocks ----------------------------------------------

/// Centered dialog rectangle: width clamped to `(min, max)`, height capped
/// to the screen (both leave a margin).
fn dialog_area(screen: Rect, (min_width, max_width): (u16, u16), height: u16) -> Rect {
    let width = screen.width.saturating_sub(6).clamp(min_width, max_width);
    let height = height.min(screen.height.saturating_sub(4));
    Rect {
        x: screen.x + (screen.width.saturating_sub(width)) / 2,
        y: screen.y + (screen.height.saturating_sub(height)) / 2,
        width,
        height,
    }
}

/// One row of a selection list: clipped to the width and rendered as the
/// full-width selection bar when selected, in `style` otherwise.
fn list_line(text: String, selected: bool, inner_width: usize, style: Style) -> Line<'static> {
    let text: String = text.chars().take(inner_width).collect();
    if selected {
        Line::from(Span::styled(
            format!("{text:<inner_width$}"),
            Style::new().fg(Color::Black).bg(Color::Cyan),
        ))
    } else {
        Line::from(Span::styled(text, style))
    }
}

/// The dim key-hint line at the bottom of a dialog.
fn hint_line(text: &str) -> Line<'static> {
    Line::from(Span::styled(
        text.to_string(),
        Style::new().fg(Color::DarkGray),
    ))
}

/// Clear the area and render a bordered dialog around the lines.
fn render_dialog(f: &mut Frame, title: &str, border: Color, area: Rect, lines: Vec<Line>) {
    let block = Block::bordered()
        .title(format!(" {title} "))
        .border_style(Style::new().fg(border));
    f.render_widget(Clear, area);
    f.render_widget(Paragraph::new(lines).block(block), area);
}

// ----- session picker ------------------------------------------------------

fn draw_session_picker(f: &mut Frame, picker: &SessionPicker) {
    let screen = f.area();
    // rows + borders + hint line, capped to the screen.
    let area = dialog_area(screen, (30, 90), (picker.sessions.len() as u16 + 3).max(5));
    let inner_width = area.width.saturating_sub(2) as usize;
    let visible = area.height.saturating_sub(3) as usize;
    // Keep the selection inside the window when the list is long.
    let offset = (picker.selected + 1).saturating_sub(visible);

    let mut lines: Vec<Line> = Vec::new();
    for (i, s) in picker
        .sessions
        .iter()
        .enumerate()
        .skip(offset)
        .take(visible)
    {
        let snippet = if s.snippet.is_empty() {
            "(no prompt)".to_string()
        } else {
            format!("\"{}\"", s.snippet)
        };
        let text = format!(
            " {} · {} msgs · {} · {snippet}",
            picocode_core::session::age(s.modified),
            s.messages,
            s.model,
        );
        lines.push(list_line(
            text,
            i == picker.selected,
            inner_width,
            Style::new(),
        ));
    }
    lines.push(hint_line(" ↑↓ select · Enter resume · Esc cancel"));
    render_dialog(f, "Resume session", Color::Cyan, area, lines);
}

// ----- model picker --------------------------------------------------------

fn draw_model_picker(f: &mut Frame, picker: &ModelPicker) {
    let screen = f.area();
    let filtered = picker.filtered();
    // search line + rows + the "+ add" row + borders + hint line.
    let area = dialog_area(screen, (30, 70), (filtered.len().max(1) as u16 + 5).max(7));
    let inner_width = area.width.saturating_sub(2) as usize;
    let visible = area.height.saturating_sub(4) as usize;
    // Keep the selection inside the window when the list is long.
    let offset = (picker.selected + 1).saturating_sub(visible);

    let mut lines: Vec<Line> = vec![Line::from(vec![
        Span::styled(" search: ", Style::new().fg(Color::DarkGray)),
        Span::styled(format!("{}▏", picker.filter), Style::new().fg(Color::Cyan)),
    ])];
    if picker.items.is_empty() {
        lines.push(hint_line(" fetching the provider's model list…"));
    } else if filtered.is_empty() {
        lines.push(hint_line(" no match"));
    }
    for (i, item) in filtered.iter().enumerate().skip(offset).take(visible) {
        let marker = if item.active { "▸" } else { " " };
        let style = if item.active {
            Style::new().fg(Color::Cyan)
        } else {
            Style::new()
        };
        lines.push(list_line(
            format!("{marker} {} — {}", item.name, item.detail),
            i == picker.selected,
            inner_width,
            style,
        ));
    }
    // Synthetic last row opening the add-model form.
    lines.push(list_line(
        "  + add a provider / model…".to_string(),
        picker.selected == filtered.len(),
        inner_width,
        Style::new().fg(Color::DarkGray),
    ));
    lines.push(hint_line(" type to search · ↑↓ · Enter switch · Esc close"));
    render_dialog(f, "Select model", Color::Cyan, area, lines);
}

/// The add-model form: provider (←→ cycles), base URL, model, and the
/// endpoint's served models below (Tab fetches, ↑↓ walks into the list).
fn draw_add_model(f: &mut Frame, form: &AddModelForm) {
    let screen = f.area();
    let rows = 3 + form.fetched.len();
    let area = dialog_area(screen, (30, 70), rows as u16 + 4);
    let inner_width = area.width.saturating_sub(2) as usize;
    let visible = (area.height.saturating_sub(4) as usize).max(3);
    let provider = picocode_core::config::provider_name(form.provider);

    let field = |text: &str| if text.is_empty() { "…" } else { text }.to_string();
    let mut lines: Vec<Line> = vec![
        list_line(
            format!(" provider  ‹ {provider} ›"),
            form.field == 0,
            inner_width,
            Style::new(),
        ),
        list_line(
            format!(" base URL  {} (empty = default)", field(&form.base_url)),
            form.field == 1,
            inner_width,
            Style::new(),
        ),
        list_line(
            format!(" model     {}", field(&form.model)),
            form.field == 2,
            inner_width,
            Style::new(),
        ),
    ];
    // Fetched list, windowed like the model picker.
    let offset = (form.field.saturating_sub(2)).saturating_sub(visible.saturating_sub(3));
    for (i, id) in form
        .fetched
        .iter()
        .enumerate()
        .skip(offset)
        .take(visible.saturating_sub(3))
    {
        lines.push(list_line(
            format!("   {id}"),
            form.field == 3 + i,
            inner_width,
            Style::new(),
        ));
    }
    lines.push(hint_line(&format!(" {}", form.note)));
    lines.push(hint_line(
        " ↑↓ field · ←→ provider · Tab fetch · Enter switch · Esc back",
    ));
    render_dialog(f, "Add provider / model", Color::Cyan, area, lines);
}

// ----- settings dialog -----------------------------------------------------

fn draw_settings(f: &mut Frame, app: &App) {
    let Some(menu) = &app.settings else { return };
    let rows = app.settings_rows();
    let screen = f.area();
    // rows + borders + hint line.
    let area = dialog_area(screen, (30, 60), rows.len() as u16 + 3);
    let inner_width = area.width.saturating_sub(2) as usize;

    let mut lines: Vec<Line> = Vec::new();
    for (i, (name, value, hint)) in rows.iter().enumerate() {
        let value = if *hint == "← →" {
            format!("‹ {value} ›")
        } else {
            value.clone()
        };
        let text = format!(" {name:<SETTING_NAME_COL$} {value}");
        lines.push(if i == menu.selected {
            list_line(text, true, inner_width, Style::new())
        } else {
            // Unselected rows dim the name column.
            let text: String = text.chars().take(inner_width).collect();
            Line::from(vec![
                Span::styled(
                    format!(" {name:<SETTING_NAME_COL$} "),
                    Style::new().fg(Color::DarkGray),
                ),
                Span::raw(text.chars().skip(SETTING_NAME_COL + 2).collect::<String>()),
            ])
        });
    }
    lines.push(hint_line(
        " ↑↓ select · ←→ change · Enter pick model · Esc close",
    ));
    render_dialog(f, "Settings (this session)", Color::Cyan, area, lines);
}

// ----- question dialog -----------------------------------------------------

fn draw_question(f: &mut Frame, q: &PendingQuestion) {
    let screen = f.area();
    let width = screen.width.saturating_sub(6).clamp(30, 80);
    let inner_width = width.saturating_sub(2) as usize;

    let mut lines: Vec<Line> = Vec::new();
    push_wrapped(
        &mut lines,
        &q.question,
        inner_width.saturating_sub(2),
        |_, s| Line::from(Span::styled(format!(" {s}"), Style::new().bold())),
    );

    // Cap the dialog to the screen. The options and the hint always stay
    // visible: a long question (e.g. a submitted plan) is truncated first.
    let max_height = screen.height.saturating_sub(4).max(8) as usize;
    let content_budget = max_height.saturating_sub(2); // borders
    let q_budget = content_budget
        .saturating_sub(q.options.len().min(6) + 2) // options + blank + hint
        .max(1);
    if lines.len() > q_budget {
        let hidden = lines.len() + 1 - q_budget;
        lines.truncate(q_budget.saturating_sub(1));
        lines.push(hint_line(&format!(" … (+{hidden} more lines)")));
    }
    lines.push(Line::default());

    // Window the options around the selection.
    let budget = content_budget.saturating_sub(lines.len() + 1); // hint
    let visible = q.options.len().min(budget.max(1));
    let offset = (q.selected + 1).saturating_sub(visible);
    for (i, opt) in q.options.iter().enumerate().skip(offset).take(visible) {
        lines.push(list_line(
            format!(" {opt}"),
            i == q.selected,
            inner_width,
            Style::new(),
        ));
    }
    lines.push(hint_line(" ↑↓ select · Enter answer · Esc dismiss"));

    let area = dialog_area(screen, (30, 80), lines.len() as u16 + 2);
    render_dialog(f, &q.title, Color::Cyan, area, lines);
}

// ----- approval modal ------------------------------------------------------

/// One logical (pre-wrap) line of the approval dialog body, as styled spans.
struct BodyLine {
    spans: crate::highlight::SpanLine,
}

impl BodyLine {
    fn new(text: impl Into<String>, style: Style) -> Self {
        Self {
            spans: vec![(style, text.into())],
        }
    }

    fn plain(text: impl Into<String>) -> Self {
        Self::new(text, Style::new())
    }

    fn from_spans(spans: crate::highlight::SpanLine) -> Self {
        Self { spans }
    }
}

/// Render tool arguments as something a human can review at a glance:
/// bash as the command line, edit_file as a diff (all additions when
/// old_string is omitted, i.e. a file create/overwrite). Unknown tools fall
/// back to `key: value` lines instead of JSON.
fn approval_body(name: &str, args: &str) -> Vec<BodyLine> {
    let Ok(value) = serde_json::from_str::<serde_json::Value>(args) else {
        return vec![BodyLine::plain(args)];
    };
    let get = |k: &str| value.get(k).and_then(|v| v.as_str());

    if name == "bash"
        && let Some(cmd) = get("command")
    {
        return cmd
            .lines()
            .enumerate()
            .map(|(i, l)| BodyLine::plain(format!("{}{l}", if i == 0 { "$ " } else { "  " })))
            .collect();
    }
    if name == "edit_file"
        && let Some(path) = get("path")
    {
        let mut out = vec![BodyLine::new(format!("path: {path}"), Style::new().bold())];
        let old = get("old_string").unwrap_or_default();
        let new = get("new_string").unwrap_or_default();
        let diff = crate::highlight::diff_lines(old, new).join("\n");
        for (_, spans) in crate::highlight::diff_spans(&diff, path) {
            out.push(BodyLine::from_spans(spans));
        }
        return out;
    }

    // Generic: one `key: value` per line; string values verbatim (multiline
    // values continue indented), everything else as compact JSON.
    if let Some(map) = value.as_object() {
        let mut out = Vec::new();
        for (k, v) in map {
            let text = match v.as_str() {
                Some(s) => s.to_string(),
                None => v.to_string(),
            };
            let mut rest = text.lines();
            out.push(BodyLine::plain(format!(
                "{k}: {}",
                rest.next().unwrap_or_default()
            )));
            for l in rest {
                out.push(BodyLine::plain(format!("   {l}")));
            }
        }
        if out.is_empty() {
            out.push(BodyLine::plain("(no arguments)"));
        }
        return out;
    }
    vec![BodyLine::plain(value.to_string())]
}

fn draw_approval(f: &mut Frame, pending: &PendingApproval) {
    let screen = f.area();
    let width = screen.width.saturating_sub(6).clamp(20, 80);
    let inner_width = width.saturating_sub(4) as usize;

    // Wrap the body first so the dialog can size itself to the content.
    let mut body: Vec<Line> = Vec::new();
    for bl in approval_body(&pending.name, &pending.args) {
        for chunk in crate::highlight::wrap_spans(&bl.spans, inner_width) {
            let spans: Vec<Span> = chunk
                .into_iter()
                .map(|(st, s)| Span::styled(s, st))
                .collect();
            body.push(Line::from(spans));
        }
    }

    // Chrome rows: borders (2) + title + blank + blank + always-rule +
    // [y]/[a]/[n] = 7. The body budget accounts for all of them so the key
    // hints always fit.
    let max_height = screen.height.saturating_sub(4).clamp(8, 21);
    let height = (body.len() as u16 + 7).clamp(8, max_height);
    let budget = height.saturating_sub(7) as usize;
    if body.len() > budget {
        let hidden = body.len() + 1 - budget;
        body.truncate(budget.saturating_sub(1));
        body.push(Line::from(Span::styled(
            format!("… (+{hidden} more lines)"),
            Style::new().fg(Color::DarkGray),
        )));
    }

    let area = dialog_area(screen, (20, 80), height);

    let mut lines: Vec<Line> = vec![
        Line::from(Span::styled(
            format!("Tool: {}", pending.name),
            Style::new().fg(Color::Yellow).bold(),
        )),
        Line::default(),
    ];
    lines.extend(body);
    lines.push(Line::default());
    // What "always" adds; truncated to the dialog width (no wrapping here).
    let mut rule = pending.always.describe();
    if rule.chars().count() > inner_width {
        rule = rule.chars().take(inner_width.saturating_sub(1)).collect();
        rule.push('…');
    }
    lines.push(Line::from(Span::styled(
        rule,
        Style::new().fg(Color::DarkGray),
    )));
    lines.push(Line::from(vec![
        Span::styled("[y]", Style::new().fg(Color::Green).bold()),
        Span::raw(" approve   "),
        Span::styled("[a]", Style::new().fg(Color::Cyan).bold()),
        Span::raw(" always   "),
        Span::styled("[n]", Style::new().fg(Color::Red).bold()),
        Span::raw(" deny"),
    ]));

    render_dialog(f, "Tool approval", Color::Yellow, area, lines);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn texts(name: &str, args: &str) -> Vec<String> {
        approval_body(name, args)
            .into_iter()
            .map(|b| b.spans.iter().map(|(_, s)| s.as_str()).collect())
            .collect()
    }

    #[test]
    fn bash_args_render_as_a_command_line() {
        assert_eq!(
            texts("bash", r#"{"command":"cargo build"}"#),
            vec!["$ cargo build"]
        );
        assert_eq!(
            texts("bash", "{\"command\":\"first\\nsecond\"}"),
            vec!["$ first", "  second"]
        );
    }

    #[test]
    fn edit_file_renders_a_diff() {
        let args = r#"{"path":"src/a.rs","old_string":"old","new_string":"new1\nnew2"}"#;
        assert_eq!(
            texts("edit_file", args),
            vec!["path: src/a.rs", "- old", "+ new1", "+ new2"]
        );
    }

    #[test]
    fn edit_file_without_old_string_renders_all_additions() {
        let args = r#"{"path":"a.txt","new_string":"hello\nworld"}"#;
        assert_eq!(
            texts("edit_file", args),
            vec!["path: a.txt", "+ hello", "+ world"]
        );
    }

    #[test]
    fn generic_tools_render_key_value_lines() {
        assert_eq!(
            texts("web_fetch", r#"{"url":"https://example.com"}"#),
            vec!["url: https://example.com"]
        );
        assert_eq!(texts("mystery", r#"{"n":3}"#), vec!["n: 3"]);
        assert_eq!(texts("mystery", "{}"), vec!["(no arguments)"]);
    }

    #[test]
    fn invalid_json_falls_back_to_raw_text() {
        assert_eq!(texts("bash", "not json"), vec!["not json"]);
    }
}
