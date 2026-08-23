//! The `/config` registry: one table both front ends render.
//!
//! A row is a [`SettingId`], not a position. Its label, its current value,
//! what `←`/`→` does to it and what gets persisted all hang off the same
//! enum, so adding a setting is one variant plus its arms — no front end
//! has to be told where it sits, and no `match` arm can drift out of step
//! with the row above it.
//!
//! Rows that only one front end has (the GUI's theme pickers, the TUI's
//! reasoning toggle) stay in that front end and are rendered around
//! [`SettingId::SHARED`]; [`Group`] says where they belong.

use rust_i18n::t;

use super::Config;
use super::saved::Saved;

/// Section heading a setting is shown under.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum Group {
    /// The model and the size of what is sent to it.
    Model,
    /// What the agent may do and how much each tool returns.
    Tools,
    /// Front-end preferences that never reach the model.
    Interface,
}

impl Group {
    /// Display order of the sections.
    pub const ALL: [Group; 3] = [Group::Model, Group::Tools, Group::Interface];

    pub fn label(self) -> String {
        match self {
            Group::Model => t!("group_model"),
            Group::Tools => t!("group_tools"),
            Group::Interface => t!("group_interface"),
        }
        .to_string()
    }
}

/// One row of the `/config` dialog that both front ends have.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum SettingId {
    Model,
    ContextWindow,
    MaxTokens,
    AutoCompact,
    Mode,
    BashTimeout,
    ReadLines,
    LineBytes,
    SearchProvider,
    SearchResults,
    SendKey,
}

impl SettingId {
    /// Every shared row, in display order (grouped, so a front end can
    /// render [`Group::ALL`] and filter by [`SettingId::group`]).
    pub const SHARED: [SettingId; 11] = [
        SettingId::Model,
        SettingId::ContextWindow,
        SettingId::MaxTokens,
        SettingId::AutoCompact,
        SettingId::Mode,
        SettingId::BashTimeout,
        SettingId::ReadLines,
        SettingId::LineBytes,
        SettingId::SearchProvider,
        SettingId::SearchResults,
        SettingId::SendKey,
    ];

    pub fn group(self) -> Group {
        match self {
            SettingId::Model
            | SettingId::ContextWindow
            | SettingId::MaxTokens
            | SettingId::AutoCompact => Group::Model,
            SettingId::Mode
            | SettingId::BashTimeout
            | SettingId::ReadLines
            | SettingId::LineBytes
            | SettingId::SearchProvider
            | SettingId::SearchResults => Group::Tools,
            SettingId::SendKey => Group::Interface,
        }
    }

    pub fn label(self) -> String {
        match self {
            SettingId::Model => t!("set_model"),
            SettingId::ContextWindow => t!("set_context_window"),
            SettingId::MaxTokens => t!("set_max_tokens"),
            SettingId::AutoCompact => t!("set_auto_compact"),
            SettingId::Mode => t!("set_mode"),
            SettingId::BashTimeout => t!("set_bash_timeout"),
            SettingId::ReadLines => t!("set_read_lines"),
            SettingId::LineBytes => t!("set_line_bytes"),
            SettingId::SearchProvider => t!("set_web_search"),
            SettingId::SearchResults => t!("set_results"),
            SettingId::SendKey => t!("set_send_key"),
        }
        .to_string()
    }

    /// The row's current value, formatted for display. The send-key row is
    /// the one a front end may want to override: a terminal that cannot
    /// report the chosen combination says so next to it.
    pub fn value(self, cfg: &Config) -> String {
        match self {
            SettingId::Model => cfg.model_label(),
            SettingId::ContextWindow => human_count(cfg.context_window.get()),
            SettingId::MaxTokens => match cfg.max_tokens.get() {
                0 => t!("val_max_tokens_off").to_string(),
                n => human_count(n),
            },
            SettingId::AutoCompact => match cfg.auto_compact.get() {
                0 => t!("val_off").to_string(),
                pct => format!("{pct}%"),
            },
            SettingId::Mode => cfg.mode.get().label().to_string(),
            SettingId::BashTimeout => t!("val_seconds", n = cfg.bash_timeout.get()).to_string(),
            SettingId::ReadLines => human_count(cfg.read_max_lines.get()),
            SettingId::LineBytes => human_count(cfg.read_max_line_bytes.get()),
            SettingId::SearchProvider => cfg.search.snapshot().provider.label().to_string(),
            SettingId::SearchResults => cfg.search.snapshot().max_results.to_string(),
            SettingId::SendKey => cfg.submit_key.label().to_string(),
        }
    }

