//! Settings remembered across runs.
//!
//! Values changed in the `/config` dialog are saved to
//! `$XDG_DATA_HOME/picocode/settings.json` and re-applied on the next
//! start, by whichever front end starts next — the TUI and the GUI share
//! the file, since they share the dialog. It is a sparse overlay: only
//! settings actually touched are stored, so anything untouched keeps
//! following `picocode.toml` (a stored value wins over a later config-file
//! edit — it was chosen more recently).
//!
//! Two settings are deliberately absent. The permission mode is not
//! persisted: starting in bypass silently would be a trap. The model, and
//! with it the context window, is remembered per project by
//! [`crate::state`] instead — a window that fits one model is wrong for the
//! next, so it belongs to the selection rather than to the user.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::config::{Language, NumHandle, SearchProvider};
use crate::keys::SubmitKey;

#[derive(Debug, Default, Clone, PartialEq, Serialize, Deserialize)]
pub struct Saved {
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
    /// Interface language; absent means follow the OS.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub language: Option<Language>,
    /// Front-end preferences, stored verbatim under a key of the front
    /// end's choosing (the GUI's theme and sidebar, the TUI's reasoning
    /// toggle). Core never interprets them; they live here so one file
    /// holds everything `/config` can change.
    #[serde(default, skip_serializing_if = "serde_json::Map::is_empty")]
    pub ui: serde_json::Map<String, serde_json::Value>,
}

/// Where both front ends remember the raw-transcript view. The flag is
/// view state rather than configuration, so it lives in [`Saved::ui`]
/// rather than in [`crate::config::Config`] — but both front ends have the
/// toggle, so the key is spelled once, here.
pub const RAW_VIEW_KEY: &str = "raw_view";

impl Saved {
    /// Read a front-end preference, or `None` if it was never stored (or
    /// was stored by an older version as a different shape).
    pub fn ui<T: serde::de::DeserializeOwned>(&self, key: &str) -> Option<T> {
        serde_json::from_value(self.ui.get(key)?.clone()).ok()
    }

    /// Store a front-end preference. A value that fails to serialize is
    /// dropped rather than corrupting the file.
    pub fn set_ui<T: Serialize>(&mut self, key: &str, value: T) {
        if let Ok(v) = serde_json::to_value(value) {
            self.ui.insert(key.to_string(), v);
        }
    }

    /// Overlay the stored values onto a freshly parsed config. Called once
    /// at startup, after `Config::from_args` and before the worker starts.
    pub fn apply(&self, cfg: &mut crate::config::Config) {
        let set = |handle: &NumHandle, v: Option<u64>| {
            if let Some(v) = v {
                handle.set(v);
            }
        };
        set(&cfg.bash_timeout, self.bash_timeout);
        set(&cfg.read_max_lines, self.read_max_lines);
        set(&cfg.read_max_line_bytes, self.read_max_line_bytes);
        set(&cfg.auto_compact, self.auto_compact);
        set(&cfg.max_tokens, self.max_tokens);
        if let Some(key) = self.submit_key {
            cfg.submit_key = key;
        }
        // Unconditional: `System` still has to be applied, since the front
        // end may have been started with the locale left at the fallback.
        cfg.language = self.language.unwrap_or_default();
        cfg.language.apply();
        // A provider whose requirements are no longer met (a dropped
        // BRAVE_API_KEY) is ignored rather than restored into a dead state.
        if let Some(p) = self.search_provider {
            cfg.search.set_provider(p);
        }
        if let Some(n) = self.search_max_results {
            cfg.search.set_max_results(n);
        }
    }
}

fn path() -> Option<PathBuf> {
    Some(crate::session::data_dir()?.join("picocode/settings.json"))
}

/// Where the GUI kept these before the TUI shared them. Read once, when
/// there is no `settings.json` yet, so an upgrade keeps the user's choices.
fn legacy_path() -> Option<PathBuf> {
    Some(crate::session::data_dir()?.join("picocode/gui-settings.json"))
}

