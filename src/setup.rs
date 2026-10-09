//! `openclaw-rs config`: an interactive editor for config.toml.
//!
//! Edits the file in place with `toml_edit`, so comments and keys it does not
//! know survive. Every change is checked by parsing the whole file as a
//! `Config`; a change that does not parse is undone on the spot.

use std::io::{BufRead, Write};
use std::path::Path;

use anyhow::{Context, Result, bail};
use toml_edit::{Array, DocumentMut, Item, Value};

use crate::config::Config;

#[derive(Clone, Copy)]
enum Kind {
    Text,
    Int,
    Bool,
    Choice(&'static [&'static str]),
    /// Comma-separated strings.
    List,
    /// Read without echo; `env` overrides the file when set.
    Secret {
        env: &'static str,
        generate: bool,
    },
}

struct Field {
    path: &'static [&'static str],
    label: &'static str,
    help: &'static str,
    kind: Kind,
}

struct Section {
    title: &'static str,
    fields: &'static [Field],
}

const PERMISSION: &[&str] = &["allow", "ask", "deny"];
const SECURITY: &[&str] = &["tls", "starttls", "none"];

const SECTIONS: &[Section] = &[
    Section {
        title: "Model and context",
        fields: &[
            Field {
                path: &["model", "model"],
                label: "Model",
                help: "Any OpenRouter model id, e.g. anthropic/claude-sonnet-4.5 or openrouter/auto.",
                kind: Kind::Text,
            },
            Field {
                path: &["model", "api_key"],
                label: "OpenRouter API key",
                help: "Stored in plain text in config.toml (mode 0600); OPENROUTER_API_KEY wins when set.",
                kind: Kind::Secret {
                    env: "OPENROUTER_API_KEY",
                    generate: false,
                },
            },
            Field {
                path: &["model", "fallbacks"],
                label: "Fallback models",
                help: "Comma-separated; OpenRouter tries them in order when the model fails.",
                kind: Kind::List,
            },
            Field {
                path: &["model", "max_retries"],
                label: "Retries on transient errors",
                help: "Connection errors, HTTP 408/429/5xx, and streams that fail before any text.",
                kind: Kind::Int,
            },
            Field {
                path: &["model", "prompt_cache"],
                label: "Prompt cache breakpoints",
                help: "auto: for anthropic/ and google/ models; on: always (e.g. openrouter/auto routing to Claude); off: never.",
                kind: Kind::Choice(&["auto", "on", "off"]),
            },
            Field {
                path: &["agent", "context_tokens"],
                label: "Context budget (tokens)",
                help: "Per model call: system prompt, tools, summary and history. Keep it below the model's window.",
                kind: Kind::Int,
            },
            Field {
                path: &["agent", "summary_model"],
                label: "Summary model",
                help: "Model that writes the context summary, e.g. a cheaper one. Reset (-) to use the main model.",
                kind: Kind::Text,
            },
            Field {
                path: &["agent", "max_steps"],
                label: "Max model calls per turn",
                help: "Stops a runaway tool loop.",
                kind: Kind::Int,
            },
        ],
    },
    Section {
        title: "Tools",
        fields: &[
            Field {
                path: &["tools", "shell"],
                label: "Shell commands",
                help: "allow: run without asking; ask: a person approves each one; deny: no shell tool.",
                kind: Kind::Choice(PERMISSION),
            },
            Field {
                path: &["tools", "write"],
                label: "File writes and edits",
                help: "Reads are always allowed inside the workspace.",
                kind: Kind::Choice(PERMISSION),
            },
            Field {
                path: &["tools", "identity"],
                label: "Identity changes",
                help: "deny removes identity_set; otherwise every draft needs a person's approval.",
                kind: Kind::Choice(PERMISSION),
            },
            Field {
                path: &["tools", "workspace"],
                label: "Workspace directory",
                help: "Where files and shell commands run. Reset (-) for <state dir>/workspace.",
                kind: Kind::Text,
            },
            Field {
                path: &["tools", "review", "provider"],
                label: "Shell command auto-review",
                help: "A model rates each `ask` command; see README \"Command auto-review\".",
                kind: Kind::Choice(&["off", "openrouter", "openrouter-chat", "typesafe"]),
            },
        ],
    },
    Section {
        title: "Gateway and Web UI",
        fields: &[
            Field {
                path: &["gateway", "bind"],
                label: "Listen address",
                help: "e.g. 127.0.0.1:18789, or 0.0.0.0:18789 for the LAN (needs a token).",
                kind: Kind::Text,
            },
            Field {
                path: &["gateway", "token"],
                label: "Access token",
                help: "Required unless bound to loopback; OPENCLAW_RS_TOKEN wins when set. Type + to generate one.",
                kind: Kind::Secret {
                    env: "OPENCLAW_RS_TOKEN",
                    generate: true,
                },
            },
        ],
    },
    Section {
        title: "QQ bot",
        fields: &[
            Field {
                path: &["qq", "enabled"],
                label: "Enabled",
                help: "Connects to QQ over WebSocket when `serve` runs.",
                kind: Kind::Bool,
            },
            Field {
                path: &["qq", "app_id"],
                label: "AppID",
                help: "From the QQ Open Platform (q.qq.com).",
                kind: Kind::Text,
            },
            Field {
                path: &["qq", "app_secret"],
                label: "AppSecret",
                help: "QQ_APP_SECRET wins when set.",
                kind: Kind::Secret {
                    env: "QQ_APP_SECRET",
                    generate: false,
                },
            },
            Field {
                path: &["qq", "allow"],
                label: "Allowed openids",
                help: "Comma-separated user or group openids; empty lets everyone chat. The log prints each sender's openid.",
                kind: Kind::List,
            },
        ],
    },
    Section {
        title: "Email",
        fields: &[
            Field {
                path: &["mail", "enabled"],
                label: "Enabled",
                help: "Polls IMAP and answers by SMTP when `serve` runs.",
                kind: Kind::Bool,
            },
            Field {
                path: &["mail", "imap_host"],
                label: "IMAP host",
                help: "e.g. imap.qq.com",
                kind: Kind::Text,
            },
            Field {
                path: &["mail", "imap_port"],
                label: "IMAP port",
                help: "993 for TLS.",
                kind: Kind::Int,
            },
            Field {
                path: &["mail", "imap_security"],
                label: "IMAP security",
                help: "none is only accepted for loopback hosts.",
                kind: Kind::Choice(SECURITY),
            },
            Field {
                path: &["mail", "smtp_host"],
                label: "SMTP host",
                help: "e.g. smtp.qq.com",
                kind: Kind::Text,
            },
            Field {
                path: &["mail", "smtp_port"],
                label: "SMTP port",
                help: "465 for TLS, 587 for STARTTLS.",
                kind: Kind::Int,
            },
            Field {
                path: &["mail", "smtp_security"],
                label: "SMTP security",
                help: "none is only accepted for loopback hosts.",
                kind: Kind::Choice(SECURITY),
            },
            Field {
                path: &["mail", "username"],
                label: "Username",
                help: "Usually the full address.",
                kind: Kind::Text,
            },
            Field {
                path: &["mail", "password"],
                label: "Password",
                help: "Many providers (QQ Mail, 163) need an app authorization code. MAIL_PASSWORD wins when set.",
                kind: Kind::Secret {
                    env: "MAIL_PASSWORD",
                    generate: false,
                },
            },
            Field {
                path: &["mail", "from"],
                label: "Reply-from address",
                help: "Reset (-) to use the username.",
                kind: Kind::Text,
            },
            Field {
                path: &["mail", "allow"],
                label: "Allowed senders",
                help: "Comma-separated addresses or @domain; required.",
                kind: Kind::List,
            },
        ],
    },
    Section {
        title: "Web search",
        fields: &[
            Field {
                path: &["search", "provider"],
                label: "Provider",
                help: "openrouter: billed per search with the OpenRouter key; searxng: your own instance; off: no web_search tool.",
                kind: Kind::Choice(&["openrouter", "searxng", "off"]),
            },
            Field {
                path: &["search", "searxng_url"],
                label: "SearXNG URL",
                help: "e.g. http://127.0.0.1:8888, with the JSON format enabled.",
                kind: Kind::Text,
            },
            Field {
                path: &["search", "model"],
                label: "Search model",
                help: "Model that runs OpenRouter searches. Reset (-) to use the main model.",
                kind: Kind::Text,
            },
        ],
    },
    Section {
        title: "Access",
        fields: &[Field {
            path: &["access", "owners"],
            label: "Owners",
            help: "Comma-separated qq:<openid> or mail:<address> treated like the terminal: every tool, /compact, identity approval.",
            kind: Kind::List,
        }],
    },
];

