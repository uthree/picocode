//! TeX math in assistant replies, approximated as Unicode.
//!
//! Models answer math questions with TeX spans (`$...$`, `$$...$$`,
//! `\(...\)`, `\[...\]`). The markdown renderer has no math support, so
//! before rendering we convert the span contents to Unicode with
//! [`unicodeit`] (`x^2` → `x²`, `\alpha` → `α`, …). Anything unicodeit
//! can't express stays as raw TeX — a graceful degradation, not an error.
//!
//! Code is left untouched: fenced blocks are skipped line-wise, inline
//! backtick spans char-wise. Inline `$...$` follows the pandoc rule (no
//! space just inside the delimiters, closing `$` not followed by a digit)
//! so prices like "$5 and $10" survive.

/// A piece of an assistant reply: ordinary markdown, or one display-math
/// block to typeset with RaTeX (see [`crate::tex`]).
pub enum Segment {
    Markdown(String),
    /// Contents of a `$$...$$` / `\[...\]` block, delimiters stripped.
    Display(String),
}

/// Split markdown into ordinary segments and display-math blocks, leaving
/// code fences and inline code untouched (they stay in the markdown
/// segments). Inline math is not extracted — it renders as Unicode inside
/// the text flow.
pub fn split_display_math(text: &str) -> Vec<Segment> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut plain = String::new(); // non-fence text pending a math scan
    let mut in_fence = false;
    for line in text.split_inclusive('\n') {
        if line.trim_start().starts_with("```") {
            if !in_fence {
                split_chunk(&plain, &mut cur, &mut out);
                plain.clear();
            }
            in_fence = !in_fence;
            cur.push_str(line);
        } else if in_fence {
            cur.push_str(line);
        } else {
            plain.push_str(line);
        }
    }
    split_chunk(&plain, &mut cur, &mut out);
    if !cur.is_empty() {
        out.push(Segment::Markdown(cur));
    }
    out
}

/// Scan a fence-free chunk for display-math blocks, appending to `cur` /
/// flushing segments into `out`.
fn split_chunk(chunk: &str, cur: &mut String, out: &mut Vec<Segment>) {
    let chars: Vec<char> = chunk.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        match chars[i] {
            '`' => match chars[i + 1..].iter().position(|&c| c == '`') {
                Some(n) => {
                    cur.extend(&chars[i..=i + 1 + n]);
                    i += n + 2;
                }
                None => {
                    cur.extend(&chars[i..]);
                    break;
                }
            },
            '$' if chars.get(i + 1) == Some(&'$') => match find_str(&chars, i + 2, &['$', '$']) {
                Some(j) => {
                    flush_display(cur, out, &chars[i + 2..j]);
                    i = j + 2;
                }
                None => {
                    cur.push('$');
                    i += 1;
                }
            },
            '\\' if chars.get(i + 1) == Some(&'[') => match find_str(&chars, i + 2, &['\\', ']']) {
                Some(j) => {
                    flush_display(cur, out, &chars[i + 2..j]);
                    i = j + 2;
                }
                None => {
                    cur.push('\\');
                    i += 1;
                }
            },
            c => {
                cur.push(c);
                i += 1;
            }
        }
    }
}

fn flush_display(cur: &mut String, out: &mut Vec<Segment>, inner: &[char]) {
    let tex: String = inner.iter().collect();
    let tex = tex.trim().to_string();
    if tex.is_empty() {
        return;
    }
    if !cur.trim().is_empty() {
        out.push(Segment::Markdown(std::mem::take(cur)));
    } else {
        cur.clear();
    }
    out.push(Segment::Display(tex));
}

/// Unicode fallback for one display-math block RaTeX could not typeset.
pub fn display_fallback(tex: &str) -> String {
    let mut out = String::new();
    push_display(&mut out, &tex.chars().collect::<Vec<_>>());
    out
}

