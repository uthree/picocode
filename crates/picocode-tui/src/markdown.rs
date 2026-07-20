//! Markdown rendering for assistant messages: headings, emphasis, lists,
//! quotes, tables, links, and syntax-highlighted code blocks (via
//! [`crate::highlight`]). The renderer walks pulldown-cmark events and
//! produces width-wrapped ratatui lines.

use pulldown_cmark::{CodeBlockKind, Event, HeadingLevel, Options, Parser, Tag, TagEnd};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use crate::highlight::{self, SpanLine};

pub fn render(text: &str, width: usize) -> Vec<Line<'static>> {
    let mut opts = Options::empty();
    opts.insert(Options::ENABLE_TABLES);
    opts.insert(Options::ENABLE_STRIKETHROUGH);
    opts.insert(Options::ENABLE_TASKLISTS);
    let mut r = Renderer::new(width.max(8));
    for ev in Parser::new_ext(text, opts) {
        r.event(ev);
    }
    r.finish()
}

#[derive(Default)]
struct Table {
    head: Vec<String>,
    rows: Vec<Vec<String>>,
    /// Cells of the row currently being parsed.
    cur: Vec<String>,
    /// Content of the cell currently being parsed.
    cell: String,
}

struct Renderer {
    width: usize,
    lines: Vec<Line<'static>>,
    /// Inline spans of the block currently being parsed.
    spans: SpanLine,
    /// Nested inline styles (emphasis, strong, heading, link, …).
    styles: Vec<Style>,
    /// Open lists: the next item number, or None for bullets.
    list_stack: Vec<Option<u64>>,
    /// Marker ("• ", "3. ") waiting for the first line of a list item.
    item_prefix: Option<String>,
    quote_depth: usize,
    /// Open code block: (language, buffered code).
    code: Option<(String, String)>,
    table: Option<Table>,
    /// Destination of the link/image currently open.
    link: Option<String>,
}

impl Renderer {
    fn new(width: usize) -> Self {
        Self {
            width,
            lines: Vec::new(),
            spans: Vec::new(),
            styles: Vec::new(),
            list_stack: Vec::new(),
            item_prefix: None,
            quote_depth: 0,
            code: None,
            table: None,
            link: None,
        }
    }

