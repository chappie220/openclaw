//! One user turn: model call, tool execution, repeat until the model answers.

use anyhow::{Result, bail};

use crate::access::{self, Actor};
use crate::config::AgentConfig;
use crate::identity;
use crate::llm::{ChatMessage, Client, Completion, ToolCall, ToolSpec};
use crate::store::Store;

tokio::task_local! {
    static CURRENT_SESSION: String;
}

/// Runs `fut` as part of `session`'s turn.
#[cfg(test)]
pub async fn with_session<F: std::future::Future>(session: String, fut: F) -> F::Output {
    CURRENT_SESSION.scope(session, fut).await
}

/// The session whose turn is running on this task, for tools that act on it.
pub fn current_session() -> Option<String> {
    CURRENT_SESSION.try_with(Clone::clone).ok()
}

/// What the agent reports while a turn runs; front ends render these.
#[derive(Debug, Clone, PartialEq)]
pub enum AgentEvent {
    Text(String),
    ToolStart { name: String, arguments: String },
    ToolEnd { name: String, output: String },
}

/// Tool host. A failing tool returns its error as text so the model can recover.
#[async_trait::async_trait]
pub trait Tools: Send + Sync {
    fn specs(&self) -> Vec<ToolSpec>;
    async fn call(&self, call: &ToolCall) -> String;
}

/// Abstracts the model so the loop can be tested without a network.
#[async_trait::async_trait]
pub trait Model: Send + Sync {
    async fn complete(
        &self,
        messages: &[ChatMessage],
        tools: &[ToolSpec],
        on_text: &mut (dyn for<'t> FnMut(&'t str) + Send),
    ) -> Result<Completion>;
}

#[async_trait::async_trait]
impl Model for Client {
    async fn complete(
        &self,
        messages: &[ChatMessage],
        tools: &[ToolSpec],
        on_text: &mut (dyn for<'t> FnMut(&'t str) + Send),
    ) -> Result<Completion> {
        Client::complete(self, messages, tools, on_text).await
    }
}

pub struct Agent<M: Model, T: Tools> {
    pub model: M,
    pub tools: T,
    pub store: Store,
    pub config: AgentConfig,
}

impl<M: Model, T: Tools> Agent<M, T> {
    /// Runs one turn and returns the final assistant text. Every message is
    /// persisted as it is produced, so an interrupted turn keeps its progress.
    /// Tools run with `actor`'s permissions.
    pub async fn run_turn(
        &self,
        actor: Actor,
        session: &str,
        input: &str,
        on_event: &mut (dyn FnMut(AgentEvent) + Send),
    ) -> Result<String> {
        let turn = CURRENT_SESSION.scope(session.to_owned(), self.turn(session, input, on_event));
        access::with_actor(actor, turn).await
    }

    async fn turn(
        &self,
        session: &str,
        input: &str,
        on_event: &mut (dyn FnMut(AgentEvent) + Send),
    ) -> Result<String> {
        let session_id = self.store.session_id(session)?;
        self.store.append(session_id, &ChatMessage::user(input))?;
        let specs = self.tools.specs();
        for _ in 0..self.config.max_steps {
            // Read per call so an identity saved mid-turn takes effect on the next call.
            let identity = self.store.identity()?;
            let mut messages = vec![ChatMessage::system(identity::system_prompt(
                &self.config.system_prompt,
                identity.as_ref(),
            ))];
            messages.extend(self.store.history(session_id, self.config.history_limit)?);
            let completion = self
                .model
                .complete(&messages, &specs, &mut |text| {
                    on_event(AgentEvent::Text(text.to_owned()))
                })
                .await?;
            if completion.tool_calls.is_empty() {
                self.store
                    .append(session_id, &ChatMessage::assistant(&completion.text))?;
                return Ok(completion.text);
            }
            let text = Some(completion.text).filter(|t| !t.is_empty());
            self.store.append(
                session_id,
                &ChatMessage::assistant_tool_calls(text, completion.tool_calls.clone()),
            )?;
            for call in &completion.tool_calls {
                on_event(AgentEvent::ToolStart {
                    name: call.function.name.clone(),
                    arguments: call.function.arguments.clone(),
                });
                let output = self.tools.call(call).await;
                on_event(AgentEvent::ToolEnd {
                    name: call.function.name.clone(),
                    output: output.clone(),
                });
                self.store
                    .append(session_id, &ChatMessage::tool_result(&call.id, output))?;
            }
        }
        bail!(
            "stopped after {} model calls without a final answer (agent.max_steps)",
            self.config.max_steps
        )
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use super::*;
    use crate::llm::FunctionCall;

    struct Scripted(Mutex<Vec<Completion>>);

    #[async_trait::async_trait]
    impl Model for Scripted {
        async fn complete(
            &self,
            _messages: &[ChatMessage],
            _tools: &[ToolSpec],
            on_text: &mut (dyn for<'t> FnMut(&'t str) + Send),
        ) -> Result<Completion> {
            let next = self.0.lock().unwrap().remove(0);
            on_text(&next.text);
            Ok(next)
        }
    }

    struct Echo;

    #[async_trait::async_trait]
    impl Tools for Echo {
        fn specs(&self) -> Vec<ToolSpec> {
            vec![ToolSpec::function(
                "echo",
                "echo",
                serde_json::json!({"type": "object"}),
            )]
        }
        async fn call(&self, call: &ToolCall) -> String {
            format!("echo:{}", call.function.arguments)
        }
    }

    #[tokio::test]
    async fn runs_tool_then_answers_and_persists_the_turn() {
        let call = ToolCall {
            id: "c1".into(),
            kind: "function".into(),
            function: FunctionCall {
                name: "echo".into(),
                arguments: "{\"x\":1}".into(),
            },
        };
        let agent = Agent {
            model: Scripted(Mutex::new(vec![
                Completion {
                    tool_calls: vec![call],
                    ..Default::default()
                },
                Completion {
                    text: "done".into(),
                    ..Default::default()
                },
            ])),
            tools: Echo,
            store: Store::open_in_memory().unwrap(),
            config: AgentConfig::default(),
        };
        let mut events = Vec::new();
        let answer = agent
            .run_turn(Actor::owner(access::CLI), "s", "go", &mut |e| {
                events.push(e)
            })
            .await
            .unwrap();
        assert_eq!(answer, "done");
        assert!(events.contains(&AgentEvent::ToolEnd {
            name: "echo".into(),
            output: "echo:{\"x\":1}".into()
        }));
        let id = agent.store.session_id("s").unwrap();
        let roles: Vec<_> = agent
            .store
            .history(id, 10)
            .unwrap()
            .iter()
            .map(|m| m.role)
            .collect();
        use crate::llm::Role::*;
        assert_eq!(roles, vec![User, Assistant, Tool, Assistant]);
    }

    #[tokio::test]
    async fn stops_a_runaway_tool_loop() {
        let looping: Vec<_> = (0..3)
            .map(|i| Completion {
                tool_calls: vec![ToolCall {
                    id: format!("c{i}"),
                    kind: "function".into(),
                    function: FunctionCall {
                        name: "echo".into(),
                        arguments: "{}".into(),
                    },
                }],
                ..Default::default()
            })
            .collect();
        let agent = Agent {
            model: Scripted(Mutex::new(looping)),
            tools: Echo,
            store: Store::open_in_memory().unwrap(),
            config: AgentConfig {
                max_steps: 3,
                ..AgentConfig::default()
            },
        };
        let err = agent
            .run_turn(Actor::owner(access::CLI), "s", "go", &mut |_| {})
            .await
            .unwrap_err();
        assert!(err.to_string().contains("max_steps"));
    }
}
