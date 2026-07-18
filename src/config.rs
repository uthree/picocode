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

    /// Skip all tool-approval prompts (dangerous). Config deny rules still apply.
    #[arg(long)]
    pub yolo: bool,

    /// Maximum model turns (tool-call rounds) per user prompt.
    #[arg(long, default_value_t = 50)]
    pub max_turns: usize,

    /// Headless mode for debugging: run one prompt without the TUI and print
    /// events to stdout. Implies --yolo.
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
    #[serde(default)]
    approval: ApprovalRules,
    #[serde(default)]
    search: SearchFileConfig,
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
    let api_key = match provider {
        SearchProvider::Brave => Some(brave_key.ok_or_else(|| {
            anyhow::anyhow!(
                "[search] provider \"brave\" requires the BRAVE_API_KEY environment variable"
            )
        })?),
        _ => None,
    };
    Ok(SearchConfig {
        provider,
        base_url: file.base_url,
        max_results: file.max_results.unwrap_or(5).clamp(1, 20),
        api_key,
    })
}

// ----- permission modes ------------------------------------------------------

/// Permission mode, cycled with Shift+Tab in the TUI. Deny rules and --yolo
/// take precedence over the mode; see [`ApprovalRules::decide`].
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Mode {
    /// Reads run freely; every write or command asks, even if allow-listed.
    #[default]
    ReadOnly,
    /// File writes run freely; commands and other tools follow the config
    /// allow/deny rules.
    Edit,
}

impl Mode {
    /// Cycle order for Shift+Tab. Future modes only need a variant, an entry
    /// here, a label, and their branch in `ApprovalRules::decide`.
    pub const ALL: &[Mode] = &[Mode::ReadOnly, Mode::Edit];

    pub fn next(self) -> Mode {
        let i = Self::ALL.iter().position(|m| *m == self).unwrap_or(0);
        Self::ALL[(i + 1) % Self::ALL.len()]
    }

