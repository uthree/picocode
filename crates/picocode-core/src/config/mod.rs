use std::path::{Path, PathBuf};

use anyhow::Context;
use clap::{Parser, ValueEnum};
use serde::Deserialize;

/// Max bytes read from a single instruction file (the rest is truncated).
const INSTRUCTION_FILE_MAX_BYTES: usize = 32 * 1024;

#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Provider {
    Ollama,
    Anthropic,
    Openai,
}

pub fn provider_name(p: Provider) -> &'static str {
    match p {
        Provider::Ollama => "ollama",
        Provider::Anthropic => "anthropic",
        Provider::Openai => "openai",
    }
}

pub fn provider_from_name(s: &str) -> Option<Provider> {
    match s {
        "ollama" => Some(Provider::Ollama),
        "anthropic" => Some(Provider::Anthropic),
        "openai" => Some(Provider::Openai),
        _ => None,
    }
}

/// picocode — a minimal TUI coding agent.
#[derive(Parser, Debug)]
#[command(name = "picocode", version, about = "picocode — a minimal coding agent")]
pub struct Args {
    /// LLM provider to use (default: ollama, or the config file's value).
    #[arg(long, value_enum)]
    pub provider: Option<Provider>,

    /// Model name. Defaults depend on the provider.
    #[arg(long)]
    pub model: Option<String>,

    /// Provider API base URL (e.g. http://localhost:11434 for Ollama, or an
    /// OpenAI-compatible server's .../v1). Overrides the *_BASE_URL env vars.
    #[arg(long)]
    pub base_url: Option<String>,

    /// Start in bypass mode: every tool call runs without confirmation
    /// (deny rules still apply). Meant for isolated environments such as
    /// containers.
    #[arg(long)]
    pub bypass: bool,

    /// Headless mode for debugging: run one prompt without the TUI and print
    /// events to stdout. Implies bypass mode.
    #[arg(long, hide = true)]
    pub smoke: Option<String>,

    /// File attached to the --smoke prompt (E2E for multimodal messages).
    #[arg(long, hide = true, requires = "smoke")]
    pub smoke_attach: Option<PathBuf>,
}

// ----- config file ----------------------------------------------------------

/// On-disk config (`picocode.toml` in the project root, merged over
/// `~/.config/picocode/config.toml`).
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct FileConfig {
    /// Name of the `[[models]]` entry to use at startup (default: the first).
    default_model: Option<String>,
    /// Named model entries, switchable at runtime with `/model <name>`.
    models: Option<Vec<ModelEntry>>,
    /// Instruction files loaded into the system prompt when present.
    instructions: Option<Vec<String>>,
    /// Replaces the built-in base system prompt. `{root}` expands to the
    /// working directory. Instruction files are still appended after it.
    system_prompt: Option<String>,
    /// Seconds before a bash command is moved to the background (default 120).
    bash_timeout: Option<u64>,
    /// Max lines a single read_file call returns (default 2000).
    read_max_lines: Option<u64>,
    /// Bytes per line before read_file truncates it (default 500).
    read_max_line_bytes: Option<u64>,
    /// Context usage (percent of the window) at which the conversation is
    /// compacted automatically after a turn; 0 disables (default 85).
    auto_compact: Option<u64>,
    /// Tools to leave unregistered entirely (schemas never sent to the
    /// model). Only the web tools (`web_search`, `web_fetch`) can be listed.
    disable_tools: Option<Vec<String>>,
    #[serde(default)]
    approval: ApprovalRules,
    #[serde(default)]
    search: SearchFileConfig,
}

/// Default bash timeout in seconds.
pub const DEFAULT_BASH_TIMEOUT: u64 = 120;
/// Default read_file output limits.
pub const DEFAULT_READ_MAX_LINES: u64 = 2000;
pub const DEFAULT_READ_MAX_LINE_BYTES: u64 = 500;
/// Default auto-compaction threshold (percent of the context window).
pub const DEFAULT_AUTO_COMPACT: u64 = 85;

