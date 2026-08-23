//! The `/config` value formats in Japanese.
//!
//! The locale is process-global, so this lives in its own test binary. A
//! missing key falls back to English rather than failing, which is exactly
//! the kind of gap a formatted value hides — `1m30s` in a Japanese dialog
//! looks deliberate.

use picocode_core::config::{human_count, human_seconds};

#[test]
fn durations_and_counts_speak_japanese() {
    rust_i18n::set_locale("ja");

    assert_eq!(human_seconds(30), "30秒");
    assert_eq!(human_seconds(120), "2分");
    assert_eq!(human_seconds(90), "1分30秒");
    assert_eq!(human_seconds(1800), "30分");

    // Counts carry no words, so they read the same in either language.
    assert_eq!(human_count(32_768), "32k");
    assert_eq!(human_count(20), "20");
}