    pub fn label(self) -> &'static str {
        match self {
            Mode::ReadOnly => "read-only",
            Mode::Edit => "edit",
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

/// Auto-approval / auto-denial rules for tool calls.
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ApprovalRules {
    /// Tools that run without an approval prompt.
    #[serde(default)]
    pub allow_tools: Vec<String>,
    /// Tools that are always denied (wins over everything, including --yolo).
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
    /// Decide what to do with a tool call. `bash_command` is the command string
    /// when the call is the bash tool, `destructive` whether the tool requires
    /// approval by default.
    /// Precedence: deny rules > --yolo > mode > allow rules > ask.
    pub fn decide(
        &self,
        yolo: bool,
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
        if yolo {
            return Decision::Allow;
        }
        // Read-only (the default) confirms every destructive call, allow
        // rules notwithstanding.
        if mode == Mode::ReadOnly {
            return Decision::Ask;
        }

        // Edit mode: file writes are permitted outright; commands and other
        // tools follow the config allow rules.
        if crate::tools::WRITE_TOOLS.contains(&tool) || in_list(&self.allow_tools) {
            return Decision::Allow;
        }
        if let (Some(cmd), Some(segments)) = (bash_command, &segments) {
            // Command substitution can smuggle arbitrary commands past a
            // prefix whitelist, so it never auto-runs.
            let has_substitution = cmd.contains("$(") || cmd.contains('`');
            if !has_substitution
                && !segments.is_empty()
                && segments
                    .iter()
                    .all(|seg| self.allow_bash.iter().any(|p| pattern_matches(p, seg)))
            {
                return Decision::Allow;
            }
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
        .map(|s| s.trim().trim_start_matches('(').trim_start().to_string())
        .filter(|s| !s.is_empty())
        .collect()
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

fn global_config_path() -> Option<PathBuf> {
    if let Some(xdg) = std::env::var_os("XDG_CONFIG_HOME") {
        return Some(PathBuf::from(xdg).join("picocode/config.toml"));
    }
    std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config/picocode/config.toml"))
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
    FileConfig {
        default_model: project.default_model.or(global.default_model),
        models: project.models.or(global.models),
        instructions: project.instructions.or(global.instructions),
        system_prompt: project.system_prompt.or(global.system_prompt),
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
    pub yolo: bool,
    pub max_turns: usize,
    /// Working directory the tools operate in.
    pub root: PathBuf,
    pub approval: ApprovalRules,
    /// Current permission mode, shared with the approval hook.
    pub mode: ModeHandle,
    pub search: SearchConfig,
    /// Base system prompt override from the config file (None = built-in).
    pub system_prompt: Option<String>,
    /// Instruction files that were found: (file name, content).
    pub instructions: Vec<(String, String)>,
    /// Config files that were loaded, for the startup notice.
    pub config_files: Vec<String>,
}

impl Config {
    pub fn from_args(args: Args) -> anyhow::Result<Self> {
        let root = std::env::current_dir()?;

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

        // CLI flags select an ad-hoc model and take precedence over the
        // config file's [[models]]; the entries stay available to /model.
        let cli_selection =
            args.provider.is_some() || args.model.is_some() || args.base_url.is_some();
        let (provider, model, base_url, active_model) =
            match pick_entry(&models, file.default_model.as_deref()) {
                Some(entry) if !cli_selection => (
                    entry.provider,
                    entry.model.clone(),
                    entry.base_url.clone(),
                    Some(entry.name.clone()),
                ),
                _ => {
                    let provider = args.provider.unwrap_or(Provider::Ollama);
                    let model = args.model.unwrap_or_else(|| {
                        match provider {
                            Provider::Ollama => "qwen3:4b",
                            Provider::Anthropic => "claude-opus-4-8",
                            Provider::Openai => "gpt-4o",
                        }
                        .to_string()
                    });
                    (provider, model, args.base_url, None)
                }
            };

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
            yolo: args.yolo,
            max_turns: args.max_turns,
            root,
            approval: file.approval,
            mode: ModeHandle::new(Mode::default()),
            search,
            system_prompt: file.system_prompt,
            instructions,
            config_files,
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
        assert_eq!(split_segments("(cd /tmp && ls)"), vec!["cd /tmp", "ls)"]);
        assert_eq!(split_segments("a\nb"), vec!["a", "b"]);
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
    fn deny_tools_wins_over_everything() {
        let r = rules(&["web_fetch"], &["web_fetch"], &[], &[]);
        assert!(matches!(
            r.decide(true, Mode::Edit, "web_fetch", None, false),
            Decision::Deny(_)
        ));
    }

    #[test]
    fn deny_bash_wins_over_yolo_and_allow() {
        let r = rules(&[], &[], &["rm"], &["rm"]);
        assert!(matches!(
            r.decide(true, Mode::Edit, "bash", Some("echo hi && rm -rf x"), true),
            Decision::Deny(_)
        ));
    }

    #[test]
    fn allow_bash_requires_every_segment_to_match() {
        let r = rules(&[], &[], &["cargo", "ls"], &[]);
        assert_eq!(
            r.decide(
                false,
                Mode::Edit,
                "bash",
                Some("cargo build && ls -la"),
                true
            ),
            Decision::Allow
        );
        assert_eq!(
            r.decide(
                false,
                Mode::Edit,
                "bash",
                Some("cargo build && curl x"),
                true
            ),
            Decision::Ask
        );
    }

    #[test]
    fn command_substitution_never_auto_runs() {
        let r = rules(&[], &[], &["echo"], &[]);
        assert_eq!(
            r.decide(false, Mode::Edit, "bash", Some("echo $(rm -rf /)"), true),
            Decision::Ask
        );
        assert_eq!(
            r.decide(false, Mode::Edit, "bash", Some("echo `date`"), true),
            Decision::Ask
        );
    }

    #[test]
    fn allow_tools_skips_prompt_for_destructive_tool() {
        let r = rules(&["bash"], &[], &[], &[]);
        assert_eq!(
            r.decide(false, Mode::Edit, "bash", Some("rm -rf x"), true),
            Decision::Allow
        );
        let none = ApprovalRules::default();
        assert_eq!(
            none.decide(false, Mode::Edit, "bash", Some("rm -rf x"), true),
            Decision::Ask
        );
    }

    #[test]
    fn read_only_tools_run_without_rules() {
        let r = ApprovalRules::default();
        assert_eq!(
            r.decide(false, Mode::Edit, "read_file", None, false),
            Decision::Allow
        );
        assert_eq!(
            r.decide(false, Mode::Edit, "bash", Some("ls"), true),
            Decision::Ask
        );
    }

    #[test]
    fn read_only_mode_always_asks_for_destructive() {
        // Allow rules are ignored: every write or command still asks.
        let r = rules(&["write_file"], &[], &["cargo"], &[]);
        assert_eq!(
            r.decide(false, Mode::ReadOnly, "write_file", None, true),
            Decision::Ask
        );
        assert_eq!(
            r.decide(false, Mode::ReadOnly, "bash", Some("cargo build"), true),
            Decision::Ask
        );
        // Reads still run, --yolo still skips, deny still wins.
        assert_eq!(
            r.decide(false, Mode::ReadOnly, "read_file", None, false),
            Decision::Allow
        );
        assert_eq!(
            r.decide(true, Mode::ReadOnly, "write_file", None, true),
            Decision::Allow
        );
        let d = rules(&[], &["write_file"], &[], &[]);
        assert!(matches!(
            d.decide(false, Mode::ReadOnly, "write_file", None, true),
            Decision::Deny(_)
        ));
    }

    #[test]
    fn edit_mode_allows_file_writes_but_not_bash() {
        let r = ApprovalRules::default();
        assert_eq!(
            r.decide(false, Mode::Edit, "write_file", None, true),
            Decision::Allow
        );
        assert_eq!(
            r.decide(false, Mode::Edit, "edit_file", None, true),
            Decision::Allow
        );
        assert_eq!(
            r.decide(false, Mode::Edit, "bash", Some("ls"), true),
            Decision::Ask
        );
    }

    #[test]
    fn modes_cycle_and_share_state() {
        assert_eq!(Mode::default(), Mode::ReadOnly);
        assert_eq!(Mode::ReadOnly.next(), Mode::Edit);
        assert_eq!(Mode::Edit.next(), Mode::ReadOnly);
        assert_eq!(Mode::ReadOnly.label(), "read-only");

        let handle = ModeHandle::new(Mode::ReadOnly);
        let clone = handle.clone();
        handle.set(handle.get().next());
        assert_eq!(clone.get(), Mode::Edit);
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
        // Project [[models]] replace the global list wholesale.
        let models = merged.models.unwrap();
        assert_eq!(models.len(), 2);
        assert_eq!(models[1].base_url.as_deref(), Some("http://host:8000/v1"));
        assert_eq!(merged.default_model.as_deref(), Some("global"));
        assert_eq!(merged.approval.allow_bash, vec!["ls", "cargo"]);
        assert_eq!(merged.approval.deny_bash, vec!["sudo"]);
        assert_eq!(merged.instructions.unwrap(), vec!["AGENTS.md", "STYLE.md"]);
        assert_eq!(
            merged.system_prompt.as_deref(),
            Some("You are a project bot in {root}.")
        );
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
