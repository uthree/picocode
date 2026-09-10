use base64::Engine;
use rig::tool::Tool;
use serde::Deserialize;
use serde_json::json;

use super::{ReadStamps, ToolError, resolve_in};
use crate::attachment::{Attachment, AttachmentKind, looks_like_text};
use crate::backend::Workspace;
use crate::config::NumHandle;

/// Cap on image files returned through the tool (base64 inflates by 4/3;
/// bigger files should be attached by the user instead).
const IMAGE_MAX_BYTES: usize = 4 * 1024 * 1024;

#[derive(Deserialize)]
pub struct ReadArgs {
    path: String,
    /// 1-based line number to start reading from.
    offset: Option<usize>,
    /// Maximum number of lines to read.
    limit: Option<usize>,
}

pub struct ReadFile {
    ws: Workspace,
    /// Output limits, shared with the `/config` dialog.
    max_lines: NumHandle,
    max_line_bytes: NumHandle,
    /// File versions shared with this agent's edit_file tool.
    stamps: ReadStamps,
}

impl ReadFile {
    pub fn new(
        ws: Workspace,
        max_lines: NumHandle,
        max_line_bytes: NumHandle,
        stamps: ReadStamps,
    ) -> Self {
        Self {
            ws,
            max_lines,
            max_line_bytes,
            stamps,
        }
    }
}

impl Tool for ReadFile {
    const NAME: &'static str = "read_file";
    type Error = ToolError;
    type Args = ReadArgs;
    type Output = String;

    fn description(&self) -> String {
        "Read a file. Text files come back with line numbers (use \
         offset/limit for large ones); image files are returned as image \
         content if the provider supports viewing them."
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
        let guard = self.stamps.lock().await;
        let path = resolve_in(&self.ws, &args.path)?;
        let bytes = self
            .ws
            .backend
            .read(&path)
            .await
            .map_err(|e| ToolError::new(format!("failed to read {}: {e}", path.display())))?;
        // Stamp the read so edit_file can detect external changes after it.
        self.stamps.record(&self.ws.backend, &path).await;
        drop(guard);

        // Known media types don't go through the text pipeline.
        match Attachment::classify(&path).map(|a| a.kind) {
            Some(AttachmentKind::Image) => {
                if bytes.len() > IMAGE_MAX_BYTES {
                    return Err(ToolError::new(format!(
                        "{} is {} bytes — too large to return as an image (limit {} bytes); \
                         ask the user to attach a smaller version to a message",
                        path.display(),
                        bytes.len(),
                        IMAGE_MAX_BYTES
                    )));
                }
                let ext = path
                    .extension()
                    .and_then(|e| e.to_str())
                    .unwrap_or_default()
                    .to_ascii_lowercase();
                let mime = match ext.as_str() {
                    "jpg" | "jpeg" => "image/jpeg",
                    "gif" => "image/gif",
                    "webp" => "image/webp",
                    _ => "image/png",
                };
                // rig's `ToolResultContent::from_tool_output` turns this JSON
                // shape into a text part plus a real image part, which
                // providers that accept images in tool results (Anthropic)
                // see as the image itself. Providers that don't (Ollama —
                // its tool messages are text-only) get the response text.
                let note = format!(
                    "Read the image file {} ({mime}, {} bytes). The image content is \
                     included in this tool result; if you cannot see any image, this \
                     provider cannot show images from tools — ask the user to attach \
                     the file to a chat message instead.",
                    path.display(),
                    bytes.len()
                );
                return Ok(json!({
                    "response": note,
                    "parts": [{
                        "type": "image",
                        "data": base64::engine::general_purpose::STANDARD.encode(&bytes),
                        "mimeType": mime,
                    }],
                })
                .to_string());
            }
            Some(AttachmentKind::Audio | AttachmentKind::Pdf) => {
                return Err(ToolError::new(format!(
                    "{} is an audio/PDF file, which read_file can't return; ask the \
                     user to attach it to a chat message (📎 in the GUI, /attach in \
                     the TUI) if the provider supports it",
                    path.display()
                )));
            }
            _ => {}
        }
        if !looks_like_text(&bytes[..bytes.len().min(8 * 1024)]) {
            return Err(ToolError::new(format!(
                "{} is a binary file ({} bytes) and can't be shown as text",
                path.display(),
                bytes.len()
            )));
        }
        let content = String::from_utf8_lossy(&bytes);

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
            Workspace::local(root.to_path_buf()),
            NumHandle::new(2000),
            NumHandle::new(500),
            Default::default(),
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
    async fn image_files_return_tool_image_json() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("dot.png"), [0x89, b'P', b'N', b'G']).unwrap();
        let out = tool(dir.path())
            .call(ReadArgs {
                path: "dot.png".into(),
                offset: None,
                limit: None,
            })
            .await
            .unwrap();
        // The rig tool-output convention: {"response": ..., "parts": [image]}.
        let v: serde_json::Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["parts"][0]["type"], "image");
        assert_eq!(v["parts"][0]["mimeType"], "image/png");
        assert_eq!(v["parts"][0]["data"], "iVBORw==");
        assert!(v["response"].as_str().unwrap().contains("dot.png"));
    }

    #[tokio::test]
    async fn audio_and_binary_files_are_refused_helpfully() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("x.mp3"), [1u8, 2]).unwrap();
        std::fs::write(dir.path().join("x.bin"), [0u8, 1, 2, 3]).unwrap();
        let tool = tool(dir.path());
        let err = tool
            .call(ReadArgs {
                path: "x.mp3".into(),
                offset: None,
                limit: None,
            })
            .await
            .unwrap_err();
        assert!(err.0.contains("attach"), "{}", err.0);
        let err = tool
            .call(ReadArgs {
                path: "x.bin".into(),
                offset: None,
                limit: None,
            })
            .await
            .unwrap_err();
        assert!(err.0.contains("binary"), "{}", err.0);
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
