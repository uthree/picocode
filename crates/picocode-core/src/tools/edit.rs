use std::path::Path;
use std::time::Duration;

use rig::tool::Tool;
use serde::Deserialize;
use serde_json::json;

use super::{ToolError, resolve, truncate_output};
use crate::backend::Workspace;

/// Hard cap on the configured `after_edit` command.
const AFTER_EDIT_TIMEOUT: Duration = Duration::from_secs(120);
/// Cap on the failure output appended to the tool result.
const AFTER_EDIT_MAX_OUTPUT: usize = 4_000;

#[derive(Deserialize)]
pub struct EditArgs {
    path: String,
    #[serde(default)]
    old_string: Option<String>,
    new_string: String,
}

pub struct EditFile {
    ws: Workspace,
    /// Shell command run after every successful write (`after_edit` in the
    /// config file); its verdict is appended to the tool result.
    after_edit: Option<String>,
    /// Per-turn journal of pre-edit file states, powering `/undo`.
    journal: crate::undo::UndoJournal,
    /// Read timestamps shared with read_file (stale-write detection).
    stamps: super::ReadStamps,
}

impl EditFile {
    pub fn new(
        ws: Workspace,
        after_edit: Option<String>,
        journal: crate::undo::UndoJournal,
        stamps: super::ReadStamps,
    ) -> Self {
        Self {
            ws,
            after_edit,
            journal,
            stamps,
        }
    }

    /// Refuse to write over external changes: the file moved since the
    /// model last read it (user edit, another process, or `/undo`), so the
    /// edit was decided against outdated contents.
    async fn check_stale(&self, path: &Path) -> Result<(), ToolError> {
        if self.stamps.is_stale(&self.ws.backend, path).await {
            return Err(ToolError::new(format!(
                "{} changed on disk after you last read it (edited externally). \
                 Re-read the file and redo the edit against the current contents.",
                path.display()
            )));
        }
        Ok(())
    }

    /// Run the configured check and append its verdict to the tool output.
    /// The model sees breakage immediately, without the system prompt having
    /// to ask it to verify (small models forget to).
    async fn append_after_edit(&self, out: &mut String) {
        let Some(command) = &self.after_edit else {
            return;
        };
        out.push_str(&run_after_edit(&self.ws, command).await);
    }
}

/// Execute the after-edit check; the returned text is appended to the
/// edit_file output. Success stays terse; failures carry the (truncated)
/// output so the model can act on it. Runs on the workspace's backend so
/// a remote edit is verified on the remote host. The after_edit hook is
/// user-configured, so it runs unsandboxed like `!`.
async fn run_after_edit(ws: &Workspace, command: &str) -> String {
    let cmd = match ws
        .backend
        .shell(command, &ws.root, &crate::sandbox::SandboxCtx::off())
    {
        Ok(cmd) => cmd,
        Err(e) => return format!("\n\nafter_edit check (`{command}`) could not run: {e:#}"),
    };
    let mut cmd = cmd;
    let run = cmd.stdin(std::process::Stdio::null()).output();
    match tokio::time::timeout(AFTER_EDIT_TIMEOUT, run).await {
        Err(_) => format!(
            "\n\nafter_edit check (`{command}`) timed out after {}s.",
            AFTER_EDIT_TIMEOUT.as_secs()
        ),
        Ok(Err(e)) => format!("\n\nafter_edit check (`{command}`) could not run: {e}"),
        Ok(Ok(out)) if out.status.success() => {
            format!("\n\nafter_edit check (`{command}`): passed.")
        }
        Ok(Ok(out)) => {
            let mut text = String::from_utf8_lossy(&out.stdout).into_owned();
            text.push_str(&String::from_utf8_lossy(&out.stderr));
            format!(
                "\n\nafter_edit check (`{command}`) failed (exit {}):\n{}",
                out.status
                    .code()
                    .map_or_else(|| "?".to_string(), |c| c.to_string()),
                truncate_output(text.trim(), AFTER_EDIT_MAX_OUTPUT)
            )
        }
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
        let path = resolve(&self.ws.root, &args.path)?;
        let backend = &self.ws.backend;

        // No (or empty) old_string: whole-file create/overwrite (the former
        // write_file).
        let Some(old_string) = args.old_string.filter(|s| !s.is_empty()) else {
            self.check_stale(&path).await?;
            self.journal.record(&path);
            let mut out = write_whole_file(backend, &path, &args.new_string).await?;
            self.stamps.record(backend, &path).await;
            self.append_after_edit(&mut out).await;
            return Ok(out);
        };

        let bytes = backend
            .read(&path)
            .await
            .map_err(|e| ToolError::new(format!("failed to read {}: {e}", path.display())))?;
        let content = String::from_utf8(bytes)
            .map_err(|_| ToolError::new(format!("{} is not a UTF-8 text file", path.display())))?;

        if old_string == args.new_string {
            return Err(ToolError::new("old_string and new_string are identical"));
        }
        let count = content.matches(&old_string).count();
        match count {
            // Small models loop on a bare "not found"; showing the actual
            // text of the closest region lets the next attempt copy it
            // verbatim instead of guessing again.
            0 => Err(ToolError::new(
                match closest_region(&content, &old_string) {
                    Some((first_line, snippet)) => format!(
                        "old_string was not found in the file. The closest text in the \
                     file starts at line {first_line}:\n{snippet}\nRe-read that part and \
                     copy it exactly (mind whitespace and punctuation)."
                    ),
                    None => "old_string was not found in the file. Re-read the file and \
                         copy the text exactly."
                        .to_string(),
                },
            )),
            1 => {
                self.check_stale(&path).await?;
                self.journal.record(&path);
                let updated = content.replacen(&old_string, &args.new_string, 1);
                backend
                    .write(&path, updated.as_bytes())
                    .await
                    .map_err(|e| {
                        ToolError::new(format!("failed to write {}: {e}", path.display()))
                    })?;
                self.stamps.record(backend, &path).await;
                let mut out = format!(
                    "Edited {}: -{} +{} lines",
                    path.display(),
                    old_string.lines().count(),
                    args.new_string.lines().count()
                );
                self.append_after_edit(&mut out).await;
                Ok(out)
            }
            n => Err(ToolError::new(format!(
                "old_string appears {n} times in the file (starting on lines {}). \
                 Add surrounding context to make it unique.",
                match_lines(&content, &old_string)
            ))),
        }
    }
}

