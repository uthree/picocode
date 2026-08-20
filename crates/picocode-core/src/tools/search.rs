use std::sync::LazyLock;
use std::time::Duration;

use regex::Regex;
use rig::tool::Tool;
use serde::Deserialize;
use serde_json::json;

use super::{ToolError, truncate_output};
use crate::config::{SearchConfig, SearchHandle, SearchProvider};

const TIMEOUT: Duration = Duration::from_secs(30);
const MAX_OUTPUT_BYTES: usize = 16_000;

const DUCKDUCKGO_ENDPOINT: &str = "https://html.duckduckgo.com";
const BRAVE_ENDPOINT: &str = "https://api.search.brave.com";

#[derive(Deserialize)]
pub struct SearchArgs {
    query: String,
}

struct SearchResult {
    title: String,
    url: String,
    snippet: String,
}

pub struct WebSearch {
    cfg: SearchHandle,
    client: reqwest::Client,
}

impl WebSearch {
    pub fn new(cfg: SearchHandle) -> Self {
        crate::config::install_tls_provider();
        let client = reqwest::Client::builder()
            .timeout(TIMEOUT)
            .user_agent(concat!("picocode/", env!("CARGO_PKG_VERSION")))
            .build()
            .expect("failed to build HTTP client");
        Self { cfg, client }
    }

    async fn search(&self, query: &str) -> Result<Vec<SearchResult>, ToolError> {
        // One consistent snapshot per call; `/config` changes apply to the
        // next search.
        let cfg = self.cfg.snapshot();
        match cfg.provider {
            SearchProvider::Duckduckgo => self.search_duckduckgo(&cfg, query).await,
            SearchProvider::Searxng => self.search_searxng(&cfg, query).await,
            SearchProvider::Brave => self.search_brave(&cfg, query).await,
        }
    }

    async fn search_duckduckgo(
        &self,
        cfg: &SearchConfig,
        query: &str,
    ) -> Result<Vec<SearchResult>, ToolError> {
        let base = cfg
            .endpoint_for(SearchProvider::Duckduckgo)
            .unwrap_or(DUCKDUCKGO_ENDPOINT);
        let body = self
            .get(
                &format!("{}/html/", base.trim_end_matches('/')),
                &[("q", query)],
                None,
            )
            .await?;
        Ok(parse_duckduckgo(&body, cfg.max_results))
    }

    async fn search_searxng(
        &self,
        cfg: &SearchConfig,
        query: &str,
    ) -> Result<Vec<SearchResult>, ToolError> {
        // base_url presence is validated at config load (and the runtime
        // provider cycle only offers searxng when it is set).
        let base = cfg
            .endpoint_for(SearchProvider::Searxng)
            .unwrap_or_default();
        let body = self
            .get(
                &format!("{}/search", base.trim_end_matches('/')),
                &[("q", query), ("format", "json")],
                None,
            )
            .await?;
        parse_searxng(&body, cfg.max_results)
    }

    async fn search_brave(
        &self,
        cfg: &SearchConfig,
        query: &str,
    ) -> Result<Vec<SearchResult>, ToolError> {
        // Only a base_url configured for brave is used here: the key rides
        // along in a header, so it must not follow a switch from searxng.
        let base = cfg
            .endpoint_for(SearchProvider::Brave)
            .unwrap_or(BRAVE_ENDPOINT);
        let count = cfg.max_results.to_string();
        let body = self
            .get(
                &format!("{}/res/v1/web/search", base.trim_end_matches('/')),
                &[("q", query), ("count", count.as_str())],
                cfg.api_key.as_deref(),
            )
            .await?;
        parse_brave(&body, cfg.max_results)
    }

    async fn get(
        &self,
        url: &str,
        params: &[(&str, &str)],
        brave_key: Option<&str>,
    ) -> Result<String, ToolError> {
        let mut req = self.client.get(url).query(params);
        if let Some(key) = brave_key {
            req = req
                .header("X-Subscription-Token", key)
                .header("Accept", "application/json");
        }
        let response = req
            .send()
            .await
            .map_err(|e| ToolError::new(format!("search request failed: {e}")))?;
        let status = response.status();
        if !status.is_success() {
            return Err(ToolError::new(format!(
                "search request failed with status {status}"
            )));
        }
        response
            .text()
            .await
            .map_err(|e| ToolError::new(format!("failed to read search response: {e}")))
    }
}

