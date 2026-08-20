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

/// A ready-to-paste picocode.toml snippet keeping a custom system prompt
/// (shown after `/prompt` applies one). A prompt containing `"""` cannot
/// be expressed in a TOML multi-line basic string as-is, so its quotes
/// are escaped.
pub fn system_prompt_snippet(prompt: &str) -> String {
    let body = prompt.trim_end().replace("\"\"\"", "\\\"\\\"\\\"");
    format!("system_prompt = \"\"\"\n{body}\n\"\"\"")
}

/// All selectable providers, in the add-model form's cycle order.
pub const PROVIDERS: &[Provider] = &[Provider::Ollama, Provider::Anthropic, Provider::Openai];

impl Provider {
    /// Neighbouring provider in the add-model form (wraps around).
    pub fn cycled(self, delta: i64) -> Provider {
        let ix = PROVIDERS.iter().position(|p| *p == self).unwrap_or(0) as i64;
        let n = PROVIDERS.len() as i64;
        PROVIDERS[((ix + delta).rem_euclid(n)) as usize]
    }

    /// Which environment variable authenticates this provider (shown in the
    /// add-model form so a missing key is obvious up front).
    pub fn api_key_hint(self) -> &'static str {
        match self {
            Provider::Ollama => "no key needed (OLLAMA_API_KEY optional)",
            Provider::Anthropic => "uses ANTHROPIC_API_KEY",
            Provider::Openai => "uses OPENAI_API_KEY (local servers may not check it)",
        }
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
#[command(
    name = "picocode",
    version,
    about = "picocode — a minimal coding agent"
)]
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

    /// Start in auto mode: the approval prompts are answered by a reviewer
    /// model instead of you (deny rules and the always-ask commands still
    /// reach you). Ignored when --bypass is given.
    #[arg(long)]
    pub auto: bool,

    /// Open a remote workspace over SSH: `host:/path` (host is an ssh alias
    /// from ~/.ssh/config or user@host), or the name of a `[[remotes]]`
    /// entry. All tools then operate on the remote host.
    #[arg(long, value_name = "HOST:PATH")]
    pub remote: Option<String>,

    /// Run one prompt without the TUI: stream the reply to stdout (tool
    /// activity goes to stderr) and exit. Tool calls that would need
    /// confirmation are denied unless --bypass is also given.
    #[arg(short = 'p', long = "print", value_name = "PROMPT")]
    pub print: Option<String>,

    /// File to attach to the --print prompt (image, or anything that reads
    /// as text). Repeatable.
    #[arg(long, value_name = "PATH", requires = "print")]
    pub attach: Vec<PathBuf>,

    /// Headless mode for debugging: run one prompt without the TUI and print
    /// events to stdout. Implies bypass mode.
    #[arg(long, hide = true)]
    pub smoke: Option<String>,

    /// File attached to the --smoke prompt (E2E for multimodal messages).
    #[arg(long, hide = true, requires = "smoke")]
    pub smoke_attach: Option<PathBuf>,

    /// Text pushed into the steering queue ~1s after the --smoke prompt
    /// starts (E2E for mid-turn injection).
    #[arg(long, hide = true, requires = "smoke")]
    pub smoke_steer: Option<String>,

    /// Goal set before the --smoke prompt runs, so the goal loop can be
    /// driven headlessly (E2E for `/goal`).
    #[arg(long, hide = true, requires = "smoke")]
    pub smoke_goal: Option<String>,
}

impl Args {
    /// The arguments for re-resolving the config when the workspace changes
    /// at runtime (`/remote`, the GUI's directory picker): no CLI overrides,
    /// so every setting comes from the config files again.
    pub fn for_workspace(remote: Option<String>) -> Self {
        Self {
            provider: None,
            model: None,
            base_url: None,
            bypass: false,
            auto: false,
            remote,
            print: None,
            attach: Vec::new(),
            smoke: None,
            smoke_attach: None,
            smoke_steer: None,
            smoke_goal: None,
        }
    }
}

