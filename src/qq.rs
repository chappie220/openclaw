//! Official QQ Bot channel: WebSocket events in, OpenAPI messages out.
//!
//! Private chats (C2C) and group @-mentions each map to one session:
//! `qq:c2c:<user_openid>` and `qq:group:<group_openid>`.

use std::collections::{HashSet, VecDeque};
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use async_trait::async_trait;
use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::Message;

use crate::agent::{Model, Tools};
use crate::attachments::Upload;
use crate::config::QqConfig;
use crate::gateway::{Gateway, Notifier};

/// GROUP_AND_C2C_EVENT: private messages and group @-mentions.
const INTENTS: u64 = 1 << 25;
/// Conservative per-message size; QQ rejects overlong messages without a published limit.
const CHUNK_CHARS: usize = 1500;
/// Passive replies allowed per user message (private chat, group).
const C2C_REPLIES: usize = 4;
const GROUP_REPLIES: usize = 5;
const SEEN_CAPACITY: usize = 1000;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Target {
    User(String),
    Group(String),
}

impl Target {
    pub fn session(&self) -> String {
        match self {
            Target::User(id) => format!("qq:c2c:{id}"),
            Target::Group(id) => format!("qq:group:{id}"),
        }
    }

    pub fn from_session(session: &str) -> Option<Self> {
        if let Some(id) = session.strip_prefix("qq:c2c:") {
            return Some(Target::User(id.to_owned()));
        }
        session
            .strip_prefix("qq:group:")
            .map(|id| Target::Group(id.to_owned()))
    }

    fn path(&self) -> String {
        match self {
            Target::User(id) => format!("/v2/users/{id}/messages"),
            Target::Group(id) => format!("/v2/groups/{id}/messages"),
        }
    }

    fn max_replies(&self) -> usize {
        match self {
            Target::User(_) => C2C_REPLIES,
            Target::Group(_) => GROUP_REPLIES,
        }
    }
}

pub struct QqBot {
    http: reqwest::Client,
    config: QqConfig,
    secret: String,
    token: tokio::sync::Mutex<Option<(String, Instant)>>,
    seen: Mutex<(HashSet<String>, VecDeque<String>)>,
}

/// An incoming message the agent should answer.
#[derive(Debug, Clone, PartialEq)]
struct Incoming {
    id: String,
    target: Target,
    sender: String,
    text: String,
    files: Vec<QqFile>,
    /// Voice messages QQ transcribed itself.
    voice: Vec<String>,
}

/// A file attached to a QQ message, still on QQ's servers.
#[derive(Debug, Clone, PartialEq)]
struct QqFile {
    name: String,
    mime: Option<String>,
    url: String,
}

/// Whether a file `url` from a QQ event may be downloaded: https on QQ's own
/// media hosts, so an event cannot make the bot fetch arbitrary addresses.
/// The API host is allowed too, which lets tests serve files locally.
fn download_allowed(url: &reqwest::Url, api_base: &str) -> bool {
    let Some(host) = url.host_str() else {
        return false;
    };
    let api = reqwest::Url::parse(api_base).ok();
    if api
        .as_ref()
        .is_some_and(|api| api.host_str() == Some(host) && api.scheme() == url.scheme())
    {
        return true;
    }
    url.scheme() == "https"
        && ["qq.com", "qq.com.cn", "qpic.cn"]
            .iter()
            .any(|domain| host == *domain || host.ends_with(&format!(".{domain}")))
}