/// 1-based line numbers where `needle` occurrences start, as "12, 45, 78"
/// (capped at the first 10).
fn match_lines(content: &str, needle: &str) -> String {
    let mut lines = Vec::new();
    let mut from = 0;
    while let Some(pos) = content[from..].find(needle) {
        let at = from + pos;
        lines.push((content[..at].matches('\n').count() + 1).to_string());
        from = at + needle.len().max(1);
        if lines.len() == 10 {
            lines.push("…".to_string());
            break;
        }
    }
    lines.join(", ")
}

/// The region of `content` most similar to `needle`: a sliding window of the
/// same line count, scored with a character diff. Returns the 1-based first
/// line and the region's text, or `None` when nothing is similar enough (or
/// the file is too large to scan).
fn closest_region(content: &str, needle: &str) -> Option<(usize, String)> {
    const MAX_LINES: usize = 5_000;
    const MAX_SNIPPET_BYTES: usize = 1_500;
    /// Similarity below this reads as "nothing like it" — no snippet.
    const MIN_RATIO: f32 = 0.5;

    let lines: Vec<&str> = content.lines().collect();
    let window = needle.lines().count().max(1);
    if lines.is_empty() || lines.len() > MAX_LINES || window > lines.len() {
        return None;
    }

    let mut best: Option<(usize, f32)> = None;
    for start in 0..=(lines.len() - window) {
        let candidate = lines[start..start + window].join("\n");
        let ratio = similar::TextDiff::from_chars(needle, &candidate).ratio();
        if best.is_none_or(|(_, r)| ratio > r) {
            best = Some((start, ratio));
        }
    }
    let (start, ratio) = best?;
    if ratio < MIN_RATIO {
        return None;
    }
    let mut snippet = lines[start..start + window].join("\n");
    if snippet.len() > MAX_SNIPPET_BYTES {
        let mut end = MAX_SNIPPET_BYTES;
        while end > 0 && !snippet.is_char_boundary(end) {
            end -= 1;
        }
        snippet.truncate(end);
        snippet.push('…');
    }
    Some((start + 1, snippet))
}

