//! Display-math typesetting via RaTeX.
//!
//! Display equations (`$$...$$`, `\[...\]`) are rendered to transparent
//! PNGs with [RaTeX](https://github.com/erweixin/RaTeX) — KaTeX-grade
//! typesetting in pure Rust — and shown as inline images. The glyph color
//! follows the theme's foreground and the pixel density the window's
//! scale factor. Inline math keeps the lighter Unicode approximation
//! (see [`crate::math`]), since images can't flow inside a text run.

use std::collections::HashMap;
use std::sync::Arc;

use gpui::Hsla;
use ratex_layout::{LayoutOptions, layout, to_display_list};
use ratex_render::{RenderOptions, render_to_png};
use ratex_types::color::Color;
use ratex_types::math_style::MathStyle;

/// Base font size of rendered formulas, in logical pixels (slightly larger
/// than the UI text, as display math typically is).
const FONT_SIZE: f32 = 17.0;
const PADDING: f32 = 2.0;

/// A rendered formula: the encoded PNG plus its logical (scale-corrected)
/// size for layout.
pub struct MathImage {
    pub image: Arc<gpui::Image>,
    pub width: f32,
    pub height: f32,
}

/// Rendered-formula cache. `None` records a failed render so it isn't
/// retried every frame; keys include color and scale, so a theme change
/// simply misses and re-renders.
pub type MathCache = HashMap<String, Option<Arc<MathImage>>>;

pub fn cache_key(tex: &str, color: Hsla, scale: f32) -> String {
    let rgba: gpui::Rgba = color.to_rgb();
    format!(
        "{scale:.2}|{:02x}{:02x}{:02x}{:02x}|{tex}",
        (rgba.r * 255.0) as u8,
        (rgba.g * 255.0) as u8,
        (rgba.b * 255.0) as u8,
        (rgba.a * 255.0) as u8,
    )
}

/// Typeset `tex` in display style, or `None` if RaTeX can't (the caller
/// falls back to the Unicode approximation).
pub fn render_display(tex: &str, color: Hsla, scale: f32) -> Option<MathImage> {
    let rgba: gpui::Rgba = color.to_rgb();
    let nodes = ratex_parser::parser::parse(tex).ok()?;
    let options = LayoutOptions::default()
        .with_style(MathStyle::Display)
        .with_color(Color {
            r: rgba.r,
            g: rgba.g,
            b: rgba.b,
            a: rgba.a,
        });
    let lbox = layout(&nodes, &options);
    let display_list = to_display_list(&lbox);
    let png = render_to_png(
        &display_list,
        &RenderOptions {
            font_size: FONT_SIZE,
            padding: PADDING,
            background_color: Color {
                r: 0.0,
                g: 0.0,
                b: 0.0,
                a: 0.0,
            },
            font_dir: String::new(), // fonts are embedded (embed-fonts)
            device_pixel_ratio: scale.clamp(0.5, 8.0),
        },
    )
    .ok()?;
    let (w, h) = png_dimensions(&png)?;
    Some(MathImage {
        image: Arc::new(gpui::Image::from_bytes(gpui::ImageFormat::Png, png)),
        width: w as f32 / scale,
        height: h as f32 / scale,
    })
}

/// Width/height from the PNG IHDR chunk (always the first chunk, at a
/// fixed offset), avoiding a full decode.
fn png_dimensions(png: &[u8]) -> Option<(u32, u32)> {
    if png.len() < 24 || &png[12..16] != b"IHDR" {
        return None;
    }
    let w = u32::from_be_bytes(png[16..20].try_into().ok()?);
    let h = u32::from_be_bytes(png[20..24].try_into().ok()?);
    (w > 0 && h > 0).then_some((w, h))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn renders_a_fraction_to_a_png_with_sane_dimensions() {
        let img = render_display(r"\frac{a}{b} + \sqrt{x^2 + 1}", gpui::white(), 2.0)
            .expect("RaTeX should typeset a basic formula");
        // Logical size is the pixel size divided by the scale factor.
        assert!(img.width > 10.0 && img.width < 400.0, "{}", img.width);
        assert!(img.height > 10.0 && img.height < 200.0, "{}", img.height);
    }

    #[test]
    fn invalid_tex_reports_failure() {
        assert!(render_display(r"\frac{a}{", gpui::white(), 2.0).is_none());
    }
}
