//! MCP client: tools from Model Context Protocol servers, offered to the
//! model next to the built-in ones. A server is either a local program
//! spoken to over stdin/stdout (`command`), or a Streamable HTTP endpoint
//! (`url`). Nothing is bundled: the servers are whatever the host has.
//!
//! Each server's tools appear as `<server>__<tool>`. They need the `mcp`
//! capability (owners by default), and a server can ask for approval of
//! every call or expose only some of its tools.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex as SyncMutex, RwLock};
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use base64::Engine;
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin};
use tokio::sync::{Mutex, oneshot};

use crate::config::{McpConfig, McpServerConfig};
use crate::llm::ToolSpec;

/// Protocol revision asked for; servers answer with the one they speak.
const PROTOCOL_VERSION: &str = "2025-06-18";
/// Separates the server from the tool in the name the model sees.
pub const SEPARATOR: &str = "__";
/// Longest tool name providers accept.
const MAX_NAME: usize = 64;
const MAX_DESCRIPTION: usize = 1024;
/// Time a server gets to start and list its tools.
const START_TIMEOUT: Duration = Duration::from_secs(30);

/// Whether a tool name belongs to an MCP server (built-in names have no `__`).
pub fn is_mcp_tool(name: &str) -> bool {
    name.contains(SEPARATOR)
}

pub struct Mcp {
    servers: Vec<Arc<Server>>,
    workspace: PathBuf,
}

struct Server {
    name: String,
    config: McpServerConfig,
    conn: Mutex<Option<Arc<Conn>>>,
    tools: RwLock<Vec<RemoteTool>>,
    /// Why the last start failed, for `doctor` and the log.
    error: SyncMutex<Option<String>>,
}

#[derive(Debug, Clone, PartialEq)]
struct RemoteTool {
    /// `<server>__<tool>`, as the model calls it.
    exposed: String,
    /// The server's own name for it.
    name: String,
    description: String,
    schema: Value,
}

/// One server's state, for `doctor`.
pub struct Status {
    pub name: String,
    pub tools: Vec<String>,
    pub error: Option<String>,
}

impl Mcp {
    /// Starts every enabled server, at the same time. A server that fails is
    /// logged and left out until a call to one of its tools retries it.
    pub async fn connect(config: &McpConfig, workspace: &Path) -> Self {
        let servers: Vec<Arc<Server>> = config
            .servers
            .iter()
            .filter(|(_, c)| c.enabled)
            .map(|(name, c)| {
                Arc::new(Server {
                    name: clean_name(name),
                    config: c.clone(),
                    conn: Mutex::new(None),
                    tools: RwLock::new(Vec::new()),
                    error: SyncMutex::new(None),
                })
            })
            .collect();
        let starts = servers.iter().map(|server| {
            let server = server.clone();
            async move {
                match server.start().await {
                    Ok(count) => eprintln!("mcp: {} ready with {count} tool(s)", server.name),
                    Err(err) => eprintln!("mcp: {} did not start: {err:#}", server.name),
                }
            }
        });
        futures_util::future::join_all(starts).await;
        Self {
            servers,
            workspace: workspace.to_owned(),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.servers.is_empty()
    }

    pub fn specs(&self) -> Vec<ToolSpec> {
        self.servers
            .iter()
            .flat_map(|s| s.tools.read().unwrap().clone())
            .map(|t| ToolSpec::function(&t.exposed, &t.description, t.schema))
            .collect()
    }

    pub fn statuses(&self) -> Vec<Status> {
        self.servers
            .iter()
            .map(|s| Status {
                name: s.name.clone(),
                tools: s
                    .tools
                    .read()
                    .unwrap()
                    .iter()
                    .map(|t| t.name.clone())
                    .collect(),
                error: s.error.lock().unwrap().clone(),
            })
            .collect()
    }

    /// The server a tool belongs to and its approval setting, `None` when no
    /// server has it.
    pub fn permission(&self, exposed: &str) -> Option<crate::config::Permission> {
        self.server_of(exposed).map(|(s, _)| s.config.permission)
    }

    fn server_of(&self, exposed: &str) -> Option<(&Arc<Server>, String)> {
        let (server, _) = exposed.split_once(SEPARATOR)?;
        let server = self.servers.iter().find(|s| s.name == server)?;
        let tool = server
            .tools
            .read()
            .unwrap()
            .iter()
            .find(|t| t.exposed == exposed)
            .map(|t| t.name.clone())?;
        Some((server, tool))
    }

    /// Runs a tool; the text the model gets back, or an error message.
    pub async fn call(&self, exposed: &str, arguments: Value) -> Result<String> {
        let (server, tool) = self
            .server_of(exposed)
            .with_context(|| format!("no MCP server offers {exposed}"))?;
        let result = server.call(&tool, arguments).await?;
        Ok(render(&result, &self.workspace, &server.name))
    }
}

impl Server {
    /// Connects, initializes and lists the tools; returns how many there are.
    async fn start(&self) -> Result<usize> {
        let started = tokio::time::timeout(START_TIMEOUT, async {
            let conn = Arc::new(Conn::open(&self.name, &self.config).await?);
            conn.initialize().await?;
            let tools = conn.list_tools().await?;
            anyhow::Ok((conn, tools))
        })
        .await
        .unwrap_or_else(|_| Err(anyhow!("no answer within {} s", START_TIMEOUT.as_secs())));
        match started {
            Ok((conn, tools)) => {
                let tools = self.expose(tools);
                let count = tools.len();
                *self.tools.write().unwrap() = tools;
                *self.conn.lock().await = Some(conn);
                *self.error.lock().unwrap() = None;
                Ok(count)
            }
            Err(err) => {
                *self.error.lock().unwrap() = Some(format!("{err:#}"));
                Err(err)
            }
        }
    }

