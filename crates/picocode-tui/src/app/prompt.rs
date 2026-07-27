//! The `/prompt` system-prompt editor and `[[prompts]]` presets, applied by
//! respawning the worker with the conversation carried over.

use picocode_core::command::PresetMatch;

use crate::input::expand_pastes;

use super::{App, EntryKind};

impl App {
    /// `/prompt`: turn the input box into a system-prompt editor loaded
    /// with the current base prompt (custom or built-in). All the usual
    /// editing works — multi-line via Alt+Enter / `\`+Enter, paste, Ctrl+V.
    /// Enter applies, Esc restores the stashed draft.
    pub(super) fn open_prompt_editor(&mut self) {
        if self.running > 0 {
            self.push(
                EntryKind::Error,
                "Cannot edit the system prompt while a turn is running".to_string(),
            );
            return;
        }
        self.prompt_edit = Some((std::mem::take(&mut self.input), self.cursor));
        self.input = picocode_core::agent::base_system_prompt(&self.cfg);
        self.cursor = self.input.chars().count();
        self.reset_completion();
        self.push(
            EntryKind::Notice,
            "Editing the system prompt — Enter applies (this session), Esc cancels. \
             `{root}` expands to the project root; project instructions are \
             appended automatically."
                .to_string(),
        );
    }

    /// Esc in prompt-edit mode: drop the draft, restore the stashed input.
    pub(super) fn cancel_prompt_edit(&mut self) {
        if let Some((input, cursor)) = self.prompt_edit.take() {
            self.input = input;
            self.cursor = cursor;
            self.push(EntryKind::Notice, "System prompt unchanged".to_string());
        }
    }

    /// Enter in prompt-edit mode: apply the edited prompt.
    pub(super) async fn apply_prompt_edit(&mut self) {
        let text = expand_pastes(self.input.trim(), &self.pasted);
        let Some((input, cursor)) = self.prompt_edit.take() else {
            return;
        };
        self.input = input;
        self.cursor = cursor;
        if text.is_empty() {
            self.push(
                EntryKind::Notice,
                "Empty prompt — system prompt unchanged (use /prompt reset for the built-in)"
                    .to_string(),
            );
            return;
        }
        // Editing the built-in into itself is not a customization.
        if self.cfg.system_prompt.is_none()
            && text == picocode_core::agent::base_system_prompt(&self.cfg)
        {
            self.push(EntryKind::Notice, "System prompt unchanged".to_string());
            return;
        }
        self.apply_system_prompt(Some(text)).await;
    }

    /// `/prompt <name>`: switch to a `[[prompts]]` preset (exact name, or
    /// a unique case-insensitive substring like /model).
    pub(super) async fn apply_prompt_preset(&mut self, name: &str) {
        if self.cfg.prompts.is_empty() {
            self.push(
                EntryKind::Error,
                "No [[prompts]] presets configured in picocode.toml".to_string(),
            );
            return;
        }
        let preset = match picocode_core::command::find_prompt_preset(&self.cfg.prompts, name) {
            PresetMatch::Unique(preset) => preset,
            PresetMatch::None => {
                let names = self
                    .cfg
                    .prompts
                    .iter()
                    .map(|p| p.name.as_str())
                    .collect::<Vec<_>>()
                    .join(", ");
                self.push(
                    EntryKind::Error,
                    format!("Unknown prompt preset `{name}` (available: {names})"),
                );
                return;
            }
            PresetMatch::Ambiguous(names) => {
                self.push(
                    EntryKind::Error,
                    format!("`{name}` is ambiguous: {}", names.join(", ")),
                );
                return;
            }
        };
        let (preset_name, text) = (preset.name.clone(), preset.prompt.clone());
        if self.set_system_prompt(Some(text)).await {
            self.push(
                EntryKind::Notice,
                format!("System prompt switched to preset `{preset_name}`"),
            );
        }
    }

    /// Swap the system prompt (None = built-in default) by respawning the
    /// worker with the conversation carried over, like a model switch.
    /// Returns whether it happened; pushes the errors, callers the notices.
    async fn set_system_prompt(&mut self, prompt: Option<String>) -> bool {
        if self.running > 0 {
            self.push(
                EntryKind::Error,
                "Cannot change the system prompt while a turn is running".to_string(),
            );
            return false;
        }
        let mut new_cfg = self.cfg.clone();
        new_cfg.system_prompt = prompt;
        if let Err(e) = self.respawn_worker(new_cfg).await {
            self.push(
                EntryKind::Error,
                format!("Failed to apply the system prompt: {e:#}"),
            );
            return false;
        }
        true
    }

    /// The editor's apply / `/prompt reset`, with their notices.
    pub(super) async fn apply_system_prompt(&mut self, prompt: Option<String>) {
        if !self.set_system_prompt(prompt.clone()).await {
            return;
        }
        match prompt {
            Some(text) => {
                self.push(
                    EntryKind::Notice,
                    "System prompt updated for this session — the next message uses it".to_string(),
                );
                self.push(
                    EntryKind::Notice,
                    format!(
                        "To keep it, add this to picocode.toml:\n{}",
                        picocode_core::config::system_prompt_snippet(&text)
                    ),
                );
            }
            None => self.push(
                EntryKind::Notice,
                "System prompt reset to the built-in default".to_string(),
            ),
        }
    }
}
