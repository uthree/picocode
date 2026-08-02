//! Textual reports shared by the front ends' `/status` and `/permissions`
//! commands, so the two UIs print the same thing.

use std::path::Path;

use crate::config::{Config, Mode};

/// The per-run numbers `/status` needs beyond what [`Config`] holds.
pub struct StatusInfo<'a> {
    /// Display label of the active model (the TUI caches its own copy).
    pub model_label: &'a str,
    /// Context tokens of the last completion request.
    pub ctx_tokens: u64,
    /// The "output …" line, preformatted — the front ends track output
    /// tokens differently (per-turn live counters vs. last reported).
    pub output: String,
    pub session_id: &'a str,
    /// User prompts in the transcript so far.
    pub prompts: usize,
    pub sessions_dir: Option<&'a Path>,
    /// Connected MCP servers ("name (n tools), …"), if any.
    pub mcp: Option<String>,
}

/// The `/status` (alias `/usage`) overview block.
pub fn status_text(cfg: &Config, info: &StatusInfo) -> String {
    let entry = match &cfg.active_model {
        Some(name) => format!(" — [[models]] entry `{name}`"),
        None => String::new(),
    };
    let endpoint = crate::models::base_url(cfg.provider, cfg.base_url.as_deref());
    let pct = (info.ctx_tokens as f64 / cfg.context_window.max(1) as f64 * 100.0).round() as u64;
    let saved = match info.sessions_dir {
        Some(dir) => format!("autosaved under {}", dir.display()),
        None => "not saved (no home directory)".to_string(),
    };
    let config = if cfg.config_files.is_empty() {
        "(built-in defaults)".to_string()
    } else {
        cfg.config_files.join(", ")
    };
    let instructions = if cfg.instructions.is_empty() {
        "(none found)".to_string()
    } else {
        let names: Vec<&str> = cfg.instructions.iter().map(|(n, _)| n.as_str()).collect();
        names.join(", ")
    };
    let mcp = match &info.mcp {
        Some(list) => format!("\nmcp           {list}"),
        None => String::new(),
    };
    let workspace = match &cfg.remote {
        Some(spec) => format!("\nworkspace     remote — {}", spec.destination),
        None => String::new(),
    };
    format!(
        "Status\n\
         model         {}{entry}\n\
         endpoint      {endpoint}\n\
         mode          {} — /permissions shows the rules\n\
         context       {} of {} tokens ({pct}%)\n\
         output        {}\n\
         session       {} — {} prompts, {saved}\n\
         project       {}{workspace}\n\
         config        {config}\n\
         instructions  {instructions}{mcp}",
        info.model_label,
        cfg.mode.get().label(),
        info.ctx_tokens,
        cfg.context_window,
        info.output,
        info.session_id,
        info.prompts,
        cfg.root.display(),
    )
}

/// The `/permissions` block: what the current mode and config rules do.
/// (The TUI appends its own note about `!` commands, which the GUI lacks.)
pub fn permissions_text(cfg: &Config) -> String {
    let mode = cfg.mode.get();
    let mode_line = match mode {
        Mode::ReadOnly => "destructive calls ask unless allow-listed",
        Mode::Edit => "file writes run freely; other destructive calls ask unless allow-listed",
        Mode::Plan => "bash and file writes are denied; web tools ask unless allow-listed",
        Mode::Auto => {
            "like edit, but a reviewer model answers the prompts instead of you \
             (destructive commands still ask)"
        }
        Mode::Bypass => "EVERYTHING runs without confirmation (deny rules still apply)",
    };
    let rules = cfg.approval.snapshot();
    let list = |xs: &[String]| {
        if xs.is_empty() {
            "(none)".to_string()
        } else {
            xs.join(", ")
        }
    };
    format!(
        "Permissions — precedence: deny > mode (plan/bypass) > allow > ask\n\
         mode         [{}] {mode_line}\n\
         deny_tools   {}\n\
         deny_bash    {}\n\
         allow_tools  {}\n\
         allow_bash   {}\n\
         sandbox      {}\n\
         Local reads (read_file, list_files, grep) always run; file tools are \
         confined to {}. Commands with $( ), backticks or > never auto-run.",
        mode.label(),
        list(&rules.deny_tools),
        list(&rules.deny_bash),
        list(&rules.allow_tools),
        list(&rules.allow_bash),
        cfg.sandbox.describe(),
        cfg.root.display(),
    )
}