    /// The tools the model is offered: those in `tools` when it is set,
    /// under unique `<server>__<tool>` names.
    fn expose(&self, listed: Vec<Value>) -> Vec<RemoteTool> {
        let mut out: Vec<RemoteTool> = Vec::new();
        for tool in listed {
            let Some(name) = tool.get("name").and_then(Value::as_str) else {
                continue;
            };
            if !self.config.tools.is_empty() && !self.config.tools.iter().any(|t| t == name) {
                continue;
            }
            let mut exposed = format!("{}{SEPARATOR}{}", self.name, clean_name(name));
            exposed.truncate(MAX_NAME);
            if out.iter().any(|t| t.exposed == exposed) {
                eprintln!(
                    "mcp: {} has two tools named {exposed}; the second is left out",
                    self.name
                );
                continue;
            }
            let description = tool
                .get("description")
                .and_then(Value::as_str)
                .unwrap_or("")
                .trim();
            let mut description = format!("[{} MCP server] {description}", self.name);
            if description.chars().count() > MAX_DESCRIPTION {
                description = description.chars().take(MAX_DESCRIPTION).collect();
            }
            let mut schema = tool
                .get("inputSchema")
                .filter(|s| s.is_object())
                .cloned()
                .unwrap_or_else(|| json!({}));
            if schema.get("type").is_none() {
                schema["type"] = json!("object");
            }
            if schema.get("properties").is_none() {
                schema["properties"] = json!({});
            }
            out.push(RemoteTool {
                exposed,
                name: name.to_owned(),
                description,
                schema,
            });
        }
        out
    }

