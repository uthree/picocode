//! Querying a provider for the models it actually serves, so `/model` can
//! list them and switch to one without a `[[models]]` config entry.

use std::time::Duration;

use anyhow::Context;

use crate::config::{Config, Provider};

/// One row in a model-switch listing: a configured `[[models]]` entry, or a
/// model id the provider reported serving.
pub struct ModelChoice {
    /// Name accepted by the model switch: a config entry name, or a model id
    /// the provider reported serving.
    pub name: String,
    /// Display detail: the entry's label (and URL), or the provider name for
    /// served ids.
    pub detail: String,
    pub active: bool,
}

/// Rows for a model-switch listing: the configured `[[models]]` entries
/// first, then the models the provider reported serving, skipping ids an
/// entry already covers (same name, or same model on the same endpoint).
pub fn model_choices(
    models: &[crate::config::ModelEntry],
    active_model: Option<&str>,
    provider: Provider,
    base_url: Option<&str>,
    current_model: &str,
    available: &[String],
) -> Vec<ModelChoice> {
    let mut items: Vec<ModelChoice> = models
        .iter()
        .map(|m| {
            let mut detail = m.label();
            if let Some(url) = &m.base_url {
                detail.push_str(&format!(" @ {url}"));
            }
            ModelChoice {
                name: m.name.clone(),
                detail,
                active: active_model == Some(m.name.as_str()),
            }
        })
        .collect();
    for id in available {
        let covered = models.iter().any(|m| {
            m.name == *id
                || (m.model == *id && m.provider == provider && m.base_url.as_deref() == base_url)
        });
        if !covered {
            items.push(ModelChoice {
                name: id.clone(),
                detail: crate::config::provider_name(provider).to_string(),
                active: active_model.is_none() && current_model == id,
            });
        }
    }
    items
}

/// Pick the first model the Ollama server reports serving (the startup
/// fallback when no model is configured anywhere); when it can't, explain
/// how to set up a model provider instead of starting broken.
pub async fn pick_ollama_model(cfg: &crate::config::Config) -> anyhow::Result<String> {
    let base = base_url(Provider::Ollama, cfg.base_url.as_deref());
    const HINT: &str = "configure a model provider instead:\n  \
         - add a [[models]] entry to picocode.toml (see the README), or\n  \
         - pass --provider and --model on the command line";
    match fetch(Provider::Ollama, cfg.base_url.as_deref()).await {
        Ok(list) => match list.into_iter().next() {
            Some(model) => Ok(model),
            None => anyhow::bail!(
                "Ollama at {base} has no models pulled.\n\
                 Pull one (e.g. `ollama pull qwen3:4b`), or {HINT}"
            ),
        },
        Err(e) => anyhow::bail!(
            "No model is configured and Ollama is not reachable at {base} ({e:#}).\n\
             Install and start it (https://ollama.com/download), or {HINT}"
        ),
    }
}

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

/// Outcome of resolving a partial model name (see [`resolve_partial`]).
#[derive(Debug, PartialEq, Eq)]
pub enum PartialMatch {
    /// Nothing matched.
    None,
    /// Exactly one candidate matched: its canonical name (an entry name or
    /// a served model id), ready for the by-name switch.
    Unique(String),
    /// Several candidates matched (listed for the error message).
    Ambiguous(Vec<String>),
}

/// Resolve a possibly-partial model name against the switch candidates
/// (configured entry names + served model ids). An exact match wins
/// outright; otherwise a case-insensitive substring match must be unique.
pub fn resolve_partial(
    name: &str,
    entries: &[crate::config::ModelEntry],
    available: &[String],
) -> PartialMatch {
    let candidates: Vec<&str> = entries
        .iter()
        .map(|m| m.name.as_str())
        .chain(available.iter().map(String::as_str))
        .collect();
    if candidates.contains(&name) {
        return PartialMatch::Unique(name.to_string());
    }
    let needle = name.to_lowercase();
    let mut matches: Vec<&str> = candidates
        .iter()
        .copied()
        .filter(|c| c.to_lowercase().contains(&needle))
        .collect();
    matches.dedup();
    match matches.len() {
        0 => PartialMatch::None,
        1 => PartialMatch::Unique(matches[0].to_string()),
        _ => PartialMatch::Ambiguous(matches.into_iter().map(str::to_string).collect()),
    }
}

/// How a `/model <name>` request resolves against the current config (see
/// [`plan_switch`]). Carries no user-facing text — each front end renders
/// its own messages from the variants.
pub enum SwitchPlan {
    /// Several candidates matched the partial name (listed for the error).
    Ambiguous(Vec<String>),
    /// The named model is already the active one; `display` labels it.
    AlreadyActive { display: String },
    /// Nothing configured or served has this name.
    Unknown { name: String },
    /// Switch to `cfg` — a clone of the current config pointed at the new
    /// model; `name` is the resolved candidate name (for the notices).
    /// Boxed to keep the enum small next to its message-only variants.
    Switch { name: String, cfg: Box<Config> },
}

