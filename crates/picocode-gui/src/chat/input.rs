//! The input box: completion, submission plumbing, and attachment staging
//! (drag & drop, the attach button, `/attach`, clipboard paste).

use gpui::{Context, Entity, Window};
use gpui_component::input::{InputEvent, InputState};
use rust_i18n::t;

use picocode_core::attachment::Attachment;
use picocode_core::models;
use picocode_core::transcript::EntryKind;

use super::{AcceptCompletion, ChatView, PasteClipboard, SubmitPrompt};

impl ChatView {
    // ---------- user actions ----------

    pub(super) fn on_input_event(
        &mut self,
        _: &Entity<InputState>,
        ev: &InputEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match ev {
            // Plain Enter is rebound to SubmitPrompt and never reaches the
            // input (see main.rs), so this only fires for stray paths; a
            // secondary Enter (Shift+Enter, Cmd+Enter) keeps the newline the
            // multi-line input inserted.
            InputEvent::PressEnter { secondary: false } => self.submit(window, cx),
            // Typing anything resets the Tab-cycling anchor (unless the
            // change came from Tab itself filling the input).
            InputEvent::Change => {
                if self.completing {
                    self.completing = false;
                } else {
                    self.comp_prefix = None;
                }
                cx.notify();
            }
            _ => {}
        }
    }

    /// Plain Enter in the input: submit. Bound directly to this action so
    /// the multi-line input never inserts a newline at the cursor first
    /// (its own Enter handling inserts, then emits PressEnter — which left
    /// a stray newline in the submitted text when the cursor sat mid-line).
    pub(super) fn on_submit_prompt(
        &mut self,
        _: &SubmitPrompt,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.submit(window, cx);
    }

    /// Completion candidates as (text to fill the input with, description).
    /// Before the first space these are command names; after it, the
    /// command's argument candidates (model names, session ids).
    pub(super) fn completion_matches(&self, cx: &gpui::App) -> Vec<(String, String)> {
        let value = self.input.read(cx).value().to_string();
        if !value.starts_with('/') || value.contains('\n') {
            return Vec::new();
        }
        let filter = self.comp_prefix.clone().unwrap_or_else(|| value.clone());
        match filter.split_once(' ') {
            None => picocode_core::command::COMMANDS
                .iter()
                .filter(|spec| spec.name.starts_with(&filter))
                .map(|spec| {
                    let key = format!("cmd_{}", spec.name[1..].replace('-', "_"));
                    (spec.name.to_string(), t!(&key).to_string())
                })
                .collect(),
            Some((cmd, arg)) => self.arg_completions(cmd, arg.trim_start()),
        }
    }

    /// Argument candidates for `cmd`, filtered by the partial `arg`.
    fn arg_completions(&self, cmd: &str, arg: &str) -> Vec<(String, String)> {
        use picocode_core::command;
        match cmd {
            "/model" => command::model_completions(
                models::model_choices(
                    &self.cfg.models,
                    self.cfg.active_model.as_deref(),
                    self.cfg.provider,
                    self.cfg.base_url.as_deref(),
                    &self.cfg.model,
                    &self.available_models,
                ),
                cmd,
                arg,
            ),
            "/resume" => match &self.sessions_dir {
                Some(dir) => command::session_completions(dir, &self.session_id, cmd, arg),
                None => Vec::new(),
            },
            "/attach" => command::path_completions(&self.cfg.root, cmd, arg),
            "/jobs" => command::jobs_completions(&self.jobs, cmd, arg),
            "/prompt" => command::prompt_completions(&self.cfg.prompts, cmd, arg),
            "/remote" => command::remote_completions(&self.cfg.remotes, cmd, arg),
            _ => Vec::new(),
        }
    }

