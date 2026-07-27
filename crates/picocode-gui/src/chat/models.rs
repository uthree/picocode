//! Model switching: the model menu's entries, the add-model dialog, and
//! the worker respawn that carries the conversation over.

use gpui::prelude::*;
use gpui::{Context, Entity, Window};
use gpui_component::input::InputState;
use rust_i18n::t;
use tokio::sync::oneshot;

use picocode_core::agent;
use picocode_core::config::{self, Config};
use picocode_core::event::{AgentEvent, WorkerCmd};
use picocode_core::models;
use picocode_core::transcript::EntryKind;

use super::ChatView;

/// State of the add-model dialog (reached from the model menu): pick a
/// provider, optionally point it at a base URL, and type or pick a model.
pub(super) struct AddModel {
    pub(super) provider: config::Provider,
    /// Endpoint override; empty uses the provider default.
    pub(super) base_url: Entity<InputState>,
    pub(super) model: Entity<InputState>,
    /// Models the probed endpoint reported serving ("fetch models").
    pub(super) fetched: Vec<String>,
    /// Status line: API-key hint, fetch progress, or error.
    pub(super) note: String,
}

impl AddModel {
    /// The base URL as the switch/probe wants it (None = provider default).
    pub(super) fn base(&self, cx: &gpui::App) -> Option<String> {
        let value = self.base_url.read(cx).value().trim().to_string();
        (!value.is_empty()).then_some(value)
    }
}

impl ChatView {
    /// Ask the provider for its model list in the background; the answer
    /// arrives as a `ModelList` event through the regular pump.
    pub(super) fn refresh_models(&self) {
        let provider = self.cfg.provider;
        let base = self.cfg.base_url.clone();
        let label = format!(
            "{} @ {}",
            config::provider_name(provider),
            models::base_url(provider, base.as_deref())
        );
        let event_tx = self.event_tx.clone();
        self.rt.spawn(async move {
            let result = models::fetch(provider, base.as_deref())
                .await
                .map_err(|e| format!("{e:#}"));
            let _ = event_tx.send(AgentEvent::ModelList { label, result }).await;
        });
    }

    /// Switch to a named `[[models]]` entry — or to a model the provider
    /// reported serving — and carry the conversation history over to the
    /// new worker (same flow as the TUI's `/model <name>`).
    pub(super) fn switch_model(&mut self, name: &str, cx: &mut Context<Self>) {
        self.menu = None;
        if self.running {
            self.push(EntryKind::Error, t!("switch_while_running").to_string());
            cx.notify();
            return;
        }
        let (matched, plan) = models::plan_switch(&self.cfg, name, &self.available_models);
        // A partial name matching exactly one candidate completes itself.
        if let Some(full) = &matched {
            self.push(
                EntryKind::Notice,
                t!("model_matched", partial = name, full = full).to_string(),
            );
        }
        match plan {
            models::SwitchPlan::Ambiguous(matches) => {
                self.push(
                    EntryKind::Error,
                    t!("model_ambiguous", name = name, matches = matches.join(", ")).to_string(),
                );
                cx.notify();
            }
            models::SwitchPlan::AlreadyActive { display } => {
                self.push(
                    EntryKind::Notice,
                    t!("already_using", name = display).to_string(),
                );
                cx.notify();
            }
            models::SwitchPlan::Unknown { name } => {
                self.push(
                    EntryKind::Error,
                    t!("unknown_model", name = name).to_string(),
                );
                cx.notify();
            }
            models::SwitchPlan::Switch { name, cfg } => {
                self.apply_model_config(*cfg, &name, cx);
            }
        }
    }

    /// Switch to an explicit provider/model/base-URL combination (the
    /// add-model dialog): an ad-hoc selection like `--provider`/`--model`
    /// on the command line, remembered per project. Returns whether it
    /// worked.
    fn switch_custom(
        &mut self,
        provider: config::Provider,
        model: String,
        base_url: Option<String>,
        cx: &mut Context<Self>,
    ) -> bool {
        if self.running {
            self.push(EntryKind::Error, t!("switch_while_running").to_string());
            cx.notify();
            return false;
        }
        let new_cfg = models::custom_config(&self.cfg, provider, model.clone(), base_url);
        self.apply_model_config(new_cfg, &model, cx)
    }