/// Replace TeX math spans in markdown `text` with Unicode approximations.
pub fn render_math(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut buf = String::new();
    let mut in_fence = false;
    for line in text.split_inclusive('\n') {
        if line.trim_start().starts_with("```") {
            if !in_fence {
                out.push_str(&convert_segment(&buf));
                buf.clear();
            }
            in_fence = !in_fence;
            out.push_str(line);
        } else if in_fence {
            out.push_str(line);
        } else {
            buf.push_str(line);
        }
    }
    out.push_str(&convert_segment(&buf));
    out
}

/// Convert the math spans of a fence-free markdown segment.
fn convert_segment(s: &str) -> String {
    let chars: Vec<char> = s.chars().collect();
    let mut out = String::with_capacity(s.len());
    let mut i = 0;
    while i < chars.len() {
        match chars[i] {
            // Inline code: copy verbatim through the closing backtick.
            '`' => match chars[i + 1..].iter().position(|&c| c == '`') {
                Some(n) => {
                    out.extend(&chars[i..=i + 1 + n]);
                    i += n + 2;
                }
                None => {
                    out.extend(&chars[i..]);
                    break;
                }
            },
            '$' if chars.get(i + 1) == Some(&'$') => {
                // Display math: $$...$$ (may span lines).
                match find_str(&chars, i + 2, &['$', '$']) {
                    Some(j) => {
                        push_display(&mut out, &chars[i + 2..j]);
                        i = j + 2;
                    }
                    None => {
                        out.push('$');
                        i += 1;
                    }
                }
            }
            '$' => match find_inline_dollar(&chars, i) {
                Some(j) => {
                    push_converted(&mut out, &chars[i + 1..j]);
                    i = j + 1;
                }
                None => {
                    out.push('$');
                    i += 1;
                }
            },
            '\\' if chars.get(i + 1) == Some(&'(') => match find_str(&chars, i + 2, &['\\', ')']) {
                Some(j) => {
                    push_converted(&mut out, &chars[i + 2..j]);
                    i = j + 2;
                }
                None => {
                    out.push('\\');
                    i += 1;
                }
            },
            '\\' if chars.get(i + 1) == Some(&'[') => match find_str(&chars, i + 2, &['\\', ']']) {
                Some(j) => {
                    push_display(&mut out, &chars[i + 2..j]);
                    i = j + 2;
                }
                None => {
                    out.push('\\');
                    i += 1;
                }
            },
            c => {
                out.push(c);
                i += 1;
            }
        }
    }
    out
}

/// First occurrence of `needle` at or after `from`.
fn find_str(chars: &[char], from: usize, needle: &[char]) -> Option<usize> {
    (from..chars.len().saturating_sub(needle.len() - 1))
        .find(|&j| &chars[j..j + needle.len()] == needle)
}

/// Closing `$` for an inline span opened at `open`: same line, non-empty
/// content with no space just inside the delimiters, and not followed by a
/// digit (pandoc's rule, so "$5 and $10" is not math).
fn find_inline_dollar(chars: &[char], open: usize) -> Option<usize> {
    let first = *chars.get(open + 1)?;
    if first.is_whitespace() || first == '$' {
        return None;
    }
    let mut j = open + 1;
    while j < chars.len() {
        match chars[j] {
            '\n' => return None,
            '$' => {
                if chars[j - 1].is_whitespace() {
                    return None;
                }
                if chars.get(j + 1).is_some_and(|c| c.is_ascii_digit()) {
                    return None;
                }
                return Some(j);
            }
            _ => j += 1,
        }
    }
    None
}

/// Convert one math span and append it, with markdown emphasis characters
/// escaped so leftover `*`/`_` can't italicize the surrounding text.
fn push_converted(out: &mut String, inner: &[char]) {
    let tex: String = inner.iter().collect();
    let unicode = unicodeit::replace(tex.trim());
    for c in unicode.chars() {
        if c == '*' || c == '_' {
            out.push('\\');
        }
        out.push(c);
    }
}