/// The GUI-only shape of the file, whose front-end keys were flat.
#[derive(Deserialize)]
struct Legacy {
    #[serde(flatten)]
    shared: Saved,
    #[serde(default)]
    theme: Option<serde_json::Value>,
    #[serde(default)]
    theme_family: Option<serde_json::Value>,
    #[serde(default)]
    sidebar: Option<serde_json::Value>,
}

fn read(path: &std::path::Path) -> Option<String> {
    std::fs::read_to_string(path).ok()
}

/// Load the saved settings; a missing or unreadable file is just defaults.
pub fn load() -> Saved {
    let Some(path) = path() else {
        return Saved::default();
    };
    if let Some(text) = read(&path) {
        return serde_json::from_str(&text).unwrap_or_default();
    }
    legacy_path()
        .and_then(|p| read(&p))
        .and_then(|text| serde_json::from_str::<Legacy>(&text).ok())
        .map(migrate)
        .unwrap_or_default()
}

fn migrate(legacy: Legacy) -> Saved {
    let mut saved = legacy.shared;
    for (key, value) in [
        ("theme", legacy.theme),
        ("theme_family", legacy.theme_family),
        ("sidebar", legacy.sidebar),
    ] {
        if let Some(v) = value {
            saved.ui.insert(key.to_string(), v);
        }
    }
    saved
}

/// Save the settings (best-effort; the running session works either way).
pub fn save(saved: &Saved) {
    let Some(path) = path() else {
        return;
    };
    let Some(dir) = path.parent() else {
        return;
    };
    let _ = std::fs::create_dir_all(dir);
    if let Ok(json) = serde_json::to_vec_pretty(saved) {
        let _ = std::fs::write(&path, json);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrips_and_stays_sparse() {
        let mut s = Saved {
            bash_timeout: Some(120),
            ..Default::default()
        };
        s.set_ui("theme", "dark");
        let json = serde_json::to_string(&s).unwrap();
        // Untouched settings are absent, not null.
        assert!(!json.contains("read_max_lines"), "{json}");
        let back: Saved = serde_json::from_str(&json).unwrap();
        assert_eq!(back, s);
        assert_eq!(back.ui::<String>("theme").as_deref(), Some("dark"));
        assert_eq!(back.ui::<String>("never_set"), None);

        s.search_provider = Some(SearchProvider::Brave);
        let json = serde_json::to_string(&s).unwrap();
        assert!(json.contains("\"brave\""), "{json}");

        // Unknown fields — from a newer version, or removed ones like the
        // old max_turns — don't break loading.
        let back: Saved =
            serde_json::from_str("{\"bash_timeout\":60,\"max_turns\":30,\"future_field\":1}")
                .unwrap();
        assert_eq!(back.bash_timeout, Some(60));
    }

    #[test]
    fn a_wrongly_typed_ui_value_reads_as_absent() {
        let mut s = Saved::default();
        s.set_ui("sidebar", "yes");
        assert_eq!(s.ui::<bool>("sidebar"), None);
    }

    #[test]
    fn the_gui_only_file_migrates_into_the_shared_shape() {
        let legacy: Legacy = serde_json::from_str(
            r#"{"theme":"dark","theme_family":"ayu","sidebar":true,"bash_timeout":300}"#,
        )
        .unwrap();
        let saved = migrate(legacy);
        assert_eq!(saved.bash_timeout, Some(300));
        assert_eq!(saved.ui::<String>("theme").as_deref(), Some("dark"));
        assert_eq!(saved.ui::<String>("theme_family").as_deref(), Some("ayu"));
        assert_eq!(saved.ui::<bool>("sidebar"), Some(true));
    }

    #[test]
    fn applying_only_overrides_what_was_stored() {
        let mut cfg = crate::config::Config::for_tests();
        let before_lines = cfg.read_max_lines.get();
        Saved {
            bash_timeout: Some(600),
            // Pinned so this doesn't retune the process-global locale to
            // whatever the machine running the tests is set to; the rest
            // of this binary reads it.
            language: Some(Language::En),
            ..Default::default()
        }
        .apply(&mut cfg);
        assert_eq!(cfg.bash_timeout.get(), 600);
        assert_eq!(cfg.read_max_lines.get(), before_lines);
    }
}
