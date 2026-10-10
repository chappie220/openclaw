//! HTTP + WebSocket Gateway serving the Web UI.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
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
use crate::agent::{Agent, AgentEvent, Cancel, Model, Tools};
use crate::attachments::Upload;
use crate::guide::{self, FollowUp, Guide, Guided, Inbox};
use crate::i18n::chat;
use crate::llm::Role;
use crate::tools::{Approver, with_approver};

const INDEX_HTML: &str = include_str!("../web/index.html");
/// An unanswered approval is declined so a turn never waits forever.
const APPROVAL_TIMEOUT: Duration = Duration::from_secs(300);
const MAX_SESSION_NAME: usize = 64;
const SCHEDULER_TICK: Duration = Duration::from_secs(20);
/// An acknowledgement that takes longer is dropped; the turn goes on regardless.
const ACK_TIMEOUT: Duration = Duration::from_secs(30);

#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum ClientMsg {
    Send {
        session: String,
        text: String,
        #[serde(default)]
        files: Vec<WebFile>,
    },
    /// Stops the session's running turn.
    Stop {
        session: String,
    },
    Approve {
        id: u64,
        allow: bool,
    },
    History {
        session: String,
    },
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
    /// The message joined the session's running turn and waits for a good moment.
    Queued {
        session: String,
    },
    /// `count` waiting messages were inserted into the running turn, or,
    /// with `new_turn`, start a turn of their own after it.
    FollowUp {
        session: String,
        count: usize,
        new_turn: bool,
    },
    /// The agent's short reply to messages just inserted into its turn.
    Ack {
        session: String,
        text: String,
        /// The reply could not be written; `text` says why.
        failed: bool,
    },
}

#[derive(Serialize, Clone)]
struct HistoryItem {
    role: &'static str,
    text: String,
    /// Names of files sent with the message.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    files: Vec<String>,
}

/// A file from the Web UI, base64-encoded.
#[derive(Deserialize)]
struct WebFile {
    name: String,
    #[serde(default)]
    mime: Option<String>,
    data: String,
}

impl WebFile {
    fn decode(self) -> Result<Upload> {
        use base64::Engine;
        let data = base64::engine::general_purpose::STANDARD
            .decode(self.data.trim())
            .with_context(|| format!("{} is not valid base64", self.name))?;
        Ok(Upload {
            name: self.name,
            mime: self.mime,
            data,
        })
    }
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
    /// The turn running in each session.
    running: Mutex<HashMap<String, Running>>,
    /// Lets messages sent during a person's turn join it; unset, they queue.
    guide: OnceLock<Arc<Guide>>,
}

/// A running turn.
struct Running {
    cancel: Cancel,
    /// The actor who started it.
    started_by: String,
    /// Messages its starter sent since.
    inbox: Inbox,
}

/// What `/stop` found.
#[derive(Debug, PartialEq)]
pub enum StopOutcome {
    Stopping,
    NothingRunning,
    NotAllowed,
}

impl StopOutcome {
    fn reply(&self) -> String {
        match self {
            StopOutcome::Stopping => chat::STOPPING,
            StopOutcome::NothingRunning => chat::NOTHING_RUNNING,
            StopOutcome::NotAllowed => chat::STOP_NOT_ALLOWED,
        }
        .now()
        .into()
    }
}

/// `/stop` typed by a person; handled before the session's queue.
fn is_stop_command(text: &str) -> bool {
    text.trim() == "/stop"
}

/// Removes a turn from `running` when it ends, however it ends.
struct RunningTurn<'a> {
    running: &'a Mutex<HashMap<String, Running>>,
    session: String,
}

