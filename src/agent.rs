//! One user turn: model call, tool execution, repeat until the model answers.

use anyhow::{Result, bail};

use crate::access::{self, Actor};
use crate::config::AgentConfig;
use crate::context;
use crate::identity;
use crate::llm::{ChatMessage, Client, Completion, ToolCall, ToolSpec};
use crate::store::Store;
use crate::usage::{MAX_RATIO, MIN_RATIO};

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
    /// Writes the context summary; `model` when not set.
    pub summarizer: Option<M>,
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
        if is_compact_command(input) {
            if !access::current().is_some_and(|a| a.owner) {
                return Ok("Only the owner can compact this conversation.".into());
            }
            return self.compact(session, session_id).await;
        }
        self.store.append(session_id, &ChatMessage::user(input))?;
        let specs = self.tools.specs();
        let tool_json = serde_json::to_string(&specs)?;
        for _ in 0..self.config.max_steps {
            // Read per call so an identity saved mid-turn takes effect on the next call.
            let identity = self.store.identity()?;
            let can_set = access::current().is_some_and(|a| a.can(access::Capability::Identity));
            let system =
                identity::system_prompt(&self.config.system_prompt, identity.as_ref(), can_set);
            let mut messages = vec![ChatMessage::system(&system)];
            messages.extend(
                self.window(session, session_id, &system, &tool_json)
                    .await?,
            );
            let estimated = context::estimate_messages(&messages) + context::estimate(&tool_json);
            let completion = self
                .model
                .complete(&messages, &specs, &mut |text| {
                    on_event(AgentEvent::Text(text.to_owned()))
                })
                .await?;
            self.record(session, "turn", &completion);
            if let Some(usage) = completion.usage
                && let Err(err) =
                    self.store
                        .update_token_ratio(session_id, estimated, usage.prompt_tokens)
            {
                eprintln!("usage: cannot update the token estimate: {err:#}");
            }
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

    /// Saves what a call cost; accounting never fails a turn.
    fn record(&self, session: &str, kind: &str, completion: &Completion) {
        let Some(usage) = &completion.usage else {
            return;
        };
        if let Err(err) = self
            .store
            .record_usage(session, kind, completion.model.as_deref(), usage)
        {
            eprintln!("usage: cannot record a model call: {err:#}");
        }
    }

    fn summarizer(&self) -> &M {
        self.summarizer.as_ref().unwrap_or(&self.model)
    }

    /// Folds every message still in the window into the summary, so the
    /// next turn starts with just the summary. Answers `/compact`.
    async fn compact(&self, session: &str, session_id: i64) -> Result<String> {
        let ctx = self.store.context(session_id)?;
        let Some(&(last, _)) = ctx.messages.last() else {
            return Ok("Nothing to compact yet.".into());
        };
        let count = ctx.messages.len();
        let messages: Vec<ChatMessage> = ctx.messages.into_iter().map(|(_, m)| m).collect();
        let budget = self.config.context_tokens;
        let summary = context::summarize(
            self.summarizer(),
            ctx.summary.as_deref(),
            &messages,
            budget,
            budget / 16,
            &mut |c| self.record(session, "summary", c),
        )
        .await?;
        let marks = context::Marks {
            start: last + 1,
            pruned_before: last + 1,
        };
        self.store.set_context(session_id, marks, Some(&summary))?;
        Ok(format!(
            "Compacted {count} messages into the summary (about {} tokens).",
            context::estimate(&summary)
        ))
    }

    /// The history to send after the system prompt, within the token budget.
    /// Turns that no longer fit are folded into the session's summary first;
    /// if that fails they are left out of this call only and the next call
    /// tries again, so nothing is dropped without a summary.
    /// The budget is scaled by how far our estimates have been from the
    /// provider's counts for this session.
    async fn window(
        &self,
        session: &str,
        session_id: i64,
        system: &str,
        tool_json: &str,
    ) -> Result<Vec<ChatMessage>> {
        let ctx = self.store.context(session_id)?;
        let ratio = ctx.token_ratio.unwrap_or(1.0).clamp(MIN_RATIO, MAX_RATIO);
        let budget = (self.config.context_tokens as f64 / ratio) as usize;
        let summary_limit = budget / 16;
        let summary_tokens = ctx.summary.as_deref().map_or(0, context::estimate);
        let overhead = context::estimate(system)
            + context::estimate(tool_json)
            + summary_tokens.max(summary_limit);
        let fitted = context::fit(ctx.messages, ctx.marks, overhead, budget);
        let mut summary = ctx.summary;
        let mut save = fitted.marks != ctx.marks;
        if !fitted.dropped.is_empty() {
            match context::summarize(
                self.summarizer(),
                summary.as_deref(),
                &fitted.dropped,
                budget,
                summary_limit,
                &mut |c| self.record(session, "summary", c),
            )
            .await
            {
                Ok(text) => summary = Some(text),
                Err(err) => {
                    eprintln!(
                        "context: cannot summarize older messages, retrying next call: {err:#}"
                    );
                    save = false;
                }
            }
        }
        if save {
            self.store
                .set_context(session_id, fitted.marks, summary.as_deref())?;
        }
        Ok(summary
            .as_deref()
            .map(context::summary_message)
            .into_iter()
            .chain(fitted.messages)
            .collect())
    }
}

