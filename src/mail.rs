//! Email channel: IMAP polling in, SMTP replies out. Each allowed sender
//! address is one session, `mail:<address>`.

use std::net::IpAddr;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context as TaskContext, Poll};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use async_trait::async_trait;
use futures_util::TryStreamExt;
use lettre::message::header::{ContentType, Header, HeaderName, HeaderValue};
use lettre::transport::smtp::authentication::Credentials;
use lettre::{AsyncSmtpTransport, AsyncTransport, Message as Email, Tokio1Executor};
use mail_parser::MessageParser;
use rusqlite::params;
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::net::TcpStream;

use crate::agent::{Model, Tools};
use crate::config::{MailConfig, MailSecurity};
use crate::gateway::{Gateway, Notifier};
use crate::store::{Store, now};

/// Bounds one poll so a flooded inbox cannot queue unbounded turns.
const MAX_PER_POLL: usize = 20;
const MAX_BODY_CHARS: usize = 20_000;
const SESSION_PREFIX: &str = "mail:";

/// An email the agent should answer.
#[derive(Debug, Clone, PartialEq)]
pub struct Incoming {
    pub message_id: String,
    pub sender: String,
    pub subject: String,
    pub text: String,
    pub references: Vec<String>,
}

/// Why an email is not answered; logged so the operator can adjust `mail.allow`.
#[derive(Debug, PartialEq)]
pub enum Skip {
    Unparseable,
    Automated,
    FromSelf,
    NotAllowed(String),
    Empty,
}

pub struct MailBot {
    config: MailConfig,
    password: String,
    from: String,
    smtp: AsyncSmtpTransport<Tokio1Executor>,
}

fn is_loopback(host: &str) -> bool {
    host == "localhost" || host.parse::<IpAddr>().is_ok_and(|ip| ip.is_loopback())
}

/// Third-party providers by address domain: (IMAP host, SMTP host, SMTP port, SMTP security).
type Preset = (
    &'static [&'static str],
    &'static str,
    &'static str,
    u16,
    MailSecurity,
);
const PRESETS: &[Preset] = &[
    (
        &["qq.com", "foxmail.com", "vip.qq.com"],
        "imap.qq.com",
        "smtp.qq.com",
        465,
        MailSecurity::Tls,
    ),
    (
        &["163.com"],
        "imap.163.com",
        "smtp.163.com",
        465,
        MailSecurity::Tls,
    ),
    (
        &["126.com"],
        "imap.126.com",
        "smtp.126.com",
        465,
        MailSecurity::Tls,
    ),
    (
        &["yeah.net"],
        "imap.yeah.net",
        "smtp.yeah.net",
        465,
        MailSecurity::Tls,
    ),
    (
        &["gmail.com", "googlemail.com"],
        "imap.gmail.com",
        "smtp.gmail.com",
        465,
        MailSecurity::Tls,
    ),
    (
        &["icloud.com", "me.com", "mac.com"],
        "imap.mail.me.com",
        "smtp.mail.me.com",
        587,
        MailSecurity::Starttls,
    ),
    (
        &["aliyun.com"],
        "imap.aliyun.com",
        "smtp.aliyun.com",
        465,
        MailSecurity::Tls,
    ),
];

/// Fills in a known provider's servers for whichever host the operator left empty.
fn apply_preset(config: &mut MailConfig) -> Result<()> {
    if !config.imap_host.is_empty() && !config.smtp_host.is_empty() {
        return Ok(());
    }
    let domain = config
        .username
        .rsplit_once('@')
        .map(|(_, d)| d.to_lowercase())
        .unwrap_or_default();
    if matches!(
        domain.as_str(),
        "outlook.com" | "hotmail.com" | "live.com" | "msn.com"
    ) {
        bail!(
            "Outlook/Hotmail accounts only allow OAuth sign-in, which is not supported; use another provider"
        );
    }
    let preset = PRESETS
        .iter()
        .find(|(domains, ..)| domains.contains(&domain.as_str()));
    let Some((_, imap, smtp, smtp_port, smtp_security)) = preset else {
        return Ok(()); // Unknown domain (e.g. a company mailbox): hosts must be configured.
    };
    if config.imap_host.is_empty() {
        config.imap_host = (*imap).into();
        config.imap_port = 993;
        config.imap_security = MailSecurity::Tls;
    }
    if config.smtp_host.is_empty() {
        config.smtp_host = (*smtp).into();
        config.smtp_port = *smtp_port;
        config.smtp_security = *smtp_security;
    }
    Ok(())
}

pub fn session_for(address: &str) -> String {
    format!("{SESSION_PREFIX}{}", address.to_lowercase())
}

impl MailBot {
    pub fn new(config: MailConfig) -> Result<Arc<Self>> {
        if config.allow.is_empty() {
            bail!("mail.allow is empty; list the addresses (or @domains) that may email the bot");
        }
        Self::connect_only(config)
    }

