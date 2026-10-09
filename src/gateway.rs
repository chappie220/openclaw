//! HTTP + WebSocket Gateway serving the Web UI.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use async_trait::async_trait;
use axum::Router;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::response::{Html, IntoResponse, Response};
use axum::routing::get;
use futures_util::{SinkExt, StreamExt};
use serde::{Deserialize, Serialize};
use tokio::sync::{broadcast, mpsc, oneshot};

use crate::access::{self, AccessConfig, Actor};
use crate::agent::{Agent, AgentEvent, Model, Tools};
use crate::llm::Role;
use crate::tools::{Approver, with_approver};

const INDEX_HTML: &str = include_str!("../web/index.html");
/// An unanswered approval is declined so a turn never waits forever.
const APPROVAL_TIMEOUT: Duration = Duration::from_secs(300);
const MAX_SESSION_NAME: usize = 64;
const SCHEDULER_TICK: Duration = Duration::from_secs(20);

#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum ClientMsg {
    Send { session: String, text: String },
    Approve { id: u64, allow: bool },
    History { session: String },
    Sessions,
}

#[derive(Serialize, Clone)]
#[serde(tag = "type", rename_all = "snake_case")]
enum ServerMsg {
    Text {
        session: String,
        delta: String,
    },
    ToolStart {
        session: String,
        name: String,
        arguments: String,
    },
    ToolEnd {
        session: String,
        name: String,
        output: String,
    },
    Approval {
        session: String,
        id: u64,
        tool: String,
        summary: String,
    },
    Done {
        session: String,
        text: String,
    },
    Error {
        session: Option<String>,
        message: String,
    },
    History {
        session: String,
        messages: Vec<HistoryItem>,
    },
    Sessions {
        sessions: Vec<SessionItem>,
    },
    /// A turn nobody watched (a scheduled job) changed this session.
    Updated {
        session: String,
    },
}

#[derive(Serialize, Clone)]
struct HistoryItem {
    role: &'static str,
    text: String,
}

#[derive(Serialize, Clone)]
struct SessionItem {
    name: String,
    messages: i64,
    updated_at: i64,
}

/// Delivers unattended results (scheduled jobs) to a channel that owns the session.
#[async_trait]
pub trait Notifier: Send + Sync {
    fn handles(&self, session: &str) -> bool;
    async fn notify(&self, session: &str, text: &str) -> Result<()>;
}

pub struct Gateway<M: Model, T: Tools> {
    notifiers: Mutex<Vec<Arc<dyn Notifier>>>,
    agent: Arc<Agent<M, T>>,
    token: Option<String>,
    access: AccessConfig,
    updates: broadcast::Sender<ServerMsg>,
    /// One turn per session at a time; later sends queue behind it.
    session_locks: Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>,
}

type Shared<M, T> = Arc<Gateway<M, T>>;

impl<M: Model + 'static, T: Tools + 'static> Gateway<M, T> {
    pub fn new(
        agent: Arc<Agent<M, T>>,
        token: Option<String>,
        access: AccessConfig,
    ) -> Shared<M, T> {
        Arc::new(Self {
            agent,
            token,
            access,
            updates: broadcast::channel(64).0,
            notifiers: Mutex::new(Vec::new()),
            session_locks: Mutex::new(HashMap::new()),
        })
    }

    pub fn router(self: &Shared<M, T>) -> Router {
        Router::new()
            .route("/", get(|| async { Html(INDEX_HTML) }))
            .route("/health", get(|| async { "ok" }))
            .route("/ws", get(ws_upgrade::<M, T>))
            .with_state(self.clone())
    }

    fn session_lock(&self, session: &str) -> Arc<tokio::sync::Mutex<()>> {
        let mut locks = self.session_locks.lock().unwrap_or_else(|p| p.into_inner());
        locks.entry(session.to_owned()).or_default().clone()
    }

    pub fn store(&self) -> &crate::store::Store {
        &self.agent.store
    }

    pub fn add_notifier(&self, notifier: Arc<dyn Notifier>) {
        self.notifiers
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .push(notifier);
    }

    async fn notify(&self, session: &str, text: &str) {
        let notifiers: Vec<_> = self
            .notifiers
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .clone();
        for notifier in notifiers.iter().filter(|n| n.handles(session)) {
            if let Err(err) = notifier.notify(session, text).await {
                eprintln!("cannot deliver to {session}: {err:#}");
            }
        }
    }

    /// The actor for a sender a channel authenticated as `sender`.
    pub fn actor(&self, sender: &str, session: &str) -> Actor {
        self.access.resolve(sender, session)
    }

