//! SQLite state: sessions and their message history.

use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};
use rusqlite::{Connection, params};

use crate::llm::{ChatMessage, Role, ToolCall};

/// Ordered schema steps; `PRAGMA user_version` records how many have run.
const MIGRATIONS: &[&str] = &[
    r#"
CREATE TABLE sessions (
  id INTEGER PRIMARY KEY,
  name TEXT NOT NULL UNIQUE,
  created_at INTEGER NOT NULL,
  updated_at INTEGER NOT NULL
);
CREATE TABLE messages (
  id INTEGER PRIMARY KEY,
  session_id INTEGER NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,
  role TEXT NOT NULL,
  content TEXT,
  tool_calls TEXT,
  tool_call_id TEXT,
  created_at INTEGER NOT NULL
);
CREATE INDEX messages_by_session ON messages(session_id, id);
"#,
    r#"
CREATE TABLE memories (
  id INTEGER PRIMARY KEY,
  content TEXT NOT NULL,
  created_at INTEGER NOT NULL
);
-- Trigram tokens let CJK text, which has no word separators, match by substring.
CREATE VIRTUAL TABLE memories_fts USING fts5(
  content, content='memories', content_rowid='id', tokenize='trigram'
);
CREATE TRIGGER memories_ai AFTER INSERT ON memories BEGIN
  INSERT INTO memories_fts(rowid, content) VALUES (new.id, new.content);
END;
CREATE TRIGGER memories_ad AFTER DELETE ON memories BEGIN
  INSERT INTO memories_fts(memories_fts, rowid, content) VALUES ('delete', old.id, old.content);
END;
"#,
    r#"
CREATE TABLE jobs (
  id INTEGER PRIMARY KEY,
  name TEXT NOT NULL UNIQUE,
  schedule TEXT NOT NULL,
  session TEXT NOT NULL,
  prompt TEXT NOT NULL,
  next_run INTEGER NOT NULL,
  last_run INTEGER,
  last_status TEXT
);
CREATE INDEX jobs_by_next_run ON jobs(next_run);
"#,
    r#"
CREATE TABLE mail_seen (
  message_id TEXT PRIMARY KEY,
  seen_at INTEGER NOT NULL
);
"#,
];

#[derive(Clone)]
pub struct Store {
    conn: Arc<Mutex<Connection>>,
}

#[derive(Debug, Clone)]
pub struct SessionSummary {
    pub name: String,
    pub messages: i64,
    pub updated_at: i64,
}

pub(crate) fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

