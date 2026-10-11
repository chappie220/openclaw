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

use crate::context::Marks;
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
    migrations: &[
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
"#,
        r#"
-- The actor that saved each memory; NULL for the owner's older ones.
ALTER TABLE memories ADD COLUMN created_by TEXT;
"#,
        r#"
-- Proposed identities. A draft is never edited: a revision is a new draft
-- that supersedes the old one, so an approval names exactly what was shown.
CREATE TABLE identity_drafts (
  id INTEGER PRIMARY KEY,
  name TEXT NOT NULL,
  creature TEXT NOT NULL,
  vibe TEXT NOT NULL,
  emoji TEXT,
  soul TEXT NOT NULL,
  -- FNV-1a of the fields, shown to the user and checked again on commit.
  hash TEXT NOT NULL,
  proposed_by TEXT NOT NULL,
  session TEXT NOT NULL,
  -- awaiting, committed, rejected, superseded
  state TEXT NOT NULL,
  -- Whether the channel has shown this draft to the user yet.
  announced INTEGER NOT NULL DEFAULT 0,
  decided_by TEXT,
  created_at INTEGER NOT NULL,
  decided_at INTEGER
);
"#,
    ],
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
    migrations: &[
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
-- Where the window sent to the model starts; see context.rs.
ALTER TABLE sessions ADD COLUMN context_start INTEGER NOT NULL DEFAULT 0;
ALTER TABLE sessions ADD COLUMN context_pruned_before INTEGER NOT NULL DEFAULT 0;
"#,
        r#"
-- Running summary of the messages before context_start.
ALTER TABLE sessions ADD COLUMN context_summary TEXT;
"#,
        r#"
-- Provider-reported prompt tokens over our estimate for the same request,
-- smoothed; scales the context budget. NULL until a call reports usage.
ALTER TABLE sessions ADD COLUMN token_ratio REAL;
"#,
        r#"
-- Files sent with a user message: JSON list of {name, mime, path, bytes},
-- paths relative to the workspace.
ALTER TABLE messages ADD COLUMN attachments TEXT;
"#,
    ],
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
    migrations: &[
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
-- Mail already handled; forgetting it would answer old mail again.
CREATE TABLE mail_seen (
  message_id TEXT PRIMARY KEY,
  seen_at INTEGER NOT NULL
);
"#,
        r#"
-- Every inbound email and how far it got, so a crash or failure at any stage
-- resumes instead of losing the request or answering twice. `mail_seen`
-- keeps the Message-IDs handled before this table existed.
CREATE TABLE mail_inbox (
  id INTEGER PRIMARY KEY,
  -- Message-ID, or mailbox/UIDVALIDITY/UID when the message has none.
  key TEXT NOT NULL UNIQUE,
  mailbox TEXT NOT NULL,
  uid_validity INTEGER,
  uid INTEGER,
  sender TEXT,
  raw BLOB,
  -- pending, processing, reply_ready, sending, sent, failed, uncertain, skipped
  state TEXT NOT NULL,
  attempts INTEGER NOT NULL DEFAULT 0,
  -- Set when a turn was cut short; the retry warns the model about side effects.
  interrupted INTEGER NOT NULL DEFAULT 0,
  next_attempt INTEGER NOT NULL,
  reply TEXT,
  reply_message_id TEXT,
  last_error TEXT,
  received_at INTEGER NOT NULL,
  updated_at INTEGER NOT NULL
);
CREATE INDEX mail_inbox_due ON mail_inbox(state, next_attempt);
"#,
        r#"
-- The actor that scheduled each job; a job runs with their permissions.
ALTER TABLE jobs ADD COLUMN created_by TEXT;
"#,
        r#"
-- Every model call's provider-reported usage; kept when a session is deleted.
CREATE TABLE usage (
  id INTEGER PRIMARY KEY,
  session TEXT NOT NULL,
  -- turn or summary
  kind TEXT NOT NULL,
  model TEXT,
  prompt_tokens INTEGER NOT NULL,
  cached_tokens INTEGER NOT NULL,
  cache_write_tokens INTEGER NOT NULL,
  completion_tokens INTEGER NOT NULL,
  cost REAL NOT NULL,
  created_at INTEGER NOT NULL
);
CREATE INDEX usage_by_time ON usage(created_at);
"#,
        r#"
-- The sender each call was for (`cli`, `qq:<openid>`, ...), for spending limits.
ALTER TABLE usage ADD COLUMN actor TEXT;
CREATE INDEX usage_by_actor ON usage(actor, created_at);
"#,
    ],
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