// ----- config file ----------------------------------------------------------

/// On-disk config (`picocode.toml` in the project root, merged over
/// `~/.config/picocode/config.toml`).
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct FileConfig {
    /// Name of the `[[models]]` entry to use at startup (default: the first).
    default_model: Option<String>,
    /// Named model entries, switchable at runtime with `/model <name>`.
    models: Option<Vec<ModelEntry>>,
    /// Instruction files loaded into the system prompt when present.
    instructions: Option<Vec<String>>,
    /// Replaces the built-in base system prompt. `{root}` expands to the
    /// working directory. Instruction files are still appended after it.
    system_prompt: Option<String>,
    /// Named system-prompt presets, switchable at runtime with
    /// `/prompt <name>`.
    prompts: Option<Vec<PromptPreset>>,
    /// MCP servers to connect at startup (opt-in: none configured means
    /// no MCP code runs and nothing changes for the model).
    mcp_servers: Option<Vec<McpServer>>,
    /// Named remote workspaces (`--remote <name>`).
    remotes: Option<Vec<RemoteEntry>>,
    /// Seconds before a bash command is moved to the background (default 120).
    bash_timeout: Option<u64>,
    /// Max lines a single read_file call returns (default 2000).
    read_max_lines: Option<u64>,
    /// Bytes per line before read_file truncates it (default 500).
    read_max_line_bytes: Option<u64>,
    /// Context usage (percent of the window) at which the conversation is
    /// compacted automatically after a turn; 0 disables (default 85).
    auto_compact: Option<u64>,
    /// How many follow-up turns a `/goal` may run before it stops and hands
    /// back to the user (default 10).
    goal_max_rounds: Option<u64>,
    /// Cap on the tokens one reply may generate (default 8192). 0 means
    /// picocode sets no cap and leaves the limit to the provider.
    max_tokens: Option<u64>,
    /// Tools to leave unregistered entirely (schemas never sent to the
    /// model). Only the web tools (`web_search`, `web_fetch`) can be listed.
    disable_tools: Option<Vec<String>>,
    /// Shell command run after every successful edit_file write (e.g.
    /// `cargo check`); its verdict is appended to the tool result so the
    /// model sees breakage immediately without being told to verify.
    after_edit: Option<String>,
    /// Which key sends the message in the input box: `enter` (default),
    /// `shift-enter`, `ctrl-enter` or `cmd-enter`. The others insert a
    /// newline.
    submit_key: Option<crate::keys::SubmitKey>,
    #[serde(default)]
    approval: ApprovalRules,
    #[serde(default)]
    search: SearchFileConfig,
    #[serde(default)]
    sandbox: SandboxFileConfig,
}

/// The `[sandbox]` section of the config file.
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct SandboxFileConfig {
    mode: Option<crate::sandbox::SandboxMode>,
    allow_network: Option<bool>,
    allow_write: Option<Vec<PathBuf>>,
}

/// Default bash timeout in seconds.
pub const DEFAULT_BASH_TIMEOUT: u64 = 120;
/// Default read_file output limits.
pub const DEFAULT_READ_MAX_LINES: u64 = 2000;
pub const DEFAULT_READ_MAX_LINE_BYTES: u64 = 500;
/// Default auto-compaction threshold (percent of the context window).
pub const DEFAULT_AUTO_COMPACT: u64 = 85;
/// Default `/goal` round limit: follow-up turns before the loop hands back.
pub const DEFAULT_GOAL_MAX_ROUNDS: u64 = 10;
/// Default cap on the tokens one reply may generate (0 = no cap).
pub const DEFAULT_MAX_TOKENS: u64 = 8192;

/// Fallback context-window size when a model entry doesn't declare one.
/// Only used for the status-bar usage gauge.
pub const DEFAULT_CONTEXT_WINDOW: u64 = 32_768;

/// One named `[[prompts]]` preset in the config file: a full replacement
/// for the base system prompt (`{root}` expands, instructions are still
/// appended), applied at runtime with `/prompt <name>`.
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PromptPreset {
    pub name: String,
    pub prompt: String,
}