impl QqBot {
    pub fn new(config: QqConfig) -> Result<Arc<Self>> {
        if config.app_id.trim().is_empty() {
            bail!("qq.app_id is empty");
        }
        let secret = config
            .app_secret()
            .context("no QQ app secret: set QQ_APP_SECRET or qq.app_secret")?;
        let http = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(15))
            .timeout(Duration::from_secs(30))
            .build()?;
        Ok(Arc::new(Self {
            http,
            config,
            secret,
            token: tokio::sync::Mutex::new(None),
            seen: Mutex::new((HashSet::new(), VecDeque::new())),
        }))
    }

    /// Logs in and asks for the event gateway, as `run` does: `doctor`'s
    /// proof that the app id, secret and permissions work.
    pub async fn check(&self) -> Result<String> {
        self.access_token().await?;
        let gateway = self.api(reqwest::Method::GET, "/gateway/bot", None).await?;
        Ok(match gateway["shards"].as_u64() {
            Some(shards) => format!("logged in, gateway ready ({shards} shard)"),
            None => "logged in, gateway ready".into(),
        })
    }

    /// Cached access token, renewed a minute before it expires.
    async fn access_token(&self) -> Result<String> {
        let mut cached = self.token.lock().await;
        if let Some((token, expires)) = cached.as_ref()
            && Instant::now() + Duration::from_secs(60) < *expires
        {
            return Ok(token.clone());
        }
        let body: Value = self
            .http
            .post(&self.config.token_url)
            .json(&json!({ "appId": self.config.app_id, "clientSecret": self.secret }))
            .send()
            .await
            .context("QQ access token request failed")?
            .json()
            .await
            .context("QQ access token response is not JSON")?;
        let token = body["access_token"]
            .as_str()
            .with_context(|| format!("QQ access token refused: {}", describe(&body)))?
            .to_owned();
        // The documented example returns expires_in as a string.
        let ttl = body["expires_in"]
            .as_u64()
            .or_else(|| body["expires_in"].as_str().and_then(|s| s.parse().ok()))
            .unwrap_or(7200);
        *cached = Some((token.clone(), Instant::now() + Duration::from_secs(ttl)));
        Ok(token)
    }

    /// Downloads a message's file, up to the attachment size limit.
    async fn download(&self, file: &QqFile) -> Result<Upload> {
        // QQ sometimes sends attachment URLs without a scheme.
        let raw = if file.url.contains("://") {
            file.url.clone()
        } else {
            format!("https://{}", file.url.trim_start_matches('/'))
        };
        let url =
            reqwest::Url::parse(&raw).with_context(|| format!("bad file URL for {}", file.name))?;
        if !download_allowed(&url, &self.config.api_base) {
            bail!("{} is not on a QQ media host", file.name);
        }
        let mut response = self
            .http
            .get(url)
            .send()
            .await
            .and_then(reqwest::Response::error_for_status)
            .with_context(|| format!("cannot download {}", file.name))?;
        let mut data = Vec::new();
        while let Some(chunk) = response.chunk().await? {
            data.extend_from_slice(&chunk);
            if data.len() > crate::attachments::MAX_FILE_BYTES {
                bail!("{} is larger than the attachment limit", file.name);
            }
        }
        Ok(Upload {
            name: file.name.clone(),
            mime: file.mime.clone(),
            data,
        })
    }

    async fn api(&self, method: reqwest::Method, path: &str, body: Option<Value>) -> Result<Value> {
        let token = self.access_token().await?;
        let mut request = self
            .http
            .request(method, format!("{}{path}", self.config.api_base))
            .header("Authorization", format!("QQBot {token}"));
        if let Some(body) = body {
            request = request.json(&body);
        }
        let response = request
            .send()
            .await
            .with_context(|| format!("QQ API {path} failed"))?;
        let status = response.status();
        let body: Value = response.json().await.unwrap_or(Value::Null);
        if !status.is_success() {
            bail!("QQ API {path} returned HTTP {status}: {}", describe(&body));
        }
        Ok(body)
    }

    /// Sends `text`, replying to `reply_to` while QQ still accepts passive
    /// replies and falling back to active messages after that.
    pub async fn send(&self, target: &Target, text: &str, reply_to: Option<&str>) -> Result<()> {
        let mut chunks = split(text, CHUNK_CHARS);
        let max = target.max_replies();
        if chunks.len() > max {
            chunks.truncate(max);
            if let Some(last) = chunks.last_mut() {
                last.push_str("\n…（内容过长，已截断）");
            }
        }
        let mut passive = reply_to;
        for (index, chunk) in chunks.iter().enumerate() {
            let mut body = json!({ "msg_type": 0, "content": chunk });
            if let Some(id) = passive {
                body["msg_id"] = json!(id);
                body["msg_seq"] = json!(index + 1);
            }
            match self
                .api(reqwest::Method::POST, &target.path(), Some(body))
                .await
            {
                Ok(_) => {}
                // An expired reply window rejects msg_id; the rest go out as active messages.
                Err(err) if passive.is_some() => {
                    eprintln!("qq: passive reply failed ({err:#}); sending as an active message");
                    passive = None;
                    self.api(
                        reqwest::Method::POST,
                        &target.path(),
                        Some(json!({ "msg_type": 0, "content": chunk })),
                    )
                    .await?;
                }
                Err(err) => return Err(err),
            }
        }
        Ok(())
    }

    /// QQ may push an event more than once; remember recent message ids.
    fn first_sight(&self, id: &str) -> bool {
        let mut seen = self.seen.lock().unwrap_or_else(|p| p.into_inner());
        if !seen.0.insert(id.to_owned()) {
            return false;
        }
        seen.1.push_back(id.to_owned());
        if seen.1.len() > SEEN_CAPACITY
            && let Some(old) = seen.1.pop_front()
        {
            seen.0.remove(&old);
        }
        true
    }

    fn allowed(&self, incoming: &Incoming) -> bool {
        let allow = &self.config.allow;
        allow.is_empty()
            || allow.contains(&incoming.sender)
            || matches!(&incoming.target, Target::Group(id) if allow.contains(id))
    }
}