    /// True for a row that opens something instead of holding a value —
    /// the front end handles it (the model row opens the model picker) and
    /// [`SettingId::adjust`] does nothing.
    pub fn is_action(self) -> bool {
        self == SettingId::Model
    }

    /// `←`/`→` on the row. Every change applies immediately; a front end
    /// with side effects of its own (rebinding the send key, rebuilding the
    /// worker's agents) does them after this returns.
    pub fn adjust(self, cfg: &mut Config, delta: i64) {
        match self {
            // Handled by the front end: it opens the model picker.
            SettingId::Model => {}
            SettingId::ContextWindow => cfg.step_context_window(delta),
            SettingId::MaxTokens => cfg.step_max_tokens(delta),
            SettingId::AutoCompact => cfg.step_auto_compact(delta),
            // Same cycle as the TUI's Shift+Tab; bypass stays
            // command-only, and adjusting away from it lands on read-only.
            SettingId::Mode => cfg.mode.set(cfg.mode.get().cycled(delta)),
            SettingId::BashTimeout => cfg.step_bash_timeout(delta),
            SettingId::ReadLines => cfg.step_read_lines(delta),
            SettingId::LineBytes => cfg.step_line_bytes(delta),
            SettingId::SearchProvider => cfg.search.cycle_provider(delta),
            SettingId::SearchResults => cfg.search.step_max_results(delta),
            SettingId::SendKey => cfg.submit_key = cfg.submit_key.cycled(delta),
        }
    }

    /// Whether the change belongs to the model selection rather than to the
    /// user, and so is remembered by [`crate::state`] for this project
    /// instead of in the shared [`Saved`] overlay.
    pub fn is_per_selection(self) -> bool {
        self == SettingId::ContextWindow
    }

    /// Copy the new value into the persisted overlay, for the settings that
    /// are remembered across runs. The mode is deliberately not persisted
    /// (starting in bypass silently would be a trap), the model is already
    /// remembered per project by [`crate::state`], and the context window
    /// rides along with the model there — see [`Saved`].
    pub fn save_into(self, cfg: &Config, saved: &mut Saved) {
        match self {
            SettingId::MaxTokens => saved.max_tokens = Some(cfg.max_tokens.get()),
            SettingId::AutoCompact => saved.auto_compact = Some(cfg.auto_compact.get()),
            SettingId::BashTimeout => saved.bash_timeout = Some(cfg.bash_timeout.get()),
            SettingId::ReadLines => saved.read_max_lines = Some(cfg.read_max_lines.get()),
            SettingId::LineBytes => saved.read_max_line_bytes = Some(cfg.read_max_line_bytes.get()),
            SettingId::SearchProvider => {
                saved.search_provider = Some(cfg.search.snapshot().provider)
            }
            SettingId::SearchResults => {
                saved.search_max_results = Some(cfg.search.snapshot().max_results)
            }
            SettingId::SendKey => saved.submit_key = Some(cfg.submit_key),
            SettingId::Model | SettingId::Mode | SettingId::ContextWindow => {}
        }
    }
}