    /// Respawn the worker for `new_cfg` and carry the conversation over;
    /// shared by the model switches and the `/prompt` editor. A spawn
    /// failure leaves everything untouched.
    pub(super) fn respawn_worker(&mut self, new_cfg: Config) -> Result<(), String> {
        // Spawn first so a failure (e.g. missing API key) leaves the current
        // worker untouched. agent::spawn calls tokio::spawn internally, so it
        // needs the runtime context entered.
        let (new_tx, new_steer) = {
            let _guard = self.rt.enter();
            agent::spawn(
                &new_cfg,
                self.event_tx.clone(),
                self.cancel_tx.subscribe(),
                self.jobs.clone(),
                self.mcp.clone(),
                self.backend.clone(),
            )
            .map_err(|e| format!("{e:#}"))?
        };
        self.steer = new_steer;

        // Carry the conversation over in the background; `running` blocks
        // prompts until the transfer's TurnComplete lands so a fast prompt
        // can't race the history seed. Dropping the old sender at the end of
        // the task shuts the old worker down.
        let old_tx = std::mem::replace(&mut self.cmd_tx, new_tx.clone());
        let event_tx = self.event_tx.clone();
        self.running = true;
        self.rt.spawn(async move {
            let (htx, hrx) = oneshot::channel();
            if old_tx.send(WorkerCmd::TakeHistory(htx)).await.is_ok()
                && let Ok(history) = hrx.await
            {
                let _ = new_tx.send(WorkerCmd::SeedHistory(history)).await;
            }
            let _ = event_tx.send(AgentEvent::TurnComplete).await;
        });
        self.cfg = new_cfg;
        Ok(())
    }

    fn apply_model_config(&mut self, new_cfg: Config, name: &str, cx: &mut Context<Self>) -> bool {
        // The cached model list belongs to the endpoint it was fetched from.
        let endpoint_changed =
            new_cfg.provider != self.cfg.provider || new_cfg.base_url != self.cfg.base_url;
        if let Err(error) = self.respawn_worker(new_cfg) {
            self.push(
                EntryKind::Error,
                t!("switch_failed", name = name, error = error).to_string(),
            );
            cx.notify();
            return false;
        }
        self.push(
            EntryKind::Notice,
            t!(
                "model_switched",
                name = name,
                label = self.cfg.model_label()
            )
            .to_string(),
        );
        if endpoint_changed {
            self.available_models.clear();
            self.refresh_models();
        }
        picocode_core::state::save_last_model(&self.cfg);
        cx.notify();
        true
    }

    /// Open the add-model dialog (the model menu's "+ add" row).
    pub(super) fn open_add_model(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.menu = None;
        let base_url = cx.new(|cx| {
            InputState::new(window, cx).placeholder(t!("add_model_base_placeholder").to_string())
        });
        let model = cx.new(|cx| {
            InputState::new(window, cx).placeholder(t!("add_model_model_placeholder").to_string())
        });
        model.update(cx, |state, cx| state.focus(window, cx));
        self.add_model = Some(AddModel {
            provider: self.cfg.provider,
            base_url,
            model,
            fetched: Vec::new(),
            note: self.cfg.provider.api_key_hint().to_string(),
        });
        cx.notify();
    }

    /// The add-model dialog's provider button: cycle to the next provider
    /// (a different provider invalidates the fetched list).
    pub(super) fn add_model_cycle_provider(&mut self, cx: &mut Context<Self>) {
        if let Some(dlg) = &mut self.add_model {
            dlg.provider = dlg.provider.cycled(1);
            dlg.fetched.clear();
            dlg.note = dlg.provider.api_key_hint().to_string();
            cx.notify();
        }
    }

    /// The add-model dialog's fetch button: probe the endpoint's model list
    /// in the background (answer arrives as `FormModelList`).
    pub(super) fn add_model_fetch(&mut self, cx: &mut Context<Self>) {
        let Some(dlg) = &mut self.add_model else {
            return;
        };
        let provider = dlg.provider;
        let base = dlg.base(cx);
        dlg.note = t!("fetching_models").to_string();
        let event_tx = self.event_tx.clone();
        self.rt.spawn(async move {
            let result = models::fetch(provider, base.as_deref())
                .await
                .map_err(|e| format!("{e:#}"));
            let _ = event_tx
                .send(AgentEvent::FormModelList {
                    provider,
                    base_url: base,
                    result,
                })
                .await;
        });
        cx.notify();
    }

    /// The add-model dialog's switch action: `model` is a fetched row's id,
    /// or `None` for the typed field. On success the dialog closes and a
    /// copyable `[[models]]` snippet lands in the transcript.
    pub(super) fn add_model_switch(&mut self, model: Option<String>, cx: &mut Context<Self>) {
        let Some(dlg) = &self.add_model else {
            return;
        };
        let provider = dlg.provider;
        let base = dlg.base(cx);
        let model = model.unwrap_or_else(|| dlg.model.read(cx).value().trim().to_string());
        if model.is_empty() {
            if let Some(dlg) = &mut self.add_model {
                dlg.note = t!("add_model_need_name").to_string();
            }
            cx.notify();
            return;
        }
        self.add_model = None;
        if self.switch_custom(provider, model.clone(), base.clone(), cx) {
            self.push(
                EntryKind::Notice,
                t!(
                    "keep_model_hint",
                    snippet = models::toml_snippet(provider, &model, base.as_deref())
                )
                .to_string(),
            );
            cx.notify();
        }
    }
}