fn describe(body: &Value) -> String {
    body.get("message")
        .and_then(Value::as_str)
        .map_or_else(|| body.to_string(), str::to_owned)
}

/// Splits on character boundaries, preferring line breaks.
fn split(text: &str, max_chars: usize) -> Vec<String> {
    let text = text.trim();
    if text.is_empty() {
        return vec![crate::i18n::chat::EMPTY.now().into()];
    }
    let mut chunks = Vec::new();
    let mut rest: Vec<char> = text.chars().collect();
    while rest.len() > max_chars {
        let window = &rest[..max_chars];
        let cut = window
            .iter()
            .rposition(|c| *c == '\n')
            .filter(|i| *i > max_chars / 2)
            .map_or(max_chars, |i| i + 1);
        chunks.push(rest.drain(..cut).collect::<String>().trim_end().to_owned());
    }
    chunks.push(rest.into_iter().collect());
    chunks
}

fn parse_incoming(event: &str, d: &Value) -> Option<Incoming> {
    let id = d["id"].as_str()?.to_owned();
    let text = d["content"].as_str().unwrap_or("").trim().to_owned();
    let (target, sender) = match event {
        "C2C_MESSAGE_CREATE" => {
            let user = d["author"]["user_openid"]
                .as_str()
                .or_else(|| d["author"]["id"].as_str())?;
            (Target::User(user.to_owned()), user.to_owned())
        }
        "GROUP_AT_MESSAGE_CREATE" => {
            let group = d["group_openid"].as_str()?;
            let member = d["author"]["member_openid"]
                .as_str()
                .or_else(|| d["author"]["id"].as_str())?;
            (Target::Group(group.to_owned()), member.to_owned())
        }
        _ => return None,
    };
    let mut files = Vec::new();
    let mut voice = Vec::new();
    for a in d["attachments"].as_array().into_iter().flatten() {
        let mime = a["content_type"].as_str().map(str::to_owned);
        let is_voice = mime
            .as_deref()
            .is_some_and(|m| m == "voice" || m.starts_with("audio/"));
        if is_voice {
            // QQ's own speech recognition, free and already done.
            if let Some(said) = a["asr_refer_text"]
                .as_str()
                .filter(|t| !t.trim().is_empty())
            {
                voice.push(said.trim().to_owned());
                continue;
            }
            // The original is SILK, which models do not take; QQ also offers WAV.
            if let Some(wav) = a["voice_wav_url"].as_str().filter(|u| !u.is_empty()) {
                files.push(QqFile {
                    name: "qq-voice.wav".into(),
                    mime: Some("audio/wav".into()),
                    url: wav.to_owned(),
                });
                continue;
            }
        }
        let Some(url) = a["url"].as_str().filter(|u| !u.is_empty()) else {
            continue;
        };
        let name = a["filename"]
            .as_str()
            .filter(|n| !n.is_empty())
            .map(str::to_owned)
            .unwrap_or_else(|| {
                let ext = match mime.as_deref() {
                    Some("voice") => "silk",
                    m => m.and_then(|m| m.split('/').nth(1)).unwrap_or("bin"),
                };
                format!("qq-file.{ext}")
            });
        // "voice" is not a MIME type; the name says what the file is.
        let mime = mime.filter(|m| m.contains('/'));
        files.push(QqFile {
            name,
            mime,
            url: url.to_owned(),
        });
    }
    if text.is_empty() && files.is_empty() && voice.is_empty() {
        return None;
    }
    Some(Incoming {
        id,
        target,
        sender,
        text,
        files,
        voice,
    })
}

