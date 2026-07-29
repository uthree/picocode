//! Which key sends a message, and which one inserts a newline.
//!
//! Habits differ — chat apps send on Enter, editors send on Ctrl/Cmd+Enter —
//! so the choice is a setting (`submit_key` in the config file, or the
//! `/config` dialog). Whichever key sends, the other Enter combinations
//! insert a newline, so there is always a way to write a multi-line message.

use serde::{Deserialize, Serialize};

/// The key combination that submits the message in the input box.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum SubmitKey {
    #[default]
    Enter,
    ShiftEnter,
    CtrlEnter,
    CmdEnter,
}

/// All choices, in the `/config` cycle order.
pub const SUBMIT_KEYS: &[SubmitKey] = &[
    SubmitKey::Enter,
    SubmitKey::ShiftEnter,
    SubmitKey::CtrlEnter,
    SubmitKey::CmdEnter,
];

impl SubmitKey {
    /// Human-readable name. The platform key is called Cmd on macOS and
    /// Super elsewhere, so the label follows the machine it runs on.
    pub fn label(self) -> &'static str {
        match self {
            SubmitKey::Enter => "Enter",
            SubmitKey::ShiftEnter => "Shift+Enter",
            SubmitKey::CtrlEnter => "Ctrl+Enter",
            SubmitKey::CmdEnter => {
                if cfg!(target_os = "macos") {
                    "Cmd+Enter"
                } else {
                    "Super+Enter"
                }
            }
        }
    }

    /// The key to advertise for inserting a newline: plain Enter whenever a
    /// modified Enter sends, and Shift+Enter when plain Enter sends.
    pub fn newline_label(self) -> &'static str {
        match self {
            SubmitKey::Enter => "Shift+Enter",
            _ => "Enter",
        }
    }

    /// Neighbouring choice in the `/config` dialog (wraps around).
    pub fn cycled(self, delta: i64) -> SubmitKey {
        let ix = SUBMIT_KEYS.iter().position(|k| *k == self).unwrap_or(0) as i64;
        let n = SUBMIT_KEYS.len() as i64;
        SUBMIT_KEYS[((ix + delta).rem_euclid(n)) as usize]
    }

    /// The gpui keystroke this combination is written as (GUI key bindings).
    pub fn keystroke(self) -> &'static str {
        match self {
            SubmitKey::Enter => "enter",
            SubmitKey::ShiftEnter => "shift-enter",
            SubmitKey::CtrlEnter => "ctrl-enter",
            SubmitKey::CmdEnter => "cmd-enter",
        }
    }

    /// The `submit_key` value that selects this combination in the config
    /// file (also what the GUI stores in its settings file).
    pub fn config_name(self) -> &'static str {
        match self {
            SubmitKey::Enter => "enter",
            SubmitKey::ShiftEnter => "shift-enter",
            SubmitKey::CtrlEnter => "ctrl-enter",
            SubmitKey::CmdEnter => "cmd-enter",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cycles_and_names_stay_in_sync() {
        assert_eq!(SubmitKey::default(), SubmitKey::Enter);
        // Cycling wraps in both directions and visits every choice.
        let mut seen = Vec::new();
        let mut key = SubmitKey::Enter;
        for _ in 0..SUBMIT_KEYS.len() {
            seen.push(key);
            key = key.cycled(1);
        }
        assert_eq!(seen, SUBMIT_KEYS);
        assert_eq!(key, SubmitKey::Enter);
        assert_eq!(SubmitKey::Enter.cycled(-1), SubmitKey::CmdEnter);

        // The config name is what serde reads and writes.
        for key in SUBMIT_KEYS {
            let json = serde_json::to_string(key).unwrap();
            assert_eq!(json, format!("\"{}\"", key.config_name()));
            let back: SubmitKey = serde_json::from_str(&json).unwrap();
            assert_eq!(back, *key);
        }

        // Whatever sends, something else inserts a newline.
        for key in SUBMIT_KEYS {
            assert_ne!(key.label(), key.newline_label());
        }
    }
}
