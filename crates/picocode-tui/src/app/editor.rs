//! Input-box editing: completion, history-aware cursor movement, paste
//! placeholders, and attachment staging (`/attach`, Ctrl+V).

use crate::input::{cursor_at, line_col, paste_placeholder};

use super::{App, EntryKind};

impl App {
    /// `/attach <path>`: stage a file to send with the next prompt
    /// (`/attach clear` unstages everything). Unsupported types and types
    /// the current provider can't take are refused with an explanation, so
    /// nothing is silently dropped later in the provider conversion.
    pub(super) fn attach(&mut self, arg: &str) {
        if arg == "clear" {
            self.attachments.clear();
            self.push(EntryKind::Notice, "Attachments cleared".to_string());
            return;
        }
        self.stage_file(&self.cfg.root.join(arg), arg);
    }

    /// Stage one file as an attachment, shared by `/attach` and clipboard
    /// paste; `display` is how the file is referred to in notices.
    fn stage_file(&mut self, path: &std::path::Path, display: &str) {
        use picocode_core::attachment::Attachment;
        if !path.is_file() {
            self.push(EntryKind::Error, format!("Not a file: {display}"));
            return;
        }
        match Attachment::detect(path) {
            Some(att) if att.supported_by(self.cfg.provider) => {
                if self.attachments.contains(&att) {
                    self.push(EntryKind::Notice, format!("Already attached: {display}"));
                    return;
                }
                self.push(
                    EntryKind::Notice,
                    format!(
                        "📎 Attached {} ({} staged)",
                        display,
                        self.attachments.len() + 1
                    ),
                );
                self.attachments.push(att);
            }
            Some(_) => self.push(
                EntryKind::Warning,
                format!(
                    "The {} provider can't take this file type; not attached",
                    picocode_core::config::provider_name(self.cfg.provider)
                ),
            ),
            None => self.push(
                EntryKind::Notice,
                "This looks like an unsupported binary format — images, audio, PDF \
                 and text files can be attached"
                    .to_string(),
            ),
        }
    }

    /// `/attach` with no argument: list what's staged.
    pub(super) fn show_attachments(&mut self) {
        if self.attachments.is_empty() {
            self.push(
                EntryKind::Notice,
                "No attachments staged. /attach <path> stages a file for the \
                 next prompt; /attach clear unstages all."
                    .to_string(),
            );
            return;
        }
        let list = self
            .attachments
            .iter()
            .map(|a| format!("  📎 {}", a.path.display()))
            .collect::<Vec<_>>()
            .join("\n");
        self.push(
            EntryKind::Notice,
            format!("Staged for the next prompt:\n{list}"),
        );
    }

    /// Candidates for the completion popup, as (text to fill the input
    /// with, description). Uses the locked prefix while cycling, otherwise
    /// the current input. Before the first space these are command names;
    /// after it, the command's argument candidates (model names, session
    /// ids, file paths).
    pub fn completions(&self) -> Vec<(String, String)> {
        // The prompt editor's content is never a command.
        if self.prompt_edit.is_some() {
            return Vec::new();
        }
        let filter = self.comp_prefix.as_deref().unwrap_or(&self.input);
        if !filter.starts_with('/') || filter.contains('\n') {
            return Vec::new();
        }
        match filter.split_once(' ') {
            None => picocode_core::command::COMMANDS
                .iter()
                .filter(|spec| spec.name.starts_with(filter))
                .map(|spec| (spec.name.to_string(), spec.description.to_string()))
                .collect(),
            Some((cmd, arg)) => self.arg_completions(cmd, arg.trim_start()),
        }
    }

