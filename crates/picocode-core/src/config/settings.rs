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
            SettingId::ContextWindow => tokens(cfg.context_window.get()),
            SettingId::MaxTokens => match cfg.max_tokens.get() {
                0 => t!("val_max_tokens_off").to_string(),
                n => tokens(n),
            },
            SettingId::AutoCompact => match cfg.auto_compact.get() {
                0 => t!("val_off").to_string(),
                pct => format!("{pct}%"),
            },
            SettingId::Mode => cfg.mode.get().label().to_string(),
            SettingId::BashTimeout => t!("val_seconds", n = cfg.bash_timeout.get()).to_string(),
            SettingId::ReadLines => cfg.read_max_lines.get().to_string(),
            SettingId::LineBytes => cfg.read_max_line_bytes.get().to_string(),
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

    /// Whether the worker has to rebuild its agents for the change to take
    /// effect: these two travel with the request rather than being read per
    /// use, so the running agents hold the old value.
    pub fn needs_worker_rebuild(self) -> bool {
        matches!(self, SettingId::MaxTokens | SettingId::ContextWindow)
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

fn tokens(n: u64) -> String {
    t!("val_tokens", n = n).to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

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