#[async_trait]
impl Notifier for QqBot {
    fn handles(&self, session: &str) -> bool {
        Target::from_session(session).is_some()
    }

    async fn notify(&self, session: &str, text: &str) -> Result<()> {
        let target = Target::from_session(session).context("not a QQ session")?;
        self.send(&target, text, None).await
    }
}

/// What a dropped connection needs to resume where it stopped.
#[derive(Default)]
struct Resume {
    session_id: Option<String>,
    seq: Arc<AtomicI64>,
}

/// Keeps the bot connected for the life of the process.
pub async fn run<M: Model + 'static, T: Tools + 'static>(
    bot: Arc<QqBot>,
    gateway: Arc<Gateway<M, T>>,
) {
    let mut resume = Resume::default();
    let mut backoff = Duration::from_secs(1);
    loop {
        let started = Instant::now();
        match connect(&bot, &gateway, &mut resume).await {
            Ok(()) => eprintln!("qq: gateway asked to reconnect"),
            Err(err) => eprintln!("qq: connection ended: {err:#}"),
        }
        if started.elapsed() > Duration::from_secs(60) {
            backoff = Duration::from_secs(1);
        }
        tokio::time::sleep(backoff).await;
        backoff = (backoff * 2).min(Duration::from_secs(60));
    }
}

async fn connect<M: Model + 'static, T: Tools + 'static>(
    bot: &Arc<QqBot>,
    gateway: &Arc<Gateway<M, T>>,
    resume: &mut Resume,
) -> Result<()> {
    let url = bot.api(reqwest::Method::GET, "/gateway/bot", None).await?["url"]
        .as_str()
        .context("QQ gateway response has no url")?
        .to_owned();
    let (socket, _) = tokio_tungstenite::connect_async(&url)
        .await
        .with_context(|| format!("cannot connect to {url}"))?;
    let (mut sink, mut stream) = socket.split();

    let hello = next_payload(&mut stream).await?;
    if hello["op"] != 10 {
        bail!("expected Hello, got {hello}");
    }
    let interval = Duration::from_millis(
        hello["d"]["heartbeat_interval"]
            .as_u64()
            .unwrap_or(30_000)
            .max(1_000),
    );
    let token = format!("QQBot {}", bot.access_token().await?);
    let greeting = match &resume.session_id {
        Some(session_id) => json!({ "op": 6, "d": {
            "token": token, "session_id": session_id, "seq": resume.seq.load(Ordering::Relaxed)
        }}),
        None => json!({ "op": 2, "d": {
            "token": token, "intents": INTENTS, "shard": [0, 1], "properties": {}
        }}),
    };
    sink.send(Message::Text(greeting.to_string().into()))
        .await?;

    let (out, mut outbox) = mpsc::unbounded_channel::<Value>();
    let writer = tokio::spawn(async move {
        while let Some(payload) = outbox.recv().await {
            if sink
                .send(Message::Text(payload.to_string().into()))
                .await
                .is_err()
            {
                break;
            }
        }
    });
    let seq = resume.seq.clone();
    let heartbeat_out = out.clone();
    let heartbeat = tokio::spawn(async move {
        let mut tick = tokio::time::interval(interval);
        tick.tick().await;
        loop {
            tick.tick().await;
            let last = seq.load(Ordering::Relaxed);
            let d = if last > 0 { json!(last) } else { Value::Null };
            if heartbeat_out.send(json!({ "op": 1, "d": d })).is_err() {
                break;
            }
        }
    });

    let result = loop {
        let payload = match next_payload(&mut stream).await {
            Ok(payload) => payload,
            Err(err) => break Err(err),
        };
        if let Some(s) = payload["s"].as_i64() {
            resume.seq.store(s, Ordering::Relaxed);
        }
        match payload["op"].as_i64() {
            Some(0) => {
                let event = payload["t"].as_str().unwrap_or("");
                match event {
                    "READY" => {
                        resume.session_id = payload["d"]["session_id"].as_str().map(str::to_owned);
                        eprintln!(
                            "qq: connected as {}",
                            payload["d"]["user"]["username"].as_str().unwrap_or("bot")
                        );
                    }
                    "RESUMED" => eprintln!("qq: session resumed"),
                    _ => {
                        if let Some(incoming) = parse_incoming(event, &payload["d"]) {
                            handle(bot.clone(), gateway.clone(), incoming);
                        }
                    }
                }
            }
            Some(7) => break Ok(()),
            Some(9) => {
                resume.session_id = None;
                resume.seq.store(0, Ordering::Relaxed);
                break Err(anyhow::anyhow!("session invalidated; identifying again"));
            }
            _ => {}
        }
    };
    heartbeat.abort();
    drop(out);
    writer.abort();
    result
}

