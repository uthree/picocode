use std::path::PathBuf;

use rig::tool::Tool;
use serde::Deserialize;
use serde_json::json;

use super::{ToolError, resolve};

#[derive(Deserialize)]
pub struct WriteArgs {
    path: String,
    content: String,
}

pub struct WriteFile {
    root: PathBuf,
}

impl WriteFile {
    pub fn new(root: PathBuf) -> Self {
        Self { root }
    }
}

impl Tool for WriteFile {
    const NAME: &'static str = "write_file";
    type Error = ToolError;
    type Args = WriteArgs;
    type Output = String;

    fn description(&self) -> String {
        "Create or overwrite a file with the given content. \
         Parent directories are created automatically."
            .to_string()
    }

    fn parameters(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "path": { "type": "string", "description": "File path to write" },
                "content": { "type": "string", "description": "Full file content" }
            },
            "required": ["path", "content"]
        })
    }

    async fn call(&self, args: Self::Args) -> Result<Self::Output, Self::Error> {
        let path = resolve(&self.root, &args.path);
        if let Some(parent) = path.parent() {
            tokio::fs::create_dir_all(parent)
                .await
                .map_err(|e| ToolError::new(format!("failed to create {}: {e}", parent.display())))?;
        }
        tokio::fs::write(&path, &args.content)
            .await
            .map_err(|e| ToolError::new(format!("failed to write {}: {e}", path.display())))?;
        Ok(format!(
            "Wrote {} bytes ({} lines) to {}",
            args.content.len(),
            args.content.lines().count(),
            path.display()
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn writes_and_creates_dirs() {
        let dir = tempfile::tempdir().unwrap();
        let tool = WriteFile::new(dir.path().to_path_buf());
        let out = tool
            .call(WriteArgs { path: "sub/dir/x.txt".into(), content: "hi\n".into() })
            .await
            .unwrap();
        assert!(out.contains("Wrote 3 bytes"));
        assert_eq!(std::fs::read_to_string(dir.path().join("sub/dir/x.txt")).unwrap(), "hi\n");
    }
}