    /// Builds the client without the allow-list, for `mail check`.
    pub fn connect_only(mut config: MailConfig) -> Result<Arc<Self>> {
        apply_preset(&mut config)?;
        if config.imap_host.is_empty() || config.smtp_host.is_empty() || config.username.is_empty()
        {
            bail!(
                "mail.username is required, and mail.imap_host/smtp_host unless the address is from a known provider"
            );
        }
        for (host, security) in [
            (&config.imap_host, config.imap_security),
            (&config.smtp_host, config.smtp_security),
        ] {
            if security == MailSecurity::None && !is_loopback(host) {
                bail!(
                    "mail security \"none\" sends the password in plain text; it is only allowed for loopback hosts, not {host}"
                );
            }
        }
        if config.imap_security == MailSecurity::Starttls {
            bail!(
                "mail.imap_security must be \"tls\" (port 993) or \"none\"; IMAP STARTTLS is not supported"
            );
        }
        let password = config
            .password()
            .context("no mail password: set MAIL_PASSWORD or mail.password")?;
        let from = config
            .from
            .clone()
            .unwrap_or_else(|| config.username.clone());
        let credentials = Credentials::new(config.username.clone(), password.clone());
        let smtp = match config.smtp_security {
            MailSecurity::Tls => AsyncSmtpTransport::<Tokio1Executor>::relay(&config.smtp_host)?,
            MailSecurity::Starttls => {
                AsyncSmtpTransport::<Tokio1Executor>::starttls_relay(&config.smtp_host)?
            }
            MailSecurity::None => {
                AsyncSmtpTransport::<Tokio1Executor>::builder_dangerous(&config.smtp_host)
            }
        }
        .port(config.smtp_port)
        .credentials(credentials)
        .timeout(Some(Duration::from_secs(30)))
        .build();
        Ok(Arc::new(Self {
            config,
            password,
            from,
            smtp,
        }))
    }

    fn allowed(&self, sender: &str) -> bool {
        let sender = sender.to_lowercase();
        self.config.allow.iter().any(|entry| {
            let entry = entry.trim().to_lowercase();
            if entry.starts_with('@') {
                sender.ends_with(&entry)
            } else {
                sender == entry
            }
        })
    }

    /// Turns raw RFC 5322 bytes into a message to answer, or the reason to skip it.
    pub fn classify(&self, raw: &[u8]) -> Result<Incoming, Skip> {
        let message = MessageParser::default()
            .parse(raw)
            .ok_or(Skip::Unparseable)?;
        let sender = message
            .from()
            .and_then(|from| from.first())
            .and_then(|addr| addr.address())
            .ok_or(Skip::Unparseable)?
            .to_lowercase();
        let header = |name: &str| message.header_raw(name).map(|v| v.trim().to_lowercase());
        // Answering auto-replies or list traffic risks mail loops between robots.
        let automated = header("Auto-Submitted").is_some_and(|v| v != "no")
            || header("Precedence").is_some_and(|v| matches!(v.as_str(), "bulk" | "list" | "junk"))
            || header("List-Id").is_some();
        if automated {
            return Err(Skip::Automated);
        }
        if sender == self.from.to_lowercase() || sender == self.config.username.to_lowercase() {
            return Err(Skip::FromSelf);
        }
        if !self.allowed(&sender) {
            return Err(Skip::NotAllowed(sender));
        }
        let subject = message.subject().unwrap_or("").trim().to_owned();
        let body = message
            .body_text(0)
            .map(|t| strip_quoted(&t))
            .unwrap_or_default();
        let text = match (subject.is_empty(), body.is_empty()) {
            (true, true) => return Err(Skip::Empty),
            (true, false) => body,
            (false, true) => subject.clone(),
            (false, false) => format!("{subject}\n\n{body}"),
        };
        let text: String = text.chars().take(MAX_BODY_CHARS).collect();
        let message_id = message
            .message_id()
            .map(|id| format!("<{id}>"))
            .unwrap_or_default();
        let mut references: Vec<String> = message
            .references()
            .as_text_list()
            .map(|ids| ids.iter().map(|id| format!("<{id}>")).collect())
            .or_else(|| {
                message
                    .references()
                    .as_text()
                    .map(|id| vec![format!("<{id}>")])
            })
            .unwrap_or_default();
        if !message_id.is_empty() {
            references.push(message_id.clone());
        }
        Ok(Incoming {
            message_id,
            sender,
            subject,
            text,
            references,
        })
    }

    async fn send(
        &self,
        to: &str,
        subject: &str,
        text: &str,
        thread: Option<&Incoming>,
        message_id: Option<&str>,
    ) -> Result<(), SendError> {
        let mut builder = Email::builder()
            .from(
                self.from
                    .parse()
                    .with_context(|| format!("invalid mail.from {:?}", self.from))?,
            )
            .to(to
                .parse()
                .with_context(|| format!("invalid recipient {to:?}"))?)
            .subject(subject)
            .header(ContentType::TEXT_PLAIN)
            .header(AutoSubmitted)
            .message_id(message_id.map(str::to_owned));
        if let Some(thread) = thread.filter(|t| !t.message_id.is_empty()) {
            builder = builder
                .in_reply_to(thread.message_id.clone())
                .references(thread.references.join(" "));
        }
        let email = builder
            .body(text.to_owned())
            .map_err(|e| SendError::Permanent(e.into()))?;
        self.smtp.send(email).await.map_err(|err| {
            // A 5xx means the server refused it for good; anything else may pass later.
            let permanent = err.is_permanent();
            let err = anyhow::Error::from(err).context("SMTP send failed");
            if permanent {
                SendError::Permanent(err)
            } else {
                SendError::Retry(err)
            }
        })?;
        Ok(())
    }

    /// Sends the reply under the stable `message_id` recorded before the attempt,
    /// so a resend after a failure is recognizably the same email.
    pub async fn reply(
        &self,
        incoming: &Incoming,
        text: &str,
        message_id: &str,
    ) -> Result<(), SendError> {
        let subject = if incoming.subject.is_empty() {
            "Re: OpenClaw".to_owned()
        } else if incoming.subject.to_lowercase().starts_with("re:") {
            incoming.subject.clone()
        } else {
            format!("Re: {}", incoming.subject)
        };
        // Replies go to From, never Reply-To: the allow-list checked From.
        self.send(
            &incoming.sender,
            &subject,
            text,
            Some(incoming),
            Some(message_id),
        )
        .await
    }

