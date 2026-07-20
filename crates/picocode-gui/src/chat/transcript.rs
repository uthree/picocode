//! Transcript rendering: one virtualized-list row per entry — markdown,
//! math, code, tool rows and diffs — plus the diff element shared with the
//! approval dialog.

use gpui::prelude::*;
use gpui::{AnyElement, App, Context, MouseButton, MouseDownEvent, SharedString, Window, div, px};
use gpui_component::clipboard::Clipboard;
use gpui_component::text::TextView;
use gpui_component::{ActiveTheme, StyledExt};

use picocode_core::transcript::{Entry, EntryKind};

use super::ChatView;
use super::one_line;

/// Diff row backgrounds (translucent, so they read on both themes).
const DIFF_ADD_BG: u32 = 0x3fb95033;
const DIFF_DEL_BG: u32 = 0xf8514933;

impl ChatView {
    /// Render one row of the virtualized transcript list: the entry body
    /// plus its right-click (copy menu) hook and inter-entry spacing.
    /// Called lazily by the list for visible rows only.
    pub(super) fn render_item(
        &mut self,
        ix: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let last_ix = self.entries.len().saturating_sub(1);
        let Some(entry) = self.entries.get(ix) else {
            // The list row count only drifts from `entries` mid-update,
            // never across a frame; render nothing just in case.
            return div().into_any_element();
        };
        // Only the entry currently receiving deltas is "streaming".
        let streaming = self.running && ix == last_ix;
        let rendered = Self::render_entry(
            entry,
            ix,
            self.show_reasoning,
            streaming,
            &mut self.math_cache,
            window,
            cx,
        );
        div()
            .when(ix > 0, |d| d.pt_2())
            .on_mouse_down(
                MouseButton::Right,
                cx.listener(move |this, ev: &MouseDownEvent, _, cx| {
                    if this.dialog_open() {
                        return;
                    }
                    this.ctx_menu = Some((ix, ev.position));
                    cx.notify();
                }),
            )
            .child(rendered)
            .into_any_element()
    }

