//! Querying a provider for the models it actually serves, so `/model` can
//! list them and switch to one without a `[[models]]` config entry.

use std::time::Duration;

use anyhow::Context;

use crate::config::Provider;

/// The base URL the model-list request goes to, resolved the same way the rig
/// clients resolve theirs: explicit config > provider env var > default.
pub fn base_url(provider: Provider, configured: Option<&str>) -> String {
    resolve_base_url(provider, configured, |var| std::env::var(var).ok())
}

fn resolve_base_url(
    provider: Provider,
    configured: Option<&str>,
    env: impl Fn(&str) -> Option<String>,
) -> String {
    let (var, default) = match provider {
        Provider::Ollama => ("OLLAMA_API_BASE_URL", "http://localhost:11434"),
        Provider::Openai => ("OPENAI_BASE_URL", "https://api.openai.com/v1"),
        Provider::Anthropic => ("ANTHROPIC_BASE_URL", "https://api.anthropic.com"),
    };
    let url = configured
        .map(str::to_string)
        .or_else(|| env(var))
        .unwrap_or_else(|| default.to_string());
    url.trim_end_matches('/').to_string()
}

/// Ask the provider which models it serves: Ollama's `/api/tags`, or the
/// `/models` listing of OpenAI-compatible and Anthropic APIs. Returns the
/// model ids in the provider's own order (the first is used as the default
/// model when nothing is configured); empty means it reported none.
pub async fn fetch(
    provider: Provider,
    configured_base: Option<&str>,
) -> anyhow::Result<Vec<String>> {
    let base = base_url(provider, configured_base);
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(10))
        .build()?;
    let request = match provider {
        Provider::Ollama => client.get(format!("{base}/api/tags")),
        Provider::Openai => {
            // Local OpenAI-compatible servers usually don't check the key.
            let key = std::env::var("OPENAI_API_KEY").unwrap_or_else(|_| "unused".into());
            client.get(format!("{base}/models")).bearer_auth(key)
        }
        Provider::Anthropic => {
            let key = std::env::var("ANTHROPIC_API_KEY").context("ANTHROPIC_API_KEY is not set")?;
            client
                .get(format!("{base}/v1/models"))
                .query(&[("limit", "100")])
                .header("x-api-key", key)
                .header("anthropic-version", "2023-06-01")
        }
    };
    let response = request
        .send()
        .await
        .with_context(|| format!("request to {base} failed"))?;
    let status = response.status();
    if !status.is_success() {
        anyhow::bail!("{base} returned {status}");
    }
    let body: serde_json::Value = response
        .json()
        .await
        .with_context(|| format!("invalid JSON from {base}"))?;
    Ok(parse_names(&body, provider))
}

/// Pull the model ids out of a listing response: Ollama nests them under
/// `models[].name`, the OpenAI and Anthropic APIs under `data[].id`.
fn parse_names(body: &serde_json::Value, provider: Provider) -> Vec<String> {
    let (list, key) = match provider {
        Provider::Ollama => ("models", "name"),
        Provider::Openai | Provider::Anthropic => ("data", "id"),
    };
    body.get(list)
        .and_then(|l| l.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|m| m.get(key).and_then(|n| n.as_str()).map(str::to_string))
                .collect()
        })
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base_url_prefers_config_then_env_then_default() {
        let no_env = |_: &str| None;
        assert_eq!(
            resolve_base_url(Provider::Ollama, Some("http://host:1234/"), no_env),
            "http://host:1234"
        );
        assert_eq!(
            resolve_base_url(Provider::Ollama, None, no_env),
            "http://localhost:11434"
        );
        assert_eq!(
            resolve_base_url(Provider::Openai, None, no_env),
            "https://api.openai.com/v1"
        );
        assert_eq!(
            resolve_base_url(Provider::Anthropic, None, no_env),
            "https://api.anthropic.com"
        );
        let env = |var: &str| (var == "OPENAI_BASE_URL").then(|| "http://vllm:8000/v1".to_string());
        assert_eq!(
            resolve_base_url(Provider::Openai, None, env),
            "http://vllm:8000/v1"
        );
        // Explicit config still wins over the env var.
        assert_eq!(
            resolve_base_url(Provider::Openai, Some("http://other:9000/v1"), env),
            "http://other:9000/v1"
        );
    }

    #[test]
    fn parses_provider_listing_shapes() {
        let ollama = serde_json::json!({
            "models": [{"name": "qwen3:4b", "size": 1}, {"name": "qwen3:8b"}]
        });
        assert_eq!(
            parse_names(&ollama, Provider::Ollama),
            ["qwen3:4b", "qwen3:8b"]
        );

        let openai = serde_json::json!({
            "object": "list",
            "data": [{"id": "gpt-4o", "object": "model"}, {"id": "gpt-4o-mini"}]
        });
        assert_eq!(
            parse_names(&openai, Provider::Openai),
            ["gpt-4o", "gpt-4o-mini"]
        );

        let anthropic = serde_json::json!({
            "data": [{"type": "model", "id": "claude-opus-4-8", "display_name": "Claude Opus 4.8"}]
        });
        assert_eq!(
            parse_names(&anthropic, Provider::Anthropic),
            ["claude-opus-4-8"]
        );

        // Malformed or empty bodies degrade to an empty list, not a panic.
        assert!(parse_names(&serde_json::json!({}), Provider::Ollama).is_empty());
        assert!(parse_names(&serde_json::json!({"models": 3}), Provider::Ollama).is_empty());
    }
}