impl Tool for WebSearch {
    const NAME: &'static str = "web_search";
    type Error = ToolError;
    type Args = SearchArgs;
    type Output = String;

    fn description(&self) -> String {
        "Search the web and return the top results as titles, URLs and snippets. \
         Follow up with web_fetch to read a promising result in full."
            .to_string()
    }

    fn parameters(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "query": { "type": "string", "description": "The search query" }
            },
            "required": ["query"]
        })
    }

    async fn call(&self, args: Self::Args) -> Result<Self::Output, Self::Error> {
        let query = args.query.trim();
        if query.is_empty() {
            return Err(ToolError::new("search query is empty"));
        }
        let results = self.search(query).await?;
        Ok(format_results(query, &results))
    }
}

// ----- providers' response parsing -----------------------------------------

/// Scrape DuckDuckGo's HTML endpoint. Result links carry the target inside a
/// `uddg` redirect parameter; ad rows (y.js redirects) are dropped.
fn parse_duckduckgo(html: &str, max: usize) -> Vec<SearchResult> {
    static LINK: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r#"(?s)class="result__a" href="([^"]+)"[^>]*>(.*?)</a>"#).unwrap()
    });
    static SNIPPET: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r#"(?s)class="result__snippet"[^>]*>(.*?)</a>"#).unwrap());

    let snippets: Vec<String> = SNIPPET
        .captures_iter(html)
        .map(|c| clean_html(&c[1]))
        .collect();

    LINK.captures_iter(html)
        .filter_map(|c| {
            let href = decode_entities(&c[1]);
            if href.contains("duckduckgo.com/y.js") {
                return None; // ad
            }
            Some((resolve_ddg_href(&href), clean_html(&c[2])))
        })
        .enumerate()
        .map(|(i, (url, title))| SearchResult {
            title,
            url,
            snippet: snippets.get(i).cloned().unwrap_or_default(),
        })
        .take(max)
        .collect()
}

/// `//duckduckgo.com/l/?uddg=<encoded target>&rut=…` → the decoded target.
fn resolve_ddg_href(href: &str) -> String {
    let absolute = if href.starts_with("//") {
        format!("https:{href}")
    } else {
        href.to_string()
    };
    if let Ok(parsed) = url::Url::parse(&absolute)
        && let Some((_, target)) = parsed.query_pairs().find(|(k, _)| k == "uddg")
    {
        return target.into_owned();
    }
    absolute
}

fn parse_searxng(body: &str, max: usize) -> Result<Vec<SearchResult>, ToolError> {
    #[derive(Deserialize)]
    struct Response {
        #[serde(default)]
        results: Vec<Item>,
    }
    #[derive(Deserialize)]
    struct Item {
        #[serde(default)]
        title: String,
        url: String,
        #[serde(default)]
        content: String,
    }
    let response: Response = serde_json::from_str(body).map_err(|e| {
        ToolError::new(format!(
            "invalid SearXNG response: {e} (is `format: json` enabled on the server?)"
        ))
    })?;
    Ok(response
        .results
        .into_iter()
        .take(max)
        .map(|r| SearchResult {
            title: r.title,
            url: r.url,
            snippet: r.content,
        })
        .collect())
}

fn parse_brave(body: &str, max: usize) -> Result<Vec<SearchResult>, ToolError> {
    #[derive(Default, Deserialize)]
    struct Response {
        #[serde(default)]
        web: Web,
    }
    #[derive(Default, Deserialize)]
    struct Web {
        #[serde(default)]
        results: Vec<Item>,
    }
    #[derive(Deserialize)]
    struct Item {
        #[serde(default)]
        title: String,
        url: String,
        #[serde(default)]
        description: String,
    }
    let response: Response = serde_json::from_str(body)
        .map_err(|e| ToolError::new(format!("invalid Brave Search response: {e}")))?;
    Ok(response
        .web
        .results
        .into_iter()
        .take(max)
        .map(|r| SearchResult {
            title: r.title,
            url: r.url,
            snippet: clean_html(&r.description),
        })
        .collect())
}

// ----- output ---------------------------------------------------------------

