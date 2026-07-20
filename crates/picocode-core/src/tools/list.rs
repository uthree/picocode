use std::path::PathBuf;

use rig::tool::Tool;
use serde::Deserialize;
use serde_json::json;

use super::{ToolError, resolve};

const MAX_ENTRIES: usize = 500;

#[derive(Deserialize)]
pub struct ListArgs {
    /// Directory to list (defaults to the working directory).
    path: Option<String>,
}

pub struct ListFiles {
    root: PathBuf,
}

impl ListFiles {
    pub fn new(root: PathBuf) -> Self {
        Self { root }
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
            Some(p) => resolve(&self.root, p)?,
            None => self.root.clone(),
        };
        if !base.is_dir() {
            return Err(ToolError::new(format!(
                "{} is not a directory",
                base.display()
            )));
        }

        let base_clone = base.clone();
        let entries = tokio::task::spawn_blocking(move || {
            let mut entries: Vec<String> = Vec::new();
            for entry in ignore::WalkBuilder::new(&base_clone)
                .hidden(true)
                .git_ignore(true)
                .require_git(false)
                .build()
                .flatten()
            {
                let path = entry.path();
                if path == base_clone {
                    continue;
                }
                let rel = path.strip_prefix(&base_clone).unwrap_or(path);
                let mut s = rel.display().to_string();
                if entry.file_type().is_some_and(|t| t.is_dir()) {
                    s.push('/');
                }
                entries.push(s);
            }
            entries.sort();
            entries
        })
        .await
        .map_err(|e| ToolError::new(format!("walk failed: {e}")))?;

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
        let tool = ListFiles::new(dir.path().to_path_buf());
        let out = tool.call(ListArgs { path: None }).await.unwrap();
        assert!(out.contains("src/main.rs"));
        assert!(!out.contains("ignored.txt"));
    }
}
