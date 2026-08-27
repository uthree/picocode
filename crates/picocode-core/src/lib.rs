//! picocode-core: the UI-independent heart of picocode.
//!
//! Everything a front end needs to run the agent lives here — configuration
//! and permission rules, the rig agent worker, the built-in tools, the
//! approval hook, and session/state persistence. Front ends (the TUI, or a
//! future GUI) talk to it exclusively through [`event::AgentEvent`] /
//! [`event::WorkerCmd`] channels and the plain data types in [`transcript`];
//! nothing in this crate depends on a rendering library.

// Strings the front ends render verbatim (the `/config` rows) live in
// locales/{en,ja}.yml here rather than in each front end, so the TUI and
// the GUI show the same wording. rust-i18n's locale is process-global and
// each crate keeps its own catalog, so a front end's `t!` still resolves
// against its own locales/ — see `set_locale_from_system`.
rust_i18n::i18n!("locales", fallback = "en");

/// Follow the OS language preference, before anything is rendered. Called
/// by both front ends at startup, ahead of reading the saved settings —
/// `/config`'s language row may pin a different one, which
/// [`config::saved::Saved::apply`] then does.
pub fn set_locale_from_system() {
    config::Language::System.apply();
}

pub mod agent;
pub mod approval;
pub mod attachment;
pub mod backend;
pub mod clipboard;
pub mod command;
pub mod config;
pub mod context;
pub mod event;
pub mod git;
pub mod history;
pub mod keys;
pub mod mcp;
pub mod media;
pub mod models;
pub mod report;
pub mod sandbox;
pub mod session;
pub mod speed;
pub mod state;
pub mod steer;
pub mod tools;
pub mod transcript;
pub mod undo;
pub mod workspace;
