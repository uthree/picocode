//! Built-in tools exposed to the agent.

mod bash;
mod edit;
mod fetch;
mod grep;
mod list;
mod plan;
mod read;
mod search;

pub use bash::{Bash, BashArgs};
pub use edit::EditFile;
pub use fetch::WebFetch;
pub use grep::Grep;
pub use list::ListFiles;
pub use plan::SubmitPlan;
pub use read::ReadFile;
pub use search::WebSearch;

use std::path::{Path, PathBuf};

use rig::tool::Tool as _;

/// Every built-in tool name; used to validate the `[approval]` config lists.
pub const ALL_TOOLS: &[&str] = &[
    ReadFile::NAME,
    ListFiles::NAME,
    Grep::NAME,
    EditFile::NAME,
    Bash::NAME,
    WebSearch::NAME,
    WebFetch::NAME,
    SubmitPlan::NAME,
];

/// Tools that need approval by default: everything that changes state or
/// sends data off the machine. The rest (local reads, dialogs) always runs.
pub const DESTRUCTIVE_TOOLS: &[&str] =
    &[Bash::NAME, EditFile::NAME, WebSearch::NAME, WebFetch::NAME];

/// Tools that modify the local system; plan mode denies exactly these.
pub const MUTATING_TOOLS: &[&str] = &[Bash::NAME, EditFile::NAME];

/// Tools that only write project files (auto-approved in edit mode).
pub const WRITE_TOOLS: &[&str] = &[EditFile::NAME];

/// Tools whose registration can be turned off entirely via `disable_tools`
/// in the config file (their schemas are then never sent to the model).
/// Restricted to the web tools: everything else is part of the core loop.
pub const OPTIONAL_TOOLS: &[&str] = &[WebSearch::NAME, WebFetch::NAME];

/// Last-seen modification times of files read via `read_file`, checked by
/// `edit_file` before writing: an mtime that moved since the last read means
/// the file was changed externally (by the user, another process, or
/// `/undo`), and blindly applying the edit would clobber that change. Only
/// files with a recorded stamp are checked — the `old_string` exact-match
/// requirement guards unread files on its own.
#[derive(Clone, Default)]
pub struct ReadStamps(
    std::sync::Arc<std::sync::Mutex<std::collections::HashMap<PathBuf, std::time::SystemTime>>>,
);

impl ReadStamps {
    /// Remember `path`'s current mtime (after a successful read or write).
    pub fn record(&self, path: &Path) {
        if let Ok(mtime) = std::fs::metadata(path).and_then(|m| m.modified()) {
            self.0.lock().unwrap().insert(path.to_path_buf(), mtime);
        }
    }

    /// Whether `path` changed on disk since it was last recorded. `false`
    /// when it was never recorded or no longer exists (other checks cover
    /// those).
    pub fn is_stale(&self, path: &Path) -> bool {
        let Some(seen) = self.0.lock().unwrap().get(path).copied() else {
            return false;
        };
        match std::fs::metadata(path).and_then(|m| m.modified()) {
            Ok(now) => now != seen,
            Err(_) => false,
        }
    }
}

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
