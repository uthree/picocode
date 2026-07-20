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
#[command(version, about)]
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

    /// Maximum model turns (tool-call rounds) per user prompt.
    #[arg(long, default_value_t = 50)]
    pub max_turns: usize,

    /// Headless mode for debugging: run one prompt without the TUI and print
    /// events to stdout. Implies bypass mode.
    #[arg(long, hide = true)]
    pub smoke: Option<String>,
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

/// Reject typos in the `[approval]` tool lists early: an unknown name would
/// otherwise be silently ineffective.
fn validate_tool_lists(rules: &ApprovalRules) -> anyhow::Result<()> {
    for name in rules.allow_tools.iter().chain(&rules.deny_tools) {
        if !crate::tools::ALL_TOOLS.contains(&name.as_str()) {
            anyhow::bail!(
                "unknown tool `{name}` in [approval] (valid tools: {})",
                crate::tools::ALL_TOOLS.join(", ")
            );
        }
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

// ----- web search -----------------------------------------------------------

/// Web search backends selectable in the `[search]` config section.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SearchProvider {
    /// DuckDuckGo's HTML endpoint. No API key needed (default).
    #[default]
    Duckduckgo,
    /// A SearXNG instance; requires `base_url` (and `format: json` enabled
    /// server-side).
    Searxng,
    /// Brave Search API; requires the `BRAVE_API_KEY` environment variable.
    Brave,
}

impl SearchProvider {
    pub fn label(self) -> &'static str {
        match self {
            SearchProvider::Duckduckgo => "duckduckgo",
            SearchProvider::Searxng => "searxng",
            SearchProvider::Brave => "brave",
        }
    }
}

/// `[search]` section of the config file.
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct SearchFileConfig {
    provider: Option<SearchProvider>,
    /// Overrides the provider's endpoint (required for searxng).
    base_url: Option<String>,
    max_results: Option<usize>,
}

/// Search settings after defaults and validation.
#[derive(Clone, Debug)]
pub struct SearchConfig {
    pub provider: SearchProvider,
    pub base_url: Option<String>,
    pub max_results: usize,
    /// API key for providers that need one (brave).
    pub api_key: Option<String>,
}

fn resolve_search(
    file: SearchFileConfig,
    brave_key: Option<String>,
) -> anyhow::Result<SearchConfig> {
    let provider = file.provider.unwrap_or_default();
    if provider == SearchProvider::Searxng && file.base_url.is_none() {
        anyhow::bail!("[search] provider \"searxng\" requires base_url in the config");
    }
    // The key is kept even when another provider starts selected, so `/config`
    // can switch to brave at runtime.
    if provider == SearchProvider::Brave && brave_key.is_none() {
        anyhow::bail!(
            "[search] provider \"brave\" requires the BRAVE_API_KEY environment variable"
        );
    }
    Ok(SearchConfig {
        provider,
        base_url: file.base_url,
        max_results: file.max_results.unwrap_or(5).clamp(1, 20),
        api_key: brave_key,
    })
}

// ----- permission modes ------------------------------------------------------

/// Permission mode, cycled with Shift+Tab in the TUI. Deny rules take
/// precedence over the mode; see [`ApprovalRules::decide`].
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Mode {
    /// Reads run freely; every write or command asks, even if allow-listed.
    #[default]
    ReadOnly,
    /// File writes run freely; commands and other tools follow the config
    /// allow/deny rules.
    Edit,
    /// Writes and commands are auto-denied: the model investigates with the
    /// read-only tools and presents a plan instead of acting.
    Plan,
    /// Everything runs without confirmation (deny rules still apply). Meant
    /// for isolated environments (containers); only reachable via the
    /// explicit /bypass command or the --bypass flag, never via Shift+Tab.
    Bypass,
}

impl Mode {
    /// Every mode, in `ModeHandle` storage order.
    pub const ALL: &[Mode] = &[Mode::ReadOnly, Mode::Edit, Mode::Plan, Mode::Bypass];