    async fn call(&self, tool: &str, arguments: Value) -> Result<Value> {
        let conn = self.conn.lock().await.clone();
        let conn = match conn {
            Some(conn) if conn.alive() => conn,
            // Exited, or never started: one new attempt.
            _ => {
                self.start()
                    .await
                    .with_context(|| format!("MCP server {} is not running", self.name))?;
                self.conn
                    .lock()
                    .await
                    .clone()
                    .context("MCP server did not start")?
            }
        };
        let params = json!({"name": tool, "arguments": arguments});
        let timeout = Duration::from_secs(self.config.timeout_secs.max(1));
        let result = tokio::time::timeout(timeout, conn.request("tools/call", params))
            .await
            .map_err(|_| {
                anyhow!(
                    "{} gave no answer within {} s",
                    self.name,
                    timeout.as_secs()
                )
            })??;
        if conn.tools_changed.swap(false, Ordering::SeqCst)
            && let Ok(tools) = conn.list_tools().await
        {
            *self.tools.write().unwrap() = self.expose(tools);
        }
        Ok(result)
    }
}

/// `[A-Za-z0-9_-]` only, as tool names allow; `__` would split wrongly.
fn clean_name(name: &str) -> String {
    let mut out: String = name
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect();
    while out.contains(SEPARATOR) {
        out = out.replace(SEPARATOR, "_");
    }
    if out.is_empty() { "mcp".into() } else { out }
}

/// `${NAME}` anywhere, or `$NAME` as the whole value, reads the Gateway's
/// environment, so secrets can stay in `/etc/conf.d/openclaw-rs`
/// (`Authorization = "Bearer ${GITHUB_TOKEN}"`).
fn expand(value: &str) -> String {
    let is_name =
        |n: &str| !n.is_empty() && n.chars().all(|c| c.is_ascii_alphanumeric() || c == '_');
    if let Some(name) = value.strip_prefix('$').filter(|n| is_name(n)) {
        return std::env::var(name).unwrap_or_default();
    }
    let mut out = String::new();
    let mut rest = value;
    while let Some(at) = rest.find("${") {
        out.push_str(&rest[..at]);
        let after = &rest[at + 2..];
        match after.find('}').filter(|&end| is_name(&after[..end])) {
            Some(end) => {
                out.push_str(&std::env::var(&after[..end]).unwrap_or_default());
                rest = &after[end + 1..];
            }
            None => {
                out.push_str("${");
                rest = after;
            }
        }
    }
    out.push_str(rest);
    out
}

type Pending = Arc<SyncMutex<HashMap<u64, oneshot::Sender<Result<Value, String>>>>>;

/// A JSON-RPC connection to one server.
struct Conn {
    transport: Transport,
    next_id: AtomicU64,
    tools_changed: Arc<AtomicBool>,
}

enum Transport {
    Stdio {
        stdin: Arc<Mutex<ChildStdin>>,
        pending: Pending,
        alive: Arc<AtomicBool>,
        _process: ServerProcess,
    },
    Http {
        http: reqwest::Client,
        url: String,
        headers: Vec<(String, String)>,
        session: SyncMutex<Option<String>>,
        protocol: SyncMutex<Option<String>>,
    },
}

impl Conn {
    async fn open(name: &str, config: &McpServerConfig) -> Result<Self> {
        let tools_changed = Arc::new(AtomicBool::new(false));
        let transport = match (config.command.as_deref(), config.url.as_deref()) {
            (Some(command), None) if !command.trim().is_empty() => {
                spawn(name, command, config, tools_changed.clone())?
            }
            (None, Some(url)) if !url.trim().is_empty() => Transport::Http {
                http: reqwest::Client::builder()
                    .connect_timeout(Duration::from_secs(20))
                    .build()?,
                url: url.trim().to_owned(),
                headers: config
                    .headers
                    .iter()
                    .map(|(k, v)| (k.clone(), expand(v)))
                    .collect(),
                session: SyncMutex::new(None),
                protocol: SyncMutex::new(None),
            },
            (Some(_), Some(_)) => bail!("set either command or url, not both"),
            _ => bail!("needs a command (a local server) or a url (a remote one)"),
        };
        Ok(Self {
            transport,
            next_id: AtomicU64::new(1),
            tools_changed,
        })
    }

    fn alive(&self) -> bool {
        match &self.transport {
            Transport::Stdio { alive, .. } => alive.load(Ordering::SeqCst),
            Transport::Http { .. } => true,
        }
    }

    async fn initialize(&self) -> Result<()> {
        let result = self
            .request(
                "initialize",
                json!({
                    "protocolVersion": PROTOCOL_VERSION,
                    "capabilities": {},
                    "clientInfo": {"name": "openclaw-rs", "version": env!("CARGO_PKG_VERSION")},
                }),
            )
            .await
            .context("initialize failed")?;
        if let Transport::Http { protocol, .. } = &self.transport {
            *protocol.lock().unwrap() = result
                .get("protocolVersion")
                .and_then(Value::as_str)
                .map(str::to_owned);
        }
        self.notify("notifications/initialized", json!({})).await
    }

    async fn list_tools(&self) -> Result<Vec<Value>> {
        let mut tools = Vec::new();
        let mut cursor: Option<String> = None;
        // A bound, in case a server keeps returning a cursor.
        for _ in 0..20 {
            let params = match &cursor {
                Some(c) => json!({"cursor": c}),
                None => json!({}),
            };
            let page = self
                .request("tools/list", params)
                .await
                .context("tools/list failed")?;
            tools.extend(
                page.get("tools")
                    .and_then(Value::as_array)
                    .cloned()
                    .unwrap_or_default(),
            );
            cursor = page
                .get("nextCursor")
                .and_then(Value::as_str)
                .filter(|c| !c.is_empty())
                .map(str::to_owned);
            if cursor.is_none() {
                break;
            }
        }
        Ok(tools)
    }

