//! picocode-gui: a gpui front end on top of picocode-core.
//!
//! The core is UI-agnostic: it exposes the agent through the
//! `AgentEvent` / `WorkerCmd` channels. This binary spawns the same worker
//! the TUI uses (on a manually created tokio runtime, since gpui brings its
//! own executor) and renders the event stream in a gpui window.

mod assets;
mod chat;
mod highlight;
mod math;
mod settings;
mod tex;

// UI strings live in locales/{en,ja}.yml; the locale is picked from the
// system at startup (rust-i18n's locale is process-global).
rust_i18n::i18n!("locales", fallback = "en");

use clap::Parser;
use gpui::{
    App, AppContext, Application, Bounds, TitlebarOptions, WindowBounds, WindowOptions, px, size,
};
use gpui_component::Root;
use picocode_core::{agent, config, models};

fn main() -> anyhow::Result<()> {
    // sys-locale reads the OS preference (works for Finder-launched apps
    // too, where $LANG is unset). Only ja is translated so far.
    if let Some(locale) = sys_locale::get_locale()
        && locale.starts_with("ja")
    {
        rust_i18n::set_locale("ja");
    }

    let args = config::Args::parse();
    // In the GUI, --smoke auto-sends the prompt once the window opens
    // (debug aid: exercises the whole worker ⇄ view bridge on launch).
    let smoke = args.smoke.clone();
    let smoke_attach = args.smoke_attach.clone();
    let mut cfg = config::Config::from_args(args)?;

    // The agent worker and tools are tokio-based; gpui has its own executor,
    // so run a tokio runtime beside it. The runtime must outlive the app and
    // must not be dropped from within an async context — leak it.
    let rt = Box::leak(Box::new(tokio::runtime::Runtime::new()?));

    // No model configured anywhere: use the first model Ollama serves.
    if cfg.model.is_empty() {
        cfg.model = rt.block_on(models::pick_ollama_model(&cfg))?;
    }

    // Fail before opening a window if the provider client can't be built
    // (e.g. a missing API key).
    let (event_tx, event_rx) = tokio::sync::mpsc::channel(256);
    let (cancel_tx, cancel_rx) = tokio::sync::watch::channel(());
    let cmd_tx = {
        let _guard = rt.enter();
        agent::spawn(&cfg, event_tx.clone(), cancel_rx)?
    };
    // The view keeps a runtime handle (model-list fetches, model switches)
    // and the event sender (so those background jobs report back through the
    // same pump as the worker).
    let handle = rt.handle().clone();

    // The asset source serves the icon SVGs gpui-component references
    // (e.g. the code-block copy button) — without it they render invisibly.
    Application::new()
        .with_assets(assets::Assets)
        .run(move |cx: &mut App| {
            gpui_component::init(cx);
            // Registered after gpui_component::init, so these win over the
            // input's own bindings. Plain Enter goes straight to the chat
            // view's submit action — bypassing the input's Enter handling,
            // which would first insert a newline at the cursor. Shift+Enter
            // keeps the input's secondary Enter (inserts the newline), and
            // Tab cycles the slash-command completion instead of indenting.
            cx.bind_keys([
                gpui::KeyBinding::new("enter", chat::SubmitPrompt, Some("Input")),
                gpui::KeyBinding::new(
                    "shift-enter",
                    gpui_component::input::Enter { secondary: true },
                    Some("Input"),
                ),
                gpui::KeyBinding::new("tab", chat::AcceptCompletion, Some("Input")),
            ]);

            let bounds = Bounds::centered(None, size(px(880.), px(720.)), cx);
            let options = WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                titlebar: Some(TitlebarOptions {
                    title: Some("picocode".into()),
                    ..Default::default()
                }),
                ..Default::default()
            };
            cx.open_window(options, |window, cx| {
                let view = cx.new(|cx| {
                    let mut view = chat::ChatView::new(
                        cfg, event_rx, event_tx, cmd_tx, cancel_tx, handle, window, cx,
                    );
                    if let Some(prompt) = smoke {
                        if prompt.starts_with('!') {
                            // Exercise the direct-shell path too.
                            view.run_shell(prompt);
                        } else {
                            let attachments = smoke_attach
                                .as_deref()
                                .and_then(picocode_core::attachment::Attachment::detect)
                                .into_iter()
                                .collect();
                            view.send_prompt(prompt, attachments);
                        }
                    }
                    view
                });
                cx.new(|cx| Root::new(view, window, cx))
            })
            .expect("failed to open the picocode window");

            cx.activate(true);
            cx.on_window_closed(|cx| {
                if cx.windows().is_empty() {
                    cx.quit();
                }
            })
            .detach();
        });
    Ok(())
}