/// Terminal I/O, abstracted so the editor can be driven by tests.
pub struct Term<R, W> {
    input: R,
    out: W,
    /// Turn echo off while a secret is typed; only for a real terminal.
    hide_secrets: bool,
}

impl<R: BufRead, W: Write> Term<R, W> {
    /// One trimmed line, or `None` at end of input.
    fn ask(&mut self, prompt: &str) -> Result<Option<String>> {
        write!(self.out, "{prompt}")?;
        self.out.flush()?;
        let mut line = String::new();
        if self.input.read_line(&mut line)? == 0 {
            return Ok(None);
        }
        Ok(Some(line.trim().to_owned()))
    }

    fn secret(&mut self, prompt: &str) -> Result<Option<String>> {
        if !self.hide_secrets {
            return self.ask(prompt);
        }
        let _echo = EchoOff::new();
        let line = self.ask(prompt);
        writeln!(self.out)?;
        line
    }

    fn say(&mut self, text: &str) -> Result<()> {
        writeln!(self.out, "{text}")?;
        Ok(())
    }
}

/// Disables terminal echo on stdin until dropped.
struct EchoOff(Option<libc::termios>);

impl EchoOff {
    fn new() -> Self {
        // SAFETY: termios is plain data; tcgetattr fills it or fails.
        unsafe {
            let mut term: libc::termios = std::mem::zeroed();
            if libc::tcgetattr(libc::STDIN_FILENO, &mut term) != 0 {
                return Self(None);
            }
            let saved = term;
            term.c_lflag &= !libc::ECHO;
            libc::tcsetattr(libc::STDIN_FILENO, libc::TCSANOW, &term);
            Self(Some(saved))
        }
    }
}

