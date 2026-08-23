//! Color-theme presets for the GUI.
//!
//! gpui-component's `ThemeRegistry` loads Zed-format theme sets from a
//! watched directory, so the bundled themes (see `themes/README.md`) are
//! written to `$XDG_DATA_HOME/picocode/themes/` at startup and loaded
//! from there. A *family* (Catppuccin, Gruvbox, …) names a light and a
//! dark variant together: picking one swaps the palette while the
//! existing appearance setting (system / light / dark) keeps deciding
//! which of the two is showing — the two settings stay orthogonal.

use gpui::App;
use gpui_component::{Theme, ThemeMode, ThemeRegistry};
use serde::{Deserialize, Serialize};

/// Which appearance the window follows. Orthogonal to the color-theme
/// family: this picks light or dark, the family picks the palette.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ThemeSetting {
    System,
    Light,
    Dark,
}

/// Keys the GUI keeps in the shared settings file's `ui` section.
pub const THEME_KEY: &str = "theme";
pub const FAMILY_KEY: &str = "theme_family";
pub const SIDEBAR_KEY: &str = "sidebar";

/// The bundled theme files, written to the themes directory at startup
/// (overwriting, so upgrades refresh them; other files are left alone).
const BUNDLED: &[(&str, &str)] = &[
    ("ayu.json", include_str!("../themes/ayu.json")),
    ("catppuccin.json", include_str!("../themes/catppuccin.json")),
    ("everforest.json", include_str!("../themes/everforest.json")),
    ("flexoki.json", include_str!("../themes/flexoki.json")),
    ("github.json", include_str!("../themes/github.json")),
    ("gruvbox.json", include_str!("../themes/gruvbox.json")),
    ("one.json", include_str!("../themes/one.json")),
    ("solarized.json", include_str!("../themes/solarized.json")),
];

/// The selectable families: (label, light theme name, dark theme name).
/// Catppuccin bundles several dark variants; Mocha is its canonical dark.
pub const FAMILIES: &[(&str, &str, &str)] = &[
    ("Default", "Default Light", "Default Dark"),
    ("Ayu", "Ayu Light", "Ayu Dark"),
    ("Catppuccin", "Catppuccin Latte", "Catppuccin Mocha"),
    ("Everforest", "Everforest Light", "Everforest Dark"),
    ("Flexoki", "Flexoki Light", "Flexoki Dark"),
    ("GitHub", "GitHub Light", "GitHub Dark"),
    ("Gruvbox", "Gruvbox Light", "Gruvbox Dark"),
    ("One", "One Light", "One Dark"),
    ("Solarized", "Solarized Light", "Solarized Dark"),
];

/// The family neighbouring `current` in the `/config` cycle (wraps).
pub fn cycled(current: &str, delta: i64) -> &'static str {
    let ix = FAMILIES
        .iter()
        .position(|(label, ..)| label.eq_ignore_ascii_case(current))
        .unwrap_or(0) as i64;
    let n = FAMILIES.len() as i64;
    FAMILIES[((ix + delta).rem_euclid(n)) as usize].0
}

fn themes_dir() -> Option<std::path::PathBuf> {
    Some(picocode_core::session::data_dir()?.join("picocode/themes"))
}

/// Write the bundled themes out and start the registry watching the
/// directory; once loaded, the saved family and appearance are applied.
/// Called from `main` right after `gpui_component::init`.
pub fn init(cx: &mut App) {
    let Some(dir) = themes_dir() else { return };
    if std::fs::create_dir_all(&dir).is_ok() {
        for (name, content) in BUNDLED {
            let _ = std::fs::write(dir.join(name), content);
        }
    }
    let saved = picocode_core::config::saved::load();
    let family: String = saved.ui(FAMILY_KEY).unwrap_or_default();
    let pref = saved.ui(THEME_KEY).unwrap_or(ThemeSetting::System);
    // watch_dir loads asynchronously; the callback runs once the themes
    // are in the registry (and file edits hot-reload via the registry's
    // own observer).
    let _ = ThemeRegistry::watch_dir(dir, cx, move |cx| {
        apply_family(&family, cx);
        apply_mode(pref, cx);
        cx.refresh_windows();
    });
}

/// Point the active theme's light/dark pair at `family`'s variants (an
/// unknown family or a not-yet-loaded registry falls back to the
/// default pair) and re-render. The current appearance mode is kept.
pub fn apply_family(family: &str, cx: &mut App) {
    let (_, light, dark) = FAMILIES
        .iter()
        .find(|(label, ..)| label.eq_ignore_ascii_case(family))
        .unwrap_or(&FAMILIES[0]);
    let registry = ThemeRegistry::global(cx);
    let light = registry.themes().get(*light).cloned();
    let dark = registry.themes().get(*dark).cloned();
    let theme = Theme::global_mut(cx);
    if let Some(light) = light {
        theme.light_theme = light;
    }
    if let Some(dark) = dark {
        theme.dark_theme = dark;
    }
    // Re-apply the current mode so the swapped pair takes effect now.
    Theme::change(theme.mode, None, cx);
    cx.refresh_windows();
}

/// Apply the appearance preference (which of the pair is showing).
pub fn apply_mode(pref: ThemeSetting, cx: &mut App) {
    match pref {
        ThemeSetting::System => Theme::sync_system_appearance(None, cx),
        ThemeSetting::Light => Theme::change(ThemeMode::Light, None, cx),
        ThemeSetting::Dark => Theme::change(ThemeMode::Dark, None, cx),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn family_cycle_wraps_and_ignores_case() {
        assert_eq!(cycled("Default", 1), "Ayu");
        assert_eq!(cycled("default", -1), "Solarized");
        assert_eq!(cycled("Solarized", 1), "Default");
        // Unknown names restart from the head of the list.
        assert_eq!(cycled("", 1), "Ayu");
    }

    #[test]
    fn bundled_files_parse_and_cover_every_family() {
        let mut names = Vec::new();
        for (file, content) in BUNDLED {
            // Through the real schema, so a key drift fails here instead of
            // being silently ignored by the registry at startup.
            let set: gpui_component::ThemeSet =
                serde_json::from_str(content).unwrap_or_else(|e| panic!("{file}: {e}"));
            for theme in set.themes {
                assert!(
                    theme.highlight.is_some(),
                    "{}: {} lacks highlight styles",
                    file,
                    theme.name
                );
                names.push(theme.name.to_string());
            }
        }
        // Every non-default family variant exists in some bundled file.
        for (label, light, dark) in FAMILIES.iter().skip(1) {
            assert!(names.iter().any(|n| n == light), "{label}: {light}");
            assert!(names.iter().any(|n| n == dark), "{label}: {dark}");
        }
    }
}
