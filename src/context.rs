//! Fits a session's history into a token budget instead of a message count.
//!
//! Two marks per session only move forward, and only when the window
//! overflows; it then shrinks to half the budget. Between those jumps the
//! messages sent to the model keep the same prefix, so prompt caching keeps
//! working. The full history always stays in SQLite.

use crate::llm::{ChatMessage, Role};

/// Where a session's window starts, persisted with the session.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Marks {
    /// Id of the first message sent; older ones are left out.
    pub start: i64,
    /// Tool results with a smaller id are sent as a short stub.
    pub pruned_before: i64,
}

/// Rough token count without a tokenizer: about four ASCII characters per
/// token, and one per other character, since CJK text is roughly one or more
/// tokens per character.
pub fn estimate(text: &str) -> usize {
    let ascii = text.chars().filter(char::is_ascii).count();
    ascii.div_ceil(4) + (text.chars().count() - ascii)
}

fn message_tokens(message: &ChatMessage) -> usize {
    let calls: usize = message
        .tool_calls
        .iter()
        .flatten()
        .map(|c| 8 + estimate(&c.function.name) + estimate(&c.function.arguments))
        .sum();
    4 + message.content.as_deref().map_or(0, estimate) + calls
}

fn stub(message: &ChatMessage) -> ChatMessage {
    let bytes = message.content.as_deref().map_or(0, str::len);
    ChatMessage {
        content: Some(format!(
            "[earlier tool output ({bytes} bytes) omitted to save context; run the tool again if you need it]"
        )),
        ..message.clone()
    }
}

struct Entry {
    id: i64,
    message: ChatMessage,
    full: usize,
    pruned: usize,
}

impl Entry {
    fn prunable(&self) -> bool {
        self.message.role == Role::Tool
    }
}

fn total(entries: &[Entry], marks: Marks, overhead: usize) -> usize {
    overhead
        + entries
            .iter()
            .filter(|e| e.id >= marks.start)
            .map(|e| {
                if e.prunable() && e.id < marks.pruned_before {
                    e.pruned
                } else {
                    e.full
                }
            })
            .sum::<usize>()
}

/// Picks what to send from `history` (every message from `marks.start` on,
/// oldest first, with ids) so it fits in `budget` tokens together with
/// `overhead` (system prompt and tool specs). Returns the messages and the
/// marks to persist.
///
/// On overflow, in order until the window is at most half the budget:
/// stub tool output from earlier turns, then drop the oldest turns. The
/// current turn is never dropped; if it alone is over budget, its older tool
/// output is stubbed too, keeping the latest results the model asked for.
pub fn fit(
    history: Vec<(i64, ChatMessage)>,
    marks: Marks,
    overhead: usize,
    budget: usize,
) -> (Vec<ChatMessage>, Marks) {
    let entries: Vec<Entry> = history
        .into_iter()
        .map(|(id, message)| Entry {
            id,
            full: message_tokens(&message),
            pruned: message_tokens(&stub(&message)),
            message,
        })
        .collect();
    let mut marks = marks;
    if total(&entries, marks, overhead) > budget {
        marks = compact(&entries, marks, overhead, budget);
    }
    let messages = entries
        .into_iter()
        .filter(|e| e.id >= marks.start)
        .map(|e| {
            if e.prunable() && e.id < marks.pruned_before {
                stub(&e.message)
            } else {
                e.message
            }
        })
        .collect();
    (messages, marks)
}

