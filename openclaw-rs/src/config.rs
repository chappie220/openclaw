//! Runtime configuration: one TOML file under the state directory, with
//! environment overrides for secrets.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

pub const DEFAULT_BASE_URL: &str = "https://openrouter.ai/api/v1";
/// OpenRouter's router picks a model when the operator has not chosen one.
pub const DEFAULT_MODEL: &str = "openrouter/auto";
const DEFAULT_SYSTEM_PROMPT: &str = "You are OpenClaw, a helpful personal assistant.";

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    pub model: ModelConfig,
    pub agent: AgentConfig,
    pub tools: ToolsConfig,
    pub gateway: GatewayConfig,
    pub qq: QqConfig,
    pub mail: MailConfig,
}

/// Transport security for a mail connection.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum MailSecurity {
    /// Implicit TLS (IMAP 993, SMTP 465).
    Tls,
    /// SMTP STARTTLS (587); IMAP always uses implicit TLS.
    Starttls,
    /// Plain text; only accepted for loopback hosts such as local mail bridges.
    None,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct MailConfig {
    pub enabled: bool,
    pub imap_host: String,
    pub imap_port: u16,
    pub imap_security: MailSecurity,
    pub smtp_host: String,
    pub smtp_port: u16,
    pub smtp_security: MailSecurity,
    pub username: String,
    /// Prefer `MAIL_PASSWORD`. Many providers (QQ Mail, 163) need an app authorization code.
    pub password: Option<String>,
    /// Sender address for replies (default: username).
    pub from: Option<String>,
    /// Required: addresses (or `@domain`) allowed to talk to the bot.
    pub allow: Vec<String>,
    pub mailbox: String,
    pub poll_secs: u64,
}

impl Default for MailConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            imap_host: String::new(),
            imap_port: 993,
            imap_security: MailSecurity::Tls,
            smtp_host: String::new(),
            smtp_port: 465,
            smtp_security: MailSecurity::Tls,
            username: String::new(),
            password: None,
            from: None,
            allow: Vec::new(),
            mailbox: "INBOX".into(),
            poll_secs: 60,
        }
    }
}

impl MailConfig {
    pub fn password(&self) -> Option<String> {
        std::env::var("MAIL_PASSWORD")
            .ok()
            .or_else(|| self.password.clone())
            .filter(|s| !s.is_empty())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct QqConfig {
    pub enabled: bool,
    pub app_id: String,
    /// Prefer `QQ_APP_SECRET`; a secret here is stored in plain text.
    pub app_secret: Option<String>,
    /// User or group openids that may talk to the bot; empty allows everyone.
    pub allow: Vec<String>,
    pub api_base: String,
    pub token_url: String,
}

impl Default for QqConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            app_id: String::new(),
            app_secret: None,
            allow: Vec::new(),
            api_base: "https://api.bot.qq.com".into(),
            token_url: "https://api.bot.qq.com/app/getAppAccessToken".into(),
        }
    }
}

impl QqConfig {
    pub fn app_secret(&self) -> Option<String> {
        std::env::var("QQ_APP_SECRET")
            .ok()
            .or_else(|| self.app_secret.clone())
            .filter(|s| !s.trim().is_empty())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct GatewayConfig {
    pub bind: String,
    /// Required for non-loopback binds; prefer `OPENCLAW_RS_TOKEN`.
    pub token: Option<String>,
}

impl Default for GatewayConfig {
    fn default() -> Self {
        Self {
            bind: "127.0.0.1:18789".into(),
            token: None,
        }
    }
}

impl GatewayConfig {
    pub fn token(&self) -> Option<String> {
        std::env::var("OPENCLAW_RS_TOKEN")
            .ok()
            .or_else(|| self.token.clone())
            .filter(|t| !t.trim().is_empty())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct ModelConfig {
    pub base_url: String,
    pub model: String,
    /// Prefer `OPENROUTER_API_KEY`; a key here is stored in plain text.
    pub api_key: Option<String>,
    pub request_timeout_secs: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct AgentConfig {
    pub system_prompt: String,
    /// Upper bound on model calls per user turn, so a tool loop cannot run forever.
    pub max_steps: usize,
    /// Most recent messages sent to the model; older history stays in SQLite.
    pub history_limit: usize,
}

/// What a tool category may do without asking.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Permission {
    Allow,
    Ask,
    Deny,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct ToolsConfig {
    /// Working directory for relative paths and shell commands (default: <state dir>/workspace).
    pub workspace: Option<PathBuf>,
    pub shell: Permission,
    /// Covers write_file and edit_file; reads are always allowed.
    pub write: Permission,
    pub shell_timeout_secs: u64,
    /// Per-stream cap on tool output returned to the model.
    pub max_output_bytes: usize,
}

impl Default for ToolsConfig {
    fn default() -> Self {
        Self {
            workspace: None,
            shell: Permission::Ask,
            write: Permission::Ask,
            shell_timeout_secs: 120,
            max_output_bytes: 16 * 1024,
        }
    }
}

impl Default for ModelConfig {
    fn default() -> Self {
        Self {
            base_url: DEFAULT_BASE_URL.into(),
            model: DEFAULT_MODEL.into(),
            api_key: None,
            request_timeout_secs: 300,
        }
    }
}

impl Default for AgentConfig {
    fn default() -> Self {
        Self {
            system_prompt: DEFAULT_SYSTEM_PROMPT.into(),
            max_steps: 25,
            history_limit: 200,
        }
    }
}

impl Config {
    pub fn load(path: &Path) -> Result<Self> {
        match std::fs::read_to_string(path) {
            Ok(text) => {
                toml::from_str(&text).with_context(|| format!("invalid config {}", path.display()))
            }
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(err) => Err(err).with_context(|| format!("cannot read {}", path.display())),
        }
    }

    /// Environment wins so secrets never need to live in the config file.
    pub fn api_key(&self) -> Result<String> {
        std::env::var("OPENROUTER_API_KEY")
            .ok()
            .filter(|key| !key.trim().is_empty())
            .or_else(|| {
                self.model
                    .api_key
                    .clone()
                    .filter(|key| !key.trim().is_empty())
            })
            .context(
                "no OpenRouter API key: set OPENROUTER_API_KEY or model.api_key in config.toml",
            )
    }
}

/// State root: `OPENCLAW_RS_HOME`, else `~/.openclaw-rs`.
pub fn state_dir() -> Result<PathBuf> {
    if let Some(dir) = std::env::var_os("OPENCLAW_RS_HOME").filter(|v| !v.is_empty()) {
        return Ok(PathBuf::from(dir));
    }
    let home = std::env::var_os("HOME")
        .filter(|v| !v.is_empty())
        .context("HOME is not set")?;
    Ok(PathBuf::from(home).join(".openclaw-rs"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn partial_file_keeps_defaults() {
        let config: Config = toml::from_str("[model]\nmodel = \"x/y\"\n").unwrap();
        assert_eq!(config.model.model, "x/y");
        assert_eq!(config.model.base_url, DEFAULT_BASE_URL);
        assert_eq!(config.agent.max_steps, 25);
    }
}
