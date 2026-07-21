use std::time::Duration;

use base64::Engine;
use rig::tool::Tool;
use serde::Deserialize;
use serde_json::json;

use super::{ToolError, truncate_output};
use crate::attachment::looks_like_text;

const TIMEOUT: Duration = Duration::from_secs(30);
const MAX_BODY_BYTES: usize = 2 * 1024 * 1024;
const MAX_OUTPUT_BYTES: usize = 24_000;
const TEXT_WIDTH: usize = 100;

/// Image MIME type of a fetched body, from its magic bytes first (servers
/// mislabel), then the Content-Type header. Only the formats providers
/// accept as message content; SVG stays on the text path.
fn image_mime(content_type: &str, body: &[u8]) -> Option<&'static str> {
    match body {
        [0x89, b'P', b'N', b'G', ..] => return Some("image/png"),
        [0xFF, 0xD8, 0xFF, ..] => return Some("image/jpeg"),
        [b'G', b'I', b'F', b'8', ..] => return Some("image/gif"),
        [
            b'R',
            b'I',
            b'F',
            b'F',
            _,
            _,
            _,
            _,
            b'W',
            b'E',
            b'B',
            b'P',
            ..,
        ] => {
            return Some("image/webp");
        }
        _ => {}
    }
    match content_type.split(';').next().unwrap_or("").trim() {
        "image/png" => Some("image/png"),
        "image/jpeg" | "image/jpg" => Some("image/jpeg"),
        "image/gif" => Some("image/gif"),
        "image/webp" => Some("image/webp"),
        _ => None,
    }
}

#[derive(Deserialize)]
pub struct FetchArgs {
    url: String,
}

pub struct WebFetch {
    client: reqwest::Client,
}

impl WebFetch {
    pub fn new() -> Self {
        let client = reqwest::Client::builder()
            .timeout(TIMEOUT)
            .user_agent(concat!("picocode/", env!("CARGO_PKG_VERSION")))
            .build()
            .expect("failed to build HTTP client");
        Self { client }
    }
}

impl Default for WebFetch {
    fn default() -> Self {
        Self::new()
    }
}

impl Tool for WebFetch {
    const NAME: &'static str = "web_fetch";
    type Error = ToolError;
    type Args = FetchArgs;
    type Output = String;

    fn description(&self) -> String {
        "Fetch a URL over HTTP(S). HTML pages are converted to plain text; \
         JSON and other text content is returned as-is; images (PNG/JPEG/\
         GIF/WebP) are returned as image content if the provider supports \
         viewing them. Use for documentation, articles, and web APIs."
            .to_string()
    }

