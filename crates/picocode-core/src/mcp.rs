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
    let mut servers = Vec::new();
    let mut errors = Vec::new();
    for config in configs {
        match tokio::time::timeout(CONNECT_TIMEOUT, connect_one(config)).await {
            Ok(Ok(connection)) => servers.push(connection),
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

async fn connect_one(config: &McpServer) -> anyhow::Result<Connection> {
    let service = match (&config.command, &config.url) {
        (Some(command), None) => {
            let mut cmd = tokio::process::Command::new(command);
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
