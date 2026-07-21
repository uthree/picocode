//! Embedded UI assets.
//!
//! gpui-component's icons are SVG assets it expects the application to
//! serve (`icons/*.svg`, Lucide icons); it doesn't bundle the files.
//! Without an asset source the icons silently render as nothing — only
//! the ones actually used are embedded here.
//!
//! Icon data © Lucide contributors, ISC license
//! (<https://lucide.dev/license>).

use std::borrow::Cow;

use gpui::{AssetSource, SharedString};

/// One embedded Lucide icon body (everything inside the `<svg>` tag).
macro_rules! lucide {
    ($body:expr) => {
        concat!(
            r##"<svg xmlns="http://www.w3.org/2000/svg" width="24" height="24" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round">"##,
            $body,
            "</svg>"
        )
    };
}

const ICONS: &[(&str, &str)] = &[
    (
        "icons/copy.svg",
        lucide!(
            r#"<rect width="14" height="14" x="8" y="8" rx="2" ry="2"/><path d="M4 16c-1.1 0-2-.9-2-2V4c0-1.1.9-2 2-2h10c1.1 0 2 .9 2 2"/>"#
        ),
    ),
    ("icons/check.svg", lucide!(r#"<path d="M20 6 9 17l-5-5"/>"#)),
    // Per-tool icons for the transcript's tool-call rows.
    (
        "icons/file-text.svg",
        lucide!(
            r#"<path d="M15 2H6a2 2 0 0 0-2 2v16a2 2 0 0 0 2 2h12a2 2 0 0 0 2-2V7Z"/><path d="M14 2v4a2 2 0 0 0 2 2h4"/><path d="M10 9H8"/><path d="M16 13H8"/><path d="M16 17H8"/>"#
        ),
    ),
    (
        "icons/folder.svg",
        lucide!(
            r#"<path d="M20 20a2 2 0 0 0 2-2V8a2 2 0 0 0-2-2h-7.9a2 2 0 0 1-1.69-.9L9.6 3.9A2 2 0 0 0 7.93 3H4a2 2 0 0 0-2 2v13a2 2 0 0 0 2 2Z"/>"#
        ),
    ),
    (
        "icons/search.svg",
        lucide!(r#"<circle cx="11" cy="11" r="8"/><path d="m21 21-4.3-4.3"/>"#),
    ),
    (
        "icons/pencil.svg",
        lucide!(
            r#"<path d="M17 3a2.85 2.83 0 1 1 4 4L7.5 20.5 2 22l1.5-5.5Z"/><path d="m15 5 4 4"/>"#
        ),
    ),
    (
        "icons/terminal.svg",
        lucide!(r#"<polyline points="4 17 10 11 4 5"/><line x1="12" x2="20" y1="19" y2="19"/>"#),
    ),
    (
        "icons/globe.svg",
        lucide!(
            r#"<circle cx="12" cy="12" r="10"/><path d="M12 2a14.5 14.5 0 0 0 0 20 14.5 14.5 0 0 0 0-20"/><path d="M2 12h20"/>"#
        ),
    ),
    (
        "icons/download.svg",
        lucide!(
            r#"<path d="M21 15v4a2 2 0 0 1-2 2H5a2 2 0 0 1-2-2v-4"/><polyline points="7 10 12 15 17 10"/><line x1="12" x2="12" y1="15" y2="3"/>"#
        ),
    ),
    (
        "icons/clipboard-list.svg",
        lucide!(
            r#"<rect width="8" height="4" x="8" y="2" rx="1" ry="1"/><path d="M16 4h2a2 2 0 0 1 2 2v14a2 2 0 0 1-2 2H6a2 2 0 0 1-2-2V6a2 2 0 0 1 2-2h2"/><path d="M12 11h4"/><path d="M12 16h4"/><path d="M8 11h.01"/><path d="M8 16h.01"/>"#
        ),
    ),
    (
        "icons/wrench.svg",
        lucide!(
            r#"<path d="M14.7 6.3a1 1 0 0 0 0 1.4l1.6 1.6a1 1 0 0 0 1.4 0l3.77-3.77a6 6 0 0 1-7.94 7.94l-6.91 6.91a2.12 2.12 0 0 1-3-3l6.91-6.91a6 6 0 0 1 7.94-7.94l-3.76 3.76z"/>"#
        ),
    ),
    (
        "icons/git-branch.svg",
        lucide!(
            r#"<line x1="6" x2="6" y1="3" y2="15"/><circle cx="18" cy="6" r="3"/><circle cx="6" cy="18" r="3"/><path d="M18 9a9 9 0 0 1-9 9"/>"#
        ),
    ),
    (
        "icons/loader-circle.svg",
        lucide!(r#"<path d="M21 12a9 9 0 1 1-6.219-8.56"/>"#),
    ),
    (
        "icons/triangle-alert.svg",
        lucide!(
            r#"<path d="m21.73 18-8-14a2 2 0 0 0-3.48 0l-8 14A2 2 0 0 0 4 21h16a2 2 0 0 0 1.73-3Z"/><path d="M12 9v4"/><path d="M12 17h.01"/>"#
        ),
    ),
    (
        "icons/circle-x.svg",
        lucide!(r#"<circle cx="12" cy="12" r="10"/><path d="m15 9-6 6"/><path d="m9 15 6-6"/>"#),
    ),
    // Attachments: the attach button and the non-image chip icons.
    (
        "icons/paperclip.svg",
        lucide!(
            r#"<path d="m21.44 11.05-9.19 9.19a6 6 0 0 1-8.49-8.49l8.57-8.57A4 4 0 1 1 18 8.84l-8.59 8.57a2 2 0 0 1-2.83-2.83l8.49-8.48"/>"#
        ),
    ),
    (
        "icons/music.svg",
        lucide!(
            r#"<path d="M9 18V5l12-2v13"/><circle cx="6" cy="18" r="3"/><circle cx="18" cy="16" r="3"/>"#
        ),
    ),
];

pub struct Assets;

impl AssetSource for Assets {
    fn load(&self, path: &str) -> anyhow::Result<Option<Cow<'static, [u8]>>> {
        Ok(ICONS
            .iter()
            .find(|(name, _)| *name == path)
            .map(|(_, svg)| Cow::Borrowed(svg.as_bytes())))
    }

    fn list(&self, path: &str) -> anyhow::Result<Vec<SharedString>> {
        Ok(ICONS
            .iter()
            .filter(|(name, _)| name.starts_with(path))
            .map(|(name, _)| SharedString::from(*name))
            .collect())
    }
}
