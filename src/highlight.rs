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

/// Style for one diff line produced by [`diff_lines`].
pub fn diff_style(line: &str) -> Style {
    if line.starts_with('+') {
        Style::new().fg(Color::Green)
    } else if line.starts_with('-') {
        Style::new().fg(Color::Red)
    } else {
        Style::new().fg(Color::DarkGray)
    }
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
        assert_eq!(diff_style("+ B"), Style::new().fg(Color::Green));
        assert_eq!(diff_style("- b"), Style::new().fg(Color::Red));
    }
}
