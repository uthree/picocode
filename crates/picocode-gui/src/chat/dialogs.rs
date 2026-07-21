//! Dialogs and popups: approval / plan-question, the right-click copy
//! menu, the queued-prompt list, command completions, the session picker
//! and the settings dialog.

use gpui::prelude::*;
use gpui::{AnyElement, App, ClipboardItem, Context, KeyDownEvent, SharedString, Window, div, px};
use gpui_component::button::{Button, ButtonVariants};
use gpui_component::{ActiveTheme, StyledExt};
use rust_i18n::t;

use picocode_core::attachment::AttachmentKind;
use picocode_core::config::{self};
use picocode_core::transcript::diff_lines;
use picocode_core::{approval, session};

use crate::settings::ThemeSetting;

use super::ChatView;
use super::status::{menu_row, mode_name};
use super::transcript::{diff_element, tool_style};
use super::{Approval, COMMANDS, clip, one_line};

impl ThemeSetting {
    fn label(self) -> String {
        match self {
            ThemeSetting::System => t!("theme_system").to_string(),
            ThemeSetting::Light => t!("theme_light").to_string(),
            ThemeSetting::Dark => t!("theme_dark").to_string(),
        }
    }
}

impl ChatView {
    pub(super) fn render_approval(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let a = self.approval.as_ref()?;
        let theme = cx.theme();
        let always_label = match approval::bash_command(&a.name, &a.args) {
            Some(cmd) => t!(
                "always_tool",
                what = format!("{} …", config::bash_allow_patterns(&cmd).join(", "))
            )
            .to_string(),
            None => t!("always_tool", what = a.name).to_string(),
        };
        Some(
            overlay()
                .child(
                    div()
                        .id("approval-dialog")
                        .track_focus(&self.dialog_focus)
                        .on_key_down(cx.listener(|this, ev: &KeyDownEvent, window, cx| {
                            match ev.keystroke.key.as_str() {
                                "y" => this.answer_approval(true, false, window, cx),
                                "n" | "escape" => this.answer_approval(false, false, window, cx),
                                "a" => this.answer_approval(true, true, window, cx),
                                _ => {}
                            }
                        }))
                        .v_flex()
                        .w(px(560.))
                        .max_h(px(420.))
                        .gap_3()
                        .p_4()
                        .rounded_lg()
                        .bg(theme.background)
                        .border_1()
                        .border_color(theme.border)
                        .child({
                            let (icon, color) = tool_style(&a.name);
                            div()
                                .h_flex()
                                .gap_2()
                                .items_center()
                                .child(
                                    gpui_component::Icon::default()
                                        .path(icon)
                                        .size_4()
                                        .flex_none()
                                        .text_color(color),
                                )
                                .child(
                                    div()
                                        .font_bold()
                                        .child(t!("run_tool", tool = a.name).to_string()),
                                )
                        })
                        .child(
                            div()
                                .id("approval-args")
                                .flex_1()
                                .overflow_y_scroll()
                                .text_sm()
                                .child(approval_body(
                                    a,
                                    theme.mono_font_family.clone(),
                                    theme.muted_foreground,
                                    cx,
                                )),
                        )
                        .child(
                            div()
                                .h_flex()
                                .gap_2()
                                .justify_end()
                                .child(Button::new("deny").label(t!("deny").to_string()).on_click(
                                    cx.listener(|this, _, window, cx| {
                                        this.answer_approval(false, false, window, cx)
                                    }),
                                ))
                                .child(Button::new("always").label(always_label).on_click(
                                    cx.listener(|this, _, window, cx| {
                                        this.answer_approval(true, true, window, cx)
                                    }),
                                ))
                                .child(
                                    Button::new("approve")
                                        .primary()
                                        .label(t!("approve").to_string())
                                        .on_click(cx.listener(|this, _, window, cx| {
                                            this.answer_approval(true, false, window, cx)
                                        })),
                                ),
                        ),
                )
                .into_any_element(),
        )
    }