    /// Tab in the input: fill the first matching candidate, or cycle
    /// through the matches of the prefix locked at the first press.
    pub(super) fn accept_completion(
        &mut self,
        _: &AcceptCompletion,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.dialog_open() {
            return;
        }
        let value = self.input.read(cx).value().to_string();
        let prefix = self.comp_prefix.clone().unwrap_or_else(|| value.clone());
        let matches = self.completion_matches(cx);
        if matches.is_empty() {
            self.comp_prefix = None;
            return;
        }
        let next = match matches.iter().position(|(fill, _)| *fill == value) {
            Some(i) => matches[(i + 1) % matches.len()].0.clone(),
            None => matches[0].0.clone(),
        };
        self.comp_prefix = Some(prefix);
        self.completing = true;
        self.input
            .update(cx, |state, cx| state.set_value(&next, window, cx));
        cx.notify();
    }

    /// `/attach`: list what is staged for the next prompt.
    pub(super) fn show_attachments(&mut self) {
        if self.pending_attachments.is_empty() {
            self.push(EntryKind::Notice, t!("attach_none").to_string());
            return;
        }
        let names: Vec<String> = self.pending_attachments.iter().map(|a| a.name()).collect();
        self.push(
            EntryKind::Notice,
            t!("attach_list", names = names.join(", ")).to_string(),
        );
    }

    /// `/attach clear` or `/attach <path>`, sharing the drop/picker staging
    /// logic (provider checks and notices included).
    pub(super) fn attach_command(&mut self, arg: &str, cx: &mut Context<Self>) {
        if arg == "clear" {
            self.pending_attachments.clear();
            self.push(EntryKind::Notice, t!("attach_cleared").to_string());
            return;
        }
        let path = self.cfg.root.join(arg);
        self.add_attachments(&[path], cx);
    }

    /// Stage dropped or picked files for the next prompt; unsupported files
    /// produce a notice instead of being silently dropped by the provider
    /// conversion later. Non-media files that read as text are staged as
    /// text attachments (inlined into the message).
    pub(super) fn add_attachments(&mut self, paths: &[std::path::PathBuf], cx: &mut Context<Self>) {
        for path in paths {
            match Attachment::detect(path) {
                Some(att) if att.supported_by(self.cfg.provider) => {
                    if !self.pending_attachments.contains(&att) {
                        self.pending_attachments.push(att);
                    }
                }
                Some(att) => self.push(
                    EntryKind::Warning,
                    t!(
                        "attach_unsupported_provider",
                        name = att.name(),
                        provider = picocode_core::config::provider_name(self.cfg.provider)
                    )
                    .to_string(),
                ),
                None => self.push(
                    EntryKind::Notice,
                    t!(
                        "attach_unsupported_type",
                        name = path.file_name().unwrap_or_default().to_string_lossy()
                    )
                    .to_string(),
                ),
            }
        }
        cx.notify();
    }

    /// Cmd+V (Ctrl+V off macOS) in the chat input: copied files and raw
    /// image data (screenshots — saved to a temp PNG first) stage as
    /// attachments. Anything else propagates so the keystroke falls back
    /// to the input's own text paste — which also keeps text pasting
    /// intact in dialog text fields (model filter, add-model form).
    pub(super) fn on_paste_clipboard(
        &mut self,
        _: &PasteClipboard,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        use picocode_core::clipboard::{self, Pasted};
        if self.dialog_open() {
            cx.propagate();
            return;
        }
        let dir = std::env::temp_dir().join(format!("picocode-{}", std::process::id()));
        self.clip_count += 1;
        match clipboard::read(&dir, self.clip_count) {
            Ok(Some(Pasted::Files(paths))) => self.add_attachments(&paths, cx),
            Ok(Some(Pasted::Image(path))) => self.add_attachments(&[path], cx),
            // Text, empty or unreadable: let the input paste text as usual.
            _ => cx.propagate(),
        }
    }

    /// Open a native file picker and stage the chosen files.
    pub(super) fn pick_attachments(&mut self, cx: &mut Context<Self>) {
        let rx = cx.prompt_for_paths(gpui::PathPromptOptions {
            files: true,
            directories: false,
            multiple: true,
            prompt: None,
        });
        cx.spawn(async move |this, cx| {
            if let Ok(Ok(Some(paths))) = rx.await {
                let _ = this.update(cx, |view, cx| view.add_attachments(&paths, cx));
            }
        })
        .detach();
    }
}