/// Resolve `/model <requested>` into a switch plan, shared by both front
/// ends: partial-name completion first, then the configured `[[models]]`
/// entries, then models the provider reported serving (an ad-hoc switch
/// keeping the current provider and base URL). The first element is the
/// full name a partial `requested` completed to, worth a notice.
pub fn plan_switch(
    cfg: &Config,
    requested: &str,
    available: &[String],
) -> (Option<String>, SwitchPlan) {
    let name = match resolve_partial(requested, &cfg.models, available) {
        PartialMatch::Unique(full) => full,
        PartialMatch::Ambiguous(matches) => return (None, SwitchPlan::Ambiguous(matches)),
        PartialMatch::None => requested.to_string(),
    };
    let matched = (name != requested).then(|| name.clone());
    let mut new_cfg = cfg.clone();
    let plan = match cfg.models.iter().find(|m| m.name == name) {
        Some(entry) => {
            if cfg.active_model.as_deref() == Some(name.as_str()) {
                SwitchPlan::AlreadyActive {
                    display: format!("{name} ({})", entry.label()),
                }
            } else {
                new_cfg.provider = entry.provider;
                new_cfg.model = entry.model.clone();
                new_cfg.base_url = entry.base_url.clone();
                new_cfg.active_model = Some(entry.name.clone());
                new_cfg.context_window = entry
                    .context_window
                    .unwrap_or(crate::config::DEFAULT_CONTEXT_WINDOW);
                SwitchPlan::Switch {
                    name,
                    cfg: Box::new(new_cfg),
                }
            }
        }
        // A model id the provider reported serving: switch ad hoc, keeping
        // the current provider and base URL.
        None if available.contains(&name) => {
            if cfg.active_model.is_none() && cfg.model == name {
                SwitchPlan::AlreadyActive {
                    display: cfg.model_label(),
                }
            } else {
                new_cfg.model = name.clone();
                new_cfg.active_model = None;
                new_cfg.context_window = crate::config::DEFAULT_CONTEXT_WINDOW;
                SwitchPlan::Switch {
                    name,
                    cfg: Box::new(new_cfg),
                }
            }
        }
        None => SwitchPlan::Unknown { name },
    };
    (matched, plan)
}

/// The config for an explicit provider/model/base-URL selection (the
/// add-model form): an ad-hoc switch like `--provider`/`--model` on the
/// command line.
pub fn custom_config(
    cfg: &Config,
    provider: Provider,
    model: String,
    base_url: Option<String>,
) -> Config {
    let mut new_cfg = cfg.clone();
    new_cfg.provider = provider;
    new_cfg.model = model;
    new_cfg.base_url = base_url;
    new_cfg.active_model = None;
    new_cfg.context_window = crate::config::DEFAULT_CONTEXT_WINDOW;
    new_cfg
}

