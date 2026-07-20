use std::path::PathBuf;

use rig::tool::Tool;
use serde::Deserialize;
use serde_json::json;

use super::{ToolError, resolve};

#[derive(Deserialize)]
pub struct EditArgs {
    path: String,
    #[serde(default)]
    old_string: Option<String>,
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
        "Edit or create a file. With `old_string`: replace it with `new_string`; \
         it must appear exactly once, so include surrounding lines to make it \
         unique, and read the file first. Without `old_string`: create the file \
         (or overwrite it entirely) with `new_string` as the full content; \
         parent directories are created automatically."
            .to_string()
    }

    fn parameters(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "path": { "type": "string", "description": "File path to edit or create" },
                "old_string": { "type": "string", "description": "Exact text to replace (must be unique in the file). Omit to create or overwrite the whole file." },
                "new_string": { "type": "string", "description": "Replacement text, or the full file content when old_string is omitted" }
            },
            "required": ["path", "new_string"]
        })
    }

    async fn call(&self, args: Self::Args) -> Result<Self::Output, Self::Error> {
        let path = resolve(&self.root, &args.path)?;

        // No old_string: whole-file create/overwrite (the former write_file).
        let old_string = match args.old_string {
            None => return write_whole_file(&path, &args.new_string).await,
            Some(s) if s.is_empty() => return write_whole_file(&path, &args.new_string).await,
            Some(s) => s,
        };

        let content = tokio::fs::read_to_string(&path)
            .await
            .map_err(|e| ToolError::new(format!("failed to read {}: {e}", path.display())))?;

        if old_string == args.new_string {
            return Err(ToolError::new("old_string and new_string are identical"));
        }
        let count = content.matches(&old_string).count();
        match count {
            0 => Err(ToolError::new(
                "old_string was not found in the file. Re-read the file and copy the text exactly.",
            )),
            1 => {
                let updated = content.replacen(&old_string, &args.new_string, 1);
                tokio::fs::write(&path, &updated).await.map_err(|e| {
                    ToolError::new(format!("failed to write {}: {e}", path.display()))
                })?;
                Ok(format!(
                    "Edited {}: -{} +{} lines",
                    path.display(),
                    old_string.lines().count(),
                    args.new_string.lines().count()
                ))
            }
            n => Err(ToolError::new(format!(
                "old_string appears {n} times in the file. Add surrounding context to make it unique."
            ))),
        }
    }
}

/// Create or overwrite `path` with `content`, creating parent directories.
async fn write_whole_file(path: &std::path::Path, content: &str) -> Result<String, ToolError> {
    if let Some(parent) = path.parent() {
        tokio::fs::create_dir_all(parent)
            .await
            .map_err(|e| ToolError::new(format!("failed to create {}: {e}", parent.display())))?;
    }
    tokio::fs::write(path, content)
        .await
        .map_err(|e| ToolError::new(format!("failed to write {}: {e}", path.display())))?;
    Ok(format!(
        "Wrote {} bytes ({} lines) to {}",
        content.len(),
        content.lines().count(),
        path.display()
    ))
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
            old_string: Some("bar".into()),
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
                old_string: Some("x".into()),
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
                old_string: Some("zzz".into()),
                new_string: "y".into(),
            })
            .await
            .unwrap_err();
        assert!(err.0.contains("not found"));
    }

    #[tokio::test]
    async fn omitted_old_string_creates_the_file() {
        let dir = tempfile::tempdir().unwrap();
        let tool = EditFile::new(dir.path().to_path_buf());
        let out = tool
            .call(EditArgs {
                path: "sub/dir/x.txt".into(),
                old_string: None,
                new_string: "hi\n".into(),
            })
            .await
            .unwrap();
        assert!(out.contains("Wrote 3 bytes"));
        assert_eq!(
            std::fs::read_to_string(dir.path().join("sub/dir/x.txt")).unwrap(),
            "hi\n"
        );

        // An empty old_string is treated the same as omitting it: overwrite.
        tool.call(EditArgs {
            path: "sub/dir/x.txt".into(),
            old_string: Some(String::new()),
            new_string: "bye\n".into(),
        })
        .await
        .unwrap();
        assert_eq!(
            std::fs::read_to_string(dir.path().join("sub/dir/x.txt")).unwrap(),
            "bye\n"
        );
    }
}