    fn finish(mut self) -> Vec<Line<'static>> {
        self.flush_spans();
        self.lines
    }

    fn cur_style(&self) -> Style {
        self.styles.last().copied().unwrap_or_default()
    }

    /// Separate blocks with one blank line (never at the very top).
    fn blank(&mut self) {
        if self.lines.last().is_some_and(|l| l.width() > 0) {
            self.lines.push(Line::default());
        }
    }

    fn text(&mut self, t: &str) {
        if let Some((_, buf)) = &mut self.code {
            buf.push_str(t);
        } else if let Some(table) = &mut self.table {
            table.cell.push_str(t);
        } else {
            let style = self.cur_style();
            self.spans.push((style, t.to_string()));
        }
    }

    fn event(&mut self, ev: Event) {
        match ev {
            Event::Start(tag) => self.start(tag),
            Event::End(tag) => self.end(tag),
            Event::Text(t) => self.text(&t),
            Event::Code(t) => {
                if self.code.is_some() || self.table.is_some() {
                    self.text(&t);
                } else {
                    self.spans
                        .push((self.cur_style().fg(Color::Cyan), t.to_string()));
                }
            }
            Event::SoftBreak => self.text(" "),
            Event::HardBreak => self.flush_spans(),
            Event::Rule => {
                self.flush_spans();
                self.blank();
                self.lines.push(Line::from(Span::styled(
                    "─".repeat(self.width.min(40)),
                    Style::new().fg(Color::DarkGray),
                )));
            }
            Event::TaskListMarker(done) => {
                let mark = if done { "[x] " } else { "[ ] " };
                self.spans
                    .push((Style::new().fg(Color::DarkGray), mark.to_string()));
            }
            Event::Html(t) | Event::InlineHtml(t) => self.text(&t),
            Event::FootnoteReference(t) => self.text(&format!("[^{t}]")),
            _ => {}
        }
    }

    fn start(&mut self, tag: Tag) {
        match tag {
            Tag::Paragraph => {
                self.flush_spans();
                // Keep list items tight: no blank inside an item.
                if self.item_prefix.is_none() && self.list_stack.is_empty() {
                    self.blank();
                }
            }
            Tag::Heading { level, .. } => {
                self.flush_spans();
                self.blank();
                let style = match level {
                    HeadingLevel::H1 | HeadingLevel::H2 => Style::new()
                        .fg(Color::Cyan)
                        .add_modifier(Modifier::BOLD | Modifier::UNDERLINED),
                    HeadingLevel::H3 => Style::new().fg(Color::Cyan).add_modifier(Modifier::BOLD),
                    _ => Style::new().add_modifier(Modifier::BOLD),
                };
                self.styles.push(style);
            }
            Tag::BlockQuote(_) => {
                self.flush_spans();
                self.blank();
                self.quote_depth += 1;
            }
            Tag::CodeBlock(kind) => {
                self.flush_spans();
                self.blank();
                let lang = match kind {
                    CodeBlockKind::Fenced(l) => l.to_string(),
                    CodeBlockKind::Indented => String::new(),
                };
                self.code = Some((lang, String::new()));
            }
            Tag::List(start) => {
                self.flush_spans();
                if self.list_stack.is_empty() {
                    self.blank();
                }
                self.list_stack.push(start);
            }
            Tag::Item => {
                self.flush_spans();
                let depth = self.list_stack.len().saturating_sub(1);
                let marker = match self.list_stack.last_mut() {
                    Some(Some(n)) => {
                        let m = format!("{n}. ");
                        *n += 1;
                        m
                    }
                    _ => "• ".to_string(),
                };
                self.item_prefix = Some(format!("{}{marker}", "  ".repeat(depth)));
            }
            Tag::Emphasis => self
                .styles
                .push(self.cur_style().add_modifier(Modifier::ITALIC)),
            Tag::Strong => self
                .styles
                .push(self.cur_style().add_modifier(Modifier::BOLD)),
            Tag::Strikethrough => self
                .styles
                .push(self.cur_style().add_modifier(Modifier::CROSSED_OUT)),
            Tag::Link { dest_url, .. } => {
                self.styles
                    .push(self.cur_style().add_modifier(Modifier::UNDERLINED));
                self.link = Some(dest_url.to_string());
            }
            Tag::Image { dest_url, .. } => {
                self.link = Some(dest_url.to_string());
            }
            Tag::Table(_) => {
                self.flush_spans();
                self.blank();
                self.table = Some(Table::default());
            }
            _ => {}
        }
    }

    fn end(&mut self, tag: TagEnd) {
        match tag {
            TagEnd::Paragraph => self.flush_spans(),
            TagEnd::Heading(_) => {
                self.flush_spans();
                self.styles.pop();
            }
            TagEnd::BlockQuote(_) => {
                self.flush_spans();
                self.quote_depth = self.quote_depth.saturating_sub(1);
            }
            TagEnd::CodeBlock => {
                if let Some((lang, code)) = self.code.take() {
                    self.push_code(&code, lang.trim());
                }
            }
            TagEnd::List(_) => {
                self.flush_spans();
                self.list_stack.pop();
            }
            TagEnd::Item => self.flush_spans(),
            TagEnd::Emphasis | TagEnd::Strong | TagEnd::Strikethrough => {
                self.styles.pop();
            }
            TagEnd::Link => {
                self.styles.pop();
                if let Some(url) = self.link.take() {
                    // Skip the parenthesis when the link text is the URL.
                    let text = self.spans.last().map(|(_, s)| s.as_str()).unwrap_or("");
                    if text.trim_end_matches('/') != url.trim_end_matches('/') {
                        self.spans
                            .push((Style::new().fg(Color::DarkGray), format!(" ({url})")));
                    }
                }
            }
            TagEnd::Image => {
                if let Some(url) = self.link.take() {
                    self.spans
                        .push((Style::new().fg(Color::DarkGray), format!(" [image: {url}]")));
                }
            }
            TagEnd::Table => {
                if let Some(t) = self.table.take() {
                    self.push_table(t);
                }
            }
            TagEnd::TableHead => {
                if let Some(t) = &mut self.table {
                    t.head = std::mem::take(&mut t.cur);
                }
            }
            TagEnd::TableRow => {
                if let Some(t) = &mut self.table {
                    let row = std::mem::take(&mut t.cur);
                    t.rows.push(row);
                }
            }
            TagEnd::TableCell => {
                if let Some(t) = &mut self.table {
                    let cell = std::mem::take(&mut t.cell);
                    t.cur.push(cell.trim().to_string());
                }
            }
            _ => {}
        }
    }

    /// Emit the buffered inline spans as wrapped lines, applying the quote
    /// prefix and the (hanging) list-item indent.
    fn flush_spans(&mut self) {
        if self.spans.is_empty() {
            return;
        }
        let quote = "▎ ".repeat(self.quote_depth);
        let prefix = self.item_prefix.take().unwrap_or_default();
        let hang = " ".repeat(prefix.width());
        let avail = self
            .width
            .saturating_sub(quote.width() + prefix.width())
            .max(4);
        let spans = std::mem::take(&mut self.spans);
        for (i, chunk) in highlight::wrap_spans(&spans, avail).into_iter().enumerate() {
            let mut line: Vec<Span> = Vec::new();
            if !quote.is_empty() {
                line.push(Span::styled(quote.clone(), Style::new().fg(Color::Green)));
            }
            let indent = if i == 0 { &prefix } else { &hang };
            if !indent.is_empty() {
                line.push(Span::raw(indent.clone()));
            }
            line.extend(chunk.into_iter().map(|(st, s)| Span::styled(s, st)));
            self.lines.push(Line::from(line));
        }
    }

    fn push_code(&mut self, code: &str, token: &str) {
        for span_line in highlight::highlight(code, token) {
            for chunk in highlight::wrap_spans(&span_line, self.width.saturating_sub(2)) {
                let mut spans = vec![Span::raw("  ")];
                spans.extend(chunk.into_iter().map(|(st, s)| Span::styled(s, st)));
                self.lines.push(Line::from(spans));
            }
        }
    }

    fn push_table(&mut self, t: Table) {
        let ncols = t
            .head
            .len()
            .max(t.rows.iter().map(Vec::len).max().unwrap_or(0));
        if ncols == 0 {
            return;
        }
        fn cell(row: &[String], i: usize) -> &str {
            row.get(i).map(String::as_str).unwrap_or("")
        }
        let mut widths: Vec<usize> = (0..ncols)
            .map(|i| {
                let mut w = cell(&t.head, i).width();
                for r in &t.rows {
                    w = w.max(cell(r, i).width());
                }
                w.max(1)
            })
            .collect();
        // Shrink the widest columns until the table fits the terminal.
        let avail = self.width.saturating_sub(3 * (ncols - 1)).max(ncols);
        while widths.iter().sum::<usize>() > avail {
            let widest = widths
                .iter()
                .enumerate()
                .max_by_key(|(_, w)| **w)
                .map(|(i, _)| i)
                .unwrap_or(0);
            if widths[widest] <= 3 {
                break;
            }
            widths[widest] -= 1;
        }

        let dim = Style::new().fg(Color::DarkGray);
        let row_line = |cells: &[String], style: Style| -> Line<'static> {
            let mut spans: Vec<Span> = Vec::new();
            for (i, w) in widths.iter().enumerate() {
                if i > 0 {
                    spans.push(Span::styled(" │ ", dim));
                }
                spans.push(Span::styled(fit(cell(cells, i), *w), style));
            }
            Line::from(spans)
        };
        if !t.head.is_empty() {
            self.lines
                .push(row_line(&t.head, Style::new().add_modifier(Modifier::BOLD)));
            let mut spans: Vec<Span> = Vec::new();
            for (i, w) in widths.iter().enumerate() {
                if i > 0 {
                    spans.push(Span::styled("─┼─", dim));
                }
                spans.push(Span::styled("─".repeat(*w), dim));
            }
            self.lines.push(Line::from(spans));
        }
        for r in &t.rows {
            self.lines.push(row_line(r, Style::new()));
        }
    }
}

