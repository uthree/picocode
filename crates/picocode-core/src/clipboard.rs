//! System-clipboard reading for paste (Ctrl+V in the TUI, Cmd+V in the
//! GUI): copied files and images become staged attachments, plain text
//! falls back to a normal paste.
//!
//! arboard exposes the clipboard flavors separately; the priority here is
//! files (Finder/Explorer copies) → image data (screenshots) → text —
//! the order of how specific each flavor is about what the user copied.

use std::path::{Path, PathBuf};

/// What the clipboard held.
pub enum Pasted {
    /// Copied files (e.g. Finder ⌘C) — stage as attachments.
    Files(Vec<PathBuf>),
    /// Raw image data (e.g. a screenshot) saved to this PNG.
    Image(PathBuf),
    /// Plain text — insert into the input box.
    Text(String),
}

/// Read the clipboard, preferring files, then image data, then text.
/// Image data is written to `dir` as `clipboard-<n>.png` so it can go
/// through the normal file-attachment path. `Ok(None)` means the
/// clipboard holds nothing usable.
pub fn read(dir: &Path, n: usize) -> Result<Option<Pasted>, String> {
    let mut clipboard = arboard::Clipboard::new().map_err(|e| e.to_string())?;
    if let Ok(files) = clipboard.get().file_list()
        && !files.is_empty()
    {
        return Ok(Some(Pasted::Files(files)));
    }
    // Image errors (unsupported source format, …) fall through to text
    // rather than failing the paste; only failing to *save* is an error.
    if let Ok(image) = clipboard.get_image() {
        let path = save_png(&image, dir, n)
            .map_err(|e| format!("could not save the pasted image: {e}"))?;
        return Ok(Some(Pasted::Image(path)));
    }
    match clipboard.get_text() {
        Ok(text) if !text.is_empty() => Ok(Some(Pasted::Text(text))),
        _ => Ok(None),
    }
}

/// Encode arboard's RGBA bitmap as `dir/clipboard-<n>.png`.
fn save_png(image: &arboard::ImageData, dir: &Path, n: usize) -> std::io::Result<PathBuf> {
    std::fs::create_dir_all(dir)?;
    let path = dir.join(format!("clipboard-{n}.png"));
    let file = std::io::BufWriter::new(std::fs::File::create(&path)?);
    let mut encoder = png::Encoder::new(file, image.width as u32, image.height as u32);
    encoder.set_color(png::ColorType::Rgba);
    encoder.set_depth(png::BitDepth::Eight);
    let mut writer = encoder.write_header().map_err(std::io::Error::other)?;
    writer
        .write_image_data(&image.bytes)
        .map_err(std::io::Error::other)?;
    writer.finish().map_err(std::io::Error::other)?;
    Ok(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn saved_png_decodes_back_to_the_same_pixels() {
        let dir = tempfile::tempdir().unwrap();
        // A 2×1 image: one red pixel, one semi-transparent blue pixel.
        let bytes = [255, 0, 0, 255, 0, 0, 255, 128];
        let image = arboard::ImageData {
            width: 2,
            height: 1,
            bytes: bytes.as_slice().into(),
        };
        let path = save_png(&image, dir.path(), 7).unwrap();
        assert!(path.ends_with("clipboard-7.png"));

        let decoder =
            png::Decoder::new(std::io::BufReader::new(std::fs::File::open(&path).unwrap()));
        let mut reader = decoder.read_info().unwrap();
        let mut buf = vec![0; reader.output_buffer_size().unwrap()];
        let info = reader.next_frame(&mut buf).unwrap();
        assert_eq!((info.width, info.height), (2, 1));
        assert_eq!(info.color_type, png::ColorType::Rgba);
        assert_eq!(&buf[..info.buffer_size()], &bytes);
    }

    /// Manual check against the real system clipboard — copy a file, an
    /// image or text first, then:
    /// `cargo test -p picocode-tui real_clipboard -- --ignored --nocapture`
    #[test]
    #[ignore = "reads the real system clipboard"]
    fn real_clipboard_read() {
        let dir = tempfile::tempdir().unwrap();
        match read(dir.path(), 1) {
            Ok(Some(Pasted::Files(paths))) => println!("files: {paths:?}"),
            Ok(Some(Pasted::Image(path))) => {
                let len = std::fs::metadata(&path).unwrap().len();
                println!("image: {} ({len} bytes)", path.display());
            }
            Ok(Some(Pasted::Text(text))) => println!("text: {text:?}"),
            Ok(None) => println!("empty"),
            Err(e) => panic!("clipboard read failed: {e}"),
        }
    }
}
