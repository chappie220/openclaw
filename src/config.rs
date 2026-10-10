//! Runtime configuration: one TOML file under the state directory, with
//! environment overrides for secrets.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

pub const DEFAULT_BASE_URL: &str = "https://openrouter.ai/api/v1";
/// Leaves the name to the identity chosen on first start.
const DEFAULT_SYSTEM_PROMPT: &str = "You are a helpful personal assistant running on OpenClaw.";

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    /// Language of CLI output and the program's own chat replies; `None`
    /// follows the locale. Root keys must come before the tables in TOML.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub language: Option<crate::i18n::Lang>,
    pub model: ModelConfig,
    pub agent: AgentConfig,
    pub tools: ToolsConfig,
    pub gateway: GatewayConfig,
    pub qq: QqConfig,
    pub mail: MailConfig,
    pub search: SearchConfig,
    pub browser: BrowserConfig,
    pub guide: GuideConfig,
    pub access: crate::access::AccessConfig,
}

/// Guided conversation: messages sent while a turn runs join that turn.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct GuideConfig {
    /// Off: a message sent during a turn waits and runs as its own turn.
    pub enabled: bool,
    /// Which model picks the moment; `off` inserts at the very next step.
    pub provider: ReviewProvider,
    /// Default: `typesafe/jev-1.13` (openrouter), model.model (openrouter-chat),
    /// `jev-latest` (typesafe), or `kev-latest` (typesafe with base_url).
    pub model: Option<String>,
    /// TypeSafe key; prefer `TYPESAFE_API_KEY`. Not sent to a local base_url.
    pub api_key: Option<String>,
    /// Loopback origin of a local Kev System One server.
    pub base_url: Option<String>,
    /// Probability that now is a good moment at or above which the messages go in.
    pub insert_at: f64,
    /// Steps a message may be held back before it goes in anyway.
    pub max_wait_steps: usize,
    pub timeout_secs: u64,
    /// When messages go in, the agent replies to them at once (written by
    /// agent.summary_model, else model.model): what it understood, what it will do.
    pub ack: bool,
}

impl Default for GuideConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            provider: ReviewProvider::Openrouter,
            model: None,
            api_key: None,
            base_url: None,
            insert_at: 0.5,
            max_wait_steps: 3,
            timeout_secs: 10,
            ack: true,
        }
    }
}

impl GuideConfig {
    pub fn api_key(&self) -> Option<String> {
        std::env::var("TYPESAFE_API_KEY")
            .ok()
            .or_else(|| self.api_key.clone())
            .filter(|k| !k.trim().is_empty())
    }
}

/// Where `web_search` looks things up.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SearchProvider {
    /// OpenRouter's `web` plugin with the configured key; billed per search.
    Openrouter,
    /// A SearXNG instance with the JSON format enabled (`searxng_url`).
    Searxng,
    /// No `web_search` tool.
    Off,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct SearchConfig {
    pub provider: SearchProvider,
    /// Model that runs OpenRouter searches (default: model.model).
    pub model: Option<String>,
    pub searxng_url: Option<String>,
    pub max_results: usize,
}

impl Default for SearchConfig {
    fn default() -> Self {
        Self {
            provider: SearchProvider::Openrouter,
            model: None,
            searxng_url: None,
            max_results: 5,
        }
    }
}

/// `browser`: a Chromium-family browser installed on the host, driven over
/// the DevTools protocol. Nothing is bundled; without one the tool is absent.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct BrowserConfig {
    /// Off: no `browser` tool even when a browser is installed.
    pub enabled: bool,
    /// Browser to start; default: the first of chromium, chromium-browser,
    /// google-chrome, microsoft-edge, brave on PATH (or in /Applications).
    pub executable: Option<PathBuf>,
    /// Use a browser that is already running with remote debugging instead
    /// of starting one, e.g. `http://127.0.0.1:9222`.
    pub cdp_url: Option<String>,
    /// Without a window; `false` needs a display.
    pub headless: bool,
    /// Extra command-line flags, e.g. `--proxy-server=socks5://127.0.0.1:1080`.
    pub args: Vec<String>,
    /// Seconds one action (open, click, ...) may take.
    pub timeout_secs: u64,
    /// The browser is closed after this many seconds without use, freeing its memory.
    pub idle_secs: u64,
    /// Characters of page text returned per read.
    pub max_chars: usize,
}

impl Default for BrowserConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            executable: None,
            cdp_url: None,
            headless: true,
            args: Vec::new(),
            timeout_secs: 30,
            idle_secs: 300,
            max_chars: 8000,
        }
    }
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
    /// Required: there is no default model, so the operator always chooses.
    pub model: String,
    /// Prefer `OPENROUTER_API_KEY`; a key here is stored in plain text.
    pub api_key: Option<String>,
    pub request_timeout_secs: u64,
    /// Models OpenRouter tries, in order, when `model` fails (OpenRouter's
    /// `models` parameter; other servers ignore it).
    pub fallbacks: Vec<String>,
    /// Retries of a failed request (connection errors, HTTP 408/429/5xx, a
    /// stream that fails before any text), with exponential backoff.
    pub max_retries: u32,
    pub prompt_cache: PromptCache,
}

