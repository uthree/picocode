//! Renderer-agnostic transcript entries.
//!
//! The transcript is the user-facing conversation log: what was said, which
//! tools ran, diffs, notices. Front ends decide how to draw each kind; the
//! core only produces and persists them (see [`crate::session`]).

use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum EntryKind {
    User,
    Assistant,
    Reasoning,
    Tool,
    ToolOut,
    /// A colored line diff shown for file-writing tool calls ("+ "/"- "/"  "
    /// prefixed lines).
    Diff,
    Notice,
    /// A prominent warning (e.g. entering bypass mode).
    Warning,
    /// The conversation summary produced by /compact.
    Summary,
    /// The colored context-usage breakdown pushed by /status: the text is
    /// an encoded [`crate::context::Breakdown`], rendered as a segmented
    /// bar + legend by both front ends.
    Context,
    Error,
    /// Rendered verbatim without wrapping (startup logo).
    Logo,
}

impl EntryKind {
    /// Short name of the kind, shown as the `[label]` in front of an entry
    /// in the raw transcript view. Untranslated on purpose: it names the
    /// transcript's own structure, the way a field name does.
    pub fn raw_label(self) -> &'static str {
        match self {
            EntryKind::User => "user",
            EntryKind::Assistant => "assistant",
            EntryKind::Reasoning => "reasoning",
            EntryKind::Tool => "tool",
            EntryKind::ToolOut => "tool-output",
            EntryKind::Diff => "diff",
            EntryKind::Notice => "notice",
            EntryKind::Warning => "warning",
            EntryKind::Summary => "summary",
            EntryKind::Context => "context",
            EntryKind::Error => "error",
            EntryKind::Logo => "logo",
        }
    }
}

#[derive(Clone, Serialize, Deserialize)]
pub struct Entry {
    pub kind: EntryKind,
    pub text: String,
    /// File name / language hint for syntax highlighting (Diff entries).
    #[serde(default)]
    pub lang: Option<String>,
    /// Paths of files attached to a User entry, for display (the media
    /// itself lives in the model-side history).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub attachments: Vec<String>,
}

/// Line diff between two strings, as "+ " / "- " / "  " prefixed lines —
/// the text form [`EntryKind::Diff`] entries carry. Front ends decide how
/// to color it.
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

/// One row of a rendered diff: the sign, and which side's line (by index)
/// carries the code, so front ends can pair rows with syntax-highlighted
/// sources.
pub enum DiffRow {
    /// A removed line: index into the old side.
    Old(usize),
    /// An added line: index into the new side.
    New(usize),
    /// An unchanged line: index into the new side (also present in old).
    Ctx(usize),
    /// Anything unprefixed (truncation markers and the like).
    Other(String),
}

/// Split [`diff_lines`]-style text back into rows plus the rebuilt old/new
/// sources. Each side comes back as one coherent snippet so highlighters
/// can parse multi-line constructs (strings, comments) correctly.
pub fn parse_diff(diff: &str) -> (Vec<DiffRow>, String, String) {
    let mut old_src: Vec<&str> = Vec::new();
    let mut new_src: Vec<&str> = Vec::new();
    let mut rows: Vec<DiffRow> = Vec::new();
    for line in diff.lines() {
        if let Some(code) = line.strip_prefix("- ") {
            rows.push(DiffRow::Old(old_src.len()));
            old_src.push(code);
        } else if let Some(code) = line.strip_prefix("+ ") {
            rows.push(DiffRow::New(new_src.len()));
            new_src.push(code);
        } else if let Some(code) = line.strip_prefix("  ") {
            rows.push(DiffRow::Ctx(new_src.len()));
            old_src.push(code);
            new_src.push(code);
        } else {
            rows.push(DiffRow::Other(line.to_string()));
        }
    }
    (rows, old_src.join("\n"), new_src.join("\n"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn diffs_lines_with_context() {
        let old = "a\nb\nc\n";
        let new = "a\nB\nc\n";
        assert_eq!(diff_lines(old, new), ["  a", "- b", "+ B", "  c"]);
    }

    #[test]
    fn parses_diff_rows_and_sides() {
        let diff = "  let x = 1;\n- let y = 2;\n+ let y = 3;\n… (+2 lines)";
        let (rows, old, new) = parse_diff(diff);
        assert_eq!(rows.len(), 4);
        assert!(matches!(rows[0], DiffRow::Ctx(0)));
        assert!(matches!(rows[1], DiffRow::Old(1)));
        assert!(matches!(rows[2], DiffRow::New(1)));
        assert!(matches!(rows[3], DiffRow::Other(_)));
        assert_eq!(old, "let x = 1;\nlet y = 2;");
        assert_eq!(new, "let x = 1;\nlet y = 3;");
    }

    /// The raw view labels entries by kind, so two kinds sharing a label
    /// would make the log ambiguous exactly where it is meant to be plain.
    #[test]
    fn every_kind_has_its_own_raw_label() {
        let kinds = [
            EntryKind::User,
            EntryKind::Assistant,
            EntryKind::Reasoning,
            EntryKind::Tool,
            EntryKind::ToolOut,
            EntryKind::Diff,
            EntryKind::Notice,
            EntryKind::Warning,
            EntryKind::Summary,
            EntryKind::Context,
            EntryKind::Error,
            EntryKind::Logo,
        ];
        let mut labels: Vec<&str> = kinds.iter().map(|k| k.raw_label()).collect();
        labels.sort_unstable();
        let mut unique = labels.clone();
        unique.dedup();
        assert_eq!(labels, unique);
    }

    #[test]
    fn entries_without_attachments_field_still_load() {
        // Session files written before attachments existed lack the field.
        let entry: Entry = serde_json::from_str(r#"{"kind":"User","text":"hi"}"#).unwrap();
        assert!(entry.attachments.is_empty());
        assert!(entry.lang.is_none());
    }
}