    pub(super) fn render_question(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let q = self.question.as_ref()?;
        let theme = cx.theme();
        let options = q.options.iter().enumerate().map(|(ix, opt)| {
            Button::new(SharedString::from(format!("opt-{ix}")))
                .label(opt.clone())
                .on_click(cx.listener(move |this, _, window, cx| {
                    this.answer_question(Some(ix), window, cx)
                }))
                .into_any_element()
        });
        Some(
            overlay()
                .child(
                    div()
                        .id("question-dialog")
                        .track_focus(&self.dialog_focus)
                        .on_key_down(cx.listener(|this, ev: &KeyDownEvent, window, cx| {
                            if ev.keystroke.key.as_str() == "escape" {
                                this.answer_question(None, window, cx);
                            }
                        }))
                        .v_flex()
                        .w(px(560.))
                        .max_h(px(480.))
                        .gap_3()
                        .p_4()
                        .rounded_lg()
                        .bg(theme.background)
                        .border_1()
                        .border_color(theme.border)
                        .child(div().font_bold().child(q.title.clone()))
                        .child(
                            div()
                                .id("question-body")
                                .flex_1()
                                .overflow_y_scroll()
                                .text_sm()
                                .child(q.question.clone()),
                        )
                        .child(div().v_flex().gap_2().children(options))
                        .child(
                            div().h_flex().justify_end().child(
                                Button::new("dismiss")
                                    .label(t!("dismiss").to_string())
                                    .on_click(cx.listener(|this, _, window, cx| {
                                        this.answer_question(None, window, cx)
                                    })),
                            ),
                        ),
                )
                .into_any_element(),
        )
    }