    /// Runs a turn with nobody to approve tools, queued behind other turns of the session.
    pub async fn run_unattended(
        &self,
        actor: Actor,
        session: &str,
        prompt: &str,
    ) -> Result<String> {
        let lock = self.session_lock(session);
        let _turn = lock.lock().await;
        let result = self
            .agent
            .run_turn(actor, session, prompt, &mut |_| {})
            .await;
        let _ = self.updates.send(ServerMsg::Updated {
            session: session.to_owned(),
        });
        result
    }

    /// Runs due cron jobs; jobs added by the CLI or tools are picked up on the next tick.
    pub async fn run_scheduler(self: Shared<M, T>) {
        let mut tick = tokio::time::interval(SCHEDULER_TICK);
        loop {
            tick.tick().await;
            let jobs = match self.agent.store.job_claim_due(crate::store::now()) {
                Ok(jobs) => jobs,
                Err(err) => {
                    eprintln!("scheduler: cannot read jobs: {err:#}");
                    continue;
                }
            };
            for job in jobs {
                let gateway = self.clone();
                tokio::spawn(async move {
                    let prompt = format!("[scheduled job {:?}] {}", job.name, job.prompt);
                    let actor = gateway.job_actor(&job);
                    let status = match gateway.run_unattended(actor, &job.session, &prompt).await {
                        Ok(text) => {
                            gateway.notify(&job.session, &text).await;
                            "ok".to_owned()
                        }
                        Err(err) => format!("error: {err:#}"),
                    };
                    eprintln!(
                        "scheduler: job {:?} in session {:?}: {status}",
                        job.name, job.session
                    );
                    if let Err(err) = gateway.agent.store.job_record(job.id, &status) {
                        eprintln!("scheduler: cannot record job {:?}: {err:#}", job.name);
                    }
                });
            }
        }
    }

    /// A job runs with the permissions its creator has now, so revoking a
    /// grant also stops their jobs from using it.
    fn job_actor(&self, job: &crate::cron::Job) -> Actor {
        match job.created_by.as_deref() {
            Some(by) => self.actor(by, &job.session),
            // From before creators were recorded: trust jobs in local sessions,
            // and treat those in channel sessions as their channel's guest.
            None if is_channel_session(&job.session) => Actor {
                id: "legacy-job".into(),
                ..self.actor("legacy:job", &job.session)
            },
            None => Actor::owner(access::CLI),
        }
    }

    fn authorized(&self, presented: Option<&str>) -> bool {
        match (&self.token, presented) {
            (None, _) => true,
            (Some(expected), Some(given)) => {
                constant_time_eq(expected.as_bytes(), given.as_bytes())
            }
            (Some(_), None) => false,
        }
    }
}

/// Refuses an unauthenticated listener that other hosts can reach.
pub async fn serve<M: Model + 'static, T: Tools + 'static>(
    gateway: Shared<M, T>,
    bind: &str,
) -> Result<()> {
    let addr: SocketAddr = bind
        .parse()
        .with_context(|| format!("invalid gateway.bind {bind:?}"))?;
    if gateway.token.is_none() && !addr.ip().is_loopback() {
        bail!(
            "gateway.bind {addr} is reachable from other hosts; set OPENCLAW_RS_TOKEN or gateway.token first"
        );
    }
    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .with_context(|| format!("cannot listen on {addr}"))?;
    eprintln!(
        "gateway listening on http://{addr}{}",
        if gateway.token.is_some() {
            " (token required)"
        } else {
            ""
        }
    );
    axum::serve(listener, gateway.router())
        .with_graceful_shutdown(async {
            let _ = tokio::signal::ctrl_c().await;
        })
        .await?;
    Ok(())
}

fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

#[derive(Deserialize)]
struct WsQuery {
    token: Option<String>,
}

async fn ws_upgrade<M: Model + 'static, T: Tools + 'static>(
    State(gateway): State<Shared<M, T>>,
    Query(query): Query<WsQuery>,
    upgrade: WebSocketUpgrade,
) -> Response {
    if !gateway.authorized(query.token.as_deref()) {
        return (StatusCode::UNAUTHORIZED, "invalid token").into_response();
    }
    upgrade.on_upgrade(move |socket| connection(gateway, socket))
}

/// Answers approvals for turns started on one WebSocket connection.
struct SocketApprover {
    session: String,
    out: mpsc::UnboundedSender<ServerMsg>,
    pending: Arc<Mutex<HashMap<u64, oneshot::Sender<bool>>>>,
    next_id: Arc<AtomicU64>,
}