/// What `Store::context` returns.
#[derive(Debug)]
pub struct SessionContext {
    pub messages: Vec<(i64, ChatMessage)>,
    pub marks: Marks,
    pub summary: Option<String>,
    /// See `Store::set_token_ratio`.
    pub token_ratio: Option<f64>,
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

/// Records a verified staging so a restart can finish or undo the commit.
const JOURNAL: &str = "state.sqlite.migration";

/// Where a migration stops in tests; production never stops.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Step {
    Checkpointed,
    Created(&'static str),
    Copied(&'static str),
    Staged,
    Journaled,
    Renamed(&'static str),
    Archived,
}

type Hook<'a> = &'a mut dyn FnMut(Step) -> Result<()>;

/// What a verified staging looked like, written before anything is renamed.
#[derive(Debug, serde::Serialize, serde::Deserialize, PartialEq)]
struct Journal {
    /// Size and modification time of `state.sqlite` when it was copied.
    source: Fingerprint,
    /// Name the legacy file is archived under.
    backup: String,
    files: Vec<StagedFile>,
}

#[derive(Debug, serde::Serialize, serde::Deserialize, PartialEq)]
struct Fingerprint {
    len: u64,
    modified_ns: u128,
}

#[derive(Debug, serde::Serialize, serde::Deserialize, PartialEq)]
struct StagedFile {
    file: String,
    user_version: i64,
    /// `(table, rows)` for every table copied from the legacy file.
    rows: Vec<(String, i64)>,
}

fn fingerprint(path: &Path) -> Result<Fingerprint> {
    let meta =
        std::fs::metadata(path).with_context(|| format!("cannot stat {}", path.display()))?;
    let modified_ns = meta
        .modified()?
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    Ok(Fingerprint {
        len: meta.len(),
        modified_ns,
    })
}

fn staging(dir: &Path, schema: &Schema) -> std::path::PathBuf {
    dir.join(format!("{}.migrating", schema.file))
}

/// Removes a database file and its WAL companions, if present.
fn remove_db(path: &Path) -> Result<()> {
    for suffix in ["", "-wal", "-shm", "-journal"] {
        let file = format!("{}{suffix}", path.display());
        match std::fs::remove_file(&file) {
            Err(err) if err.kind() != std::io::ErrorKind::NotFound => {
                return Err(err).with_context(|| format!("cannot remove {file}"));
            }
            _ => {}
        }
    }
    Ok(())
}

fn sync_file(path: &Path) -> Result<()> {
    std::fs::File::open(path)
        .and_then(|f| f.sync_all())
        .with_context(|| format!("cannot flush {}", path.display()))
}

/// Makes renames and creations in `dir` durable.
fn sync_dir(dir: &Path) -> Result<()> {
    sync_file(dir)
}

/// The rows each table holds, `None` for tables the file does not have.
fn row_counts(conn: &Connection, db: &str, schema: &Schema) -> Result<Vec<(String, i64)>> {
    let mut counts = Vec::new();
    for (table, _) in schema.legacy {
        let exists: bool = conn.query_row(
            &format!(
                "SELECT EXISTS(SELECT 1 FROM {db}.sqlite_master WHERE type = 'table' AND name = ?1)"
            ),
            [table],
            |row| row.get(0),
        )?;
        let rows = if exists {
            conn.query_row(&format!("SELECT COUNT(*) FROM {db}.{table}"), [], |row| {
                row.get(0)
            })?
        } else {
            0
        };
        counts.push((table.to_string(), rows));
    }
    Ok(counts)
}

/// Checks a staged file is intact and holds what the journal expects.
fn verify_staged(path: &Path, schema: &Schema, expected: &StagedFile) -> Result<()> {
    let conn = Connection::open_with_flags(path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
        .with_context(|| format!("cannot open {}", path.display()))?;
    let integrity: String = conn.query_row("PRAGMA integrity_check", [], |row| row.get(0))?;
    if integrity != "ok" {
        bail!("{} failed its integrity check: {integrity}", path.display());
    }
    let broken: i64 =
        conn.query_row("SELECT COUNT(*) FROM pragma_foreign_key_check", [], |row| {
            row.get(0)
        })?;
    if broken > 0 {
        bail!("{} has {broken} broken foreign keys", path.display());
    }
    let version: i64 = conn.pragma_query_value(None, "user_version", |row| row.get(0))?;
    let actual = StagedFile {
        file: schema.file.into(),
        user_version: version,
        rows: row_counts(&conn, "main", schema)?,
    };
    if &actual != expected {
        bail!(
            "{} does not match what was staged: {actual:?} != {expected:?}",
            path.display()
        );
    }
    Ok(())
}

/// Splits a pre-split `state.sqlite` into the three files, once.
///
/// Each file is built and verified under a `.migrating` name; a journal
/// recording what was verified is flushed before the first rename, and the
/// legacy file is archived only after every file is in place. A restart after
/// an interruption at any point either finishes the commit or, while
/// `state.sqlite` is still there, discards the copies and starts over.
fn migrate_legacy(dir: &Path) -> Result<()> {
    migrate_legacy_with(dir, &mut |_| Ok(()))
}

fn migrate_legacy_with(dir: &Path, hook: Hook) -> Result<()> {
    let legacy = dir.join(LEGACY_FILE);
    let journal_path = dir.join(JOURNAL);
    if journal_path.exists() {
        let journal: Result<Journal> = std::fs::read(&journal_path)
            .map_err(anyhow::Error::from)
            .and_then(|bytes| Ok(serde_json::from_slice(&bytes)?))
            .with_context(|| format!("cannot read {}", journal_path.display()));
        match journal {
            Ok(journal) => match commit(dir, &journal, hook) {
                Ok(()) => return Ok(()),
                Err(err) if legacy.exists() => {
                    eprintln!(
                        "cannot finish the interrupted state migration ({err:#}); starting it over"
                    );
                    rollback(dir, &journal)?;
                }
                Err(err) => {
                    return Err(err.context(format!(
                        "the state migration was interrupted after {} was archived and cannot be \
                         finished; restore it as {} from the backup named in {}, remove the split \
                         files and that journal, then start again",
                        LEGACY_FILE,
                        legacy.display(),
                        journal_path.display()
                    )));
                }
            },
            // The journal is only renamed into place once complete, so an
            // unreadable one was damaged afterwards: trust nothing it says.
            Err(err) if legacy.exists() => {
                return Err(err.context(format!(
                    "the state migration journal is damaged; {} is untouched, so remove {} and the \
                     split .sqlite files, then start again",
                    legacy.display(),
                    journal_path.display()
                )));
            }
            Err(err) => return Err(err),
        }
    }
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
    let journal = stage(dir, &legacy, hook)?;
    let temp = dir.join(format!("{JOURNAL}.tmp"));
    std::fs::write(&temp, serde_json::to_vec_pretty(&journal)?)
        .with_context(|| format!("cannot write {}", temp.display()))?;
    sync_file(&temp)?;
    std::fs::rename(&temp, &journal_path)?;
    sync_dir(dir)?;
    hook(Step::Journaled)?;
    commit(dir, &journal, hook)
}

/// Copies the legacy file into verified `.migrating` files.
fn stage(dir: &Path, legacy: &Path, hook: Hook) -> Result<Journal> {
    // Fold the WAL into the file so the copy and the kept backup are complete.
    let source =
        Connection::open(legacy).with_context(|| format!("cannot open {}", legacy.display()))?;
    source.pragma_update(None, "journal_mode", "DELETE")?;
    let integrity: String = source.query_row("PRAGMA integrity_check", [], |row| row.get(0))?;
    if integrity != "ok" {
        bail!(
            "{} failed its integrity check ({integrity}); repair it with sqlite3 .recover first",
            legacy.display()
        );
    }
    drop(source);
    hook(Step::Checkpointed)?;
    let mut files = Vec::new();
    for schema in SCHEMAS {
        let path = staging(dir, schema);
        // Leftovers of an earlier interrupted run are never trusted.
        remove_db(&path)?;
        let conn = init(Connection::open(&path)?, schema)?;
        hook(Step::Created(schema.file))?;
        let expected = copy_legacy(&conn, legacy, schema)
            .with_context(|| format!("cannot migrate {} into {}", legacy.display(), schema.file))?;
        // Leave a single self-contained file to rename; opening it restores WAL.
        conn.pragma_update(None, "journal_mode", "DELETE")?;
        drop(conn);
        sync_file(&path)?;
        hook(Step::Copied(schema.file))?;
        let staged = StagedFile {
            file: schema.file.into(),
            user_version: schema.migrations.len() as i64,
            rows: expected,
        };
        verify_staged(&path, schema, &staged)?;
        files.push(staged);
    }
    sync_dir(dir)?;
    hook(Step::Staged)?;
    let mut backup = LEGACY_BACKUP.to_string();
    if dir.join(&backup).exists() {
        // Never replace the backup of an earlier migration.
        backup = format!("{LEGACY_BACKUP}.{}", now());
    }
    Ok(Journal {
        source: fingerprint(legacy)?,
        backup,
        files,
    })
}

/// Moves verified files into place and archives the legacy file. Every step
/// can be repeated, so an interrupted commit is finished by running it again.
fn commit(dir: &Path, journal: &Journal, hook: Hook) -> Result<()> {
    let legacy = dir.join(LEGACY_FILE);
    let backup = dir.join(&journal.backup);
    let wal = dir.join(format!("{LEGACY_FILE}-wal"));
    if legacy.exists()
        && (fingerprint(&legacy)? != journal.source
            || std::fs::metadata(&wal).is_ok_and(|m| m.len() > 0))
    {
        bail!("{} changed after it was staged", legacy.display());
    }
    if !legacy.exists() && !backup.exists() {
        bail!(
            "neither {} nor its backup {} exists",
            legacy.display(),
            backup.display()
        );
    }
    for staged in &journal.files {
        let schema = SCHEMAS
            .iter()
            .find(|schema| schema.file == staged.file)
            .with_context(|| format!("unknown file {} in the journal", staged.file))?;
        let temp = staging(dir, schema);
        let target = dir.join(schema.file);
        match (temp.exists(), target.exists()) {
            (true, false) => {
                verify_staged(&temp, schema, staged)?;
                std::fs::rename(&temp, &target)?;
                sync_dir(dir)?;
                hook(Step::Renamed(schema.file))?;
            }
            (false, true) => verify_staged(&target, schema, staged)?,
            (true, true) => bail!(
                "both {} and {} exist; the migration did not create {}",
                temp.display(),
                target.display(),
                target.display()
            ),
            (false, false) => bail!("{} is missing", temp.display()),
        }
    }
    if legacy.exists() {
        std::fs::rename(&legacy, &backup)?;
        sync_dir(dir)?;
    }
    hook(Step::Archived)?;
    std::fs::remove_file(dir.join(JOURNAL))?;
    sync_dir(dir)?;
    Ok(())
}

/// Undoes an unfinished commit while `state.sqlite` is still in charge.
/// A split file is removed only when it still holds exactly what was staged.
fn rollback(dir: &Path, journal: &Journal) -> Result<()> {
    for staged in &journal.files {
        let Some(schema) = SCHEMAS.iter().find(|schema| schema.file == staged.file) else {
            continue;
        };
        let target = dir.join(schema.file);
        if target.exists() {
            verify_staged(&target, schema, staged).with_context(|| {
                format!(
                    "{} is not the copy this migration made; move it aside, remove {}, then \
                     start again",
                    target.display(),
                    dir.join(JOURNAL).display()
                )
            })?;
            remove_db(&target)?;
        }
        remove_db(&staging(dir, schema))?;
    }
    std::fs::remove_file(dir.join(JOURNAL))?;
    sync_dir(dir)?;
    Ok(())
}

/// Copies `schema`'s tables and returns how many rows the legacy file held.
fn copy_legacy(conn: &Connection, legacy: &Path, schema: &Schema) -> Result<Vec<(String, i64)>> {
    let legacy = legacy.to_str().context("state path is not valid UTF-8")?;
    conn.execute("ATTACH DATABASE ?1 AS legacy", [legacy])?;
    let tx = conn.unchecked_transaction()?;
    let counts = row_counts(&tx, "legacy", schema)?;
    for ((table, columns), (_, rows)) in schema.legacy.iter().zip(&counts) {
        // Older files predate some tables; there is nothing to copy for those.
        if *rows > 0 {
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
    Ok(counts)
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
        let attachments = if message.attachments.is_empty() {
            None
        } else {
            Some(serde_json::to_string(&message.attachments)?)
        };
        let conn = self.chats();
        let ts = now();
        conn.execute(
            "INSERT INTO messages(session_id, role, content, tool_calls, tool_call_id,
               attachments, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![
                session_id,
                message.role.as_str(),
                message.content,
                tool_calls,
                message.tool_call_id,
                attachments,
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
            "SELECT id, role, content, tool_calls, tool_call_id, attachments FROM (
               SELECT id, role, content, tool_calls, tool_call_id, attachments FROM messages
               WHERE session_id = ?1 ORDER BY id DESC LIMIT ?2
             ) ORDER BY id ASC",
        )?;
        let messages = read_messages(&mut stmt, params![session_id, limit as i64])?;
        Ok(trim_orphan_tool_results(
            messages.into_iter().map(|(_, m)| m).collect(),
        ))
    }

    /// The session's context marks, its summary of what came before them,
    /// and every message from the window start on, with ids.
    pub fn context(&self, session_id: i64) -> Result<SessionContext> {
        let conn = self.chats();
        let (marks, summary, token_ratio) = conn.query_row(
            "SELECT context_start, context_pruned_before, context_summary, token_ratio
             FROM sessions WHERE id = ?1",
            [session_id],
            |row| {
                Ok((
                    Marks {
                        start: row.get(0)?,
                        pruned_before: row.get(1)?,
                    },
                    row.get(2)?,
                    row.get(3)?,
                ))
            },
        )?;
        let mut stmt = conn.prepare(
            "SELECT id, role, content, tool_calls, tool_call_id, attachments FROM messages
             WHERE session_id = ?1 AND id >= ?2 ORDER BY id ASC",
        )?;
        let messages = read_messages(&mut stmt, params![session_id, marks.start])?;
        Ok(SessionContext {
            messages,
            marks,
            summary,
            token_ratio,
        })
    }

    pub fn set_context(&self, session_id: i64, marks: Marks, summary: Option<&str>) -> Result<()> {
        self.chats().execute(
            "UPDATE sessions SET context_start = ?2, context_pruned_before = ?3,
             context_summary = ?4 WHERE id = ?1",
            params![session_id, marks.start, marks.pruned_before, summary],
        )?;
        Ok(())
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

/// Rows of `id, role, content, tool_calls, tool_call_id, attachments`.
fn read_messages(
    stmt: &mut rusqlite::Statement<'_>,
    params: impl rusqlite::Params,
) -> Result<Vec<(i64, ChatMessage)>> {
    let rows = stmt.query_map(params, |row| {
        Ok((
            row.get::<_, i64>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, Option<String>>(2)?,
            row.get::<_, Option<String>>(3)?,
            row.get::<_, Option<String>>(4)?,
            row.get::<_, Option<String>>(5)?,
        ))
    })?;
    let mut messages = Vec::new();
    for row in rows {
        let (id, role, content, tool_calls, tool_call_id, attachments) = row?;
        let tool_calls: Option<Vec<ToolCall>> = tool_calls
            .map(|json| serde_json::from_str(&json))
            .transpose()?;
        let attachments = attachments
            .map(|json| serde_json::from_str(&json))
            .transpose()?
            .unwrap_or_default();
        messages.push((
            id,
            ChatMessage {
                role: Role::parse(&role)?,
                content,
                tool_calls,
                tool_call_id,
                attachments,
                images: Vec::new(),
            },
        ));
    }
    Ok(messages)
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
    #[test]
    fn context_starts_at_the_persisted_mark() {
        let store = Store::open_in_memory().unwrap();
        let id = store.session_id("main").unwrap();
        for text in ["a", "b", "c"] {
            store.append(id, &ChatMessage::user(text)).unwrap();
        }
        let all = store.context(id).unwrap();
        assert_eq!(all.marks, Marks::default());
        assert_eq!(all.summary, None);
        assert_eq!(all.messages.len(), 3);
        let marks = Marks {
            start: all.messages[1].0,
            pruned_before: all.messages[2].0,
        };
        store.set_context(id, marks, Some("said a")).unwrap();
        let rest = store.context(id).unwrap();
        assert_eq!(rest.marks, marks);
        assert_eq!(rest.summary.as_deref(), Some("said a"));
        assert_eq!(rest.messages[0].1.content.as_deref(), Some("b"));
        assert_eq!(rest.messages.len(), 2);
    }

    fn legacy_file(dir: &Path) {
        let conn = Connection::open(dir.join(LEGACY_FILE)).unwrap();
        conn.pragma_update(None, "journal_mode", "WAL").unwrap();
        for schema in SCHEMAS {
            // The pre-split file had each table's first layout only.
            for sql in &schema.migrations[..1] {
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
        assert!(store.mail_handled("<a@b>").unwrap());
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

    /// Data `legacy_file` wrote, as the split store sees it.
    fn assert_migrated(store: &Store) {
        let id = store.session_id("main").unwrap();
        assert_eq!(id, 7);
        let history = store.history(id, 10).unwrap();
        assert_eq!(history.len(), 1, "no duplicate import");
        assert_eq!(history[0].content.as_deref(), Some("hi"));
        assert_eq!(store.memory_search("乌龙茶", 5).unwrap().len(), 1);
        assert_eq!(store.identity().unwrap().unwrap().name, "悟空");
        assert_eq!(store.job_list().unwrap().len(), 1);
        assert!(store.mail_handled("<a@b>").unwrap());
    }

    fn leftovers(dir: &Path) -> Vec<String> {
        let mut names: Vec<String> = std::fs::read_dir(dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .filter(|n| n.contains("migrating") || n == JOURNAL || n.ends_with(".tmp"))
            .collect();
        names.sort();
        names
    }

    #[test]
    fn migration_recovers_from_an_interruption_at_every_step() {
        let mut stops = Vec::new();
        for stop_at in 0.. {
            let dir = tempfile::tempdir().unwrap();
            legacy_file(dir.path());
            let mut seen = 0;
            let mut stopped = None;
            let result = migrate_legacy_with(dir.path(), &mut |step| {
                seen += 1;
                if seen > stop_at {
                    stopped = Some(step);
                    bail!("interrupted at {step:?}");
                }
                Ok(())
            });
            let Some(step) = stopped else {
                result.unwrap();
                break;
            };
            assert!(result.is_err());
            stops.push(step);
            // The legacy data stays reachable at every point: either the
            // original file or its backup is there.
            assert!(
                dir.path().join(LEGACY_FILE).exists() || dir.path().join(LEGACY_BACKUP).exists(),
                "{step:?}"
            );
            let store = Store::open(dir.path()).unwrap_or_else(|e| panic!("{step:?}: {e:#}"));
            assert_migrated(&store);
            drop(store);
            assert!(
                leftovers(dir.path()).is_empty(),
                "{step:?}: {:?}",
                leftovers(dir.path())
            );
            assert!(!dir.path().join(LEGACY_FILE).exists(), "{step:?}");
            assert!(dir.path().join(LEGACY_BACKUP).exists(), "{step:?}");
            // And again: restarting a finished migration changes nothing.
            assert_migrated(&Store::open(dir.path()).unwrap());
        }
        for step in [
            Step::Checkpointed,
            Step::Created("soul.sqlite"),
            Step::Copied("runtime.sqlite"),
            Step::Staged,
            Step::Journaled,
            Step::Renamed("soul.sqlite"),
            Step::Renamed("chats.sqlite"),
            Step::Renamed("runtime.sqlite"),
            Step::Archived,
        ] {
            assert!(stops.contains(&step), "{step:?} not exercised: {stops:?}");
        }
    }

    /// Stops a migration right after its journal is flushed.
    fn interrupted_after_journal(dir: &Path) {
        legacy_file(dir);
        let err = migrate_legacy_with(dir, &mut |step| {
            if step == Step::Journaled {
                bail!("stop");
            }
            Ok(())
        });
        assert!(err.is_err());
    }

    #[test]
    fn corrupt_staging_files_are_rebuilt() {
        // Without a journal the staged copies are never trusted.
        let dir = tempfile::tempdir().unwrap();
        legacy_file(dir.path());
        std::fs::write(dir.path().join("soul.sqlite.migrating"), b"garbage").unwrap();
        assert_migrated(&Store::open(dir.path()).unwrap());

        // With one, a staged copy that no longer verifies is redone from the
        // legacy file, which is still in place.
        let dir = tempfile::tempdir().unwrap();
        interrupted_after_journal(dir.path());
        std::fs::write(dir.path().join("chats.sqlite.migrating"), b"garbage").unwrap();
        assert_migrated(&Store::open(dir.path()).unwrap());
        assert!(leftovers(dir.path()).is_empty());
    }

    #[test]
    fn a_legacy_file_changed_mid_commit_is_staged_again() {
        let dir = tempfile::tempdir().unwrap();
        interrupted_after_journal(dir.path());
        {
            let conn = Connection::open(dir.path().join(LEGACY_FILE)).unwrap();
            conn.execute(
                "INSERT INTO messages VALUES (2, 7, 'user', 'more', NULL, NULL, 3)",
                [],
            )
            .unwrap();
            conn.pragma_update(None, "journal_mode", "DELETE").unwrap();
        }
        let store = Store::open(dir.path()).unwrap();
        assert_eq!(store.history(7, 10).unwrap().len(), 2);
    }

    #[test]
    fn a_split_file_the_migration_did_not_make_is_never_removed() {
        let dir = tempfile::tempdir().unwrap();
        interrupted_after_journal(dir.path());
        // Someone puts their own soul.sqlite in place and damages a staged copy.
        let mine = Connection::open(dir.path().join("soul.sqlite.migrating")).unwrap();
        drop(mine);
        std::fs::rename(
            dir.path().join("soul.sqlite.migrating"),
            dir.path().join("keep"),
        )
        .unwrap();
        {
            let conn = Connection::open(dir.path().join("soul.sqlite")).unwrap();
            conn.execute_batch("CREATE TABLE mine(x); INSERT INTO mine VALUES (1);")
                .unwrap();
        }
        let err = Store::open(dir.path()).err().unwrap();
        assert!(format!("{err:#}").contains("move it aside"), "{err:#}");
        let conn = Connection::open(dir.path().join("soul.sqlite")).unwrap();
        let x: i64 = conn
            .query_row("SELECT x FROM mine", [], |r| r.get(0))
            .unwrap();
        assert_eq!(x, 1);
        assert!(dir.path().join(LEGACY_FILE).exists());
    }

    #[test]
    fn migrates_an_old_file_missing_optional_tables() {
        let dir = tempfile::tempdir().unwrap();
        let conn = Connection::open(dir.path().join(LEGACY_FILE)).unwrap();
        conn.execute_batch(CHATS.migrations[0]).unwrap();
        conn.execute_batch(
            "INSERT INTO sessions VALUES (1, 'main', 1, 2);
             INSERT INTO messages VALUES (1, 1, 'user', 'hi', NULL, NULL, 2);",
        )
        .unwrap();
        drop(conn);
        let store = Store::open(dir.path()).unwrap();
        assert_eq!(store.sessions().unwrap()[0].messages, 1);
        assert!(store.identity().unwrap().is_none());
        assert!(store.job_list().unwrap().is_empty());
    }

    #[test]
    fn an_earlier_backup_is_kept() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(LEGACY_BACKUP), b"older backup").unwrap();
        legacy_file(dir.path());
        assert_migrated(&Store::open(dir.path()).unwrap());
        assert_eq!(
            std::fs::read(dir.path().join(LEGACY_BACKUP)).unwrap(),
            b"older backup"
        );
        let backups = std::fs::read_dir(dir.path())
            .unwrap()
            .filter(|e| {
                e.as_ref()
                    .unwrap()
                    .file_name()
                    .to_string_lossy()
                    .starts_with(&format!("{LEGACY_BACKUP}."))
            })
            .count();
        assert_eq!(backups, 1);
    }
}