    /// Right-click menu on a transcript entry: copy its text.
    pub(super) fn render_ctx_menu(
        &self,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let (ix, pos) = self.ctx_menu?;
        let text = self.entries.get(ix)?.text.clone();
        let theme = cx.theme();
        // Keep the panel inside the window.
        let viewport = window.viewport_size();
        let x = pos.x.min(viewport.width - px(240.)).max(px(0.));
        let y = pos.y.min(viewport.height - px(64.)).max(px(0.));
        Some(
            div()
                .absolute()
                .inset_0()
                .child(
                    div()
                        .id("ctx-menu-backdrop")
                        .absolute()
                        .inset_0()
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.ctx_menu = None;
                            cx.notify();
                        })),
                )
                .child(
                    div().absolute().left(x).top(y).occlude().child(
                        div()
                            .v_flex()
                            .w(px(220.))
                            .p_1()
                            .rounded_lg()
                            .bg(theme.background)
                            .border_1()
                            .border_color(theme.border)
                            .shadow_lg()
                            .text_sm()
                            .child(
                                menu_row(
                                    SharedString::from("ctx-copy"),
                                    t!("copy_text").to_string(),
                                    one_line(&text, 32),
                                    false,
                                    theme,
                                )
                                .on_click(cx.listener(
                                    move |this, _, _, cx| {
                                        cx.write_to_clipboard(ClipboardItem::new_string(
                                            text.clone(),
                                        ));
                                        this.ctx_menu = None;
                                        cx.notify();
                                    },
                                )),
                            ),
                    ),
                )
                .into_any_element(),
        )
    }

    /// Prompts held back while a turn runs, listed above the input box so
    /// it's clear they were accepted and will be sent, not dropped.
    pub(super) fn render_queued(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        if self.queued.is_empty() {
            return None;
        }
        let theme = cx.theme();
        let mut block = div()
            .v_flex()
            .gap_0p5()
            .pl_2()
            .border_l_2()
            .border_color(theme.border)
            .text_sm()
            .text_color(theme.muted_foreground)
            .child(t!("queued_n", n = self.queued.len()).to_string());
        for (text, attachments) in &self.queued {
            let mut line = format!("⏳ {}", one_line(text, 100));
            if !attachments.is_empty() {
                line.push_str(&format!(" (📎 {})", attachments.len()));
            }
            block = block.child(line);
        }
        Some(block.into_any_element())
    }

    /// Full-size view of a transcript image, opened by clicking its
    /// thumbnail. Clicking anywhere closes it.
    pub(super) fn render_image_preview(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let path = self.image_preview.clone()?;
        let name = path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        Some(
            overlay()
                .id("image-preview")
                .cursor_pointer()
                .bg(gpui::black().opacity(0.6))
                .on_click(cx.listener(|this, _, _, cx| {
                    this.image_preview = None;
                    cx.notify();
                }))
                .child(
                    div()
                        .v_flex()
                        .gap_2()
                        .items_center()
                        .max_w(gpui::relative(0.85))
                        .max_h(gpui::relative(0.85))
                        .child(
                            gpui::img(path)
                                .max_w(gpui::relative(1.))
                                .max_h(gpui::relative(1.))
                                .rounded_lg(),
                        )
                        .child(
                            div()
                                .text_sm()
                                .text_color(gpui::white().opacity(0.8))
                                .child(name),
                        ),
                )
                .into_any_element(),
        )
    }

    /// Chips for files staged to go with the next prompt: image thumbnails,
    /// icons for audio/PDF, each with a click-to-remove ✕.
    pub(super) fn render_attachments(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        if self.pending_attachments.is_empty() {
            return None;
        }
        let theme = cx.theme();
        let mut row = div().h_flex().gap_2().flex_wrap();
        for (ix, att) in self.pending_attachments.iter().enumerate() {
            let visual: AnyElement = match att.kind {
                AttachmentKind::Image => gpui::img(att.path.clone())
                    .h(px(40.))
                    .max_w(px(96.))
                    .rounded_md()
                    .into_any_element(),
                AttachmentKind::Audio => gpui_component::Icon::default()
                    .path("icons/music.svg")
                    .size_4()
                    .into_any_element(),
                AttachmentKind::Pdf | AttachmentKind::Text => gpui_component::Icon::default()
                    .path("icons/file-text.svg")
                    .size_4()
                    .into_any_element(),
            };
            row = row.child(
                div()
                    .id(SharedString::from(format!("att-{ix}")))
                    .h_flex()
                    .gap_1()
                    .items_center()
                    .px_2()
                    .py_1()
                    .rounded_md()
                    .bg(theme.muted)
                    .text_sm()
                    .child(visual)
                    .child(one_line(&att.name(), 40))
                    .child(
                        div()
                            .id(SharedString::from(format!("att-x-{ix}")))
                            .cursor_pointer()
                            .text_color(theme.muted_foreground)
                            .hover(|s| s.text_color(gpui::red()))
                            .child("✕")
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.pending_attachments.remove(ix);
                                cx.notify();
                            })),
                    ),
            );
        }
        Some(row.into_any_element())
    }

    /// Completion popup: matching slash commands, shown above the input
    /// while it holds a bare `/command` prefix. Click fills; Tab cycles.
    pub(super) fn render_completions(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        if self.dialog_open() {
            return None;
        }
        let value = self.input.read(cx).value().to_string();
        if !value.starts_with('/') || value.contains(char::is_whitespace) {
            return None;
        }
        let prefix = self.comp_prefix.clone().unwrap_or_else(|| value.clone());
        let matches: Vec<(&str, &str)> = COMMANDS
            .iter()
            .filter(|(name, _)| name.starts_with(&prefix))
            .copied()
            .collect();
        if matches.is_empty() {
            return None;
        }
        let theme = cx.theme();
        let mono = theme.mono_font_family.clone();
        let mut list = div()
            .id("completions")
            .v_flex()
            .max_h(px(240.))
            .overflow_y_scroll();
        for (name, desc) in matches {
            let fill = name.to_string();
            let active = name == value;
            list = list.child(
                div()
                    .id(SharedString::from(format!("comp-{name}")))
                    .cursor_pointer()
                    .h_flex()
                    .justify_between()
                    .gap_4()
                    .px_2()
                    .py_0p5()
                    .rounded_md()
                    .when(active, |s| s.bg(theme.muted))
                    .hover(|s| s.bg(theme.muted))
                    .child(div().font_family(mono.clone()).child(name))
                    .child(
                        div()
                            .text_sm()
                            .text_color(theme.muted_foreground)
                            .child(t!(desc).to_string()),
                    )
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.completing = true;
                        this.input
                            .update(cx, |state, cx| state.set_value(&fill, window, cx));
                        cx.notify();
                    })),
            );
        }
        Some(
            div()
                .mx_3()
                .p_1()
                .rounded_lg()
                .border_1()
                .border_color(theme.border)
                .bg(theme.background)
                .text_sm()
                .child(list)
                .into_any_element(),
        )
    }

    /// The `/resume` dialog: this project's saved sessions, newest first.
    pub(super) fn render_session_picker(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let sessions = self.session_picker.as_ref()?;
        let theme = cx.theme();
        let mut list = div()
            .id("session-picker-list")
            .v_flex()
            .gap_1()
            .overflow_y_scroll();
        for (ix, s) in sessions.iter().enumerate() {
            let id = s.id.clone();
            let title = if s.snippet.is_empty() {
                id.clone()
            } else {
                s.snippet.clone()
            };
            let detail = t!(
                "session_detail",
                age = session::age(s.modified),
                n = s.messages,
                model = s.model
            )
            .to_string();
            list = list.child(
                menu_row(
                    SharedString::from(format!("session-{ix}")),
                    title,
                    detail,
                    false,
                    theme,
                )
                .on_click(cx.listener(move |this, _, _, cx| this.resume_session(&id.clone(), cx))),
            );
        }
        Some(
            overlay()
                .child(
                    div()
                        .v_flex()
                        .w(px(560.))
                        .max_h(px(480.))
                        .gap_3()
                        .p_4()
                        .rounded_lg()
                        .bg(theme.background)
                        .border_1()
                        .border_color(theme.border)
                        .child(div().font_bold().child(t!("resume_title").to_string()))
                        .child(list)
                        .child(
                            div().h_flex().justify_end().child(
                                Button::new("resume-cancel")
                                    .label(t!("cancel").to_string())
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.session_picker = None;
                                        cx.notify();
                                    })),
                            ),
                        ),
                )
                .into_any_element(),
        )
    }

    /// The `/config` dialog: the same rows as the TUI's settings dialog,
    /// adjusted with −/+ buttons; every change applies immediately.
    pub(super) fn render_settings(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        if !self.settings_open {
            return None;
        }
        let theme = cx.theme();
        let search = self.cfg.search.snapshot();
        let rows: [(String, String); 9] = [
            (t!("row_theme").to_string(), self.theme_pref.label()),
            (t!("row_mode").to_string(), mode_name(self.cfg.mode.get())),
            (
                t!("row_bash_timeout").to_string(),
                format!("{}s", self.cfg.bash_timeout.get()),
            ),
            (
                t!("row_read_lines").to_string(),
                self.cfg.read_max_lines.get().to_string(),
            ),
            (
                t!("row_line_bytes").to_string(),
                self.cfg.read_max_line_bytes.get().to_string(),
            ),
            (
                t!("row_web_search").to_string(),
                search.provider.label().to_string(),
            ),
            (
                t!("row_results").to_string(),
                search.max_results.to_string(),
            ),
            (
                t!("row_auto_compact").to_string(),
                match self.cfg.auto_compact.get() {
                    0 => t!("auto_compact_off").to_string(),
                    pct => format!("{pct}%"),
                },
            ),
            (t!("row_model").to_string(), self.cfg.model_label()),
        ];

        let mut panel = div().v_flex().gap_1();
        let model_row = rows.len() - 1;
        for (ix, (label, value)) in rows.into_iter().enumerate() {
            // The model row is a single button opening the model menu; the
            // others adjust in place with −/+.
            let controls: AnyElement = if ix == model_row {
                Button::new("cfg-model")
                    .label(value)
                    .on_click(
                        cx.listener(move |this, _, _, cx| this.adjust_setting(model_row, 1, cx)),
                    )
                    .into_any_element()
            } else {
                div()
                    .h_flex()
                    .gap_2()
                    .items_center()
                    .child(
                        Button::new(SharedString::from(format!("cfg-dec-{ix}")))
                            .ghost()
                            .label("−")
                            .on_click(
                                cx.listener(move |this, _, _, cx| this.adjust_setting(ix, -1, cx)),
                            ),
                    )
                    .child(div().min_w(px(110.)).text_center().child(value))
                    .child(
                        Button::new(SharedString::from(format!("cfg-inc-{ix}")))
                            .ghost()
                            .label("+")
                            .on_click(
                                cx.listener(move |this, _, _, cx| this.adjust_setting(ix, 1, cx)),
                            ),
                    )
                    .into_any_element()
            };
            panel = panel.child(
                div()
                    .h_flex()
                    .justify_between()
                    .items_center()
                    .px_2()
                    .py_1()
                    .child(label)
                    .child(controls),
            );
        }

        Some(
            overlay()
                .child(
                    div()
                        .v_flex()
                        .w(px(460.))
                        .gap_3()
                        .p_4()
                        .rounded_lg()
                        .bg(theme.background)
                        .border_1()
                        .border_color(theme.border)
                        .child(div().font_bold().child(t!("settings_title").to_string()))
                        .child(
                            div()
                                .text_sm()
                                .text_color(theme.muted_foreground)
                                .child(t!("settings_note").to_string()),
                        )
                        .child(panel)
                        .child(
                            div().h_flex().justify_end().child(
                                Button::new("settings-close")
                                    .primary()
                                    .label(t!("close").to_string())
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.settings_open = false;
                                        cx.notify();
                                    })),
                            ),
                        ),
                )
                .into_any_element(),
        )
    }
}

