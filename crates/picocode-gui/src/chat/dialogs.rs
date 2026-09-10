//! Dialogs and popups: approval / plan-question, the right-click copy
//! menu, the queued-prompt list, command completions, the session picker
//! and the settings dialog.

use gpui::prelude::*;
use gpui::{AnyElement, App, ClipboardItem, Context, KeyDownEvent, SharedString, Window, div, px};
use gpui_component::button::{Button, ButtonVariants};
use gpui_component::input::Input;
use gpui_component::{ActiveTheme, StyledExt};
use rust_i18n::t;

use picocode_core::attachment::AttachmentKind;
use picocode_core::config::{self, Group, SettingId};
use picocode_core::transcript::diff_lines;
use picocode_core::{approval, session};

use crate::theme::ThemeSetting;

use super::ChatView;
use super::status::{menu_row, mode_name};
use super::transcript::{diff_element, tool_style};
use super::{Approval, clip, one_line};

impl ThemeSetting {
    fn label(self) -> String {
        match self {
            ThemeSetting::System => t!("theme_system").to_string(),
            ThemeSetting::Light => t!("theme_light").to_string(),
            ThemeSetting::Dark => t!("theme_dark").to_string(),
        }
    }
}

/// One `/config` row the GUI shows: the shared table, plus the appearance
/// and color-theme pickers, which only the GUI has, and the raw-transcript
/// toggle, which both front ends have but neither keeps in
/// [`picocode_core::config::Config`].
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) enum GuiSetting {
    Shared(SettingId),
    Theme,
    ThemeFamily,
    RawView,
}

impl GuiSetting {
    fn group(self) -> Group {
        match self {
            GuiSetting::Shared(id) => id.group(),
            GuiSetting::Theme | GuiSetting::ThemeFamily | GuiSetting::RawView => Group::Interface,
        }
    }

    fn label(self) -> String {
        match self {
            GuiSetting::Shared(id) => id.label(),
            GuiSetting::Theme => t!("row_theme").to_string(),
            GuiSetting::ThemeFamily => t!("row_color_theme").to_string(),
            GuiSetting::RawView => picocode_core::config::raw_view_label(),
        }
    }

    fn is_action(self) -> bool {
        matches!(self, GuiSetting::Shared(id) if id.is_action())
    }
}

/// Every `/config` row, in display order, grouped by section. The theme
/// rows lead the Interface section: they are what most people come here
/// for, and they are the two the GUI adds.
fn settings_order() -> Vec<GuiSetting> {
    let mut order = Vec::new();
    for group in Group::ALL {
        if group == Group::Interface {
            order.push(GuiSetting::Theme);
            order.push(GuiSetting::ThemeFamily);
            order.push(GuiSetting::RawView);
        }
        order.extend(
            SettingId::SHARED
                .into_iter()
                .filter(|id| id.group() == group)
                .map(GuiSetting::Shared),
        );
    }
    order
}

