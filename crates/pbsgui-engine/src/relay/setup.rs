//! Relay configuration and startup.
//!
//! Two small JSON files under the config dir decide this install's relay
//! roles (both may be present; the roles are independent):
//!
//! - `relay.json` - this machine is an AGENT: it dials the proxy and serves
//!   VDI device sessions. The token lives in the credential store under
//!   `relay:proxy`. Written by `pbsgui-engine relay join`.
//! - `relay-server.json` - this machine is a PROXY: it listens for agents.
//!   Each agent's token lives under `relayagent:<name>`. Managed by
//!   `pbsgui-engine relay add-agent` (which also prints what to paste into
//!   `relay join` on the SQL host).

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Context;
use serde::{Deserialize, Serialize};
use tokio_util::sync::CancellationToken;

use crate::{config, secrets};

use super::agent::{run_agent, AgentConfig, VdiDeviceRunner};
use super::proto::DEFAULT_RELAY_PORT;
use super::server::{AgentAuth, RelayServer};

/// `relay.json`: this machine serves VDI sessions for a proxy.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentFile {
    /// `host:port` of the proxy's relay listener.
    pub proxy_addr: String,
    /// SHA-256 fingerprint of the proxy's relay certificate.
    pub proxy_fingerprint: String,
    /// This agent's name in the proxy's registry.
    pub agent_name: String,
}

/// `relay-server.json`: this machine accepts relay agents.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServerFile {
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// Listen address; agents connect from other machines, so all interfaces
    /// by default (the port carries only pinned TLS with per-agent tokens).
    #[serde(default = "default_bind")]
    pub bind: String,
    #[serde(default = "default_port")]
    pub port: u16,
    /// Configured agent names (tokens live in the credential store).
    #[serde(default)]
    pub agents: Vec<String>,
}

fn default_true() -> bool {
    true
}
fn default_bind() -> String {
    "0.0.0.0".to_string()
}
fn default_port() -> u16 {
    DEFAULT_RELAY_PORT
}

fn agent_file_path() -> PathBuf {
    config::config_dir().join("relay.json")
}
fn server_file_path() -> PathBuf {
    config::config_dir().join("relay-server.json")
}
/// Where the proxy's relay certificate and key live.
fn tls_dir() -> PathBuf {
    config::config_dir().join("relay")
}

/// The credential-store key for the agent's token toward its proxy.
const AGENT_TOKEN_KEY: &str = "relay:proxy";
/// The credential-store key for a configured agent's token on the proxy.
fn agent_token_key(name: &str) -> String {
    format!("relayagent:{name}")
}

pub fn load_agent_file() -> Option<AgentFile> {
    let bytes = std::fs::read(agent_file_path()).ok()?;
    match serde_json::from_slice(&bytes) {
        Ok(f) => Some(f),
        Err(e) => {
            tracing::warn!("ignoring malformed relay.json: {e}");
            None
        }
    }
}

pub fn load_server_file() -> Option<ServerFile> {
    let bytes = std::fs::read(server_file_path()).ok()?;
    match serde_json::from_slice(&bytes) {
        Ok(f) => Some(f),
        Err(e) => {
            tracing::warn!("ignoring malformed relay-server.json: {e}");
            None
        }
    }
}

/// Start whichever relay roles this install is configured for. Called once
/// from `run_engine`; failures are logged, never fatal to the engine (a broken
/// relay config must not take down local backups).
pub async fn start() {
    if let Some(file) = load_server_file() {
        if file.enabled {
            if let Err(e) = start_server(&file).await {
                tracing::error!("relay listener failed to start: {e:#}");
            }
        }
    }
    if let Some(file) = load_agent_file() {
        match secrets::get(AGENT_TOKEN_KEY) {
            Ok(Some(token)) => {
                let cfg = AgentConfig {
                    proxy_addr: file.proxy_addr,
                    proxy_fingerprint: file.proxy_fingerprint,
                    agent_name: file.agent_name,
                    token,
                };
                tracing::info!(
                    "starting relay agent '{}' toward {}",
                    cfg.agent_name,
                    cfg.proxy_addr
                );
                tokio::spawn(run_agent(
                    cfg,
                    Arc::new(VdiDeviceRunner),
                    CancellationToken::new(),
                ));
            }
            Ok(None) => tracing::error!(
                "relay.json is present but no token is stored; run `pbsgui-engine relay join` again"
            ),
            Err(e) => tracing::error!("could not read the relay token: {e:#}"),
        }
    }
}