async fn next_payload<S>(stream: &mut S) -> Result<Value>
where
    S: futures_util::Stream<Item = Result<Message, tokio_tungstenite::tungstenite::Error>> + Unpin,
{
    loop {
        match stream
            .next()
            .await
            .context("QQ gateway closed the connection")??
        {
            Message::Text(text) => {
                return serde_json::from_str(&text).context("malformed QQ gateway payload");
            }
            Message::Close(frame) => bail!("QQ gateway closed: {frame:?}"),
            _ => continue,
        }
    }
}

fn handle<M: Model + 'static, T: Tools + 'static>(
    bot: Arc<QqBot>,
    gateway: Arc<Gateway<M, T>>,
    incoming: Incoming,
) {
    if !bot.first_sight(&incoming.id) {
        return;
    }
    if !bot.allowed(&incoming) {
        eprintln!(
            "qq: ignored {} from {} (not in qq.allow)",
            incoming.target.session(),
            incoming.sender
        );
        return;
    }
    eprintln!(
        "qq: message from {} in {}",
        incoming.sender,
        incoming.target.session()
    );
    tokio::spawn(async move {
        let session = incoming.target.session();
        let actor = gateway.actor(&format!("qq:{}", incoming.sender), &session);
        let mut uploads = Vec::new();
        let mut problems = Vec::new();
        for file in &incoming.files {
            match bot.download(file).await {
                Ok(upload) => uploads.push(upload),
                Err(err) => problems.push(format!("{err:#}")),
            }
        }
        let heard: Vec<String> = incoming
            .voice
            .iter()
            .map(|t| crate::voice::qq_transcript(t))
            .collect();
        let text = crate::voice::with_notes(&incoming.text, &heard);
        let text = crate::attachments::with_problems(&text, &problems);
        let reply = match gateway
            .chat_unattended_with(actor, &session, &text, &uploads)
            .await
        {
            Ok(text) => text,
            Err(err) => crate::i18n::chat::FAILED.with(&[&format!("{err:#}")]),
        };
        // A message that joined a running turn is answered when it goes in.
        if reply.is_empty() {
            return;
        }
        if let Err(err) = bot.send(&incoming.target, &reply, Some(&incoming.id)).await {
            eprintln!("qq: cannot reply in {session}: {err:#}");
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::Agent;
    use crate::config::AgentConfig;
    use crate::llm::{ChatMessage, Completion, ToolCall, ToolSpec};
    use crate::store::Store;

    struct Echo;

    #[async_trait]
    impl Model for Echo {
        async fn complete(
            &self,
            messages: &[ChatMessage],
            _tools: &[ToolSpec],
            _on_text: &mut (dyn for<'t> FnMut(&'t str) + Send),
        ) -> Result<Completion> {
            let last = messages
                .last()
                .and_then(|m| m.content.clone())
                .unwrap_or_default();
            Ok(Completion {
                text: format!("回声:{last}"),
                ..Default::default()
            })
        }
    }

    struct NoTools;

    #[async_trait]
    impl Tools for NoTools {
        fn specs(&self) -> Vec<ToolSpec> {
            Vec::new()
        }
        async fn call(&self, _call: &ToolCall) -> String {
            String::new()
        }
    }

    type Sent = Arc<Mutex<Vec<(String, Value)>>>;

    /// Fake QQ OpenAPI + WebSocket gateway following the documented protocol.
    async fn fake_qq(sent: Sent) -> String {
        use axum::extract::ws::{Message as WsMessage, WebSocketUpgrade};
        use axum::extract::{Path, State};
        use axum::http::{HeaderMap, StatusCode};
        use axum::routing::{get, post};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let ws_url = base.replace("http://", "ws://") + "/websocket";
        fn authorized(headers: &HeaderMap) -> bool {
            headers
                .get("authorization")
                .is_some_and(|v| v == "QQBot T1")
        }
        let app = axum::Router::new()
            .route(
                "/app/getAppAccessToken",
                post(|axum::Json(body): axum::Json<Value>| async move {
                    assert_eq!(body, json!({"appId": "1", "clientSecret": "s"}));
                    axum::Json(json!({"access_token": "T1", "expires_in": "7200"}))
                }),
            )
            .route(
                "/gateway/bot",
                get(move |headers: HeaderMap| async move {
                    assert!(authorized(&headers));
                    axum::Json(json!({"url": ws_url}))
                }),
            )
            .route(
                "/websocket",
                get(|upgrade: WebSocketUpgrade| async move {
                    upgrade.on_upgrade(|mut ws| async move {
                        let send = |v: Value| WsMessage::Text(v.to_string().into());
                        ws.send(send(json!({"op": 10, "d": {"heartbeat_interval": 1000}}))).await.unwrap();
                        let Some(Ok(WsMessage::Text(identify))) = ws.recv().await else { panic!("no identify") };
                        let identify: Value = serde_json::from_str(&identify).unwrap();
                        assert_eq!(identify["op"], 2);
                        assert_eq!(identify["d"]["token"], "QQBot T1");
                        assert_eq!(identify["d"]["intents"], 1 << 25);
                        let c2c = json!({"id": "m1", "author": {"user_openid": "U1"}, "content": "你好"});
                        for (s, payload) in [
                            (1, json!({"t": "READY", "d": {"session_id": "S1", "user": {"username": "bot"}}})),
                            (2, json!({"t": "C2C_MESSAGE_CREATE", "d": c2c})),
                            (3, json!({"t": "C2C_MESSAGE_CREATE", "d": c2c})),
                            (4, json!({"t": "GROUP_AT_MESSAGE_CREATE", "d": {
                                "id": "m2", "group_openid": "G1", "author": {"member_openid": "M9"}, "content": " 群里问 "}})),
                            (5, json!({"t": "C2C_MESSAGE_CREATE", "d": {
                                "id": "m3", "author": {"user_openid": "STRANGER"}, "content": "hi"}})),
                        ] {
                            let mut frame = payload;
                            frame["op"] = json!(0);
                            frame["s"] = json!(s);
                            ws.send(send(frame)).await.unwrap();
                        }
                        // Keep the connection open, acknowledging heartbeats.
                        while let Some(Ok(msg)) = ws.recv().await {
                            if let WsMessage::Text(text) = msg
                                && serde_json::from_str::<Value>(&text).unwrap()["op"] == 1
                            {
                                let _ = ws.send(send(json!({"op": 11}))).await;
                            }
                        }
                    })
                }),
            )
            .route(
                "/v2/{kind}/{id}/messages",
                post(
                    |State(sent): State<Sent>, Path((kind, id)): Path<(String, String)>, headers: HeaderMap, axum::Json(body): axum::Json<Value>| async move {
                        assert!(authorized(&headers));
                        sent.lock().unwrap().push((format!("/v2/{kind}/{id}/messages"), body.clone()));
                        // Groups reject the passive reply as expired, forcing the active fallback.
                        if kind == "groups" && body.get("msg_id").is_some() {
                            return (StatusCode::BAD_REQUEST, axum::Json(json!({"message": "msg expired"})));
                        }
                        (StatusCode::OK, axum::Json(json!({"id": "out", "timestamp": "2026-10-09T10:00:00+08:00"})))
                    },
                ),
            )
            .with_state(sent);
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        base
    }

    #[tokio::test]
    async fn answers_private_and_group_messages_over_the_gateway() {
        let sent: Sent = Arc::default();
        let base = fake_qq(sent.clone()).await;
        let bot = QqBot::new(QqConfig {
            enabled: true,
            app_id: "1".into(),
            app_secret: Some("s".into()),
            allow: vec!["U1".into(), "G1".into()],
            api_base: base.clone(),
            token_url: format!("{base}/app/getAppAccessToken"),
        })
        .unwrap();
        let agent = Arc::new(Agent {
            model: Echo,
            summarizer: None,
            vision: None,
            voice: None,
            tools: NoTools,
            store: Store::open_in_memory().unwrap(),
            config: AgentConfig::default(),
        });
        let gateway = Gateway::new(agent.clone(), None, Default::default());
        tokio::spawn(run(bot, gateway));

        let deadline = Instant::now() + Duration::from_secs(10);
        while sent.lock().unwrap().len() < 3 && Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        // Let a duplicate or disallowed message surface if it was going to.
        tokio::time::sleep(Duration::from_millis(500)).await;
        let mut sent = sent.lock().unwrap().clone();
        sent.sort_by(|a, b| a.0.cmp(&b.0));
        assert_eq!(
            sent,
            vec![
                (
                    "/v2/groups/G1/messages".into(),
                    json!({"msg_type": 0, "content": "回声:群里问", "msg_id": "m2", "msg_seq": 1})
                ),
                (
                    "/v2/groups/G1/messages".into(),
                    json!({"msg_type": 0, "content": "回声:群里问"})
                ),
                (
                    "/v2/users/U1/messages".into(),
                    json!({"msg_type": 0, "content": "回声:你好", "msg_id": "m1", "msg_seq": 1})
                ),
            ]
        );
        let id = agent.store.session_id("qq:c2c:U1").unwrap();
        assert_eq!(agent.store.history(id, 10).unwrap().len(), 2);
    }

    #[test]
    fn sessions_round_trip_targets() {
        let user = Target::User("U1".into());
        assert_eq!(Target::from_session(&user.session()), Some(user));
        assert_eq!(Target::from_session("main"), None);
    }

    #[test]
    fn splits_long_text_on_line_breaks_within_the_limit() {
        let text = format!("{}\n{}", "甲".repeat(8), "乙".repeat(8));
        let chunks = split(&text, 10);
        assert_eq!(chunks, vec!["甲".repeat(8), "乙".repeat(8)]);
        assert!(
            split(&"x".repeat(25), 10)
                .iter()
                .all(|c| c.chars().count() <= 10)
        );
        assert_eq!(split("  ", 10), vec![crate::i18n::chat::EMPTY.now()]);
    }

    #[test]
    fn parses_attachments_even_without_text() {
        let image = json!({"id": "m4", "author": {"user_openid": "U"}, "content": "",
        "attachments": [
            {"content_type": "image/jpeg", "filename": "cat.jpg", "url": "multimedia.nt.qq.com.cn/download?id=1"},
            {"content_type": "image/png", "url": "https://gchat.qpic.cn/x"},
            {"content_type": "image/png", "url": ""}
        ]});
        let parsed = parse_incoming("C2C_MESSAGE_CREATE", &image).unwrap();
        assert_eq!(parsed.text, "");
        assert_eq!(
            parsed.files,
            [
                QqFile {
                    name: "cat.jpg".into(),
                    mime: Some("image/jpeg".into()),
                    url: "multimedia.nt.qq.com.cn/download?id=1".into(),
                },
                QqFile {
                    name: "qq-file.png".into(),
                    mime: Some("image/png".into()),
                    url: "https://gchat.qpic.cn/x".into(),
                },
            ]
        );
    }

    #[test]
    fn voice_messages_use_qq_transcripts_or_wav() {
        let voice = json!({"id": "m5", "author": {"user_openid": "U"}, "content": "",
        "attachments": [
            {"content_type": "voice", "filename": "a.amr", "url": "https://multimedia.nt.qq.com.cn/a",
             "asr_refer_text": " 你好 "},
            {"content_type": "voice", "url": "https://multimedia.nt.qq.com.cn/b",
             "voice_wav_url": "https://multimedia.nt.qq.com.cn/b.wav", "asr_refer_text": ""},
            {"content_type": "voice", "url": "https://multimedia.nt.qq.com.cn/c"}
        ]});
        let parsed = parse_incoming("C2C_MESSAGE_CREATE", &voice).unwrap();
        assert_eq!(parsed.voice, ["你好"]);
        assert_eq!(
            parsed.files,
            [
                QqFile {
                    name: "qq-voice.wav".into(),
                    mime: Some("audio/wav".into()),
                    url: "https://multimedia.nt.qq.com.cn/b.wav".into(),
                },
                QqFile {
                    name: "qq-file.silk".into(),
                    mime: None,
                    url: "https://multimedia.nt.qq.com.cn/c".into(),
                },
            ]
        );
    }

    #[test]
    fn downloads_only_from_qq_media_hosts() {
        let api = "https://api.bot.qq.com";
        let ok = |u: &str| download_allowed(&reqwest::Url::parse(u).unwrap(), api);
        assert!(ok("https://multimedia.nt.qq.com.cn/download?x"));
        assert!(ok("https://gchat.qpic.cn/a.jpg"));
        assert!(ok("https://api.bot.qq.com/file"));
        assert!(!ok("http://multimedia.nt.qq.com.cn/download"), "https only");
        assert!(!ok("https://evilqq.com/x"));
        assert!(!ok("https://qq.com.evil.example/x"));
        assert!(!ok("https://169.254.169.254/latest/meta-data"));
        assert!(download_allowed(
            &reqwest::Url::parse("http://127.0.0.1:9/f").unwrap(),
            "http://127.0.0.1:9"
        ));
    }

    #[tokio::test]
    async fn downloads_a_file_within_the_size_limit() {
        let app = axum::Router::new()
            .route("/small", axum::routing::get(|| async { "hello" }))
            .route(
                "/huge",
                axum::routing::get(|| async { vec![0u8; crate::attachments::MAX_FILE_BYTES + 1] }),
            );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        tokio::spawn(async move { axum::serve(listener, app).await });
        let bot = QqBot::new(QqConfig {
            app_id: "1".into(),
            app_secret: Some("s".into()),
            api_base: base.clone(),
            ..QqConfig::default()
        })
        .unwrap();
        let file = |path: &str| QqFile {
            name: "f.txt".into(),
            mime: Some("text/plain".into()),
            url: format!("{base}{path}"),
        };
        let upload = bot.download(&file("/small")).await.unwrap();
        assert_eq!(upload.data, b"hello");
        assert_eq!(upload.mime.as_deref(), Some("text/plain"));
        let err = bot.download(&file("/huge")).await.unwrap_err();
        assert!(err.to_string().contains("limit"), "{err}");
        let elsewhere = QqFile {
            url: "http://example.com/x".into(),
            ..file("/small")
        };
        assert!(bot.download(&elsewhere).await.is_err());
    }

    #[test]
    fn parses_private_and_group_events() {
        let c2c = json!({"id": "m1", "author": {"user_openid": "U1"}, "content": " 你好 "});
        assert_eq!(
            parse_incoming("C2C_MESSAGE_CREATE", &c2c),
            Some(Incoming {
                id: "m1".into(),
                target: Target::User("U1".into()),
                sender: "U1".into(),
                text: "你好".into(),
                files: Vec::new(),
                voice: Vec::new(),
            })
        );
        let group = json!({"id": "m2", "group_openid": "G1", "author": {"member_openid": "M1"}, "content": " /天气 "});
        let parsed = parse_incoming("GROUP_AT_MESSAGE_CREATE", &group).unwrap();
        assert_eq!(
            (parsed.target, parsed.sender.as_str(), parsed.text.as_str()),
            (Target::Group("G1".into()), "M1", "/天气")
        );
        assert!(
            parse_incoming(
                "C2C_MESSAGE_CREATE",
                &json!({"id": "m3", "author": {"user_openid": "U"}, "content": ""})
            )
            .is_none()
        );
    }
}
