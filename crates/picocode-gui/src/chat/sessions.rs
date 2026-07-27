//! Session persistence: the `/resume` picker and the per-turn autosave.

use gpui::Context;
use rust_i18n::t;

use picocode_core::event::WorkerCmd;
use picocode_core::session;
use picocode_core::transcript::EntryKind;

use super::ChatView;

impl ChatView {
    /// Snapshot the conversation to disk. Runs in the background after each
    /// completed turn; empty conversations are not written.
    pub(super) fn autosave(&mut self) {
        let Some(dir) = self.sessions_dir.clone() else {
            return;
        };
        self.rt.spawn(session::autosave(
            dir,
            self.session_id.clone(),
            self.cfg.root.display().to_string(),
            self.cfg.model_label(),
            self.entries.clone(),
            self.cmd_tx.clone(),
            self.event_tx.clone(),
        ));
    }

    /// `/resume`: open the session-selection dialog.
    pub(super) fn open_session_picker(&mut self, cx: &mut Context<Self>) {
        if self.running {
            self.push(EntryKind::Error, t!("resume_while_running").to_string());
            cx.notify();
            return;
        }
        let Some(dir) = self.sessions_dir.clone() else {
            self.push(EntryKind::Error, t!("no_home").to_string());
            cx.notify();
            return;
        };
        // Resuming the current session would be a no-op, so it isn't offered.
        let sessions: Vec<_> = session::list(&dir)
            .into_iter()
            .filter(|s| s.id != self.session_id)
            .collect();
        if sessions.is_empty() {
            self.push(EntryKind::Notice, t!("no_sessions").to_string());
            cx.notify();
            return;
        }
        self.session_picker = Some(sessions);
        cx.notify();
    }

    /// Resume a session by id (picked in the dialog or given to
    /// `/resume <id>`): seed the worker with its history and restore the
    /// transcript.
    pub(super) fn resume_session(&mut self, id: &str, cx: &mut Context<Self>) {
        self.session_picker = None;
        if self.running {
            self.push(EntryKind::Error, t!("resume_while_running").to_string());
            cx.notify();
            return;
        }
        let Some(dir) = self.sessions_dir.clone() else {
            self.push(EntryKind::Error, t!("no_home").to_string());
            cx.notify();
            return;
        };
        if id == self.session_id {
            self.push(EntryKind::Notice, t!("current_session").to_string());
            cx.notify();
            return;
        }

        let saved = match session::load(&dir, id) {
            Ok(s) => s,
            Err(e) => {
                self.push(
                    EntryKind::Error,
                    t!("resume_failed", error = format!("{e:#}")).to_string(),
                );
                cx.notify();
                return;
            }
        };
        let messages = saved.history.len();
        if self
            .cmd_tx
            .try_send(WorkerCmd::SeedHistory(saved.history))
            .is_err()
        {
            self.push(EntryKind::Error, t!("worker_stopped").to_string());
            cx.notify();
            return;
        }

        self.entries.clear();
        self.tokens_in = 0;
        self.tokens_out = 0;
        self.est_out = 0;
        self.push(
            EntryKind::Notice,
            t!("resumed", id = id, n = messages, model = saved.model).to_string(),
        );
        self.entries.extend(saved.entries);
        self.session_id = id.to_string();
        self.queued.clear();
        self.pending_attachments.clear();
        self.expanded_reasoning.clear();
        self.reset_list();
        cx.notify();
    }
}