fn compact(entries: &[Entry], mut marks: Marks, overhead: usize, budget: usize) -> Marks {
    let target = budget / 2;
    let users: Vec<i64> = entries
        .iter()
        .filter(|e| e.message.role == Role::User && e.id >= marks.start)
        .map(|e| e.id)
        .collect();
    let Some(&current) = users.last() else {
        return marks;
    };
    marks.pruned_before = marks.pruned_before.max(current);
    // Windows start at a user message so no tool result loses its call.
    if total(entries, marks, overhead) > target {
        marks.start = users
            .iter()
            .copied()
            .find(|&start| total(entries, Marks { start, ..marks }, overhead) <= target)
            .unwrap_or(current)
            .max(marks.start);
    }
    // The current turn alone is over budget: stub its older tool output.
    let latest_call = entries
        .iter()
        .rev()
        .find(|e| e.message.role == Role::Assistant)
        .map_or(i64::MAX, |e| e.id);
    for entry in entries
        .iter()
        .filter(|e| e.id >= current && e.id < latest_call)
    {
        if total(entries, marks, overhead) <= budget {
            break;
        }
        if entry.prunable() {
            marks.pruned_before = marks.pruned_before.max(entry.id + 1);
        }
    }
    marks
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::llm::{FunctionCall, ToolCall};

    fn call(id: &str) -> ChatMessage {
        ChatMessage::assistant_tool_calls(
            None,
            vec![ToolCall {
                id: id.into(),
                kind: "function".into(),
                function: FunctionCall {
                    name: "shell".into(),
                    arguments: "{}".into(),
                },
            }],
        )
    }

    /// Turns of: user, tool call, a 400-token result, answer.
    fn turns(n: usize) -> Vec<(i64, ChatMessage)> {
        let mut out = Vec::new();
        for t in 0..n {
            let id = |i: usize| (t * 4 + i + 1) as i64;
            out.push((id(0), ChatMessage::user(format!("q{t}"))));
            out.push((id(1), call(&format!("c{t}"))));
            out.push((
                id(2),
                ChatMessage::tool_result(format!("c{t}"), "x".repeat(1600)),
            ));
            out.push((id(3), ChatMessage::assistant(format!("a{t}"))));
        }
        out
    }

    fn contents(messages: &[ChatMessage]) -> Vec<String> {
        messages
            .iter()
            .map(|m| m.content.clone().unwrap_or_default())
            .collect()
    }

    #[test]
    fn estimates_cjk_per_character() {
        assert_eq!(estimate("abcdefgh"), 2);
        assert_eq!(estimate("你好"), 2);
    }

    #[test]
    fn under_budget_sends_everything_unchanged() {
        let history = turns(3);
        let (messages, marks) = fit(history.clone(), Marks::default(), 100, 100_000);
        assert_eq!(marks, Marks::default());
        assert_eq!(
            messages,
            history.into_iter().map(|(_, m)| m).collect::<Vec<_>>()
        );
    }

    #[test]
    fn stubs_old_tool_output_before_dropping_turns() {
        // Five turns are ~2100 tokens; stubbing four old results is enough.
        let mut history = turns(5);
        history.push((21, ChatMessage::user("now")));
        let (messages, marks) = fit(history, Marks::default(), 0, 2000);
        assert_eq!(
            marks,
            Marks {
                start: 0,
                pruned_before: 21
            }
        );
        assert_eq!(messages.len(), 21);
        assert!(contents(&messages)[2].starts_with("[earlier tool output (1600 bytes)"));
        assert!(contents(&messages).iter().all(|c| c.len() < 200));
    }

    #[test]
    fn drops_oldest_turns_to_half_the_budget_and_stays_put() {
        let mut history = turns(20);
        history.push((81, ChatMessage::user("now")));
        let budget = 1000;
        let (messages, marks) = fit(history.clone(), Marks::default(), 50, budget);
        assert!(marks.start > 1);
        assert_eq!(messages[0].role, Role::User);
        let sent: usize = 50 + messages.iter().map(message_tokens).sum::<usize>();
        assert!(sent <= budget / 2, "{sent}");
        // The next call with one more message keeps the same prefix.
        history.push((82, ChatMessage::assistant("ok")));
        let kept: Vec<_> = history
            .into_iter()
            .filter(|(id, _)| *id >= marks.start)
            .collect();
        let (next, again) = fit(kept, marks, 50, budget);
        assert_eq!(again, marks);
        assert_eq!(next[..messages.len()], messages[..]);
    }

    #[test]
    fn an_oversized_current_turn_keeps_its_latest_results() {
        let mut history = vec![(1, ChatMessage::user("go"))];
        for i in 0..5 {
            let id = 2 + i * 2;
            history.push((id, call(&format!("c{i}"))));
            history.push((
                id + 1,
                ChatMessage::tool_result(format!("c{i}"), "y".repeat(1600)),
            ));
        }
        let (messages, marks) = fit(history, Marks::default(), 0, 1000);
        assert_eq!(marks.start, 1);
        assert_eq!(messages.len(), 11);
        let last = contents(&messages).pop().unwrap();
        assert_eq!(last.len(), 1600, "latest result is intact");
        assert!(messages.iter().map(message_tokens).sum::<usize>() <= 1000);
    }
}