    async fn imap_session(&self) -> Result<async_imap::Session<MailStream>> {
        let target = format!("{}:{}", self.config.imap_host, self.config.imap_port);
        let tcp = tokio::time::timeout(
            Duration::from_secs(30),
            TcpStream::connect((self.config.imap_host.as_str(), self.config.imap_port)),
        )
        .await
        .with_context(|| format!("IMAP connect to {target} timed out"))?
        .with_context(|| format!("cannot connect to {target}"))?;
        let stream = match self.config.imap_security {
            MailSecurity::None => MailStream::Plain(tcp),
            _ => MailStream::Tls(Box::new(tls_connect(&self.config.imap_host, tcp).await?)),
        };
        let mut client = async_imap::Client::new(stream);
        client
            .read_response()
            .await?
            .context("IMAP server closed before greeting")?;
        let mut session = client.login(&self.config.username, &self.password).await.map_err(|(err, _)| {
            anyhow::anyhow!(
                "IMAP login failed: {err}. Use the provider's app authorization code and enable IMAP in the mailbox settings"
            )
        })?;
        // 163/126 reject mailbox access from clients that do not identify themselves.
        let _ = session
            .id([
                ("name", Some("openclaw-rs")),
                ("version", Some(env!("CARGO_PKG_VERSION"))),
            ])
            .await;
        Ok(session)
    }

    /// Logs in to both servers and reports what works, without reading or sending mail.
    pub async fn check(&self) -> Vec<(String, Result<String>)> {
        // Concurrent, so a blocked network reports both failures within one timeout.
        let imap = async {
            let mut session = self.imap_session().await?;
            session
                .select(&self.config.mailbox)
                .await
                .with_context(|| format!("cannot open {}", self.config.mailbox))?;
            let unseen = session.uid_search("UNSEEN").await?.len();
            let _ = session.logout().await;
            Ok(format!(
                "login ok, {} has {unseen} unread",
                self.config.mailbox
            ))
        };
        let smtp = async {
            match tokio::time::timeout(Duration::from_secs(30), self.smtp.test_connection()).await {
                Err(_) => Err(anyhow::anyhow!(
                    "connect timed out; is the port blocked by the network?"
                )),
                Ok(Ok(true)) => Ok("login ok".to_owned()),
                Ok(Ok(false)) => Err(anyhow::anyhow!("server did not accept the connection")),
                Ok(Err(err)) => Err(anyhow::anyhow!(
                    "{err}. Check the port (465 tls / 587 starttls) and the authorization code"
                )),
            }
        };
        let (imap_result, smtp_result) = tokio::join!(imap, smtp);
        vec![
            (
                format!("IMAP {}:{}", self.config.imap_host, self.config.imap_port),
                imap_result,
            ),
            (
                format!("SMTP {}:{}", self.config.smtp_host, self.config.smtp_port),
                smtp_result,
            ),
        ]
    }

    /// Records unseen mail in the inbox, then marks it seen. `\\Seen` only
    /// means "safely queued": a crash before the flag is set refetches the
    /// message, and its key makes the second copy a no-op.
    async fn poll(&self, store: &Store) -> Result<usize> {
        let mut session = self.imap_session().await?;
        let mailbox = session
            .select(&self.config.mailbox)
            .await
            .with_context(|| format!("cannot open {}", self.config.mailbox))?;
        let mut uids: Vec<u32> = session.uid_search("UNSEEN").await?.into_iter().collect();
        uids.sort_unstable();
        uids.truncate(MAX_PER_POLL);
        let mut queued = 0;
        for uid in uids {
            let fetched: Vec<_> = session
                .uid_fetch(uid.to_string(), "BODY.PEEK[]")
                .await?
                .try_collect()
                .await?;
            let Some(raw) = fetched.first().and_then(|f| f.body()) else {
                continue;
            };
            let source = Source {
                mailbox: &self.config.mailbox,
                uid_validity: mailbox.uid_validity,
                uid,
            };
            if self.ingest(store, &source, raw)? {
                queued += 1;
            }
            let _: Vec<_> = session
                .uid_store(uid.to_string(), "+FLAGS (\\Seen)")
                .await?
                .try_collect()
                .await?;
        }
        let _ = session.logout().await;
        Ok(queued)
    }

    /// Queues one fetched message; false when it is skipped or already known.
    fn ingest(&self, store: &Store, source: &Source<'_>, raw: &[u8]) -> Result<bool> {
        let key = message_key(raw).unwrap_or_else(|| {
            format!(
                "uid:{}:{}:{}",
                source.mailbox,
                source.uid_validity.unwrap_or(0),
                source.uid
            )
        });
        if store.mail_handled(&key)? {
            return Ok(false);
        }
        let (sender, raw, state) = match self.classify(raw) {
            Ok(incoming) => (Some(incoming.sender), Some(raw), "pending"),
            Err(Skip::NotAllowed(sender)) => {
                eprintln!("mail: ignored message from {sender} (not in mail.allow)");
                (Some(sender), None, "skipped")
            }
            Err(reason) => {
                eprintln!("mail: skipped message ({reason:?})");
                (None, None, "skipped")
            }
        };
        let queued = store.mail_queue(&key, source, sender.as_deref(), raw, state)?;
        if queued && state == "pending" {
            eprintln!("mail: queued {key} from {}", sender.unwrap_or_default());
        }
        Ok(queued && state == "pending")
    }
}

/// Why an SMTP attempt failed, which decides whether it is retried.
#[derive(Debug)]
pub enum SendError {
    /// The server refused the message for good, or it could not be built.
    Permanent(anyhow::Error),
    /// The server deferred it or the connection failed; the same email may be resent.
    Retry(anyhow::Error),
}

