//! Syntax highlighting for diff text, via gpui-component's tree-sitter
//! highlighter (the same engine that colors markdown code blocks, so diffs
//! match them and follow the light/dark theme).
//!
//! Results are cached process-wide: the virtualized transcript re-renders
//! visible rows every frame, and re-parsing the same diff each time would
//! be wasted work. Keys include the theme identity, so a theme switch
//! simply misses the cache and re-highlights.

use std::collections::HashMap;
use std::ops::Range;
use std::sync::{Arc, LazyLock, Mutex};

use gpui::HighlightStyle;
use gpui_component::highlighter::{HighlightTheme, Language, SyntaxHighlighter};
use ropey::Rope;

/// Highlight spans for one source line, byte ranges relative to the line.
pub type LineSpans = Vec<(Range<usize>, HighlightStyle)>;

static CACHE: LazyLock<Mutex<HashMap<CacheKey, Arc<Vec<LineSpans>>>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

/// (language, source, theme identity). Diffs are clipped to a few dozen
/// lines before they get here, so holding the sources is cheap.
type CacheKey = (String, String, usize);

/// Resolve a fence tag or file path ("rust", "src/main.rs") to a language
/// name the highlighter knows; plain text when nothing matches. Unknown
/// tokens map to `Language::Plain`, so anything that doesn't is a real
/// language name or alias ("rs", "py") — otherwise try the file extension.
pub fn resolve_lang(token: &str) -> &str {
    if Language::from_str(token) != Language::Plain {
        return token;
    }
    std::path::Path::new(token)
        .extension()
        .and_then(|e| e.to_str())
        .filter(|ext| Language::from_str(ext) != Language::Plain)
        .unwrap_or("text")
}

/// Highlight `src` as `token`'s language and split the result per line.
/// The whole source is parsed at once so multi-line constructs (strings,
/// comments) color correctly, which is why diff sides are rebuilt and
/// highlighted separately by the caller.
pub fn highlight_lines(src: &str, token: &str, theme: &Arc<HighlightTheme>) -> Arc<Vec<LineSpans>> {
    let lang = resolve_lang(token);
    let key: CacheKey = (
        lang.to_string(),
        src.to_string(),
        Arc::as_ptr(theme) as usize,
    );
    if let Some(hit) = CACHE.lock().unwrap().get(&key) {
        return hit.clone();
    }

    let mut highlighter = SyntaxHighlighter::new(lang);
    highlighter.update(None, &Rope::from_str(src));
    let styles = highlighter.styles(&(0..src.len()), theme);

    let mut lines = Vec::new();
    let mut line_start = 0usize;
    for line in src.split('\n') {
        let line_end = line_start + line.len();
        let mut spans: LineSpans = Vec::new();
        for (range, style) in &styles {
            let start = range.start.max(line_start);
            let end = range.end.min(line_end);
            if start < end {
                spans.push((start - line_start..end - line_start, *style));
            }
        }
        lines.push(spans);
        line_start = line_end + 1;
    }

    let lines = Arc::new(lines);
    CACHE.lock().unwrap().insert(key, lines.clone());
    lines
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolves_tags_paths_and_unknowns() {
        assert_eq!(resolve_lang("rust"), "rust");
        assert_eq!(resolve_lang("src/main.rs"), "rs");
        assert_eq!(resolve_lang("no-such-language"), "text");
        assert_eq!(resolve_lang("notes.unknownext"), "text");
    }

    #[test]
    fn highlights_per_line_and_caches() {
        let theme = HighlightTheme::default_dark();
        let src = "fn main() {\n    let x = 1;\n}";
        let lines = highlight_lines(src, "rust", &theme);
        assert_eq!(lines.len(), 3);
        // The `fn` keyword gets a color.
        assert!(
            lines[0]
                .iter()
                .any(|(r, st)| *r == (0..2) && st.color.is_some())
        );
        // Spans stay within their line.
        for (line, spans) in src.split('\n').zip(lines.iter()) {
            assert!(spans.iter().all(|(r, _)| r.end <= line.len()));
        }
        // Second call is served from the cache (same allocation).
        let again = highlight_lines(src, "rust", &theme);
        assert!(Arc::ptr_eq(&lines, &again));
    }
}