    /// Shift+Tab cycle. Bypass is deliberately excluded: it can only be
    /// entered with /bypass, and Shift+Tab from it returns to read-only.
    /// Future modes need a variant, an ALL entry, an entry here (unless
    /// command-only), a label, and their branch in `ApprovalRules::decide`.
    pub const CYCLE: &[Mode] = &[Mode::ReadOnly, Mode::Edit, Mode::Plan];

    pub fn next(self) -> Mode {
        match Self::CYCLE.iter().position(|m| *m == self) {
            Some(i) => Self::CYCLE[(i + 1) % Self::CYCLE.len()],
            None => Self::CYCLE[0],
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Mode::ReadOnly => "read-only",
            Mode::Edit => "edit",
            Mode::Plan => "plan",
            Mode::Bypass => "bypass",
        }
    }
}

/// Shared, atomically updatable mode: the TUI switches it while the approval
/// hook (running in the worker task) reads it per tool call, so a switch
/// takes effect immediately, even mid-turn.
#[derive(Clone, Debug)]
pub struct ModeHandle(std::sync::Arc<std::sync::atomic::AtomicU8>);

impl ModeHandle {
    pub fn new(mode: Mode) -> Self {
        let handle = Self(Default::default());
        handle.set(mode);
        handle
    }

    pub fn get(&self) -> Mode {
        let i = self.0.load(std::sync::atomic::Ordering::Relaxed) as usize;
        *Mode::ALL.get(i).unwrap_or(&Mode::ReadOnly)
    }

    pub fn set(&self, mode: Mode) {
        let i = Mode::ALL.iter().position(|m| *m == mode).unwrap_or(0);
        self.0.store(i as u8, std::sync::atomic::Ordering::Relaxed);
    }
}

/// Shared, runtime-adjustable numeric setting: the `/config` dialog writes
/// it while the worker or a tool reads it per use, so a change applies to
/// the next prompt / tool call. Used for the turn limit, the bash timeout
/// and the read_file output limits.
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
}

/// Shared, runtime-editable web-search settings: the `/config` dialog
/// switches the provider / result count while the web_search tool takes a
/// snapshot per call.
#[derive(Clone, Debug)]
pub struct SearchHandle(std::sync::Arc<std::sync::RwLock<SearchConfig>>);

impl SearchHandle {
    pub fn new(cfg: SearchConfig) -> Self {
        Self(std::sync::Arc::new(std::sync::RwLock::new(cfg)))
    }

    /// Copy of the current settings (one consistent view per search call).
    pub fn snapshot(&self) -> SearchConfig {
        self.0.read().unwrap().clone()
    }

    pub fn set_max_results(&self, n: usize) {
        self.0.write().unwrap().max_results = n;
    }

    /// Providers usable right now: searxng needs a configured base_url and
    /// brave a BRAVE_API_KEY; duckduckgo always works.
    pub fn available_providers(&self) -> Vec<SearchProvider> {
        let cfg = self.0.read().unwrap();
        [
            SearchProvider::Duckduckgo,
            SearchProvider::Searxng,
            SearchProvider::Brave,
        ]
        .into_iter()
        .filter(|p| match p {
            SearchProvider::Duckduckgo => true,
            SearchProvider::Searxng => cfg.base_url.is_some(),
            SearchProvider::Brave => cfg.api_key.is_some(),
        })
        .collect()
    }

    /// Step to the previous/next usable provider (`/config` ←/→).
    pub fn cycle_provider(&self, delta: i64) {
        let choices = self.available_providers();
        let mut cfg = self.0.write().unwrap();
        let n = choices.len();
        let next = match choices.iter().position(|p| *p == cfg.provider) {
            Some(i) if delta < 0 => choices[(i + n - 1) % n],
            Some(i) => choices[(i + 1) % n],
            None => choices[0],
        };
        cfg.provider = next;
    }
}

/// Shared, runtime-extensible approval rules: the approval dialog's
/// "always allow" answer adds to them from the TUI while the approval hook
/// (running in the worker task) reads them per tool call, so an addition
/// takes effect immediately and survives model switches.
#[derive(Clone, Debug)]
pub struct RulesHandle(std::sync::Arc<std::sync::RwLock<ApprovalRules>>);