/// Fallback context-window size when a model entry doesn't declare one.
/// Only used for the status-bar usage gauge.
pub const DEFAULT_CONTEXT_WINDOW: u64 = 32_768;

/// One switchable `[[models]]` entry in the config file.
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModelEntry {
    pub name: String,
    pub provider: Provider,
    pub model: String,
    #[serde(default)]
    pub base_url: Option<String>,
    /// Context-window size in tokens (for the usage gauge).
    #[serde(default)]
    pub context_window: Option<u64>,
}

impl ModelEntry {
    pub fn label(&self) -> String {
        format!("{}/{}", provider_name(self.provider), self.model)
    }
}

fn validate_models(models: &[ModelEntry], default: Option<&str>) -> anyhow::Result<()> {
    let mut seen = std::collections::HashSet::new();
    for m in models {
        if m.name.is_empty() || m.name.contains(char::is_whitespace) {
            anyhow::bail!(
                "invalid model entry name `{}` (must be non-empty, no spaces)",
                m.name
            );
        }
        if !seen.insert(m.name.as_str()) {
            anyhow::bail!("duplicate model entry name `{}` in config", m.name);
        }
    }
    if let Some(d) = default
        && !models.iter().any(|m| m.name == d)
    {
        anyhow::bail!("default_model `{d}` does not match any [[models]] entry");
    }
    Ok(())
}

fn pick_entry<'a>(models: &'a [ModelEntry], default: Option<&str>) -> Option<&'a ModelEntry> {
    match default {
        Some(d) => models.iter().find(|m| m.name == d),
        None => models.first(),
    }
}

/// One resolved startup selection:
/// (provider, model, base_url, active entry name, context window).
type Selection = (Provider, String, Option<String>, Option<String>, u64);

fn entry_selection(entry: &ModelEntry) -> Selection {
    (
        entry.provider,
        entry.model.clone(),
        entry.base_url.clone(),
        Some(entry.name.clone()),
        entry.context_window.unwrap_or(DEFAULT_CONTEXT_WINDOW),
    )
}

/// Startup selection from the saved last-used model: a still-existing
/// `[[models]]` entry wins (its current definition applies); otherwise the
/// saved ad-hoc provider/model is used directly.
fn restore_selection(state: &crate::state::LastModel, models: &[ModelEntry]) -> Option<Selection> {
    if let Some(name) = &state.entry
        && let Some(entry) = models.iter().find(|m| m.name == *name)
    {
        return Some(entry_selection(entry));
    }
    let provider = provider_from_name(&state.provider)?;
    if state.model.is_empty() {
        return None;
    }
    Some((
        provider,
        state.model.clone(),
        state.base_url.clone(),
        None,
        DEFAULT_CONTEXT_WINDOW,
    ))
}

/// CLI default model when only `--provider` is given. Ollama has no
/// hardcoded default: an empty model means "use the first model the server
/// serves", resolved in `main` before the agent is built.
fn default_model_for(provider: Provider) -> String {
    match provider {
        Provider::Ollama => String::new(),
        Provider::Anthropic => "claude-opus-4-8".to_string(),
        Provider::Openai => "gpt-4o".to_string(),
    }
}

/// Shared, runtime-adjustable numeric setting: the `/config` dialog writes
/// it while the worker or a tool reads it per use, so a change applies to
/// the next prompt / tool call. Used for the bash timeout, the read_file
/// output limits and the auto-compact threshold.
#[derive(Clone, Debug)]
pub struct NumHandle(std::sync::Arc<std::sync::atomic::AtomicU64>);

impl NumHandle {
    pub fn new(n: u64) -> Self {
        Self(std::sync::Arc::new(std::sync::atomic::AtomicU64::new(n)))
    }

    pub fn get(&self) -> u64 {
        self.0.load(std::sync::atomic::Ordering::Relaxed)
    }

    pub fn set(&self, n: u64) {
        self.0.store(n, std::sync::atomic::Ordering::Relaxed);
    }