async fn start_server(file: &ServerFile) -> anyhow::Result<()> {
    let tls = super::tls::server_tls(&tls_dir())?;
    let mut agents = Vec::new();
    for name in &file.agents {
        match secrets::get(&agent_token_key(name))? {
            Some(token) => agents.push(AgentAuth {
                name: name.clone(),
                token,
            }),
            None => tracing::warn!("relay agent '{name}' has no stored token; skipping"),
        }
    }
    let addr = format!("{}:{}", file.bind, file.port);
    let listener = tokio::net::TcpListener::bind(&addr)
        .await
        .with_context(|| format!("binding the relay listener on {addr}"))?;
    tracing::info!(
        "relay listener on {addr} ({} agent(s) configured, fingerprint {})",
        agents.len(),
        tls.fingerprint
    );
    let server = RelayServer::new(agents);
    super::server::set_global(server.clone());
    tokio::spawn(async move {
        server.run(listener, tls, CancellationToken::new()).await;
    });
    Ok(())
}

/// CLI `relay add-agent <name>` (run on the PROXY): configure an agent name,
/// generate its token, and print everything the SQL host needs for `relay
/// join`. Idempotent per name: re-running rotates the token.
pub fn cli_add_agent(name: &str) -> anyhow::Result<()> {
    if name.is_empty() || !name.chars().all(|c| c.is_ascii_alphanumeric() || c == '-') {
        anyhow::bail!("agent names are ascii letters, digits, and dashes");
    }
    config::ensure_dirs();
    // Materialize the certificate now so the fingerprint can be printed.
    let tls = super::tls::server_tls(&tls_dir())?;
    let mut file = load_server_file().unwrap_or(ServerFile {
        enabled: true,
        bind: default_bind(),
        port: default_port(),
        agents: Vec::new(),
    });
    let token = uuid::Uuid::new_v4().simple().to_string();
    secrets::set(&agent_token_key(name), &token)?;
    if !file.agents.iter().any(|a| a == name) {
        file.agents.push(name.to_string());
    }
    std::fs::write(server_file_path(), serde_json::to_vec_pretty(&file)?)
        .context("writing relay-server.json")?;

    println!("relay agent '{name}' configured on this proxy.");
    println!("On the SQL Server host, run:");
    println!();
    println!(
        "  pbsgui-engine relay join --proxy <THIS-HOST>:{} --fingerprint {} --name {name} --token {token}",
        file.port, tls.fingerprint
    );
    println!();
    println!("Then restart the pbsgui engine service on both machines.");
    Ok(())
}

/// CLI `relay join` (run on the SQL HOST): store how to reach the proxy.
pub fn cli_join(proxy: &str, fingerprint: &str, name: &str, token: &str) -> anyhow::Result<()> {
    config::ensure_dirs();
    let file = AgentFile {
        proxy_addr: proxy.to_string(),
        proxy_fingerprint: fingerprint.to_string(),
        agent_name: name.to_string(),
    };
    secrets::set(AGENT_TOKEN_KEY, token)?;
    std::fs::write(agent_file_path(), serde_json::to_vec_pretty(&file)?)
        .context("writing relay.json")?;
    println!(
        "relay agent '{name}' configured toward {proxy}; restart the pbsgui engine service to \
         connect."
    );
    Ok(())
}

/// The relay agents configured on this machine (proxy role), each flagged with
/// whether it is currently connected. Empty when this install is not a proxy.
/// Merges the configured list (relay-server.json) with the live registry so the
/// GUI can show a configured-but-offline agent, not only connected ones.
pub fn agent_infos() -> Vec<pbsgui_ipc::RelayAgentInfo> {
    let configured = load_server_file().map(|f| f.agents).unwrap_or_default();
    let connected = super::server::global()
        .map(|s| s.agents())
        .unwrap_or_default();
    configured
        .into_iter()
        .map(|name| {
            let live = connected.iter().find(|a| a.name == name);
            pbsgui_ipc::RelayAgentInfo {
                connected: live.is_some(),
                host: live.map(|a| a.host.clone()),
                version: live.map(|a| a.version.clone()),
                name,
            }
        })
        .collect()
}

/// CLI `relay show`: print this install's relay roles.
pub fn cli_show() -> anyhow::Result<()> {
    match load_server_file() {
        Some(f) => {
            let fp = super::tls::server_tls(&tls_dir())
                .map(|t| t.fingerprint)
                .unwrap_or_else(|e| format!("(unavailable: {e})"));
            println!(
                "proxy role: {} on {}:{} - agents: {} - fingerprint {fp}",
                if f.enabled { "enabled" } else { "disabled" },
                f.bind,
                f.port,
                if f.agents.is_empty() {
                    "(none)".to_string()
                } else {
                    f.agents.join(", ")
                }
            );
        }
        None => println!("proxy role: not configured (relay-server.json absent)"),
    }
    match load_agent_file() {
        Some(f) => println!(
            "agent role: '{}' toward {} (token {})",
            f.agent_name,
            f.proxy_addr,
            match secrets::get(AGENT_TOKEN_KEY) {
                Ok(Some(_)) => "stored",
                _ => "MISSING - run relay join again",
            }
        ),
        None => println!("agent role: not configured (relay.json absent)"),
    }
    Ok(())
}