    async fn request(&self, method: &str, params: Value) -> Result<Value> {
        let id = self.next_id.fetch_add(1, Ordering::SeqCst);
        let message = json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params});
        match &self.transport {
            Transport::Stdio {
                stdin,
                pending,
                alive,
                ..
            } => {
                if !alive.load(Ordering::SeqCst) {
                    bail!("the server has exited");
                }
                let (tx, rx) = oneshot::channel();
                pending.lock().unwrap().insert(id, tx);
                if let Err(err) = write_line(stdin, &message).await {
                    pending.lock().unwrap().remove(&id);
                    return Err(err);
                }
                match rx.await {
                    Ok(Ok(result)) => Ok(result),
                    Ok(Err(error)) => bail!("{error}"),
                    Err(_) => bail!("the server has exited"),
                }
            }
            Transport::Http { .. } => {
                let reply = self.post(&message).await?;
                let reply = reply.with_context(|| format!("no answer to {method}"))?;
                response_result(&reply).map_err(|e| anyhow!(e))
            }
        }
    }

    async fn notify(&self, method: &str, params: Value) -> Result<()> {
        let message = json!({"jsonrpc": "2.0", "method": method, "params": params});
        match &self.transport {
            Transport::Stdio { stdin, .. } => write_line(stdin, &message).await,
            Transport::Http { .. } => self.post(&message).await.map(|_| ()),
        }
    }

    /// Streamable HTTP: one POST per message; the answer comes as JSON or as
    /// an event stream that carries it.
    async fn post(&self, message: &Value) -> Result<Option<Value>> {
        let Transport::Http {
            http,
            url,
            headers,
            session,
            protocol,
        } = &self.transport
        else {
            unreachable!("only for HTTP")
        };
        let mut request = http
            .post(url)
            .header(
                reqwest::header::ACCEPT,
                "application/json, text/event-stream",
            )
            .json(message);
        for (k, v) in headers {
            request = request.header(k, v);
        }
        if let Some(id) = session.lock().unwrap().clone() {
            request = request.header("Mcp-Session-Id", id);
        }
        if let Some(version) = protocol.lock().unwrap().clone() {
            request = request.header("MCP-Protocol-Version", version);
        }
        let mut response = request.send().await.context("request failed")?;
        if let Some(id) = response
            .headers()
            .get("mcp-session-id")
            .and_then(|v| v.to_str().ok())
        {
            *session.lock().unwrap() = Some(id.to_owned());
        }
        let status = response.status();
        if !status.is_success() {
            let text: String = response
                .text()
                .await
                .unwrap_or_default()
                .chars()
                .take(300)
                .collect();
            bail!("HTTP {status}: {text}");
        }
        let Some(id) = message.get("id").cloned() else {
            return Ok(None);
        };
        let stream = response
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .is_some_and(|t| t.starts_with("text/event-stream"));
        if !stream {
            let body = response.text().await?;
            return Ok(Some(
                serde_json::from_str(&body).context("answer is not JSON")?,
            ));
        }
        let mut buffer = String::new();
        while let Some(chunk) = response.chunk().await? {
            buffer.push_str(&String::from_utf8_lossy(&chunk));
            while let Some(end) = buffer.find("\n\n") {
                let event: String = buffer.drain(..end + 2).collect();
                if let Some(reply) = sse_message(&event)
                    && reply.get("id") == Some(&id)
                {
                    return Ok(Some(reply));
                }
            }
        }
        Ok(sse_message(&buffer).filter(|r| r.get("id") == Some(&id)))
    }
}

/// The JSON message in one server-sent event's `data:` lines.
fn sse_message(event: &str) -> Option<Value> {
    let event = event.replace("\r\n", "\n");
    let data: Vec<&str> = event
        .lines()
        .filter_map(|l| l.strip_prefix("data:"))
        .map(|d| d.strip_prefix(' ').unwrap_or(d))
        .collect();
    if data.is_empty() {
        return None;
    }
    serde_json::from_str(&data.join("\n")).ok()
}