/// Full-window dimmed backdrop for dialogs.
fn overlay() -> gpui::Div {
    div()
        .absolute()
        .inset_0()
        .flex()
        .items_center()
        .justify_center()
        .bg(gpui::black().opacity(0.4))
}

/// Body of the approval dialog: `edit_file` shows the path plus a colored
/// diff, `bash` the command line, anything else pretty-printed JSON args.
fn approval_body(a: &Approval, mono: SharedString, muted: gpui::Hsla, cx: &App) -> AnyElement {
    let parsed: Option<serde_json::Value> = serde_json::from_str(&a.args).ok();
    let get = |k: &str| {
        parsed
            .as_ref()
            .and_then(|v| v.get(k))
            .and_then(|v| v.as_str())
    };
    if a.name == "edit_file"
        && let (Some(path), Some(new)) = (get("path"), get("new_string"))
    {
        let old = get("old_string").unwrap_or_default();
        let diff = diff_lines(old, new).join("\n");
        return div()
            .v_flex()
            .gap_2()
            .child(
                div()
                    .font_family(mono.clone())
                    .text_color(muted)
                    .child(format!("path: {path}")),
            )
            .child(diff_element(&clip(&diff, 200), Some(path), mono, cx))
            .into_any_element();
    }
    if a.name == "bash"
        && let Some(cmd) = get("command")
    {
        return div()
            .font_family(mono)
            .child(cmd.to_string())
            .into_any_element();
    }
    let pretty = parsed
        .as_ref()
        .and_then(|v| serde_json::to_string_pretty(v).ok())
        .unwrap_or_else(|| a.args.clone());
    div()
        .font_family(mono)
        .text_color(muted)
        .child(clip(&pretty, 60))
        .into_any_element()
}
