use std::path::PathBuf;

use rig::tool::Tool;
use serde::Deserialize;
use serde_json::json;

use super::{ToolError, resolve};

const MAX_MATCHES: usize = 200;
const MAX_LINE_LEN: usize = 250;

#[derive(Deserialize)]
pub struct GrepArgs {
    /// Regular expression to search for.
    pattern: String,
    /// Directory or file to search in (defaults to the working directory).
    path: Option<String>,
}

pub struct Grep {
    root: PathBuf,
}

impl Grep {
    pub fn new(root: PathBuf) -> Self {
        Self { root }
    }
}

impl Tool for Grep {
    const NAME: &'static str = "grep";
    type Error = ToolError;
    type Args = GrepArgs;
    type Output = String;

    fn description(&self) -> String {
        "Search file contents with a regular expression (respects .gitignore). \
         Returns matching lines as `path:line: text`."
            .to_string()
    }

    fn parameters(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "pattern": { "type": "string", "description": "Rust-flavored regular expression" },
                "path": { "type": "string", "description": "File or directory to search (optional)" }
            },
            "required": ["pattern"]
        })
    }

    async fn call(&self, args: Self::Args) -> Result<Self::Output, Self::Error> {
        let re = regex::Regex::new(&args.pattern)
            .map_err(|e| ToolError::new(format!("invalid regex: {e}")))?;
        let base = match &args.path {
            Some(p) => resolve(&self.root, p),
            None => self.root.clone(),
        };

        let out = tokio::task::spawn_blocking(move || {
            let mut matches: Vec<String> = Vec::new();
            let mut truncated = false;
            'walk: for entry in ignore::WalkBuilder::new(&base)
                .hidden(true)
                .git_ignore(true)
                .require_git(false)
                .build()
                .flatten()
            {
                if !entry.file_type().is_some_and(|t| t.is_file()) {
                    continue;
                }
                let path = entry.path();
                let Ok(content) = std::fs::read_to_string(path) else {
                    continue; // skip binary / non-utf8 files
                };
                let rel = path.strip_prefix(&base).unwrap_or(path);
                for (i, line) in content.lines().enumerate() {
                    if re.is_match(line) {
                        let mut line = line.trim_end().to_string();
                        if line.len() > MAX_LINE_LEN {
                            let end = (0..=MAX_LINE_LEN)
                                .rev()
                                .find(|&j| line.is_char_boundary(j))
                                .unwrap_or(0);
                            line.truncate(end);
                            line.push('…');
                        }
                        matches.push(format!("{}:{}: {}", rel.display(), i + 1, line));
                        if matches.len() >= MAX_MATCHES {
                            truncated = true;
                            break 'walk;
                        }
                    }
                }
            }
            let mut out = matches.join("\n");
            if truncated {
                out.push_str(&format!("\n... (stopped at {MAX_MATCHES} matches; narrow the pattern)"));
            }
            if out.is_empty() {
                out = "(no matches)".to_string();
            }
            out
        })
        .await
        .map_err(|e| ToolError::new(format!("search failed: {e}")))?;

        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn finds_matches_with_line_numbers() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.rs"), "fn main() {}\nfn helper() {}\n").unwrap();
        let tool = Grep::new(dir.path().to_path_buf());
        let out = tool.call(GrepArgs { pattern: "fn \\w+".into(), path: None }).await.unwrap();
        assert!(out.contains("a.rs:1: fn main() {}"));
        assert!(out.contains("a.rs:2: fn helper() {}"));
    }

    #[tokio::test]
    async fn invalid_regex_is_error() {
        let dir = tempfile::tempdir().unwrap();
        let tool = Grep::new(dir.path().to_path_buf());
        let err = tool.call(GrepArgs { pattern: "(".into(), path: None }).await.unwrap_err();
        assert!(err.0.contains("invalid regex"));
    }
}
