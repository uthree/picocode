//! Shared slash-command parsing for both front ends.
//!
//! The TUI and GUI used to interpret `/commands` with separate hand-written
//! matches that drifted apart (aliases, unknown-command handling, whether a
//! typo'd command leaked to the model as a prompt). This module is the
//! single source of truth: one parser, one alias table, one command list
//! for the completion popups. The front ends only *execute* the parsed
//! command — how a dialog looks stays UI-specific.
//!
//! Rules:
//! - Multi-line input is never a command (it is a prompt, even if it
//!   starts with `/`).
//! - A single-line input starting with `/` is always treated as a command:
//!   unknown names are an error, never sent to the model.
//! - `!` direct-shell input is not handled here (the front ends check for
//!   it before parsing).

use crate::config::Mode;

/// What the `/jobs` command should do.
#[derive(Debug, PartialEq, Eq)]
pub enum JobsAction {
    List,
    Kill(u64),
}

/// What the `/prompt` command should do.
#[derive(Debug, PartialEq, Eq)]
pub enum PromptAction {
    /// `/prompt`: open the editor.
    Edit,
    /// `/prompt reset`: back to the built-in default.
    Reset,
    /// `/prompt <name>`: switch to a `[[prompts]]` preset (the front end
    /// resolves the name against the config).
    Preset(String),
}

/// A parsed slash command, ready for the front end to execute.
#[derive(Debug, PartialEq, Eq)]
pub enum Command {
    Clear,
    Compact,
    Undo,
    Jobs(JobsAction),
    /// `/model` (dialog) or `/model <name>` (direct switch).
    Model(Option<String>),
    /// `/resume` (dialog) or `/resume <id>` (direct).
    Resume(Option<String>),
    /// `/attach` (list), `/attach clear`, or `/attach <path>` — the front
    /// end interprets the argument.
    Attach(Option<String>),
    /// `/prompt` and its argument forms (see [`PromptAction`]).
    SystemPrompt(PromptAction),
    /// `/remote` (show the workspace and the configured remotes) or
    /// `/remote <name|host:/path|local>` (switch workspace).
    Remote(Option<String>),
    Mode(Mode),
    Permissions,
    Config,
    Status,
    Quit,
}

/// Outcome of looking at one submitted input line.
#[derive(Debug, PartialEq, Eq)]
pub enum ParseOutcome {
    /// Not a command: send it to the model as a prompt.
    Prompt,
    Command(Command),
    /// `/name` is not a known command (never sent to the model).
    Unknown {
        name: String,
    },
    /// A known command with a malformed argument.
    Invalid {
        message: String,
    },
}

/// One row of the command listing (completion popups, docs). Aliases parse
/// but are not listed.
pub struct CommandSpec {
    pub name: &'static str,
    /// English description (the TUI shows it directly; the GUI derives its
    /// locale key from `name`).
    pub description: &'static str,
}

/// The visible command set, in completion-popup order. Kept in sync with
/// [`parse`] by the `every_listed_command_parses` test.
pub const COMMANDS: &[CommandSpec] = &[
    CommandSpec {
        name: "/attach",
        description: "Attach a file to the next prompt: /attach <path> (clear unstages)",
    },
    CommandSpec {
        name: "/clear",
        description: "Clear conversation history",
    },
    CommandSpec {
        name: "/compact",
        description: "Summarize history to free context",
    },
    CommandSpec {
        name: "/undo",
        description: "Revert the last turn's file edits (repeatable)",
    },
    CommandSpec {
        name: "/jobs",
        description: "List background jobs; /jobs kill <id> stops one",
    },
    CommandSpec {
        name: "/model",
        description: "Pick a model (dialog) or switch: /model <name>",
    },
    CommandSpec {
        name: "/prompt",
        description: "Edit the system prompt; /prompt <preset> switches, /prompt reset restores",
    },
    CommandSpec {
        name: "/remote",
        description: "Show the workspace; /remote <name|host:/path> switches, /remote local returns",
    },
    CommandSpec {
        name: "/resume",
        description: "Pick a saved session to resume",
    },
    CommandSpec {
        name: "/read-only",
        description: "Mode: reads only, every write asks",
    },
    CommandSpec {
        name: "/edit",
        description: "Mode: file writes run freely",
    },
    CommandSpec {
        name: "/plan",
        description: "Mode: investigate and plan, writes blocked",
    },
    CommandSpec {
        name: "/bypass",
        description: "Mode: run EVERYTHING unconfirmed (isolated envs)",
    },
    CommandSpec {
        name: "/permissions",
        description: "Show the effective permission rules",
    },
    CommandSpec {
        name: "/config",
        description: "Edit settings in a dialog",
    },
    CommandSpec {
        name: "/status",
        description: "Show model, token usage and session info",
    },
    CommandSpec {
        name: "/quit",
        description: "Exit picocode",
    },
];

