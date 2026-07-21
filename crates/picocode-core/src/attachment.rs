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

/// What kind of media a file is, judged by its extension.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AttachmentKind {
    Image,
    Audio,
    Pdf,
}

impl AttachmentKind {
    pub fn label(self) -> &'static str {
        match self {
            AttachmentKind::Image => "image",
            AttachmentKind::Audio => "audio",
            AttachmentKind::Pdf => "PDF",
        }
    }
}

/// A file staged for sending with the next prompt.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Attachment {
    pub path: PathBuf,
    pub kind: AttachmentKind,
}

impl Attachment {
    /// Classify a file by extension; `None` for anything picocode can't
    /// send as media (callers point the user at `read_file` for text).
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

    /// Whether rig's conversion for `provider` actually forwards this kind
    /// (anything else would be dropped or rejected mid-request).
    pub fn supported_by(&self, provider: Provider) -> bool {
        match provider {
            Provider::Ollama => self.kind == AttachmentKind::Image,
            Provider::Anthropic => matches!(self.kind, AttachmentKind::Image | AttachmentKind::Pdf),
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
}
