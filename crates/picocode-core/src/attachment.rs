//! User-attached files (images, audio, PDFs) sent to the model as
//! multimodal message content.
//!
//! rig's provider conversions silently drop content a provider can't
//! express (e.g. audio on Ollama), so the front ends check
//! [`Attachment::supported_by`] up front and warn instead of letting an
//! attachment vanish. Media is base64-encoded here because the Ollama
//! conversion only forwards `DocumentSourceKind::Base64` images.

use std::path::{Path, PathBuf};

use base64::Engine;
use rig::message::{
    Audio, AudioMediaType, Document, DocumentMediaType, DocumentSourceKind, ImageMediaType,
    UserContent,
};

use crate::config::Provider;

/// Bytes sniffed from a file's head to decide whether it is text.
const SNIFF_BYTES: usize = 8 * 1024;
/// Cap on the inlined contents of a text attachment.
const TEXT_ATTACHMENT_MAX_BYTES: u64 = 64 * 1024;
/// Cap on a media attachment's file size. The bytes are base64-encoded
/// (~4/3 the size) and the result is held in memory alongside them, so an
/// oversized file is an out-of-memory rather than a slow request. Well
/// above any real screenshot or PDF.
const MEDIA_ATTACHMENT_MAX_BYTES: u64 = 32 * 1024 * 1024;

/// What kind of media a file is, judged by its extension (or, for `Text`,
/// its contents).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AttachmentKind {
    Image,
    Audio,
    Pdf,
    /// Anything that isn't a known media type but decodes as text
    /// (markdown, source code, …) — inlined into the message as text.
    Text,
}

impl AttachmentKind {
    pub fn label(self) -> &'static str {
        match self {
            AttachmentKind::Image => "image",
            AttachmentKind::Audio => "audio",
            AttachmentKind::Pdf => "PDF",
            AttachmentKind::Text => "text, contents inlined below",
        }
    }
}

/// Whether a sample from a file's head reads as text: no NUL bytes and
/// valid UTF-8 (allowing one multi-byte char cut off at the sample edge).
pub fn looks_like_text(sample: &[u8]) -> bool {
    if sample.contains(&0) {
        return false;
    }
    match std::str::from_utf8(sample) {
        Ok(_) => true,
        // `error_len() == None` means the bytes so far are a valid prefix
        // and only the tail was truncated mid-character.
        Err(e) => e.error_len().is_none() && e.valid_up_to() + 4 > sample.len(),
    }
}

/// A file staged for sending with the next prompt.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Attachment {
    pub path: PathBuf,
    pub kind: AttachmentKind,
}

impl Attachment {
    /// Classify a file as media by extension; `None` for anything else
    /// (see [`Attachment::detect`] for the content-sniffing variant that
    /// also accepts text files).
    pub fn classify(path: &Path) -> Option<Self> {
        let ext = path.extension()?.to_str()?.to_ascii_lowercase();
        let kind = match ext.as_str() {
            "jpg" | "jpeg" | "png" | "gif" | "webp" => AttachmentKind::Image,
            "wav" | "mp3" | "ogg" | "flac" | "m4a" | "aac" => AttachmentKind::Audio,
            "pdf" => AttachmentKind::Pdf,
            _ => return None,
        };
        Some(Self {
            path: path.to_path_buf(),
            kind,
        })
    }

    /// Classify by extension, then fall back to sniffing the file's head:
    /// non-media files that read as text (markdown, source code, …) become
    /// [`AttachmentKind::Text`] and are inlined into the message. `None`
    /// means unattachable (unknown binary format).
    pub fn detect(path: &Path) -> Option<Self> {
        if let Some(att) = Self::classify(path) {
            return Some(att);
        }
        let mut head = vec![0u8; SNIFF_BYTES];
        let n = {
            use std::io::Read;
            let mut f = std::fs::File::open(path).ok()?;
            f.read(&mut head).ok()?
        };
        head.truncate(n);
        looks_like_text(&head).then(|| Self {
            path: path.to_path_buf(),
            kind: AttachmentKind::Text,
        })
    }

    /// Whether rig's conversion for `provider` actually forwards this kind
    /// (anything else would be dropped or rejected mid-request). Text is
    /// plain message content, so every provider takes it.
    pub fn supported_by(&self, provider: Provider) -> bool {
        match provider {
            Provider::Ollama => {
                matches!(self.kind, AttachmentKind::Image | AttachmentKind::Text)
            }
            Provider::Anthropic => matches!(
                self.kind,
                AttachmentKind::Image | AttachmentKind::Pdf | AttachmentKind::Text
            ),
            Provider::Openai => true,
        }
    }

