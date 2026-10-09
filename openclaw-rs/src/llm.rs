//! OpenRouter (OpenAI-compatible) chat completions with SSE streaming and tool calls.

use std::time::Duration;

use anyhow::{Context, Result, bail};
use futures_util::StreamExt;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::config::ModelConfig;

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
}

pub struct Client {
    http: reqwest::Client,
    base_url: String,
    model: String,
    api_key: String,
}

#[derive(Serialize)]
struct Request<'a> {
    model: &'a str,
    messages: &'a [ChatMessage],
    #[serde(skip_serializing_if = "<[ToolSpec]>::is_empty")]
    tools: &'a [ToolSpec],
    stream: bool,
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
        Ok(Self {
            http,
            base_url: config.base_url.trim_end_matches('/').to_owned(),
            model: config.model.clone(),
            api_key,
        })
    }

    /// Streams one completion; `on_text` receives assistant text as it arrives.
    pub async fn complete(
        &self,
        messages: &[ChatMessage],
        tools: &[ToolSpec],
        on_text: &mut (dyn for<'t> FnMut(&'t str) + Send),
    ) -> Result<Completion> {
        let response = self
            .http
            .post(format!("{}/chat/completions", self.base_url))
            .bearer_auth(&self.api_key)
            // OpenRouter attribution headers; harmless for other compatible servers.
            .header("X-Title", "OpenClaw")
            .json(&Request {
                model: &self.model,
                messages,
                tools,
                stream: true,
            })
            .send()
            .await
            .context("model request failed")?;
        let status = response.status();
        if !status.is_success() {
            let body = response.text().await.unwrap_or_default();
            bail!(
                "model request failed with HTTP {status}: {}",
                error_message(&body)
            );
        }
        let mut parser = StreamParser::default();
        let mut body = response.bytes_stream();
        while let Some(chunk) = body.next().await {
            let chunk = chunk.context("model stream interrupted")?;
            for text in parser.push(&chunk)? {
                on_text(&text);
            }
            if parser.done {
                break;
            }
        }
        parser.finish()
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
    done: bool,
}

impl StreamParser {
    fn push(&mut self, bytes: &[u8]) -> Result<Vec<String>> {
        self.buffer.extend_from_slice(bytes);
        let mut texts = Vec::new();
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
                texts.push(text);
            }
        }
        Ok(texts)
    }

    fn event(&mut self, data: &str) -> Result<Option<String>> {
        let event: Value = serde_json::from_str(data).context("malformed model stream event")?;
        if let Some(message) = event.pointer("/error/message").and_then(Value::as_str) {
            bail!("model stream error: {message}");
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
            texts.extend(parser.push(chunk).unwrap());
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
    fn surfaces_mid_stream_errors() {
        let mut parser = StreamParser::default();
        let err = parser
            .push(b"data: {\"error\":{\"message\":\"rate limited\"}}\n")
            .unwrap_err();
        assert!(err.to_string().contains("rate limited"));
    }
}