fn response_result(reply: &Value) -> Result<Value, String> {
    if let Some(error) = reply.get("error") {
        let message = error
            .get("message")
            .and_then(Value::as_str)
            .unwrap_or("unknown error");
        return Err(match error.get("code").and_then(Value::as_i64) {
            Some(code) => format!("{message} (code {code})"),
            None => message.to_owned(),
        });
    }
    Ok(reply.get("result").cloned().unwrap_or(Value::Null))
}

async fn write_line(stdin: &Mutex<ChildStdin>, message: &Value) -> Result<()> {
    let mut line = serde_json::to_vec(message)?;
    line.push(b'\n');
    let mut stdin = stdin.lock().await;
    stdin
        .write_all(&line)
        .await
        .context("cannot write to the server")?;
    stdin.flush().await.context("cannot write to the server")
}

fn spawn(
    name: &str,
    command: &str,
    config: &McpServerConfig,
    tools_changed: Arc<AtomicBool>,
) -> Result<Transport> {
    let mut cmd = tokio::process::Command::new(command.trim());
    cmd.args(&config.args)
        .envs(config.env.iter().map(|(k, v)| (k, expand(v))))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        // Own process group, so ending it also ends what it started (npx starts node).
        .process_group(0);
    if let Some(dir) = &config.cwd {
        cmd.current_dir(dir);
    }
    #[cfg(target_os = "linux")]
    // SAFETY: prctl is async-signal-safe; it only asks the kernel to end the
    // server if the Gateway dies without stopping it.
    unsafe {
        cmd.pre_exec(|| {
            libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGTERM);
            Ok(())
        });
    }
    let mut child: Child = cmd
        .spawn()
        .with_context(|| format!("cannot start {command} (is it installed and on PATH?)"))?;
    let process = ServerProcess(child.id());
    let stdin = Arc::new(Mutex::new(child.stdin.take().expect("piped stdin")));
    let stdout = child.stdout.take().expect("piped stdout");
    let stderr = child.stderr.take().expect("piped stderr");
    let pending: Pending = Arc::new(SyncMutex::new(HashMap::new()));
    let alive = Arc::new(AtomicBool::new(true));

    let label = name.to_owned();
    tokio::spawn(async move {
        let mut lines = BufReader::new(stderr).lines();
        while let Ok(Some(line)) = lines.next_line().await {
            eprintln!("mcp {label}: {line}");
        }
    });
    let (reader_pending, reader_alive, reader_stdin) =
        (pending.clone(), alive.clone(), stdin.clone());
    let label = name.to_owned();
    tokio::spawn(async move {
        let mut lines = BufReader::new(stdout).lines();
        while let Ok(Some(line)) = lines.next_line().await {
            let Ok(message) = serde_json::from_str::<Value>(&line) else {
                if !line.trim().is_empty() {
                    eprintln!("mcp {label} (not JSON): {line}");
                }
                continue;
            };
            handle_message(&message, &reader_pending, &reader_stdin, &tools_changed).await;
        }
        reader_alive.store(false, Ordering::SeqCst);
        for (_, tx) in reader_pending.lock().unwrap().drain() {
            let _ = tx.send(Err("the server has exited".into()));
        }
        // Reap it, so it does not linger as a zombie.
        let _ = child.wait().await;
        eprintln!("mcp {label}: exited");
    });
    Ok(Transport::Stdio {
        stdin,
        pending,
        alive,
        _process: process,
    })
}

/// A response goes to its waiting request; a request from the server is
/// answered (only `ping` is supported); notifications are noted.
async fn handle_message(
    message: &Value,
    pending: &Pending,
    stdin: &Mutex<ChildStdin>,
    tools_changed: &AtomicBool,
) {
    let method = message.get("method").and_then(Value::as_str);
    match (method, message.get("id")) {
        (None, Some(id)) => {
            let Some(id) = id.as_u64() else { return };
            if let Some(tx) = pending.lock().unwrap().remove(&id) {
                let _ = tx.send(response_result(message));
            }
        }
        (Some(method), Some(id)) => {
            let reply = if method == "ping" {
                json!({"jsonrpc": "2.0", "id": id, "result": {}})
            } else {
                json!({"jsonrpc": "2.0", "id": id, "error": {
                    "code": -32601, "message": format!("{method} is not supported by this client")
                }})
            };
            let _ = write_line(stdin, &reply).await;
        }
        (Some("notifications/tools/list_changed"), None) => {
            tools_changed.store(true, Ordering::SeqCst);
        }
        _ => {}
    }
}