    /// File name for display (falls back to the full path).
    pub fn name(&self) -> String {
        self.path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| self.path.display().to_string())
    }

    /// Read the file and build the rig message content for it.
    pub fn to_user_content(&self) -> std::io::Result<UserContent> {
        use std::io::Read as _;

        let total = std::fs::metadata(&self.path)?.len();
        // Text attachments are inlined, not base64-encoded media. Only the
        // part that will actually be shown is read, so a huge log file never
        // enters memory just to have most of it thrown away.
        if self.kind == AttachmentKind::Text {
            let mut bytes = Vec::new();
            std::fs::File::open(&self.path)?
                .take(TEXT_ATTACHMENT_MAX_BYTES)
                .read_to_end(&mut bytes)?;
            let truncated = (bytes.len() as u64) < total;
            // The read can stop mid-character. `error_len() == None` is
            // exactly "the input ended inside a sequence", so trim there;
            // invalid bytes *within* the file are left to from_utf8_lossy.
            let end = match std::str::from_utf8(&bytes) {
                Ok(_) => bytes.len(),
                Err(e) if e.error_len().is_none() => e.valid_up_to(),
                Err(_) => bytes.len(),
            };
            let body = String::from_utf8_lossy(&bytes[..end]);
            let mut text = format!("Contents of the attached file {}:\n\n{body}", self.name());
            if truncated {
                text.push_str(&format!("\n… (truncated: {end} of {total} bytes shown)"));
            }
            return Ok(UserContent::text(text));
        }
        if total > MEDIA_ATTACHMENT_MAX_BYTES {
            return Err(std::io::Error::other(format!(
                "{} is {total} bytes; attachments are capped at {MEDIA_ATTACHMENT_MAX_BYTES}",
                self.name()
            )));
        }
        let bytes = std::fs::read(&self.path)?;
        let b64 = base64::engine::general_purpose::STANDARD.encode(&bytes);
        let ext = self
            .path
            .extension()
            .and_then(|e| e.to_str())
            .unwrap_or_default()
            .to_ascii_lowercase();
        Ok(match self.kind {
            AttachmentKind::Image => {
                let media_type = match ext.as_str() {
                    "jpg" | "jpeg" => ImageMediaType::JPEG,
                    "gif" => ImageMediaType::GIF,
                    "webp" => ImageMediaType::WEBP,
                    _ => ImageMediaType::PNG,
                };
                UserContent::image_base64(b64, Some(media_type), None)
            }
            AttachmentKind::Audio => {
                let media_type = match ext.as_str() {
                    "mp3" => AudioMediaType::MP3,
                    "ogg" => AudioMediaType::OGG,
                    "flac" => AudioMediaType::FLAC,
                    "m4a" => AudioMediaType::M4A,
                    "aac" => AudioMediaType::AAC,
                    _ => AudioMediaType::WAV,
                };
                UserContent::Audio(Audio {
                    data: DocumentSourceKind::Base64(b64),
                    media_type: Some(media_type),
                    additional_params: None,
                })
            }
            AttachmentKind::Pdf => UserContent::Document(Document {
                data: DocumentSourceKind::Base64(b64),
                media_type: Some(DocumentMediaType::PDF),
                additional_params: None,
            }),
            AttachmentKind::Text => unreachable!("handled by the early return above"),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rig::message::Image;

    #[test]
    fn classifies_by_extension() {
        let kind = |p: &str| Attachment::classify(Path::new(p)).map(|a| a.kind);
        assert_eq!(kind("shot.PNG"), Some(AttachmentKind::Image));
        assert_eq!(kind("dir/pic.jpeg"), Some(AttachmentKind::Image));
        assert_eq!(kind("voice.mp3"), Some(AttachmentKind::Audio));
        assert_eq!(kind("paper.pdf"), Some(AttachmentKind::Pdf));
        assert_eq!(kind("notes.txt"), None);
        assert_eq!(kind("no-extension"), None);
    }

    #[test]
    fn provider_support_matrix() {
        let a = |p: &str| Attachment::classify(Path::new(p)).unwrap();
        assert!(a("x.png").supported_by(Provider::Ollama));
        assert!(!a("x.mp3").supported_by(Provider::Ollama));
        assert!(!a("x.pdf").supported_by(Provider::Ollama));
        assert!(a("x.pdf").supported_by(Provider::Anthropic));
        assert!(!a("x.wav").supported_by(Provider::Anthropic));
        assert!(a("x.wav").supported_by(Provider::Openai));
    }

    #[test]
    fn builds_base64_image_content() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("dot.png");
        std::fs::write(&path, [0x89, b'P', b'N', b'G']).unwrap();
        let content = Attachment::classify(&path)
            .unwrap()
            .to_user_content()
            .unwrap();
        match content {
            UserContent::Image(Image {
                data: DocumentSourceKind::Base64(b64),
                media_type: Some(ImageMediaType::PNG),
                ..
            }) => assert_eq!(b64, "iVBORw=="),
            other => panic!("unexpected content: {other:?}"),
        }
    }

    #[test]
    fn detects_text_files_by_content() {
        let dir = tempfile::tempdir().unwrap();
        let md = dir.path().join("notes.md");
        std::fs::write(&md, "# Title\n日本語もOK\n").unwrap();
        assert_eq!(
            Attachment::detect(&md).map(|a| a.kind),
            Some(AttachmentKind::Text)
        );
        // Unknown binary is rejected …
        let bin = dir.path().join("blob.dat");
        std::fs::write(&bin, [0u8, 159, 146, 150]).unwrap();
        assert!(Attachment::detect(&bin).is_none());
        // … while media extensions never go through sniffing.
        let png = dir.path().join("x.png");
        std::fs::write(&png, [0x89, b'P']).unwrap();
        assert_eq!(
            Attachment::detect(&png).map(|a| a.kind),
            Some(AttachmentKind::Image)
        );
    }

    #[test]
    fn text_attachment_inlines_contents() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("main.rs");
        std::fs::write(&path, "fn main() {}\n").unwrap();
        let content = Attachment::detect(&path)
            .unwrap()
            .to_user_content()
            .unwrap();
        match content {
            UserContent::Text(t) => {
                assert!(t.text.contains("main.rs"));
                assert!(t.text.contains("fn main() {}"));
            }
            other => panic!("unexpected content: {other:?}"),
        }
        assert!(
            Attachment::detect(&path)
                .unwrap()
                .supported_by(Provider::Ollama)
        );
    }
}

