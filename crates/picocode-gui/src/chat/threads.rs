//! Window-owned conversations. Each retained entity owns its worker and event
//! pump, so selecting another row changes presentation without stopping work.

use gpui::prelude::*;
use gpui::{Context, Entity, EventEmitter, MouseButton, SharedString, Window, div, px};
use gpui_component::button::Button;
use gpui_component::{ActiveTheme, Disableable, Sizable, StyledExt};
use picocode_core::event::WorkerCmd;
use picocode_core::transcript::EntryKind;
use picocode_core::{agent, config, session};
use rust_i18n::t;

use super::ChatView;

pub(super) enum ThreadAction {
    New,
    Resume(String),
    Delete(String),
}

impl EventEmitter<ThreadAction> for ChatView {}

pub struct ThreadsView {
    views: Vec<Entity<ChatView>>,
    active: usize,
    creating: bool,
    error: Option<String>,
}

impl ThreadsView {
    pub fn new(view: Entity<ChatView>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let mut this = Self {
            views: Vec::new(),
            active: 0,
            creating: false,
            error: None,
        };
        this.retain(view, window, cx);
        this
    }

    fn retain(&mut self, view: Entity<ChatView>, window: &mut Window, cx: &mut Context<Self>) {
        view.update(cx, |view, _| view.hosted = true);
        cx.observe(&view, |_, _, cx| cx.notify()).detach();
        cx.subscribe_in(&view, window, |this, _, action, window, cx| match action {
            ThreadAction::New => this.open(None, false, window, cx),
            ThreadAction::Resume(id) => this.open(Some(id.clone()), false, window, cx),
            ThreadAction::Delete(id) => this.delete(id, window, cx),
        })
        .detach();
        self.views.push(view);
        self.select(self.views.len() - 1, window, cx);
    }

    fn select(&mut self, index: usize, window: &mut Window, cx: &mut Context<Self>) {
        let sidebar = self.views[self.active].read(cx).sidebar;
        self.views[self.active].update(cx, |view, _| view.visible = false);
        self.active = index;
        self.views[index].update(cx, |view, cx| {
            view.visible = true;
            view.sidebar = sidebar;
            view.refresh_sessions();
            super::input::bind_send_key(view.cfg.submit_key, cx);
            if view.dialog_open() {
                view.dialog_focus.focus(window);
            } else {
                view.input.update(cx, |input, cx| input.focus(window, cx));
            }
            cx.notify();
        });
        self.error = None;
        cx.notify();
    }

