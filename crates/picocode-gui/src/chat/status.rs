//! The status bar (mode chip, activity, background jobs, context gauge,
//! model chip) and the popup menus it opens.

use std::path::{Path, PathBuf};

use gpui::prelude::*;
use gpui::{AnyElement, Context, SharedString, div, px};
use gpui_component::{ActiveTheme, Sizable, StyledExt};
use rust_i18n::t;

use picocode_core::config::Mode;
use picocode_core::models;

use super::ChatView;
use super::Menu;

impl ChatView {
    /// Fraction of the model's context window used by the latest request.
    fn context_ratio(&self) -> f64 {
        self.tokens_in as f64 / self.cfg.context_window.max(1) as f64
    }

    /// Output tokens for display: the last reported count, plus the live
    /// estimate while a completion is streaming (the estimate keeps the
    /// counter moving between usage reports and snaps to the real number
    /// on each one).
    fn tokens_out_live(&self) -> String {
        (self.tokens_out + self.est_out).to_string()
    }

    /// Status bar, matching the TUI's layout: the clickable mode chip and
    /// the activity state on the left; the context gauge and the clickable
    /// model chip on the right.
    pub(super) fn render_status_bar(&self, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme();
        let muted_fg = theme.muted_foreground;
        let mode = self.cfg.mode.get();

        // Context usage gauge, colored by pressure (same thresholds as the
        // TUI: red ≥ 85%, yellow ≥ 60%).
        let ratio = self.context_ratio().min(1.0);
        let gauge_color = if ratio >= 0.85 {
            theme.danger
        } else if ratio >= 0.6 {
            theme.warning
        } else {
            gpui::rgb(0x3fb950).into()
        };
        const GAUGE_W: f32 = 96.;
        let gauge = div()
            .id("context-gauge")
            .cursor_pointer()
            .rounded_md()
            .px_1()
            .hover(|s| s.bg(theme.muted))
            .on_click(
                cx.listener(|this, _, window, cx| this.toggle_menu(Menu::Context, window, cx)),
            )
            .h_flex()
            .gap_2()
            .items_center()
            .child(
                div()
                    .w(px(GAUGE_W))
                    .h(px(6.))
                    .rounded_full()
                    .bg(theme.muted)
                    .child(
                        div()
                            .w(px(GAUGE_W * ratio as f32))
                            .h_full()
                            .rounded_full()
                            .bg(gauge_color),
                    ),
            )
            .child({
                // While running: nothing extra during the API wait (the
                // spinner already says "waiting"), then ↓ + tok/s while
                // tokens stream in. Idle shows both totals.
                let counters = if self.running && self.waiting {
                    String::new()
                } else if self.running {
                    let rate = match self.speed.rate() {
                        Some(rate) => format!(" · {} tok/s", rate.round().max(1.0) as u64),
                        None => String::new(),
                    };
                    format!("  ↓ {}{rate}", self.tokens_out_live())
                } else {
                    format!("  ↑ {} ↓ {}", self.tokens_in, self.tokens_out_live())
                };
                format!("{}%{counters}", (ratio * 100.0).round() as u64)
            });

        // Animated spinner while a turn runs; "waiting" until the first
        // token arrives (like the TUI), "generating" after.
        let state: AnyElement = if self.running {
            let label = if self.waiting {
                t!("waiting")
            } else {
                t!("generating")
            };
            div()
                .h_flex()
                .gap_1()
                .items_center()
                .child(
                    gpui_component::spinner::Spinner::new()
                        .icon(gpui_component::Icon::default().path("icons/loader-circle.svg"))
                        .xsmall(),
                )
                .child(label.to_string())
                .into_any_element()
        } else {
            div().child(t!("idle").to_string()).into_any_element()
        };

        // Button-like pill: mode color as the fill, like the Send button.
        let mode_chip = div()
            .id("mode-chip")
            .cursor_pointer()
            .rounded(theme.radius)
            .px_2()
            .py_0p5()
            .bg(mode_color(mode))
            .text_color(gpui::white())
            .hover(|s| s.opacity(0.85))
            .child(mode_name(mode))
            .on_click(cx.listener(|this, _, window, cx| this.toggle_menu(Menu::Mode, window, cx)));
        let model_chip = div()
            .id("model-chip")
            .cursor_pointer()
            .rounded_md()
            .px_2()
            .hover(|s| s.bg(theme.muted))
            .child(self.cfg.model_label())
            .on_click(cx.listener(|this, _, window, cx| this.toggle_menu(Menu::Model, window, cx)));

        div()
            .h_flex()
            .justify_between()
            .px_3()
            .pb_2()
            .text_sm()
            .text_color(muted_fg)
            .child(
                div()
                    .h_flex()
                    .gap_3()
                    .child(mode_chip)
                    .child(state)
                    .children((!self.bg_jobs.is_empty()).then(|| {
                        div()
                            .id("bg-jobs")
                            .cursor_pointer()
                            .rounded_md()
                            .px_1()
                            .hover(|s| s.bg(theme.muted))
                            .text_color(theme.warning)
                            .child(t!("bg_jobs", n = self.bg_jobs.len()).to_string())
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.toggle_menu(Menu::Background, window, cx)
                            }))
                    })),
            )
            .child(
                div()
                    .h_flex()
                    .gap_3()
                    .items_center()
                    .child(gauge)
                    .child(model_chip),
            )
            .into_any_element()
    }

    /// The open status-bar menu (mode or model picker), anchored above its
    /// chip — mode bottom-left, model bottom-right — with a click-away
    /// backdrop.
    pub(super) fn render_menu(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let menu = self.menu?;
        let theme = cx.theme();
        let current_mode = self.cfg.mode.get();

        let mut panel = div()
            .v_flex()
            .w(px(340.))
            .max_h(px(360.))
            .p_1()
            .gap_1()
            .rounded_lg()
            .bg(theme.background)
            .border_1()
            .border_color(theme.border)
            .shadow_lg();
        match menu {
            Menu::Mode => {
                for (mode, desc) in [
                    (Mode::ReadOnly, "mode_read_only_desc"),
                    (Mode::Edit, "mode_edit_desc"),
                    (Mode::Plan, "mode_plan_desc"),
                    (Mode::Bypass, "mode_bypass_desc"),
                ] {
                    let active = mode == current_mode;
                    panel = panel.child(
                        menu_row(
                            SharedString::from(format!("mode-{}", mode.label())),
                            mode_name(mode),
                            t!(desc).to_string(),
                            active,
                            theme,
                        )
                        .on_click(cx.listener(move |this, _, _, cx| this.select_mode(mode, cx))),
                    );
                }
            }
            Menu::Model => {
                let needle = self.model_filter.read(cx).value().trim().to_lowercase();
                let choices: Vec<_> = models::model_choices(
                    &self.cfg.models,
                    self.cfg.active_model.as_deref(),
                    self.cfg.provider,
                    self.cfg.base_url.as_deref(),
                    &self.cfg.model,
                    &self.available_models,
                )
                .into_iter()
                .filter(|c| {
                    needle.is_empty()
                        || c.name.to_lowercase().contains(&needle)
                        || c.detail.to_lowercase().contains(&needle)
                })
                .collect();
                panel = panel.child(gpui_component::input::Input::new(&self.model_filter).small());
                if choices.is_empty() {
                    panel = panel.child(
                        div()
                            .p_2()
                            .text_sm()
                            .text_color(theme.muted_foreground)
                            .child(t!("fetching_models").to_string()),
                    );
                }
                let mut list = div()
                    .id("model-menu-list")
                    .v_flex()
                    .gap_1()
                    .overflow_y_scroll();
                for (ix, choice) in choices.iter().enumerate() {
                    let name = choice.name.clone();
                    list = list.child(
                        menu_row(
                            SharedString::from(format!("model-{ix}")),
                            &choice.name,
                            choice.detail.clone(),
                            choice.active,
                            theme,
                        )
                        .on_click(
                            cx.listener(move |this, _, _, cx| this.switch_model(&name.clone(), cx)),
                        ),
                    );
                }
                // Trailing row opening the add-model dialog.
                list = list.child(
                    menu_row(
                        SharedString::from("model-add"),
                        t!("add_model_row").to_string(),
                        t!("add_model_row_desc").to_string(),
                        false,
                        theme,
                    )
                    .on_click(cx.listener(|this, _, window, cx| this.open_add_model(window, cx))),
                );
                panel = panel.child(list);
            }
            Menu::Workspace => {
                panel = panel.child(
                    div()
                        .px_2()
                        .py_1()
                        .font_bold()
                        .child(t!("ws_title").to_string()),
                );
                // The local project first, then every configured remote —
                // a host's filesystem has no native picker, so [[remotes]]
                // entries are how a remote root is chosen.
                panel = panel.child(
                    menu_row(
                        SharedString::from("ws-local"),
                        t!("ws_local").to_string(),
                        picocode_core::git::display_dir(&local_root()),
                        self.cfg.remote.is_none(),
                        theme,
                    )
                    .on_click(cx.listener(|this, _, _, cx| this.switch_workspace("local", cx))),
                );
                for (ix, entry) in self.cfg.remotes.iter().enumerate() {
                    let active = self.cfg.remote.as_ref().is_some_and(|spec| {
                        spec.destination == entry.host && spec.path == Path::new(&entry.path)
                    });
                    let name = entry.name.clone();
                    panel = panel.child(
                        menu_row(
                            SharedString::from(format!("ws-remote-{ix}")),
                            entry.name.clone(),
                            format!("{}:{}", entry.host, entry.path),
                            active,
                            theme,
                        )
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.switch_workspace(&name.clone(), cx)
                        })),
                    );
                }
                if self.cfg.remotes.is_empty() {
                    panel = panel.child(
                        div()
                            .px_2()
                            .py_1()
                            .text_sm()
                            .text_color(theme.muted_foreground)
                            .child(t!("ws_no_remotes").to_string()),
                    );
                }
                panel = panel.child(
                    menu_row(
                        SharedString::from("ws-folder"),
                        t!("ws_choose_folder").to_string(),
                        t!("ws_choose_folder_desc").to_string(),
                        false,
                        theme,
                    )
                    .on_click(cx.listener(|this, _, _, cx| this.pick_workdir(cx))),
                );
            }
            Menu::Background => {
                panel = panel.child(
                    div()
                        .px_2()
                        .py_1()
                        .font_bold()
                        .child(t!("bg_title").to_string()),
                );
                if self.bg_jobs.is_empty() {
                    panel = panel.child(
                        div()
                            .px_2()
                            .py_1()
                            .text_sm()
                            .text_color(theme.muted_foreground)
                            .child(t!("bg_none").to_string()),
                    );
                }
                for (id, command, started) in &self.bg_jobs {
                    let secs = started.elapsed().as_secs();
                    let elapsed = if secs >= 60 {
                        format!("{}m{:02}s", secs / 60, secs % 60)
                    } else {
                        format!("{secs}s")
                    };
                    let job_id = *id;
                    panel = panel.child(
                        div()
                            .px_2()
                            .py_1()
                            .child(
                                div()
                                    .h_flex()
                                    .gap_2()
                                    .justify_between()
                                    .child(format!("#{id}"))
                                    .child(
                                        div()
                                            .h_flex()
                                            .gap_2()
                                            .items_center()
                                            .child(
                                                div()
                                                    .text_color(theme.muted_foreground)
                                                    .text_sm()
                                                    .child(
                                                        t!("bg_elapsed", elapsed = elapsed)
                                                            .to_string(),
                                                    ),
                                            )
                                            .child(
                                                gpui_component::button::Button::new(
                                                    SharedString::from(format!("kill-{id}")),
                                                )
                                                .small()
                                                .label(t!("bg_kill").to_string())
                                                .on_click(cx.listener(move |this, _, _, cx| {
                                                    this.kill_job(job_id, cx);
                                                })),
                                            ),
                                    ),
                            )
                            .child(
                                div()
                                    .font_family(theme.mono_font_family.clone())
                                    .text_sm()
                                    .text_color(theme.muted_foreground)
                                    .truncate()
                                    .child(command.clone()),
                            ),
                    );
                }
            }
            Menu::Context => {
                let pct = (self.context_ratio() * 100.0).round() as u64;
                let rows: [(String, String); 5] = [
                    (t!("ctx_model").to_string(), self.cfg.model_label()),
                    (
                        t!("ctx_window").to_string(),
                        format!("{}", self.cfg.context_window),
                    ),
                    (
                        t!("ctx_used").to_string(),
                        format!("{} ({pct}%)", self.tokens_in),
                    ),
                    (t!("ctx_output").to_string(), self.tokens_out_live()),
                    (
                        t!("row_auto_compact").to_string(),
                        match self.cfg.auto_compact.get() {
                            0 => t!("auto_compact_off").to_string(),
                            p => format!("{p}%"),
                        },
                    ),
                ];
                panel = panel.child(
                    div()
                        .px_2()
                        .py_1()
                        .font_bold()
                        .child(t!("ctx_title").to_string()),
                );
                for (label, value) in rows {
                    panel = panel.child(
                        div()
                            .h_flex()
                            .gap_3()
                            .justify_between()
                            .px_2()
                            .py_0p5()
                            .child(div().text_color(theme.muted_foreground).child(label))
                            .child(value),
                    );
                }
            }
        }

        Some(
            div()
                .absolute()
                .inset_0()
                .child(
                    div()
                        .id("menu-backdrop")
                        .absolute()
                        .inset_0()
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.menu = None;
                            cx.notify();
                        })),
                )
                .child({
                    let anchored = div().absolute().bottom(px(36.)).occlude();
                    match menu {
                        Menu::Mode | Menu::Background | Menu::Workspace => anchored.left(px(12.)),
                        Menu::Model | Menu::Context => anchored.right(px(12.)),
                    }
                    .child(panel)
                })
                .into_any_element(),
        )
    }
}

