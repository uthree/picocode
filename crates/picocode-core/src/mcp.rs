//! Opt-in MCP (Model Context Protocol) client support.
//!
//! Servers listed under `[[mcp_servers]]` in picocode.toml are connected
//! once at startup; their tools are registered on every agent the app
//! spawns (model switches and `/prompt` respawns reuse the same
//! connections). With no servers configured this module never runs and
//! nothing changes for the model — that opt-in is deliberate policy:
//! extra tools confuse small local models.
//!
//! MCP tool calls go through the same approval flow as destructive
//! built-ins: names the approval hook doesn't recognize are treated as
//! external and ask by default (allow-listable via `allow_tools`).
//!
//! That check is by *name*, which is why [`may_register`] refuses a server
//! tool that wears a built-in's name — see its doc comment.

use std::collections::HashSet;
use std::sync::Arc;

use anyhow::Context as _;
use rmcp::ServiceExt;
use rmcp::service::{RoleClient, RunningService, ServerSink};

use crate::config::McpServer;

/// Cap on connecting + listing one server, so a misconfigured command
/// (e.g. one that reads stdin forever) can't hang startup.
const CONNECT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(20);

/// One connected MCP server and its advertised tools. Dropping this
/// disconnects (the child process exits / the HTTP session closes).
pub struct Connection {
    pub name: String,
    pub tools: Vec<rmcp::model::Tool>,
    pub sink: ServerSink,
    _service: RunningService<RoleClient, ()>,
}

/// All MCP connections of this app instance, shared across worker
/// respawns. Empty when nothing is configured.
#[derive(Clone, Default)]
pub struct McpConnections {
    pub servers: Arc<Vec<Connection>>,
}

impl McpConnections {
    pub fn is_empty(&self) -> bool {
        self.servers.is_empty()
    }

    /// Total tools across all servers.
    pub fn tool_count(&self) -> usize {
        self.servers.iter().map(|s| s.tools.len()).sum()
    }

    /// `name (n tools), …` — the /status line.
    pub fn summary(&self) -> String {
        self.servers
            .iter()
            .map(|s| format!("{} ({} tools)", s.name, s.tools.len()))
            .collect::<Vec<_>>()
            .join(", ")
    }
}

/// Connect every configured server. Failures don't abort startup: each
/// failed server is reported as a message for the UI to show, and the
/// app runs with whatever connected.
pub async fn connect_all(configs: &[McpServer]) -> (McpConnections, Vec<String>) {
    connect_all_in(configs, None).await
}

/// Connect session-owned servers. Local stdio processes start in that
/// session's workspace; remote workspaces keep the local launch directory.
pub async fn connect_all_in(
    configs: &[McpServer],
    root: Option<&std::path::Path>,
) -> (McpConnections, Vec<String>) {
    let mut servers = Vec::new();
    let mut errors = Vec::new();
    let mut taken = HashSet::new();
    for config in configs {
        match tokio::time::timeout(CONNECT_TIMEOUT, connect_one(config, root)).await {
            Ok(Ok(mut connection)) => {
                let mut refused = Vec::new();
                connection.tools.retain(|tool| {
                    let ok = may_register(tool.name.as_ref(), &mut taken);
                    if !ok {
                        refused.push(tool.name.to_string());
                    }
                    ok
                });
                if !refused.is_empty() {
                    errors.push(format!(
                        "MCP server `{}`: not registering {} — that name is already taken, \
                         and a tool taking it over would inherit the approval class the \
                         name carries instead of being asked about",
                        config.name,
                        refused.join(", ")
                    ));
                }
                servers.push(connection);
            }
            Ok(Err(e)) => errors.push(format!("MCP server `{}`: {e:#}", config.name)),
            Err(_) => errors.push(format!(
                "MCP server `{}`: connection timed out after {}s",
                config.name,
                CONNECT_TIMEOUT.as_secs()
            )),
        }
    }
    (
        McpConnections {
            servers: Arc::new(servers),
        },
        errors,
    )
}

/// Whether a server-supplied tool name may be registered, remembering it
/// when it may.
///
/// Two things go wrong when it collides with a built-in. rig's tool set is
/// keyed by name and MCP tools are added last, so the server's tool
/// *replaces* the built-in (a `tracing::warn!` nobody sees — picocode
/// installs no subscriber). And the approval hook classifies by name: a
/// tool called `read_file`, `grep`, `list_files` or `submit_plan` is not in
/// `DESTRUCTIVE_TOOLS`, so it would run with no prompt in any mode, while
/// `edit_file` would inherit the edit-mode auto-approval. An unrecognized
/// name asks — which is what every MCP tool should do.
///
/// Names are also unique across servers: the second one to claim a name
/// would silently shadow the first.
fn may_register(name: &str, taken: &mut HashSet<String>) -> bool {
    !crate::tools::ALL_TOOLS.contains(&name) && taken.insert(name.to_string())
}

async fn connect_one(
    config: &McpServer,
    root: Option<&std::path::Path>,
) -> anyhow::Result<Connection> {
    let service = match (&config.command, &config.url) {
        (Some(command), None) => {
            let mut cmd = tokio::process::Command::new(command);
            if let Some(root) = root {
                cmd.current_dir(root);
            }
            cmd.args(&config.args);
            for (key, value) in &config.env {
                cmd.env(key, value);
            }
            let transport = rmcp::transport::TokioChildProcess::new(cmd)
                .with_context(|| format!("spawning `{command}`"))?;
            ().serve(transport).await.context("MCP handshake")?
        }
        (None, Some(url)) => {
            let transport = rmcp::transport::StreamableHttpClientTransport::from_uri(url.as_str());
            ().serve(transport).await.context("MCP handshake")?
        }
        _ => anyhow::bail!("set exactly one of `command` or `url`"),
    };
    let tools = service
        .peer()
        .list_all_tools()
        .await
        .context("listing tools")?;
    Ok(Connection {
        name: config.name.clone(),
        tools,
        sink: service.peer().clone(),
        _service: service,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn built_in_names_and_duplicates_are_refused() {
        let mut taken = HashSet::new();
        assert!(may_register("weather", &mut taken));
        // A second server claiming the same name would shadow the first.
        assert!(!may_register("weather", &mut taken));

        // Every built-in is off limits, whether or not it is destructive:
        // the non-destructive ones are the dangerous case, since the hook
        // lets those run without asking.
        for name in crate::tools::ALL_TOOLS {
            assert!(!may_register(name, &mut taken), "{name} should be refused");
        }
    }
}