/// A message that cannot be built will never send.
impl From<anyhow::Error> for SendError {
    fn from(err: anyhow::Error) -> Self {
        SendError::Permanent(err)
    }
}

impl SendError {
    fn into_inner(self) -> anyhow::Error {
        match self {
            SendError::Permanent(err) | SendError::Retry(err) => err,
        }
    }
}

/// Where a message was fetched from.
pub struct Source<'a> {
    pub mailbox: &'a str,
    pub uid_validity: Option<u32>,
    pub uid: u32,
}

/// The Message-ID, which identifies duplicate deliveries across mailboxes and UIDs.
fn message_key(raw: &[u8]) -> Option<String> {
    MessageParser::default()
        .parse(raw)?
        .message_id()
        .filter(|id| !id.trim().is_empty())
        .map(|id| format!("<{id}>"))
}

/// Removes quoted history so the model sees only what the sender wrote.
pub fn strip_quoted(body: &str) -> String {
    let mut kept = Vec::new();
    for line in body.lines() {
        let trimmed = line.trim();
        let starts_history = trimmed.starts_with("-----Original Message-----")
            || trimmed.starts_with("------------------ 原始邮件")
            || trimmed.contains("原始邮件")
            || (trimmed.starts_with("On ") && trimmed.ends_with("wrote:"))
            || trimmed.ends_with("写道：")
            || trimmed.ends_with("写道:");
        if starts_history {
            break;
        }
        if trimmed.starts_with('>') {
            continue;
        }
        kept.push(line);
    }
    kept.join("\n").trim().to_owned()
}

/// A queued email claimed for its next step.
#[derive(Debug, Clone, PartialEq)]
pub struct InboxItem {
    pub id: i64,
    pub key: String,
    /// `processing` to run the turn, `sending` to deliver the stored reply.
    pub state: String,
    pub attempts: i64,
    pub interrupted: bool,
    pub raw: Option<Vec<u8>>,
    pub reply: Option<String>,
    pub reply_message_id: Option<String>,
}

/// `(state, messages)` for each state in use.
pub type StateCounts = Vec<(String, i64)>;

/// One row of `mail queue`.
#[derive(Debug, Clone)]
pub struct InboxEntry {
    pub id: i64,
    pub key: String,
    pub sender: Option<String>,
    pub state: String,
    pub attempts: i64,
    pub last_error: Option<String>,
    pub updated_at: i64,
}

/// Turns tried before the error itself is sent as the reply.
const MAX_TURN_ATTEMPTS: i64 = 3;
/// SMTP attempts before a reply is given up on.
const MAX_SEND_ATTEMPTS: i64 = 6;

/// Exponential backoff from one minute, capped at an hour.
fn backoff(attempts: i64) -> i64 {
    60 * (1i64 << attempts.clamp(0, 6)).min(60)
}

impl Store {
    /// Whether this message was queued or handled before, including mail
    /// handled before the inbox existed.
    pub fn mail_handled(&self, key: &str) -> Result<bool> {
        Ok(self.runtime().query_row(
            "SELECT EXISTS(SELECT 1 FROM mail_inbox WHERE key = ?1)
                 OR EXISTS(SELECT 1 FROM mail_seen WHERE message_id = ?1)",
            [key],
            |row| row.get(0),
        )?)
    }