    fn parameters(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "url": { "type": "string", "description": "The http:// or https:// URL to fetch" }
            },
            "required": ["url"]
        })
    }

    async fn call(&self, args: Self::Args) -> Result<Self::Output, Self::Error> {
        if !args.url.starts_with("http://") && !args.url.starts_with("https://") {
            return Err(ToolError::new(
                "only http:// and https:// URLs are supported",
            ));
        }

        let response = self
            .client
            .get(&args.url)
            .send()
            .await
            .map_err(|e| ToolError::new(format!("request failed: {e}")))?;

        let status = response.status();
        let content_type = response
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_ascii_lowercase();

        if let Some(len) = response.content_length()
            && len as usize > MAX_BODY_BYTES
        {
            return Err(ToolError::new(format!(
                "response too large ({len} bytes; limit is {MAX_BODY_BYTES})"
            )));
        }

        let bytes = response
            .bytes()
            .await
            .map_err(|e| ToolError::new(format!("failed to read response body: {e}")))?;
        if bytes.len() > MAX_BODY_BYTES {
            return Err(ToolError::new(format!(
                "response too large ({} bytes; limit is {MAX_BODY_BYTES})",
                bytes.len()
            )));
        }

        // Images ride the same tool-output convention as read_file: rig
        // turns this JSON into a text part plus an image part. Providers
        // that take images in tool results (Anthropic) see the image;
        // text-only ones (Ollama) see the response note.
        if let Some(mime) = image_mime(&content_type, &bytes) {
            let note = format!(
                "Fetched the image at {} ({mime}, {} bytes). The image content is \
                 included in this tool result; if you cannot see any image, this \
                 provider cannot show images from tools.",
                args.url,
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

        let is_html =
            content_type.contains("text/html") || content_type.contains("application/xhtml");
        let is_text = is_html
            || content_type.starts_with("text/")
            || content_type.contains("json")
            || content_type.contains("xml")
            || content_type.contains("javascript")
            || content_type.is_empty();
        // Content-Type lies or is missing often enough that text-looking
        // bodies pass regardless; real binary is refused.
        if !is_text && !looks_like_text(&bytes[..bytes.len().min(8 * 1024)]) {
            return Err(ToolError::new(format!(
                "unsupported content type `{content_type}` (binary content is not returned)"
            )));
        }

        let text = if is_html {
            html2text::from_read(&bytes[..], TEXT_WIDTH)
                .map_err(|e| ToolError::new(format!("failed to convert HTML to text: {e}")))?
        } else {
            String::from_utf8_lossy(&bytes).into_owned()
        };

        let mut out = format!("[{status}] {}\n\n", args.url);
        out.push_str(&truncate_output(text.trim(), MAX_OUTPUT_BYTES));
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};

    /// Serve one canned HTTP response on a random local port.
    fn serve_once_bytes(body: &'static [u8], content_type: &'static str) -> String {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        std::thread::spawn(move || {
            if let Ok((mut stream, _)) = listener.accept() {
                let mut buf = [0u8; 4096];
                let _ = stream.read(&mut buf);
                let header = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    body.len()
                );
                let _ = stream.write_all(header.as_bytes());
                let _ = stream.write_all(body);
            }
        });
        format!("http://{addr}/")
    }

    fn serve_once(body: &'static str, content_type: &'static str) -> String {
        serve_once_bytes(body.as_bytes(), content_type)
    }

    #[tokio::test]
    async fn converts_html_to_text() {
        let url = serve_once(
            "<html><body><h1>Title</h1><p>Hello <b>world</b></p></body></html>",
            "text/html; charset=utf-8",
        );
        let out = WebFetch::new().call(FetchArgs { url }).await.unwrap();
        assert!(out.contains("Title"));
        // html2text may decorate inline elements (e.g. `**world**`), so check
        // the words survived rather than the exact phrase.
        assert!(out.contains("Hello"));
        assert!(out.contains("world"));
        assert!(!out.contains("<h1>"));
    }

    #[tokio::test]
    async fn returns_json_as_is() {
        let url = serve_once(r#"{"ok":true}"#, "application/json");
        let out = WebFetch::new().call(FetchArgs { url }).await.unwrap();
        assert!(out.contains(r#"{"ok":true}"#));
    }

    #[tokio::test]
    async fn rejects_non_http_schemes() {
        let err = WebFetch::new()
            .call(FetchArgs {
                url: "ftp://example.com/file".into(),
            })
            .await
            .unwrap_err();
        assert!(err.0.contains("only http"));
    }

    #[tokio::test]
    async fn returns_images_as_tool_image_json() {
        // Magic bytes win even when the header lies (octet-stream).
        let url = serve_once_bytes(&[0x89, b'P', b'N', b'G'], "application/octet-stream");
        let out = WebFetch::new().call(FetchArgs { url }).await.unwrap();
        let v: serde_json::Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["parts"][0]["type"], "image");
        assert_eq!(v["parts"][0]["mimeType"], "image/png");
        assert_eq!(v["parts"][0]["data"], "iVBORw==");
        assert!(v["response"].as_str().unwrap().contains("image/png"));
    }

    #[tokio::test]
    async fn rejects_binary_content() {
        // Neither a known image format nor text.
        let url = serve_once_bytes(&[0u8, 1, 2, 3], "application/octet-stream");
        let err = WebFetch::new().call(FetchArgs { url }).await.unwrap_err();
        assert!(err.0.contains("unsupported content type"));

        // Unsupported image formats aren't smuggled through as text either.
        let url = serve_once_bytes(&[0u8, 1, 2, 3], "image/tiff");
        let err = WebFetch::new().call(FetchArgs { url }).await.unwrap_err();
        assert!(err.0.contains("unsupported content type"));
    }

    #[tokio::test]
    async fn svg_stays_on_the_text_path() {
        let url = serve_once("<svg xmlns='x'><rect/></svg>", "image/svg+xml");
        let out = WebFetch::new().call(FetchArgs { url }).await.unwrap();
        assert!(out.contains("<svg"));
    }
}