/// A ready-to-paste `[[models]]` snippet for an ad-hoc selection, shown
/// after switching via the add-model form so the choice can be made
/// permanent in picocode.toml.
pub fn toml_snippet(provider: Provider, model: &str, base_url: Option<&str>) -> String {
    let name: String = model
        .chars()
        .map(|c| if c.is_whitespace() { '-' } else { c })
        .collect();
    let mut out = format!(
        "[[models]]\nname = \"{name}\"\nprovider = \"{}\"\nmodel = \"{model}\"",
        crate::config::provider_name(provider),
    );
    if let Some(url) = base_url {
        out.push_str(&format!("\nbase_url = \"{url}\""));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_partial_matches_uniquely() {
        let entries = vec![crate::config::ModelEntry {
            name: "local".into(),
            provider: Provider::Ollama,
            model: "qwen3:4b".into(),
            base_url: None,
            context_window: None,
        }];
        let available = vec![
            "qwen3:4b".to_string(),
            "qwen3:8b".to_string(),
            "gemma4:e2b".to_string(),
        ];

        // Exact name wins even when it is a substring of others.
        assert_eq!(
            resolve_partial("qwen3:4b", &entries, &available),
            PartialMatch::Unique("qwen3:4b".into())
        );
        // Unique substring (case-insensitive) resolves.
        assert_eq!(
            resolve_partial("GEMMA", &entries, &available),
            PartialMatch::Unique("gemma4:e2b".into())
        );
        assert_eq!(
            resolve_partial("loc", &entries, &available),
            PartialMatch::Unique("local".into())
        );
        // Ambiguous and missing names are reported as such.
        assert!(matches!(
            resolve_partial("qwen", &entries, &available),
            PartialMatch::Ambiguous(_)
        ));
        assert_eq!(
            resolve_partial("nope", &entries, &available),
            PartialMatch::None
        );
    }

    #[test]
    fn plan_switch_covers_entries_served_models_and_misses() {
        let mut base = Config::for_tests();
        base.models = vec![crate::config::ModelEntry {
            name: "local".into(),
            provider: Provider::Ollama,
            model: "qwen3:8b".into(),
            base_url: None,
            context_window: Some(64_000),
        }];
        let available = vec![
            "qwen3:4b".to_string(),
            "qwen3:30b".to_string(),
            "gemma4:e2b".to_string(),
        ];

        // A configured entry switches to its provider/model and records the
        // entry as active; a partial name reports what it completed to.
        let (matched, plan) = plan_switch(&base, "loc", &available);
        assert_eq!(matched.as_deref(), Some("local"));
        match plan {
            SwitchPlan::Switch { name, cfg } => {
                assert_eq!(name, "local");
                assert_eq!(cfg.model, "qwen3:8b");
                assert_eq!(cfg.active_model.as_deref(), Some("local"));
                assert_eq!(cfg.context_window, 64_000);
            }
            _ => panic!("expected a switch"),
        }

        // A served model id switches ad hoc (no entry, default window).
        let (matched, plan) = plan_switch(&base, "gemma4:e2b", &available);
        assert_eq!(matched, None);
        match plan {
            SwitchPlan::Switch { name, cfg } => {
                assert_eq!(name, "gemma4:e2b");
                assert_eq!(cfg.active_model, None);
                assert_eq!(cfg.context_window, crate::config::DEFAULT_CONTEXT_WINDOW);
            }
            _ => panic!("expected a switch"),
        }

        // The current selection is reported as already active, both for the
        // active entry and for an ad-hoc model id.
        base.active_model = Some("local".into());
        assert!(matches!(
            plan_switch(&base, "local", &available).1,
            SwitchPlan::AlreadyActive { display } if display == "local (ollama/qwen3:8b)"
        ));
        base.active_model = None;
        assert!(matches!(
            plan_switch(&base, "qwen3:4b", &available).1,
            SwitchPlan::AlreadyActive { display } if display == "ollama/qwen3:4b"
        ));

        // Unknown and ambiguous names resolve to their message variants.
        assert!(matches!(
            plan_switch(&base, "nope", &available).1,
            SwitchPlan::Unknown { name } if name == "nope"
        ));
        assert!(matches!(
            plan_switch(&base, "qwen", &available).1,
            SwitchPlan::Ambiguous(_)
        ));
    }

    #[test]
    fn custom_config_is_an_adhoc_selection() {
        let mut base = Config::for_tests();
        base.active_model = Some("local".into());
        base.context_window = 64_000;
        let cfg = custom_config(
            &base,
            Provider::Openai,
            "gpt-x".into(),
            Some("http://host:8000/v1".into()),
        );
        assert_eq!(cfg.model, "gpt-x");
        assert_eq!(cfg.provider, Provider::Openai);
        assert_eq!(cfg.base_url.as_deref(), Some("http://host:8000/v1"));
        assert_eq!(cfg.active_model, None);
        assert_eq!(cfg.context_window, crate::config::DEFAULT_CONTEXT_WINDOW);
    }

    #[test]
    fn toml_snippet_is_pasteable() {
        let s = toml_snippet(Provider::Anthropic, "claude-haiku-4-5", None);
        assert!(s.contains("provider = \"anthropic\""));
        assert!(s.contains("name = \"claude-haiku-4-5\""));
        assert!(!s.contains("base_url"));
        let s = toml_snippet(Provider::Openai, "qwen3:4b", Some("http://host:8000/v1"));
        assert!(s.contains("base_url = \"http://host:8000/v1\""));
        // The snippet is valid TOML.
        assert!(toml::from_str::<toml::Value>(&s).is_ok());
    }

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

    #[test]
    fn model_choices_merge_config_and_served_models() {
        use crate::config::ModelEntry;
        let models = vec![
            ModelEntry {
                name: "local".into(),
                provider: Provider::Ollama,
                model: "qwen3:4b".into(),
                base_url: None,
                context_window: None,
            },
            ModelEntry {
                name: "vllm".into(),
                provider: Provider::Openai,
                model: "qwen3:8b".into(),
                base_url: Some("http://host:8000/v1".into()),
                context_window: None,
            },
        ];
        let available = vec!["qwen3:0.6b".into(), "qwen3:4b".into()];
        let items = model_choices(
            &models,
            Some("local"),
            Provider::Ollama,
            None,
            "qwen3:4b",
            &available,
        );
        // Config entries first; qwen3:4b is covered by `local` on the same
        // endpoint, so only qwen3:0.6b is appended.
        let names: Vec<&str> = items.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(names, ["local", "vllm", "qwen3:0.6b"]);
        assert!(items[0].active);
        assert!(!items[2].active);
        assert_eq!(items[1].detail, "openai/qwen3:8b @ http://host:8000/v1");
        assert_eq!(items[2].detail, "ollama");

        // Ad-hoc selection: no active entry, the current model id is marked.
        let items = model_choices(
            &models,
            None,
            Provider::Ollama,
            None,
            "qwen3:0.6b",
            &available,
        );
        assert!(items.iter().any(|c| c.name == "qwen3:0.6b" && c.active));
        assert!(items.iter().all(|c| c.name != "local" || !c.active));
    }
}