#[async_trait]
impl Approver for SocketApprover {
    async fn approve(&self, tool: &str, summary: &str) -> bool {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = oneshot::channel();
        self.pending
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .insert(id, tx);
        let request = ServerMsg::Approval {
            session: self.session.clone(),
            id,
            tool: tool.to_owned(),
            summary: summary.to_owned(),
        };
        if self.out.send(request).is_err() {
            return false;
        }
        let allowed = matches!(
            tokio::time::timeout(APPROVAL_TIMEOUT, rx).await,
            Ok(Ok(true))
        );
        self.pending
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .remove(&id);
        allowed
    }
}

async fn connection<M: Model + 'static, T: Tools + 'static>(
    gateway: Shared<M, T>,
    socket: WebSocket,
) {
    let (mut sink, mut stream) = socket.split();
    let (out, mut outbox) = mpsc::unbounded_channel::<ServerMsg>();
    let writer = tokio::spawn(async move {
        while let Some(msg) = outbox.recv().await {
            let Ok(json) = serde_json::to_string(&msg) else {
                continue;
            };
            if sink.send(Message::Text(json.into())).await.is_err() {
                break;
            }
        }
    });
    let mut updates = gateway.updates.subscribe();
    let relay_out = out.clone();
    let relay = tokio::spawn(async move {
        loop {
            match updates.recv().await {
                Ok(msg) => {
                    if relay_out.send(msg).is_err() {
                        break;
                    }
                }
                Err(broadcast::error::RecvError::Lagged(_)) => continue,
                Err(broadcast::error::RecvError::Closed) => break,
            }
        }
    });
    let pending: Arc<Mutex<HashMap<u64, oneshot::Sender<bool>>>> = Arc::default();
    let next_id = Arc::new(AtomicU64::new(1));
    while let Some(Ok(frame)) = stream.next().await {
        let Message::Text(text) = frame else { continue };
        let msg = match serde_json::from_str::<ClientMsg>(&text) {
            Ok(msg) => msg,
            Err(err) => {
                let _ = out.send(ServerMsg::Error {
                    session: None,
                    message: format!("bad request: {err}"),
                });
                continue;
            }
        };
        match msg {
            ClientMsg::Send { session, text } => {
                if let Err(message) = validate_session(&session) {
                    let _ = out.send(ServerMsg::Error {
                        session: Some(session),
                        message,
                    });
                    continue;
                }
                let approver = SocketApprover {
                    session: session.clone(),
                    out: out.clone(),
                    pending: pending.clone(),
                    next_id: next_id.clone(),
                };
                // The turn outlives this connection: it keeps saving if the browser goes away.
                tokio::spawn(run_turn(
                    gateway.clone(),
                    session,
                    text,
                    out.clone(),
                    approver,
                ));
            }
            ClientMsg::Approve { id, allow } => {
                if let Some(tx) = pending
                    .lock()
                    .unwrap_or_else(|p| p.into_inner())
                    .remove(&id)
                {
                    let _ = tx.send(allow);
                }
            }
            ClientMsg::History { session } => {
                let _ = out.send(history(&gateway, session));
            }
            ClientMsg::Sessions => {
                let _ = out.send(sessions(&gateway));
            }
        }
    }
    // Dropping pending senders declines approvals nobody can answer any more.
    pending.lock().unwrap_or_else(|p| p.into_inner()).clear();
    relay.abort();
    drop(out);
    let _ = writer.await;
}

/// Sessions owned by an external channel rather than the terminal or Web UI.
fn is_channel_session(name: &str) -> bool {
    name.starts_with("qq:") || name.starts_with("mail:")
}

fn validate_session(name: &str) -> Result<(), String> {
    let count = name.chars().count();
    if count == 0 || count > MAX_SESSION_NAME || name.chars().any(char::is_control) {
        return Err(format!(
            "session names need 1-{MAX_SESSION_NAME} printable characters"
        ));
    }
    Ok(())
}

async fn run_turn<M: Model + 'static, T: Tools + 'static>(
    gateway: Shared<M, T>,
    session: String,
    text: String,
    out: mpsc::UnboundedSender<ServerMsg>,
    approver: SocketApprover,
) {
    let lock = gateway.session_lock(&session);
    let _turn = lock.lock().await;
    let events = out.clone();
    let name = session.clone();
    let mut on_event = move |event: AgentEvent| {
        let session = name.clone();
        let _ = events.send(match event {
            AgentEvent::Text(delta) => ServerMsg::Text { session, delta },
            AgentEvent::ToolStart { name, arguments } => ServerMsg::ToolStart {
                session,
                name,
                arguments,
            },
            AgentEvent::ToolEnd { name, output } => ServerMsg::ToolEnd {
                session,
                name,
                output,
            },
        });
    };
    let result = with_approver(
        Arc::new(approver),
        // The connection presented the gateway token (or is loopback-only).
        gateway
            .agent
            .run_turn(Actor::owner(access::WEB), &session, &text, &mut on_event),
    )
    .await;
    let _ = out.send(match result {
        Ok(text) => ServerMsg::Done { session, text },
        Err(err) => ServerMsg::Error {
            session: Some(session),
            message: format!("{err:#}"),
        },
    });
}