/// A resolved remote workspace target: an ssh destination and the path
/// on the host to use as the working directory.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RemoteSpec {
    /// ssh destination — an alias or `user@host`.
    pub destination: String,
    /// Working directory on the remote host.
    pub path: PathBuf,
}

impl RemoteSpec {
    /// Parse `host:/path` (the last colon separates host from path, so
    /// `user@host:/srv/app` works). Resolves `name` against configured
    /// `[[remotes]]` first.
    pub fn parse(spec: &str, remotes: &[RemoteEntry]) -> anyhow::Result<Self> {
        if let Some(entry) = remotes.iter().find(|r| r.name == spec) {
            return Ok(Self {
                destination: entry.host.clone(),
                path: PathBuf::from(&entry.path),
            });
        }
        let (host, path) = spec.rsplit_once(':').ok_or_else(|| {
            anyhow::anyhow!("--remote expects `host:/path` (or a [[remotes]] name), got `{spec}`")
        })?;
        if host.is_empty() || path.is_empty() {
            anyhow::bail!("--remote `{spec}` is missing the host or the path");
        }
        Ok(Self {
            destination: host.to_string(),
            path: PathBuf::from(path),
        })
    }

    /// The `host:/path` form, as `--remote` would take it (used to
    /// re-resolve the config when switching workspaces at runtime).
    pub fn to_arg(&self) -> String {
        format!("{}:{}", self.destination, self.path.display())
    }

    /// Filesystem-safe slug identifying this remote for session storage,
    /// e.g. `ssh-user@host-srv-app`.
    pub fn slug(&self) -> String {
        let raw = format!("ssh-{}-{}", self.destination, self.path.display());
        raw.chars()
            .map(|c| {
                if c.is_alphanumeric() || c == '-' {
                    c
                } else {
                    '-'
                }
            })
            .collect()
    }
}

/// One `[[remotes]]` entry: a named remote workspace.
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RemoteEntry {
    pub name: String,
    /// ssh alias or `user@host`.
    pub host: String,
    /// Working directory on the host.
    pub path: String,
}

/// One `[[mcp_servers]]` entry: an MCP server to connect at startup.
/// Exactly one of `command` (stdio child process) or `url` (streamable
/// HTTP) must be set.
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct McpServer {
    pub name: String,
    /// Executable for a stdio server (e.g. `npx`).
    #[serde(default)]
    pub command: Option<String>,
    #[serde(default)]
    pub args: Vec<String>,
    /// Extra environment variables for the child process.
    #[serde(default)]
    pub env: std::collections::HashMap<String, String>,
    /// Endpoint of a streamable-HTTP server (e.g. `http://localhost:8000/mcp`).
    #[serde(default)]
    pub url: Option<String>,
}

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

/// Install the process-wide TLS crypto provider (ring). reqwest is built
/// without a bundled provider (rustls-no-provider — aws-lc needs CMake+NASM
/// on Windows and blocks cross-builds), so this must run before anything
/// opens an HTTPS connection. Called from `Config::from_args` and from the
/// web tools' constructors; repeat calls are harmless no-ops.
pub fn install_tls_provider() {
    let _ = rustls::crypto::ring::default_provider().install_default();
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
        // Like models: a project's preset list replaces the global one.
        prompts: project.prompts.or(global.prompts),
        mcp_servers: project.mcp_servers.or(global.mcp_servers),
        remotes: project.remotes.or(global.remotes),
        bash_timeout: project.bash_timeout.or(global.bash_timeout),
        read_max_lines: project.read_max_lines.or(global.read_max_lines),
        read_max_line_bytes: project.read_max_line_bytes.or(global.read_max_line_bytes),
        auto_compact: project.auto_compact.or(global.auto_compact),
        goal_max_rounds: project.goal_max_rounds.or(global.goal_max_rounds),
        max_tokens: project.max_tokens.or(global.max_tokens),
        after_edit: project.after_edit.or(global.after_edit),
        submit_key: project.submit_key.or(global.submit_key),
        // Like the approval lists: a project can add disables, not re-enable.
        disable_tools: match (global.disable_tools, project.disable_tools) {
            (Some(mut g), Some(p)) => {
                g.extend(p);
                Some(g)
            }
            (g, p) => p.or(g),
        },
        approval,
        sandbox: SandboxFileConfig {
            mode: project.sandbox.mode.or(global.sandbox.mode),
            allow_network: project
                .sandbox
                .allow_network
                .or(global.sandbox.allow_network),
            allow_write: project.sandbox.allow_write.or(global.sandbox.allow_write),
        },
        search: SearchFileConfig {
            provider: project.search.provider.or(global.search.provider),
            base_url: project.search.base_url.or(global.search.base_url),
            max_results: project.search.max_results.or(global.search.max_results),
        },
    }
}

