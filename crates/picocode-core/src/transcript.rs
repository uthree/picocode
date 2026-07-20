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
    Error,
    /// Rendered verbatim without wrapping (startup logo).
    Logo,
}

#[derive(Clone, Serialize, Deserialize)]
pub struct Entry {
    pub kind: EntryKind,
    pub text: String,
    /// File name / language hint for syntax highlighting (Diff entries).
    #[serde(default)]
    pub lang: Option<String>,
}
