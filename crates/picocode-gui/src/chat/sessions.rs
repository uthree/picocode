//! Session persistence: the sidebar, the `/resume` picker and the
//! per-turn autosave.

use gpui::prelude::*;
use gpui::{AnyElement, Context, SharedString, div, px};
use gpui_component::button::{Button, ButtonVariants};
use gpui_component::{ActiveTheme, Sizable, StyledExt};
use rust_i18n::t;

use picocode_core::event::WorkerCmd;
use picocode_core::session;
use picocode_core::transcript::EntryKind;

use crate::settings;

use super::ChatView;

/// Width of the session sidebar.
const SIDEBAR_W: f32 = 220.;

impl ChatView {
    /// Snapshot the conversation to disk. Runs in the background after each
    /// completed turn; empty conversations are not written. The sidebar is
    /// re-read once the file has landed, so the current session's row picks
    /// up its new message count and age.
    pub(super) fn autosave(&mut self, cx: &mut Context<Self>) {
        let Some(dir) = self.sessions_dir.clone() else {
            return;
        };
        let saved = self.rt.spawn(session::autosave(
            dir,
            self.session_id.clone(),
            self.cfg.root.display().to_string(),
            self.cfg.model_label(),
            self.entries.clone(),
            self.cmd_tx.clone(),
            self.event_tx.clone(),
        ));
        cx.spawn(async move |this, cx| {
            let _ = saved.await;
            let _ = this.update(cx, |view, cx| {
                view.refresh_sessions();
                cx.notify();
            });
        })
        .detach();
    }

    /// Re-read this project's saved sessions (newest first) for the
    /// sidebar. Listing parses every session file, so a hidden sidebar
    /// doesn't pay for it — opening it reads them again.
    pub(super) fn refresh_sessions(&mut self) {
        self.sessions = match (&self.sessions_dir, self.sidebar) {
            (Some(dir), true) => session::list(dir),
            _ => Vec::new(),
        };
    }

    /// Show or hide the session sidebar; the choice is remembered across
    /// runs like the other GUI preferences.
    pub(super) fn toggle_sidebar(&mut self, cx: &mut Context<Self>) {
        self.sidebar = !self.sidebar;
        self.refresh_sessions();
        self.saved.sidebar = Some(self.sidebar);
        settings::save(&self.saved);
        cx.notify();
    }

    /// `/clear` (and the sidebar's new-session button): drop the
    /// conversation and start a fresh session log. The old session stays on
    /// disk, so it remains one click away in the sidebar.
    pub(super) fn new_session(&mut self, cx: &mut Context<Self>) {
        let _ = self.cmd_tx.try_send(WorkerCmd::Clear);
        self.goal = None;
        self.goal_round = 0;
        self.entries.clear();
        self.queued.clear();
        self.pending_attachments.clear();
        self.context_info = None;
        self.tokens_in = 0;
        self.tokens_out = 0;
        self.est_out = 0;
        // A cleared conversation starts a fresh session log.
        self.session_id = session::new_id();
        self.expanded_reasoning.clear();
        self.push(EntryKind::Notice, t!("cleared").to_string());
        self.reset_list();
        self.refresh_sessions();
        cx.notify();
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
        // The row that was active moves; ages and counts refresh with it.
        self.refresh_sessions();
        cx.notify();
    }

    /// The sidebar: this project's saved sessions, newest first, with the
    /// current one marked. Clicking a row resumes it — the same path
    /// `/resume` takes, so a running turn blocks it with a notice.
    pub(super) fn render_sidebar(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        if !self.sidebar {
            return None;
        }
        let theme = cx.theme();

        let mut rows = div()
            .id("session-list")
            .v_flex()
            .gap_0p5()
            .flex_1()
            .px_1p5()
            .pb_2()
            .overflow_y_scroll();
        // A conversation with nothing in it yet (fresh start, /clear) has no
        // file on disk; show it anyway so the sidebar always says where you
        // are.
        if !self.sessions.iter().any(|s| s.id == self.session_id) {
            rows = rows.child(session_row(
                "session-current".into(),
                t!("session_new").to_string(),
                t!("session_unsaved").to_string(),
                true,
                theme,
            ));
        }
        for (ix, s) in self.sessions.iter().enumerate() {
            let active = s.id == self.session_id;
            let title = if s.snippet.is_empty() {
                s.id.clone()
            } else {
                s.snippet.clone()
            };
            let detail = t!(
                "session_detail_short",
                age = session::age(s.modified),
                n = s.messages
            )
            .to_string();
            let row = session_row(
                SharedString::from(format!("session-row-{ix}")),
                title,
                detail,
                active,
                theme,
            );
            rows = rows.child(if active {
                row
            } else {
                let id = s.id.clone();
                row.on_click(cx.listener(move |this, _, _, cx| this.resume_session(&id, cx)))
            });
        }

        Some(
            div()
                .v_flex()
                .flex_none()
                .w(px(SIDEBAR_W))
                .h_full()
                .bg(theme.sidebar)
                .border_r_1()
                .border_color(theme.sidebar_border)
                .child(
                    div()
                        .h_flex()
                        .items_center()
                        .justify_between()
                        .px_3()
                        .py_2()
                        .child(
                            div()
                                .text_sm()
                                .text_color(theme.muted_foreground)
                                .child(t!("sidebar_title").to_string()),
                        )
                        .child(
                            Button::new("new-session")
                                .ghost()
                                .xsmall()
                                .icon(gpui_component::Icon::default().path("icons/square-pen.svg"))
                                .tooltip(t!("new_session_tooltip").to_string())
                                .on_click(cx.listener(|this, _, _, cx| this.new_session(cx))),
                        ),
                )
                .child(rows)
                .into_any_element(),
        )
    }
}

/// One sidebar row: the first prompt over its age and message count. The
/// current session is marked with the accent fill instead of being
/// clickable.
fn session_row(
    id: SharedString,
    title: String,
    detail: String,
    active: bool,
    theme: &gpui_component::theme::Theme,
) -> gpui::Stateful<gpui::Div> {
    div()
        .id(id)
        .v_flex()
        .px_2()
        .py_1()
        .rounded_md()
        .when(active, |s| {
            s.bg(theme.sidebar_accent)
                .text_color(theme.sidebar_accent_foreground)
        })
        .when(!active, |s| s.cursor_pointer().hover(|s| s.bg(theme.muted)))
        .child(div().text_sm().truncate().child(title))
        .child(
            div()
                .text_xs()
                .text_color(theme.muted_foreground)
                .truncate()
                .child(detail),
        )
}