impl RulesHandle {
    pub fn new(rules: ApprovalRules) -> Self {
        Self(std::sync::Arc::new(std::sync::RwLock::new(rules)))
    }

    /// See [`ApprovalRules::decide`].
    pub fn decide(
        &self,
        mode: Mode,
        tool: &str,
        bash_command: Option<&str>,
        destructive: bool,
    ) -> Decision {
        self.0
            .read()
            .unwrap()
            .decide(mode, tool, bash_command, destructive)
    }

    /// Copy of the current rules (for `/permissions`).
    pub fn snapshot(&self) -> ApprovalRules {
        self.0.read().unwrap().clone()
    }

    /// Add a tool to `allow_tools` for the rest of the session.
    pub fn allow_tool(&self, tool: &str) {
        let mut rules = self.0.write().unwrap();
        if !rules.allow_tools.iter().any(|t| t == tool) {
            rules.allow_tools.push(tool.to_string());
        }
    }

    /// Add bash prefix patterns to `allow_bash` for the rest of the session.
    pub fn allow_bash(&self, patterns: &[String]) {
        let mut rules = self.0.write().unwrap();
        for p in patterns {
            if !rules.allow_bash.contains(p) {
                rules.allow_bash.push(p.clone());
            }
        }
    }
}

/// Auto-approval / auto-denial rules for tool calls.
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ApprovalRules {
    /// Tools that run without an approval prompt.
    #[serde(default)]
    pub allow_tools: Vec<String>,
    /// Tools that are always denied (wins over everything, in every mode).
    #[serde(default)]
    pub deny_tools: Vec<String>,
    /// Bash command prefixes that run without an approval prompt.
    #[serde(default)]
    pub allow_bash: Vec<String>,
    /// Bash command prefixes that are always denied.
    #[serde(default)]
    pub deny_bash: Vec<String>,
}

/// What the approval hook should do with a tool call.
#[derive(Debug, PartialEq, Eq)]
pub enum Decision {
    /// Run without asking.
    Allow,
    /// Ask the user (the normal approval prompt).
    Ask,
    /// Deny without asking; the string is the reason returned to the model.
    Deny(String),
}

impl ApprovalRules {
    /// Decide what to do with a tool call. `bash_command` is the command
    /// string when the call is the bash tool, `destructive` whether the tool
    /// requires approval by default (state changes or network access).
    ///
    /// The rules are absolute and the mode fills in the default:
    /// 1. deny rules always deny, in every mode;
    /// 2. local read tools and dialogs always run;
    /// 3. bypass mode runs everything else;
    /// 4. plan mode denies the mutating tools (bash and file writes);
    /// 5. allow rules always allow;
    /// 6. edit mode additionally allows file writes (project-confined);
    /// 7. whatever is left asks the user.
    pub fn decide(
        &self,
        mode: Mode,
        tool: &str,
        bash_command: Option<&str>,
        destructive: bool,
    ) -> Decision {
        let in_list = |list: &[String]| list.iter().any(|t| t == tool);
        if in_list(&self.deny_tools) {
            return Decision::Deny(deny_reason(tool, "deny_tools"));
        }
        let segments = bash_command.map(split_segments);
        if let Some(segments) = &segments
            && segments
                .iter()
                .any(|seg| self.deny_bash.iter().any(|p| pattern_matches(p, seg)))
        {
            return Decision::Deny(deny_reason(tool, "deny_bash"));
        }

        if !destructive {
            return Decision::Allow;
        }
        if mode == Mode::Bypass {
            return Decision::Allow;
        }
        // Plan mode blocks anything that could change the system; web tools
        // stay available for research under the usual allow/ask rules.
        if mode == Mode::Plan && crate::tools::MUTATING_TOOLS.contains(&tool) {
            return Decision::Deny(
                "picocode is in plan mode: writes and commands are blocked. Continue \
                 investigating with the read-only tools and present a concise \
                 implementation plan; the user will switch to edit mode to execute it. \
                 Do not retry this call."
                    .to_string(),
            );
        }
        if in_list(&self.allow_tools) {
            return Decision::Allow;
        }
        if let (Some(cmd), Some(segments)) = (bash_command, &segments) {
            // Command substitution and output redirection can smuggle
            // effects past a prefix whitelist, so they never auto-run.
            let risky = cmd.contains("$(") || cmd.contains('`') || cmd.contains('>');
            if !risky
                && !segments.is_empty()
                && segments
                    .iter()
                    .all(|seg| self.allow_bash.iter().any(|p| pattern_matches(p, seg)))
            {
                return Decision::Allow;
            }
        }
        if mode == Mode::Edit && crate::tools::WRITE_TOOLS.contains(&tool) {
            return Decision::Allow;
        }
        Decision::Ask
    }
}