    fn render_entry(
        entry: &Entry,
        ix: usize,
        show_reasoning: bool,
        streaming: bool,
        math_cache: &mut crate::tex::MathCache,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let theme = cx.theme();
        let muted = theme.muted_foreground;
        let foreground = theme.foreground;
        let mono = theme.mono_font_family.clone();
        match entry.kind {
            EntryKind::User => div()
                .px_3()
                .py_2()
                .rounded_lg()
                .bg(theme.muted)
                .border_1()
                .border_color(theme.border)
                .child(entry.text.clone())
                .into_any_element(),
            EntryKind::Assistant => {
                // While the reply is still streaming, render it as plain
                // text: the markdown TextView re-parses on a 200ms debounce
                // that RESETS on every change, so a delta stream faster than
                // that postpones the parse indefinitely and the displayed
                // text freezes until the stream pauses. Plain text updates
                // every delta; the markdown (and math) rendering takes over
                // the moment the entry stops growing.
                if streaming {
                    return div().child(entry.text.clone()).into_any_element();
                }
                // Display math blocks are typeset by RaTeX as images; the
                // markdown between them still gets inline math as Unicode.
                let scale = window.scale_factor();
                let mut col = div().v_flex().gap_1();
                let segments = crate::math::split_display_math(&entry.text);
                for (six, segment) in segments.into_iter().enumerate() {
                    match segment {
                        crate::math::Segment::Markdown(md) => {
                            col = col.child(
                                TextView::markdown(
                                    SharedString::from(format!("md-{ix}-{six}")),
                                    SharedString::from(crate::math::render_math(&md)),
                                    window,
                                    cx,
                                )
                                .selectable(true)
                                // Copy button in each code block's top-right
                                // corner (ids hashed from the code so every
                                // block gets its own "copied" check mark).
                                .code_block_actions(
                                    |code_block, _, _| {
                                        use std::hash::{Hash, Hasher};
                                        let code = code_block.code();
                                        let mut hasher =
                                            std::collections::hash_map::DefaultHasher::new();
                                        code.hash(&mut hasher);
                                        Clipboard::new(SharedString::from(format!(
                                            "copy-code-{:x}",
                                            hasher.finish()
                                        )))
                                        .value(code)
                                    },
                                ),
                            );
                        }
                        crate::math::Segment::Display(tex_src) => {
                            let key = crate::tex::cache_key(&tex_src, foreground, scale);
                            let cached = math_cache.entry(key).or_insert_with(|| {
                                crate::tex::render_display(&tex_src, foreground, scale)
                                    .map(std::sync::Arc::new)
                            });
                            match cached {
                                Some(mi) => {
                                    col = col.child(
                                        div().py_1().child(
                                            gpui::img(mi.image.clone())
                                                .w(px(mi.width))
                                                .h(px(mi.height)),
                                        ),
                                    );
                                }
                                // RaTeX couldn't typeset it: Unicode text.
                                None => {
                                    col = col.child(
                                        TextView::markdown(
                                            SharedString::from(format!("md-{ix}-{six}")),
                                            SharedString::from(crate::math::display_fallback(
                                                &tex_src,
                                            )),
                                            window,
                                            cx,
                                        )
                                        .selectable(true),
                                    );
                                }
                            }
                        }
                    }
                }
                col.into_any_element()
            }
            EntryKind::Reasoning => div()
                .italic()
                .text_sm()
                .text_color(muted)
                .child(if show_reasoning {
                    entry.text.clone()
                } else {
                    // Collapsed (the `/config` "reasoning" row): one line.
                    format!("✳ {}", one_line(&entry.text, 80))
                })
                .into_any_element(),
            EntryKind::Tool => {
                // "{name} {args}" — icon and accent color per tool.
                let (name, rest) = entry
                    .text
                    .split_once(' ')
                    .unwrap_or((entry.text.as_str(), ""));
                let (icon, color) = tool_style(name);
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
                            .flex_none()
                            .text_sm()
                            .font_weight(gpui::FontWeight::MEDIUM)
                            .text_color(color)
                            .child(name.to_string()),
                    )
                    .child(
                        div()
                            .font_family(mono)
                            .text_sm()
                            .text_color(muted)
                            .truncate()
                            .child(rest.to_string()),
                    )
                    .into_any_element()
            }
            EntryKind::ToolOut => div()
                .ml_2()
                .pl_3()
                .border_l_2()
                .border_color(theme.border)
                .font_family(mono)
                .text_sm()
                .text_color(muted)
                .child(entry.text.clone())
                .into_any_element(),
            EntryKind::Diff => div()
                .pl_4()
                .child(diff_element(&entry.text, entry.lang.as_deref(), mono, cx))
                .into_any_element(),
            EntryKind::Notice | EntryKind::Logo => div()
                .text_sm()
                .text_color(muted)
                .child(entry.text.clone())
                .into_any_element(),
            EntryKind::Warning => div()
                .h_flex()
                .gap_2()
                .items_start()
                .text_sm()
                .text_color(theme.warning)
                .child(
                    gpui_component::Icon::default()
                        .path("icons/triangle-alert.svg")
                        .size_4()
                        .flex_none()
                        .mt_0p5(),
                )
                .child(entry.text.clone())
                .into_any_element(),
            EntryKind::Summary => div()
                .px_3()
                .py_2()
                .rounded_lg()
                .border_1()
                .border_color(theme.border)
                .text_sm()
                .text_color(muted)
                .child(entry.text.clone())
                .into_any_element(),
            EntryKind::Error => div()
                .h_flex()
                .gap_2()
                .items_start()
                .text_sm()
                .text_color(theme.danger)
                .child(
                    gpui_component::Icon::default()
                        .path("icons/circle-x.svg")
                        .size_4()
                        .flex_none()
                        .mt_0p5(),
                )
                .child(entry.text.clone())
                .into_any_element(),
        }
    }
}

