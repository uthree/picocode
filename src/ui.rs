//! ratatui rendering: transcript, input box, status bar, approval modal.

use ratatui::Frame;
use ratatui::layout::{Alignment, Constraint, Layout, Position, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Clear, Paragraph};
use unicode_width::UnicodeWidthChar;

use crate::app::{App, EntryKind, PendingApproval, PendingQuestion, SessionPicker};

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

    if app.pending.is_none() && app.session_picker.is_none() && app.question.is_none() {
        let matches = app.completions();
        if !matches.is_empty() {
            draw_completions(f, app, &matches, input);
        }
    }
    if let Some(picker) = &app.session_picker {
        draw_session_picker(f, picker);
    }
    if let Some(q) = &app.question {
        draw_question(f, q);
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
        if app.pending.is_none() && app.session_picker.is_none() && app.question.is_none() {
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
    if app.pending.is_none() && app.session_picker.is_none() && app.question.is_none() {
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
    let mode = app.mode();
    let mode_style = match mode {
        crate::config::Mode::ReadOnly => Style::new().fg(Color::Cyan),
        crate::config::Mode::Edit => Style::new().fg(Color::Yellow),
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
    if !app.follow {
        left.push(Span::styled(
            "  ⇡ scrolled (PgDn to bottom)",
            Style::new().fg(Color::Yellow),
        ));
    }
    let right = Line::from(vec![
        Span::styled(app.model_label.clone(), Style::new().fg(Color::Magenta)),
        Span::raw("  "),
        Span::styled(
            format!("ctx {} · out {}", app.ctx_tokens, app.out_tokens),
            Style::new().fg(Color::DarkGray),
        ),
        Span::raw(" "),
    ]);
    let right_width = (right.width() as u16).min(area.width);
    let [left_area, right_area] =
        Layout::horizontal([Constraint::Min(0), Constraint::Length(right_width)]).areas(area);
    f.render_widget(Paragraph::new(Line::from(left)), left_area);
    f.render_widget(
        Paragraph::new(right).alignment(Alignment::Right),
        right_area,
    );
}

// ----- session picker ------------------------------------------------------

fn draw_session_picker(f: &mut Frame, picker: &SessionPicker) {
    let screen = f.area();
    let width = screen.width.saturating_sub(6).clamp(30, 90);
    // rows + borders + hint line, capped to the screen.
    let height = (picker.sessions.len() as u16 + 3)
        .min(screen.height.saturating_sub(4))
        .max(5);
    let area = Rect {
        x: screen.x + (screen.width.saturating_sub(width)) / 2,
        y: screen.y + (screen.height.saturating_sub(height)) / 2,
        width,
        height,
    };

    let inner_width = width.saturating_sub(2) as usize;
    let visible = height.saturating_sub(3) as usize;
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
            crate::session::age(s.modified),
            s.messages,
            s.model,
        );
        let text: String = text.chars().take(inner_width).collect();
        lines.push(if i == picker.selected {
            Line::from(Span::styled(
                format!("{text:<inner_width$}"),
                Style::new().fg(Color::Black).bg(Color::Cyan),
            ))
        } else {
            Line::from(Span::raw(text))
        });
    }
    lines.push(Line::from(Span::styled(
        " ↑↓ select · Enter resume · Esc cancel",
        Style::new().fg(Color::DarkGray),
    )));

    let block = Block::bordered()
        .title(" Resume session ")
        .border_style(Style::new().fg(Color::Cyan));
    f.render_widget(Clear, area);
    f.render_widget(Paragraph::new(lines).block(block), area);
}

// ----- ask_user dialog -----------------------------------------------------

fn draw_question(f: &mut Frame, q: &PendingQuestion) {
    let screen = f.area();
    let width = screen.width.saturating_sub(6).clamp(30, 70);
    let inner_width = width.saturating_sub(2) as usize;

    let mut lines: Vec<Line> = Vec::new();
    push_wrapped(
        &mut lines,
        &q.question,
        inner_width.saturating_sub(2),
        |_, s| Line::from(Span::styled(format!(" {s}"), Style::new().bold())),
    );
    lines.push(Line::default());

    // Cap the dialog to the screen; window the options around the selection.
    let max_height = screen.height.saturating_sub(4).max(6) as usize;
    let budget = max_height
        .saturating_sub(2) // borders
        .saturating_sub(lines.len() + 1); // question + blank + hint
    let visible = q.options.len().min(budget.max(1));
    let offset = (q.selected + 1).saturating_sub(visible);
    for (i, opt) in q.options.iter().enumerate().skip(offset).take(visible) {
        let text: String = format!(" {opt}").chars().take(inner_width).collect();
        lines.push(if i == q.selected {
            Line::from(Span::styled(
                format!("{text:<inner_width$}"),
                Style::new().fg(Color::Black).bg(Color::Cyan),
            ))
        } else {
            Line::from(Span::raw(text))
        });
    }
    lines.push(Line::from(Span::styled(
        " ↑↓ select · Enter answer · Esc dismiss",
        Style::new().fg(Color::DarkGray),
    )));

    let height = (lines.len() as u16 + 2).min(max_height as u16);
    let area = Rect {
        x: screen.x + (screen.width.saturating_sub(width)) / 2,
        y: screen.y + (screen.height.saturating_sub(height)) / 2,
        width,
        height,
    };
    let block = Block::bordered()
        .title(" Question ")
        .border_style(Style::new().fg(Color::Cyan));
    f.render_widget(Clear, area);
    f.render_widget(Paragraph::new(lines).block(block), area);
}

// ----- approval modal ------------------------------------------------------

/// One logical (pre-wrap) line of the approval dialog body.
struct BodyLine {
    text: String,
    style: Style,
}

impl BodyLine {
    fn new(text: impl Into<String>, style: Style) -> Self {
        Self {
            text: text.into(),
            style,
        }
    }

    fn plain(text: impl Into<String>) -> Self {
        Self::new(text, Style::new())
    }
}

/// Render tool arguments as something a human can review at a glance:
/// bash as the command line, edit_file as a diff, write_file as path plus
/// content. Unknown tools fall back to `key: value` lines instead of JSON.
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
    if name == "write_file"
        && let Some(path) = get("path")
    {
        let mut out = vec![BodyLine::new(format!("path: {path}"), Style::new().bold())];
        if let Some(content) = get("content") {
            out.push(BodyLine::new(
                format!("content ({} lines):", content.lines().count().max(1)),
                Style::new().fg(Color::DarkGray),
            ));
            out.extend(
                content
                    .lines()
                    .map(|l| BodyLine::new(l, Style::new().fg(Color::Gray))),
            );
        }
        return out;
    }
    if name == "edit_file"
        && let Some(path) = get("path")
    {
        let mut out = vec![BodyLine::new(format!("path: {path}"), Style::new().bold())];
        for l in get("old_string").unwrap_or_default().lines() {
            out.push(BodyLine::new(format!("- {l}"), Style::new().fg(Color::Red)));
        }
        for l in get("new_string").unwrap_or_default().lines() {
            out.push(BodyLine::new(
                format!("+ {l}"),
                Style::new().fg(Color::Green),
            ));
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
        let style = bl.style;
        push_wrapped(&mut body, &bl.text, inner_width, |_, s| {
            Line::from(Span::styled(s, style))
        });
    }

    // Chrome rows: borders (2) + title + blank + blank + [y]/[n] = 6. The
    // body budget accounts for all of them so the key hints always fit.
    let max_height = screen.height.saturating_sub(4).clamp(7, 20);
    let height = (body.len() as u16 + 6).clamp(7, max_height);
    let budget = height.saturating_sub(6) as usize;
    if body.len() > budget {
        let hidden = body.len() + 1 - budget;
        body.truncate(budget.saturating_sub(1));
        body.push(Line::from(Span::styled(
            format!("… (+{hidden} more lines)"),
            Style::new().fg(Color::DarkGray),
        )));
    }

    let area = Rect {
        x: screen.x + (screen.width.saturating_sub(width)) / 2,
        y: screen.y + (screen.height.saturating_sub(height)) / 2,
        width,
        height,
    };

    let mut lines: Vec<Line> = vec![
        Line::from(Span::styled(
            format!("Tool: {}", pending.name),
            Style::new().fg(Color::Yellow).bold(),
        )),
        Line::default(),
    ];
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

#[cfg(test)]
mod tests {
    use super::*;

    fn texts(name: &str, args: &str) -> Vec<String> {
        approval_body(name, args)
            .into_iter()
            .map(|b| b.text)
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
    fn write_file_shows_path_and_content() {
        let args = r#"{"path":"a.txt","content":"hello\nworld"}"#;
        let t = texts("write_file", args);
        assert_eq!(t[0], "path: a.txt");
        assert_eq!(t[1], "content (2 lines):");
        assert_eq!(&t[2..], ["hello", "world"]);
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
