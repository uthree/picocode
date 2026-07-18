use std::path::PathBuf;

use rig::tool::Tool;
use serde::Deserialize;
use serde_json::json;

use super::{ToolError, resolve};

const MAX_LINES: usize = 2000;
const MAX_LINE_LEN: usize = 500;

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
}

impl ReadFile {
    pub fn new(root: PathBuf) -> Self {
        Self { root }
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
        let path = resolve(&self.root, &args.path);
        let content = tokio::fs::read_to_string(&path)
            .await
            .map_err(|e| ToolError::new(format!("failed to read {}: {e}", path.display())))?;

        let offset = args.offset.unwrap_or(1).max(1);
        let limit = args.limit.unwrap_or(MAX_LINES).min(MAX_LINES);

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
            let line = if line.len() > MAX_LINE_LEN {
                let end = (0..=MAX_LINE_LEN).rev().find(|&j| line.is_char_boundary(j)).unwrap_or(0);
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

    #[tokio::test]
    async fn reads_with_line_numbers() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.txt"), "hello\nworld\n").unwrap();
        let tool = ReadFile::new(dir.path().to_path_buf());
        let out = tool
            .call(ReadArgs { path: "a.txt".into(), offset: None, limit: None })
            .await
            .unwrap();
        assert!(out.contains("1\thello"));
        assert!(out.contains("2\tworld"));
    }

    #[tokio::test]
    async fn missing_file_is_error() {
        let dir = tempfile::tempdir().unwrap();
        let tool = ReadFile::new(dir.path().to_path_buf());
        let err = tool
            .call(ReadArgs { path: "nope.txt".into(), offset: None, limit: None })
            .await
            .unwrap_err();
        assert!(err.0.contains("failed to read"));
    }
}