/// Parse one submitted input line. See the module docs for the rules.
pub fn parse(input: &str) -> ParseOutcome {
    let input = input.trim();
    if !input.starts_with('/') || input.contains('\n') {
        return ParseOutcome::Prompt;
    }
    let (name, arg) = match input.split_once(char::is_whitespace) {
        Some((name, arg)) => (name, Some(arg.trim())),
        None => (input, None),
    };
    let arg_string = || arg.map(str::to_string);

    let command = match name {
        "/clear" => no_arg(Command::Clear, name, arg),
        "/compact" => no_arg(Command::Compact, name, arg),
        "/undo" => no_arg(Command::Undo, name, arg),
        "/jobs" => match arg {
            None => Ok(Command::Jobs(JobsAction::List)),
            Some(rest) => match rest.strip_prefix("kill").map(str::trim) {
                Some(id) => match id.parse::<u64>() {
                    Ok(id) => Ok(Command::Jobs(JobsAction::Kill(id))),
                    Err(_) => Err(format!("`/jobs kill` needs a numeric id, got `{id}`")),
                },
                None => Err(format!("unknown /jobs action `{rest}` (try `kill <id>`)")),
            },
        },
        "/model" => Ok(Command::Model(arg_string())),
        "/prompt" => Ok(Command::SystemPrompt(match arg {
            None | Some("") => PromptAction::Edit,
            Some("reset") => PromptAction::Reset,
            Some(name) => PromptAction::Preset(name.to_string()),
        })),
        "/remote" => Ok(Command::Remote(arg_string())),
        "/resume" => Ok(Command::Resume(arg_string())),
        "/attach" => Ok(Command::Attach(arg_string())),
        "/read-only" => no_arg(Command::Mode(Mode::ReadOnly), name, arg),
        "/edit" => no_arg(Command::Mode(Mode::Edit), name, arg),
        "/plan" => no_arg(Command::Mode(Mode::Plan), name, arg),
        "/bypass" => no_arg(Command::Mode(Mode::Bypass), name, arg),
        "/permissions" => no_arg(Command::Permissions, name, arg),
        "/config" | "/settings" => no_arg(Command::Config, name, arg),
        "/status" | "/usage" => no_arg(Command::Status, name, arg),
        "/quit" | "/exit" | "/q" => no_arg(Command::Quit, name, arg),
        _ => {
            return ParseOutcome::Unknown {
                name: name.to_string(),
            };
        }
    };
    match command {
        Ok(command) => ParseOutcome::Command(command),
        Err(message) => ParseOutcome::Invalid { message },
    }
}

/// A command that takes no argument; a stray one is an error rather than
/// silently ignored (it usually means a typo'd different command).
fn no_arg(command: Command, name: &str, arg: Option<&str>) -> Result<Command, String> {
    match arg {
        None | Some("") => Ok(command),
        Some(arg) => Err(format!("{name} takes no argument (got `{arg}`)")),
    }
}

/// File-path candidates under `root` for `/attach`-style arguments:
/// completes the last path segment, directories with a trailing `/`,
/// hidden files only when explicitly asked for. Returns (fill, kind).
pub fn path_completions(root: &std::path::Path, cmd: &str, arg: &str) -> Vec<(String, String)> {
    let (dir_part, file_part) = match arg.rsplit_once('/') {
        Some((d, f)) => (d.to_string(), f.to_string()),
        None => (String::new(), arg.to_string()),
    };
    let dir = root.join(&dir_part);
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return Vec::new();
    };
    let mut out: Vec<(String, String)> = entries
        .filter_map(|e| e.ok())
        .filter_map(|e| {
            let name = e.file_name().to_string_lossy().into_owned();
            if name.starts_with('.') && !file_part.starts_with('.') {
                return None;
            }
            if !name.to_lowercase().starts_with(&file_part.to_lowercase()) {
                return None;
            }
            let is_dir = e.file_type().is_ok_and(|t| t.is_dir());
            let rel = if dir_part.is_empty() {
                name
            } else {
                format!("{dir_part}/{name}")
            };
            Some(if is_dir {
                (format!("{cmd} {rel}/"), "directory".to_string())
            } else {
                (format!("{cmd} {rel}"), "file".to_string())
            })
        })
        .collect();
    out.sort();
    out.truncate(30);
    out
}

/// `/prompt` argument candidates: the configured `[[prompts]]` preset
/// names plus `reset`, filtered by the partial argument.
pub fn prompt_completions(
    prompts: &[crate::config::PromptPreset],
    cmd: &str,
    arg: &str,
) -> Vec<(String, String)> {
    let needle = arg.to_lowercase();
    let mut out: Vec<(String, String)> = prompts
        .iter()
        .filter(|p| needle.is_empty() || p.name.to_lowercase().contains(&needle))
        .map(|p| {
            let first = p.prompt.lines().next().unwrap_or_default();
            let detail: String = first.chars().take(48).collect();
            (format!("{cmd} {}", p.name), detail)
        })
        .collect();
    if "reset".contains(&needle) {
        out.push((
            format!("{cmd} reset"),
            "restore the built-in default".to_string(),
        ));
    }
    out
}

