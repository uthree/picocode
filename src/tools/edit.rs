use std::path::PathBuf;

use rig::tool::Tool;
use serde::Deserialize;
use serde_json::json;

use super::{ToolError, resolve};

#[derive(Deserialize)]
pub struct EditArgs {
    path: String,
    old_string: String,
    new_string: String,
}

pub struct EditFile {
    root: PathBuf,
}

impl EditFile {
    pub fn new(root: PathBuf) -> Self {
        Self { root }
    }
}

impl Tool for EditFile {
    const NAME: &'static str = "edit_file";
    type Error = ToolError;
    type Args = EditArgs;
    type Output = String;

    fn description(&self) -> String {
        "Replace an exact string in a file. `old_string` must appear exactly once; \
         include surrounding lines to make it unique. Read the file first."
            .to_string()
    }

    fn parameters(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "path": { "type": "string", "description": "File path to edit" },
                "old_string": { "type": "string", "description": "Exact text to replace (must be unique in the file)" },
                "new_string": { "type": "string", "description": "Replacement text" }
            },
            "required": ["path", "old_string", "new_string"]
        })
    }

    async fn call(&self, args: Self::Args) -> Result<Self::Output, Self::Error> {
        let path = resolve(&self.root, &args.path);
        let content = tokio::fs::read_to_string(&path)
            .await
            .map_err(|e| ToolError::new(format!("failed to read {}: {e}", path.display())))?;

        if args.old_string == args.new_string {
            return Err(ToolError::new("old_string and new_string are identical"));
        }
        let count = content.matches(&args.old_string).count();
        match count {
            0 => Err(ToolError::new(
                "old_string was not found in the file. Re-read the file and copy the text exactly.",
            )),
            1 => {
                let updated = content.replacen(&args.old_string, &args.new_string, 1);
                tokio::fs::write(&path, &updated).await.map_err(|e| {
                    ToolError::new(format!("failed to write {}: {e}", path.display()))
                })?;
                Ok(format!(
                    "Edited {}: -{} +{} lines",
                    path.display(),
                    args.old_string.lines().count(),
                    args.new_string.lines().count()
                ))
            }
            n => Err(ToolError::new(format!(
                "old_string appears {n} times in the file. Add surrounding context to make it unique."
            ))),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn setup(content: &str) -> (tempfile::TempDir, EditFile) {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("f.txt"), content).unwrap();
        let tool = EditFile::new(dir.path().to_path_buf());
        (dir, tool)
    }

    #[tokio::test]
    async fn replaces_unique_match() {
        let (dir, tool) = setup("foo\nbar\nbaz\n");
        tool.call(EditArgs {
            path: "f.txt".into(),
            old_string: "bar".into(),
            new_string: "BAR".into(),
        })
        .await
        .unwrap();
        assert_eq!(
            std::fs::read_to_string(dir.path().join("f.txt")).unwrap(),
            "foo\nBAR\nbaz\n"
        );
    }

    #[tokio::test]
    async fn rejects_ambiguous_match() {
        let (_dir, tool) = setup("x\nx\n");
        let err = tool
            .call(EditArgs {
                path: "f.txt".into(),
                old_string: "x".into(),
                new_string: "y".into(),
            })
            .await
            .unwrap_err();
        assert!(err.0.contains("appears"));
    }

    #[tokio::test]
    async fn rejects_missing_match() {
        let (_dir, tool) = setup("abc\n");
        let err = tool
            .call(EditArgs {
                path: "f.txt".into(),
                old_string: "zzz".into(),
                new_string: "y".into(),
            })
            .await
            .unwrap_err();
        assert!(err.0.contains("not found"));
    }
}