fn deny_reason(tool: &str, list: &str) -> String {
    format!(
        "This `{tool}` call was automatically denied by the picocode config ({list}). \
         Do not retry it; explain what you wanted to do or take a different approach."
    )
}

/// Split a shell command into segments at `&&`, `||`, `;`, `|`, `&` and
/// newlines. Quotes are not interpreted; that only makes matching more
/// conservative (a quoted operator yields an extra segment that simply won't
/// match an allow pattern).
fn split_segments(cmd: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut chars = cmd.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '&' | '|' => {
                if chars.peek() == Some(&c) {
                    chars.next();
                }
                out.push(std::mem::take(&mut cur));
            }
            ';' | '\n' => out.push(std::mem::take(&mut cur)),
            _ => cur.push(c),
        }
    }
    out.push(cur);
    out.iter()
        .map(|s| {
            s.trim()
                .trim_start_matches('(')
                .trim_end_matches(')')
                .trim()
                .to_string()
        })
        .filter(|s| !s.is_empty())
        .collect()
}

/// The `allow_bash` patterns an "always allow" answer adds for a command:
/// each segment's program name, plus the subcommand word when there is one
/// (`cargo build --release` → `cargo build`, `ls -la` → `ls`). The dialog
/// shows the result, so the user sees exactly what gets whitelisted.
pub fn bash_allow_patterns(cmd: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for seg in split_segments(cmd) {
        let mut words = seg.split_whitespace();
        let Some(first) = words.next() else { continue };
        // A subcommand is a plain word (`build`, `status`), not a flag,
        // number or path.
        let sub = words.next().filter(|w| {
            w.starts_with(|c: char| c.is_ascii_alphabetic())
                && w.chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
        });
        let pat = match sub {
            Some(sub) => format!("{first} {sub}"),
            None => first.to_string(),
        };
        if !out.contains(&pat) {
            out.push(pat);
        }
    }
    out
}