impl ChatView {
    /// The displayed value of one `/config` row. The shared rows come from
    /// core's table; the mode is the exception, since the GUI names the
    /// modes itself.
    fn setting_value(&self, setting: GuiSetting) -> String {
        match setting {
            GuiSetting::Theme => self.theme_pref.label(),
            GuiSetting::ThemeFamily => self.theme_family.clone(),
            GuiSetting::RawView => picocode_core::config::on_off(self.raw_view),
            GuiSetting::Shared(SettingId::Mode) => mode_name(self.cfg.mode.get()),
            GuiSetting::Shared(id) => id.value(&self.cfg),
        }
    }

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
                            let (icon, color) = tool_style(&a.name, theme);
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
                                .child(div().font_bold().child(match a.agent_id {
                                    Some(id) => {
                                        t!("subagent_approval", id = id, tool = a.name).to_string()
                                    }
                                    None => t!("run_tool", tool = a.name).to_string(),
                                }))
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
        let matches = self.completion_matches(cx);
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
        for (ix, (fill, desc)) in matches.into_iter().enumerate() {
            let active = fill == value;
            let name = fill.clone();
            list = list.child(
                div()
                    .id(SharedString::from(format!("comp-{ix}")))
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
                            .child(desc),
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

        let mut panel = div().v_flex().gap_1();
        let mut group = None;
        for (ix, setting) in settings_order().into_iter().enumerate() {
            if group != Some(setting.group()) {
                group = Some(setting.group());
                panel = panel.child(
                    div()
                        .px_2()
                        .pt_2()
                        .text_xs()
                        .text_color(theme.muted_foreground)
                        .child(setting.group().label()),
                );
            }
            let value = self.setting_value(setting);
            // The model row is a single button opening the model menu; the
            // others adjust in place with −/+.
            let controls: AnyElement = if setting.is_action() {
                Button::new("cfg-model")
                    .label(value)
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.adjust_setting(setting, 1, window, cx)
                    }))
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
                            .on_click(cx.listener(move |this, _, window, cx| {
                                this.adjust_setting(setting, -1, window, cx)
                            })),
                    )
                    .child(div().min_w(px(130.)).text_center().child(value))
                    .child(
                        Button::new(SharedString::from(format!("cfg-inc-{ix}")))
                            .ghost()
                            .label("+")
                            .on_click(cx.listener(move |this, _, window, cx| {
                                this.adjust_setting(setting, 1, window, cx)
                            })),
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
                    .child(setting.label())
                    .child(controls),
            );
        }

        Some(
            overlay()
                .child(
                    div()
                        .v_flex()
                        // The rows outgrew a short window once they were
                        // sectioned, so the list scrolls inside the dialog
                        // rather than the title and the close button
                        // sliding off the screen with it.
                        .w(px(460.))
                        .max_h(px(560.))
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
                        .child(
                            div()
                                .id("settings-rows")
                                .flex_1()
                                .overflow_y_scroll()
                                .child(panel),
                        )
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

impl ChatView {
    /// The add-model dialog: provider (click cycles), base URL, model name,
    /// and the endpoint's served models (fetched on demand; clicking one
    /// switches to it directly).
    /// The `/prompt` dialog: a multi-line editor over the base system
    /// prompt with Apply / Reset-to-default / Cancel.
    pub(super) fn render_prompt_edit(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let editor = self.prompt_edit.as_ref()?;
        let theme = cx.theme();
        Some(
            overlay()
                .child(
                    div()
                        .id("prompt-dialog")
                        .on_key_down(cx.listener(|this, ev: &KeyDownEvent, _, cx| {
                            if ev.keystroke.key.as_str() == "escape" {
                                this.prompt_edit = None;
                                cx.notify();
                            }
                        }))
                        .v_flex()
                        .w(px(620.))
                        .gap_3()
                        .p_4()
                        .rounded_lg()
                        .bg(theme.background)
                        .border_1()
                        .border_color(theme.border)
                        .child(div().font_bold().child(t!("prompt_title").to_string()))
                        .child(
                            div()
                                .text_sm()
                                .text_color(theme.muted_foreground)
                                .child(t!("prompt_hint").to_string()),
                        )
                        .child(Input::new(editor))
                        .child(
                            div()
                                .h_flex()
                                .gap_2()
                                .justify_end()
                                .child(
                                    Button::new("prompt-reset")
                                        .ghost()
                                        .label(t!("prompt_reset_btn").to_string())
                                        .on_click(cx.listener(|this, _, _, cx| {
                                            this.prompt_edit = None;
                                            this.apply_system_prompt(None, cx);
                                        })),
                                )
                                .child(
                                    Button::new("prompt-cancel")
                                        .ghost()
                                        .label(t!("cancel").to_string())
                                        .on_click(cx.listener(|this, _, _, cx| {
                                            this.prompt_edit = None;
                                            cx.notify();
                                        })),
                                )
                                .child(
                                    Button::new("prompt-apply")
                                        .primary()
                                        .label(t!("prompt_apply").to_string())
                                        .on_click(cx.listener(|this, _, _, cx| {
                                            this.apply_prompt_edit(cx);
                                        })),
                                ),
                        ),
                )
                .into_any_element(),
        )
    }

    /// The add-remote dialog: an ssh destination and a path on it, with
    /// the `~/.ssh/config` aliases listed to click.
    pub(super) fn render_add_remote(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let dlg = self.add_remote.as_ref()?;
        let theme = cx.theme();

        let mut list = div().v_flex().gap_1().max_h(px(140.)).overflow_hidden();
        if !dlg.hosts.is_empty() {
            let mut rows = div()
                .id("add-remote-hosts")
                .v_flex()
                .gap_1()
                .overflow_y_scroll();
            for (ix, alias) in dlg.hosts.iter().enumerate() {
                let host = alias.clone();
                rows = rows.child(
                    div()
                        .id(SharedString::from(format!("add-remote-host-{ix}")))
                        .cursor_pointer()
                        .rounded_md()
                        .px_2()
                        .py_0p5()
                        .hover(|s| s.bg(theme.muted))
                        .child(alias.clone())
                        .on_click(cx.listener(move |this, _, window, cx| {
                            this.add_remote_pick_host(host.clone(), window, cx);
                        })),
                );
            }
            list = list.child(rows);
        }

        let row = |label: String, control: AnyElement| {
            div()
                .h_flex()
                .gap_3()
                .items_center()
                .child(
                    div()
                        .w(px(90.))
                        .text_color(theme.muted_foreground)
                        .child(label),
                )
                .child(div().flex_1().child(control))
        };

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
                        .child(div().font_bold().child(t!("add_remote_title").to_string()))
                        .child(row(
                            t!("row_remote_name").to_string(),
                            Input::new(&dlg.name).into_any_element(),
                        ))
                        .child(row(
                            t!("row_remote_host").to_string(),
                            Input::new(&dlg.host).into_any_element(),
                        ))
                        .child(row(
                            t!("row_remote_path").to_string(),
                            Input::new(&dlg.path).into_any_element(),
                        ))
                        .child(
                            div()
                                .text_sm()
                                .text_color(theme.muted_foreground)
                                .child(dlg.note.clone()),
                        )
                        .child(list)
                        .child(
                            div()
                                .h_flex()
                                .gap_2()
                                .justify_end()
                                .child(
                                    Button::new("add-remote-cancel")
                                        .ghost()
                                        .label(t!("cancel").to_string())
                                        .on_click(cx.listener(|this, _, _, cx| {
                                            this.add_remote = None;
                                            cx.notify();
                                        })),
                                )
                                .child(
                                    Button::new("add-remote-connect")
                                        .primary()
                                        .label(t!("connect_btn").to_string())
                                        .on_click(cx.listener(|this, _, _, cx| {
                                            this.add_remote_connect(cx);
                                        })),
                                ),
                        ),
                )
                .into_any_element(),
        )
    }

    pub(super) fn render_add_model(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let dlg = self.add_model.as_ref()?;
        let theme = cx.theme();
        let provider_name = picocode_core::config::provider_name(dlg.provider);

        let mut list = div().v_flex().gap_1().max_h(px(180.)).overflow_hidden();
        if !dlg.fetched.is_empty() {
            let mut rows = div()
                .id("add-model-fetched")
                .v_flex()
                .gap_1()
                .overflow_y_scroll();
            for (ix, id) in dlg.fetched.iter().enumerate() {
                let model = id.clone();
                rows = rows.child(
                    div()
                        .id(SharedString::from(format!("add-model-{ix}")))
                        .cursor_pointer()
                        .rounded_md()
                        .px_2()
                        .py_0p5()
                        .hover(|s| s.bg(theme.muted))
                        .child(id.clone())
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.add_model_switch(Some(model.clone()), cx);
                        })),
                );
            }
            list = list.child(rows);
        }

        let row = |label: String, control: AnyElement| {
            div()
                .h_flex()
                .gap_3()
                .items_center()
                .child(
                    div()
                        .w(px(90.))
                        .text_color(theme.muted_foreground)
                        .child(label),
                )
                .child(div().flex_1().child(control))
        };

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
                        .child(div().font_bold().child(t!("add_model_title").to_string()))
                        .child(row(
                            t!("row_provider").to_string(),
                            Button::new("add-model-provider")
                                .label(format!("‹ {provider_name} ›"))
                                .on_click(
                                    cx.listener(|this, _, _, cx| this.add_model_cycle_provider(cx)),
                                )
                                .into_any_element(),
                        ))
                        .child(row(
                            t!("row_base_url").to_string(),
                            Input::new(&dlg.base_url).into_any_element(),
                        ))
                        .child(row(
                            t!("row_model_name").to_string(),
                            Input::new(&dlg.model).into_any_element(),
                        ))
                        .child(
                            div()
                                .text_sm()
                                .text_color(theme.muted_foreground)
                                .child(dlg.note.clone()),
                        )
                        .child(list)
                        .child(
                            div()
                                .h_flex()
                                .gap_2()
                                .justify_end()
                                .child(
                                    Button::new("add-model-fetch")
                                        .ghost()
                                        .label(t!("fetch_models").to_string())
                                        .on_click(
                                            cx.listener(|this, _, _, cx| this.add_model_fetch(cx)),
                                        ),
                                )
                                .child(
                                    Button::new("add-model-cancel")
                                        .ghost()
                                        .label(t!("cancel").to_string())
                                        .on_click(cx.listener(|this, _, _, cx| {
                                            this.add_model = None;
                                            cx.notify();
                                        })),
                                )
                                .child(
                                    Button::new("add-model-switch")
                                        .primary()
                                        .label(t!("switch_btn").to_string())
                                        .on_click(cx.listener(|this, _, _, cx| {
                                            this.add_model_switch(None, cx);
                                        })),
                                ),
                        ),
                )
                .into_any_element(),
        )
    }
}

