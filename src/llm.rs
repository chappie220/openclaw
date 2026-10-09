//! OpenRouter (OpenAI-compatible) chat completions with SSE streaming and tool calls.
//!
//! Failed requests are retried with backoff while nothing has been shown to
//! the user yet; OpenRouter itself falls back to `model.fallbacks`. Each
//! completion reports its token usage, and prompt caching is requested with
//! `cache_control` breakpoints for providers that need them.

use std::time::Duration;

use anyhow::{Context, Result, bail};
use futures_util::StreamExt;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::config::{ModelConfig, PromptCache};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    System,
    User,
    Assistant,
    Tool,
}

impl Role {
    pub fn as_str(self) -> &'static str {
        match self {
            Role::System => "system",
            Role::User => "user",
            Role::Assistant => "assistant",
            Role::Tool => "tool",
        }
    }

    pub fn parse(value: &str) -> Result<Self> {
        Ok(match value {
            "system" => Role::System,
            "user" => Role::User,
            "assistant" => Role::Assistant,
            "tool" => Role::Tool,
            other => bail!("unknown message role {other:?}"),
        })
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ChatMessage {
    pub role: Role,
    pub content: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_calls: Option<Vec<ToolCall>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
}

impl ChatMessage {
    fn plain(role: Role, content: impl Into<String>) -> Self {
        Self {
            role,
            content: Some(content.into()),
            tool_calls: None,
            tool_call_id: None,
        }
    }
    pub fn system(content: impl Into<String>) -> Self {
        Self::plain(Role::System, content)
    }
    pub fn user(content: impl Into<String>) -> Self {
        Self::plain(Role::User, content)
    }
    pub fn assistant(content: impl Into<String>) -> Self {
        Self::plain(Role::Assistant, content)
    }
    pub fn assistant_tool_calls(content: Option<String>, calls: Vec<ToolCall>) -> Self {
        Self {
            role: Role::Assistant,
            content,
            tool_calls: Some(calls),
            tool_call_id: None,
        }
    }
    pub fn tool_result(call_id: impl Into<String>, content: impl Into<String>) -> Self {
        Self {
            role: Role::Tool,
            content: Some(content.into()),
            tool_calls: None,
            tool_call_id: Some(call_id.into()),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolCall {
    pub id: String,
    #[serde(rename = "type", default = "function_kind")]
    pub kind: String,
    pub function: FunctionCall,
}

fn function_kind() -> String {
    "function".into()
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FunctionCall {
    pub name: String,
    /// JSON text exactly as the model produced it; tools parse and validate it.
    pub arguments: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct ToolSpec {
    #[serde(rename = "type")]
    pub kind: &'static str,
    pub function: ToolFunctionSpec,
}

#[derive(Debug, Clone, Serialize)]
pub struct ToolFunctionSpec {
    pub name: String,
    pub description: String,
    pub parameters: Value,
}

impl ToolSpec {
    pub fn function(name: &str, description: &str, parameters: Value) -> Self {
        Self {
            kind: "function",
            function: ToolFunctionSpec {
                name: name.into(),
                description: description.into(),
                parameters,
            },
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Completion {
    pub text: String,
    pub tool_calls: Vec<ToolCall>,
    pub finish_reason: Option<String>,
    /// What the provider reported; `None` when it sent no usage.
    pub usage: Option<Usage>,
    /// The model that answered, which can differ from the one asked for.
    pub model: Option<String>,
}

/// Token counts and cost of one model call, as the provider reports them.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Usage {
    /// Every input token, cached ones included.
    pub prompt_tokens: u64,
    /// Input tokens read from the prompt cache.
    pub cached_tokens: u64,
    /// Input tokens written to the prompt cache.
    pub cache_write_tokens: u64,
    pub completion_tokens: u64,
    /// In USD, as OpenRouter reports it; 0 when the server sends none.
    pub cost: f64,
}

impl Usage {
    fn parse(value: &Value) -> Self {
        let int = |path: &str| value.pointer(path).and_then(Value::as_u64).unwrap_or(0);
        Self {
            prompt_tokens: int("/prompt_tokens"),
            cached_tokens: int("/prompt_tokens_details/cached_tokens"),
            cache_write_tokens: int("/prompt_tokens_details/cache_write_tokens"),
            completion_tokens: int("/completion_tokens"),
            cost: value.get("cost").and_then(Value::as_f64).unwrap_or(0.0),
        }
    }
}

impl std::ops::AddAssign for Usage {
    fn add_assign(&mut self, other: Self) {
        self.prompt_tokens += other.prompt_tokens;
        self.cached_tokens += other.cached_tokens;
        self.cache_write_tokens += other.cache_write_tokens;
        self.completion_tokens += other.completion_tokens;
        self.cost += other.cost;
    }
}

pub struct Client {
    http: reqwest::Client,
    base_url: String,
    model: String,
    /// `model` then `model.fallbacks`, for OpenRouter's `models`; empty
    /// without fallbacks.
    models: Vec<String>,
    api_key: String,
    cache: bool,
    max_retries: u32,
    retry_base: Duration,
}

#[derive(Serialize)]
struct Request<'a> {
    model: &'a str,
    #[serde(skip_serializing_if = "<[String]>::is_empty")]
    models: &'a [String],
    messages: Vec<Value>,
    #[serde(skip_serializing_if = "<[ToolSpec]>::is_empty")]
    tools: &'a [ToolSpec],
    stream: bool,
    stream_options: StreamOptions,
}

#[derive(Serialize)]
struct StreamOptions {
    include_usage: bool,
}

/// Longest wait between attempts, whatever `Retry-After` asks for.
const MAX_RETRY_WAIT: Duration = Duration::from_secs(30);

/// Why an attempt failed, and whether trying again could help.
enum Failure {
    Retry {
        error: anyhow::Error,
        after: Option<Duration>,
    },
    Fatal(anyhow::Error),
}

/// An `error` event inside an otherwise healthy stream, such as a provider
/// failing mid-answer.
#[derive(Debug)]
struct StreamError(String);

impl std::fmt::Display for StreamError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "model stream error: {}", self.0)
    }
}

impl std::error::Error for StreamError {}

/// Whether `model` is served by a provider that caches only at explicit
/// `cache_control` breakpoints.
fn needs_cache_breakpoints(model: &str) -> bool {
    model.starts_with("anthropic/") || model.starts_with("google/")
}

/// Messages as sent. With `cache`, the system prompt, the first history
/// message (the summary, when there is one) and the last non-tool message
/// with text get
/// a `cache_control` breakpoint, so the stable prefix and the turn so far are
/// read from the cache on the next call.
fn wire_messages(messages: &[ChatMessage], cache: bool) -> Result<Vec<Value>> {
    let mut out = messages
        .iter()
        .map(serde_json::to_value)
        .collect::<serde_json::Result<Vec<_>>>()?;
    if !cache {
        return Ok(out);
    }
    let last = messages
        .iter()
        .rposition(|m| m.role != Role::Tool && m.content.as_deref().is_some_and(|t| !t.is_empty()));
    let mut marks: Vec<usize> = [Some(0), Some(1), last].into_iter().flatten().collect();
    marks.sort_unstable();
    marks.dedup();
    for index in marks {
        let Some(message) = out.get_mut(index) else {
            continue;
        };
        if messages[index].role == Role::Tool {
            continue;
        }
        let Some(text) = message
            .get("content")
            .and_then(Value::as_str)
            .filter(|t| !t.is_empty())
            .map(str::to_owned)
        else {
            continue;
        };
        message["content"] = serde_json::json!([{
            "type": "text",
            "text": text,
            "cache_control": {"type": "ephemeral"},
        }]);
    }
    Ok(out)
}

fn retry_after(response: &reqwest::Response) -> Option<Duration> {
    let value = response.headers().get(reqwest::header::RETRY_AFTER)?;
    let seconds: u64 = value.to_str().ok()?.trim().parse().ok()?;
    Some(Duration::from_secs(seconds))
}

fn retryable_status(status: reqwest::StatusCode) -> bool {
    matches!(status.as_u16(), 408 | 429 | 500 | 502 | 503 | 504 | 529)
}

impl Client {
    pub fn new(config: &ModelConfig, api_key: String) -> Result<Self> {
        let http = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(20))
            .read_timeout(Duration::from_secs(config.request_timeout_secs))
            .build()
            .context(
                "cannot build HTTP client; if no CA certificates load, install ca-certificates or fix SSL_CERT_FILE",
            )?;
        let fallbacks: Vec<String> = config
            .fallbacks
            .iter()
            .map(|m| m.trim().to_owned())
            .filter(|m| !m.is_empty() && *m != config.model)
            .collect();
        let models = if fallbacks.is_empty() {
            Vec::new()
        } else {
            std::iter::once(config.model.clone())
                .chain(fallbacks)
                .collect()
        };
        let cache = match config.prompt_cache {
            PromptCache::On => true,
            PromptCache::Off => false,
            PromptCache::Auto => std::iter::once(&config.model)
                .chain(&models)
                .any(|m| needs_cache_breakpoints(m)),
        };
        Ok(Self {
            http,
            base_url: config.base_url.trim_end_matches('/').to_owned(),
            model: config.model.clone(),
            models,
            api_key,
            cache,
            max_retries: config.max_retries,
            retry_base: Duration::from_secs(1),
        })
    }

    /// Streams one completion; `on_text` receives assistant text as it arrives.
    /// A failed attempt is retried only while no text has been passed on.
    pub async fn complete(
        &self,
        messages: &[ChatMessage],
        tools: &[ToolSpec],
        on_text: &mut (dyn for<'t> FnMut(&'t str) + Send),
    ) -> Result<Completion> {
        let body = serde_json::to_vec(&Request {
            model: &self.model,
            models: &self.models,
            messages: wire_messages(messages, self.cache)?,
            tools,
            stream: true,
            stream_options: StreamOptions {
                include_usage: true,
            },
        })?;
        let mut attempt = 0;
        loop {
            let mut shown = false;
            let result = {
                let mut forward = |text: &str| {
                    shown = true;
                    on_text(text);
                };
                self.attempt(&body, &mut forward).await
            };
            let (error, after) = match result {
                Ok(completion) => return Ok(completion),
                Err(Failure::Fatal(error)) => return Err(error),
                Err(Failure::Retry { error, after }) => (error, after),
            };
            if shown || attempt >= self.max_retries {
                return Err(error);
            }
            let backoff = self.retry_base * 2u32.saturating_pow(attempt);
            let wait = after.unwrap_or(backoff).min(MAX_RETRY_WAIT);
            eprintln!(
                "model: {error:#}; retrying in {:.1}s ({}/{})",
                wait.as_secs_f64(),
                attempt + 1,
                self.max_retries
            );
            tokio::time::sleep(wait).await;
            attempt += 1;
        }
    }

    async fn attempt(
        &self,
        body: &[u8],
        on_text: &mut (dyn for<'t> FnMut(&'t str) + Send),
    ) -> Result<Completion, Failure> {
        let response = self
            .http
            .post(format!("{}/chat/completions", self.base_url))
            .bearer_auth(&self.api_key)
            // OpenRouter attribution headers; harmless for other compatible servers.
            .header("X-Title", "OpenClaw")
            .header(reqwest::header::CONTENT_TYPE, "application/json")
            .body(body.to_vec())
            .send()
            .await
            .map_err(|err| Failure::Retry {
                error: anyhow::Error::new(err).context("model request failed"),
                after: None,
            })?;
        let status = response.status();
        if !status.is_success() {
            let after = retry_after(&response);
            let body = response.text().await.unwrap_or_default();
            let error = anyhow::anyhow!(
                "model request failed with HTTP {status}: {}",
                error_message(&body)
            );
            return Err(if retryable_status(status) {
                Failure::Retry { error, after }
            } else {
                Failure::Fatal(error)
            });
        }
        let mut parser = StreamParser::default();
        let mut body = response.bytes_stream();
        let classify = |error: anyhow::Error| {
            if error.downcast_ref::<StreamError>().is_some() {
                Failure::Retry { error, after: None }
            } else {
                Failure::Fatal(error)
            }
        };
        while let Some(chunk) = body.next().await {
            let chunk = chunk.map_err(|err| Failure::Retry {
                error: anyhow::Error::new(err).context("model stream interrupted"),
                after: None,
            })?;
            parser
                .push(&chunk, &mut |text| on_text(text))
                .map_err(classify)?;
            if parser.done {
                break;
            }
        }
        parser.finish().map_err(Failure::Fatal)
    }
}

fn error_message(body: &str) -> String {
    serde_json::from_str::<Value>(body)
        .ok()
        .and_then(|v| {
            v.pointer("/error/message")
                .and_then(Value::as_str)
                .map(str::to_owned)
        })
        .unwrap_or_else(|| body.chars().take(500).collect())
}

#[derive(Default)]
struct PartialCall {
    id: String,
    name: String,
    arguments: String,
}

/// Incremental SSE decoder; tool-call fragments are merged by their `index`.
#[derive(Default)]
struct StreamParser {
    buffer: Vec<u8>,
    text: String,
    calls: Vec<PartialCall>,
    finish_reason: Option<String>,
    usage: Option<Usage>,
    model: Option<String>,
    done: bool,
}

impl StreamParser {
    /// Feeds `bytes`, passing each piece of assistant text to `on_text` as
    /// soon as it is parsed, so text before an error in the same chunk is
    /// not lost.
    fn push(&mut self, bytes: &[u8], on_text: &mut dyn FnMut(&str)) -> Result<()> {
        self.buffer.extend_from_slice(bytes);
        while let Some(newline) = self.buffer.iter().position(|b| *b == b'\n') {
            let line: Vec<u8> = self.buffer.drain(..=newline).collect();
            let line = String::from_utf8_lossy(&line);
            let line = line.trim_end_matches(['\r', '\n']);
            // Blank separators and `: keep-alive` comments carry no data.
            let Some(data) = line.strip_prefix("data:") else {
                continue;
            };
            let data = data.trim_start();
            if data == "[DONE]" {
                self.done = true;
                break;
            }
            if let Some(text) = self.event(data)? {
                on_text(&text);
            }
        }
        Ok(())
    }

    fn event(&mut self, data: &str) -> Result<Option<String>> {
        let event: Value = serde_json::from_str(data).context("malformed model stream event")?;
        if let Some(message) = event.pointer("/error/message").and_then(Value::as_str) {
            return Err(StreamError(message.to_owned()).into());
        }
        // Usage arrives in the last event, usually with no choices.
        if let Some(usage) = event.get("usage").filter(|u| u.is_object()) {
            self.usage = Some(Usage::parse(usage));
        }
        if let Some(model) = event.get("model").and_then(Value::as_str) {
            self.model = Some(model.to_owned());
        }
        let Some(choice) = event.pointer("/choices/0") else {
            return Ok(None);
        };
        if let Some(reason) = choice.get("finish_reason").and_then(Value::as_str) {
            self.finish_reason = Some(reason.to_owned());
        }
        let delta = choice.get("delta");
        for fragment in delta
            .and_then(|d| d.get("tool_calls"))
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            let index = fragment.get("index").and_then(Value::as_u64).unwrap_or(0) as usize;
            if index >= 128 {
                bail!("model stream tool call index {index} out of range");
            }
            if self.calls.len() <= index {
                self.calls.resize_with(index + 1, PartialCall::default);
            }
            let call = &mut self.calls[index];
            if let Some(id) = fragment.get("id").and_then(Value::as_str) {
                call.id.push_str(id);
            }
            if let Some(name) = fragment.pointer("/function/name").and_then(Value::as_str) {
                call.name.push_str(name);
            }
            if let Some(args) = fragment
                .pointer("/function/arguments")
                .and_then(Value::as_str)
            {
                call.arguments.push_str(args);
            }
        }
        let text = delta
            .and_then(|d| d.get("content"))
            .and_then(Value::as_str)
            .filter(|t| !t.is_empty());
        if let Some(text) = text {
            self.text.push_str(text);
        }
        Ok(text.map(str::to_owned))
    }

    fn finish(self) -> Result<Completion> {
        let mut tool_calls = Vec::new();
        for (index, call) in self.calls.into_iter().enumerate() {
            if call.name.is_empty() {
                continue;
            }
            tool_calls.push(ToolCall {
                id: if call.id.is_empty() {
                    format!("call_{index}")
                } else {
                    call.id
                },
                kind: function_kind(),
                function: FunctionCall {
                    name: call.name,
                    arguments: if call.arguments.trim().is_empty() {
                        "{}".into()
                    } else {
                        call.arguments
                    },
                },
            });
        }
        Ok(Completion {
            text: self.text,
            tool_calls,
            finish_reason: self.finish_reason,
            usage: self.usage,
            model: self.model,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn merges_split_text_and_tool_call_fragments() {
        let mut parser = StreamParser::default();
        let stream = concat!(
            ": OPENROUTER PROCESSING\n\n",
            "data: {\"choices\":[{\"delta\":{\"content\":\"Hel\"}}]}\n\n",
            "data: {\"choices\":[{\"delta\":{\"content\":\"lo\"}}]}\n\n",
            "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"c1\",\"function\":{\"name\":\"read_file\",\"arguments\":\"{\\\"pa\"}}]}}]}\n\n",
            "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"function\":{\"arguments\":\"th\\\":\\\"a\\\"}\"}}]},\"finish_reason\":\"tool_calls\"}]}\n\n",
            "data: [DONE]\n\n",
        );
        // Feed in awkward chunk boundaries, including mid-line splits.
        let bytes = stream.as_bytes();
        let mut texts = Vec::new();
        for chunk in bytes.chunks(7) {
            parser
                .push(chunk, &mut |t| texts.push(t.to_owned()))
                .unwrap();
        }
        assert!(parser.done);
        assert_eq!(texts.concat(), "Hello");
        let done = parser.finish().unwrap();
        assert_eq!(done.text, "Hello");
        assert_eq!(done.finish_reason.as_deref(), Some("tool_calls"));
        assert_eq!(done.tool_calls[0].id, "c1");
        assert_eq!(done.tool_calls[0].function.arguments, "{\"path\":\"a\"}");
    }

    #[test]
    fn reads_usage_and_model_from_the_last_event() {
        let mut parser = StreamParser::default();
        let stream = concat!(
            "data: {\"model\":\"anthropic/claude-x\",\"choices\":[{\"delta\":{\"content\":\"hi\"},\"finish_reason\":\"stop\"}]}\n\n",
            "data: {\"choices\":[],\"usage\":{\"prompt_tokens\":1200,\"completion_tokens\":7,",
            "\"prompt_tokens_details\":{\"cached_tokens\":1000,\"cache_write_tokens\":150},\"cost\":0.0021}}\n\n",
            "data: [DONE]\n\n",
        );
        parser.push(stream.as_bytes(), &mut |_| {}).unwrap();
        let done = parser.finish().unwrap();
        assert_eq!(done.model.as_deref(), Some("anthropic/claude-x"));
        assert_eq!(
            done.usage,
            Some(Usage {
                prompt_tokens: 1200,
                cached_tokens: 1000,
                cache_write_tokens: 150,
                completion_tokens: 7,
                cost: 0.0021,
            })
        );
    }

    #[test]
    fn marks_cache_breakpoints_on_stable_and_latest_messages() {
        let messages = vec![
            ChatMessage::system("sys"),
            ChatMessage::user("summary"),
            ChatMessage::assistant("a"),
            ChatMessage::user("q"),
            ChatMessage::assistant_tool_calls(None, vec![]),
            ChatMessage::tool_result("c1", "out"),
        ];
        let plain = wire_messages(&messages, false).unwrap();
        assert!(plain.iter().all(|m| !m["content"].is_array()));
        let wired = wire_messages(&messages, true).unwrap();
        let marked: Vec<usize> = wired
            .iter()
            .enumerate()
            .filter(|(_, m)| m["content"][0]["cache_control"]["type"] == "ephemeral")
            .map(|(i, _)| i)
            .collect();
        // The tool result and the empty tool-call message stay plain, so the
        // latest breakpoint is the user's message.
        assert_eq!(marked, [0, 1, 3]);
        assert_eq!(wired[3]["content"][0]["text"], "q");
        assert_eq!(wired[5]["content"], "out");
    }

    #[test]
    fn caches_by_default_only_where_breakpoints_are_needed() {
        let client = |model: &str, fallbacks: &[&str], cache| {
            Client::new(
                &ModelConfig {
                    model: model.into(),
                    fallbacks: fallbacks.iter().map(|m| m.to_string()).collect(),
                    prompt_cache: cache,
                    ..ModelConfig::default()
                },
                "k".into(),
            )
            .unwrap()
        };
        assert!(client("anthropic/claude-sonnet", &[], PromptCache::Auto).cache);
        assert!(client("google/gemini-pro", &[], PromptCache::Auto).cache);
        assert!(!client("openai/gpt-x", &[], PromptCache::Auto).cache);
        assert!(client("openai/gpt-x", &["anthropic/claude"], PromptCache::Auto).cache);
        assert!(!client("anthropic/claude", &[], PromptCache::Off).cache);
        assert!(client("openrouter/auto", &[], PromptCache::On).cache);
        let c = client("a/b", &["c/d", " ", "a/b"], PromptCache::Auto);
        assert_eq!(c.models, ["a/b", "c/d"]);
        assert!(client("a/b", &[], PromptCache::Auto).models.is_empty());
    }

    /// A server that answers each request with the next scripted response:
    /// `(status, retry-after, body)`. Returns its URL, the request count and
    /// the last request body.
    async fn scripted_server(
        responses: Vec<(u16, Option<&'static str>, &'static str)>,
    ) -> (
        String,
        std::sync::Arc<std::sync::atomic::AtomicUsize>,
        std::sync::Arc<std::sync::Mutex<Value>>,
    ) {
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::sync::{Arc, Mutex};
        let hits = Arc::new(AtomicUsize::new(0));
        let last = Arc::new(Mutex::new(Value::Null));
        let responses = Arc::new(responses);
        let app = axum::Router::new().route(
            "/chat/completions",
            axum::routing::post({
                let (hits, last, responses) = (hits.clone(), last.clone(), responses.clone());
                move |body: axum::Json<Value>| {
                    let i = hits.fetch_add(1, Ordering::SeqCst);
                    *last.lock().unwrap() = body.0;
                    let (status, after, body) = responses[i.min(responses.len() - 1)];
                    async move {
                        let mut response = axum::response::Response::builder()
                            .status(status)
                            .header("content-type", "text/event-stream");
                        if let Some(after) = after {
                            response = response.header("retry-after", after);
                        }
                        response.body(axum::body::Body::from(body)).unwrap()
                    }
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        tokio::spawn(async move { axum::serve(listener, app).await });
        (url, hits, last)
    }

    fn test_client(url: &str, fallbacks: &[&str]) -> Client {
        let mut client = Client::new(
            &ModelConfig {
                base_url: url.into(),
                model: "anthropic/claude-test".into(),
                fallbacks: fallbacks.iter().map(|m| m.to_string()).collect(),
                max_retries: 2,
                ..ModelConfig::default()
            },
            "k".into(),
        )
        .unwrap();
        client.retry_base = Duration::from_millis(1);
        client
    }

    const OK: &str = "data: {\"choices\":[{\"delta\":{\"content\":\"ok\"}}]}\n\ndata: [DONE]\n\n";

    async fn run(client: &Client) -> (Result<Completion>, String) {
        let mut shown = String::new();
        let result = client
            .complete(&[ChatMessage::user("hi")], &[], &mut |t| shown.push_str(t))
            .await;
        (result, shown)
    }

    #[tokio::test]
    async fn retries_overload_then_answers() {
        let (url, hits, last) = scripted_server(vec![
            (503, Some("0"), "{\"error\":{\"message\":\"overloaded\"}}"),
            (429, None, "{\"error\":{\"message\":\"slow down\"}}"),
            (200, None, OK),
        ])
        .await;
        let (result, shown) = run(&test_client(&url, &["openai/gpt-x"])).await;
        assert_eq!(result.unwrap().text, "ok");
        assert_eq!(shown, "ok");
        assert_eq!(hits.load(std::sync::atomic::Ordering::SeqCst), 3);
        let body = last.lock().unwrap().clone();
        assert_eq!(
            body["models"],
            serde_json::json!(["anthropic/claude-test", "openai/gpt-x"])
        );
        assert_eq!(body["stream_options"]["include_usage"], true);
        assert_eq!(
            body["messages"][0]["content"][0]["cache_control"]["type"],
            "ephemeral"
        );
    }

    #[tokio::test]
    async fn gives_up_after_max_retries() {
        let (url, hits, _) = scripted_server(vec![(502, None, "bad gateway")]).await;
        let (result, _) = run(&test_client(&url, &[])).await;
        assert!(result.unwrap_err().to_string().contains("HTTP 502"));
        assert_eq!(hits.load(std::sync::atomic::Ordering::SeqCst), 3);
    }

    #[tokio::test]
    async fn does_not_retry_a_client_error() {
        let (url, hits, last) =
            scripted_server(vec![(400, None, "{\"error\":{\"message\":\"bad\"}}")]).await;
        let (result, _) = run(&test_client(&url, &[])).await;
        assert!(result.unwrap_err().to_string().contains("bad"));
        assert_eq!(hits.load(std::sync::atomic::Ordering::SeqCst), 1);
        assert!(last.lock().unwrap().get("models").is_none());
    }

    #[tokio::test]
    async fn retries_a_stream_error_only_before_any_text() {
        let early = "data: {\"error\":{\"message\":\"provider down\"}}\n\n";
        let (url, hits, _) = scripted_server(vec![(200, None, early), (200, None, OK)]).await;
        let (result, shown) = run(&test_client(&url, &[])).await;
        assert_eq!(result.unwrap().text, "ok");
        assert_eq!(shown, "ok");
        assert_eq!(hits.load(std::sync::atomic::Ordering::SeqCst), 2);

        let late = "data: {\"choices\":[{\"delta\":{\"content\":\"par\"}}]}\n\ndata: {\"error\":{\"message\":\"provider down\"}}\n\n";
        let (url, hits, _) = scripted_server(vec![(200, None, late), (200, None, OK)]).await;
        let (result, shown) = run(&test_client(&url, &[])).await;
        assert!(result.unwrap_err().to_string().contains("provider down"));
        assert_eq!(shown, "par", "no repeated text");
        assert_eq!(hits.load(std::sync::atomic::Ordering::SeqCst), 1);
    }

    #[test]
    fn surfaces_mid_stream_errors() {
        let mut parser = StreamParser::default();
        let err = parser
            .push(
                b"data: {\"error\":{\"message\":\"rate limited\"}}\n",
                &mut |_| {},
            )
            .unwrap_err();
        assert!(err.to_string().contains("rate limited"));
    }
}
