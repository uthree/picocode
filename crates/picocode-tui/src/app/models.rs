//! Model switching: the `/model` picker, the add-model form, and the
//! worker respawn that carries the conversation over.

use ratatui::crossterm::event::{KeyCode, KeyEvent};
use tokio::sync::oneshot;

use picocode_core::config::Config;
use picocode_core::event::{AgentEvent, WorkerCmd};
use picocode_core::models::{ModelChoice, SwitchPlan, model_choices};

use super::{AddModelForm, App, EntryKind, ModelPicker};

impl App {
    /// Ask the provider for its model list in the background; the answer
    /// arrives as a `ModelList` event, which updates the switch candidates
    /// and an open `/model` dialog.
    pub(super) fn refresh_models(&self) {
        let provider = self.cfg.provider;
        let base = self.cfg.base_url.clone();
        let label = format!(
            "{} @ {}",
            picocode_core::config::provider_name(provider),
            picocode_core::models::base_url(provider, base.as_deref())
        );
        let event_tx = self.event_tx.clone();
        tokio::spawn(async move {
            let result = picocode_core::models::fetch(provider, base.as_deref())
                .await
                .map_err(|e| format!("{e:#}"));
            let _ = event_tx.send(AgentEvent::ModelList { label, result }).await;
        });
    }

    /// `/model` with no argument: open the model-selection dialog with the
    /// configured entries plus the provider's served models, and refresh the
    /// latter in the background.
    pub(super) fn open_model_picker(&mut self) {
        if self.running > 0 {
            self.push(
                EntryKind::Error,
                "Cannot switch models while a turn is running".to_string(),
            );
            return;
        }
        let items = self.model_choices();
        let selected = items.iter().position(|c| c.active).unwrap_or(0);
        self.model_picker = Some(ModelPicker {
            items,
            selected,
            filter: String::new(),
        });
        self.refresh_models();
    }

    /// Rows for the `/model` dialog: configured entries first, then the
    /// models the provider reported serving (minus ones an entry already
    /// covers).
    pub(super) fn model_choices(&self) -> Vec<ModelChoice> {
        model_choices(
            &self.cfg.models,
            self.cfg.active_model.as_deref(),
            self.cfg.provider,
            self.cfg.base_url.as_deref(),
            &self.cfg.model,
            &self.available_models,
        )
    }

    /// Rebuild the open `/model` dialog after a fresh provider list arrived,
    /// keeping the selection on the same item where possible.
    pub(super) fn rebuild_model_picker(&mut self) {
        let Some(picker) = &self.model_picker else {
            return;
        };
        let filter = picker.filter.clone();
        let keep = picker
            .filtered()
            .get(picker.selected)
            .map(|c| c.name.clone());
        let items = self.model_choices();
        let rebuilt = ModelPicker {
            items,
            selected: 0,
            filter,
        };
        let selected = keep
            .and_then(|k| rebuilt.filtered().iter().position(|c| c.name == k))
            .unwrap_or(0);
        self.model_picker = Some(ModelPicker {
            selected,
            ..rebuilt
        });
    }

    /// `/model <name>`: spawn a worker for the named entry — or for a model
    /// the provider reported serving — and carry the conversation history
    /// over to it. A partial name that matches exactly one candidate is
    /// completed automatically.
    pub(super) async fn switch_model(&mut self, name: &str) {
        if self.running > 0 {
            self.push(
                EntryKind::Error,
                "Cannot switch models while a turn is running".to_string(),
            );
            return;
        }
        let (matched, plan) =
            picocode_core::models::plan_switch(&self.cfg, name, &self.available_models);
        if let Some(full) = &matched {
            self.push(EntryKind::Notice, format!("`{name}` matched {full}"));
        }
        match plan {
            SwitchPlan::Ambiguous(matches) => {
                self.push(
                    EntryKind::Error,
                    format!("`{name}` is ambiguous: {}", matches.join(", ")),
                );
            }
            SwitchPlan::AlreadyActive { display } => {
                self.push(EntryKind::Notice, format!("Already using {display}"));
            }
            SwitchPlan::Unknown { name } => {
                let names: Vec<&str> = self.cfg.models.iter().map(|m| m.name.as_str()).collect();
                let hint = if names.is_empty() {
                    "run /model to list what the provider serves".to_string()
                } else {
                    format!(
                        "configured: {}; /model lists what the provider serves",
                        names.join(", ")
                    )
                };
                self.push(EntryKind::Error, format!("Unknown model `{name}` — {hint}"));
            }
            SwitchPlan::Switch { name, cfg } => {
                self.apply_model_config(*cfg, &name).await;
            }
        }
    }

    /// Switch to an explicit provider/model/base-URL combination (the
    /// add-model form): an ad-hoc selection like `--provider`/`--model` on
    /// the command line, remembered per project. Returns whether it worked.
    async fn switch_custom(
        &mut self,
        provider: picocode_core::config::Provider,
        model: String,
        base_url: Option<String>,
    ) -> bool {
        if self.running > 0 {
            self.push(
                EntryKind::Error,
                "Cannot switch models while a turn is running".to_string(),
            );
            return false;
        }
        let new_cfg =
            picocode_core::models::custom_config(&self.cfg, provider, model.clone(), base_url);
        self.apply_model_config(new_cfg, &model).await
    }