/// Full-window dimmed backdrop for dialogs.
///
/// `occlude` is what makes the dialog modal: dimming alone still let clicks
/// through to whatever sat behind, and clicking the dark area is exactly
/// what people do to dismiss a dialog. A click landing on a sidebar row
/// there would resume another session behind the open dialog.
pub(super) fn overlay() -> gpui::Div {
    div()
        .absolute()
        .inset_0()
        .occlude()
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

#[cfg(test)]
mod tests {
    use super::*;

    /// The dialog renders `settings_order` straight through, emitting a
    /// heading when the section changes — so a row out of group order
    /// would put a second heading with the same name further down.
    #[test]
    fn each_section_runs_once() {
        let mut seen: Vec<Group> = Vec::new();
        let mut last = None;
        for setting in settings_order() {
            if last != Some(setting.group()) {
                assert!(
                    !seen.contains(&setting.group()),
                    "{:?} is split across the dialog",
                    setting.group()
                );
                seen.push(setting.group());
                last = Some(setting.group());
            }
        }
        assert_eq!(seen, Group::ALL.to_vec());
    }

    /// The theme rows are the GUI's own; everything else comes from core,
    /// and all of it is rendered.
    #[test]
    fn the_shared_table_is_rendered_whole() {
        let order = settings_order();
        for id in SettingId::SHARED {
            assert!(order.contains(&GuiSetting::Shared(id)), "{id:?} is missing");
        }
        assert!(order.contains(&GuiSetting::Theme));
        assert!(order.contains(&GuiSetting::ThemeFamily));
        assert!(order.contains(&GuiSetting::RawView));
    }

    /// One `-`/`+` pair per row, keyed by position: a duplicate key would
    /// make two rows' buttons collide in gpui's element tree.
    #[test]
    fn every_row_gets_a_distinct_button_key() {
        let keys: Vec<String> = (0..settings_order().len())
            .map(|ix| format!("cfg-dec-{ix}"))
            .collect();
        let mut unique = keys.clone();
        unique.sort();
        unique.dedup();
        assert_eq!(keys.len(), unique.len());
    }
}
