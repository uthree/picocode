use std::path::PathBuf;

use rig::tool::Tool;
use serde::Deserialize;
use serde_json::json;

use super::{ToolError, resolve};
use crate::config::NumHandle;

#[derive(Deserialize)]
pub struct ReadArgs {
    path: String,
    /// 1-based line number to start reading from.
    offset: Option<usize>,
    /// Maximum number of lines to read.
    limit: Option<usize>,
}

pub struct ReadFile {
    root: PathBuf,
    /// Output limits, shared with the `/config` dialog.
    max_lines: NumHandle,
    max_line_bytes: NumHandle,
}

impl ReadFile {
    pub fn new(root: PathBuf, max_lines: NumHandle, max_line_bytes: NumHandle) -> Self {
        Self {
            root,
            max_lines,
            max_line_bytes,
        }
    }
}

impl Tool for ReadFile {
    const NAME: &'static str = "read_file";
    type Error = ToolError;
    type Args = ReadArgs;
    type Output = String;

    fn description(&self) -> String {
        "Read a text file and return its contents with line numbers. \
         Use offset/limit for large files."
            .to_string()
    }

    fn parameters(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "path": { "type": "string", "description": "File path (relative to the working directory or absolute)" },
                "offset": { "type": "integer", "description": "1-based line number to start from (optional)" },
                "limit": { "type": "integer", "description": "Maximum number of lines to read (optional)" }
            },
            "required": ["path"]
        })
    }

    async fn call(&self, args: Self::Args) -> Result<Self::Output, Self::Error> {
        let path = resolve(&self.root, &args.path)?;
        let content = tokio::fs::read_to_string(&path)
            .await
            .map_err(|e| ToolError::new(format!("failed to read {}: {e}", path.display())))?;

        let max_lines = (self.max_lines.get().max(1)) as usize;
        let max_line_bytes = (self.max_line_bytes.get().max(1)) as usize;
        let offset = args.offset.unwrap_or(1).max(1);
        let limit = args.limit.unwrap_or(max_lines).min(max_lines);

        let mut out = String::new();
        let mut shown = 0usize;
        let total = content.lines().count();
        for (i, line) in content.lines().enumerate() {
            let lineno = i + 1;
            if lineno < offset {
                continue;
            }
            if shown >= limit {
                break;
            }
            let line = if line.len() > max_line_bytes {
                let end = (0..=max_line_bytes)
                    .rev()
                    .find(|&j| line.is_char_boundary(j))
                    .unwrap_or(0);
                format!("{}…", &line[..end])
            } else {
                line.to_string()
            };
            out.push_str(&format!("{lineno:>5}\t{line}\n"));
            shown += 1;
        }
        if offset + shown <= total {
            out.push_str(&format!(
                "... ({} more lines; re-read with offset={})\n",
                total - (offset - 1) - shown,
                offset + shown
            ));
        }
        if out.is_empty() {
            out = "(empty file)".to_string();
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tool(root: &std::path::Path) -> ReadFile {
        ReadFile::new(
            root.to_path_buf(),
            NumHandle::new(2000),
            NumHandle::new(500),
        )
    }

    #[tokio::test]
    async fn reads_with_line_numbers() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.txt"), "hello\nworld\n").unwrap();
        let out = tool(dir.path())
            .call(ReadArgs {
                path: "a.txt".into(),
                offset: None,
                limit: None,
            })
            .await
            .unwrap();
        assert!(out.contains("1\thello"));
        assert!(out.contains("2\tworld"));
    }

    #[tokio::test]
    async fn configured_limits_apply_at_call_time() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.txt"), "aaaaaaaaaa\nbb\ncc\n").unwrap();
        let tool = tool(dir.path());
        // Runtime changes through the shared handles apply to the next call.
        tool.max_lines.set(2);
        tool.max_line_bytes.set(4);
        let out = tool
            .call(ReadArgs {
                path: "a.txt".into(),
                offset: None,
                limit: None,
            })
            .await
            .unwrap();
        assert!(out.contains("1\taaaa…"), "{out}");
        assert!(out.contains("2\tbb"));
        assert!(!out.contains("cc"));
        assert!(out.contains("1 more lines; re-read with offset=3"));
    }

    #[tokio::test]
    async fn missing_file_is_error() {
        let dir = tempfile::tempdir().unwrap();
        let tool = tool(dir.path());
        let err = tool
            .call(ReadArgs {
                path: "nope.txt".into(),
                offset: None,
                limit: None,
            })
            .await
            .unwrap_err();
        assert!(err.0.contains("failed to read"));
    }
}