    /// Argument candidates for `cmd`, filtered by the partial `arg`.
    fn arg_completions(&self, cmd: &str, arg: &str) -> Vec<(String, String)> {
        use picocode_core::command;
        match cmd {
            "/model" => command::model_completions(self.model_choices(), cmd, arg),
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

    /// Tab completion: the first press fills the input with the highlighted
    /// candidate; subsequent presses cycle through the candidates matched by
    /// the prefix as it was when completion started.
    pub(super) fn complete(&mut self, backwards: bool) {
        let was_cycling = self.comp_prefix.is_some();
        let matches = self.completions();
        if matches.is_empty() {
            self.comp_prefix = None;
            return;
        }
        if !was_cycling {
            self.comp_prefix = Some(self.input.clone());
        }
        let count = matches.len();
        self.comp_selected = self.comp_selected.min(count - 1);
        if was_cycling {
            self.comp_selected = if backwards {
                (self.comp_selected + count - 1) % count
            } else {
                (self.comp_selected + 1) % count
            };
        }
        self.input = matches[self.comp_selected].0.clone();
        self.cursor = self.input.chars().count();
    }

    /// Move the completion selection with the arrow keys (fills the input).
    pub(super) fn move_completion(&mut self, up: bool) {
        let matches = self.completions();
        if matches.is_empty() {
            return;
        }
        if self.comp_prefix.is_none() {
            self.comp_prefix = Some(self.input.clone());
        }
        let count = matches.len();
        self.comp_selected = self.comp_selected.min(count - 1);
        self.comp_selected = if up {
            (self.comp_selected + count - 1) % count
        } else {
            (self.comp_selected + 1) % count
        };
        self.input = matches[self.comp_selected].0.clone();
        self.cursor = self.input.chars().count();
    }

    pub(super) fn reset_completion(&mut self) {
        self.comp_selected = 0;
        self.comp_prefix = None;
        // Called on every edit — an edit also ends history browsing (the
        // recalled text stays and becomes the current input).
        self.input_history.stop();
    }

    /// `↑`/`↓` outside the completion popup: move between input lines when
    /// the cursor can, otherwise step through the submitted-message history.
    pub(super) fn move_line_or_history(&mut self, up: bool) {
        let (row, _) = line_col(&self.input, self.cursor);
        let rows = self.input.split('\n').count();
        if up && row > 0 {
            self.move_input_line(true);
        } else if !up && row + 1 < rows {
            self.move_input_line(false);
        } else {
            // No history recall while the input box edits the system
            // prompt — a stray ↑ must not overwrite the draft.
            if self.prompt_edit.is_some() {
                return;
            }
            let recalled = if up {
                self.input_history.prev(&self.input)
            } else {
                self.input_history.next()
            };
            if let Some(text) = recalled {
                self.cursor = text.chars().count();
                self.input = text;
                // Not reset_completion(): that would end the browsing that
                // just moved here.
                self.comp_selected = 0;
                self.comp_prefix = None;
            }
        }
    }

    /// Ctrl+V: read the system clipboard. Copied files (Finder/Explorer)
    /// and raw image data (screenshots — saved to a temp PNG first) go
    /// through the `/attach` staging; plain text is a normal paste. The
    /// terminal's own paste keeps working independently of this.
    pub(super) fn paste_clipboard(&mut self) {
        use picocode_core::clipboard::{self, Pasted};
        let dir = std::env::temp_dir().join(format!("picocode-{}", std::process::id()));
        self.clip_count += 1;
        match clipboard::read(&dir, self.clip_count) {
            Ok(Some(Pasted::Files(paths))) => {
                for path in paths {
                    self.stage_file(&path, &path.display().to_string());
                }
            }
            Ok(Some(Pasted::Image(path))) => {
                self.stage_file(&path, "clipboard image");
            }
            Ok(Some(Pasted::Text(text))) => {
                let text = text
                    .replace("\r\n", "\n")
                    .replace('\r', "\n")
                    .replace('\t', "    ");
                self.insert_paste(text);
                self.reset_completion();
            }
            Ok(None) => self.push(
                EntryKind::Notice,
                "Clipboard is empty — copy a file, an image or text first".to_string(),
            ),
            Err(e) => self.push(EntryKind::Error, format!("Clipboard read failed: {e}")),
        }
    }

    /// Insert pasted text at the cursor: long pastes collapse into a
    /// `[Pasted text #n +N lines]` placeholder and the full text is kept
    /// aside until the message is submitted.
    pub(super) fn insert_paste(&mut self, text: String) {
        match paste_placeholder(&text, self.pasted.len() + 1) {
            Some(placeholder) => {
                self.insert_str(&placeholder);
                self.pasted.push((placeholder, text));
            }
            None => self.insert_str(&text),
        }
    }

    /// If the text right before the cursor is a paste placeholder, delete it
    /// whole (Backspace).
    pub(super) fn delete_placeholder_before_cursor(&mut self) -> bool {
        let byte = self.byte_index();
        let Some(len) = self
            .pasted
            .iter()
            .find(|(ph, _)| self.input[..byte].ends_with(ph.as_str()))
            .map(|(ph, _)| ph.len())
        else {
            return false;
        };
        let chars = self.input[byte - len..byte].chars().count();
        self.input.replace_range(byte - len..byte, "");
        self.cursor -= chars;
        true
    }

    /// If the text right at the cursor is a paste placeholder, delete it
    /// whole (Delete).
    pub(super) fn delete_placeholder_at_cursor(&mut self) -> bool {
        let byte = self.byte_index();
        let Some(len) = self
            .pasted
            .iter()
            .find(|(ph, _)| self.input[byte..].starts_with(ph.as_str()))
            .map(|(ph, _)| ph.len())
        else {
            return false;
        };
        self.input.replace_range(byte..byte + len, "");
        true
    }

    /// Up/Down in a multi-line input: move the cursor a line, keeping the
    /// column where possible.
    fn move_input_line(&mut self, up: bool) {
        let (row, col) = line_col(&self.input, self.cursor);
        let rows = self.input.split('\n').count();
        let target = if up {
            row.checked_sub(1)
        } else {
            (row + 1 < rows).then_some(row + 1)
        };
        if let Some(r) = target {
            self.cursor = cursor_at(&self.input, r, col);
        }
    }

    pub(super) fn insert_char(&mut self, c: char) {
        let i = self.byte_index();
        self.input.insert(i, c);
        self.cursor += 1;
    }

    pub(super) fn insert_str(&mut self, s: &str) {
        let i = self.byte_index();
        self.input.insert_str(i, s);
        self.cursor += s.chars().count();
        self.comp_selected = 0;
    }

    pub(super) fn byte_index(&self) -> usize {
        self.input
            .char_indices()
            .nth(self.cursor)
            .map(|(i, _)| i)
            .unwrap_or(self.input.len())
    }
}