    /// `/config` ←/→ stepping: move by `delta` steps of `step`, clamped to
    /// `min..=max`. Shared by both front ends so the ranges stay in sync.
    pub fn step(&self, delta: i64, step: i64, min: u64, max: u64) {
        let next = self.get() as i64 + delta * step;
        self.set(next.clamp(min as i64, max as i64) as u64);
    }
}

fn load_file(path: &Path) -> anyhow::Result<Option<FileConfig>> {
    match std::fs::read_to_string(path) {
        Ok(s) => toml::from_str(&s)
            .map(Some)
            .with_context(|| format!("invalid config file: {}", path.display())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e).with_context(|| format!("failed to read {}", path.display())),
    }
}

/// The user's home directory: `$HOME`, or `%USERPROFILE%` on Windows.
pub(crate) fn home_dir() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from)
}

fn global_config_path() -> Option<PathBuf> {
    if let Some(xdg) = std::env::var_os("XDG_CONFIG_HOME") {
        return Some(PathBuf::from(xdg).join("picocode/config.toml"));
    }
    Some(home_dir()?.join(".config/picocode/config.toml"))
}

/// Merge the project config over the global one: scalars from the project win,
/// list rules are concatenated (deny always wins at decision time, so order
/// doesn't matter).
fn merge(global: FileConfig, project: FileConfig) -> FileConfig {
    let mut approval = global.approval;
    approval.allow_tools.extend(project.approval.allow_tools);
    approval.deny_tools.extend(project.approval.deny_tools);
    approval.allow_bash.extend(project.approval.allow_bash);
    approval.deny_bash.extend(project.approval.deny_bash);
    // [[models]] and default_model travel together: a project defining its
    // own roster starts from a clean slate (a global default_model can't
    // point into it), while a project default_model alone picks from the
    // global roster.
    let (models, default_model) = if project.models.is_some() {
        (project.models, project.default_model)
    } else {
        (
            global.models,
            project.default_model.or(global.default_model),
        )
    };
    FileConfig {
        default_model,
        models,
        instructions: project.instructions.or(global.instructions),
        system_prompt: project.system_prompt.or(global.system_prompt),
        bash_timeout: project.bash_timeout.or(global.bash_timeout),
        read_max_lines: project.read_max_lines.or(global.read_max_lines),
        read_max_line_bytes: project.read_max_line_bytes.or(global.read_max_line_bytes),
        auto_compact: project.auto_compact.or(global.auto_compact),
        // Like the approval lists: a project can add disables, not re-enable.
        disable_tools: match (global.disable_tools, project.disable_tools) {
            (Some(mut g), Some(p)) => {
                g.extend(p);
                Some(g)
            }
            (g, p) => p.or(g),
        },
        approval,
        search: SearchFileConfig {
            provider: project.search.provider.or(global.search.provider),
            base_url: project.search.base_url.or(global.search.base_url),
            max_results: project.search.max_results.or(global.search.max_results),
        },
    }
}

fn load_instructions(root: &Path, files: &[String]) -> Vec<(String, String)> {
    files
        .iter()
        .filter_map(|name| {
            let mut content = std::fs::read_to_string(root.join(name)).ok()?;
            if content.len() > INSTRUCTION_FILE_MAX_BYTES {
                let mut cut = INSTRUCTION_FILE_MAX_BYTES;
                while !content.is_char_boundary(cut) {
                    cut -= 1;
                }
                content.truncate(cut);
                content.push_str("\n… (truncated)");
            }
            Some((name.clone(), content))
        })
        .collect()
}

// ----- resolved config ------------------------------------------------------

