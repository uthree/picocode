//! Persisted GUI settings.
//!
//! Values changed in the `/config` dialog are saved to
//! `$XDG_DATA_HOME/picocode/gui-settings.json` and re-applied on the next
//! start. The file is a sparse overlay: only settings the user actually
//! touched are stored, so anything untouched keeps following
//! `picocode.toml` (a stored value wins over a later config-file edit —
//! it was chosen more recently). The permission mode is deliberately not
//! persisted (starting in bypass silently would be a trap), and the model
//! is already remembered per project by the core state.

use std::path::PathBuf;

use picocode_core::config::SearchProvider;
use picocode_core::keys::SubmitKey;
use picocode_core::session;
use serde::{Deserialize, Serialize};

#[derive(Debug, Default, Clone, PartialEq, Serialize, Deserialize)]
pub struct GuiSettings {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub theme: Option<ThemeSetting>,
    /// Color-theme family (see `theme::FAMILIES`), orthogonal to `theme`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub theme_family: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bash_timeout: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub read_max_lines: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub read_max_line_bytes: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub search_provider: Option<SearchProvider>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub search_max_results: Option<usize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auto_compact: Option<u64>,
    /// Cap on the tokens one reply may generate (0 = no cap from picocode).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_tokens: Option<u64>,
    /// Which key sends the message (the rest of the Enter combinations
    /// insert a newline).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub submit_key: Option<SubmitKey>,
    /// Whether the session sidebar is open (remembered across runs).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sidebar: Option<bool>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ThemeSetting {
    System,
    Light,
    Dark,
}

fn path() -> Option<PathBuf> {
    Some(session::data_dir()?.join("picocode/gui-settings.json"))
}

/// Load the saved settings; a missing or unreadable file is just defaults.
pub fn load() -> GuiSettings {
    let Some(path) = path() else {
        return GuiSettings::default();
    };
    std::fs::read_to_string(&path)
        .ok()
        .and_then(|text| serde_json::from_str(&text).ok())
        .unwrap_or_default()
}

/// Save the settings (best-effort; the running session works either way).
pub fn save(settings: &GuiSettings) {
    let Some(path) = path() else {
        return;
    };
    let Some(dir) = path.parent() else {
        return;
    };
    let _ = std::fs::create_dir_all(dir);
    if let Ok(json) = serde_json::to_vec_pretty(settings) {
        let _ = std::fs::write(&path, json);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrips_and_stays_sparse() {
        let mut s = GuiSettings {
            theme: Some(ThemeSetting::Dark),
            bash_timeout: Some(120),
            ..Default::default()
        };
        let json = serde_json::to_string(&s).unwrap();
        // Untouched settings are absent, not null.
        assert!(!json.contains("read_max_lines"), "{json}");
        let back: GuiSettings = serde_json::from_str(&json).unwrap();
        assert_eq!(back, s);

        s.search_provider = Some(SearchProvider::Brave);
        let json = serde_json::to_string(&s).unwrap();
        assert!(json.contains("\"brave\""), "{json}");

        // Unknown fields — from a newer version, or removed ones like the
        // old max_turns — don't break loading.
        let back: GuiSettings =
            serde_json::from_str("{\"bash_timeout\":60,\"max_turns\":30,\"future_field\":1}")
                .unwrap();
        assert_eq!(back.bash_timeout, Some(60));
    }
}
