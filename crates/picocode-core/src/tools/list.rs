use std::collections::BTreeSet;
use std::path::Path;

use rig::tool::Tool;
use serde::Deserialize;
use serde_json::json;

use super::{ToolError, resolve};
use crate::backend::Workspace;

const MAX_ENTRIES: usize = 500;

#[derive(Deserialize)]
pub struct ListArgs {
    /// Directory to list (defaults to the working directory).
    path: Option<String>,
}

pub struct ListFiles {
    ws: Workspace,
}

impl ListFiles {
    pub fn new(ws: Workspace) -> Self {
        Self { ws }
    }
}

impl Tool for ListFiles {
    const NAME: &'static str = "list_files";
    type Error = ToolError;
    type Args = ListArgs;
    type Output = String;

    fn description(&self) -> String {
        "Recursively list files under a directory (respects .gitignore). \
         Use this to explore the project structure."
            .to_string()
    }

    fn parameters(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "path": { "type": "string", "description": "Directory to list (optional, defaults to the working directory)" }
            }
        })
    }

    async fn call(&self, args: Self::Args) -> Result<Self::Output, Self::Error> {
        let base = match &args.path {
            Some(p) => resolve(&self.ws.root, p)?,
            None => self.ws.root.clone(),
        };
        if !self.ws.backend.is_dir(&base).await {
            return Err(ToolError::new(format!(
                "{} is not a directory",
                base.display()
            )));
        }

        let files = self
            .ws
            .backend
            .walk_files(&base)
            .await
            .map_err(|e| ToolError::new(format!("walk failed: {e}")))?;
        // Files come back relative to `base`; add each file plus every
        // ancestor directory (marked with a trailing `/`), like the
        // gitignore-aware local walk did.
        let mut entries: BTreeSet<String> = BTreeSet::new();
        for file in &files {
            let mut ancestors = file.ancestors().skip(1);
            for dir in ancestors.by_ref() {
                if dir.as_os_str().is_empty() {
                    break;
                }
                entries.insert(format!("{}/", posix(dir)));
            }
            entries.insert(posix(file));
        }
        let entries: Vec<String> = entries.into_iter().collect();

        let total = entries.len();
        let mut out: String = entries
            .into_iter()
            .take(MAX_ENTRIES)
            .collect::<Vec<_>>()
            .join("\n");
        if total > MAX_ENTRIES {
            out.push_str(&format!("\n... ({} more entries)", total - MAX_ENTRIES));
        }
        if out.is_empty() {
            out = "(empty directory)".to_string();
        }
        Ok(out)
    }
}

/// Render a relative path with `/` separators on every platform. Directory
/// rows always ended in `/`, so a Windows listing used to mix the two
/// (`src/` next to `src\main.rs`, and `a\b/` within one row); the ssh
/// backend reports posix paths either way, so `/` is the one form that
/// makes local and remote listings agree.
fn posix(path: &Path) -> String {
    path.components()
        .map(|c| c.as_os_str().to_string_lossy())
        .collect::<Vec<_>>()
        .join("/")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn lists_files_respecting_gitignore() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("src")).unwrap();
        std::fs::write(dir.path().join("src/main.rs"), "").unwrap();
        std::fs::write(dir.path().join(".gitignore"), "ignored.txt\n").unwrap();
        std::fs::write(dir.path().join("ignored.txt"), "").unwrap();
        let tool = ListFiles::new(Workspace::local(dir.path().to_path_buf()));
        let out = tool.call(ListArgs { path: None }).await.unwrap();
        assert!(out.contains("src/main.rs"));
        assert!(out.contains("src/"));
        assert!(!out.contains("ignored.txt"));
        // Separators are posix on every platform, never mixed within a row.
        assert!(!out.contains('\\'), "{out}");
    }

    #[tokio::test]
    async fn nested_paths_use_one_separator_style() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("a/b/c")).unwrap();
        std::fs::write(dir.path().join("a/b/c/deep.rs"), "").unwrap();
        let tool = ListFiles::new(Workspace::local(dir.path().to_path_buf()));
        let out = tool.call(ListArgs { path: None }).await.unwrap();
        assert!(out.contains("a/b/c/deep.rs"), "{out}");
        assert!(out.contains("a/b/c/"), "{out}");
        assert!(!out.contains('\\'), "{out}");
    }
}
