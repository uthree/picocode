use std::net::IpAddr;
use std::time::Duration;

use base64::Engine;
use futures::StreamExt as _;
use rig::tool::Tool;
use serde::Deserialize;
use serde_json::json;

use super::{ToolError, truncate_output};
use crate::attachment::looks_like_text;

const TIMEOUT: Duration = Duration::from_secs(30);
const MAX_BODY_BYTES: usize = 2 * 1024 * 1024;
const MAX_OUTPUT_BYTES: usize = 24_000;
const TEXT_WIDTH: usize = 100;
/// Redirects followed before giving up. Each hop is re-checked against
/// [`is_blocked`], which is why they are followed here rather than by
/// reqwest: a public URL that redirects to 169.254.169.254 would otherwise
/// sail straight past the check on the original address.
const MAX_REDIRECTS: usize = 5;

/// Addresses `web_fetch` will not reach: the cloud metadata endpoints on
/// the link-local range and the private networks around them. The model
/// chooses this URL, and a page it just read can suggest one as easily as
/// the user can, so "somewhere on the company network" is not a
/// destination it should be able to pick.
///
/// Loopback is deliberately *not* blocked: a local dev server is the one
/// internal address the user plainly meant, and picocode is already running
/// on that machine.
fn is_blocked(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => {
            let [a, b, ..] = v4.octets();
            v4.is_private()
                || v4.is_link_local()
                || v4.is_broadcast()
                || v4.is_unspecified()
                // "this network", and the carrier-grade NAT range.
                || a == 0
                || (a == 100 && (64..128).contains(&b))
        }
        IpAddr::V6(v6) => {
            let first = v6.segments()[0];
            v6.is_unspecified()
                || first & 0xfe00 == 0xfc00 // unique local
                || first & 0xffc0 == 0xfe80 // link local
                || v6.to_ipv4_mapped().is_some_and(|v4| is_blocked(v4.into()))
        }
    }
}