fn format_results(query: &str, results: &[SearchResult]) -> String {
    if results.is_empty() {
        return format!("No results for \"{query}\".");
    }
    let mut out = format!("Results for \"{query}\":\n");
    for (i, r) in results.iter().enumerate() {
        out.push_str(&format!("\n{}. {}\n   {}\n", i + 1, r.title, r.url));
        if !r.snippet.is_empty() {
            out.push_str(&format!("   {}\n", r.snippet));
        }
    }
    truncate_output(out.trim_end(), MAX_OUTPUT_BYTES)
}

/// Strip tags and decode the handful of entities search results use.
fn clean_html(s: &str) -> String {
    static TAG: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"<[^>]*>").unwrap());
    decode_entities(&TAG.replace_all(s, ""))
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

fn decode_entities(s: &str) -> String {
    s.replace("&amp;", "&")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#x27;", "'")
        .replace("&#39;", "'")
        .replace("&nbsp;", " ")
}

#[cfg(test)]
mod tests {
    use super::*;

    const DDG_HTML: &str = r##"
        <div class="result">
          <a rel="nofollow" class="result__a" href="//duckduckgo.com/l/?uddg=https%3A%2F%2Frust%2Dlang.org%2F&amp;rut=abc">Rust Programming Language</a>
          <a class="result__snippet" href="//duckduckgo.com/l/?uddg=https%3A%2F%2Frust%2Dlang.org%2F&amp;rut=abc">A <b>language</b> empowering everyone &amp; more.</a>
        </div>
        <div class="result result--ad">
          <a rel="nofollow" class="result__a" href="//duckduckgo.com/y.js?ad_provider=x">Sponsored</a>
        </div>
        <div class="result">
          <a rel="nofollow" class="result__a" href="//duckduckgo.com/l/?uddg=https%3A%2F%2Fdoc.rust%2Dlang.org%2Fbook%2F&amp;rut=def">The Book</a>
          <a class="result__snippet" href="//duckduckgo.com/l/?uddg=https%3A%2F%2Fdoc.rust%2Dlang.org%2Fbook%2F&amp;rut=def">Learn Rust</a>
        </div>
    "##;

    #[test]
    fn duckduckgo_parses_and_skips_ads() {
        let results = parse_duckduckgo(DDG_HTML, 5);
        assert_eq!(results.len(), 2);
        assert_eq!(results[0].title, "Rust Programming Language");
        assert_eq!(results[0].url, "https://rust-lang.org/");
        assert_eq!(results[0].snippet, "A language empowering everyone & more.");
        assert_eq!(results[1].url, "https://doc.rust-lang.org/book/");
        assert_eq!(parse_duckduckgo(DDG_HTML, 1).len(), 1);
    }

    #[test]
    fn searxng_parses_json() {
        let body = r#"{"query":"rust","results":[
            {"title":"Rust","url":"https://rust-lang.org/","content":"The site"},
            {"title":"Book","url":"https://doc.rust-lang.org/"}
        ]}"#;
        let results = parse_searxng(body, 5).unwrap();
        assert_eq!(results.len(), 2);
        assert_eq!(results[0].url, "https://rust-lang.org/");
        assert_eq!(results[1].snippet, "");
        assert!(parse_searxng("<html>", 5).is_err());
    }

    #[test]
    fn brave_parses_json() {
        let body = r#"{"web":{"results":[
            {"title":"Rust","url":"https://rust-lang.org/","description":"A <strong>language</strong>"}
        ]}}"#;
        let results = parse_brave(body, 5).unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].snippet, "A language");
        assert!(parse_brave(r#"{"type":"search"}"#, 5).unwrap().is_empty());
    }

    #[test]
    fn output_formatting() {
        let results = vec![SearchResult {
            title: "T".into(),
            url: "https://u/".into(),
            snippet: "S".into(),
        }];
        let out = format_results("q", &results);
        assert!(out.contains("1. T"));
        assert!(out.contains("https://u/"));
        assert!(out.contains("   S"));
        assert_eq!(format_results("q", &[]), "No results for \"q\".");
    }

    #[tokio::test]
    async fn rejects_empty_query() {
        let cfg = SearchConfig {
            provider: SearchProvider::Duckduckgo,
            base_url: None,
            base_url_provider: SearchProvider::Duckduckgo,
            max_results: 5,
            api_key: None,
        };
        let err = WebSearch::new(SearchHandle::new(cfg))
            .call(SearchArgs { query: "  ".into() })
            .await
            .unwrap_err();
        assert!(err.0.contains("empty"));
    }
}