#[derive(Clone, Debug)]
pub struct Config {
    pub provider: Provider,
    pub model: String,
    /// Provider API base URL override (overrides the *_BASE_URL env vars).
    pub base_url: Option<String>,
    /// Named model entries available to `/model`.
    pub models: Vec<ModelEntry>,
    /// Name of the active `[[models]]` entry (None for CLI/ad-hoc selection).
    pub active_model: Option<String>,
    /// Where the startup model came from, when worth mentioning
    /// ("last used", "first model served by Ollama").
    pub model_note: Option<String>,
    /// Bash timeout in seconds, shared with the bash tool and adjustable at
    /// runtime (`/config`).
    pub bash_timeout: NumHandle,
    /// read_file output limits, shared with the tool and adjustable at
    /// runtime (`/config`).
    pub read_max_lines: NumHandle,
    pub read_max_line_bytes: NumHandle,
    /// Auto-compaction threshold in percent of the context window (0 = off),
    /// checked after each completed turn and adjustable at runtime (`/config`).
    pub auto_compact: NumHandle,
    /// Working directory the tools operate in.
    pub root: PathBuf,
    /// Approval rules, shared with the hook and extensible at runtime.
    pub approval: RulesHandle,
    /// Current permission mode, shared with the approval hook.
    pub mode: ModeHandle,
    /// Web-search settings, shared with the tool and editable at runtime
    /// (`/config`: provider and result count).
    pub search: SearchHandle,
    /// Tools left unregistered entirely (only web tools; from `disable_tools`
    /// in the config file, so a change requires a restart).
    pub disable_tools: Vec<String>,
    /// Base system prompt override from the config file (None = built-in).
    pub system_prompt: Option<String>,
    /// Instruction files that were found: (file name, content).
    pub instructions: Vec<(String, String)>,
    /// Config files that were loaded, for the startup notice.
    pub config_files: Vec<String>,
    /// Context-window size of the active model (for the usage gauge).
    pub context_window: u64,
}

impl Config {
    /// `/config` ←/→ steppers for the numeric rows, one per setting so both
    /// front ends share the step sizes and ranges.
    pub fn step_bash_timeout(&self, delta: i64) {
        self.bash_timeout.step(delta, 30, 30, 1800);
    }

    pub fn step_read_lines(&self, delta: i64) {
        self.read_max_lines.step(delta, 500, 500, 10_000);
    }

    pub fn step_line_bytes(&self, delta: i64) {
        self.read_max_line_bytes.step(delta, 100, 100, 5000);
    }

    /// Auto-compact threshold: ±5% between 50 and 95; stepping below 50
    /// turns it off (0), and stepping up from off restarts at 50.
    pub fn step_auto_compact(&self, delta: i64) {
        let cur = self.auto_compact.get() as i64;
        let next = if delta < 0 {
            if cur <= 50 { 0 } else { cur - 5 }
        } else if cur == 0 {
            50
        } else {
            (cur + 5).min(95)
        };
        self.auto_compact.set(next as u64);
    }

