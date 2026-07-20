use std::time::Duration;

use rig::tool::Tool;
use serde::Deserialize;
use serde_json::json;

use super::{ToolError, truncate_output};

const TIMEOUT: Duration = Duration::from_secs(30);
const MAX_BODY_BYTES: usize = 2 * 1024 * 1024;
const MAX_OUTPUT_BYTES: usize = 24_000;
const TEXT_WIDTH: usize = 100;

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
        "Fetch a URL over HTTP(S) and return its content as readable text. \
         HTML pages are converted to plain text; JSON and other text content \
         is returned as-is. Use for documentation, articles, and web APIs."
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

        let is_html =
            content_type.contains("text/html") || content_type.contains("application/xhtml");
        let is_text = is_html
            || content_type.starts_with("text/")
            || content_type.contains("json")
            || content_type.contains("xml")
            || content_type.contains("javascript")
            || content_type.is_empty();
        if !is_text {
            return Err(ToolError::new(format!(
                "unsupported content type `{content_type}` (binary content is not returned)"
            )));
        }

        let body = response
            .text()
            .await
            .map_err(|e| ToolError::new(format!("failed to read response body: {e}")))?;
        if body.len() > MAX_BODY_BYTES {
            return Err(ToolError::new(format!(
                "response too large ({} bytes; limit is {MAX_BODY_BYTES})",
                body.len()
            )));
        }

        let text = if is_html {
            html2text::from_read(body.as_bytes(), TEXT_WIDTH)
                .map_err(|e| ToolError::new(format!("failed to convert HTML to text: {e}")))?
        } else {
            body
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
    fn serve_once(body: &'static str, content_type: &'static str) -> String {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        std::thread::spawn(move || {
            if let Ok((mut stream, _)) = listener.accept() {
                let mut buf = [0u8; 4096];
                let _ = stream.read(&mut buf);
                let response = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = stream.write_all(response.as_bytes());
            }
        });
        format!("http://{addr}/")
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
    async fn rejects_binary_content() {
        let url = serve_once("PNGDATA", "image/png");
        let err = WebFetch::new().call(FetchArgs { url }).await.unwrap_err();
        assert!(err.0.contains("unsupported content type"));
    }
}