fn history<M: Model, T: Tools>(gateway: &Gateway<M, T>, session: String) -> ServerMsg {
    let result = (|| -> Result<Vec<HistoryItem>> {
        let id = gateway.agent.store.session_id(&session)?;
        Ok(gateway
            .agent
            .store
            .history(id, 200)?
            .into_iter()
            .filter_map(|m| {
                let text = m.content.filter(|c| !c.is_empty())?;
                let role = match m.role {
                    Role::User => "user",
                    Role::Assistant => "assistant",
                    Role::Tool | Role::System => return None,
                };
                Some(HistoryItem { role, text })
            })
            .collect())
    })();
    match result {
        Ok(messages) => ServerMsg::History { session, messages },
        Err(err) => ServerMsg::Error {
            session: Some(session),
            message: format!("{err:#}"),
        },
    }
}

fn sessions<M: Model, T: Tools>(gateway: &Gateway<M, T>) -> ServerMsg {
    match gateway.agent.store.sessions() {
        Ok(list) => ServerMsg::Sessions {
            sessions: list
                .into_iter()
                .map(|s| SessionItem {
                    name: s.name,
                    messages: s.messages,
                    updated_at: s.updated_at,
                })
                .collect(),
        },
        Err(err) => ServerMsg::Error {
            session: None,
            message: format!("{err:#}"),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn token_check_requires_an_exact_match() {
        assert!(constant_time_eq(b"secret", b"secret"));
        assert!(!constant_time_eq(b"secret", b"secreT"));
        assert!(!constant_time_eq(b"secret", b"secret2"));
    }

    /// A model that ignores which tools it was offered and always tries to
    /// save a memory, then reports what the tool said.
    struct Pushy;

    #[async_trait]
    impl Model for Pushy {
        async fn complete(
            &self,
            messages: &[crate::llm::ChatMessage],
            _tools: &[crate::llm::ToolSpec],
            _on_text: &mut (dyn for<'t> FnMut(&'t str) + Send),
        ) -> Result<crate::llm::Completion> {
            let last = messages.last().unwrap();
            if last.role == Role::Tool {
                return Ok(crate::llm::Completion {
                    text: last.content.clone().unwrap_or_default(),
                    ..Default::default()
                });
            }
            Ok(crate::llm::Completion {
                tool_calls: vec![crate::llm::ToolCall {
                    id: "c1".into(),
                    kind: "function".into(),
                    function: crate::llm::FunctionCall {
                        name: "memory_save".into(),
                        arguments: r#"{"content":"the owner's password is 1234"}"#.into(),
                    },
                }],
                ..Default::default()
            })
        }
    }

    #[tokio::test]
    async fn every_channel_runs_tools_with_its_senders_permissions() {
        let dir = tempfile::tempdir().unwrap();
        let store = crate::store::Store::open_in_memory().unwrap();
        let tools = crate::tools::BuiltinTools::new(
            dir.path().to_owned(),
            crate::config::ToolsConfig::default(),
            store.clone(),
        )
        .unwrap();
        let agent = Arc::new(Agent {
            model: Pushy,
            tools,
            store: store.clone(),
            config: crate::config::AgentConfig::default(),
        });
        let access: AccessConfig = toml::from_str(r#"owners = ["mail:me@example.org"]"#).unwrap();
        let gateway = Gateway::new(agent, None, access);
        let cases = [
            ("qq:STRANGER", "qq:c2c:STRANGER", false),
            ("qq:MEMBER", "qq:group:G1", false),
            ("mail:x@example.org", "mail:x@example.org", false),
            ("mail:me@example.org", "mail:me@example.org", true),
            (access::CLI, "main", true),
            (access::WEB, "web", true),
        ];
        for (sender, session, allowed) in cases {
            let actor = gateway.actor(sender, session);
            let reply = gateway.run_unattended(actor, session, "hi").await.unwrap();
            assert_eq!(
                reply.starts_with("saved memory"),
                allowed,
                "{sender}: {reply}"
            );
        }
        assert_eq!(store.memory_list(10).unwrap().len(), 3);
    }

    #[test]
    fn session_names_are_bounded_and_printable() {
        assert!(validate_session("工作").is_ok());
        assert!(validate_session("").is_err());
        assert!(validate_session("a\nb").is_err());
        assert!(validate_session(&"x".repeat(65)).is_err());
    }
}