/// Ends the server's whole process group when the connection goes.
struct ServerProcess(Option<u32>);

impl Drop for ServerProcess {
    fn drop(&mut self) {
        if let Some(pid) = self.0 {
            // SAFETY: signalling the process group we created; failure is harmless.
            unsafe { libc::kill(-(pid as i32), libc::SIGTERM) };
        }
    }
}

/// A `tools/call` result as text for the model. Images, audio and binary
/// resources are saved under `mcp/` in the workspace; the first image is
/// shown to the model during the turn.
fn render(result: &Value, workspace: &Path, server: &str) -> String {
    let mut parts = Vec::new();
    let mut image = None;
    let mut saved = 0;
    let mut save = |data: &str, mime: &str| -> Option<String> {
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(data)
            .ok()?;
        saved += 1;
        let ext = crate::attachments::extension_for(mime);
        let path = format!(
            "mcp/{server}-{}-{saved}.{ext}",
            chrono::Local::now().format("%Y%m%d-%H%M%S")
        );
        let full = workspace.join(&path);
        std::fs::create_dir_all(full.parent()?).ok()?;
        std::fs::write(&full, bytes).ok()?;
        Some(path)
    };
    for item in result
        .get("content")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        let field = |k: &str| item.get(k).and_then(Value::as_str).unwrap_or("");
        match field("type") {
            "text" => parts.push(field("text").to_owned()),
            kind @ ("image" | "audio") => match save(field("data"), field("mimeType")) {
                Some(path) => {
                    if kind == "image" && image.is_none() {
                        image = Some(path.clone());
                    }
                    parts.push(format!("[{kind} saved as {path}]"));
                }
                None => parts.push(format!("[{kind} that could not be saved]")),
            },
            "resource" => {
                let resource = item.get("resource").cloned().unwrap_or_default();
                let uri = resource.get("uri").and_then(Value::as_str).unwrap_or("");
                if let Some(text) = resource.get("text").and_then(Value::as_str) {
                    parts.push(format!("[resource {uri}]\n{text}"));
                } else if let Some(blob) = resource.get("blob").and_then(Value::as_str) {
                    let mime = resource
                        .get("mimeType")
                        .and_then(Value::as_str)
                        .unwrap_or("");
                    match save(blob, mime) {
                        Some(path) => parts.push(format!("[resource {uri} saved as {path}]")),
                        None => parts.push(format!("[resource {uri} that could not be saved]")),
                    }
                }
            }
            "resource_link" => parts.push(format!("[link: {} {}]", field("uri"), field("name"))),
            _ => {}
        }
    }
    if parts.is_empty()
        && let Some(structured) = result.get("structuredContent")
    {
        parts.push(serde_json::to_string_pretty(structured).unwrap_or_default());
    }
    let mut out = parts.join("\n");
    if out.trim().is_empty() {
        out = "(no output)".into();
    }
    if result.get("isError").and_then(Value::as_bool) == Some(true) {
        out = format!("error: {out}");
    }
    match image {
        Some(path) => format!("{}\n{out}", crate::attachments::tool_image_line(&path)),
        None => out,
    }
}