/// Create or overwrite `path` with `content`, creating parent directories.
async fn write_whole_file(
    backend: &crate::backend::Backend,
    path: &std::path::Path,
    content: &str,
) -> Result<String, ToolError> {
    if let Some(parent) = path.parent() {
        backend
            .create_dir_all(parent)
            .await
            .map_err(|e| ToolError::new(format!("failed to create {}: {e}", parent.display())))?;
    }
    backend
        .write(path, content.as_bytes())
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
        let tool = EditFile::new(
            Workspace::local(dir.path().to_path_buf()),
            None,
            Default::default(),
            Default::default(),
        );
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
    async fn rejects_ambiguous_match_with_line_numbers() {
        let (_dir, tool) = setup("x\ny\nx\n");
        let err = tool
            .call(EditArgs {
                path: "f.txt".into(),
                old_string: Some("x".into()),
                new_string: "y".into(),
            })
            .await
            .unwrap_err();
        assert!(err.0.contains("appears 2 times"));
        assert!(err.0.contains("lines 1, 3"));
    }

    #[tokio::test]
    async fn rejects_missing_match_with_closest_snippet() {
        // A near miss (wrong indentation) shows the actual text to copy.
        let (_dir, tool) = setup("fn main() {\n    println!(\"hi\");\n}\n");
        let err = tool
            .call(EditArgs {
                path: "f.txt".into(),
                old_string: Some("fn main() {\nprintln!(\"hi\");\n}".into()),
                new_string: "y".into(),
            })
            .await
            .unwrap_err();
        assert!(err.0.contains("not found"));
        assert!(err.0.contains("starts at line 1"), "{}", err.0);
        assert!(err.0.contains("    println!(\"hi\");"), "{}", err.0);

        // Nothing remotely similar: no snippet, just the plain error.
        let (_dir, tool) = setup("abc\n");
        let err = tool
            .call(EditArgs {
                path: "f.txt".into(),
                old_string: Some("zzzzzzzz".into()),
                new_string: "y".into(),
            })
            .await
            .unwrap_err();
        assert!(err.0.contains("not found"));
        assert!(!err.0.contains("closest"), "{}", err.0);
    }

    #[tokio::test]
    async fn after_edit_verdict_is_appended() {
        // Passing check: terse verdict.
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("f.txt"), "a\n").unwrap();
        let tool = EditFile::new(
            Workspace::local(dir.path().to_path_buf()),
            Some("true".into()),
            Default::default(),
            Default::default(),
        );
        let out = tool
            .call(EditArgs {
                path: "f.txt".into(),
                old_string: Some("a".into()),
                new_string: "b".into(),
            })
            .await
            .unwrap();
        assert!(out.contains("Edited"));
        assert!(out.contains("after_edit check (`true`): passed"), "{out}");

        // Failing check: exit code and output are included (also covers the
        // whole-file write path).
        let tool = EditFile::new(
            Workspace::local(dir.path().to_path_buf()),
            Some("echo broken; exit 3".into()),
            Default::default(),
            Default::default(),
        );
        let out = tool
            .call(EditArgs {
                path: "g.txt".into(),
                old_string: None,
                new_string: "x\n".into(),
            })
            .await
            .unwrap();
        assert!(out.contains("failed (exit 3)"), "{out}");
        assert!(out.contains("broken"), "{out}");

        // A failed edit runs no check.
        let tool = EditFile::new(
            Workspace::local(dir.path().to_path_buf()),
            Some("true".into()),
            Default::default(),
            Default::default(),
        );
        let err = tool
            .call(EditArgs {
                path: "f.txt".into(),
                old_string: Some("zzzz-not-there".into()),
                new_string: "y".into(),
            })
            .await
            .unwrap_err();
        assert!(!err.0.contains("after_edit"), "{}", err.0);
    }

    #[test]
    fn stale_writes_are_refused_until_reread() {
        // Runs inside one #[test] via a manual runtime so the filetime
        // manipulation stays synchronous and explicit.
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("f.txt");
            std::fs::write(&path, "one\n").unwrap();
            let stamps = crate::tools::ReadStamps::default();
            let tool = EditFile::new(
                Workspace::local(dir.path().to_path_buf()),
                None,
                Default::default(),
                stamps.clone(),
            );

            // Simulate a read, then an external change (bump mtime).
            stamps.record(&crate::backend::Backend::Local, &path).await;
            let later = std::time::SystemTime::now() + std::time::Duration::from_secs(5);
            std::fs::File::options()
                .append(true)
                .open(&path)
                .unwrap()
                .set_times(std::fs::FileTimes::new().set_modified(later))
                .unwrap();

            let err = tool
                .call(EditArgs {
                    path: "f.txt".into(),
                    old_string: Some("one".into()),
                    new_string: "two".into(),
                })
                .await
                .unwrap_err();
            assert!(err.0.contains("changed on disk"), "{}", err.0);

            // Re-reading (re-recording) clears the staleness; consecutive
            // edits then keep working because writes refresh the stamp.
            stamps.record(&crate::backend::Backend::Local, &path).await;
            tool.call(EditArgs {
                path: "f.txt".into(),
                old_string: Some("one".into()),
                new_string: "two".into(),
            })
            .await
            .unwrap();
            tool.call(EditArgs {
                path: "f.txt".into(),
                old_string: Some("two".into()),
                new_string: "three".into(),
            })
            .await
            .unwrap();
            assert_eq!(std::fs::read_to_string(&path).unwrap(), "three\n");
        });
    }

    #[test]
    fn match_lines_caps_at_ten() {
        let content = "x\n".repeat(30);
        let out = match_lines(&content, "x");
        assert!(out.ends_with("…"));
        assert!(
            out.starts_with("1, 3,") || out.starts_with("1, 2,"),
            "{out}"
        );
    }

    #[tokio::test]
    async fn omitted_old_string_creates_the_file() {
        let dir = tempfile::tempdir().unwrap();
        let tool = EditFile::new(
            Workspace::local(dir.path().to_path_buf()),
            None,
            Default::default(),
            Default::default(),
        );
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
