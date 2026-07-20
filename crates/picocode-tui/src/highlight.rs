//! Syntax highlighting (syntect) and line diffs (similar) for the TUI.
//!
//! Highlighted text is returned as per-line span lists `(Style, String)` so
//! the renderer can wrap them to the terminal width while keeping colors.

use std::sync::LazyLock;

use ratatui::style::{Color, Style};
use syntect::easy::HighlightLines;
use syntect::highlighting::{Theme, ThemeSet};
use syntect::parsing::SyntaxSet;
use unicode_width::UnicodeWidthChar;

static SYNTAXES: LazyLock<SyntaxSet> = LazyLock::new(SyntaxSet::load_defaults_newlines);
static THEME: LazyLock<Theme> = LazyLock::new(|| {
    ThemeSet::load_defaults()
        .themes
        .remove("base16-eighties.dark")
        .expect("syntect default themes include base16-eighties.dark")
});

/// One display line as styled fragments.
pub type SpanLine = Vec<(Style, String)>;

/// Highlight a code block. `token` is a fence language tag ("rust", "py", …)
/// or a file name/extension; unknown tokens fall back to plain text. Only
/// foreground colors are used so the terminal background shows through.
pub fn highlight(code: &str, token: &str) -> Vec<SpanLine> {
    let syntax = SYNTAXES
        .find_syntax_by_token(token)
        .or_else(|| {
            // Try as a file name: "src/main.rs" -> extension "rs".
            std::path::Path::new(token)
                .extension()
                .and_then(|e| e.to_str())
                .and_then(|e| SYNTAXES.find_syntax_by_extension(e))
        })
        .unwrap_or_else(|| SYNTAXES.find_syntax_plain_text());
    let mut hl = HighlightLines::new(syntax, &THEME);

    code.lines()
        .map(|line| {
            match hl.highlight_line(line, &SYNTAXES) {
                Ok(regions) => regions
                    .into_iter()
                    .filter(|(_, s)| !s.is_empty())
                    .map(|(style, s)| {
                        let fg = style.foreground;
                        (Style::new().fg(Color::Rgb(fg.r, fg.g, fg.b)), s.to_string())
                    })
                    .collect(),
                // A parse error on one line degrades to plain text.
                Err(_) => vec![(Style::new(), line.to_string())],
            }
        })
        .collect()
}

/// Wrap one styled line into chunks of at most `width` display columns,
/// splitting spans as needed. Always yields at least one (possibly empty)
/// chunk so blank code lines keep their vertical space.
pub fn wrap_spans(line: &SpanLine, width: usize) -> Vec<SpanLine> {
    let width = width.max(4);
    let mut out: Vec<SpanLine> = Vec::new();
    let mut cur: SpanLine = Vec::new();
    let mut cur_width = 0usize;
    for (style, text) in line {
        let mut piece = String::new();
        for c in text.chars() {
            let w = c.width().unwrap_or(0);
            if cur_width + w > width {
                if !piece.is_empty() {
                    cur.push((*style, std::mem::take(&mut piece)));
                }
                out.push(std::mem::take(&mut cur));
                cur_width = 0;
            }
            piece.push(c);
            cur_width += w;
        }
        if !piece.is_empty() {
            cur.push((*style, piece));
        }
    }
    out.push(cur);
    out
}

/// Line diff between `old` and `new`: each returned line starts with "- ",
/// "+ " or "  " (unchanged context).
pub fn diff_lines(old: &str, new: &str) -> Vec<String> {
    similar::TextDiff::from_lines(old, new)
        .iter_all_changes()
        .map(|change| {
            let prefix = match change.tag() {
                similar::ChangeTag::Delete => "- ",
                similar::ChangeTag::Insert => "+ ",
                similar::ChangeTag::Equal => "  ",
            };
            format!("{prefix}{}", change.value().trim_end_matches('\n'))
        })
        .collect()
}

/// Diff row backgrounds: the +/- marking lives in the background so the
/// foreground stays free for syntax highlighting.
const ADD_BG: Color = Color::Rgb(16, 60, 28);
const DEL_BG: Color = Color::Rgb(75, 24, 24);