/// Whether to mark `cache_control` breakpoints for prompt caching.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PromptCache {
    /// For `anthropic/` and `google/` models, which cache only at breakpoints;
    /// others (OpenAI, DeepSeek, …) cache on their own.
    #[default]
    Auto,
    On,
    Off,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct AgentConfig {
    pub system_prompt: String,
    /// Upper bound on model calls per user turn, so a tool loop cannot run forever.
    pub max_steps: usize,
    /// Token budget for each model call: system prompt, tool specs and
    /// history. Keep it below the model's context window, leaving room for
    /// the answer. Older history stays in SQLite.
    pub context_tokens: usize,
    /// Model that writes the context summary, e.g. a cheaper one; default:
    /// model.model.
    pub summary_model: Option<String>,
    /// Model for the calls that carry images (files people send, browser
    /// screenshots); default: model.model. Set it when the main model has
    /// no image input.
    pub vision_model: Option<String>,
    /// Saved memories looked up from each message and shown with it; 0 turns
    /// recall off.
    pub recall_limit: usize,
    /// Token cap on the recalled memories of one turn.
    pub recall_tokens: usize,
    /// Where files sent with messages are saved (under `inbox/`); set at
    /// startup from `tools.workspace`, never read from the file.
    #[serde(skip)]
    pub workspace: PathBuf,
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
    /// `deny` removes `identity_set`; otherwise every draft, including the
    /// first, is saved only after a person approves it.
    pub identity: Permission,
    pub shell_timeout_secs: u64,
    /// Per-stream cap on tool output returned to the model, and on shell output
    /// buffered while a command runs.
    pub max_output_bytes: usize,
    /// Model review of `shell` commands before anyone is asked.
    pub review: ReviewConfig,
}

/// Which model judges whether a shell command is dangerous.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ReviewProvider {
    /// Every `ask` command goes to a person.
    Off,
    /// A decision model on OpenRouter's decisions API (`typesafe/jev-1.13`), with the OpenRouter key.
    Openrouter,
    /// Any OpenRouter chat model, asked for a JSON rating, with the OpenRouter key.
    #[serde(rename = "openrouter-chat")]
    OpenrouterChat,
    /// TypeSafe AI's own System One API (`jev-latest` hosted, or a local Kev server).
    Typesafe,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct ReviewConfig {
    pub provider: ReviewProvider,
    /// Default: `typesafe/jev-1.13` (openrouter), model.model (openrouter-chat),
    /// `jev-latest` (typesafe), or `kev-latest` (typesafe with base_url).
    pub model: Option<String>,
    /// TypeSafe key; prefer `TYPESAFE_API_KEY`. Not sent to a local base_url.
    pub api_key: Option<String>,
    /// Loopback origin of a local Kev System One server, e.g. http://127.0.0.1:8009.
    pub base_url: Option<String>,
    /// Danger probability below which a command runs without asking.
    pub allow_below: f64,
    /// Danger probability at or above which a command is declined without asking.
    pub deny_at: f64,
    pub timeout_secs: u64,
}

impl Default for ReviewConfig {
    fn default() -> Self {
        Self {
            provider: ReviewProvider::Off,
            model: None,
            api_key: None,
            base_url: None,
            allow_below: 0.2,
            deny_at: 0.9,
            timeout_secs: 20,
        }
    }
}

impl ReviewConfig {
    pub fn api_key(&self) -> Option<String> {
        std::env::var("TYPESAFE_API_KEY")
            .ok()
            .or_else(|| self.api_key.clone())
            .filter(|k| !k.trim().is_empty())
    }
}

impl Default for ToolsConfig {
    fn default() -> Self {
        Self {
            workspace: None,
            shell: Permission::Ask,
            write: Permission::Ask,
            identity: Permission::Ask,
            shell_timeout_secs: 120,
            max_output_bytes: 16 * 1024,
            review: ReviewConfig::default(),
        }
    }
}

impl Default for ModelConfig {
    fn default() -> Self {
        Self {
            base_url: DEFAULT_BASE_URL.into(),
            model: String::new(),
            api_key: None,
            request_timeout_secs: 300,
            fallbacks: Vec::new(),
            max_retries: 3,
            prompt_cache: PromptCache::Auto,
        }
    }
}

impl Default for AgentConfig {
    fn default() -> Self {
        Self {
            system_prompt: DEFAULT_SYSTEM_PROMPT.into(),
            max_steps: 25,
            context_tokens: 64_000,
            summary_model: None,
            vision_model: None,
            recall_limit: 5,
            recall_tokens: 800,
            workspace: PathBuf::new(),
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

    /// The main model; there is none until the operator sets one.
    pub fn model_id(&self) -> Result<&str> {
        let model = self.model.model.trim();
        if model.is_empty() {
            anyhow::bail!(crate::cli_text::NO_MODEL.now());
        }
        Ok(model)
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
                "no OpenRouter API key: run `openclaw-rs init`, or set OPENROUTER_API_KEY or model.api_key in config.toml",
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

    #[test]
    fn the_model_must_be_chosen() {
        let config = Config::default();
        assert!(config.model.model.is_empty());
        assert!(
            config
                .model_id()
                .unwrap_err()
                .to_string()
                .contains("model.model")
        );
        let config: Config = toml::from_str("[model]\nmodel = \" x/y \"\n").unwrap();
        assert_eq!(config.model_id().unwrap(), "x/y");
    }
}
