//! The interface language.
//!
//! Both front ends render their `/config` labels through rust-i18n, whose
//! locale is process-global, so one setting steers the whole app. The
//! default follows the OS; the point of the row is that a machine set to
//! English can still be told to speak Japanese, which is otherwise only
//! reachable by changing the system language.

use rust_i18n::t;
use serde::{Deserialize, Serialize};

/// What `/config`'s language row is set to.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Language {
    /// Follow the OS preference — what an untouched install does.
    #[default]
    System,
    En,
    Ja,
}

impl Language {
    /// Cycle order of the row.
    pub const ALL: [Language; 3] = [Language::System, Language::En, Language::Ja];

    /// The row's value. A language is named in itself: someone looking for
    /// Japanese has to find it in an English dialog, and "Japanese" is not
    /// what they are looking for. Only `System` is translated, since it
    /// names a behaviour rather than a language.
    pub fn label(self) -> String {
        match self {
            Language::System => t!("val_language_system").to_string(),
            Language::En => "English".to_string(),
            Language::Ja => "日本語".to_string(),
        }
    }

    /// The locale to render in, with `System` resolved against the OS.
    pub fn locale(self) -> &'static str {
        match self {
            Language::System => system_locale(),
            Language::En => "en",
            Language::Ja => "ja",
        }
    }

    pub fn cycled(self, delta: i64) -> Language {
        let at = Language::ALL.iter().position(|l| *l == self).unwrap_or(0) as i64;
        let n = Language::ALL.len() as i64;
        Language::ALL[(at + delta).rem_euclid(n) as usize]
    }

    /// Render in this language from now on. Strings already emitted (a
    /// notice in the transcript) keep the wording they were written with;
    /// both dialogs rebuild their rows every frame.
    pub fn apply(self) {
        rust_i18n::set_locale(self.locale());
    }
}

/// The OS language preference, narrowed to what is translated. sys-locale
/// reads the preference rather than `$LANG`, so it works for a
/// Finder-launched app too.
fn system_locale() -> &'static str {
    match sys_locale::get_locale() {
        Some(locale) if locale.starts_with("ja") => "ja",
        _ => "en",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_row_cycles_both_ways_and_wraps() {
        assert_eq!(Language::System.cycled(1), Language::En);
        assert_eq!(Language::En.cycled(1), Language::Ja);
        assert_eq!(Language::Ja.cycled(1), Language::System);
        assert_eq!(Language::System.cycled(-1), Language::Ja);
    }

    #[test]
    fn system_resolves_to_a_locale_that_exists() {
        // Whatever the machine running this is set to, `System` has to land
        // on a catalog we ship — an untranslated OS language falls back to
        // English rather than to a locale with no strings in it.
        assert!(matches!(Language::System.locale(), "en" | "ja"));
    }
}