    /// All fallible preparation happens before the active entity is changed.
    /// Worktree creation and config loading run off the UI thread.
    fn open(
        &mut self,
        id: Option<String>,
        worktree: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.creating {
            return;
        }
        let source = self.views[self.active].read(cx);
        let dir = source.sessions_dir.clone();
        if let Some(id) = &id
            && let Some(index) = self.views.iter().position(|view| {
                let view = view.read(cx);
                view.session_id == *id && view.sessions_dir == dir
            })
        {
            self.select(index, window, cx);
            return;
        }
        let cfg = source.cfg.for_session();
        let backend = source.backend.clone();
        let rt = source.rt.clone();
        let sidebar = source.sidebar;
        let new_id = id.clone().unwrap_or_else(session::new_id);
        self.creating = true;
        self.error = None;
        let prepared = rt.spawn_blocking(move || -> anyhow::Result<_> {
            let saved = id
                .as_ref()
                .map(|id| {
                    session::load(
                        dir.as_deref()
                            .ok_or_else(|| anyhow::anyhow!("No session directory"))?,
                        id,
                    )
                })
                .transpose()?;
            let mut next_cfg = cfg;
            let root = if worktree {
                anyhow::ensure!(
                    !backend.is_remote(),
                    "Worktrees require a local Git workspace"
                );
                let store = dir
                    .as_ref()
                    .ok_or_else(|| anyhow::anyhow!("No session directory"))?;
                Some(picocode_core::git::create_worktree(
                    &next_cfg.root,
                    &store.join("worktrees"),
                    &new_id,
                )?)
            } else {
                saved
                    .as_ref()
                    .filter(|_| !backend.is_remote())
                    .map(|s| std::path::PathBuf::from(&s.cwd))
            };
            if let Some(root) = root {
                anyhow::ensure!(root.is_dir(), "Workspace is missing: {}", root.display());
                if root != next_cfg.root {
                    next_cfg = next_cfg.in_local_workspace(&root)?;
                }
            }
            config::saved::load().apply(&mut next_cfg);
            Ok((next_cfg, backend, dir, new_id, saved))
        });
        cx.spawn_in(window, async move |this, cx| {
            let result = async {
                let (cfg, backend, dir, id, saved) = prepared.await??;
                // MCP clients are session-owned as well: a server may retain
                // conversation state, so sharing it would break isolation.
                let (mcp, errors) = rt
                    .spawn({
                        let servers = cfg.mcp_servers.clone();
                        let root = (!backend.is_remote()).then(|| cfg.root.clone());
                        async move {
                            picocode_core::mcp::connect_all_in(&servers, root.as_deref()).await
                        }
                    })
                    .await?;
                let (event_tx, event_rx) = tokio::sync::mpsc::channel(256);
                let (cancel_tx, cancel_rx) = tokio::sync::watch::channel(());
                let jobs = picocode_core::tools::BackgroundJobs::new();
                let (cmd_tx, steer) = {
                    let _guard = rt.enter();
                    agent::spawn(
                        &cfg,
                        event_tx.clone(),
                        cancel_rx,
                        jobs.clone(),
                        mcp.clone(),
                        backend.clone(),
                    )?
                };
                for error in errors {
                    let _ = event_tx.try_send(picocode_core::event::AgentEvent::Error(error));
                }
                this.update_in(cx, |this, window, cx| {
                    let view = cx.new(|cx| {
                        let mut view = ChatView::new(
                            cfg, event_rx, event_tx, cmd_tx, steer, jobs, cancel_tx, rt, mcp,
                            backend, window, cx,
                        );
                        view.session_id = id;
                        view.sessions_dir = dir;
                        view.sidebar = sidebar;
                        if let Some(saved) = saved {
                            // The worker is new and its command queue is empty.
                            if view
                                .cmd_tx
                                .try_send(WorkerCmd::SeedHistory(saved.history))
                                .is_ok()
                            {
                                view.entries.extend(saved.entries);
                                view.reset_list();
                            } else {
                                view.push(EntryKind::Error, t!("worker_stopped").to_string());
                            }
                        }
                        view
                    });
                    this.creating = false;
                    this.retain(view, window, cx);
                })?;
                anyhow::Ok(())
            }
            .await;
            if let Err(error) = result {
                let _ = this.update(cx, |this, cx| {
                    this.creating = false;
                    this.error =
                        Some(t!("thread_open_failed", error = format!("{error:#}")).to_string());
                    cx.notify();
                });
            }
        })
        .detach();
        cx.notify();
    }

    fn delete(&mut self, id: &str, window: &mut Window, cx: &mut Context<Self>) {
        let dir = self.views[self.active].read(cx).sessions_dir.clone();
        let Some(dir) = dir else { return };
        let index = self.views.iter().position(|view| {
            let view = view.read(cx);
            view.session_id == id && view.sessions_dir.as_ref() == Some(&dir)
        });
        if let Some(index) = index {
            let view = self.views[index].read(cx);
            if view.running || !view.bg_jobs.is_empty() {
                self.error = Some(t!("session_delete_running").to_string());
                cx.notify();
                return;
            }
        }
        if let Err(error) = session::delete(&dir, id) {
            self.error = Some(t!("session_delete_failed", error = error.to_string()).to_string());
        } else {
            if let Some(index) = index {
                // Keep the entity alive for an in-flight autosave's cleanup.
                self.views[index].update(cx, |view, cx| {
                    view.deleted_sessions.insert(id.to_string());
                    view.reset_session(cx);
                });
                self.select(index, window, cx);
            } else {
                self.views[self.active].update(cx, |view, cx| {
                    view.refresh_sessions();
                    cx.notify();
                });
            }
        }
        cx.notify();
    }
}