/// The numeric and list settings of a merged file config, validated.
struct Settings {
    bash_timeout: u64,
    read_max_lines: u64,
    read_max_line_bytes: u64,
    auto_compact: u64,
    goal_max_rounds: u64,
    max_tokens: u64,
    disable_tools: Vec<String>,
}

/// Validate and default the settings a config file can carry. Shared by
/// the startup resolution and the remote-workspace reload so a host's
/// `picocode.toml` is checked exactly like a local one.
fn resolve_settings(file: &FileConfig) -> anyhow::Result<Settings> {
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
    let goal_max_rounds = file.goal_max_rounds.unwrap_or(DEFAULT_GOAL_MAX_ROUNDS);
    if goal_max_rounds == 0 || goal_max_rounds > 100 {
        anyhow::bail!("goal_max_rounds must be 1 to 100 follow-up turns");
    }
    let max_tokens = file.max_tokens.unwrap_or(DEFAULT_MAX_TOKENS);
    if max_tokens != 0 && !(256..=1_000_000).contains(&max_tokens) {
        anyhow::bail!("max_tokens must be 0 (no cap from picocode) or 256 to 1000000");
    }
    let mut disable_tools = file.disable_tools.clone().unwrap_or_default();
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
    Ok(Settings {
        bash_timeout,
        read_max_lines,
        read_max_line_bytes,
        auto_compact,
        goal_max_rounds,
        max_tokens,
        disable_tools,
    })
}

/// Strip the sections of a remote workspace's `picocode.toml` that describe
/// the *local* machine rather than the workspace: the model roster (whose
/// endpoints and API keys are local), MCP servers (stdio ones would be
/// launched as local child processes), the OS sandbox (a local-only guard),
/// the web-search settings (local network + local API key) and the send
/// key (a preference of the person at the local keyboard). What remains —
/// approval rules, instruction files, the system prompt and its presets,
/// `after_edit`, the timeouts and output limits — genuinely belongs to the
/// project being worked on.
fn workspace_only(mut file: FileConfig) -> FileConfig {
    file.models = None;
    file.default_model = None;
    file.mcp_servers = None;
    file.remotes = None;
    file.submit_key = None;
    file.sandbox = SandboxFileConfig::default();
    file.search = SearchFileConfig::default();
    file
}

fn load_instructions(root: &Path, files: &[String]) -> Vec<(String, String)> {
    files
        .iter()
        .filter_map(|name| {
            Some((
                name.clone(),
                cap_instruction(std::fs::read(root.join(name)).ok()?),
            ))
        })
        .collect()
}

/// Truncate an instruction file's bytes to the cap and lossily decode.
fn cap_instruction(bytes: Vec<u8>) -> String {
    let mut content = String::from_utf8_lossy(&bytes).into_owned();
    if content.len() > INSTRUCTION_FILE_MAX_BYTES {
        let mut cut = INSTRUCTION_FILE_MAX_BYTES;
        while !content.is_char_boundary(cut) {
            cut -= 1;
        }
        content.truncate(cut);
        content.push_str("\n… (truncated)");
    }
    content
}