    pub fn from_args(args: Args) -> anyhow::Result<Self> {
        // The project root is the nearest ancestor holding a picocode.toml,
        // so starting from a subdirectory finds the same config, sessions
        // and state; without one the current directory is the root.
        let cwd = std::env::current_dir()?;
        let root = cwd
            .ancestors()
            .find(|d| d.join("picocode.toml").is_file())
            .map(Path::to_path_buf)
            .unwrap_or(cwd);

        let mut config_files = Vec::new();
        let global = match global_config_path() {
            Some(path) => {
                let loaded = load_file(&path)?;
                if loaded.is_some() {
                    config_files.push(path.display().to_string());
                }
                loaded
            }
            None => None,
        };
        let project_path = root.join("picocode.toml");
        let project = load_file(&project_path)?;
        if project.is_some() {
            config_files.push("picocode.toml".to_string());
        }
        let file = merge(global.unwrap_or_default(), project.unwrap_or_default());

        let models = file.models.unwrap_or_default();
        validate_models(&models, file.default_model.as_deref())?;
        validate_tool_lists(&file.approval)?;
        let bash_timeout = file.bash_timeout.unwrap_or(DEFAULT_BASH_TIMEOUT);
        if bash_timeout == 0 {
            anyhow::bail!("bash_timeout must be at least 1 second");
        }
        let read_max_lines = file.read_max_lines.unwrap_or(DEFAULT_READ_MAX_LINES);
        let read_max_line_bytes = file
            .read_max_line_bytes
            .unwrap_or(DEFAULT_READ_MAX_LINE_BYTES);
        if read_max_lines == 0 || read_max_line_bytes == 0 {
            anyhow::bail!("read_max_lines and read_max_line_bytes must be at least 1");
        }
        let auto_compact = file.auto_compact.unwrap_or(DEFAULT_AUTO_COMPACT);
        if auto_compact > 99 {
            anyhow::bail!("auto_compact must be 0 (off) to 99 (percent of the context window)");
        }
        let mut disable_tools = file.disable_tools.unwrap_or_default();
        disable_tools.sort();
        disable_tools.dedup();
        for name in &disable_tools {
            if !crate::tools::OPTIONAL_TOOLS.contains(&name.as_str()) {
                anyhow::bail!(
                    "disable_tools: `{name}` cannot be disabled (only {} can)",
                    crate::tools::OPTIONAL_TOOLS.join(", ")
                );
            }
        }

        // Startup model precedence: CLI flags > last-used state > config
        // default_model / first entry > empty (main() picks the first model
        // Ollama serves, or reports how to configure a provider).
        // --base-url is not a selection; it overrides the endpoint below.
        let cli_selection = args.provider.is_some() || args.model.is_some();
        let state = if cli_selection {
            None
        } else {
            crate::state::state_path(&root).and_then(|p| crate::state::load(&p))
        };
        let mut model_note = None;
        let (provider, model, base_url, active_model, context_window) = if cli_selection {
            let provider = args.provider.unwrap_or(Provider::Ollama);
            let model = args.model.unwrap_or_else(|| default_model_for(provider));
            (provider, model, None, None, DEFAULT_CONTEXT_WINDOW)
        } else if let Some(sel) = state.as_ref().and_then(|s| restore_selection(s, &models)) {
            model_note = Some("last used".to_string());
            sel
        } else if let Some(entry) = pick_entry(&models, file.default_model.as_deref()) {
            entry_selection(entry)
        } else {
            (
                Provider::Ollama,
                String::new(),
                None,
                None,
                DEFAULT_CONTEXT_WINDOW,
            )
        };
        let base_url = args.base_url.or(base_url);

        let instruction_names = file
            .instructions
            .unwrap_or_else(|| vec!["AGENTS.md".to_string()]);
        let instructions = load_instructions(&root, &instruction_names);
        let search = resolve_search(file.search, std::env::var("BRAVE_API_KEY").ok())?;

        Ok(Self {
            provider,
            model,
            base_url,
            models,
            active_model,
            model_note,
            bash_timeout: NumHandle::new(bash_timeout),
            read_max_lines: NumHandle::new(read_max_lines),
            read_max_line_bytes: NumHandle::new(read_max_line_bytes),
            auto_compact: NumHandle::new(auto_compact),
            root,
            approval: RulesHandle::new(file.approval),
            mode: ModeHandle::new(if args.bypass {
                Mode::Bypass
            } else {
                Mode::default()
            }),
            search: SearchHandle::new(search),
            disable_tools,
            system_prompt: file.system_prompt,
            instructions,
            config_files,
            context_window,
        })
    }

    pub fn model_label(&self) -> String {
        format!("{}/{}", provider_name(self.provider), self.model)
    }
}

mod rules;
mod search;

pub use rules::*;
pub use search::*;