/// Resolve `url`'s host and refuse it if anything it points at is blocked.
/// The lookup happens here rather than trusting the hostname, so a public
/// name resolving to 10.0.0.1 is caught too.
///
/// This is a check, not a guarantee: DNS can answer differently for the
/// connection that follows (rebinding). Closing that would mean dialing the
/// address we checked and carrying the Host header ourselves.
async fn check_destination(url: &url::Url) -> Result<(), ToolError> {
    if !matches!(url.scheme(), "http" | "https") {
        return Err(ToolError::new(
            "only http:// and https:// URLs are supported",
        ));
    }
    let host = url
        .host_str()
        .ok_or_else(|| ToolError::new(format!("`{url}` has no host")))?;
    let port = url.port_or_known_default().unwrap_or(80);
    let addrs: Vec<_> = tokio::net::lookup_host((host, port))
        .await
        .map_err(|e| ToolError::new(format!("could not resolve {host}: {e}")))?
        .collect();
    match addrs.iter().find(|a| is_blocked(a.ip())) {
        Some(bad) => Err(ToolError::new(format!(
            "{host} resolves to {} — web_fetch does not reach link-local or private \
             addresses (cloud metadata endpoints and internal services). If that is \
             really the intent, run it yourself with a `!` command.",
            bad.ip()
        ))),
        None => Ok(()),
    }
}

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
        crate::config::install_tls_provider();
        let client = reqwest::Client::builder()
            .timeout(TIMEOUT)
            // Redirects are followed by hand so every hop gets checked.
            .redirect(reqwest::redirect::Policy::none())
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
        let mut url = url::Url::parse(args.url.trim())
            .map_err(|e| ToolError::new(format!("`{}` is not a valid URL: {e}", args.url)))?;

        let mut hops = 0;
        let response = loop {
            check_destination(&url).await?;
            let response = self
                .client
                .get(url.clone())
                .send()
                .await
                .map_err(|e| ToolError::new(format!("request failed: {e}")))?;
            if !response.status().is_redirection() || hops == MAX_REDIRECTS {
                break response;
            }
            let next = response
                .headers()
                .get(reqwest::header::LOCATION)
                .and_then(|v| v.to_str().ok())
                .and_then(|target| url.join(target).ok());
            match next {
                Some(next) => {
                    url = next;
                    hops += 1;
                }
                // A redirect status with no usable Location: report what
                // came back rather than pretending it was a redirect.
                None => break response,
            }
        };

        let status = response.status();
        let content_type = response
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_ascii_lowercase();

        // Content-Length is only a hint — it is absent on chunked responses,
        // and a server is free to lie — so the cap is enforced as the body
        // arrives rather than after buffering all of it.
        if let Some(len) = response.content_length()
            && len as usize > MAX_BODY_BYTES
        {
            return Err(ToolError::new(format!(
                "response too large ({len} bytes; limit is {MAX_BODY_BYTES})"
            )));
        }
        let mut stream = response.bytes_stream();
        let mut bytes: Vec<u8> = Vec::new();
        while let Some(chunk) = stream.next().await {
            let chunk =
                chunk.map_err(|e| ToolError::new(format!("failed to read response body: {e}")))?;
            if bytes.len() + chunk.len() > MAX_BODY_BYTES {
                return Err(ToolError::new(format!(
                    "response too large (over {MAX_BODY_BYTES} bytes; the limit is enforced \
                     while reading, so the rest was not downloaded)"
                )));
            }
            bytes.extend_from_slice(&chunk);
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
                url,
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

        // The final URL, not the one asked for: after a redirect the model
        // should see where the text actually came from.
        let mut out = format!("[{status}] {url}\n\n");
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
    async fn refuses_link_local_and_private_destinations() {
        for url in [
            // The cloud metadata endpoint, the classic SSRF target.
            "http://169.254.169.254/latest/meta-data/",
            "http://10.0.0.1/admin",
            "http://192.168.1.1/",
            "http://172.16.5.4:8080/",
            "http://[fd00::1]/",
        ] {
            let err = WebFetch::new()
                .call(FetchArgs { url: url.into() })
                .await
                .unwrap_err();
            assert!(
                err.0.contains("does not reach link-local or private"),
                "{url}: {}",
                err.0
            );
        }
    }

    #[test]
    fn loopback_stays_reachable_for_local_dev_servers() {
        assert!(!is_blocked("127.0.0.1".parse().unwrap()));
        assert!(!is_blocked("::1".parse().unwrap()));
        assert!(!is_blocked("93.184.216.34".parse().unwrap()));
        // …including through an IPv4-mapped address.
        assert!(is_blocked("::ffff:169.254.169.254".parse().unwrap()));
        assert!(is_blocked("0.0.0.0".parse().unwrap()));
    }

    /// Serve one redirect to `target`, so the hop can be checked too.
    fn serve_redirect(target: String) -> String {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        std::thread::spawn(move || {
            if let Ok((mut stream, _)) = listener.accept() {
                let mut buf = [0u8; 4096];
                let _ = stream.read(&mut buf);
                let _ = stream.write_all(
                    format!(
                        "HTTP/1.1 302 Found\r\nLocation: {target}\r\nContent-Length: 0\r\n\
                         Connection: close\r\n\r\n"
                    )
                    .as_bytes(),
                );
            }
        });
        format!("http://{addr}/")
    }

    #[tokio::test]
    async fn redirects_are_followed_and_re_checked() {
        // An ordinary redirect still lands on its target.
        let target = serve_once(r#"{"ok":true}"#, "application/json");
        let url = serve_redirect(target);
        let out = WebFetch::new().call(FetchArgs { url }).await.unwrap();
        assert!(out.contains(r#"{"ok":true}"#));

        // A public URL redirecting into the metadata endpoint does not get
        // there: the check on the first address would otherwise be all of it.
        let url = serve_redirect("http://169.254.169.254/latest/meta-data/".to_string());
        let err = WebFetch::new().call(FetchArgs { url }).await.unwrap_err();
        assert!(
            err.0.contains("does not reach link-local or private"),
            "{}",
            err.0
        );
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