/// Load the instruction files from a (possibly remote) backend rooted at
/// `root`. Used by the front ends for remote workspaces, where the files
/// live on the host.
pub async fn load_instructions_via(
    backend: &crate::backend::Backend,
    root: &Path,
    files: &[String],
) -> Vec<(String, String)> {
    let mut out = Vec::new();
    for name in files {
        if let Ok(bytes) = backend.read(&root.join(name)).await {
            out.push((name.clone(), cap_instruction(bytes)));
        }
    }
    out
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
    /// How many follow-up turns a `/goal` runs before handing back to the
    /// user. Read by the worker, which is respawned whenever the workspace
    /// or the model changes, so a plain number is enough.
    pub goal_max_rounds: u64,
    /// Cap on the tokens one reply may generate, adjustable at runtime
    /// (`/config`); 0 leaves the limit to the provider. The worker rebuilds
    /// its agents when it changes — for Ollama it also travels as
    /// `num_predict`, next to the `num_ctx` taken from `context_window`.
    pub max_tokens: NumHandle,
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
    /// Shell command run after each successful edit_file write; its verdict
    /// is appended to the tool result (from `after_edit` in the config file).
    pub after_edit: Option<String>,
    /// Which key sends the message in the input box (the rest of the Enter
    /// combinations insert a newline); a local preference, so a remote
    /// workspace's config never changes it.
    pub submit_key: crate::keys::SubmitKey,
    /// Base system prompt override from the config file (None = built-in).
    pub system_prompt: Option<String>,
    /// Named system-prompt presets (`[[prompts]]`), switchable with
    /// `/prompt <name>`.
    pub prompts: Vec<PromptPreset>,
    /// MCP servers to connect at startup (`[[mcp_servers]]`, opt-in).
    pub mcp_servers: Vec<McpServer>,
    /// Remote workspace target when `--remote` was given (else local).
    pub remote: Option<RemoteSpec>,
    /// Named remote workspaces from the local config (`[[remotes]]`),
    /// selectable at runtime with `/remote <name>`.
    pub remotes: Vec<RemoteEntry>,
    /// Instruction file names to look for (loaded from the remote root by
    /// the front end when `remote` is set).
    pub instruction_names: Vec<String>,
    /// The merged local config file, kept so a remote workspace's own
    /// `picocode.toml` can be merged over it after the connection is up.
    pub(crate) local_file: FileConfig,
    /// OS sandbox for model-initiated bash commands (`[sandbox]`, opt-in).
    pub sandbox: crate::sandbox::SandboxSettings,
    /// Instruction files that were found: (file name, content).
    pub instructions: Vec<(String, String)>,
    /// Config files that were loaded, for the startup notice.
    pub config_files: Vec<String>,
    /// The project `picocode.toml` the trust gate applies to, whether or not
    /// it exists — what `/trust` records.
    pub project_config: PathBuf,
    /// Settings that `picocode.toml` asked for and did not get, because the
    /// file is not trusted yet. Empty in the ordinary case; when it is not,
    /// the front ends say so at startup and `/trust` allows them.
    pub gated_settings: Vec<&'static str>,
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

    /// Reply-length cap: powers of two between 1024 and 131072; stepping
    /// below 1024 turns the cap off (0 — the provider's own limit applies),
    /// and stepping up from off restarts at 1024.
    pub fn step_max_tokens(&self, delta: i64) {
        const MIN: u64 = 1024;
        const MAX: u64 = 131_072;
        let cur = self.max_tokens.get();
        let next = if delta < 0 {
            match cur {
                0 => 0,
                c if c <= MIN => 0,
                c => (c / 2).max(MIN),
            }
        } else {
            match cur {
                0 => MIN,
                c => (c.saturating_mul(2)).min(MAX),
            }
        };
        self.max_tokens.set(next);
    }

    /// The reply-length cap as a `/config` row value.
    pub fn max_tokens_label(&self) -> String {
        match self.max_tokens.get() {
            0 => "no limit (provider default)".to_string(),
            n => format!("{n} tokens"),
        }
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
        install_tls_provider();

        // The project root is the nearest ancestor holding a picocode.toml,
        // so starting from a subdirectory finds the same config, sessions
        // and state; without one the current directory is the root.
        let cwd = std::env::current_dir()?;
        let local_root = cwd
            .ancestors()
            .find(|d| d.join("picocode.toml").is_file())
            .map(Path::to_path_buf)
            .unwrap_or_else(|| cwd.clone());

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
        let project_path = local_root.join("picocode.toml");
        let mut project = load_file(&project_path)?;
        if project.is_some() {
            // Name the file in full when the ancestor walk found it above
            // the working directory: "picocode.toml" would hide that.
            config_files.push(if local_root == cwd {
                "picocode.toml".to_string()
            } else {
                project_path.display().to_string()
            });
        }
        // A project config can run commands and relax approvals, and it is
        // found by walking up from the working directory — so the settings
        // that carry that power wait for `/trust`. See config::trust.
        let mut gated_settings = Vec::new();
        if let Some(file) = project.as_mut()
            && let Ok(bytes) = std::fs::read(&project_path)
            && !trust::is_trusted(&project_path, &bytes)
        {
            gated_settings = trust::strip(file);
        }
        let file = merge(global.unwrap_or_default(), project.unwrap_or_default());

        let models = file.models.clone().unwrap_or_default();
        validate_models(&models, file.default_model.as_deref())?;
        let settings = resolve_settings(&file)?;

        // Startup model precedence: CLI flags > last-used state > config
        // default_model / first entry > empty (main() picks the first model
        // Ollama serves, or reports how to configure a provider).
        // --base-url is not a selection; it overrides the endpoint below.
        let cli_selection = args.provider.is_some() || args.model.is_some();
        let state = if cli_selection {
            None
        } else {
            crate::state::state_path(&local_root).and_then(|p| crate::state::load(&p))
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

        // Remote workspace: the root becomes the host path and the tools
        // operate over SSH. The host's own picocode.toml and instruction
        // files are applied by `apply_workspace_settings` once the
        // connection is up (async); locally they're read now.
        let remotes = file.remotes.clone().unwrap_or_default();
        let remote = match &args.remote {
            Some(spec) => Some(RemoteSpec::parse(spec, &remotes)?),
            None => None,
        };
        let root = match &remote {
            Some(spec) => spec.path.clone(),
            None => local_root,
        };
        let instruction_names = file
            .instructions
            .clone()
            .unwrap_or_else(|| vec!["AGENTS.md".to_string()]);
        let instructions = if remote.is_some() {
            Vec::new()
        } else {
            load_instructions(&root, &instruction_names)
        };
        let search = resolve_search(file.search.clone(), std::env::var("BRAVE_API_KEY").ok())?;

        Ok(Self {
            provider,
            model,
            base_url,
            models,
            active_model,
            model_note,
            bash_timeout: NumHandle::new(settings.bash_timeout),
            read_max_lines: NumHandle::new(settings.read_max_lines),
            read_max_line_bytes: NumHandle::new(settings.read_max_line_bytes),
            auto_compact: NumHandle::new(settings.auto_compact),
            goal_max_rounds: settings.goal_max_rounds,
            max_tokens: NumHandle::new(settings.max_tokens),
            root,
            approval: RulesHandle::new(file.approval.clone()),
            mode: ModeHandle::new(match (args.bypass, args.auto) {
                (true, _) => Mode::Bypass,
                (false, true) => Mode::Auto,
                _ => Mode::default(),
            }),
            search: SearchHandle::new(search),
            disable_tools: settings.disable_tools,
            after_edit: file.after_edit.clone().filter(|c| !c.trim().is_empty()),
            submit_key: file.submit_key.unwrap_or_default(),
            system_prompt: file.system_prompt.clone(),
            prompts: file.prompts.clone().unwrap_or_default(),
            mcp_servers: file.mcp_servers.clone().unwrap_or_default(),
            remote,
            remotes,
            instruction_names,
            sandbox: crate::sandbox::SandboxSettings {
                mode: file.sandbox.mode.unwrap_or_default(),
                allow_network: file.sandbox.allow_network.unwrap_or(false),
                allow_write: file.sandbox.allow_write.clone().unwrap_or_default(),
            },
            instructions,
            config_files,
            project_config: project_path,
            gated_settings,
            context_window,
            local_file: file,
        })
    }

    /// Apply a remote workspace's own `picocode.toml` (merged over the local
    /// config) and load its instruction files. Called by the front ends once
    /// the SSH connection is up; a no-op for a local workspace, where
    /// `from_args` already read both.
    ///
    /// Only workspace-shaped settings are taken from the host (see
    /// [`workspace_only`]) — the model roster, MCP servers, the OS sandbox
    /// and web search stay under local control, so opening a remote
    /// workspace never makes the local machine run something it was not
    /// already configured to run.
    pub async fn apply_workspace_settings(
        &mut self,
        backend: &crate::backend::Backend,
    ) -> anyhow::Result<()> {
        if !backend.is_remote() {
            return Ok(());
        }
        let path = self.root.join("picocode.toml");
        let file = match backend.read(&path).await {
            Ok(bytes) => {
                let text = String::from_utf8_lossy(&bytes).into_owned();
                let remote: FileConfig = toml::from_str(&text)
                    .with_context(|| format!("invalid config file: {}", path.display()))?;
                self.config_files
                    .push(format!("{}:picocode.toml", backend.label()));
                merge(self.local_file.clone(), workspace_only(remote))
            }
            Err(_) => self.local_file.clone(),
        };

        let settings = resolve_settings(&file)?;
        self.bash_timeout.set(settings.bash_timeout);
        self.read_max_lines.set(settings.read_max_lines);
        self.read_max_line_bytes.set(settings.read_max_line_bytes);
        self.auto_compact.set(settings.auto_compact);
        self.goal_max_rounds = settings.goal_max_rounds;
        self.max_tokens.set(settings.max_tokens);
        self.disable_tools = settings.disable_tools;
        self.approval = RulesHandle::new(file.approval);
        self.after_edit = file.after_edit.filter(|c| !c.trim().is_empty());
        self.system_prompt = file.system_prompt;
        self.prompts = file.prompts.unwrap_or_default();
        self.instruction_names = file
            .instructions
            .unwrap_or_else(|| vec!["AGENTS.md".to_string()]);
        self.instructions =
            load_instructions_via(backend, &self.root, &self.instruction_names).await;
        Ok(())
    }

    pub fn model_label(&self) -> String {
        format!("{}/{}", provider_name(self.provider), self.model)
    }
}

#[cfg(test)]
impl Config {
    /// A minimal valid config (Ollama on defaults) for unit tests across
    /// the crate; tweak fields as needed.
    pub(crate) fn for_tests() -> Self {
        Config {
            provider: Provider::Ollama,
            model: "qwen3:4b".into(),
            base_url: None,
            models: Vec::new(),
            active_model: None,
            model_note: None,
            bash_timeout: NumHandle::new(120),
            read_max_lines: NumHandle::new(2000),
            read_max_line_bytes: NumHandle::new(500),
            auto_compact: NumHandle::new(85),
            goal_max_rounds: DEFAULT_GOAL_MAX_ROUNDS,
            max_tokens: NumHandle::new(DEFAULT_MAX_TOKENS),
            root: std::path::PathBuf::from("/tmp/proj"),
            submit_key: crate::keys::SubmitKey::default(),
            approval: RulesHandle::new(ApprovalRules::default()),
            mode: ModeHandle::new(Mode::ReadOnly),
            search: SearchHandle::new(SearchConfig {
                provider: SearchProvider::Duckduckgo,
                base_url: None,
                max_results: 5,
                api_key: None,
            }),
            disable_tools: Vec::new(),
            after_edit: None,
            system_prompt: None,
            prompts: Vec::new(),
            mcp_servers: Vec::new(),
            remote: None,
            remotes: Vec::new(),
            instruction_names: Vec::new(),
            sandbox: crate::sandbox::SandboxSettings::default(),
            instructions: Vec::new(),
            config_files: Vec::new(),
            project_config: std::path::PathBuf::from("/tmp/proj/picocode.toml"),
            gated_settings: Vec::new(),
            context_window: DEFAULT_CONTEXT_WINDOW,
            local_file: Default::default(),
        }
    }
}

mod rules;
mod search;
pub mod trust;

pub use rules::*;
pub use search::*;

use rules::validate_tool_lists;
use search::{SearchFileConfig, resolve_search};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_reply_cap_steps_by_doubling_and_can_be_turned_off() {
        let cfg = Config::for_tests();
        assert_eq!(cfg.max_tokens.get(), DEFAULT_MAX_TOKENS);
        cfg.step_max_tokens(1);
        assert_eq!(cfg.max_tokens.get(), 16_384);
        cfg.step_max_tokens(-1);
        cfg.step_max_tokens(-1);
        assert_eq!(cfg.max_tokens.get(), 4096);
        // Stepping below the minimum turns the cap off; stepping up from
        // off restarts at the minimum.
        for _ in 0..3 {
            cfg.step_max_tokens(-1);
        }
        assert_eq!(cfg.max_tokens.get(), 0);
        assert!(cfg.max_tokens_label().contains("no limit"));
        cfg.step_max_tokens(-1);
        assert_eq!(cfg.max_tokens.get(), 0, "off is the floor");
        cfg.step_max_tokens(1);
        assert_eq!(cfg.max_tokens.get(), 1024);
        // And it never runs past the ceiling.
        for _ in 0..20 {
            cfg.step_max_tokens(1);
        }
        assert_eq!(cfg.max_tokens.get(), 131_072);
    }

    #[test]
    fn num_handle_shares_runtime_changes() {
        let handle = NumHandle::new(50);
        let worker_side = handle.clone();
        handle.set(120);
        assert_eq!(worker_side.get(), 120);
    }

    #[test]
    fn a_remote_config_contributes_workspace_settings_only() {
        let local: FileConfig = toml::from_str(
            "bash_timeout = 60\n\
             [[models]]\nname = \"local\"\nprovider = \"ollama\"\nmodel = \"qwen3:4b\"\n\
             [[mcp_servers]]\nname = \"time\"\ncommand = \"uvx\"\n",
        )
        .unwrap();
        let remote: FileConfig = toml::from_str(
            "bash_timeout = 300\nafter_edit = \"cargo check\"\n\
             instructions = [\"HOST.md\"]\n\
             [approval]\nallow_bash = [\"ls\"]\n\
             [[models]]\nname = \"host\"\nprovider = \"openai\"\nmodel = \"gpt-4o\"\n\
             [[mcp_servers]]\nname = \"evil\"\ncommand = \"rm\"\n\
             [sandbox]\nmode = \"off\"\n",
        )
        .unwrap();

        let merged = merge(local, workspace_only(remote));
        // Workspace-shaped settings come from the host…
        assert_eq!(merged.bash_timeout, Some(300));
        assert_eq!(merged.after_edit.as_deref(), Some("cargo check"));
        assert_eq!(merged.instructions, Some(vec!["HOST.md".to_string()]));
        assert_eq!(merged.approval.allow_bash, vec!["ls".to_string()]);
        // …but anything that would run or connect locally stays local.
        let models = merged.models.unwrap();
        assert_eq!(models.len(), 1);
        assert_eq!(models[0].name, "local");
        let servers = merged.mcp_servers.unwrap();
        assert_eq!(servers.len(), 1);
        assert_eq!(servers[0].name, "time");
        assert!(merged.sandbox.mode.is_none());
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
