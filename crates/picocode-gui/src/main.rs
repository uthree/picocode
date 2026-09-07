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
mod tex;
mod theme;

// UI strings live in locales/{en,ja}.yml. The `/config` row labels come
// from picocode-core's own catalog instead, so the TUI names them the same
// way; rust-i18n keys a catalog per crate but the locale is process-global.
rust_i18n::i18n!("locales", fallback = "en");

use clap::Parser;
use gpui::{
    App, AppContext, Application, Bounds, TitlebarOptions, WindowBounds, WindowOptions, px, size,
};
use gpui_component::Root;
use picocode_core::{agent, config, models};

fn main() -> anyhow::Result<()> {
    // Sets the process-global locale both catalogs read: this crate's, and
    // picocode-core's, which carries the shared `/config` row labels.
    picocode_core::set_locale_from_system();

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
    let jobs = picocode_core::tools::BackgroundJobs::new();
    // Remote workspace: connect over SSH and load the host's instruction
    // files before spawning (fatal on failure — no workspace otherwise).
    // Remote workspace: connect over SSH and apply the host's picocode.toml
    // and instruction files (fatal on failure — there is no workspace).
    let backend = rt.block_on(picocode_core::workspace::connect(&mut cfg))?;
    // Opt-in MCP servers connect once here; failures are shown, not fatal.
    let (mcp, mcp_errors) = rt.block_on(picocode_core::mcp::connect_all(&cfg.mcp_servers));
    for error in mcp_errors {
        let _ = event_tx.try_send(picocode_core::event::AgentEvent::Error(error));
    }
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
            // Bundled color themes: written out and watched; the saved
            // family applies once loaded.
            theme::init(cx);
            // Registered after gpui_component::init, so these win over the
            // input's own bindings: Tab cycles the slash-command completion
            // instead of indenting. The Enter family is bound by the chat
            // view itself (`chat::bind_send_key`), which knows the
            // configured send key and rebinds when `/config` changes it.
            cx.bind_keys([
                gpui::KeyBinding::new("tab", chat::AcceptCompletion, Some("Input")),
                // The paste shortcut goes through the chat view first, which
                // stages copied files/images as attachments and propagates
                // for plain text — falling back to the input's own paste.
                gpui::KeyBinding::new(
                    if cfg!(target_os = "macos") {
                        "cmd-v"
                    } else {
                        "ctrl-v"
                    },
                    chat::PasteClipboard,
                    Some("Input"),
                ),
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
                        cfg, event_rx, event_tx, cmd_tx, steer, jobs, cancel_tx, handle, mcp,
                        backend, window, cx,
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
                let threads = cx.new(|cx| chat::ThreadsView::new(view, window, cx));
                cx.new(|cx| Root::new(threads, window, cx))
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
