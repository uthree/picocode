//! Built-in tools exposed to the agent.

mod ask;
mod bash;
mod edit;
mod fetch;
mod grep;
mod list;
mod plan;
mod read;
mod search;
mod write;

pub use ask::AskUser;
pub use bash::{Bash, BashArgs};
pub use edit::EditFile;
pub use fetch::WebFetch;
pub use grep::Grep;
pub use list::ListFiles;
pub use plan::SubmitPlan;
pub use read::ReadFile;
pub use search::WebSearch;
pub use write::WriteFile;

use std::path::{Path, PathBuf};

use rig::tool::Tool as _;

/// Every built-in tool name; used to validate the `[approval]` config lists.
pub const ALL_TOOLS: &[&str] = &[
    ReadFile::NAME,
    ListFiles::NAME,
    Grep::NAME,
    WriteFile::NAME,
    EditFile::NAME,
    Bash::NAME,
    WebSearch::NAME,
    WebFetch::NAME,
    AskUser::NAME,
    SubmitPlan::NAME,
];

/// Tools that need approval by default: everything that changes state or
/// sends data off the machine. The rest (local reads, dialogs) always runs.
pub const DESTRUCTIVE_TOOLS: &[&str] = &[
    Bash::NAME,
    WriteFile::NAME,
    EditFile::NAME,
    WebSearch::NAME,
    WebFetch::NAME,
];

/// Tools that modify the local system; plan mode denies exactly these.
pub const MUTATING_TOOLS: &[&str] = &[Bash::NAME, WriteFile::NAME, EditFile::NAME];

/// Tools that only write project files (auto-approved in edit mode).
pub const WRITE_TOOLS: &[&str] = &[WriteFile::NAME, EditFile::NAME];

/// Common error type for all tools. The message is fed back to the model.
#[derive(Debug, thiserror::Error)]
#[error("{0}")]
pub struct ToolError(pub String);

impl ToolError {
    pub fn new(msg: impl Into<String>) -> Self {
        Self(msg.into())
    }
}

/// Resolve a (possibly relative) path against the tool root and confine it
/// to the root: `..` is applied lexically and the result must stay inside
/// the working directory. File tools can never touch anything outside the
/// project; the model is told to fall back to `bash` (which asks) instead.
pub(crate) fn resolve(root: &Path, path: &str) -> Result<PathBuf, ToolError> {
    let p = Path::new(path);
    let joined = if p.is_absolute() {
        p.to_path_buf()
    } else {
        root.join(p)
    };
    let mut out = PathBuf::new();
    for component in joined.components() {
        match component {
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir => {
                out.pop();
            }
            other => out.push(other),
        }
    }
    if out.starts_with(root) {
        Ok(out)
    } else {
        Err(ToolError::new(format!(
            "path `{path}` is outside the working directory ({}); file tools are \
             confined to the project — use bash for anything outside it",
            root.display()
        )))
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
    fn resolve_confines_paths_to_the_root() {
        let root = Path::new("/tmp/proj");
        assert_eq!(
            resolve(root, "src/main.rs").unwrap(),
            PathBuf::from("/tmp/proj/src/main.rs")
        );
        // Absolute paths are fine as long as they stay inside the root…
        assert_eq!(
            resolve(root, "/tmp/proj/a.txt").unwrap(),
            PathBuf::from("/tmp/proj/a.txt")
        );
        // …and `.`/`..` are applied lexically before the check.
        assert_eq!(
            resolve(root, "src/../a.txt").unwrap(),
            PathBuf::from("/tmp/proj/a.txt")
        );

        // Anything escaping the root is rejected.
        assert!(resolve(root, "/etc/hosts").is_err());
        assert!(resolve(root, "../secrets").is_err());
        assert!(resolve(root, "src/../../other").is_err());
        let err = resolve(root, "/etc/hosts").unwrap_err().to_string();
        assert!(err.contains("outside the working directory"));
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
