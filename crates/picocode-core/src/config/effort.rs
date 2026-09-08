//! Reasoning effort shared by the settings UI and the running worker.

use serde::{Deserialize, Serialize};

use super::{NumHandle, Provider};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Effort {
    #[default]
    Default,
    None,
    Minimal,
    Low,
    Medium,
    High,
    Xhigh,
    Max,
}

impl Effort {
    pub fn label(self) -> String {
        match self {
            Self::Default => rust_i18n::t!("val_effort_default").to_string(),
            _ => self.as_str().to_string(),
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Default => "default",
            Self::None => "none",
            Self::Minimal => "minimal",
            Self::Low => "low",
            Self::Medium => "medium",
            Self::High => "high",
            Self::Xhigh => "xhigh",
            Self::Max => "max",
        }
    }

    /// API-level choices. Individual models may accept only a subset.
    pub fn choices(provider: Provider, model: &str) -> &'static [Self] {
        use Effort::*;
        match provider {
            Provider::Openai => &[Default, None, Minimal, Low, Medium, High, Xhigh],
            Provider::Anthropic => &[Default, Low, Medium, High, Xhigh, Max],
            Provider::Ollama
                if model
                    .split('/')
                    .next_back()
                    .unwrap_or(model)
                    .starts_with("gpt-oss") =>
            {
                &[Default, Low, Medium, High]
            }
            Provider::Ollama => &[Default, None, Low, Medium, High, Max],
        }
    }

    /// A selection retained from another provider must not send invalid values.
    pub fn for_model(self, provider: Provider, model: &str) -> Self {
        if Self::choices(provider, model).contains(&self) {
            self
        } else {
            Self::Default
        }
    }

    pub fn cycled(self, provider: Provider, model: &str, delta: i64) -> Self {
        let choices = Self::choices(provider, model);
        let ix = choices.iter().position(|v| *v == self).unwrap_or(0) as i64;
        choices[(ix + delta.rem_euclid(choices.len() as i64)).rem_euclid(choices.len() as i64)
            as usize]
    }
}

/// Clones share changes with a worker; `Config::for_session` makes a fresh handle.
#[derive(Clone, Debug)]
pub struct EffortHandle(NumHandle);

impl EffortHandle {
    pub fn new(effort: Effort) -> Self {
        Self(NumHandle::new(effort as u64))
    }

    pub fn get(&self) -> Effort {
        match self.0.get() {
            1 => Effort::None,
            2 => Effort::Minimal,
            3 => Effort::Low,
            4 => Effort::Medium,
            5 => Effort::High,
            6 => Effort::Xhigh,
            7 => Effort::Max,
            _ => Effort::Default,
        }
    }

    pub fn set(&self, effort: Effort) {
        self.0.set(effort as u64);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{Config, FileConfig, SettingId, merge, saved::Saved};

    #[test]
    fn settings_cycle_persist_and_reset_to_provider_default() {
        let mut cfg = Config::for_tests();
        cfg.provider = Provider::Openai;
        let worker = cfg.clone();
        let other_session = cfg.for_session();
        SettingId::Effort.adjust(&mut cfg, -1);
        assert_eq!(worker.effort.get(), Effort::Xhigh);
        assert_eq!(other_session.effort.get(), Effort::Default);

        let mut saved = Saved::default();
        SettingId::Effort.save_into(&cfg, &mut saved);
        let json = serde_json::to_string(&saved).unwrap();
        assert!(json.contains("\"effort\":\"xhigh\""));
        let saved: Saved = serde_json::from_str(&json).unwrap();
        let mut restored = Config::for_tests();
        saved.apply(&mut restored);
        assert_eq!(restored.effort.get(), Effort::Xhigh);

        SettingId::Effort.adjust(&mut cfg, 1);
        assert_eq!(worker.effort.get(), Effort::Default);
        let mut reset = Saved::default();
        SettingId::Effort.save_into(&cfg, &mut reset);
        reset.apply(&mut restored);
        assert_eq!(restored.effort.get(), Effort::Default);
    }

    #[test]
    fn config_overlay_validates_effort_and_preserves_untouched_defaults() {
        let global: FileConfig = toml::from_str("effort = 'high'").unwrap();
        let project: FileConfig = toml::from_str("effort = 'low'").unwrap();
        assert_eq!(merge(global.clone(), project).effort, Some(Effort::Low));
        assert_eq!(
            merge(global, FileConfig::default()).effort,
            Some(Effort::High)
        );
        assert!(toml::from_str::<FileConfig>("effort = 'invalid'").is_err());
        let mut cfg = Config::for_tests();
        cfg.effort.set(Effort::High);
        serde_json::from_str::<Saved>("{}").unwrap().apply(&mut cfg);
        assert_eq!(cfg.effort.get(), Effort::High);
    }

    #[test]
    fn provider_switches_never_send_incompatible_effort_values() {
        assert_eq!(
            Effort::Xhigh.for_model(Provider::Ollama, "qwen3"),
            Effort::Default
        );
        assert_eq!(
            Effort::None.for_model(Provider::Anthropic, "claude"),
            Effort::Default
        );
        assert_eq!(
            Effort::Max.for_model(Provider::Ollama, "gpt-oss:20b"),
            Effort::Default
        );
        assert_eq!(
            Effort::Default.cycled(Provider::Ollama, "gpt-oss:20b", -1),
            Effort::High
        );
        assert_eq!(
            Effort::High.cycled(Provider::Ollama, "gpt-oss:20b", 1),
            Effort::Default
        );
    }
}