/// Word-boundary prefix match: pattern `cargo` matches `cargo build` but not
/// `cargofoo`; `git status` matches `git status -s`. A trailing `*` in the
/// pattern is tolerated (Claude-Code-style `cargo *`) and stripped.
fn pattern_matches(pattern: &str, segment: &str) -> bool {
    let pat = pattern.trim().trim_end_matches('*').trim_end();
    if pat.is_empty() {
        return false;
    }
    match segment.strip_prefix(pat) {
        Some(rest) => rest.is_empty() || rest.starts_with(char::is_whitespace),
        None => false,
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
    /// Turn limit per prompt, shared with the worker and adjustable at
    /// runtime (`/config`).
    pub max_turns: NumHandle,
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
            max_turns: NumHandle::new(args.max_turns as u64),
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

#[cfg(test)]
mod tests {
    use super::*;

    fn rules(
        allow_tools: &[&str],
        deny_tools: &[&str],
        allow_bash: &[&str],
        deny_bash: &[&str],
    ) -> ApprovalRules {
        let v = |xs: &[&str]| xs.iter().map(|s| s.to_string()).collect();
        ApprovalRules {
            allow_tools: v(allow_tools),
            deny_tools: v(deny_tools),
            allow_bash: v(allow_bash),
            deny_bash: v(deny_bash),
        }
    }

    #[test]
    fn segments_split_on_shell_operators() {
        assert_eq!(split_segments("cargo build"), vec!["cargo build"]);
        assert_eq!(
            split_segments("cargo build && cargo test; ls | wc -l"),
            vec!["cargo build", "cargo test", "ls", "wc -l"]
        );
        assert_eq!(split_segments("(cd /tmp && ls)"), vec!["cd /tmp", "ls"]);
        assert_eq!(split_segments("a\nb"), vec!["a", "b"]);
    }

    #[test]
    fn always_allow_patterns_take_program_and_subcommand() {
        assert_eq!(
            bash_allow_patterns("cargo build --release"),
            ["cargo build"]
        );
        assert_eq!(bash_allow_patterns("ls -la"), ["ls"]);
        assert_eq!(
            bash_allow_patterns("git status && git diff | head"),
            ["git status", "git diff", "head"]
        );
        assert_eq!(bash_allow_patterns("python script.py"), ["python"]);
        assert_eq!(bash_allow_patterns("sleep 2"), ["sleep"]);
        assert_eq!(
            bash_allow_patterns("cargo test && cargo test"),
            ["cargo test"]
        );
        assert!(bash_allow_patterns("").is_empty());
    }

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
    fn search_handle_switches_between_available_providers() {
        let handle = SearchHandle::new(SearchConfig {
            provider: SearchProvider::Duckduckgo,
            base_url: None,
            max_results: 5,
            api_key: None,
        });
        // Neither searxng (no base_url) nor brave (no key) is available.
        assert_eq!(handle.available_providers(), [SearchProvider::Duckduckgo]);
        handle.cycle_provider(1);
        assert_eq!(handle.snapshot().provider, SearchProvider::Duckduckgo);

        // The tool side sees runtime changes.
        let tool_side = handle.clone();
        handle.set_max_results(9);
        assert_eq!(tool_side.snapshot().max_results, 9);

        // With a key, brave joins the cycle.
        let handle = SearchHandle::new(SearchConfig {
            provider: SearchProvider::Duckduckgo,
            base_url: None,
            max_results: 5,
            api_key: Some("k".into()),
        });
        assert_eq!(
            handle.available_providers(),
            [SearchProvider::Duckduckgo, SearchProvider::Brave]
        );
        handle.cycle_provider(1);
        assert_eq!(handle.snapshot().provider, SearchProvider::Brave);
        handle.cycle_provider(1);
        assert_eq!(handle.snapshot().provider, SearchProvider::Duckduckgo);
        handle.cycle_provider(-1);
        assert_eq!(handle.snapshot().provider, SearchProvider::Brave);
    }

    #[test]
    fn rules_handle_shares_runtime_additions() {
        let handle = RulesHandle::new(ApprovalRules::default());
        let hook_side = handle.clone();

        let ask = handle.decide(Mode::ReadOnly, "bash", Some("cargo build"), true);
        assert_eq!(ask, Decision::Ask);
        handle.allow_bash(&["cargo build".to_string()]);
        assert_eq!(
            hook_side.decide(Mode::ReadOnly, "bash", Some("cargo build --release"), true),
            Decision::Allow
        );

        assert_eq!(
            hook_side.decide(Mode::ReadOnly, "web_search", None, true),
            Decision::Ask
        );
        handle.allow_tool("web_search");
        assert_eq!(
            hook_side.decide(Mode::ReadOnly, "web_search", None, true),
            Decision::Allow
        );

        // Duplicates are not stacked.
        handle.allow_tool("web_search");
        handle.allow_bash(&["cargo build".to_string()]);
        let snap = hook_side.snapshot();
        assert_eq!(snap.allow_tools, ["web_search"]);
        assert_eq!(snap.allow_bash, ["cargo build"]);
    }

    #[test]
    fn patterns_match_on_word_boundaries() {
        assert!(pattern_matches("cargo", "cargo build"));
        assert!(pattern_matches("cargo", "cargo"));
        assert!(pattern_matches("cargo *", "cargo test"));
        assert!(pattern_matches("git status", "git status -s"));
        assert!(!pattern_matches("cargo", "cargofoo"));
        assert!(!pattern_matches("git status", "git stash"));
        assert!(!pattern_matches("", "anything"));
    }

    #[test]
    fn deny_rules_win_in_every_mode() {
        let r = rules(&["web_fetch"], &["web_fetch"], &["rm"], &["rm"]);
        for mode in Mode::ALL {
            assert!(matches!(
                r.decide(*mode, "web_fetch", None, true),
                Decision::Deny(_)
            ));
            assert!(matches!(
                r.decide(*mode, "bash", Some("echo hi && rm -rf x"), true),
                Decision::Deny(_)
            ));
        }
    }

    #[test]
    fn allow_rules_work_in_read_only_and_edit() {
        // allow_tools and allow_bash always allow (outside plan's denials).
        let r = rules(&["web_search"], &[], &["cargo", "ls"], &[]);
        for mode in [Mode::ReadOnly, Mode::Edit] {
            assert_eq!(r.decide(mode, "web_search", None, true), Decision::Allow);
            assert_eq!(
                r.decide(mode, "bash", Some("cargo build && ls -la"), true),
                Decision::Allow
            );
        }
        // A segment outside the list still asks.
        assert_eq!(
            r.decide(Mode::Edit, "bash", Some("cargo build && curl x"), true),
            Decision::Ask
        );
        // Unlisted destructive calls ask in both modes.
        let none = ApprovalRules::default();
        assert_eq!(
            none.decide(Mode::ReadOnly, "bash", Some("ls"), true),
            Decision::Ask
        );
        assert_eq!(
            none.decide(Mode::ReadOnly, "web_fetch", None, true),
            Decision::Ask
        );
    }

    #[test]
    fn risky_shell_syntax_never_auto_runs() {
        let r = rules(&[], &[], &["echo", "cargo"], &[]);
        for cmd in [
            "echo $(rm -rf /)",
            "echo `date`",
            "echo hi > ~/.zshrc",
            "cargo build 2>err.txt",
        ] {
            assert_eq!(
                r.decide(Mode::Edit, "bash", Some(cmd), true),
                Decision::Ask,
                "{cmd} must not auto-run"
            );
        }
        // Env-var prefixes don't prefix-match either (PATH=… could hijack).
        assert_eq!(
            r.decide(Mode::Edit, "bash", Some("PATH=/evil cargo build"), true),
            Decision::Ask
        );
    }

    #[test]
    fn local_reads_always_run() {
        let r = ApprovalRules::default();
        for mode in Mode::ALL {
            assert_eq!(r.decide(*mode, "read_file", None, false), Decision::Allow);
        }
    }

    #[test]
    fn edit_mode_allows_file_writes_but_not_bash() {
        let r = ApprovalRules::default();
        assert_eq!(
            r.decide(Mode::Edit, "write_file", None, true),
            Decision::Allow
        );
        assert_eq!(
            r.decide(Mode::Edit, "edit_file", None, true),
            Decision::Allow
        );
        assert_eq!(
            r.decide(Mode::Edit, "bash", Some("ls"), true),
            Decision::Ask
        );
        // In read-only the same writes ask.
        assert_eq!(
            r.decide(Mode::ReadOnly, "write_file", None, true),
            Decision::Ask
        );
    }

    #[test]
    fn modes_cycle_and_share_state() {
        assert_eq!(Mode::default(), Mode::ReadOnly);
        assert_eq!(Mode::ReadOnly.next(), Mode::Edit);
        assert_eq!(Mode::Edit.next(), Mode::Plan);
        assert_eq!(Mode::Plan.next(), Mode::ReadOnly);
        assert_eq!(Mode::ReadOnly.label(), "read-only");
        // Bypass is not in the Shift+Tab cycle: never entered by next(),
        // and leaving it lands on read-only.
        assert!(!Mode::CYCLE.contains(&Mode::Bypass));
        assert_eq!(Mode::Bypass.next(), Mode::ReadOnly);

        let handle = ModeHandle::new(Mode::ReadOnly);
        let clone = handle.clone();
        handle.set(handle.get().next());
        assert_eq!(clone.get(), Mode::Edit);
        // The handle can still store bypass (set via the /bypass command).
        handle.set(Mode::Bypass);
        assert_eq!(clone.get(), Mode::Bypass);
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
    fn bypass_mode_allows_everything_except_deny_rules() {
        let r = ApprovalRules::default();
        assert_eq!(
            r.decide(Mode::Bypass, "write_file", None, true),
            Decision::Allow
        );
        assert_eq!(
            r.decide(Mode::Bypass, "bash", Some("rm -rf build"), true),
            Decision::Allow
        );
        assert_eq!(
            r.decide(Mode::Bypass, "web_fetch", None, true),
            Decision::Allow
        );
    }

    #[test]
    fn plan_mode_denies_mutations_but_not_research() {
        // Even allow-listed writes/commands are denied with a plan-mode reason.
        let r = rules(&["write_file"], &[], &["cargo"], &[]);
        for (tool, cmd) in [
            ("write_file", None),
            ("edit_file", None),
            ("bash", Some("cargo build")),
        ] {
            match r.decide(Mode::Plan, tool, cmd, true) {
                Decision::Deny(reason) => assert!(reason.contains("plan mode")),
                other => panic!("expected Deny, got {other:?}"),
            }
        }
        // Reads still run; web research follows the usual allow/ask rules.
        assert_eq!(
            r.decide(Mode::Plan, "read_file", None, false),
            Decision::Allow
        );
        assert_eq!(r.decide(Mode::Plan, "web_fetch", None, true), Decision::Ask);
        let w = rules(&["web_search"], &[], &[], &[]);
        assert_eq!(
            r.decide(Mode::Plan, "web_search", None, true),
            Decision::Ask
        );
        assert_eq!(
            w.decide(Mode::Plan, "web_search", None, true),
            Decision::Allow
        );
    }

    #[test]
    fn approval_tool_lists_reject_unknown_names() {
        assert!(validate_tool_lists(&rules(&["web_search"], &["bash"], &[], &[])).is_ok());
        let err = validate_tool_lists(&rules(&["web-fetch"], &[], &[], &[]))
            .unwrap_err()
            .to_string();
        assert!(err.contains("web-fetch"));
        assert!(err.contains("web_fetch"));
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
    fn search_section_parses_merges_and_validates() {
        let global: FileConfig = toml::from_str(
            r#"
            [search]
            provider = "duckduckgo"
            max_results = 3
            "#,
        )
        .unwrap();
        let project: FileConfig = toml::from_str(
            r#"
            [search]
            provider = "searxng"
            base_url = "http://localhost:8888"
            "#,
        )
        .unwrap();
        let merged = merge(global, project);
        assert_eq!(merged.search.provider, Some(SearchProvider::Searxng));
        assert_eq!(merged.search.max_results, Some(3));

        let resolved = resolve_search(merged.search, None).unwrap();
        assert_eq!(resolved.provider, SearchProvider::Searxng);
        assert_eq!(resolved.base_url.as_deref(), Some("http://localhost:8888"));
        assert_eq!(resolved.max_results, 3);

        // Defaults: duckduckgo, 5 results.
        let default = resolve_search(SearchFileConfig::default(), None).unwrap();
        assert_eq!(default.provider, SearchProvider::Duckduckgo);
        assert_eq!(default.max_results, 5);

        // searxng without base_url is a config error.
        let bad: FileConfig = toml::from_str("[search]\nprovider = \"searxng\"").unwrap();
        assert!(resolve_search(bad.search, None).is_err());

        // brave requires an API key from the environment.
        let brave: FileConfig = toml::from_str("[search]\nprovider = \"brave\"").unwrap();
        assert!(resolve_search(brave.search.clone(), None).is_err());
        let ok = resolve_search(brave.search, Some("k".into())).unwrap();
        assert_eq!(ok.api_key.as_deref(), Some("k"));

        assert!(toml::from_str::<FileConfig>("[search]\nproviderr = \"x\"").is_err());
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