/// Display math becomes its own paragraph.
fn push_display(out: &mut String, inner: &[char]) {
    while out.ends_with(' ') || out.ends_with('\t') {
        out.pop();
    }
    if !out.is_empty() && !out.ends_with('\n') {
        out.push('\n');
    }
    out.push('\n');
    push_converted(out, inner);
    out.push_str("\n\n");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn split_extracts_display_blocks() {
        let segs = split_display_math("intro\n$$\\frac{a}{b}$$\noutro $x^2$ end");
        assert_eq!(segs.len(), 3);
        assert!(matches!(&segs[0], Segment::Markdown(s) if s.contains("intro")));
        assert!(matches!(&segs[1], Segment::Display(s) if s == "\\frac{a}{b}"));
        // Inline math stays in the markdown segment.
        assert!(matches!(&segs[2], Segment::Markdown(s) if s.contains("$x^2$")));

        let segs = split_display_math(r"a \[E=mc^2\] b");
        assert!(matches!(&segs[1], Segment::Display(s) if s == "E=mc^2"));
    }

    #[test]
    fn split_leaves_code_alone() {
        let text = "```\n$$not math$$\n```\n";
        let segs = split_display_math(text);
        assert_eq!(segs.len(), 1);
        assert!(matches!(&segs[0], Segment::Markdown(s) if s == text));

        let segs = split_display_math("`$$x$$` and $$y$$");
        assert!(matches!(&segs[0], Segment::Markdown(s) if s.contains("`$$x$$`")));
        assert!(matches!(&segs[1], Segment::Display(s) if s == "y"));
    }

    #[test]
    fn split_keeps_unclosed_display_literal() {
        let segs = split_display_math("open $$ never closes");
        assert_eq!(segs.len(), 1);
        assert!(matches!(&segs[0], Segment::Markdown(s) if s == "open $$ never closes"));
    }

    #[test]
    fn inline_dollar_math_is_converted() {
        assert_eq!(render_math("so $x^2 + y_1$ holds"), "so x² + y₁ holds");
        assert_eq!(render_math(r"angle $\alpha$ here"), "angle α here");
    }

    #[test]
    fn paren_and_bracket_delimiters_work() {
        assert_eq!(render_math(r"value \(x_1\) ok"), "value x₁ ok");
        let display = render_math(r"before \[E = mc^2\] after");
        assert!(display.contains("E = mc²"), "{display}");
        // Display math sits on its own paragraph.
        assert!(display.contains("\n\nE = mc²\n\n"), "{display}");
    }

    #[test]
    fn display_dollars_span_lines() {
        let out = render_math("sum:\n$$\n\\sum x_i\n$$\ndone");
        assert!(out.contains('∑'), "{out}");
        assert!(!out.contains('$'), "{out}");
    }

    #[test]
    fn currency_is_not_math() {
        let s = "it costs $5 and $10 today";
        assert_eq!(render_math(s), s);
        // A space just inside the delimiter also disqualifies the span.
        let spaced = "a $ b $ c";
        assert_eq!(render_math(spaced), spaced);
    }

    #[test]
    fn code_is_untouched() {
        let fenced = "```\nlet x = 2; // $x^2$\n```\n";
        assert_eq!(render_math(fenced), fenced);
        let inline = "run `echo $HOME$PATH` now";
        assert_eq!(render_math(inline), inline);
    }

    #[test]
    fn unknown_tex_survives_as_text() {
        // unicodeit can't express \frac — the span degrades to its TeX
        // source instead of vanishing.
        let out = render_math(r"$\frac{a}{b}$");
        assert!(out.contains("frac") || out.contains('/'), "{out}");
    }

    #[test]
    fn emphasis_chars_in_output_are_escaped() {
        // `_` that unicodeit cannot subscript must not italicize markdown.
        let out = render_math(r"$x_\mathrm{max}$");
        assert!(!out.contains("_m") || out.contains("\\_"), "{out}");
    }

    #[test]
    fn unclosed_spans_stay_literal() {
        assert_eq!(render_math("price is $5"), "price is $5");
        assert_eq!(render_math("open $$ never closes"), "open $$ never closes");
        assert_eq!(render_math(r"stray \( here"), r"stray \( here");
    }
}
