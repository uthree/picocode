use std::path::PathBuf;
use std::process::Stdio;
use std::time::Duration;

use rig::tool::Tool;
use serde::Deserialize;
use serde_json::json;

use super::{ToolError, truncate_output};

const TIMEOUT: Duration = Duration::from_secs(120);
const MAX_OUTPUT_BYTES: usize = 20_000;

#[derive(Deserialize)]
pub struct BashArgs {
    pub command: String,
}

pub struct Bash {
    root: PathBuf,
}

impl Bash {
    pub fn new(root: PathBuf) -> Self {
        Self { root }
    }
}

impl Tool for Bash {
    const NAME: &'static str = "bash";
    type Error = ToolError;
    type Args = BashArgs;
    type Output = String;

    fn description(&self) -> String {
        "Run a shell command in the working directory and return its output. \
         Use for builds, tests, git, and anything the other tools don't cover. \
         Times out after 120 seconds."
            .to_string()
    }

    fn parameters(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "command": { "type": "string", "description": "Shell command to execute" }
            },
            "required": ["command"]
        })
    }

    async fn call(&self, args: Self::Args) -> Result<Self::Output, Self::Error> {
        let mut child = tokio::process::Command::new("sh")
            .arg("-c")
            .arg(&args.command)
            .current_dir(&self.root)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .map_err(|e| ToolError::new(format!("failed to spawn command: {e}")))?;

        let output = match tokio::time::timeout(TIMEOUT, async {
            let mut stdout = Vec::new();
            let mut stderr = Vec::new();
            use tokio::io::AsyncReadExt;
            let (mut o, mut e) = (child.stdout.take().unwrap(), child.stderr.take().unwrap());
            let (r1, r2, status) = tokio::join!(
                o.read_to_end(&mut stdout),
                e.read_to_end(&mut stderr),
                child.wait()
            );
            r1.and(r2)
                .map_err(|e| ToolError::new(format!("failed to read output: {e}")))?;
            let status = status.map_err(|e| ToolError::new(format!("wait failed: {e}")))?;
            Ok::<_, ToolError>((stdout, stderr, status))
        })
        .await
        {
            Ok(res) => res?,
            Err(_) => {
                return Err(ToolError::new(format!(
                    "command timed out after {}s",
                    TIMEOUT.as_secs()
                )));
            }
        };

        let (stdout, stderr, status) = output;
        let mut text = String::new();
        if !stdout.is_empty() {
            text.push_str(&String::from_utf8_lossy(&stdout));
        }
        if !stderr.is_empty() {
            if !text.is_empty() {
                text.push_str("\n--- stderr ---\n");
            }
            text.push_str(&String::from_utf8_lossy(&stderr));
        }
        let mut text = truncate_output(text.trim_end(), MAX_OUTPUT_BYTES);
        if text.is_empty() {
            text = "(no output)".to_string();
        }
        if !status.success() {
            text.push_str(&format!(
                "\n[exit code: {}]",
                status
                    .code()
                    .map_or("signal".to_string(), |c| c.to_string())
            ));
        }
        Ok(text)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn runs_command_and_captures_output() {
        let dir = tempfile::tempdir().unwrap();
        let tool = Bash::new(dir.path().to_path_buf());
        let out = tool
            .call(BashArgs {
                command: "echo hello && echo err >&2".into(),
            })
            .await
            .unwrap();
        assert!(out.contains("hello"));
        assert!(out.contains("err"));
    }

    #[tokio::test]
    async fn reports_exit_code() {
        let dir = tempfile::tempdir().unwrap();
        let tool = Bash::new(dir.path().to_path_buf());
        let out = tool
            .call(BashArgs {
                command: "exit 3".into(),
            })
            .await
            .unwrap();
        assert!(out.contains("[exit code: 3]"));
    }
}
