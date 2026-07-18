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
    provider: Option<Provider>,
    model: Option<String>,
    /// Instruction files loaded into the system prompt when present.
    instructions: Option<Vec<String>>,
    #[serde(default)]
    approval: ApprovalRules,
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
    /// approval by default. Precedence: deny rules > --yolo > allow rules > ask.
    pub fn decide(&self, yolo: bool, tool: &str, bash_command: Option<&str>, destructive: bool) -> Decision {
        let in_list = |list: &[String]| list.iter().any(|t| t == tool);
        if in_list(&self.deny_tools) {
            return Decision::Deny(deny_reason(tool, "deny_tools"));
        }

        if let Some(cmd) = bash_command {
            let segments = split_segments(cmd);
            if segments
                .iter()
                .any(|seg| self.deny_bash.iter().any(|p| pattern_matches(p, seg)))
            {
                return Decision::Deny(deny_reason(tool, "deny_bash"));
            }
            if yolo || in_list(&self.allow_tools) {
                return Decision::Allow;
            }
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
            return if destructive { Decision::Ask } else { Decision::Allow };
        }

        if yolo || in_list(&self.allow_tools) {
            return Decision::Allow;
        }
        if destructive { Decision::Ask } else { Decision::Allow }
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
        provider: project.provider.or(global.provider),
        model: project.model.or(global.model),
        instructions: project.instructions.or(global.instructions),
        approval,
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
    pub yolo: bool,
    pub max_turns: usize,
    /// Working directory the tools operate in.
    pub root: PathBuf,
    pub approval: ApprovalRules,
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

        let provider = args.provider.or(file.provider).unwrap_or(Provider::Ollama);
        let model = args.model.or(file.model).unwrap_or_else(|| {
            match provider {
                Provider::Ollama => "qwen3:4b",
                Provider::Anthropic => "claude-opus-4-8",
                Provider::Openai => "gpt-4o",
            }
            .to_string()
        });
        let instruction_names = file.instructions.unwrap_or_else(|| vec!["AGENTS.md".to_string()]);
        let instructions = load_instructions(&root, &instruction_names);

        Ok(Self {
            provider,
            model,
            yolo: args.yolo,
            max_turns: args.max_turns,
            root,
            approval: file.approval,
            instructions,
            config_files,
        })
    }

    pub fn model_label(&self) -> String {
        let provider = match self.provider {
            Provider::Ollama => "ollama",
            Provider::Anthropic => "anthropic",
            Provider::Openai => "openai",
        };
        format!("{provider}/{}", self.model)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rules(allow_tools: &[&str], deny_tools: &[&str], allow_bash: &[&str], deny_bash: &[&str]) -> ApprovalRules {
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
        assert!(matches!(r.decide(true, "web_fetch", None, false), Decision::Deny(_)));
    }

    #[test]
    fn deny_bash_wins_over_yolo_and_allow() {
        let r = rules(&[], &[], &["rm"], &["rm"]);
        assert!(matches!(
            r.decide(true, "bash", Some("echo hi && rm -rf x"), true),
            Decision::Deny(_)
        ));
    }

    #[test]
    fn allow_bash_requires_every_segment_to_match() {
        let r = rules(&[], &[], &["cargo", "ls"], &[]);
        assert_eq!(r.decide(false, "bash", Some("cargo build && ls -la"), true), Decision::Allow);
        assert_eq!(r.decide(false, "bash", Some("cargo build && curl x"), true), Decision::Ask);
    }

    #[test]
    fn command_substitution_never_auto_runs() {
        let r = rules(&[], &[], &["echo"], &[]);
        assert_eq!(r.decide(false, "bash", Some("echo $(rm -rf /)"), true), Decision::Ask);
        assert_eq!(r.decide(false, "bash", Some("echo `date`"), true), Decision::Ask);
    }

    #[test]
    fn allow_tools_skips_prompt_for_destructive_tool() {
        let r = rules(&["write_file"], &[], &[], &[]);
        assert_eq!(r.decide(false, "write_file", None, true), Decision::Allow);
        assert_eq!(r.decide(false, "edit_file", None, true), Decision::Ask);
    }

    #[test]
    fn read_only_tools_run_without_rules() {
        let r = ApprovalRules::default();
        assert_eq!(r.decide(false, "read_file", None, false), Decision::Allow);
        assert_eq!(r.decide(false, "bash", Some("ls"), true), Decision::Ask);
    }

    #[test]
    fn config_files_parse_and_merge() {
        let global: FileConfig = toml::from_str(
            r#"
            model = "qwen3:8b"
            [approval]
            allow_bash = ["ls"]
            "#,
        )
        .unwrap();
        let project: FileConfig = toml::from_str(
            r#"
            model = "qwen3:4b"
            instructions = ["AGENTS.md", "STYLE.md"]
            [approval]
            allow_bash = ["cargo"]
            deny_bash = ["sudo"]
            "#,
        )
        .unwrap();
        let merged = merge(global, project);
        assert_eq!(merged.model.as_deref(), Some("qwen3:4b"));
        assert_eq!(merged.approval.allow_bash, vec!["ls", "cargo"]);
        assert_eq!(merged.approval.deny_bash, vec!["sudo"]);
        assert_eq!(merged.instructions.unwrap(), vec!["AGENTS.md", "STYLE.md"]);
    }

    #[test]
    fn unknown_config_keys_are_rejected() {
        assert!(toml::from_str::<FileConfig>("allow_bash = []").is_err());
        assert!(toml::from_str::<FileConfig>("[approval]\nallowbash = []").is_err());
    }

    #[test]
    fn instructions_load_existing_files_only() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("AGENTS.md"), "be nice").unwrap();
        let loaded = load_instructions(dir.path(), &["AGENTS.md".into(), "MISSING.md".into()]);
        assert_eq!(loaded, vec![("AGENTS.md".to_string(), "be nice".to_string())]);
    }
}