#[cfg(test)]
mod size_tests {
    use super::*;

    fn text_of(content: &UserContent) -> String {
        match content {
            UserContent::Text(t) => t.text.clone(),
            other => panic!("expected text, got {other:?}"),
        }
    }

    #[test]
    fn a_large_text_file_is_truncated_without_being_read_whole() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("big.log");
        let total = (TEXT_ATTACHMENT_MAX_BYTES as usize) * 3;
        std::fs::write(&path, "a".repeat(total)).unwrap();

        let att = Attachment::detect(&path).unwrap();
        assert_eq!(att.kind, AttachmentKind::Text);
        let text = text_of(&att.to_user_content().unwrap());
        assert!(text.contains(&format!("of {total} bytes shown")), "{text}");
        // Only the cap made it in, plus the header and the notice.
        assert!(
            text.len() < TEXT_ATTACHMENT_MAX_BYTES as usize + 500,
            "{}",
            text.len()
        );
    }

    /// The cap can land inside a multi-byte character.
    #[test]
    fn truncation_stops_at_a_character_boundary() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("cjk.txt");
        // 3 bytes per character, so the 64 KiB cap cannot land on a boundary.
        let chars = (TEXT_ATTACHMENT_MAX_BYTES as usize / 3) + 100;
        std::fs::write(&path, "あ".repeat(chars)).unwrap();

        let att = Attachment::detect(&path).unwrap();
        let text = text_of(&att.to_user_content().unwrap());
        assert!(!text.contains('\u{fffd}'), "left a replacement character");
        assert!(text.contains("bytes shown"), "{text}");
    }

    #[test]
    fn an_oversized_media_file_is_refused_instead_of_buffered() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("huge.png");
        let att = Attachment {
            path: path.clone(),
            kind: AttachmentKind::Image,
        };
        // A sparse file: the point is the length check, not the bytes.
        let file = std::fs::File::create(&path).unwrap();
        file.set_len(MEDIA_ATTACHMENT_MAX_BYTES + 1).unwrap();
        drop(file);

        let err = att.to_user_content().unwrap_err();
        assert!(err.to_string().contains("capped at"), "{err}");
    }
}
