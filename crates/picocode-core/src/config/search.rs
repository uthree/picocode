//! Web-search configuration: providers, the `[search]` file section,
//! and the runtime-shared handle.

use serde::Deserialize;

// ----- web search -----------------------------------------------------------

/// Web search backends selectable in the `[search]` config section.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SearchProvider {
    /// DuckDuckGo's HTML endpoint. No API key needed (default).
    #[default]
    Duckduckgo,
    /// A SearXNG instance; requires `base_url` (and `format: json` enabled
    /// server-side).
    Searxng,
    /// Brave Search API; requires the `BRAVE_API_KEY` environment variable.
    Brave,
}

impl SearchProvider {
    pub fn label(self) -> &'static str {
        match self {
            SearchProvider::Duckduckgo => "duckduckgo",
            SearchProvider::Searxng => "searxng",
            SearchProvider::Brave => "brave",
        }
    }
}

/// `[search]` section of the config file.
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct SearchFileConfig {
    pub(super) provider: Option<SearchProvider>,
    /// Overrides the provider's endpoint (required for searxng).
    pub(super) base_url: Option<String>,
    pub(super) max_results: Option<usize>,
}

/// Search settings after defaults and validation.
#[derive(Clone, Debug)]
pub struct SearchConfig {
    pub provider: SearchProvider,
    pub base_url: Option<String>,
    pub max_results: usize,
    /// API key for providers that need one (brave).
    pub api_key: Option<String>,
}

pub(super) fn resolve_search(
    file: SearchFileConfig,
    brave_key: Option<String>,
) -> anyhow::Result<SearchConfig> {
    let provider = file.provider.unwrap_or_default();
    if provider == SearchProvider::Searxng && file.base_url.is_none() {
        anyhow::bail!("[search] provider \"searxng\" requires base_url in the config");
    }
    // The key is kept even when another provider starts selected, so `/config`
    // can switch to brave at runtime.
    if provider == SearchProvider::Brave && brave_key.is_none() {
        anyhow::bail!(
            "[search] provider \"brave\" requires the BRAVE_API_KEY environment variable"
        );
    }
    Ok(SearchConfig {
        provider,
        base_url: file.base_url,
        max_results: file.max_results.unwrap_or(5).clamp(1, 20),
        api_key: brave_key,
    })
}

/// Shared, runtime-editable web-search settings: the `/config` dialog
/// switches the provider / result count while the web_search tool takes a
/// snapshot per call.
#[derive(Clone, Debug)]
pub struct SearchHandle(std::sync::Arc<std::sync::RwLock<SearchConfig>>);

impl SearchHandle {
    pub fn new(cfg: SearchConfig) -> Self {
        Self(std::sync::Arc::new(std::sync::RwLock::new(cfg)))
    }

    /// Copy of the current settings (one consistent view per search call).
    pub fn snapshot(&self) -> SearchConfig {
        self.0.read().unwrap().clone()
    }

    pub fn set_max_results(&self, n: usize) {
        self.0.write().unwrap().max_results = n;
    }

    /// Providers usable right now: searxng needs a configured base_url and
    /// brave a BRAVE_API_KEY; duckduckgo always works.
    pub fn available_providers(&self) -> Vec<SearchProvider> {
        let cfg = self.0.read().unwrap();
        [
            SearchProvider::Duckduckgo,
            SearchProvider::Searxng,
            SearchProvider::Brave,
        ]
        .into_iter()
        .filter(|p| match p {
            SearchProvider::Duckduckgo => true,
            SearchProvider::Searxng => cfg.base_url.is_some(),
            SearchProvider::Brave => cfg.api_key.is_some(),
        })
        .collect()
    }

    /// Step to the previous/next usable provider (`/config` ←/→).
    pub fn cycle_provider(&self, delta: i64) {
        let choices = self.available_providers();
        let mut cfg = self.0.write().unwrap();
        let n = choices.len();
        let next = match choices.iter().position(|p| *p == cfg.provider) {
            Some(i) if delta < 0 => choices[(i + n - 1) % n],
            Some(i) => choices[(i + 1) % n],
            None => choices[0],
        };
        cfg.provider = next;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{FileConfig, merge};

    #[test]
    fn search_handle_switches_between_available_providers() {
        let handle = SearchHandle::new(SearchConfig {
            provider: SearchProvider::Duckduckgo,
            base_url: None,
            max_results: 5,
            api_key: None,
        });
        // Neither searxng (no base_url) nor brave (no key) is available.
        assert_eq!(handle.available_providers(), [SearchProvider::Duckduckgo]);
        handle.cycle_provider(1);
        assert_eq!(handle.snapshot().provider, SearchProvider::Duckduckgo);

        // The tool side sees runtime changes.
        let tool_side = handle.clone();
        handle.set_max_results(9);
        assert_eq!(tool_side.snapshot().max_results, 9);

        // With a key, brave joins the cycle.
        let handle = SearchHandle::new(SearchConfig {
            provider: SearchProvider::Duckduckgo,
            base_url: None,
            max_results: 5,
            api_key: Some("k".into()),
        });
        assert_eq!(
            handle.available_providers(),
            [SearchProvider::Duckduckgo, SearchProvider::Brave]
        );
        handle.cycle_provider(1);
        assert_eq!(handle.snapshot().provider, SearchProvider::Brave);
        handle.cycle_provider(1);
        assert_eq!(handle.snapshot().provider, SearchProvider::Duckduckgo);
        handle.cycle_provider(-1);
        assert_eq!(handle.snapshot().provider, SearchProvider::Brave);
    }

    #[test]
    fn search_section_parses_merges_and_validates() {
        let global: FileConfig = toml::from_str(
            r#"
            [search]
            provider = "duckduckgo"
            max_results = 3
            "#,
        )
        .unwrap();
        let project: FileConfig = toml::from_str(
            r#"
            [search]
            provider = "searxng"
            base_url = "http://localhost:8888"
            "#,
        )
        .unwrap();
        let merged = merge(global, project);
        assert_eq!(merged.search.provider, Some(SearchProvider::Searxng));
        assert_eq!(merged.search.max_results, Some(3));

        let resolved = resolve_search(merged.search, None).unwrap();
        assert_eq!(resolved.provider, SearchProvider::Searxng);
        assert_eq!(resolved.base_url.as_deref(), Some("http://localhost:8888"));
        assert_eq!(resolved.max_results, 3);

        // Defaults: duckduckgo, 5 results.
        let default = resolve_search(SearchFileConfig::default(), None).unwrap();
        assert_eq!(default.provider, SearchProvider::Duckduckgo);
        assert_eq!(default.max_results, 5);

        // searxng without base_url is a config error.
        let bad: FileConfig = toml::from_str("[search]\nprovider = \"searxng\"").unwrap();
        assert!(resolve_search(bad.search, None).is_err());

        // brave requires an API key from the environment.
        let brave: FileConfig = toml::from_str("[search]\nprovider = \"brave\"").unwrap();
        assert!(resolve_search(brave.search.clone(), None).is_err());
        let ok = resolve_search(brave.search, Some("k".into())).unwrap();
        assert_eq!(ok.api_key.as_deref(), Some("k"));

        assert!(toml::from_str::<FileConfig>("[search]\nproviderr = \"x\"").is_err());
    }
}