use rules::validate_tool_lists;
use search::{SearchFileConfig, resolve_search};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn num_handle_shares_runtime_changes() {
        let handle = NumHandle::new(50);
        let worker_side = handle.clone();
        handle.set(120);
        assert_eq!(worker_side.get(), 120);
    }

    #[test]
    fn tool_limits_parse_and_merge() {
        let global: FileConfig = toml::from_str(
            "bash_timeout = 60\nread_max_lines = 100\nread_max_line_bytes = 200\nauto_compact = 70",
        )
        .unwrap();
        assert_eq!(global.bash_timeout, Some(60));
        assert_eq!(global.read_max_lines, Some(100));
        assert_eq!(global.read_max_line_bytes, Some(200));
        assert_eq!(global.auto_compact, Some(70));
        // Project value wins; a global one fills in. auto_compact = 0 (off)
        // is a real value, not an absent one.
        let project: FileConfig = toml::from_str("bash_timeout = 240\nauto_compact = 0").unwrap();
        let merged = merge(global, project);
        assert_eq!(merged.bash_timeout, Some(240));
        assert_eq!(merged.read_max_lines, Some(100));
        assert_eq!(merged.read_max_line_bytes, Some(200));
        assert_eq!(merged.auto_compact, Some(0));
    }

    #[test]
    fn context_window_parses_per_model() {
        let f: FileConfig = toml::from_str(
            r#"
            [[models]]
            name = "a"
            provider = "ollama"
            model = "m"
            context_window = 40960
            [[models]]
            name = "b"
            provider = "ollama"
            model = "m2"
            "#,
        )
        .unwrap();
        let models = f.models.unwrap();
        assert_eq!(models[0].context_window, Some(40960));
        assert_eq!(models[1].context_window, None);
    }

    #[test]
    fn config_files_parse_and_merge() {
        let global: FileConfig = toml::from_str(
            r#"
            default_model = "global"
            [[models]]
            name = "global"
            provider = "ollama"
            model = "qwen3:8b"
            [approval]
            allow_bash = ["ls"]
            "#,
        )
        .unwrap();
        let project: FileConfig = toml::from_str(
            r#"
            instructions = ["AGENTS.md", "STYLE.md"]
            system_prompt = "You are a project bot in {root}."
            [[models]]
            name = "local"
            provider = "ollama"
            model = "qwen3:4b"
            [[models]]
            name = "vllm"
            provider = "openai"
            model = "qwen3:8b"
            base_url = "http://host:8000/v1"
            [approval]
            allow_bash = ["cargo"]
            deny_bash = ["sudo"]
            "#,
        )
        .unwrap();
        let merged = merge(global, project);
        // Project [[models]] replace the global list wholesale, and take
        // default_model with them: the global default (which points into the
        // replaced roster) must not leak through.
        let models = merged.models.unwrap();
        assert_eq!(models.len(), 2);
        assert_eq!(models[1].base_url.as_deref(), Some("http://host:8000/v1"));
        assert_eq!(merged.default_model, None);
        assert!(validate_models(&models, merged.default_model.as_deref()).is_ok());
        assert_eq!(merged.approval.allow_bash, vec!["ls", "cargo"]);
        assert_eq!(merged.approval.deny_bash, vec!["sudo"]);
        assert_eq!(merged.instructions.unwrap(), vec!["AGENTS.md", "STYLE.md"]);
        assert_eq!(
            merged.system_prompt.as_deref(),
            Some("You are a project bot in {root}.")
        );
    }

    #[test]
    fn project_default_model_alone_picks_from_global_roster() {
        let global: FileConfig = toml::from_str(
            r#"
            [[models]]
            name = "a"
            provider = "ollama"
            model = "m1"
            [[models]]
            name = "b"
            provider = "ollama"
            model = "m2"
            "#,
        )
        .unwrap();
        let project: FileConfig = toml::from_str(r#"default_model = "b""#).unwrap();
        let merged = merge(global, project);
        assert_eq!(merged.default_model.as_deref(), Some("b"));
        assert_eq!(merged.models.unwrap().len(), 2);
    }

    #[test]
    fn disable_tools_concatenates_and_never_reenables() {
        let global: FileConfig = toml::from_str(r#"disable_tools = ["web_search"]"#).unwrap();
        let project: FileConfig = toml::from_str(r#"disable_tools = ["web_fetch"]"#).unwrap();
        assert_eq!(
            merge(global, project).disable_tools.unwrap(),
            vec!["web_search", "web_fetch"]
        );

        // A project without the key inherits the global disables.
        let global: FileConfig = toml::from_str(r#"disable_tools = ["web_search"]"#).unwrap();
        assert_eq!(
            merge(global, FileConfig::default()).disable_tools.unwrap(),
            vec!["web_search"]
        );
    }

    #[test]
    fn system_prompt_falls_back_to_global() {
        let global: FileConfig = toml::from_str(r#"system_prompt = "global prompt""#).unwrap();
        let project = FileConfig::default();
        assert_eq!(
            merge(global, project).system_prompt.as_deref(),
            Some("global prompt")
        );
    }

    #[test]
    fn saved_state_restores_entry_or_ad_hoc_selection() {
        use crate::state::LastModel;
        let models: Vec<ModelEntry> = toml::from_str::<FileConfig>(
            r#"
            [[models]]
            name = "local"
            provider = "ollama"
            model = "qwen3:4b"
            context_window = 40960
            "#,
        )
        .unwrap()
        .models
        .unwrap();
        let state = |entry: Option<&str>, provider: &str, model: &str| LastModel {
            entry: entry.map(str::to_string),
            provider: provider.into(),
            model: model.into(),
            base_url: None,
        };

        // A still-existing entry wins and applies its current definition.
        let sel = restore_selection(&state(Some("local"), "ollama", "old-model"), &models).unwrap();
        assert_eq!(sel.1, "qwen3:4b");
        assert_eq!(sel.3.as_deref(), Some("local"));
        assert_eq!(sel.4, 40960);

        // A removed entry falls back to the saved ad-hoc selection.
        let sel = restore_selection(&state(Some("gone"), "ollama", "qwen3:0.6b"), &models).unwrap();
        assert_eq!(sel.0, Provider::Ollama);
        assert_eq!(sel.1, "qwen3:0.6b");
        assert_eq!(sel.3, None);

        // Broken state is ignored.
        assert!(restore_selection(&state(None, "nope", "m"), &models).is_none());
        assert!(restore_selection(&state(None, "ollama", ""), &models).is_none());

        assert_eq!(provider_from_name("openai"), Some(Provider::Openai));
        assert_eq!(provider_from_name("x"), None);
    }

    #[test]
    fn cli_defaults_leave_ollama_model_to_discovery() {
        assert_eq!(default_model_for(Provider::Ollama), "");
        assert_eq!(default_model_for(Provider::Anthropic), "claude-opus-4-8");
        assert_eq!(default_model_for(Provider::Openai), "gpt-4o");
    }

    #[test]
    fn model_entry_validation_and_pick() {
        let models: Vec<ModelEntry> = toml::from_str::<FileConfig>(
            r#"
            [[models]]
            name = "a"
            provider = "ollama"
            model = "m1"
            [[models]]
            name = "b"
            provider = "openai"
            model = "m2"
            "#,
        )
        .unwrap()
        .models
        .unwrap();

        assert!(validate_models(&models, Some("b")).is_ok());
        assert!(validate_models(&models, Some("nope")).is_err());
        assert_eq!(pick_entry(&models, Some("b")).unwrap().name, "b");
        assert_eq!(pick_entry(&models, None).unwrap().name, "a");

        let mut dup = models.clone();
        dup.push(models[0].clone());
        assert!(validate_models(&dup, None).is_err());

        let mut spaced = models.clone();
        spaced[0].name = "has space".into();
        assert!(validate_models(&spaced, None).is_err());
    }

    #[test]
    fn unknown_config_keys_are_rejected() {
        assert!(toml::from_str::<FileConfig>("allow_bash = []").is_err());
        assert!(toml::from_str::<FileConfig>("[approval]\nallowbash = []").is_err());
        // The pre-[[models]] top-level keys are no longer accepted.
        assert!(toml::from_str::<FileConfig>("model = \"qwen3:4b\"").is_err());
    }

    #[test]
    fn instructions_load_existing_files_only() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("AGENTS.md"), "be nice").unwrap();
        let loaded = load_instructions(dir.path(), &["AGENTS.md".into(), "MISSING.md".into()]);
        assert_eq!(
            loaded,
            vec![("AGENTS.md".to_string(), "be nice".to_string())]
        );
    }
}