impl Store {
    pub fn open(path: &Path) -> Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("cannot create {}", parent.display()))?;
        }
        let conn =
            Connection::open(path).with_context(|| format!("cannot open {}", path.display()))?;
        Self::init(conn)
    }

    #[cfg(test)]
    pub fn open_in_memory() -> Result<Self> {
        Self::init(Connection::open_in_memory()?)
    }

    fn init(mut conn: Connection) -> Result<Self> {
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "foreign_keys", "ON")?;
        conn.pragma_update(None, "busy_timeout", 5000)?;
        let version: i64 = conn.pragma_query_value(None, "user_version", |row| row.get(0))?;
        let version = usize::try_from(version).unwrap_or(usize::MAX);
        if version > MIGRATIONS.len() {
            bail!(
                "database schema {version} is newer than this binary supports ({})",
                MIGRATIONS.len()
            );
        }
        for (index, sql) in MIGRATIONS.iter().enumerate().skip(version) {
            let tx = conn.transaction()?;
            tx.execute_batch(sql)?;
            tx.pragma_update(None, "user_version", (index + 1) as i64)?;
            tx.commit()?;
        }
        Ok(Self {
            conn: Arc::new(Mutex::new(conn)),
        })
    }

    pub(crate) fn lock(&self) -> std::sync::MutexGuard<'_, Connection> {
        // A panic while holding the lock leaves SQLite itself consistent; keep serving.
        self.conn
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    pub fn session_id(&self, name: &str) -> Result<i64> {
        let conn = self.lock();
        let ts = now();
        conn.execute(
            "INSERT INTO sessions(name, created_at, updated_at) VALUES (?1, ?2, ?2)
             ON CONFLICT(name) DO NOTHING",
            params![name, ts],
        )?;
        Ok(
            conn.query_row("SELECT id FROM sessions WHERE name = ?1", [name], |row| {
                row.get(0)
            })?,
        )
    }

    pub fn append(&self, session_id: i64, message: &ChatMessage) -> Result<()> {
        let tool_calls = match &message.tool_calls {
            Some(calls) if !calls.is_empty() => Some(serde_json::to_string(calls)?),
            _ => None,
        };
        let conn = self.lock();
        let ts = now();
        conn.execute(
            "INSERT INTO messages(session_id, role, content, tool_calls, tool_call_id, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![
                session_id,
                message.role.as_str(),
                message.content,
                tool_calls,
                message.tool_call_id,
                ts
            ],
        )?;
        conn.execute(
            "UPDATE sessions SET updated_at = ?2 WHERE id = ?1",
            params![session_id, ts],
        )?;
        Ok(())
    }

    /// The newest `limit` messages in chronological order.
    pub fn history(&self, session_id: i64, limit: usize) -> Result<Vec<ChatMessage>> {
        let conn = self.lock();
        let mut stmt = conn.prepare(
            "SELECT role, content, tool_calls, tool_call_id FROM (
               SELECT id, role, content, tool_calls, tool_call_id FROM messages
               WHERE session_id = ?1 ORDER BY id DESC LIMIT ?2
             ) ORDER BY id ASC",
        )?;
        let rows = stmt.query_map(params![session_id, limit as i64], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, Option<String>>(1)?,
                row.get::<_, Option<String>>(2)?,
                row.get::<_, Option<String>>(3)?,
            ))
        })?;
        let mut messages = Vec::new();
        for row in rows {
            let (role, content, tool_calls, tool_call_id) = row?;
            let tool_calls: Option<Vec<ToolCall>> = tool_calls
                .map(|json| serde_json::from_str(&json))
                .transpose()?;
            messages.push(ChatMessage {
                role: Role::parse(&role)?,
                content,
                tool_calls,
                tool_call_id,
            });
        }
        Ok(trim_orphan_tool_results(messages))
    }

    pub fn sessions(&self) -> Result<Vec<SessionSummary>> {
        let conn = self.lock();
        let mut stmt = conn.prepare(
            "SELECT s.name, COUNT(m.id), s.updated_at FROM sessions s
             LEFT JOIN messages m ON m.session_id = s.id
             GROUP BY s.id ORDER BY s.updated_at DESC",
        )?;
        let rows = stmt.query_map([], |row| {
            Ok(SessionSummary {
                name: row.get(0)?,
                messages: row.get(1)?,
                updated_at: row.get(2)?,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    pub fn delete_session(&self, name: &str) -> Result<bool> {
        let conn = self.lock();
        Ok(conn.execute("DELETE FROM sessions WHERE name = ?1", [name])? > 0)
    }
}

/// A history window can start between an assistant tool call and its results;
/// providers reject tool results without the call, so drop the leading orphans.
fn trim_orphan_tool_results(mut messages: Vec<ChatMessage>) -> Vec<ChatMessage> {
    let start = messages
        .iter()
        .position(|m| m.role != Role::Tool)
        .unwrap_or(messages.len());
    messages.drain(..start);
    messages
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::llm::FunctionCall;

    #[test]
    fn round_trips_history_with_tool_calls() {
        let store = Store::open_in_memory().unwrap();
        let id = store.session_id("main").unwrap();
        assert_eq!(store.session_id("main").unwrap(), id);
        store.append(id, &ChatMessage::user("hi")).unwrap();
        let call = ToolCall {
            id: "c1".into(),
            kind: "function".into(),
            function: FunctionCall {
                name: "read_file".into(),
                arguments: "{\"path\":\"a\"}".into(),
            },
        };
        store
            .append(
                id,
                &ChatMessage::assistant_tool_calls(None, vec![call.clone()]),
            )
            .unwrap();
        store
            .append(id, &ChatMessage::tool_result("c1", "data"))
            .unwrap();
        let history = store.history(id, 10).unwrap();
        assert_eq!(history.len(), 3);
        assert_eq!(history[1].tool_calls.as_ref().unwrap()[0], call);
        assert_eq!(history[2].tool_call_id.as_deref(), Some("c1"));
        // A window starting at the tool result must not send an orphan.
        assert!(store.history(id, 1).unwrap().is_empty());
        assert_eq!(store.sessions().unwrap()[0].messages, 3);
    }
}