/// `/compact` typed by a person; handled by the program, never sent to the model.
fn is_compact_command(input: &str) -> bool {
    input.trim() == "/compact"
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use anyhow::Context as _;

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
            summarizer: None,
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

    /// Answers "ok", or `summary` when asked to summarize; keeps every
    /// request that was not a summary.
    struct Recorder {
        summary: Option<&'static str>,
        seen: Mutex<Vec<Vec<ChatMessage>>>,
    }

    #[async_trait::async_trait]
    impl Model for Recorder {
        async fn complete(
            &self,
            messages: &[ChatMessage],
            _tools: &[ToolSpec],
            _on_text: &mut (dyn for<'t> FnMut(&'t str) + Send),
        ) -> Result<Completion> {
            let first = messages[0].content.as_deref().unwrap_or("");
            if first.starts_with("You keep the running summary") {
                let text = self.summary.context("summary refused")?;
                return Ok(Completion {
                    text: text.into(),
                    ..Default::default()
                });
            }
            self.seen.lock().unwrap().push(messages.to_vec());
            Ok(Completion {
                text: "ok".into(),
                ..Default::default()
            })
        }
    }

    async fn long_chat(summary: Option<&'static str>) -> Agent<Recorder, Echo> {
        let agent = Agent {
            model: Recorder {
                summary,
                seen: Mutex::new(Vec::new()),
            },
            summarizer: None,
            tools: Echo,
            store: Store::open_in_memory().unwrap(),
            config: AgentConfig {
                context_tokens: 4000,
                ..AgentConfig::default()
            },
        };
        // Each turn is about 500 tokens, so the budget overflows by turn 8.
        for turn in 0..8 {
            let input = format!("turn {turn} {}", "x".repeat(2000));
            agent
                .run_turn(Actor::owner(access::CLI), "s", &input, &mut |_| {})
                .await
                .unwrap();
        }
        agent
    }

    #[tokio::test]
    async fn folds_dropped_turns_into_a_summary() {
        let agent = long_chat(Some("SUMMARY")).await;
        let seen = agent.model.seen.lock().unwrap();
        let last = seen.last().unwrap();
        let summary = last[1].content.as_deref().unwrap();
        assert!(summary.starts_with("[Summary of the earlier conversation"));
        assert!(summary.ends_with("SUMMARY"));
        assert!(
            !last
                .iter()
                .any(|m| m.content.as_deref().unwrap_or("").starts_with("turn 0 "))
        );
        let id = agent.store.session_id("s").unwrap();
        let ctx = agent.store.context(id).unwrap();
        assert_eq!(ctx.summary.as_deref(), Some("SUMMARY"));
        assert!(ctx.marks.start > 1);
    }

    #[tokio::test]
    async fn compact_folds_the_whole_window_into_the_summary() {
        let agent = long_chat(Some("ALL")).await;
        let reply = agent
            .run_turn(Actor::owner(access::CLI), "s", " /compact\n", &mut |_| {})
            .await
            .unwrap();
        assert!(reply.starts_with("Compacted "), "{reply}");
        let id = agent.store.session_id("s").unwrap();
        let ctx = agent.store.context(id).unwrap();
        assert_eq!(ctx.summary.as_deref(), Some("ALL"));
        assert!(ctx.messages.is_empty(), "the command is not stored");
        agent
            .run_turn(Actor::owner(access::CLI), "s", "next", &mut |_| {})
            .await
            .unwrap();
        let seen = agent.model.seen.lock().unwrap();
        let roles: Vec<_> = seen.last().unwrap().iter().map(|m| m.role).collect();
        use crate::llm::Role::*;
        assert_eq!(roles, vec![System, User, User]);
    }

    #[tokio::test]
    async fn the_summary_model_writes_the_summary() {
        let mut agent = long_chat(None).await;
        agent.summarizer = Some(Recorder {
            summary: Some("CHEAP"),
            seen: Mutex::new(Vec::new()),
        });
        let reply = agent
            .run_turn(Actor::owner(access::CLI), "s", "/compact", &mut |_| {})
            .await
            .unwrap();
        assert!(reply.starts_with("Compacted "), "{reply}");
        let id = agent.store.session_id("s").unwrap();
        assert_eq!(
            agent.store.context(id).unwrap().summary.as_deref(),
            Some("CHEAP")
        );
    }

    #[tokio::test]
    async fn only_the_owner_can_compact() {
        let agent = long_chat(Some("ALL")).await;
        let id = agent.store.session_id("s").unwrap();
        let before = agent.store.context(id).unwrap();
        let guest = Actor {
            id: "qq:someone".into(),
            owner: false,
            capabilities: Default::default(),
        };
        let reply = agent
            .run_turn(guest, "s", "/compact", &mut |_| {})
            .await
            .unwrap();
        assert!(reply.starts_with("Only the owner"), "{reply}");
        let after = agent.store.context(id).unwrap();
        assert_eq!(after.marks, before.marks, "nothing moved");
        assert_eq!(after.messages, before.messages, "nothing stored");
    }

    #[tokio::test]
    async fn a_failed_summary_moves_nothing() {
        let agent = long_chat(None).await;
        let seen = agent.model.seen.lock().unwrap();
        let last = seen.last().unwrap();
        assert!(
            last.iter()
                .all(|m| !m.content.as_deref().unwrap_or("").starts_with("[Summary"))
        );
        assert!(
            last.iter()
                .all(|m| !m.content.as_deref().unwrap_or("").starts_with("turn 0 "))
        );
        let id = agent.store.session_id("s").unwrap();
        let ctx = agent.store.context(id).unwrap();
        assert_eq!(ctx.summary, None);
        assert_eq!(ctx.marks, context::Marks::default());
    }

    #[tokio::test]
    async fn records_usage_and_calibrates_the_estimate() {
        let usage = crate::llm::Usage {
            prompt_tokens: 5000,
            cached_tokens: 4000,
            completion_tokens: 20,
            cost: 0.003,
            ..Default::default()
        };
        let agent = Agent {
            model: Scripted(Mutex::new(vec![Completion {
                text: "done".into(),
                usage: Some(usage),
                model: Some("x/served".into()),
                ..Default::default()
            }])),
            summarizer: None,
            tools: Echo,
            store: Store::open_in_memory().unwrap(),
            config: AgentConfig::default(),
        };
        agent
            .run_turn(Actor::owner(access::CLI), "s", "go", &mut |_| {})
            .await
            .unwrap();
        let rows = agent.store.usage_since(0).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].session, "s");
        assert_eq!(rows[0].usage, usage);
        let id = agent.store.session_id("s").unwrap();
        let ratio = agent.store.context(id).unwrap().token_ratio.unwrap();
        assert!(
            ratio > 1.0,
            "the provider counted more than we estimated: {ratio}"
        );
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
            summarizer: None,
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