impl Render for ThreadsView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let active = self.views[self.active].clone();
        let source = active.read(cx);
        let sidebar = source.sidebar;
        let can_worktree = !source.backend.is_remote() && source.git_branch.is_some();
        let mut rows = div()
            .id("thread-list")
            .v_flex()
            .gap_1()
            .flex_1()
            .min_h_0()
            .overflow_y_scroll();
        for (index, entity) in self.views.iter().enumerate() {
            let view = entity.read(cx);
            let title = view
                .entries
                .iter()
                .find(|e| e.kind == EntryKind::User)
                .map(|e| e.text.split_whitespace().collect::<Vec<_>>().join(" "))
                .unwrap_or_else(|| t!("session_new").to_string());
            let status = if view.approval.is_some() || view.question.is_some() {
                t!("thread_needs_input")
            } else if view.running || !view.bg_jobs.is_empty() {
                t!("thread_running")
            } else {
                t!("thread_ready")
            };
            let detail = format!(
                "{status} · {}",
                view.git_branch
                    .clone()
                    .unwrap_or_else(|| view.workdir_label())
            );
            let id = view.session_id.clone();
            let row = super::sessions::session_row(
                SharedString::from(format!("live-{index}")),
                title,
                detail,
                index == self.active,
                cx.theme(),
            )
            .on_click(cx.listener(move |this, _, window, cx| this.select(index, window, cx)))
            .on_mouse_down(
                MouseButton::Right,
                cx.listener(move |this, ev: &gpui::MouseDownEvent, window, cx| {
                    this.select(index, window, cx);
                    this.views[index].update(cx, |view, cx| {
                        view.session_menu = Some((id.clone(), ev.position));
                        cx.notify();
                    });
                }),
            );
            rows = rows.child(row);
        }
        rows = rows.child(
            div()
                .mt_3()
                .text_xs()
                .text_color(cx.theme().muted_foreground)
                .child(t!("thread_saved").to_string()),
        );
        for (index, saved) in source.sessions.iter().enumerate() {
            if self.views.iter().any(|view| {
                let view = view.read(cx);
                view.session_id == saved.id && view.sessions_dir == source.sessions_dir
            }) {
                continue;
            }
            let id = saved.id.clone();
            let menu_id = id.clone();
            rows = rows.child(
                super::sessions::session_row(
                    SharedString::from(format!("saved-{index}")),
                    if saved.snippet.is_empty() {
                        id.clone()
                    } else {
                        saved.snippet.clone()
                    },
                    t!(
                        "session_detail_short",
                        age = session::age(saved.modified),
                        n = saved.messages
                    )
                    .to_string(),
                    false,
                    cx.theme(),
                )
                .on_click(cx.listener(move |this, _, window, cx| {
                    this.open(Some(id.clone()), false, window, cx)
                }))
                .on_mouse_down(
                    MouseButton::Right,
                    cx.listener(move |this, ev: &gpui::MouseDownEvent, _, cx| {
                        this.views[this.active].update(cx, |view, cx| {
                            view.session_menu = Some((menu_id.clone(), ev.position));
                            cx.notify();
                        });
                    }),
                ),
            );
        }
        let overlays = active.update(cx, |view, cx| {
            (
                view.render_session_menu(window, cx),
                view.render_session_delete(cx),
            )
        });
        div()
            .flex()
            .relative()
            .size_full()
            .bg(cx.theme().background)
            .when(sidebar, |layout| {
                layout.child(
                    div()
                        .v_flex()
                        .w(px(240.))
                        .h_full()
                        .flex_none()
                        .p_2()
                        .gap_2()
                        .bg(cx.theme().sidebar)
                        .border_r_1()
                        .border_color(cx.theme().sidebar_border)
                        .child(div().text_sm().child(t!("sidebar_title").to_string()))
                        .child(
                            Button::new("new-thread")
                                .small()
                                .label(t!("thread_new_local").to_string())
                                .disabled(self.creating)
                                .on_click(cx.listener(|this, _, window, cx| {
                                    this.open(None, false, window, cx)
                                })),
                        )
                        .child(
                            Button::new("new-worktree")
                                .small()
                                .label(t!("thread_new_worktree").to_string())
                                .disabled(self.creating || !can_worktree)
                                .on_click(cx.listener(|this, _, window, cx| {
                                    this.open(None, true, window, cx)
                                })),
                        )
                        .when(self.creating, |s| {
                            s.child(div().text_xs().child(t!("thread_creating").to_string()))
                        })
                        .child(rows),
                )
            })
            .child(
                div()
                    .v_flex()
                    .flex_1()
                    .min_w_0()
                    .h_full()
                    .children(self.error.as_ref().map(|error| {
                        div()
                            .p_2()
                            .text_sm()
                            .text_color(cx.theme().danger)
                            .child(error.clone())
                    }))
                    .child(div().flex_1().min_h_0().child(active)),
            )
            .children(overlays.0)
            .children(overlays.1)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use picocode_core::event::AgentEvent;

    #[gpui::test]
    fn switching_retains_streams_drafts_approvals_and_independent_stop(
        cx: &mut gpui::TestAppContext,
    ) {
        cx.update(gpui_component::init);
        let temp = tempfile::tempdir().unwrap();
        let attachment = temp.path().join("draft.txt");
        std::fs::write(&attachment, "draft attachment").unwrap();
        let rt = tokio::runtime::Runtime::new().unwrap();
        let mut args = config::Args::for_workspace(None);
        args.provider = Some(config::Provider::Ollama);
        args.model = Some(String::new());
        let cfg = config::Config::from_args_in(args, temp.path()).unwrap();
        let cx = cx.add_empty_window();
        let (threads, first, second, tx1, tx2, mut commands1, mut commands2, cancel1, cancel2) = cx
            .update(|window, cx| {
                let mut make = || {
                    let (tx, rx) = tokio::sync::mpsc::channel(256);
                    let (cmd, commands) = tokio::sync::mpsc::channel(256);
                    let (cancel, cancel_rx) = tokio::sync::watch::channel(());
                    let view = cx.new(|cx| {
                        let mut view = ChatView::new(
                            cfg.for_session(),
                            rx,
                            tx.clone(),
                            cmd,
                            Default::default(),
                            Default::default(),
                            cancel,
                            rt.handle().clone(),
                            Default::default(),
                            picocode_core::backend::Backend::Local,
                            window,
                            cx,
                        );
                        // Persistence is covered by the core integration tests.
                        view.sessions_dir = None;
                        view
                    });
                    (view, tx, commands, cancel_rx)
                };
                let (first, tx1, commands1, cancel1) = make();
                let (second, tx2, commands2, cancel2) = make();
                let threads = cx.new(|cx| ThreadsView::new(first.clone(), window, cx));
                threads.update(cx, |threads, cx| threads.retain(second.clone(), window, cx));
                first.update(cx, |view, cx| {
                    view.send_prompt("first prompt".into(), vec![]);
                    view.pending_attachments
                        .push(picocode_core::attachment::Attachment::detect(&attachment).unwrap());
                    view.input
                        .update(cx, |input, cx| input.set_value("first draft", window, cx));
                });
                second.update(cx, |view, _| {
                    view.send_prompt("second prompt".into(), vec![])
                });
                (
                    threads, first, second, tx1, tx2, commands1, commands2, cancel1, cancel2,
                )
            });
        cx.replace_root_view(|window, cx| gpui_component::Root::new(threads.clone(), window, cx));
        cx.run_until_parked();
        assert!(
            matches!(commands1.try_recv(), Ok(WorkerCmd::Prompt { text, .. }) if text == "first prompt")
        );
        assert!(
            matches!(commands2.try_recv(), Ok(WorkerCmd::Prompt { text, .. }) if text == "second prompt")
        );
        let focus = cx.update(|window, cx| window.focused(cx));
        tx1.try_send(AgentEvent::TextDelta("first response".into()))
            .unwrap();
        tx2.try_send(AgentEvent::TextDelta("second response".into()))
            .unwrap();
        let (respond, mut answer) = tokio::sync::oneshot::channel();
        tx1.try_send(AgentEvent::ApprovalRequest {
            name: "bash".into(),
            args: "{}".into(),
            respond,
        })
        .unwrap();
        cx.run_until_parked();
        cx.update(|window, cx| {
            assert_eq!(
                window.focused(cx),
                focus,
                "background approvals must not steal focus"
            );
            assert!(first.read(cx).approval.is_some());
            assert!(second.read(cx).approval.is_none());
            assert!(
                first
                    .read(cx)
                    .entries
                    .iter()
                    .any(|e| e.text == "first response")
            );
            assert!(
                !second
                    .read(cx)
                    .entries
                    .iter()
                    .any(|e| e.text == "first response")
            );
            threads.update(cx, |threads, cx| threads.select(0, window, cx));
            assert!(first.read(cx).dialog_focus.is_focused(window));
            assert_eq!(
                first.read(cx).input.read(cx).value().as_str(),
                "first draft"
            );
            assert!(first.read(cx).running && second.read(cx).running);
            assert_eq!(first.read(cx).pending_attachments.len(), 1);
            assert!(second.read(cx).pending_attachments.is_empty());
            first.update(cx, |view, _| {
                view.approval.take().unwrap().respond.send(true).unwrap()
            });
            threads.update(cx, |threads, cx| threads.select(1, window, cx));
            second.update(cx, |view, cx| {
                view.stop(&gpui::ClickEvent::default(), window, cx)
            });
        });
        assert!(answer.try_recv().unwrap());
        assert!(!cancel1.has_changed().unwrap());
        assert!(cancel2.has_changed().unwrap());
        tx1.try_send(AgentEvent::TurnComplete).unwrap();
        cx.run_until_parked();
        cx.update(|window, cx| {
            assert!(!first.read(cx).running);
            assert!(
                second.read(cx).running,
                "another worker's completion must not end this turn"
            );
            let id = first.read(cx).session_id.clone();
            threads.update(cx, |threads, cx| {
                threads.open(Some(id), false, window, cx);
                assert_eq!(threads.views.len(), 2);
                assert_eq!(threads.active, 0);
            });
        });
    }
}
