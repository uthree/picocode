//! Session persistence: the `/resume` picker and the per-turn autosave.

use picocode_core::event::WorkerCmd;
use picocode_core::session;

use super::{App, Entry, EntryKind, LOGO, SessionPicker};

impl App {
    /// `/resume` with no argument: open the session-selection dialog.
    pub(super) fn open_session_picker(&mut self) {
        if self.running > 0 {
            self.push(
                EntryKind::Error,
                "Cannot resume while a turn is running".to_string(),
            );
            return;
        }
        let Some(dir) = self.sessions_dir.clone() else {
            self.push(
                EntryKind::Error,
                "Session storage is unavailable (no $HOME)".to_string(),
            );
            return;
        };
        // Resuming the current session would be a no-op, so it isn't offered.
        let sessions: Vec<_> = session::list(&dir)
            .into_iter()
            .filter(|s| s.id != self.session_id)
            .collect();
        if sessions.is_empty() {
            self.push(
                EntryKind::Notice,
                "No saved sessions for this project yet".to_string(),
            );
            return;
        }
        self.session_picker = Some(SessionPicker {
            sessions,
            selected: 0,
        });
    }

    /// Resume a session by id (picked in the dialog or given to `/resume <id>`):
    /// seed the worker with its history and restore the transcript.
    pub(super) async fn resume_session(&mut self, id: &str) {
        if self.running > 0 {
            self.push(
                EntryKind::Error,
                "Cannot resume while a turn is running".to_string(),
            );
            return;
        }
        let Some(dir) = self.sessions_dir.clone() else {
            self.push(
                EntryKind::Error,
                "Session storage is unavailable (no $HOME)".to_string(),
            );
            return;
        };
        if id == self.session_id {
            self.push(EntryKind::Notice, "That is the current session".to_string());
            return;
        }

        let saved = match session::load(&dir, id) {
            Ok(s) => s,
            Err(e) => {
                self.push(EntryKind::Error, format!("Failed to resume: {e:#}"));
                return;
            }
        };
        // GUI-created worktree sessions share the source project's store.
        // Restore their tool root before seeding history in either front end.
        if !self.backend.is_remote() && std::path::Path::new(&saved.cwd) != self.cfg.root {
            if !self.jobs.list().is_empty() {
                self.push(
                    EntryKind::Error,
                    "Stop background jobs before changing the session workspace".into(),
                );
                return;
            }
            let opened = self
                .cfg
                .in_local_workspace(std::path::Path::new(&saved.cwd));
            let mut cfg = match opened {
                Ok(cfg) => cfg,
                Err(error) => {
                    self.push(
                        EntryKind::Error,
                        format!("Failed to restore session workspace: {error:#}"),
                    );
                    return;
                }
            };
            self.saved.apply(&mut cfg);
            let (mcp, errors) =
                picocode_core::mcp::connect_all_in(&cfg.mcp_servers, Some(&cfg.root)).await;
            let previous_mcp = std::mem::replace(&mut self.mcp, mcp);
            if let Err(error) = self.respawn_worker(cfg).await {
                self.mcp = previous_mcp;
                self.push(
                    EntryKind::Error,
                    format!("Failed to restore session workspace: {error:#}"),
                );
                return;
            }
            for error in errors {
                self.push(EntryKind::Error, error);
            }
            self.git_branch = picocode_core::git::branch(&self.cfg.root);
            self.model_label = self.cfg.model_label();
            self.goal = None;
            self.goal_round = 0;
        }
        let messages = saved.history.len();
        if self
            .cmd_tx
            .send(WorkerCmd::SeedHistory(saved.history))
            .await
            .is_err()
        {
            self.push(EntryKind::Error, "The agent worker has stopped".to_string());
            return;
        }

        self.entries.clear();
        self.close_blocks();
        self.ctx_tokens = 0;
        self.follow = true;
        self.top_line = 0;
        self.push(EntryKind::Logo, LOGO.to_string());
        self.push(
            EntryKind::Notice,
            format!(
                "Resumed session {id} — {messages} messages, last saved with {}",
                saved.model
            ),
        );
        self.entries.extend(saved.entries);
        self.session_id = id.to_string();
    }

    /// Snapshot the conversation to disk. Runs in the background after each
    /// completed turn; empty conversations are not written.
    pub(super) fn autosave(&mut self) {
        let Some(dir) = self.sessions_dir.clone() else {
            return;
        };
        let entries: Vec<Entry> = self
            .entries
            .iter()
            .filter(|e| e.kind != EntryKind::Logo)
            .cloned()
            .collect();
        tokio::spawn(session::autosave(
            dir,
            self.session_id.clone(),
            self.cfg.root.display().to_string(),
            self.model_label.clone(),
            entries,
            self.cmd_tx.clone(),
            self.event_tx.clone(),
        ));
    }
}
