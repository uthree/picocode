//! The `/prompt` system-prompt editor and `[[prompts]]` presets, applied
//! by respawning the worker with the conversation carried over.

use gpui::prelude::*;
use gpui::{Context, Window};
use gpui_component::input::InputState;
use rust_i18n::t;

use picocode_core::transcript::EntryKind;

use super::ChatView;

impl ChatView {
    /// `/prompt`: open the system-prompt editor dialog, seeded with the
    /// current base prompt (custom or built-in).
    pub(super) fn open_prompt_editor(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.running {
            self.push(EntryKind::Error, t!("prompt_while_running").to_string());
            return;
        }
        let editor = cx.new(|cx| InputState::new(window, cx).auto_grow(8, 16));
        let current = picocode_core::agent::base_system_prompt(&self.cfg);
        editor.update(cx, |state, cx| {
            state.set_value(&current, window, cx);
            state.focus(window, cx);
        });
        self.prompt_edit = Some(editor);
        cx.notify();
    }

    /// The prompt dialog's Apply button.
    pub(super) fn apply_prompt_edit(&mut self, cx: &mut Context<Self>) {
        let Some(editor) = self.prompt_edit.take() else {
            return;
        };
        let text = editor.read(cx).value().trim().to_string();
        if text.is_empty() {
            self.push(EntryKind::Notice, t!("prompt_empty").to_string());
            cx.notify();
            return;
        }
        // Editing the built-in into itself is not a customization.
        if self.cfg.system_prompt.is_none()
            && text == picocode_core::agent::base_system_prompt(&self.cfg)
        {
            self.push(EntryKind::Notice, t!("prompt_unchanged").to_string());
            cx.notify();
            return;
        }
        self.apply_system_prompt(Some(text), cx);
    }

    /// `/prompt <name>`: switch to a `[[prompts]]` preset (exact name, or
    /// a unique case-insensitive substring like /model).
    pub(super) fn apply_prompt_preset(&mut self, name: &str, cx: &mut Context<Self>) {
        if self.cfg.prompts.is_empty() {
            self.push(EntryKind::Error, t!("prompt_no_presets").to_string());
            cx.notify();
            return;
        }
        use picocode_core::command::PresetMatch;
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
                    t!("prompt_unknown_preset", name = name, names = names).to_string(),
                );
                cx.notify();
                return;
            }
            PresetMatch::Ambiguous(names) => {
                self.push(
                    EntryKind::Error,
                    t!("model_ambiguous", name = name, matches = names.join(", ")).to_string(),
                );
                cx.notify();
                return;
            }
        };
        let (preset_name, text) = (preset.name.clone(), preset.prompt.clone());
        if self.set_system_prompt(Some(text), cx) {
            self.push(
                EntryKind::Notice,
                t!("prompt_preset_switched", name = preset_name).to_string(),
            );
        }
        cx.notify();
    }

    /// Swap the system prompt (None = built-in default) by respawning the
    /// worker with the conversation carried over, like a model switch.
    /// Returns whether it happened; pushes the errors, callers the notices.
    fn set_system_prompt(&mut self, prompt: Option<String>, cx: &mut Context<Self>) -> bool {
        if self.running {
            self.push(EntryKind::Error, t!("prompt_while_running").to_string());
            cx.notify();
            return false;
        }
        let mut new_cfg = self.cfg.clone();
        new_cfg.system_prompt = prompt;
        if let Err(error) = self.respawn_worker(new_cfg) {
            self.push(
                EntryKind::Error,
                t!("prompt_failed", error = error).to_string(),
            );
            cx.notify();
            return false;
        }
        true
    }

    /// The editor's apply / `/prompt reset`, with their notices.
    pub(super) fn apply_system_prompt(&mut self, prompt: Option<String>, cx: &mut Context<Self>) {
        if !self.set_system_prompt(prompt.clone(), cx) {
            return;
        }
        match prompt {
            Some(text) => {
                self.push(EntryKind::Notice, t!("prompt_updated").to_string());
                self.push(
                    EntryKind::Notice,
                    t!(
                        "keep_prompt_hint",
                        snippet = picocode_core::config::system_prompt_snippet(&text)
                    )
                    .to_string(),
                );
            }
            None => self.push(EntryKind::Notice, t!("prompt_reset_done").to_string()),
        }
        cx.notify();
    }
}