impl Drop for EchoOff {
    fn drop(&mut self) {
        if let Some(saved) = &self.0 {
            // SAFETY: restores the attributes read in `new`.
            unsafe {
                libc::tcsetattr(libc::STDIN_FILENO, libc::TCSANOW, saved);
            }
        }
    }
}

/// Runs the editor on `path` with the process's terminal.
pub fn run(path: &Path) -> Result<()> {
    let stdin = std::io::stdin();
    let hide = unsafe { libc::isatty(libc::STDIN_FILENO) } == 1;
    let mut term = Term {
        input: stdin.lock(),
        out: std::io::stdout(),
        hide_secrets: hide,
    };
    edit(path, &mut term)
}

fn load(path: &Path) -> Result<DocumentMut> {
    match std::fs::read_to_string(path) {
        Ok(text) => text
            .parse()
            .with_context(|| format!("{} is not valid TOML; fix it by hand first", path.display())),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(DocumentMut::new()),
        Err(err) => Err(err).with_context(|| format!("cannot read {}", path.display())),
    }
}

fn parse(doc: &DocumentMut) -> Result<Config> {
    Ok(toml::from_str(&doc.to_string())?)
}

/// The menu loop: edits `path` and saves it when asked.
pub fn edit<R: BufRead, W: Write>(path: &Path, term: &mut Term<R, W>) -> Result<()> {
    let mut doc = load(path)?;
    if let Err(err) = parse(&doc) {
        term.say(&format!(
            "warning: the current file does not load ({err:#}); fix the field it names below"
        ))?;
    }
    let original = doc.to_string();
    term.say(&format!("Editing {}", path.display()))?;
    term.say("In each field: Enter keeps the value, - resets it to the default, ? explains it.")?;
    loop {
        term.say("")?;
        for (i, section) in SECTIONS.iter().enumerate() {
            term.say(&format!("  {}) {}", i + 1, section.title))?;
        }
        term.say("  s) Save and exit")?;
        term.say("  q) Quit without saving")?;
        let Some(choice) = term.ask("> ")? else {
            return finish(path, &doc, &original, term, false);
        };
        match choice.as_str() {
            "s" | "S" => return finish(path, &doc, &original, term, true),
            "q" | "Q" => return finish(path, &doc, &original, term, false),
            other => match other
                .parse::<usize>()
                .ok()
                .and_then(|n| SECTIONS.get(n.wrapping_sub(1)))
            {
                Some(section) => {
                    if !edit_section(&mut doc, section, term)? {
                        return finish(path, &doc, &original, term, false);
                    }
                }
                None => term.say("Pick a number, s or q.")?,
            },
        }
    }
}