    /// Records a fetched message; false when its key is already known.
    pub fn mail_queue(
        &self,
        key: &str,
        source: &Source<'_>,
        sender: Option<&str>,
        raw: Option<&[u8]>,
        state: &str,
    ) -> Result<bool> {
        let ts = now();
        Ok(self.runtime().execute(
            "INSERT INTO mail_inbox(key, mailbox, uid_validity, uid, sender, raw, state,
                                    next_attempt, received_at, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?8, ?8) ON CONFLICT(key) DO NOTHING",
            params![
                key,
                source.mailbox,
                source.uid_validity,
                source.uid,
                sender,
                raw,
                state,
                ts
            ],
        )? > 0)
    }

    /// Run at startup, when nothing is in flight: an interrupted turn runs
    /// again with a warning, and an interrupted send is left for the operator
    /// because the server may already have accepted it.
    pub fn mail_recover(&self) -> Result<(usize, usize)> {
        let conn = self.runtime();
        let ts = now();
        let turns = conn.execute(
            "UPDATE mail_inbox SET state = 'pending', interrupted = 1, next_attempt = ?1,
                    updated_at = ?1, last_error = 'interrupted while the turn was running'
             WHERE state = 'processing'",
            [ts],
        )?;
        let sends = conn.execute(
            "UPDATE mail_inbox SET state = 'uncertain', updated_at = ?1,
                    last_error = 'interrupted during SMTP; the reply may or may not have been delivered'
             WHERE state = 'sending'",
            [ts],
        )?;
        Ok((turns, sends))
    }

    /// Claims due work: pending turns become `processing`, prepared replies `sending`.
    pub fn mail_claim(&self, now: i64, limit: usize) -> Result<Vec<InboxItem>> {
        let conn = self.runtime();
        let mut stmt = conn.prepare(
            "UPDATE mail_inbox
             SET state = CASE state WHEN 'pending' THEN 'processing' ELSE 'sending' END,
                 attempts = attempts + 1, updated_at = ?1
             WHERE id IN (SELECT id FROM mail_inbox
                          WHERE state IN ('pending', 'reply_ready') AND next_attempt <= ?1
                          ORDER BY id LIMIT ?2)
             RETURNING id, key, state, attempts, interrupted, raw, reply, reply_message_id",
        )?;
        let rows = stmt.query_map(params![now, limit as i64], |row| {
            Ok(InboxItem {
                id: row.get(0)?,
                key: row.get(1)?,
                state: row.get(2)?,
                attempts: row.get(3)?,
                interrupted: row.get(4)?,
                raw: row.get(5)?,
                reply: row.get(6)?,
                reply_message_id: row.get(7)?,
            })
        })?;
        let mut items: Vec<InboxItem> = rows.collect::<rusqlite::Result<_>>()?;
        items.sort_by_key(|item| item.id);
        Ok(items)
    }

    /// Stores the reply and its Message-ID before any SMTP attempt, then
    /// claims it for sending.
    pub fn mail_reply_ready(&self, id: i64, reply: &str, message_id: &str) -> Result<()> {
        self.runtime().execute(
            "UPDATE mail_inbox SET state = 'sending', reply = ?2, reply_message_id = ?3,
                    attempts = 1, last_error = NULL, updated_at = ?4
             WHERE id = ?1 AND state = 'processing'",
            params![id, reply, message_id, now()],
        )?;
        Ok(())
    }

    pub fn mail_sent(&self, id: i64) -> Result<()> {
        self.mail_set(id, "sent", None, 0)
    }

    /// Puts a claimed item back to `pending` or `reply_ready` after `error`,
    /// or marks it failed once its attempts run out.
    pub fn mail_retry(&self, item: &InboxItem, error: &str) -> Result<&'static str> {
        let (back, max) = if item.state == "processing" {
            ("pending", MAX_TURN_ATTEMPTS)
        } else {
            ("reply_ready", MAX_SEND_ATTEMPTS)
        };
        let state = if item.attempts >= max { "failed" } else { back };
        self.mail_set(item.id, state, Some(error), backoff(item.attempts))?;
        Ok(state)
    }

    pub fn mail_fail(&self, id: i64, error: &str) -> Result<()> {
        self.mail_set(id, "failed", Some(error), 0)
    }

    fn mail_set(&self, id: i64, state: &str, error: Option<&str>, delay: i64) -> Result<()> {
        let ts = now();
        self.runtime().execute(
            "UPDATE mail_inbox SET state = ?2, last_error = COALESCE(?3, last_error),
                    next_attempt = ?4, updated_at = ?5
             WHERE id = ?1",
            params![id, state, error, ts + delay, ts],
        )?;
        Ok(())
    }

    /// Queues a failed or uncertain item again: a stored reply is resent
    /// under the same Message-ID, otherwise the turn runs again.
    pub fn mail_requeue(&self, id: i64) -> Result<bool> {
        Ok(self.runtime().execute(
            "UPDATE mail_inbox
             SET state = CASE WHEN reply IS NULL THEN 'pending' ELSE 'reply_ready' END,
                 attempts = 0, next_attempt = ?2, updated_at = ?2
             WHERE id = ?1 AND state IN ('failed', 'uncertain')",
            params![id, now()],
        )? > 0)
    }

    /// Counts by state, and the entries that need attention or are in flight.
    pub fn mail_inbox(&self) -> Result<(StateCounts, Vec<InboxEntry>)> {
        let conn = self.runtime();
        let mut stmt =
            conn.prepare("SELECT state, COUNT(*) FROM mail_inbox GROUP BY state ORDER BY state")?;
        let counts = stmt
            .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?
            .collect::<rusqlite::Result<_>>()?;
        let mut stmt = conn.prepare(
            "SELECT id, key, sender, state, attempts, last_error, updated_at FROM mail_inbox
             WHERE state NOT IN ('sent', 'skipped') ORDER BY id",
        )?;
        let entries = stmt
            .query_map([], |row| {
                Ok(InboxEntry {
                    id: row.get(0)?,
                    key: row.get(1)?,
                    sender: row.get(2)?,
                    state: row.get(3)?,
                    attempts: row.get(4)?,
                    last_error: row.get(5)?,
                    updated_at: row.get(6)?,
                })
            })?
            .collect::<rusqlite::Result<_>>()?;
        Ok((counts, entries))
    }
}

#[derive(Clone)]
struct AutoSubmitted;

impl Header for AutoSubmitted {
    fn name() -> HeaderName {
        HeaderName::new_from_ascii_str("Auto-Submitted")
    }
    fn parse(_: &str) -> Result<Self, Box<dyn std::error::Error + Send + Sync>> {
        Ok(Self)
    }
    fn display(&self) -> HeaderValue {
        HeaderValue::new(Self::name(), "auto-replied".into())
    }
}

#[async_trait]
impl Notifier for MailBot {
    fn handles(&self, session: &str) -> bool {
        session.starts_with(SESSION_PREFIX)
    }

    async fn notify(&self, session: &str, text: &str) -> Result<()> {
        let to = session
            .strip_prefix(SESSION_PREFIX)
            .context("not a mail session")?;
        self.send(to, "[OpenClaw] 定时消息", text, None, None)
            .await
            .map_err(SendError::into_inner)
    }
}