/// Pad or truncate (with `…`) to exactly `w` display columns.
fn fit(s: &str, w: usize) -> String {
    if s.width() <= w {
        return format!("{s}{}", " ".repeat(w - s.width()));
    }
    let mut out = String::new();
    let mut used = 0;
    for c in s.chars() {
        let cw = c.width().unwrap_or(0);
        if used + cw > w.saturating_sub(1) {
            break;
        }
        out.push(c);
        used += cw;
    }
    out.push('…');
    used += 1;
    format!("{out}{}", " ".repeat(w - used))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn texts(lines: &[Line]) -> Vec<String> {
        lines
            .iter()
            .map(|l| l.spans.iter().map(|s| s.content.as_ref()).collect())
            .collect()
    }

    #[test]
    fn headings_and_inline_styles() {
        let lines = render("# Title\n\nplain **bold** `code`", 60);
        let t = texts(&lines);
        assert_eq!(t[0], "Title");
        assert!(
            lines[0].spans[0]
                .style
                .add_modifier
                .contains(Modifier::BOLD)
        );
        assert_eq!(t[1], "");
        assert_eq!(t[2], "plain bold code");
        let bold = &lines[2].spans[1];
        assert!(bold.style.add_modifier.contains(Modifier::BOLD));
        let code = lines[2].spans.last().unwrap();
        assert_eq!(code.style.fg, Some(Color::Cyan));
    }

    #[test]
    fn lists_render_markers_and_numbers() {
        let t = texts(&render("- a\n- b\n\n1. x\n2. y", 60));
        assert!(t.contains(&"• a".to_string()));
        assert!(t.contains(&"• b".to_string()));
        assert!(t.contains(&"1. x".to_string()));
        assert!(t.contains(&"2. y".to_string()));
    }

    #[test]
    fn tables_align_columns() {
        let t = texts(&render("|name|value|\n|-|-|\n|a|long text|\n|bb|c|", 60));
        assert!(t.contains(&"name │ value    ".to_string()));
        assert!(t.contains(&"─────┼──────────".to_string()));
        assert!(t.contains(&"a    │ long text".to_string()));
        assert!(t.contains(&"bb   │ c        ".to_string()));
    }

    #[test]
    fn code_blocks_stay_highlighted() {
        let lines = render("```rust\nfn main() {}\n```", 60);
        let t = texts(&lines);
        assert!(t.contains(&"  fn main() {}".to_string()));
        // More than the indent span means the line is styled by syntect.
        let code_line = lines
            .iter()
            .find(|l| {
                l.spans
                    .iter()
                    .map(|s| s.content.as_ref())
                    .collect::<String>()
                    .contains("fn main")
            })
            .unwrap();
        assert!(code_line.spans.len() > 2);
    }

    #[test]
    fn quotes_and_wrapping() {
        let t = texts(&render("> quoted words", 60));
        assert!(t.contains(&"▎ quoted words".to_string()));
        // A long paragraph wraps to the width.
        let wrapped = render(&"word ".repeat(30), 20);
        assert!(wrapped.len() > 3);
    }
}