fn finish<R: BufRead, W: Write>(
    path: &Path,
    doc: &DocumentMut,
    original: &str,
    term: &mut Term<R, W>,
    save: bool,
) -> Result<()> {
    let text = doc.to_string();
    if !save || text == original {
        term.say(if text == original {
            "No changes."
        } else {
            "Quit without saving."
        })?;
        return Ok(());
    }
    if let Err(err) = toml::from_str::<Config>(&text) {
        bail!("not saved: the result does not load: {err:#}");
    }
    write_private(path, &text)?;
    term.say(&format!("Saved {}.", path.display()))?;
    term.say("Restart a running `serve` (rc-service openclaw-rs restart) to apply it.")?;
    Ok(())
}

/// Writes atomically with mode 0600, since the file may hold secrets.
fn write_private(path: &Path, text: &str) -> Result<()> {
    use std::os::unix::fs::OpenOptionsExt;
    let dir = path
        .parent()
        .filter(|d| !d.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    std::fs::create_dir_all(dir).with_context(|| format!("cannot create {}", dir.display()))?;
    let tmp = dir.join(format!(
        ".{}.tmp",
        path.file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("config.toml")
    ));
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(&tmp)
        .with_context(|| format!("cannot write {}", tmp.display()))?;
    file.write_all(text.as_bytes())?;
    file.sync_all()?;
    std::fs::rename(&tmp, path).with_context(|| format!("cannot replace {}", path.display()))?;
    Ok(())
}

/// Walks one section's fields. `false` when input ended.
fn edit_section<R: BufRead, W: Write>(
    doc: &mut DocumentMut,
    section: &Section,
    term: &mut Term<R, W>,
) -> Result<bool> {
    term.say(&format!("\n[{}]", section.title))?;
    let mut index = 0;
    while let Some(field) = section.fields.get(index) {
        let current = describe(doc, field);
        let options = match field.kind {
            Kind::Choice(choices) => format!(
                " ({})",
                choices
                    .iter()
                    .enumerate()
                    .map(|(i, c)| format!("{}={c}", i + 1))
                    .collect::<Vec<_>>()
                    .join(" ")
            ),
            Kind::Bool => " (y/n)".into(),
            _ => String::new(),
        };
        let prompt = format!("{}{options} [{current}]: ", field.label);
        let input = match field.kind {
            Kind::Secret { .. } => term.secret(&prompt)?,
            _ => term.ask(&prompt)?,
        };
        let Some(input) = input else {
            return Ok(false);
        };
        match input.as_str() {
            "" => {}
            "?" => {
                term.say(&format!("  {}", field.help))?;
                continue;
            }
            "-" => {
                let before = doc.clone();
                remove(doc, field.path);
                if let Err(err) = parse(doc) {
                    *doc = before;
                    term.say(&format!("  cannot reset: {err:#}"))?;
                    continue;
                }
            }
            text => {
                let value = match value_for(field.kind, text) {
                    Ok(value) => value,
                    Err(err) => {
                        term.say(&format!("  {err:#}"))?;
                        continue;
                    }
                };
                let before = doc.clone();
                set(doc, field.path, value);
                if let Err(err) = parse(doc) {
                    *doc = before;
                    term.say(&format!("  not accepted: {err:#}"))?;
                    continue;
                }
            }
        }
        index += 1;
    }
    Ok(true)
}

fn value_for(kind: Kind, text: &str) -> Result<Value> {
    Ok(match kind {
        Kind::Text => Value::from(text),
        Kind::Int => {
            let n: u32 = text
                .replace('_', "")
                .parse()
                .with_context(|| format!("{text:?} is not a whole number"))?;
            Value::from(i64::from(n))
        }
        Kind::Bool => match text.to_ascii_lowercase().as_str() {
            "y" | "yes" | "true" | "on" | "1" => Value::from(true),
            "n" | "no" | "false" | "off" | "0" => Value::from(false),
            _ => bail!("answer y or n"),
        },
        Kind::Choice(choices) => {
            let pick = text
                .parse::<usize>()
                .ok()
                .and_then(|n| choices.get(n.wrapping_sub(1)))
                .or_else(|| choices.iter().find(|c| c.eq_ignore_ascii_case(text)));
            match pick {
                Some(choice) => Value::from(*choice),
                None => bail!("pick one of: {}", choices.join(", ")),
            }
        }
        Kind::List => {
            let mut array = Array::new();
            for item in text.split(',').map(str::trim).filter(|s| !s.is_empty()) {
                array.push(item);
            }
            Value::Array(array)
        }
        Kind::Secret { generate, .. } => {
            if generate && text == "+" {
                Value::from(random_token()?)
            } else {
                Value::from(text)
            }
        }
    })
}

/// 24 random bytes, hex-encoded.
fn random_token() -> Result<String> {
    use std::io::Read;
    let mut bytes = [0u8; 24];
    std::fs::File::open("/dev/urandom")
        .and_then(|mut f| f.read_exact(&mut bytes))
        .context("cannot read /dev/urandom")?;
    Ok(bytes.iter().map(|b| format!("{b:02x}")).collect())
}

/// Sets the value at `path`, keeping the old value's surrounding comments.
fn set(doc: &mut DocumentMut, path: &[&str], mut value: Value) {
    let (key, tables) = path.split_last().expect("paths are never empty");
    let mut item = doc.as_item_mut();
    for name in tables {
        if !item.get(*name).is_some_and(Item::is_table_like) {
            item[*name] = toml_edit::table();
        }
        item = &mut item[*name];
    }
    if let Some(old) = item.get(*key).and_then(Item::as_value) {
        *value.decor_mut() = old.decor().clone();
    }
    item[*key] = Item::Value(value);
}

fn remove(doc: &mut DocumentMut, path: &[&str]) {
    let (key, tables) = path.split_last().expect("paths are never empty");
    let mut item = doc.as_item_mut();
    for name in tables {
        match item.get_mut(*name) {
            Some(next) => item = next,
            None => return,
        }
    }
    if let Some(table) = item.as_table_like_mut() {
        table.remove(key);
    }
}

/// The value in effect for `field`, defaults included; secrets are never shown.
fn describe(doc: &DocumentMut, field: &Field) -> String {
    let effective = parse(doc)
        .ok()
        .and_then(|config| toml::Value::try_from(&config).ok());
    let mut value = effective.as_ref();
    for name in field.path {
        value = value.and_then(|v| v.get(*name));
    }
    if let Kind::Secret { env, .. } = field.kind {
        let from_env = std::env::var(env).is_ok_and(|v| !v.trim().is_empty());
        let in_file = value
            .and_then(toml::Value::as_str)
            .is_some_and(|v| !v.is_empty());
        return match (from_env, in_file) {
            (true, _) => format!("from ${env}"),
            (false, true) => "set in this file".into(),
            (false, false) => "not set".into(),
        };
    }
    match value {
        None => "default".into(),
        Some(toml::Value::String(s)) if s.is_empty() => "empty".into(),
        Some(toml::Value::String(s)) => s.clone(),
        Some(toml::Value::Array(items)) if items.is_empty() => "none".into(),
        Some(toml::Value::Array(items)) => items
            .iter()
            .map(|i| i.as_str().map_or_else(|| i.to_string(), str::to_owned))
            .collect::<Vec<_>>()
            .join(", "),
        Some(other) => other.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn drive(path: &Path, script: &str) -> String {
        let mut term = Term {
            input: script.as_bytes(),
            out: Vec::new(),
            hide_secrets: false,
        };
        edit(path, &mut term).unwrap();
        String::from_utf8(term.out).unwrap()
    }

    #[test]
    fn edits_keep_comments_and_unknown_keys() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(
            &path,
            "# my notes\n[model]\nmodel = \"x/old\"  # keep me\n\n[custom]\nthing = 1\n",
        )
        .unwrap();
        // Model: new id, keep key, two fallbacks, keep retries, cache by number,
        // a bad then a good budget, reset summary model, keep max steps.
        let script = "1\na/new\n\nb/one, c/two ,\n\n3\nlots\n32000\n-\n\ns\n";
        let out = drive(&path, script);
        assert!(out.contains("\"lots\" is not a whole number"), "{out}");
        assert!(out.contains("Saved"), "{out}");
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("# my notes"));
        assert!(text.contains("model = \"a/new\"  # keep me"), "{text}");
        assert!(text.contains("[custom]\nthing = 1"));
        let config: Config = toml::from_str(&text).unwrap();
        assert_eq!(config.model.model, "a/new");
        assert_eq!(config.model.fallbacks, ["b/one", "c/two"]);
        assert_eq!(config.model.prompt_cache, crate::config::PromptCache::Off);
        assert_eq!(config.agent.context_tokens, 32000);
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
    }

    #[test]
    fn shows_current_values_and_never_secrets() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(
            &path,
            "[qq]\napp_secret = \"hunter2\"\nallow = [\"A\", \"B\"]\n",
        )
        .unwrap();
        let out = drive(&path, "4\n?\n\n\n\n\nq\n");
        assert!(out.contains("Enabled (y/n) [false]"), "{out}");
        assert!(out.contains("Connects to QQ over WebSocket"), "{out}");
        assert!(out.contains("Allowed openids [A, B]"), "{out}");
        assert!(!out.contains("hunter2"), "{out}");
        assert!(out.contains("No changes."));
    }

    #[test]
    fn quitting_or_running_out_of_input_saves_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        let out = drive(&path, "3\n0.0.0.0:1\n+\nq\n");
        assert!(out.contains("Quit without saving."), "{out}");
        assert!(!path.exists());
        drive(&path, "4\ny\n");
        assert!(!path.exists());
    }

    #[test]
    fn generates_a_token_and_sets_nested_tables() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        drive(&path, "3\n\n+\n2\n\n\n\n\nopenrouter\ns\n");
        let config: Config = toml::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        let token = config.gateway.token.unwrap();
        assert_eq!(token.len(), 48);
        assert!(token.chars().all(|c| c.is_ascii_hexdigit()));
        assert_eq!(
            config.tools.review.provider,
            crate::config::ReviewProvider::Openrouter
        );
    }

    #[test]
    fn rejects_a_choice_outside_the_list() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        let out = drive(&path, "2\nmaybe\n9\nask\n\n\n\n\ns\n");
        assert_eq!(
            out.matches("pick one of: allow, ask, deny").count(),
            2,
            "{out}"
        );
        assert!(
            std::fs::read_to_string(&path)
                .unwrap()
                .contains("shell = \"ask\"")
        );
    }
}