/// Render [`diff_lines`]-style text ("+ "/"- "/"  " prefixes) with
/// syntax-highlighted foregrounds; insertions/removals are marked by the
/// background color only. Both sides of the diff are rebuilt and highlighted
/// separately so syntect's per-line state stays consistent. Returns the row
/// background (None for context) plus the styled spans per line.
pub fn diff_spans(diff: &str, token: &str) -> Vec<(Option<Color>, SpanLine)> {
    enum Row<'a> {
        Old(usize),
        New(usize),
        Ctx(usize),
        Other(&'a str),
    }
    let mut old_src: Vec<&str> = Vec::new();
    let mut new_src: Vec<&str> = Vec::new();
    let mut rows: Vec<Row> = Vec::new();
    for l in diff.lines() {
        if let Some(code) = l.strip_prefix("- ") {
            rows.push(Row::Old(old_src.len()));
            old_src.push(code);
        } else if let Some(code) = l.strip_prefix("+ ") {
            rows.push(Row::New(new_src.len()));
            new_src.push(code);
        } else if let Some(code) = l.strip_prefix("  ") {
            rows.push(Row::Ctx(new_src.len()));
            old_src.push(code);
            new_src.push(code);
        } else {
            // Truncation markers and the like.
            rows.push(Row::Other(l));
        }
    }
    let old_hl = highlight(&old_src.join("\n"), token);
    let new_hl = highlight(&new_src.join("\n"), token);
    let line = |hl: &[SpanLine], i: usize| hl.get(i).cloned().unwrap_or_default();
    let marked = |sign: &str, fg: Color, bg: Color, code: SpanLine| {
        let mut spans: SpanLine = vec![(Style::new().fg(fg).bg(bg), sign.to_string())];
        spans.extend(code.into_iter().map(|(st, s)| (st.bg(bg), s)));
        spans
    };

    rows.into_iter()
        .map(|row| match row {
            Row::Old(i) => (
                Some(DEL_BG),
                marked("- ", Color::Red, DEL_BG, line(&old_hl, i)),
            ),
            Row::New(i) => (
                Some(ADD_BG),
                marked("+ ", Color::Green, ADD_BG, line(&new_hl, i)),
            ),
            Row::Ctx(i) => {
                let mut spans: SpanLine = vec![(Style::new(), "  ".to_string())];
                spans.extend(line(&new_hl, i));
                (None, spans)
            }
            Row::Other(l) => (
                None,
                vec![(Style::new().fg(Color::DarkGray), l.to_string())],
            ),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn highlights_rust_line_by_line() {
        let out = highlight("fn main() {\n    let x = 1;\n}\n", "rust");
        assert_eq!(out.len(), 3);
        // The `fn` keyword gets a color distinct from plain text.
        let plain = highlight("fn main() {", "no-such-language");
        assert_ne!(out[0], plain[0]);
        let text: String = out[0].iter().map(|(_, s)| s.as_str()).collect();
        assert_eq!(text, "fn main() {");
    }

    #[test]
    fn wraps_styled_spans_at_width() {
        let line: SpanLine = vec![
            (Style::new(), "abcdef".to_string()),
            (Style::new().fg(Color::Red), "ghij".to_string()),
        ];
        let chunks = wrap_spans(&line, 4);
        let texts: Vec<String> = chunks
            .iter()
            .map(|c| c.iter().map(|(_, s)| s.as_str()).collect())
            .collect();
        assert_eq!(texts, ["abcd", "efgh", "ij"]);
        // The split span keeps its style.
        assert_eq!(chunks[1][1].0, Style::new().fg(Color::Red));
        assert!(!wrap_spans(&Vec::new(), 4).is_empty());
    }

    #[test]
    fn diffs_lines_with_context() {
        let old = "a\nb\nc\n";
        let new = "a\nB\nc\n";
        assert_eq!(diff_lines(old, new), ["  a", "- b", "+ B", "  c"]);
    }

    #[test]
    fn diff_spans_mark_rows_by_background() {
        let diff = "  let x = 1;\n- let y = 2;\n+ let y = 3;\n… (+2 lines)";
        let rows = diff_spans(diff, "rust");
        assert_eq!(rows.len(), 4);
        assert_eq!(rows[0].0, None); // context: no background
        assert!(rows[1].0.is_some()); // deletion
        assert!(rows[2].0.is_some()); // insertion
        assert_eq!(rows[3].0, None); // truncation marker

        // The code keeps its syntax colors, tinted only by the background.
        let (bg, spans) = &rows[2];
        assert!(spans.iter().all(|(st, _)| st.bg == *bg));
        assert!(
            spans
                .iter()
                .any(|(st, _)| st.fg.is_some() && st.fg != Some(Color::Green))
        );
        let text: String = spans.iter().map(|(_, s)| s.as_str()).collect();
        assert_eq!(text, "+ let y = 3;");
    }
}