/// `/remote` argument candidates: the configured `[[remotes]]` entry names
/// plus `local`, filtered by the partial argument.
pub fn remote_completions(
    remotes: &[crate::config::RemoteEntry],
    cmd: &str,
    arg: &str,
) -> Vec<(String, String)> {
    let needle = arg.to_lowercase();
    let mut out: Vec<(String, String)> = remotes
        .iter()
        .filter(|r| needle.is_empty() || r.name.to_lowercase().contains(&needle))
        .map(|r| {
            (
                format!("{cmd} {}", r.name),
                format!("{}:{}", r.host, r.path),
            )
        })
        .collect();
    if "local".contains(&needle) {
        out.push((
            format!("{cmd} local"),
            "back to the local workspace".to_string(),
        ));
    }
    out
}

/// `/jobs kill <id>` candidates from the running-jobs registry, filtered
/// by the partial argument (id or command substring).
pub fn jobs_completions(
    jobs: &crate::tools::BackgroundJobs,
    cmd: &str,
    arg: &str,
) -> Vec<(String, String)> {
    let needle = arg.to_lowercase();
    jobs.list()
        .into_iter()
        .filter(|(id, command, _)| {
            needle.is_empty()
                || format!("kill {id}").contains(&needle)
                || command.to_lowercase().contains(&needle)
        })
        .map(|(id, command, elapsed)| {
            (
                format!("{cmd} kill {id}"),
                format!("{}s · {command}", elapsed.as_secs()),
            )
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prompts_are_not_commands() {
        assert_eq!(parse("hello"), ParseOutcome::Prompt);
        assert_eq!(parse("  fix the bug  "), ParseOutcome::Prompt);
        // Multi-line input is never a command, even starting with `/`.
        assert_eq!(parse("/model x\nsecond line"), ParseOutcome::Prompt);
    }

    #[test]
    fn commands_aliases_and_args_parse() {
        assert_eq!(parse("/clear"), ParseOutcome::Command(Command::Clear));
        assert_eq!(parse("/q"), ParseOutcome::Command(Command::Quit));
        assert_eq!(parse("/settings"), ParseOutcome::Command(Command::Config));
        assert_eq!(parse("/usage"), ParseOutcome::Command(Command::Status));
        assert_eq!(
            parse("/plan"),
            ParseOutcome::Command(Command::Mode(Mode::Plan))
        );
        assert_eq!(parse("/model"), ParseOutcome::Command(Command::Model(None)));
        assert_eq!(
            parse("/model qwen3:4b"),
            ParseOutcome::Command(Command::Model(Some("qwen3:4b".into())))
        );
        assert_eq!(
            parse("/attach src/main.rs"),
            ParseOutcome::Command(Command::Attach(Some("src/main.rs".into())))
        );
        assert_eq!(
            parse("/jobs"),
            ParseOutcome::Command(Command::Jobs(JobsAction::List))
        );
        assert_eq!(
            parse("/jobs kill 3"),
            ParseOutcome::Command(Command::Jobs(JobsAction::Kill(3)))
        );
        assert_eq!(
            parse("/prompt"),
            ParseOutcome::Command(Command::SystemPrompt(PromptAction::Edit))
        );
        assert_eq!(
            parse("/prompt reset"),
            ParseOutcome::Command(Command::SystemPrompt(PromptAction::Reset))
        );
        assert_eq!(
            parse("/prompt strict"),
            ParseOutcome::Command(Command::SystemPrompt(PromptAction::Preset("strict".into())))
        );
    }

    #[test]
    fn remote_parses_with_and_without_an_argument() {
        assert_eq!(
            parse("/remote"),
            ParseOutcome::Command(Command::Remote(None))
        );
        assert_eq!(
            parse("/remote box"),
            ParseOutcome::Command(Command::Remote(Some("box".into())))
        );
        assert_eq!(
            parse("/remote user@host:/srv/app"),
            ParseOutcome::Command(Command::Remote(Some("user@host:/srv/app".into())))
        );
    }

    #[test]
    fn unknown_and_invalid_are_reported_not_prompted() {
        // The old TUI sent "/mdoel qwen" to the model; now it is an error.
        assert!(matches!(
            parse("/mdoel qwen"),
            ParseOutcome::Unknown { name } if name == "/mdoel"
        ));
        assert!(matches!(
            parse("/jobs kill x"),
            ParseOutcome::Invalid { .. }
        ));
        assert!(matches!(parse("/jobs foo"), ParseOutcome::Invalid { .. }));
        assert!(matches!(parse("/clear now"), ParseOutcome::Invalid { .. }));
    }

    #[test]
    fn every_listed_command_parses() {
        for spec in COMMANDS {
            assert!(
                matches!(parse(spec.name), ParseOutcome::Command(_)),
                "{} does not parse",
                spec.name
            );
        }
    }
}