    /// Respawn the worker for `new_cfg` and carry the conversation over;
    /// shared by the model switches and the `/prompt` editor. A spawn
    /// failure leaves everything untouched.
    pub(super) async fn respawn_worker(&mut self, new_cfg: Config) -> anyhow::Result<()> {
        // Spawn first so a failure (e.g. missing API key) leaves the current
        // worker untouched.
        let (new_tx, new_steer) = picocode_core::agent::spawn(
            &new_cfg,
            self.event_tx.clone(),
            self.cancel_tx.subscribe(),
            self.jobs.clone(),
            self.mcp.clone(),
            self.backend.clone(),
        )?;

        // Carry the conversation over to the new worker.
        let (htx, hrx) = oneshot::channel();
        if self.cmd_tx.send(WorkerCmd::TakeHistory(htx)).await.is_ok()
            && let Ok(history) = hrx.await
        {
            let _ = new_tx.send(WorkerCmd::SeedHistory(history)).await;
        }

        self.cmd_tx = new_tx; // dropping the old sender shuts the old worker down
        self.steer = new_steer;
        self.cfg = new_cfg;
        Ok(())
    }

    /// Switch to `new_cfg`'s model: respawn and report. Shared by the
    /// by-name switch and the add-model form.
    async fn apply_model_config(&mut self, new_cfg: Config, name: &str) -> bool {
        // The cached model list belongs to the endpoint it was fetched from.
        let endpoint_changed =
            new_cfg.provider != self.cfg.provider || new_cfg.base_url != self.cfg.base_url;
        if let Err(e) = self.respawn_worker(new_cfg).await {
            self.push(
                EntryKind::Error,
                format!("Failed to switch to `{name}`: {e:#}"),
            );
            return false;
        }
        self.model_label = self.cfg.model_label();
        self.push(
            EntryKind::Notice,
            format!("Model switched to {name} ({})", self.model_label),
        );
        if endpoint_changed {
            self.available_models.clear();
            self.refresh_models();
        }
        picocode_core::state::save_last_model(&self.cfg);
        true
    }

    /// Open the add-model form (the `/model` dialog's "+ add" row).
    pub(super) fn open_add_model(&mut self) {
        self.add_model = Some(AddModelForm {
            provider: self.cfg.provider,
            base_url: String::new(),
            model: String::new(),
            field: 0,
            fetched: Vec::new(),
            note: "Tab: fetch the endpoint's model list".to_string(),
        });
    }

    /// Key handling for the add-model form.
    pub(super) async fn add_model_key(&mut self, key: KeyEvent) {
        let Some(form) = &mut self.add_model else {
            return;
        };
        let rows = 3 + form.fetched.len();
        match key.code {
            KeyCode::Esc => {
                self.add_model = None;
                self.open_model_picker();
            }
            KeyCode::Up => form.field = (form.field + rows - 1) % rows,
            KeyCode::Down => form.field = (form.field + 1) % rows,
            // Provider row: cycle. A different provider invalidates the
            // fetched list.
            KeyCode::Left | KeyCode::Right if form.field == 0 => {
                let delta = if key.code == KeyCode::Left { -1 } else { 1 };
                form.provider = form.provider.cycled(delta);
                form.fetched.clear();
                form.note = format!(
                    "{} — Tab: fetch the endpoint's model list",
                    form.provider.api_key_hint()
                );
            }
            KeyCode::Char(c) if form.field == 1 => form.base_url.push(c),
            KeyCode::Char(c) if form.field == 2 => form.model.push(c),
            KeyCode::Backspace if form.field == 1 => {
                form.base_url.pop();
            }
            KeyCode::Backspace if form.field == 2 => {
                form.model.pop();
            }
            KeyCode::Tab => {
                form.note = "fetching…".to_string();
                let provider = form.provider;
                let base = form.base();
                let event_tx = self.event_tx.clone();
                tokio::spawn(async move {
                    let result = picocode_core::models::fetch(provider, base.as_deref())
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
            }
            KeyCode::Enter => {
                // A fetched row switches to that model; the form rows switch
                // to the typed one. `field` is only re-clamped by Up/Down,
                // while a second Tab can replace `fetched` with a shorter
                // list underneath it — so index by `get`, not by `[]`.
                let model = match form.fetched.get(form.field.wrapping_sub(3)) {
                    Some(name) => name.clone(),
                    None => form.model.trim().to_string(),
                };
                if model.is_empty() {
                    form.note = "type a model name (or Tab to fetch, ↑↓ to pick one)".to_string();
                    return;
                }
                let provider = form.provider;
                let base = form.base();
                self.add_model = None;
                if self
                    .switch_custom(provider, model.clone(), base.clone())
                    .await
                {
                    self.push(
                        EntryKind::Notice,
                        format!(
                            "To keep this model across projects, add it to picocode.toml:\n{}",
                            picocode_core::models::toml_snippet(provider, &model, base.as_deref())
                        ),
                    );
                }
            }
            _ => {}
        }
    }
}