/// Render "+ "/"- "/"  " diff text: add/remove rows are marked by the
/// background color, while the text keeps its syntax highlighting, picked
/// from the file path (like the TUI). Both sides of the diff are rebuilt
/// and highlighted separately so multi-line constructs color correctly.
pub(super) fn diff_element(
    text: &str,
    lang: Option<&str>,
    mono: SharedString,
    cx: &App,
) -> AnyElement {
    use crate::highlight::{self, LineSpans};
    use picocode_core::transcript::{DiffRow, parse_diff};

    let theme = cx.theme();
    let token = lang.unwrap_or("text");
    let (rows, old_src, new_src) = parse_diff(text);
    let old_hl = highlight::highlight_lines(&old_src, token, &theme.highlight_theme);
    let new_hl = highlight::highlight_lines(&new_src, token, &theme.highlight_theme);

    // One row: the sign prefix in its own color, the code spans shifted
    // past it. Blank rows keep a space so they hold their line height.
    let styled_row = |sign: &str, sign_color: gpui::Hsla, code: &str, spans: Option<&LineSpans>| {
        let row_text = format!("{sign}{code}");
        let mut highlights: Vec<(std::ops::Range<usize>, gpui::HighlightStyle)> = Vec::new();
        if !sign.trim().is_empty() {
            highlights.push((
                0..sign.len(),
                gpui::HighlightStyle {
                    color: Some(sign_color),
                    ..Default::default()
                },
            ));
        }
        highlights.extend(
            spans
                .into_iter()
                .flatten()
                .map(|(r, st)| (r.start + sign.len()..r.end + sign.len(), *st)),
        );
        gpui::StyledText::new(if row_text.trim_end().is_empty() {
            " ".into()
        } else {
            row_text
        })
        .with_highlights(highlights)
    };

    let old_lines: Vec<&str> = old_src.split('\n').collect();
    let new_lines: Vec<&str> = new_src.split('\n').collect();
    let code = |lines: &[&str], i: usize| lines.get(i).copied().unwrap_or_default().to_string();

    let mut out = div().v_flex().font_family(mono).text_sm();
    for row in rows {
        let el = match row {
            DiffRow::Old(i) => div().px_1().bg(gpui::rgba(DIFF_DEL_BG)).child(styled_row(
                "- ",
                theme.danger,
                &code(&old_lines, i),
                old_hl.get(i),
            )),
            DiffRow::New(i) => div().px_1().bg(gpui::rgba(DIFF_ADD_BG)).child(styled_row(
                "+ ",
                theme.success,
                &code(&new_lines, i),
                new_hl.get(i),
            )),
            DiffRow::Ctx(i) => div().px_1().child(styled_row(
                "  ",
                theme.foreground,
                &code(&new_lines, i),
                new_hl.get(i),
            )),
            DiffRow::Other(text) => {
                div()
                    .px_1()
                    .text_color(theme.muted_foreground)
                    .child(if text.is_empty() {
                        " ".to_string()
                    } else {
                        text
                    })
            }
        };
        out = out.child(el);
    }
    out.into_any_element()
}

/// Icon asset path and accent color for a tool-call row: blue-ish for
/// local reads, yellow for file edits, green for the shell, purple for
/// the web tools, blue for plans.
pub(super) fn tool_style(name: &str) -> (&'static str, gpui::Hsla) {
    let (icon, rgb) = match name {
        "read_file" => ("icons/file-text.svg", 0x0ea5e9),
        "list_files" => ("icons/folder.svg", 0x0ea5e9),
        "grep" => ("icons/search.svg", 0x0ea5e9),
        "edit_file" => ("icons/pencil.svg", 0xeab308),
        "bash" => ("icons/terminal.svg", 0x3fb950),
        "web_search" => ("icons/globe.svg", 0xa855f7),
        "web_fetch" => ("icons/download.svg", 0xa855f7),
        "submit_plan" => ("icons/clipboard-list.svg", 0x3b82f6),
        _ => ("icons/wrench.svg", 0x8b949e),
    };
    (icon, gpui::rgb(rgb).into())
}