/// Polls for the life of the process; each email becomes a turn in its
/// sender's session, driven through the inbox so every stage survives a restart.
pub async fn run<M: Model + 'static, T: Tools + 'static>(
    bot: Arc<MailBot>,
    gateway: Arc<Gateway<M, T>>,
) {
    match gateway.store().mail_recover() {
        Ok((0, 0)) => {}
        Ok((turns, sends)) => eprintln!(
            "mail: resuming after a restart: {turns} interrupted turn(s) will run again, \
             {sends} reply(s) may or may not have been sent (see `openclaw-rs mail queue`)"
        ),
        Err(err) => eprintln!("mail: cannot recover the inbox: {err:#}"),
    }
    let mut tick = tokio::time::interval(Duration::from_secs(bot.config.poll_secs.max(10)));
    loop {
        tick.tick().await;
        if let Err(err) = bot.poll(gateway.store()).await {
            eprintln!("mail: poll failed: {err:#}");
        }
        let items = match gateway.store().mail_claim(now(), MAX_PER_POLL) {
            Ok(items) => items,
            Err(err) => {
                eprintln!("mail: cannot read the inbox: {err:#}");
                continue;
            }
        };
        for item in items {
            let bot = bot.clone();
            let gateway = gateway.clone();
            tokio::spawn(async move {
                if let Err(err) = advance(&bot, &gateway, item).await {
                    eprintln!("mail: cannot update the inbox: {err:#}");
                }
            });
        }
    }
}

/// Takes one claimed item through its next steps, recording each outcome.
async fn advance<M: Model + 'static, T: Tools + 'static>(
    bot: &MailBot,
    gateway: &Gateway<M, T>,
    mut item: InboxItem,
) -> Result<()> {
    let store = gateway.store();
    let Some(incoming) = item.raw.as_deref().map(|raw| bot.classify(raw)) else {
        store.mail_fail(item.id, "the message body was not kept")?;
        return Ok(());
    };
    let incoming = match incoming {
        Ok(incoming) => incoming,
        Err(reason) => {
            // mail.allow may have changed since the message was queued.
            eprintln!("mail: {} is no longer answered ({reason:?})", item.key);
            store.mail_set(item.id, "skipped", Some(&format!("{reason:?}")), 0)?;
            return Ok(());
        }
    };
    if item.state == "processing" {
        let session = session_for(&incoming.sender);
        let prompt = if item.interrupted {
            format!(
                "[An earlier attempt to answer this email was interrupted before a reply was \
                 sent. Tool calls from that attempt may already have taken effect; check the \
                 conversation above before repeating anything with side effects.]\n\n{}",
                incoming.text
            )
        } else {
            incoming.text.clone()
        };
        // `classify` checked the From address against mail.allow.
        let actor = gateway.actor(&session, &session);
        let reply = match gateway.run_unattended(actor, &session, &prompt).await {
            Ok(text) => text,
            Err(err) if item.attempts < MAX_TURN_ATTEMPTS => {
                let state = store.mail_retry(&item, &format!("{err:#}"))?;
                eprintln!(
                    "mail: turn for {} failed (attempt {}), {state}: {err:#}",
                    item.key, item.attempts
                );
                return Ok(());
            }
            // Out of attempts: tell the sender rather than leaving them waiting.
            Err(err) => crate::i18n::chat::FAILED.with(&[&format!("{err:#}")]),
        };
        let message_id = outgoing_message_id(&bot.from, item.id);
        store.mail_reply_ready(item.id, &reply, &message_id)?;
        item = InboxItem {
            state: "sending".into(),
            attempts: 1,
            reply: Some(reply),
            reply_message_id: Some(message_id),
            ..item
        };
    }
    let (Some(reply), Some(message_id)) = (&item.reply, &item.reply_message_id) else {
        store.mail_fail(item.id, "no reply was stored")?;
        return Ok(());
    };
    match bot.reply(&incoming, reply, message_id).await {
        Ok(()) => {
            store.mail_sent(item.id)?;
            eprintln!("mail: replied to {} ({message_id})", incoming.sender);
        }
        Err(SendError::Permanent(err)) => {
            store.mail_fail(item.id, &format!("{err:#}"))?;
            eprintln!("mail: reply to {} refused: {err:#}", incoming.sender);
        }
        Err(SendError::Retry(err)) => {
            let state = store.mail_retry(&item, &format!("{err:#}"))?;
            eprintln!(
                "mail: cannot reply to {} (attempt {}), {state}: {err:#}",
                incoming.sender, item.attempts
            );
        }
    }
    Ok(())
}

/// A Message-ID that stays the same for every attempt to send one reply.
fn outgoing_message_id(from: &str, inbox_id: i64) -> String {
    let domain = from
        .rsplit_once('@')
        .map_or("openclaw.local", |(_, d)| d.trim_end_matches('>'));
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    format!("<openclaw.{inbox_id}.{nanos:x}@{domain}>")
}

async fn tls_connect(
    host: &str,
    tcp: TcpStream,
) -> Result<tokio_rustls::client::TlsStream<TcpStream>> {
    let mut roots = tokio_rustls::rustls::RootCertStore::empty();
    for cert in rustls_native_certs::load_native_certs().certs {
        let _ = roots.add(cert);
    }
    if roots.is_empty() {
        bail!("no CA certificates loaded; install ca-certificates or fix SSL_CERT_FILE");
    }
    let config = tokio_rustls::rustls::ClientConfig::builder()
        .with_root_certificates(roots)
        .with_no_client_auth();
    let name = tokio_rustls::rustls::pki_types::ServerName::try_from(host.to_owned())
        .with_context(|| format!("invalid TLS server name {host:?}"))?;
    tokio_rustls::TlsConnector::from(Arc::new(config))
        .connect(name, tcp)
        .await
        .with_context(|| format!("TLS handshake with {host} failed"))
}

/// IMAP transport: implicit TLS, or plain text for loopback bridges.
#[derive(Debug)]
enum MailStream {
    Plain(TcpStream),
    Tls(Box<tokio_rustls::client::TlsStream<TcpStream>>),
}

impl AsyncRead for MailStream {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut TaskContext<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        match self.get_mut() {
            MailStream::Plain(s) => Pin::new(s).poll_read(cx, buf),
            MailStream::Tls(s) => Pin::new(s.as_mut()).poll_read(cx, buf),
        }
    }
}

