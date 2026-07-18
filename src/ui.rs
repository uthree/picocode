//! ratatui rendering: transcript, input box, status bar, approval modal.

use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Position, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Clear, Paragraph};
use unicode_width::UnicodeWidthChar;

use crate::app::{App, EntryKind, PendingApproval};

const SPINNER: &[&str] = &["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

pub fn draw(f: &mut Frame, app: &mut App) {
    let [transcript, input, status] = Layout::vertical([
        Constraint::Min(1),
        Constraint::Length(3),
        Constraint::Length(1),
    ])
    .areas(f.area());

    draw_transcript(f, app, transcript);
    draw_input(f, app, input);
    draw_status(f, app, status);

    if app.pending.is_none() {
        let matches = app.completions();
        if !matches.is_empty() {
            draw_completions(f, app, &matches, input);
        }
    }
    if let Some(pending) = &app.pending {
        draw_approval(f, pending);
    }
}

// ----- command completion popup --------------------------------------------

fn draw_completions(f: &mut Frame, app: &App, matches: &[(&str, &str)], input_area: Rect) {
    const CMD_COL: usize = 8;
    let height = (matches.len() as u16 + 2).min(input_area.y);
    if height < 3 {
        return;
    }
    let inner_width = matches
        .iter()
        .map(|(_, desc)| CMD_COL + desc.len() + 3)
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
            let text = format!(" {cmd:<CMD_COL$} {desc}");
            if i == selected {
                Line::from(Span::styled(
                    text,
                    Style::new().fg(Color::Black).bg(Color::Cyan),
                ))
            } else {
                Line::from(vec![
                    Span::styled(format!(" {cmd:<CMD_COL$}"), Style::new().fg(Color::Cyan)),
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
            }
            EntryKind::Assistant => {
                lines.push(Line::default());
                push_wrapped(&mut lines, &entry.text, width, |_, s| Line::from(s));
            }
            EntryKind::Reasoning => {
                lines.push(Line::default());
                if app.show_reasoning {
                    push_wrapped(&mut lines, &entry.text, width.saturating_sub(2), |_, s| {
                        Line::from(Span::styled(
                            format!("  {s}"),
                            Style::new()
                                .fg(Color::DarkGray)
                                .add_modifier(Modifier::ITALIC),
                        ))
                    });
                } else {
                    // Collapsed: one dim line with a live-updating size.
                    lines.push(Line::from(Span::styled(
                        format!(
                            "∴ thinking… ({} lines · Ctrl+T)",
                            entry.text.lines().count()
                        ),
                        Style::new()
                            .fg(Color::DarkGray)
                            .add_modifier(Modifier::ITALIC),
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
            EntryKind::Notice => {
                push_wrapped(&mut lines, &entry.text, width.saturating_sub(2), |_, s| {
                    Line::from(Span::styled(format!("· {s}"), Style::new().fg(Color::Blue)))
                });
            }
            EntryKind::Summary => {
                lines.push(Line::default());
                push_wrapped(&mut lines, &entry.text, width.saturating_sub(2), |_, s| {
                    Line::from(Span::styled(format!("  {s}"), Style::new().fg(Color::Gray)))
                });
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
                        Style::new().fg(Color::Cyan),
                    )));
                }
                lines.push(Line::default());
            }
        }
    }
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
    let block = Block::bordered()
        .title(" picocode ")
        .border_style(Style::new().fg(Color::DarkGray));
    let inner_width = area.width.saturating_sub(2) as usize;

    if app.input.is_empty() {
        let hint = Paragraph::new(Span::styled(
            "Type a message (Enter to send · / commands · ! runs shell)",
            Style::new().fg(Color::DarkGray),
        ))
        .block(block);
        f.render_widget(hint, area);
        if app.pending.is_none() {
            f.set_cursor_position(Position::new(area.x + 1, area.y + 1));
        }
        return;
    }

    // Horizontal scroll: drop chars from the left until the cursor fits.
    let chars: Vec<char> = app.input.chars().collect();
    let mut start = 0usize;
    let cursor_width = |from: usize, to: usize| -> usize {
        chars[from..to].iter().map(|c| c.width().unwrap_or(0)).sum()
    };
    while cursor_width(start, app.cursor) >= inner_width.saturating_sub(1) {
        start += 1;
    }
    let visible: String = chars[start..].iter().collect();
    let cursor_x = cursor_width(start, app.cursor);

    f.render_widget(Paragraph::new(visible).block(block), area);
    if app.pending.is_none() {
        f.set_cursor_position(Position::new(area.x + 1 + cursor_x as u16, area.y + 1));
    }
}

// ----- status bar ----------------------------------------------------------

fn draw_status(f: &mut Frame, app: &App, area: Rect) {
    let indicator = if app.running > 0 {
        Span::styled(
            format!("{} running", SPINNER[app.spinner % SPINNER.len()]),
            Style::new().fg(Color::Green),
        )
    } else {
        Span::styled("● idle", Style::new().fg(Color::DarkGray))
    };
    let mut spans = vec![
        Span::raw(" "),
        indicator,
        Span::raw("  "),
        Span::styled(app.model_label.clone(), Style::new().fg(Color::Magenta)),
        Span::raw("  "),
        Span::styled(
            format!("ctx {} · out {}", app.ctx_tokens, app.out_tokens),
            Style::new().fg(Color::DarkGray),
        ),
        Span::raw("  "),
        Span::styled(
            "PgUp/PgDn scroll · Ctrl+T thinking",
            Style::new().fg(Color::DarkGray),
        ),
    ];
    if !app.follow {
        spans.push(Span::styled(
            "  ⇡ scrolled (PgDn to bottom)",
            Style::new().fg(Color::Yellow),
        ));
    }
    f.render_widget(Paragraph::new(Line::from(spans)), area);
}

// ----- approval modal ------------------------------------------------------

fn draw_approval(f: &mut Frame, pending: &PendingApproval) {
    let screen = f.area();
    let width = screen.width.saturating_sub(6).clamp(20, 72);
    let height = screen.height.saturating_sub(4).clamp(7, 14);
    let area = Rect {
        x: screen.x + (screen.width.saturating_sub(width)) / 2,
        y: screen.y + (screen.height.saturating_sub(height)) / 2,
        width,
        height,
    };

    let args_pretty = serde_json::from_str::<serde_json::Value>(&pending.args)
        .and_then(|v| serde_json::to_string_pretty(&v))
        .unwrap_or_else(|_| pending.args.clone());

    let inner_width = width.saturating_sub(4) as usize;
    let mut lines: Vec<Line> = vec![
        Line::from(Span::styled(
            format!("Tool: {}", pending.name),
            Style::new().fg(Color::Yellow).bold(),
        )),
        Line::default(),
    ];
    let body_budget = height.saturating_sub(5) as usize;
    let mut body: Vec<Line> = Vec::new();
    push_wrapped(&mut body, &args_pretty, inner_width, |_, s| {
        Line::from(Span::styled(s, Style::new().fg(Color::Gray)))
    });
    if body.len() > body_budget {
        body.truncate(body_budget.saturating_sub(1));
        body.push(Line::from(Span::styled(
            "…",
            Style::new().fg(Color::DarkGray),
        )));
    }
    lines.extend(body);
    lines.push(Line::default());
    lines.push(Line::from(vec![
        Span::styled("[y]", Style::new().fg(Color::Green).bold()),
        Span::raw(" approve   "),
        Span::styled("[n]", Style::new().fg(Color::Red).bold()),
        Span::raw(" deny"),
    ]));

    let block = Block::bordered()
        .title(" Tool approval ")
        .border_style(Style::new().fg(Color::Yellow));
    f.render_widget(Clear, area);
    f.render_widget(Paragraph::new(lines).block(block), area);
}