#[cfg(test)]
pub fn stdio_server(command: &str, args: &[&str]) -> McpServerConfig {
    McpServerConfig {
        command: Some(command.into()),
        args: args.iter().map(|a| a.to_string()).collect(),
        ..McpServerConfig::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    /// A small MCP server as a Python script; the test is skipped without python3.
    const SERVER: &str = r#"
import json, sys
for line in sys.stdin:
    msg = json.loads(line)
    method, mid = msg.get("method"), msg.get("id")
    if mid is None:
        continue
    if method == "initialize":
        result = {"protocolVersion": "2025-06-18", "capabilities": {"tools": {}},
                  "serverInfo": {"name": "fake", "version": "1"}}
    elif method == "tools/list":
        if msg["params"].get("cursor") is None:
            result = {"tools": [{"name": "echo", "description": "Echo text",
                       "inputSchema": {"type": "object", "properties": {"text": {"type": "string"}}}}],
                      "nextCursor": "2"}
        else:
            result = {"tools": [{"name": "dot.name", "inputSchema": {}},
                                {"name": "hidden"}, {"name": "crash"}]}
    elif method == "tools/call":
        name, args = msg["params"]["name"], msg["params"]["arguments"]
        if name == "crash":
            sys.exit(3)
        if name == "echo":
            result = {"content": [{"type": "text", "text": "you said " + args.get("text", "")},
                                  {"type": "image", "mimeType": "image/png", "data": "iVBORw0KGgo="}]}
        else:
            result = {"content": [], "structuredContent": {"ok": True}, "isError": True}
    else:
        print(json.dumps({"jsonrpc": "2.0", "id": mid, "error": {"code": -32601, "message": "nope"}}), flush=True)
        continue
    print(json.dumps({"jsonrpc": "2.0", "id": mid, "result": result}), flush=True)
"#;

    fn python() -> Option<&'static str> {
        std::process::Command::new("python3")
            .arg("-c")
            .arg("")
            .status()
            .ok()
            .filter(|s| s.success())
            .map(|_| "python3")
    }

    fn config(server: McpServerConfig) -> McpConfig {
        McpConfig {
            servers: BTreeMap::from([("my.fake".to_owned(), server)]),
        }
    }

    #[tokio::test]
    async fn stdio_servers_offer_and_run_tools() {
        let Some(python) = python() else {
            eprintln!("skipped: no python3");
            return;
        };
        let dir = tempfile::tempdir().unwrap();
        let mut server = stdio_server(python, &["-c", SERVER]);
        server.tools = vec!["echo".into(), "dot.name".into(), "crash".into()];
        let mcp = Mcp::connect(&config(server), dir.path()).await;
        let names: Vec<String> = mcp.specs().into_iter().map(|s| s.function.name).collect();
        assert_eq!(
            names,
            ["my_fake__echo", "my_fake__dot_name", "my_fake__crash"]
        );
        let spec = &mcp.specs()[1];
        assert_eq!(spec.function.parameters["type"], "object");
        assert!(
            spec.function
                .description
                .starts_with("[my_fake MCP server]")
        );

        let out = mcp
            .call("my_fake__echo", json!({"text": "hi"}))
            .await
            .unwrap();
        let first = out.lines().next().unwrap();
        let image = crate::attachments::tool_image(first).unwrap();
        assert!(
            image.starts_with("mcp/my_fake-") && image.ends_with(".png"),
            "{out}"
        );
        assert!(dir.path().join(image).is_file());
        assert!(out.contains("you said hi"), "{out}");

        let out = mcp.call("my_fake__dot_name", json!({})).await.unwrap();
        assert_eq!(out, "error: {\n  \"ok\": true\n}");

        // A server that dies is started again for the next call.
        let err = mcp.call("my_fake__crash", json!({})).await.unwrap_err();
        assert!(err.to_string().contains("exited"), "{err:#}");
        let out = mcp
            .call("my_fake__echo", json!({"text": "again"}))
            .await
            .unwrap();
        assert!(out.contains("you said again"), "{out}");
        assert!(mcp.call("my_fake__hidden", json!({})).await.is_err());
        assert!(mcp.call("other__echo", json!({})).await.is_err());
    }

    #[tokio::test]
    async fn a_server_that_cannot_start_is_reported() {
        let dir = tempfile::tempdir().unwrap();
        let mcp = Mcp::connect(
            &config(stdio_server("/nonexistent/mcp-server", &[])),
            dir.path(),
        )
        .await;
        assert!(mcp.specs().is_empty());
        let status = &mcp.statuses()[0];
        assert!(
            status.error.as_deref().unwrap().contains("cannot start"),
            "{:?}",
            status.error
        );
        let mut both = stdio_server("x", &[]);
        both.url = Some("http://x".into());
        let mcp = Mcp::connect(&config(both), dir.path()).await;
        assert!(
            mcp.statuses()[0]
                .error
                .as_deref()
                .unwrap()
                .contains("not both")
        );
    }

    #[tokio::test]
    async fn http_servers_answer_in_json_or_event_streams() {
        use axum::{Json, Router, http::HeaderMap, response::IntoResponse, routing::post};
        let app = Router::new().route(
            "/mcp",
            post(|headers: HeaderMap, Json(msg): Json<Value>| async move {
                let id = msg.get("id").cloned();
                let method = msg["method"].as_str().unwrap_or("").to_owned();
                let Some(id) = id else {
                    return axum::http::StatusCode::ACCEPTED.into_response();
                };
                let session = headers.get("mcp-session-id").is_some();
                let auth = headers.get("authorization").and_then(|v| v.to_str().ok()) == Some("Bearer s3cret");
                let result = match method.as_str() {
                    "initialize" => json!({"protocolVersion": "2025-03-26", "capabilities": {}}),
                    "tools/list" => json!({"tools": [{"name": "time", "inputSchema": {"type": "object"}}]}),
                    _ => json!({"content": [{"type": "text", "text": format!("session={session} auth={auth}")}]}),
                };
                let reply = json!({"jsonrpc": "2.0", "id": id, "result": result});
                if method == "tools/call" {
                    // As an event stream, with a notification first.
                    let body = format!(
                        "event: message\ndata: {}\n\nevent: message\ndata: {}\n\n",
                        json!({"jsonrpc": "2.0", "method": "notifications/progress", "params": {}}),
                        reply
                    );
                    ([("content-type", "text/event-stream")], body).into_response()
                } else {
                    ([("mcp-session-id", "abc")], Json(reply)).into_response()
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

        // SAFETY: no other test reads this variable.
        unsafe { std::env::set_var("OC_TEST_MCP_TOKEN", "s3cret") };
        let server = McpServerConfig {
            url: Some(format!("http://{addr}/mcp")),
            headers: BTreeMap::from([(
                "Authorization".to_owned(),
                "Bearer ${OC_TEST_MCP_TOKEN}".to_owned(),
            )]),
            ..McpServerConfig::default()
        };
        let dir = tempfile::tempdir().unwrap();
        let mcp = Mcp::connect(&config(server), dir.path()).await;
        assert_eq!(mcp.specs()[0].function.name, "my_fake__time");
        let out = mcp.call("my_fake__time", json!({})).await.unwrap();
        assert_eq!(out, "session=true auth=true");
    }

    #[test]
    fn names_and_values_are_cleaned() {
        assert_eq!(clean_name("my server"), "my_server");
        assert_eq!(clean_name("a__b___c"), "a_b_c");
        assert_eq!(clean_name("工具"), "_");
        assert!(is_mcp_tool("fs__read") && !is_mcp_tool("read_file"));
        // SAFETY: no other test reads this variable.
        unsafe { std::env::set_var("OC_TEST_EXPAND", "v") };
        assert_eq!(expand("${OC_TEST_EXPAND}"), "v");
        assert_eq!(expand("$OC_TEST_EXPAND"), "v");
        assert_eq!(expand("plain"), "plain");
        assert_eq!(expand("Bearer ${OC_TEST_EXPAND}!"), "Bearer v!");
        assert_eq!(expand("a ${not closed"), "a ${not closed");
        assert_eq!(expand("cost $5"), "cost $5");
    }

    #[test]
    fn event_stream_messages_are_parsed() {
        assert_eq!(
            sse_message("event: message\r\ndata: {\"a\":\r\ndata: 1}\r\n\r\n"),
            Some(json!({"a": 1}))
        );
        assert_eq!(sse_message(": keep-alive\n\n"), None);
    }
}

/// Real servers: `cargo test mcp::live -- --ignored --nocapture` (needs npx).
#[cfg(test)]
mod live {
    use super::*;
    use std::collections::BTreeMap;

    #[tokio::test]
    #[ignore]
    async fn the_reference_server_works() {
        let dir = tempfile::tempdir().unwrap();
        let config = McpConfig {
            servers: BTreeMap::from([(
                "everything".to_owned(),
                stdio_server("npx", &["-y", "@modelcontextprotocol/server-everything"]),
            )]),
        };
        let mcp = Mcp::connect(&config, dir.path()).await;
        for (tool, args) in [
            ("everything__echo", json!({"message": "你好"})),
            ("everything__get-sum", json!({"a": 2, "b": 3})),
            ("everything__get-tiny-image", json!({})),
            (
                "everything__get-structured-content",
                json!({"location": "New York"}),
            ),
            (
                "everything__get-resource-reference",
                json!({"resourceType": "Text", "resourceId": 1}),
            ),
        ] {
            let out = mcp.call(tool, args).await.unwrap();
            println!("--- {tool}\n{out}");
            assert!(!out.starts_with("error"), "{out}");
        }
    }
}
