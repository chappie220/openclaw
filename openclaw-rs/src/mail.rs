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

pub fn session_for(address: &str) -> String {
    format!("{SESSION_PREFIX}{}", address.to_lowercase())
}

impl MailBot {
    pub fn new(config: MailConfig) -> Result<Arc<Self>> {
        if config.imap_host.is_empty() || config.smtp_host.is_empty() || config.username.is_empty()
        {
            bail!("mail.imap_host, mail.smtp_host and mail.username are required");
        }
        if config.allow.is_empty() {
            bail!("mail.allow is empty; list the addresses (or @domains) that may email the bot");
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
    ) -> Result<()> {
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
            .header(AutoSubmitted);
        if let Some(thread) = thread.filter(|t| !t.message_id.is_empty()) {
            builder = builder
                .in_reply_to(thread.message_id.clone())
                .references(thread.references.join(" "));
        }
        let email = builder.body(text.to_owned())?;
        self.smtp.send(email).await.context("SMTP send failed")?;
        Ok(())
    }

    pub async fn reply(&self, incoming: &Incoming, text: &str) -> Result<()> {
        let subject = if incoming.subject.is_empty() {
            "Re: OpenClaw".to_owned()
        } else if incoming.subject.to_lowercase().starts_with("re:") {
            incoming.subject.clone()
        } else {
            format!("Re: {}", incoming.subject)
        };
        // Replies go to From, never Reply-To: the allow-list checked From.
        self.send(&incoming.sender, &subject, text, Some(incoming))
            .await
    }

    /// Fetches unseen mail, marking each message seen before it is handled.
    async fn poll(&self) -> Result<Vec<Vec<u8>>> {
        let tcp = tokio::time::timeout(
            Duration::from_secs(30),
            TcpStream::connect((self.config.imap_host.as_str(), self.config.imap_port)),
        )
        .await
        .context("IMAP connect timed out")?
        .with_context(|| {
            format!(
                "cannot connect to {}:{}",
                self.config.imap_host, self.config.imap_port
            )
        })?;
        let stream = match self.config.imap_security {
            MailSecurity::None => MailStream::Plain(tcp),
            _ => MailStream::Tls(Box::new(tls_connect(&self.config.imap_host, tcp).await?)),
        };
        let mut client = async_imap::Client::new(stream);
        client
            .read_response()
            .await?
            .context("IMAP server closed before greeting")?;
        let mut session = client
            .login(&self.config.username, &self.password)
            .await
            .map_err(|(err, _)| anyhow::anyhow!("IMAP login failed: {err}"))?;
        // 163/126 reject mailbox access from clients that do not identify themselves.
        let _ = session
            .id([
                ("name", Some("openclaw-rs")),
                ("version", Some(env!("CARGO_PKG_VERSION"))),
            ])
            .await;
        session
            .select(&self.config.mailbox)
            .await
            .with_context(|| format!("cannot open {}", self.config.mailbox))?;
        let mut uids: Vec<u32> = session.uid_search("UNSEEN").await?.into_iter().collect();
        uids.sort_unstable();
        uids.truncate(MAX_PER_POLL);
        let mut raw = Vec::new();
        for uid in uids {
            let fetched: Vec<_> = session
                .uid_fetch(uid.to_string(), "BODY.PEEK[]")
                .await?
                .try_collect()
                .await?;
            let _: Vec<_> = session
                .uid_store(uid.to_string(), "+FLAGS (\\Seen)")
                .await?
                .try_collect()
                .await?;
            if let Some(body) = fetched.first().and_then(|f| f.body()) {
                raw.push(body.to_vec());
            }
        }
        let _ = session.logout().await;
        Ok(raw)
    }
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

impl Store {
    /// Records a Message-ID; false when it was already handled (duplicate delivery).
    pub fn mail_first_sight(&self, message_id: &str) -> Result<bool> {
        if message_id.is_empty() {
            return Ok(true);
        }
        Ok(self.lock().execute(
            "INSERT INTO mail_seen(message_id, seen_at) VALUES (?1, ?2) ON CONFLICT DO NOTHING",
            params![message_id, now()],
        )? > 0)
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
        self.send(to, "[OpenClaw] 定时消息", text, None).await
    }
}

/// Polls for the life of the process; each email becomes a turn in its sender's session.
pub async fn run<M: Model + 'static, T: Tools + 'static>(
    bot: Arc<MailBot>,
    gateway: Arc<Gateway<M, T>>,
) {
    let mut tick = tokio::time::interval(Duration::from_secs(bot.config.poll_secs.max(10)));
    loop {
        tick.tick().await;
        let raw = match bot.poll().await {
            Ok(raw) => raw,
            Err(err) => {
                eprintln!("mail: poll failed: {err:#}");
                continue;
            }
        };
        for raw in raw {
            let incoming = match bot.classify(&raw) {
                Ok(incoming) => incoming,
                Err(Skip::NotAllowed(sender)) => {
                    eprintln!("mail: ignored message from {sender} (not in mail.allow)");
                    continue;
                }
                Err(reason) => {
                    eprintln!("mail: skipped message ({reason:?})");
                    continue;
                }
            };
            match gateway.store().mail_first_sight(&incoming.message_id) {
                Ok(true) => {}
                Ok(false) => continue,
                Err(err) => {
                    eprintln!("mail: cannot record {}: {err:#}", incoming.message_id);
                    continue;
                }
            }
            eprintln!("mail: message from {}", incoming.sender);
            let bot = bot.clone();
            let gateway = gateway.clone();
            tokio::spawn(async move {
                let session = session_for(&incoming.sender);
                let reply = match gateway.run_unattended(&session, &incoming.text).await {
                    Ok(text) => text,
                    Err(err) => format!("出错了：{err:#}"),
                };
                if let Err(err) = bot.reply(&incoming, &reply).await {
                    eprintln!("mail: cannot reply to {}: {err:#}", incoming.sender);
                }
            });
        }
    }
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
    fn strips_chinese_and_english_quote_markers() {
        assert_eq!(
            strip_quoted("好的\n\n------------------ 原始邮件 ------------------\n发件人: x"),
            "好的"
        );
        assert_eq!(strip_quoted("ok\n> quoted\nmore"), "ok\nmore");
    }

    #[test]
    fn remembers_handled_message_ids() {
        let store = Store::open_in_memory().unwrap();
        assert!(store.mail_first_sight("<a@b>").unwrap());
        assert!(!store.mail_first_sight("<a@b>").unwrap());
    }
}