impl AsyncWrite for MailStream {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut TaskContext<'_>,
        buf: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        match self.get_mut() {
            MailStream::Plain(s) => Pin::new(s).poll_write(cx, buf),
            MailStream::Tls(s) => Pin::new(s.as_mut()).poll_write(cx, buf),
        }
    }
    fn poll_flush(self: Pin<&mut Self>, cx: &mut TaskContext<'_>) -> Poll<std::io::Result<()>> {
        match self.get_mut() {
            MailStream::Plain(s) => Pin::new(s).poll_flush(cx),
            MailStream::Tls(s) => Pin::new(s.as_mut()).poll_flush(cx),
        }
    }
    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut TaskContext<'_>) -> Poll<std::io::Result<()>> {
        match self.get_mut() {
            MailStream::Plain(s) => Pin::new(s).poll_shutdown(cx),
            MailStream::Tls(s) => Pin::new(s.as_mut()).poll_shutdown(cx),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bot() -> Arc<MailBot> {
        MailBot::new(MailConfig {
            enabled: true,
            imap_host: "127.0.0.1".into(),
            imap_security: MailSecurity::None,
            smtp_host: "127.0.0.1".into(),
            smtp_security: MailSecurity::None,
            username: "bot@example.org".into(),
            password: Some("pw".into()),
            allow: vec!["me@example.org".into(), "@family.cn".into()],
            ..MailConfig::default()
        })
        .unwrap()
    }

    #[test]
    fn rejects_unsafe_or_open_configs() {
        let base = MailConfig {
            imap_host: "imap.qq.com".into(),
            smtp_host: "smtp.qq.com".into(),
            username: "a@qq.com".into(),
            password: Some("x".into()),
            allow: vec!["me@qq.com".into()],
            ..MailConfig::default()
        };
        assert!(MailBot::new(base.clone()).is_ok());
        assert!(
            MailBot::new(MailConfig {
                allow: vec![],
                ..base.clone()
            })
            .is_err()
        );
        assert!(
            MailBot::new(MailConfig {
                imap_security: MailSecurity::None,
                ..base
            })
            .is_err()
        );
    }

    #[test]
    fn classifies_allowed_automated_and_quoted_mail() {
        let bot = bot();
        let raw = b"From: Me <ME@example.org>\r\nTo: bot@example.org\r\nSubject: =?UTF-8?B?5aSp5rCU?=\r\n\
Message-ID: <a1@example.org>\r\nReferences: <r0@example.org>\r\nContent-Type: text/plain; charset=utf-8\r\n\r\n\
\xe6\x98\x8e\xe5\xa4\xa9\xe4\xb8\x8b\xe9\x9b\xa8\xe5\x90\x97\xef\xbc\x9f\r\n\r\nOn Mon, Bob wrote:\r\n> old text\r\n";
        let incoming = bot.classify(raw).unwrap();
        assert_eq!(incoming.sender, "me@example.org");
        assert_eq!(incoming.subject, "天气");
        assert_eq!(incoming.text, "天气\n\n明天下雨吗？");
        assert_eq!(
            incoming.references,
            vec!["<r0@example.org>", "<a1@example.org>"]
        );

        let gbk = b"From: mom@family.cn\r\nSubject: hi\r\nContent-Type: text/plain; charset=gbk\r\n\r\n\xc4\xe3\xba\xc3\r\n";
        assert_eq!(bot.classify(gbk).unwrap().text, "hi\n\n你好");

        let auto = b"From: me@example.org\r\nAuto-Submitted: auto-replied\r\nSubject: x\r\n\r\nout of office\r\n";
        assert_eq!(bot.classify(auto), Err(Skip::Automated));
        let stranger = b"From: evil@example.com\r\nSubject: x\r\n\r\nrun rm -rf\r\n";
        assert_eq!(
            bot.classify(stranger),
            Err(Skip::NotAllowed("evil@example.com".into()))
        );
        let own = b"From: bot@example.org\r\nSubject: x\r\n\r\nloop\r\n";
        assert_eq!(bot.classify(own), Err(Skip::FromSelf));
    }

    #[test]
    fn fills_provider_servers_from_the_address() {
        let mut qq = MailConfig {
            username: "Bot@QQ.com".into(),
            ..MailConfig::default()
        };
        apply_preset(&mut qq).unwrap();
        assert_eq!(
            (qq.imap_host.as_str(), qq.smtp_host.as_str(), qq.smtp_port),
            ("imap.qq.com", "smtp.qq.com", 465)
        );
        let mut icloud = MailConfig {
            username: "a@icloud.com".into(),
            ..MailConfig::default()
        };
        apply_preset(&mut icloud).unwrap();
        assert_eq!(
            (icloud.smtp_port, icloud.smtp_security),
            (587, MailSecurity::Starttls)
        );
        let mut custom = MailConfig {
            username: "a@qq.com".into(),
            smtp_host: "smtp.example.com".into(),
            smtp_port: 2525,
            ..MailConfig::default()
        };
        apply_preset(&mut custom).unwrap();
        assert_eq!(
            (
                custom.imap_host.as_str(),
                custom.smtp_host.as_str(),
                custom.smtp_port
            ),
            ("imap.qq.com", "smtp.example.com", 2525)
        );
        let mut outlook = MailConfig {
            username: "a@outlook.com".into(),
            ..MailConfig::default()
        };
        assert!(
            apply_preset(&mut outlook)
                .unwrap_err()
                .to_string()
                .contains("OAuth")
        );
        let mut company = MailConfig {
            username: "a@corp.cn".into(),
            ..MailConfig::default()
        };
        apply_preset(&mut company).unwrap();
        assert!(company.imap_host.is_empty());
    }

    #[test]
    fn strips_chinese_and_english_quote_markers() {
        assert_eq!(
            strip_quoted("好的\n\n------------------ 原始邮件 ------------------\n发件人: x"),
            "好的"
        );
        assert_eq!(strip_quoted("ok\n> quoted\nmore"), "ok\nmore");
    }

    const SOURCE: Source<'static> = Source {
        mailbox: "INBOX",
        uid_validity: Some(7),
        uid: 1,
    };

    fn raw(id: &str) -> Vec<u8> {
        format!("From: me@example.org\r\nSubject: hi\r\nMessage-ID: {id}\r\n\r\nping\r\n")
            .into_bytes()
    }

    #[test]
    fn duplicate_deliveries_queue_once() {
        let bot = bot();
        let store = Store::open_in_memory().unwrap();
        assert!(bot.ingest(&store, &SOURCE, &raw("<a@b>")).unwrap());
        // The same Message-ID under another UID, as a refetch after a crash.
        let again = Source { uid: 2, ..SOURCE };
        assert!(!bot.ingest(&store, &again, &raw("<a@b>")).unwrap());
        // Mail handled before the inbox existed is not answered again.
        store
            .runtime()
            .execute("INSERT INTO mail_seen VALUES ('<old@b>', 1)", [])
            .unwrap();
        assert!(!bot.ingest(&store, &SOURCE, &raw("<old@b>")).unwrap());
        // Strangers are recorded as skipped, not queued.
        let stranger = b"From: x@evil.com\r\nMessage-ID: <s@b>\r\n\r\nhi\r\n";
        assert!(!bot.ingest(&store, &SOURCE, stranger).unwrap());
        let (counts, _) = store.mail_inbox().unwrap();
        assert_eq!(counts, vec![("pending".into(), 1), ("skipped".into(), 1)]);
        // Only one claimer gets the item.
        assert_eq!(store.mail_claim(now(), 10).unwrap().len(), 1);
        assert!(store.mail_claim(now(), 10).unwrap().is_empty());
    }

    #[test]
    fn every_stage_survives_a_restart() {
        let bot = bot();
        let store = Store::open_in_memory().unwrap();
        let state = |id: i64| -> String {
            store
                .runtime()
                .query_row("SELECT state FROM mail_inbox WHERE id = ?1", [id], |r| {
                    r.get(0)
                })
                .unwrap()
        };
        bot.ingest(&store, &SOURCE, &raw("<a@b>")).unwrap();
        bot.ingest(&store, &Source { uid: 2, ..SOURCE }, &raw("<c@d>"))
            .unwrap();

        // Crash during the turn: it runs again, flagged as interrupted.
        let claimed = store.mail_claim(now(), 1).unwrap();
        assert_eq!(claimed[0].state, "processing");
        assert_eq!(store.mail_recover().unwrap(), (1, 0));
        let retried = store.mail_claim(now(), 1).unwrap();
        assert_eq!(
            (retried[0].id, retried[0].interrupted),
            (claimed[0].id, true)
        );

        // Reply generated, crash during SMTP: never resent on its own.
        store
            .mail_reply_ready(retried[0].id, "pong", "<r1@x>")
            .unwrap();
        assert_eq!(store.mail_recover().unwrap(), (0, 1));
        assert_eq!(state(retried[0].id), "uncertain");
        assert!(
            store
                .mail_claim(now() + 3600, 10)
                .unwrap()
                .iter()
                .all(|i| i.id != retried[0].id)
        );
        // The operator re-queues it: the same reply and Message-ID go out.
        assert!(store.mail_requeue(retried[0].id).unwrap());
        let resend = store.mail_claim(now(), 10).unwrap();
        let resend = resend.iter().find(|i| i.id == retried[0].id).unwrap();
        assert_eq!(resend.state, "sending");
        assert_eq!(resend.reply.as_deref(), Some("pong"));
        assert_eq!(resend.reply_message_id.as_deref(), Some("<r1@x>"));

        // A transient SMTP failure keeps the reply and waits before retrying.
        assert_eq!(
            store.mail_retry(resend, "451 later").unwrap(),
            "reply_ready"
        );
        assert!(
            store
                .mail_claim(now(), 10)
                .unwrap()
                .iter()
                .all(|i| i.id != resend.id)
        );
        let later = store.mail_claim(now() + 3600, 10).unwrap();
        let later = later.iter().find(|i| i.id == resend.id).unwrap();
        assert_eq!(later.reply.as_deref(), Some("pong"));
        store.mail_sent(later.id).unwrap();
        assert_eq!(state(later.id), "sent");
    }

    #[test]
    fn retries_run_out_into_failed() {
        let bot = bot();
        let store = Store::open_in_memory().unwrap();
        bot.ingest(&store, &SOURCE, &raw("<a@b>")).unwrap();
        let mut t = now();
        let mut last = "";
        for _ in 0..MAX_TURN_ATTEMPTS {
            t += 3600;
            let item = store.mail_claim(t, 1).unwrap().remove(0);
            last = store.mail_retry(&item, "model down").unwrap();
        }
        assert_eq!(last, "failed");
        let (_, entries) = store.mail_inbox().unwrap();
        assert_eq!(entries[0].state, "failed");
        assert_eq!(entries[0].last_error.as_deref(), Some("model down"));
    }

    #[test]
    fn reply_message_ids_are_stable_per_reply() {
        let id = outgoing_message_id("Bot <bot@example.org>", 5);
        assert!(id.starts_with("<openclaw.5."), "{id}");
        assert!(id.ends_with("@example.org>"), "{id}");
    }
}