/// The directory `/remote local` would open: the project root is
/// rediscovered from the process working directory, which a remote
/// workspace never changes.
fn local_root() -> PathBuf {
    std::env::current_dir().unwrap_or_default()
}

/// Localized display name for a permission mode (the technical /status
/// and /permissions blocks keep the English names).
pub(super) fn mode_name(mode: Mode) -> String {
    match mode {
        Mode::ReadOnly => t!("mode_name_read_only").to_string(),
        Mode::Edit => t!("mode_name_edit").to_string(),
        Mode::Plan => t!("mode_name_plan").to_string(),
        Mode::Bypass => t!("mode_name_bypass").to_string(),
    }
}

/// Status-bar color per permission mode (mirrors the TUI's palette).
pub(super) fn mode_color(mode: Mode) -> gpui::Hsla {
    let rgb = match mode {
        Mode::ReadOnly => 0x0ea5e9, // cyan
        Mode::Edit => 0xeab308,     // yellow
        Mode::Plan => 0x3b82f6,     // blue
        Mode::Bypass => 0xef4444,   // red
    };
    gpui::rgb(rgb).into()
}

/// One clickable row of a status-bar menu: name, dimmed detail, and a check
/// mark on the active item.
pub(super) fn menu_row(
    id: SharedString,
    name: impl Into<SharedString>,
    detail: String,
    active: bool,
    theme: &gpui_component::theme::Theme,
) -> gpui::Stateful<gpui::Div> {
    div()
        .id(id)
        .cursor_pointer()
        .rounded_md()
        .px_2()
        .py_1()
        .hover(|s| s.bg(theme.muted))
        .child(
            div()
                .h_flex()
                .gap_2()
                .justify_between()
                .child(name.into())
                .child(
                    div()
                        .text_color(theme.muted_foreground)
                        .text_sm()
                        .child(if active {
                            "✓".to_string()
                        } else {
                            String::new()
                        }),
                ),
        )
        .child(
            div()
                .text_sm()
                .text_color(theme.muted_foreground)
                .child(detail),
        )
}
