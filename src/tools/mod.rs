//! Built-in tools exposed to the agent.

mod bash;
mod edit;
mod fetch;
mod grep;
mod list;
mod read;
mod search;
mod write;

pub use bash::{Bash, BashArgs};
pub use edit::EditFile;
pub use fetch::WebFetch;
pub use grep::Grep;
pub use list::ListFiles;
pub use read::ReadFile;
pub use search::WebSearch;
pub use write::WriteFile;

use std::path::{Path, PathBuf};

/// Tool names that require user approval before running.
pub const DESTRUCTIVE_TOOLS: &[&str] = &[Bash::NAME, WriteFile::NAME, EditFile::NAME];

/// Tools that only write files (auto-approved in edit mode).
pub const WRITE_TOOLS: &[&str] = &[WriteFile::NAME, EditFile::NAME];

use rig::tool::Tool as _;

/// Common error type for all tools. The message is fed back to the model.
#[derive(Debug, thiserror::Error)]
#[error("{0}")]
pub struct ToolError(pub String);

impl ToolError {
    pub fn new(msg: impl Into<String>) -> Self {
        Self(msg.into())
    }
}

/// Resolve a (possibly relative) path against the tool root.
pub(crate) fn resolve(root: &Path, path: &str) -> PathBuf {
    let p = Path::new(path);
    if p.is_absolute() {
        p.to_path_buf()
    } else {
        root.join(p)
    }
}

/// Truncate long tool output keeping the head and tail.
pub(crate) fn truncate_output(s: &str, max_bytes: usize) -> String {
    if s.len() <= max_bytes {
        return s.to_string();
    }
    let head_target = max_bytes / 2;
    let tail_target = max_bytes / 2;
    let head_end = (0..=head_target)
        .rev()
        .find(|&i| s.is_char_boundary(i))
        .unwrap_or(0);
    let tail_start = (s.len() - tail_target..s.len())
        .find(|&i| s.is_char_boundary(i))
        .unwrap_or(s.len());
    format!(
        "{}\n... [{} bytes truncated] ...\n{}",
        &s[..head_end],
        s.len() - head_end - (s.len() - tail_start),
        &s[tail_start..]
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_relative_and_absolute() {
        let root = Path::new("/tmp/proj");
        assert_eq!(
            resolve(root, "src/main.rs"),
            PathBuf::from("/tmp/proj/src/main.rs")
        );
        assert_eq!(resolve(root, "/etc/hosts"), PathBuf::from("/etc/hosts"));
    }

    #[test]
    fn truncate_keeps_head_and_tail() {
        let s = "a".repeat(100) + &"b".repeat(100);
        let t = truncate_output(&s, 40);
        assert!(t.starts_with("aaaa"));
        assert!(t.ends_with("bbbb"));
        assert!(t.contains("truncated"));
    }
}