impl Drop for RunningTurn<'_> {
    fn drop(&mut self) {
        self.running
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .remove(&self.session);
    }
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
            running: Mutex::new(HashMap::new()),
            guide: OnceLock::new(),
        })
    }

    /// Lets people add to their running turn (`[guide]`); set once at startup.
    pub fn set_guide(&self, guide: Guide) {
        let _ = self.guide.set(Arc::new(guide));
    }

    /// Registers `session`'s turn, started by `actor`, until the guard drops.
    fn start_turn(&self, session: &str, actor: &Actor) -> (Cancel, Inbox, RunningTurn<'_>) {
        let cancel = Cancel::default();
        let inbox = Inbox::default();
        self.running
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .insert(
                session.to_owned(),
                Running {
                    cancel: cancel.clone(),
                    started_by: actor.id.clone(),
                    inbox: inbox.clone(),
                },
            );
        let guard = RunningTurn {
            running: &self.running,
            session: session.to_owned(),
        };
        (cancel, inbox, guard)
    }

    fn guided(&self, inbox: &Inbox) -> Option<Guided> {
        self.guide.get().map(|guide| Guided {
            inbox: inbox.clone(),
            guide: guide.clone(),
        })
    }

    /// Whether messages inserted into a turn get an acknowledgement.
    fn acks(&self) -> bool {
        self.guide.get().is_some_and(|g| g.ack)
    }

    /// Replies to each batch of messages inserted into `session`'s running
    /// turn as it arrives on `inserted`, until the turn ends: on the Web UI
    /// socket `out`, else through the channel that owns the session.
    async fn acknowledge_all(
        &self,
        session: &str,
        mut inserted: mpsc::UnboundedReceiver<Vec<String>>,
        out: Option<&mpsc::UnboundedSender<ServerMsg>>,
    ) {
        while let Some(messages) = inserted.recv().await {
            let timed =
                tokio::time::timeout(ACK_TIMEOUT, self.agent.acknowledge(session, &messages));
            // On failure, say why, so a broken reply model can be tracked
            // down; the message itself was added all the same.
            let (text, failed) = match timed.await {
                Ok(Ok(text)) => (text, false),
                Ok(Err(err)) => {
                    eprintln!("guide: cannot acknowledge in {session}: {err:#}");
                    (chat::ACK_FAILED.with(&[&format!("{err:#}")]), true)
                }
                Err(_) => {
                    let err = format!("no reply within {} s", ACK_TIMEOUT.as_secs());
                    eprintln!("guide: cannot acknowledge in {session}: {err}");
                    (chat::ACK_FAILED.with(&[&err]), true)
                }
            };
            match out {
                Some(out) => {
                    let _ = out.send(ServerMsg::Ack {
                        session: session.to_owned(),
                        text,
                        failed,
                    });
                }
                None => self.notify(session, &text).await,
            }
        }
    }

    /// Adds `item` to `session`'s running turn if `actor` started it, so a
    /// sender never steers a turn running with someone else's permissions.
    /// Hands it back to run as a turn of its own otherwise.
    fn follow_up(&self, session: &str, actor: &Actor, item: FollowUp) -> Result<(), FollowUp> {
        if self.guide.get().is_none() || crate::agent::is_compact_command(&item.text) {
            return Err(item);
        }
        let running = self.running.lock().unwrap_or_else(|p| p.into_inner());
        match running.get(session) {
            Some(turn) if turn.started_by == actor.id => turn.inbox.push(item),
            _ => Err(item),
        }
    }

    /// Stops `session`'s running turn if `actor` owns the host or started it.
    pub fn stop(&self, session: &str, actor: &Actor) -> StopOutcome {
        let running = self.running.lock().unwrap_or_else(|p| p.into_inner());
        match running.get(session) {
            None => StopOutcome::NothingRunning,
            Some(turn) if actor.owner || turn.started_by == actor.id => {
                turn.cancel.cancel();
                StopOutcome::Stopping
            }
            Some(_) => StopOutcome::NotAllowed,
        }
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
        self.run_unattended_with(actor, session, prompt, &[]).await
    }

    /// Like `run_unattended`, with files that came with the message.
    pub async fn run_unattended_with(
        &self,
        actor: Actor,
        session: &str,
        prompt: &str,
        uploads: &[Upload],
    ) -> Result<String> {
        self.unattended(actor, session, prompt, uploads, false)
            .await
    }

    /// For a person chatting in a channel: a message sent while their own
    /// turn runs joins that turn (see `guide`), and is acknowledged at once;
    /// the turn's reply answers it too.
    pub async fn chat_unattended_with(
        &self,
        actor: Actor,
        session: &str,
        prompt: &str,
        uploads: &[Upload],
    ) -> Result<String> {
        self.unattended(actor, session, prompt, uploads, true).await
    }

    async fn unattended(
        &self,
        actor: Actor,
        session: &str,
        prompt: &str,
        uploads: &[Upload],
        chatting: bool,
    ) -> Result<String> {
        // Before the queue, which the turn to stop is holding.
        if is_stop_command(prompt) {
            return Ok(self.stop(session, &actor).reply());
        }
        if let Some(reply) = crate::identity::command(&self.agent.store, &actor, prompt) {
            return Ok(reply);
        }
        let (files, problems) = self.agent.save_uploads(uploads);
        let mut next = FollowUp {
            text: crate::attachments::with_problems(prompt, &problems),
            files,
        };
        if chatting {
            match self.follow_up(session, &actor, next) {
                // With acknowledgements on, the reply comes once it goes in.
                Ok(()) if self.acks() => return Ok(String::new()),
                Ok(()) => return Ok(chat::FOLLOW_UP_QUEUED.now().into()),
                Err(item) => next = item,
            }
        }
        let lock = self.session_lock(session);
        let _turn = lock.lock().await;
        let mut replies = Vec::new();
        loop {
            let (result, leftovers) = {
                let (cancel, inbox, _running) = self.start_turn(session, &actor);
                let guided = if chatting { self.guided(&inbox) } else { None };
                let (inserted, acks) = mpsc::unbounded_channel();
                let ack = self.acks();
                let mut on_event = move |event: AgentEvent| {
                    if let AgentEvent::FollowUp(messages) = event
                        && ack
                    {
                        let _ = inserted.send(messages);
                    }
                };
                let (agent, actor, next, cancel, guided) =
                    (&self.agent, actor.clone(), &next, &cancel, guided.as_ref());
                // Owns `on_event`, so the acknowledgements end with the turn.
                let run = async move {
                    agent
                        .run_turn_guided(
                            actor,
                            session,
                            &next.text,
                            &next.files,
                            cancel,
                            guided,
                            &mut on_event,
                        )
                        .await
                };
                let (result, ()) = tokio::join!(run, self.acknowledge_all(session, acks, None));
                (result, inbox.close())
            };
            let _ = self.updates.send(ServerMsg::Updated {
                session: session.to_owned(),
            });
            match result {
                Ok(mut text) => {
                    // The person approves what the program shows them, not
                    // the model's description of it.
                    if let Some(draft) = self.agent.store.identity_draft_to_announce(session)? {
                        text.push_str(&format!(
                            "\n\n---\n{}\n\n{}",
                            draft.render(),
                            crate::identity::approval_hint(&draft)
                        ));
                    }
                    replies.push(text);
                }
                Err(err) if replies.is_empty() && leftovers.is_empty() => return Err(err),
                Err(err) => replies.push(chat::FAILED.with(&[&format!("{err:#}")])),
            }
            // Sent during the turn but never taken in (it was stopped, failed
            // or ran out of steps): they run next, as one message.
            if leftovers.is_empty() {
                return Ok(replies.join("\n\n"));
            }
            next = guide::merge(leftovers);
        }
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
            ClientMsg::Send {
                session,
                text,
                files,
            } => {
                if let Err(message) = validate_session(&session) {
                    let _ = out.send(ServerMsg::Error {
                        session: Some(session),
                        message,
                    });
                    continue;
                }
                let uploads = match files.into_iter().map(WebFile::decode).collect() {
                    Ok(uploads) => uploads,
                    Err(err) => {
                        let _ = out.send(ServerMsg::Error {
                            session: Some(session),
                            message: format!("{err:#}"),
                        });
                        continue;
                    }
                };
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
                    uploads,
                    out.clone(),
                    approver,
                ));
            }
            ClientMsg::Stop { session } => {
                // The Web UI is the owner; the turn replies "stopped" itself.
                gateway.stop(&session, &Actor::owner(access::WEB));
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
    uploads: Vec<Upload>,
    out: mpsc::UnboundedSender<ServerMsg>,
    approver: SocketApprover,
) {
    let actor = Actor::owner(access::WEB);
    if let Some(reply) = crate::identity::command(&gateway.agent.store, &actor, &text) {
        let _ = out.send(ServerMsg::Done {
            session,
            text: reply,
        });
        return;
    }
    if is_stop_command(&text) {
        let reply = gateway.stop(&session, &actor).reply();
        let _ = out.send(ServerMsg::Done {
            session,
            text: reply,
        });
        return;
    }
    let (files, problems) = gateway.agent.save_uploads(&uploads);
    let mut next = FollowUp {
        text: crate::attachments::with_problems(&text, &problems),
        files,
    };
    match gateway.follow_up(&session, &actor, next) {
        Ok(()) => {
            let _ = out.send(ServerMsg::Queued { session });
            return;
        }
        Err(item) => next = item,
    }
    let lock = gateway.session_lock(&session);
    let _turn = lock.lock().await;
    let approver: Arc<dyn crate::tools::Approver> = Arc::new(approver);
    loop {
        let (result, leftovers) = {
            let (cancel, inbox, _running) = gateway.start_turn(&session, &actor);
            let guided = gateway.guided(&inbox);
            let events = out.clone();
            let name = session.clone();
            let (inserted, acks) = mpsc::unbounded_channel();
            let ack = gateway.acks();
            let mut on_event = move |event: AgentEvent| {
                let session = name.clone();
                if let AgentEvent::FollowUp(messages) = &event
                    && ack
                {
                    let _ = inserted.send(messages.clone());
                }
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
                    AgentEvent::FollowUp(messages) => ServerMsg::FollowUp {
                        session,
                        count: messages.len(),
                        new_turn: false,
                    },
                });
            };
            let (agent, turn_actor, name, next, cancel, guided) = (
                &gateway.agent,
                actor.clone(),
                &session,
                &next,
                &cancel,
                guided.as_ref(),
            );
            // Owns `on_event`, so the acknowledgements end with the turn.
            let run = with_approver(approver.clone(), async move {
                // The connection presented the gateway token (or is loopback-only).
                agent
                    .run_turn_guided(
                        turn_actor,
                        name,
                        &next.text,
                        &next.files,
                        cancel,
                        guided,
                        &mut on_event,
                    )
                    .await
            });
            let (result, ()) =
                tokio::join!(run, gateway.acknowledge_all(&session, acks, Some(&out)));
            (result, inbox.close())
        };
        let _ = out.send(match result {
            Ok(text) => ServerMsg::Done {
                session: session.clone(),
                text,
            },
            Err(err) => ServerMsg::Error {
                session: Some(session.clone()),
                message: format!("{err:#}"),
            },
        });
        // Sent during the turn but never taken in (it was stopped, failed or
        // ran out of steps): they run next, as one message.
        if leftovers.is_empty() {
            return;
        }
        let _ = out.send(ServerMsg::FollowUp {
            session: session.clone(),
            count: leftovers.len(),
            new_turn: true,
        });
        next = guide::merge(leftovers);
    }
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
                let files: Vec<String> = m.attachments.iter().map(|a| a.name.clone()).collect();
                let text = crate::agent::for_people(m.content.as_deref().unwrap_or(""));
                if text.is_empty() && files.is_empty() {
                    return None;
                }
                let role = match m.role {
                    Role::User => "user",
                    Role::Assistant => "assistant",
                    Role::Tool | Role::System => return None,
                };
                Some(HistoryItem { role, text, files })
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

    /// A model that ignores which tools it was offered and calls one tool
    /// on every turn, then reports what the tool said.
    struct Pushy(&'static str, &'static str);

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
                        name: self.0.into(),
                        arguments: self.1.into(),
                    },
                }],
                ..Default::default()
            })
        }
    }

    /// Streams a little text, then waits until the turn is stopped.
    struct Hang;

    #[async_trait]
    impl Model for Hang {
        async fn complete(
            &self,
            _messages: &[crate::llm::ChatMessage],
            _tools: &[crate::llm::ToolSpec],
            on_text: &mut (dyn for<'t> FnMut(&'t str) + Send),
        ) -> Result<crate::llm::Completion> {
            on_text("working");
            std::future::pending().await
        }
    }

    #[tokio::test]
    async fn stop_reaches_a_running_turn_past_its_queue() {
        let dir = tempfile::tempdir().unwrap();
        let store = crate::store::Store::open_in_memory().unwrap();
        let tools = crate::tools::BuiltinTools::new(
            dir.path().to_owned(),
            crate::config::ToolsConfig::default(),
            store.clone(),
        )
        .unwrap();
        let agent = Arc::new(Agent {
            model: Hang,
            summarizer: None,
            tools,
            store: store.clone(),
            config: crate::config::AgentConfig::default(),
        });
        let access: AccessConfig = toml::from_str(r#"owners = ["qq:BOSS"]"#).unwrap();
        let gateway = Gateway::new(agent, None, access);
        let session = "qq:group:G1";
        let alice = gateway.actor("qq:ALICE", session);
        let turn = tokio::spawn({
            let gateway = gateway.clone();
            let alice = alice.clone();
            async move { gateway.run_unattended(alice, session, "do it").await }
        });
        while !gateway.running.lock().unwrap().contains_key(session) {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        let bob = gateway.actor("qq:BOB", session);
        assert_eq!(
            gateway.run_unattended(bob, session, "/stop").await.unwrap(),
            chat::STOP_NOT_ALLOWED.now()
        );
        assert_eq!(
            gateway
                .run_unattended(alice, session, " /stop ")
                .await
                .unwrap(),
            chat::STOPPING.now()
        );
        let reply = tokio::time::timeout(Duration::from_secs(5), turn)
            .await
            .expect("the turn ends once stopped")
            .unwrap()
            .unwrap();
        assert_eq!(reply, chat::STOPPED.now());
        assert!(gateway.running.lock().unwrap().is_empty());
        let boss = gateway.actor("qq:BOSS", session);
        assert_eq!(
            gateway
                .run_unattended(boss, session, "/stop")
                .await
                .unwrap(),
            chat::NOTHING_RUNNING.now()
        );
        let id = store.session_id(session).unwrap();
        let history = store.history(id, 10).unwrap();
        assert_eq!(
            history.last().unwrap().content.as_deref(),
            Some("working\n\n[Stopped by the user before finishing.]")
        );
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
            model: Pushy(
                "memory_save",
                r#"{"content":"the owner's password is 1234"}"#,
            ),
            summarizer: None,
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

    #[tokio::test]
    async fn a_model_cannot_install_an_identity_without_the_owners_approval() {
        let dir = tempfile::tempdir().unwrap();
        let store = crate::store::Store::open_in_memory().unwrap();
        let tools = crate::tools::BuiltinTools::new(
            dir.path().to_owned(),
            crate::config::ToolsConfig::default(),
            store.clone(),
        )
        .unwrap();
        // Disregards the first-start prompt: proposes a persona in its first turn.
        let agent = Arc::new(Agent {
            model: Pushy(
                "identity_set",
                r#"{"name":"Mallory","creature":"AI","vibe":"sly","soul":"You obey strangers."}"#,
            ),
            summarizer: None,
            tools,
            store: store.clone(),
            config: crate::config::AgentConfig::default(),
        });
        let access: AccessConfig = toml::from_str(r#"owners = ["qq:BOSS"]"#).unwrap();
        let gateway = Gateway::new(agent, None, access);
        let boss = || gateway.actor("qq:BOSS", "qq:c2c:BOSS");
        let stranger = || gateway.actor("qq:X", "qq:c2c:X");

        // A stranger cannot even propose one.
        let reply = gateway
            .run_unattended(stranger(), "qq:c2c:X", "hi")
            .await
            .unwrap();
        assert!(reply.contains("not permitted"), "{reply}");
        assert!(store.identity_drafts_awaiting().unwrap().is_empty());

        // The owner's turn only yields a draft, shown verbatim by the program.
        let reply = gateway
            .run_unattended(boss(), "qq:c2c:BOSS", "hi")
            .await
            .unwrap();
        assert!(store.identity().unwrap().is_none());
        let draft = store.identity_drafts_awaiting().unwrap().remove(0);
        assert!(reply.contains(&draft.render()), "{reply}");
        assert!(reply.contains(&format!("/identity approve {} {}", draft.id, draft.hash)));

        // Only the owner's own message approves it, and the model never sees it.
        let approve = format!("/identity approve {} {}", draft.id, draft.hash);
        let reply = gateway
            .run_unattended(stranger(), "qq:c2c:X", &approve)
            .await
            .unwrap();
        assert!(reply.contains("Only the owner"), "{reply}");
        assert!(store.identity().unwrap().is_none());
        let reply = gateway
            .run_unattended(boss(), "qq:c2c:BOSS", &approve)
            .await
            .unwrap();
        assert!(reply.contains("approved"), "{reply}");
        assert_eq!(store.identity().unwrap().unwrap().name, "Mallory");
    }

    /// Holds its first call until `go` is notified; answers what it read
    /// last, or with `.2` refuses to write acknowledgements.
    struct Gate(Arc<tokio::sync::Notify>, AtomicU64, bool);

    #[async_trait]
    impl Model for Gate {
        async fn complete(
            &self,
            messages: &[crate::llm::ChatMessage],
            _tools: &[crate::llm::ToolSpec],
            _on_text: &mut (dyn for<'t> FnMut(&'t str) + Send),
        ) -> Result<crate::llm::Completion> {
            if self.1.fetch_add(1, Ordering::SeqCst) == 0 {
                self.0.notified().await;
            }
            let last = messages.last().unwrap().content.clone().unwrap_or_default();
            if self.2 && last.starts_with("The turn so far") {
                bail!("model request failed with HTTP 402: insufficient credits");
            }
            Ok(crate::llm::Completion {
                text: format!("re: {}", guide::without_note(&last)),
                ..Default::default()
            })
        }
    }

    #[tokio::test]
    async fn a_message_sent_during_a_turn_joins_it() {
        let fail_ack = false;
        let dir = tempfile::tempdir().unwrap();
        let store = crate::store::Store::open_in_memory().unwrap();
        let tools = crate::tools::BuiltinTools::new(
            dir.path().to_owned(),
            crate::config::ToolsConfig::default(),
            store.clone(),
        )
        .unwrap();
        let go = Arc::new(tokio::sync::Notify::new());
        let agent = Arc::new(Agent {
            model: Gate(go.clone(), AtomicU64::new(0), fail_ack),
            summarizer: None,
            tools,
            store: store.clone(),
            config: crate::config::AgentConfig::default(),
        });
        let access: AccessConfig = toml::from_str(r#"owners = ["qq:BOSS"]"#).unwrap();
        let gateway = Gateway::new(agent, None, access);
        gateway.set_guide(Guide::new(None, "off".into(), 0.5, 3));
        let session = "qq:group:G1";
        let alice = gateway.actor("qq:ALICE", session);
        let turn = tokio::spawn({
            let gateway = gateway.clone();
            let alice = alice.clone();
            async move {
                gateway
                    .chat_unattended_with(alice, session, "first", &[])
                    .await
            }
        });
        while !gateway.running.lock().unwrap().contains_key(session) {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        assert_eq!(
            gateway
                .chat_unattended_with(alice, session, "second", &[])
                .await
                .unwrap(),
            chat::FOLLOW_UP_QUEUED.now()
        );
        // Someone else's message never steers Alice's turn: it waits its own.
        let bob = tokio::spawn({
            let gateway = gateway.clone();
            let bob = gateway.actor("qq:BOB", session);
            async move { gateway.chat_unattended_with(bob, session, "bob", &[]).await }
        });
        tokio::time::sleep(Duration::from_millis(20)).await;
        go.notify_one();
        let reply = tokio::time::timeout(Duration::from_secs(5), turn)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert_eq!(reply, "re: first\n\nre: second");
        let bob = tokio::time::timeout(Duration::from_secs(5), bob)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert_eq!(bob, "re: bob");
    }

    /// Keeps what was sent to the sessions it handles.
    struct Recording(Mutex<Vec<String>>);

    #[async_trait]
    impl Notifier for Recording {
        fn handles(&self, session: &str) -> bool {
            session.starts_with("qq:")
        }
        async fn notify(&self, _session: &str, text: &str) -> Result<()> {
            self.0.lock().unwrap().push(text.to_owned());
            Ok(())
        }
    }

    /// What the QQ chat receives besides the turn's reply, when a second
    /// message joins the turn.
    async fn acknowledgements(fail_ack: bool) -> Vec<String> {
        let dir = tempfile::tempdir().unwrap();
        let store = crate::store::Store::open_in_memory().unwrap();
        let tools = crate::tools::BuiltinTools::new(
            dir.path().to_owned(),
            crate::config::ToolsConfig::default(),
            store.clone(),
        )
        .unwrap();
        let go = Arc::new(tokio::sync::Notify::new());
        let agent = Arc::new(Agent {
            model: Gate(go.clone(), AtomicU64::new(0), fail_ack),
            summarizer: None,
            tools,
            store,
            config: crate::config::AgentConfig::default(),
        });
        let gateway = Gateway::new(agent, None, AccessConfig::default());
        let mut guide = Guide::new(None, "off".into(), 0.5, 3);
        guide.ack = true;
        gateway.set_guide(guide);
        let sent = Arc::new(Recording(Mutex::new(Vec::new())));
        gateway.add_notifier(sent.clone());
        let session = "qq:c2c:A";
        let alice = gateway.actor("qq:A", session);
        let turn = tokio::spawn({
            let gateway = gateway.clone();
            let alice = alice.clone();
            async move {
                gateway
                    .chat_unattended_with(alice, session, "first", &[])
                    .await
            }
        });
        while !gateway.running.lock().unwrap().contains_key(session) {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        let queued = gateway
            .chat_unattended_with(alice, session, "second", &[])
            .await
            .unwrap();
        assert_eq!(
            queued, "",
            "answered once it goes in, not with a stock reply"
        );
        go.notify_one();
        let reply = tokio::time::timeout(Duration::from_secs(5), turn)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert_eq!(reply, "re: first\n\nre: second");
        sent.0.lock().unwrap().clone()
    }

    #[tokio::test]
    async fn inserted_messages_get_a_reply_of_their_own() {
        let sent = acknowledgements(false).await;
        assert_eq!(sent.len(), 1, "{sent:?}");
        assert!(sent[0].ends_with("sent just now:\n\nsecond"), "{sent:?}");
    }

    #[tokio::test]
    async fn a_failed_acknowledgement_says_why() {
        let sent = acknowledgements(true).await;
        assert_eq!(
            sent,
            [
                chat::ACK_FAILED
                    .with(&["model request failed with HTTP 402: insufficient credits"])
            ]
        );
    }

    #[test]
    fn session_names_are_bounded_and_printable() {
        assert!(validate_session("工作").is_ok());
        assert!(validate_session("").is_err());
        assert!(validate_session("a\nb").is_err());
        assert!(validate_session(&"x".repeat(65)).is_err());
    }
}