/// A `/config` count the way people say it: `32768` → `32k`, `262144` →
/// `256k`, `200000` → `200k`, `1048576` → `1M`, `1500` → `1.5k`. Digits in
/// a row are hard to compare at a glance, and none of these settings is
/// discussed at single-unit precision.
///
/// The divisor follows the number: 1024 when it divides evenly, 1000
/// otherwise. That is for the context window, where model sizes come in
/// both flavours — Ollama's are powers of two and named for them (262144
/// is a 256k model), a hosted API's are round decimals. The line and byte
/// limits step in hundreds, so they never meet the 1024 case and simply
/// read as thousands. Under 1000 the figure is short enough as it is.
pub fn human_count(n: u64) -> String {
    const K: u64 = 1024;
    const M: u64 = K * K;
    let exact = |unit: u64, suffix: &str| {
        (n >= unit && n.is_multiple_of(unit)).then(|| format!("{}{suffix}", n / unit))
    };
    exact(M, "M")
        .or_else(|| exact(1_000_000, "M"))
        .or_else(|| exact(K, "k"))
        .or_else(|| {
            (n >= 1000).then(|| {
                let k = format!("{:.1}", n as f64 / 1000.0);
                format!("{}k", k.trim_end_matches(".0"))
            })
        })
        .unwrap_or_else(|| n.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn token_counts_read_the_way_models_are_named() {
        // Powers of two divide by 1024, which is how these windows are
        // quoted: a 262144-token model is a 256k model, not a 262.1k one.
        assert_eq!(human_count(2048), "2k");
        assert_eq!(human_count(8192), "8k");
        assert_eq!(human_count(32_768), "32k");
        assert_eq!(human_count(40_960), "40k");
        assert_eq!(human_count(131_072), "128k");
        assert_eq!(human_count(262_144), "256k");
        assert_eq!(human_count(1_048_576), "1M");

        // A hosted API's round decimals keep their own shape.
        assert_eq!(human_count(200_000), "200k");
        assert_eq!(human_count(500_000), "500k");
        assert_eq!(human_count(1_000_000), "1M");

        // Anything else gets one decimal, with a bare .0 trimmed.
        assert_eq!(human_count(1500), "1.5k");
        assert_eq!(human_count(45_000), "45k");
        assert_eq!(human_count(33_000), "33k");

        // Small enough to read as it is.
        assert_eq!(human_count(0), "0");
        assert_eq!(human_count(512), "512");
        assert_eq!(human_count(999), "999");
    }

    /// The line and byte limits step in hundreds, so the 1024 rule never
    /// fires for them and every reachable value reads as thousands.
    #[test]
    fn every_read_limit_the_stepper_reaches_abbreviates_cleanly() {
        let cfg = Config::for_tests();
        let mut seen = Vec::new();
        for _ in 0..40 {
            cfg.step_read_lines(-1);
        }
        for _ in 0..40 {
            seen.push(SettingId::ReadLines.value(&cfg));
            cfg.step_read_lines(1);
        }
        seen.dedup();
        assert_eq!(
            seen,
            [
                "500", "1k", "1.5k", "2k", "2.5k", "3k", "3.5k", "4k", "4.5k", "5k", "5.5k", "6k",
                "6.5k", "7k", "7.5k", "8k", "8.5k", "9k", "9.5k", "10k"
            ]
        );

        let mut seen = Vec::new();
        for _ in 0..60 {
            cfg.step_line_bytes(-1);
        }
        for _ in 0..60 {
            seen.push(SettingId::LineBytes.value(&cfg));
            cfg.step_line_bytes(1);
        }
        seen.dedup();
        assert_eq!(seen.first().map(String::as_str), Some("100"));
        assert_eq!(seen.last().map(String::as_str), Some("5k"));
        assert!(seen.contains(&"900".to_string()));
        assert!(seen.contains(&"1k".to_string()), "{seen:?}");
        assert!(seen.contains(&"1.1k".to_string()), "{seen:?}");
    }

    #[test]
    fn the_window_row_shows_the_value_alone() {
        // The provider's ceiling bounds the stepper; it is not spelled out
        // in the row, which would double the width of the longest value.
        let mut cfg = Config::for_tests();
        cfg.adopt_model_window(Some(32_768));
        cfg.apply_context_limit(crate::config::Provider::Ollama, 262_144);
        assert_eq!(SettingId::ContextWindow.value(&cfg), "32k");
    }

    #[test]
    fn every_shared_setting_is_grouped_and_labelled() {
        for id in SettingId::SHARED {
            assert!(!id.label().is_empty(), "{id:?} has no label");
            assert!(
                Group::ALL.contains(&id.group()),
                "{id:?} is in no rendered group"
            );
        }
    }

    #[test]
    fn shared_rows_are_listed_in_group_order() {
        // A front end renders `Group::ALL` and filters, so a stray row
        // would silently jump sections; keep SHARED sorted by group.
        let order: Vec<usize> = SettingId::SHARED
            .iter()
            .map(|id| {
                Group::ALL
                    .iter()
                    .position(|g| *g == id.group())
                    .expect("grouped")
            })
            .collect();
        assert!(
            order.windows(2).all(|w| w[0] <= w[1]),
            "SHARED is out of group order: {order:?}"
        );
    }

    #[test]
    fn shared_rows_are_unique() {
        let mut seen = std::collections::HashSet::new();
        for id in SettingId::SHARED {
            assert!(seen.insert(id), "{id:?} listed twice");
        }
    }

    #[test]
    fn adjusting_reports_a_new_value() {
        let mut cfg = Config::for_tests();
        for id in SettingId::SHARED {
            // The model row opens a picker rather than holding a value,
            // and the search provider has nowhere to step in a bare config
            // (searxng needs a base_url, brave a BRAVE_API_KEY).
            if id.is_action() || id == SettingId::SearchProvider {
                continue;
            }
            let before = id.value(&cfg);
            id.adjust(&mut cfg, 1);
            let after = id.value(&cfg);
            assert_ne!(before, after, "{id:?} ignored a → step");
        }
    }
}
