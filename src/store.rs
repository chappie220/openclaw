//! SQLite state, split by what it is for so each file can be backed up,
//! moved or cleared on its own:
//!
//! - `soul.sqlite`: who the agent is: identity and long-term memories
//! - `chats.sqlite`: sessions and their message history
//! - `runtime.sqlite`: the Gateway's own bookkeeping: scheduled jobs and seen mail

use std::path::Path;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};
use rusqlite::{Connection, params};

use crate::llm::{ChatMessage, Role, ToolCall};

/// One database file: its ordered schema steps (`PRAGMA user_version` records
/// how many have run) and what it takes from the pre-split `state.sqlite`.
struct Schema {
    file: &'static str,
    migrations: &'static [&'static str],
    /// `(table, columns)` to copy, parents before children.
    legacy: &'static [(&'static str, &'static str)],
}

const SOUL: Schema = Schema {
    file: "soul.sqlite",
    migrations: &[r#"
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
-- One row: the agent's IDENTITY.md fields and SOUL.md, shared by every session.
CREATE TABLE identity (
  id INTEGER PRIMARY KEY CHECK (id = 1),
  name TEXT NOT NULL,
  creature TEXT NOT NULL,
  vibe TEXT NOT NULL,
  emoji TEXT,
  soul TEXT NOT NULL,
  updated_at INTEGER NOT NULL
);
"#],
    legacy: &[
        ("memories", "id, content, created_at"),
        (
            "identity",
            "id, name, creature, vibe, emoji, soul, updated_at",
        ),
    ],
};

const CHATS: Schema = Schema {
    file: "chats.sqlite",
    migrations: &[r#"
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
"#],
    legacy: &[
        ("sessions", "id, name, created_at, updated_at"),
        (
            "messages",
            "id, session_id, role, content, tool_calls, tool_call_id, created_at",
        ),
    ],
};

const RUNTIME: Schema = Schema {
    file: "runtime.sqlite",
    migrations: &[r#"
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
-- Mail already handled; forgetting it would answer old mail again.
CREATE TABLE mail_seen (
  message_id TEXT PRIMARY KEY,
  seen_at INTEGER NOT NULL
);
"#],
    legacy: &[
        (
            "jobs",
            "id, name, schedule, session, prompt, next_run, last_run, last_status",
        ),
        ("mail_seen", "message_id, seen_at"),
    ],
};

const SCHEMAS: [&Schema; 3] = [&SOUL, &CHATS, &RUNTIME];

/// The single file used before the split; migrated once, then kept renamed.
const LEGACY_FILE: &str = "state.sqlite";
const LEGACY_BACKUP: &str = "state.sqlite.migrated";

type Shared = Arc<Mutex<Connection>>;

#[derive(Clone)]
pub struct Store {
    soul: Shared,
    chats: Shared,
    runtime: Shared,
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

fn init(mut conn: Connection, schema: &Schema) -> Result<Connection> {
    conn.pragma_update(None, "journal_mode", "WAL")?;
    conn.pragma_update(None, "foreign_keys", "ON")?;
    conn.pragma_update(None, "busy_timeout", 5000)?;
    let version: i64 = conn.pragma_query_value(None, "user_version", |row| row.get(0))?;
    let version = usize::try_from(version).unwrap_or(usize::MAX);
    if version > schema.migrations.len() {
        bail!(
            "{} schema {version} is newer than this binary supports ({})",
            schema.file,
            schema.migrations.len()
        );
    }
    for (index, sql) in schema.migrations.iter().enumerate().skip(version) {
        let tx = conn.transaction()?;
        tx.execute_batch(sql)?;
        tx.pragma_update(None, "user_version", (index + 1) as i64)?;
        tx.commit()?;
    }
    Ok(conn)
}

fn open_file(dir: &Path, schema: &Schema) -> Result<Shared> {
    let path = dir.join(schema.file);
    let conn =
        Connection::open(&path).with_context(|| format!("cannot open {}", path.display()))?;
    Ok(Arc::new(Mutex::new(init(conn, schema)?)))
}

fn guard(conn: &Shared) -> MutexGuard<'_, Connection> {
    // A panic while holding the lock leaves SQLite itself consistent; keep serving.
    conn.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Splits a pre-split `state.sqlite` into the three files, once. Each copy is
/// built under a temporary name and renamed only when all three are complete,
/// so an interrupted run leaves the old file in charge and simply runs again.
fn migrate_legacy(dir: &Path) -> Result<()> {
    let legacy = dir.join(LEGACY_FILE);
    if !legacy.exists() {
        return Ok(());
    }
    if let Some(existing) = SCHEMAS
        .iter()
        .map(|schema| dir.join(schema.file))
        .find(|path| path.exists())
    {
        bail!(
            "both {} and {} exist; move one of them aside, then start again",
            legacy.display(),
            existing.display()
        );
    }
    // Fold the WAL into the file so the copy and the kept backup are complete.
    Connection::open(&legacy)
        .with_context(|| format!("cannot open {}", legacy.display()))?
        .pragma_update(None, "journal_mode", "DELETE")?;
    let temp = |schema: &Schema| dir.join(format!("{}.migrating", schema.file));
    for schema in SCHEMAS {
        let path = temp(schema);
        for suffix in ["", "-wal", "-shm"] {
            let file = format!("{}{suffix}", path.display());
            match std::fs::remove_file(&file) {
                Err(err) if err.kind() != std::io::ErrorKind::NotFound => {
                    return Err(err).with_context(|| format!("cannot remove {file}"));
                }
                _ => {}
            }
        }
        let conn = init(Connection::open(&path)?, schema)?;
        copy_legacy(&conn, &legacy, schema)
            .with_context(|| format!("cannot migrate {} into {}", legacy.display(), schema.file))?;
        // Leave a single self-contained file to rename; opening it restores WAL.
        conn.pragma_update(None, "journal_mode", "DELETE")?;
    }
    for schema in SCHEMAS {
        std::fs::rename(temp(schema), dir.join(schema.file))?;
    }
    std::fs::rename(&legacy, dir.join(LEGACY_BACKUP))?;
    Ok(())
}

fn copy_legacy(conn: &Connection, legacy: &Path, schema: &Schema) -> Result<()> {
    let legacy = legacy.to_str().context("state path is not valid UTF-8")?;
    conn.execute("ATTACH DATABASE ?1 AS legacy", [legacy])?;
    let tx = conn.unchecked_transaction()?;
    for (table, columns) in schema.legacy {
        // Older files predate some tables; there is nothing to copy for those.
        let exists: bool = tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM legacy.sqlite_master WHERE type = 'table' AND name = ?1)",
            [table],
            |row| row.get(0),
        )?;
        if exists {
            tx.execute(
                &format!(
                    "INSERT INTO main.{table}({columns}) SELECT {columns} FROM legacy.{table}"
                ),
                [],
            )?;
        }
    }
    tx.commit()?;
    conn.execute("DETACH DATABASE legacy", [])?;
    Ok(())
}

impl Store {
    /// Opens the three state files in `dir`, migrating a pre-split
    /// `state.sqlite` first if one is there.
    pub fn open(dir: &Path) -> Result<Self> {
        std::fs::create_dir_all(dir).with_context(|| format!("cannot create {}", dir.display()))?;
        migrate_legacy(dir)?;
        Ok(Self {
            soul: open_file(dir, &SOUL)?,
            chats: open_file(dir, &CHATS)?,
            runtime: open_file(dir, &RUNTIME)?,
        })
    }

    #[cfg(test)]
    pub fn open_in_memory() -> Result<Self> {
        let open = |schema: &Schema| -> Result<Shared> {
            Ok(Arc::new(Mutex::new(init(
                Connection::open_in_memory()?,
                schema,
            )?)))
        };
        Ok(Self {
            soul: open(&SOUL)?,
            chats: open(&CHATS)?,
            runtime: open(&RUNTIME)?,
        })
    }

    /// Identity and memories.
    pub(crate) fn soul(&self) -> MutexGuard<'_, Connection> {
        guard(&self.soul)
    }

    /// Sessions and messages.
    pub(crate) fn chats(&self) -> MutexGuard<'_, Connection> {
        guard(&self.chats)
    }

    /// Scheduled jobs and seen mail.
    pub(crate) fn runtime(&self) -> MutexGuard<'_, Connection> {
        guard(&self.runtime)
    }

    pub fn session_id(&self, name: &str) -> Result<i64> {
        let conn = self.chats();
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
        let conn = self.chats();
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
        let conn = self.chats();
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
        let conn = self.chats();
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
        let conn = self.chats();
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

    /// Builds a pre-split `state.sqlite` with the same tables in one file.
    fn legacy_file(dir: &Path) {
        let conn = Connection::open(dir.join(LEGACY_FILE)).unwrap();
        conn.pragma_update(None, "journal_mode", "WAL").unwrap();
        for schema in SCHEMAS {
            for sql in schema.migrations {
                conn.execute_batch(sql).unwrap();
            }
        }
        conn.execute_batch(
            "INSERT INTO sessions VALUES (7, 'main', 1, 2);
             INSERT INTO messages VALUES (1, 7, 'user', 'hi', NULL, NULL, 2);
             INSERT INTO memories VALUES (3, '用户喜欢喝乌龙茶', 1);
             INSERT INTO identity VALUES (1, '悟空', '石猴', '顽皮', NULL, '你自称俺老孙', 1);
             INSERT INTO jobs VALUES (1, 'beat', '*/5 * * * *', 'main', 'ping', 9, NULL, NULL);
             INSERT INTO mail_seen VALUES ('<a@b>', 1);",
        )
        .unwrap();
    }

    #[test]
    fn splits_legacy_state_once() {
        let dir = tempfile::tempdir().unwrap();
        legacy_file(dir.path());
        let store = Store::open(dir.path()).unwrap();

        let id = store.session_id("main").unwrap();
        assert_eq!(id, 7);
        assert_eq!(
            store.history(id, 10).unwrap()[0].content.as_deref(),
            Some("hi")
        );
        assert_eq!(store.memory_search("乌龙茶", 5).unwrap()[0].id, 3);
        assert_eq!(store.identity().unwrap().unwrap().name, "悟空");
        assert_eq!(store.job_list().unwrap()[0].name, "beat");
        assert!(!store.mail_first_sight("<a@b>").unwrap());
        assert!(!dir.path().join(LEGACY_FILE).exists());
        assert!(dir.path().join(LEGACY_BACKUP).exists());
        for schema in SCHEMAS {
            assert!(dir.path().join(schema.file).exists());
        }
        drop(store);

        // Reopening finds nothing to migrate and keeps the data.
        let store = Store::open(dir.path()).unwrap();
        assert_eq!(store.sessions().unwrap()[0].messages, 1);
    }

    #[test]
    fn refuses_legacy_next_to_split_files() {
        let dir = tempfile::tempdir().unwrap();
        drop(Store::open(dir.path()).unwrap());
        legacy_file(dir.path());
        let err = Store::open(dir.path()).err().unwrap().to_string();
        assert!(err.contains("move one of them aside"), "{err}");
    }
}
