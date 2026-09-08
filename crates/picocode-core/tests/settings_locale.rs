//! The `/config` value formats in Japanese.
//!
//! The locale is process-global, so this lives in its own test binary. A
//! missing key falls back to English rather than failing, which is exactly
//! the kind of gap a formatted value hides — `1m30s` in a Japanese dialog
//! looks deliberate.

use picocode_core::config::{
    Language, SettingId, human_count, human_seconds, on_off, raw_view_label,
};

#[test]
fn durations_and_counts_speak_japanese() {
    Language::Ja.apply();

    assert_eq!(human_seconds(30), "30秒");
    assert_eq!(human_seconds(120), "2分");
    assert_eq!(human_seconds(90), "1分30秒");
    assert_eq!(human_seconds(1800), "30分");

    // Counts carry no words, so they read the same in either language.
    assert_eq!(human_count(32_768), "32k");
    assert_eq!(human_count(20), "20");

    // The row that got us here, and the one value on it that is a word.
    assert_eq!(SettingId::Language.label(), "言語");
    assert_eq!(SettingId::Effort.label(), "推論量 (Effort)");
    assert_eq!(
        picocode_core::config::Effort::Default.label(),
        "プロバイダー既定"
    );
    assert_eq!(Language::System.label(), "システム");

    // The raw-transcript row lives in the front ends but is named here:
    // `t!` only ever reads its own crate's catalog, so a front end asking
    // for the key directly would get the key back.
    assert_eq!(raw_view_label(), "生ログ表示");
    assert_eq!(on_off(true), "オン");
    assert_eq!(on_off(false), "オフ");

    // Language names stay in their own language: picking Japanese from an
    // English dialog means finding 日本語, not "Japanese".
    Language::En.apply();
    assert_eq!(SettingId::Language.label(), "language");
    assert_eq!(Language::Ja.label(), "日本語");
    assert_eq!(Language::En.label(), "English");
    assert_eq!(human_seconds(90), "1m30s");
    assert_eq!(raw_view_label(), "raw transcript");
    assert_eq!(on_off(true), "on");
}
