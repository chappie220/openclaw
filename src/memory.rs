//! Long-term memory: short notes the agent saves and recalls across sessions.

use anyhow::{Result, bail};
use rusqlite::params;

use crate::store::{Store, now};

/// Notes are facts, not documents; long content belongs in workspace files.
pub const MAX_MEMORY_CHARS: usize = 2000;
/// The trigram tokenizer cannot index shorter terms; they fall back to LIKE.
const TRIGRAM: usize = 3;

#[derive(Debug, Clone, PartialEq)]
pub struct Memory {
    pub id: i64,
    pub content: String,
    pub created_at: i64,
}

fn row(row: &rusqlite::Row<'_>) -> rusqlite::Result<Memory> {
    Ok(Memory {
        id: row.get(0)?,
        content: row.get(1)?,
        created_at: row.get(2)?,
    })
}

impl Store {
    pub fn memory_save(&self, content: &str) -> Result<i64> {
        let content = content.trim();
        if content.is_empty() {
            bail!("memory content is empty");
        }
        if content.chars().count() > MAX_MEMORY_CHARS {
            bail!("memory is longer than {MAX_MEMORY_CHARS} characters; save a shorter summary");
        }
        let conn = self.soul();
        conn.execute(
            "INSERT INTO memories(content, created_at) VALUES (?1, ?2)",
            params![content, now()],
        )?;
        Ok(conn.last_insert_rowid())
    }

    pub fn memory_delete(&self, id: i64) -> Result<bool> {
        Ok(self
            .soul()
            .execute("DELETE FROM memories WHERE id = ?1", [id])?
            > 0)
    }

    pub fn memory_list(&self, limit: usize) -> Result<Vec<Memory>> {
        let conn = self.soul();
        let mut stmt =
            conn.prepare("SELECT id, content, created_at FROM memories ORDER BY id DESC LIMIT ?1")?;
        Ok(stmt
            .query_map([limit as i64], row)?
            .collect::<rusqlite::Result<_>>()?)
    }

    /// Any-term match: full-text ranked hits first, then short-term substring hits.
    pub fn memory_search(&self, query: &str, limit: usize) -> Result<Vec<Memory>> {
        let (long, short): (Vec<&str>, Vec<&str>) = query
            .split_whitespace()
            .partition(|term| term.chars().count() >= TRIGRAM);
        let conn = self.soul();
        let mut found: Vec<Memory> = Vec::new();
        if !long.is_empty() {
            // Quoted phrases keep FTS5 operators in user text from being interpreted.
            let expr = long
                .iter()
                .map(|t| format!("\"{}\"", t.replace('"', "\"\"")))
                .collect::<Vec<_>>()
                .join(" OR ");
            let mut stmt = conn.prepare(
                "SELECT m.id, m.content, m.created_at FROM memories_fts f
                 JOIN memories m ON m.id = f.rowid
                 WHERE memories_fts MATCH ?1 ORDER BY bm25(memories_fts) LIMIT ?2",
            )?;
            found.extend(
                stmt.query_map(params![expr, limit as i64], row)?
                    .collect::<rusqlite::Result<Vec<_>>>()?,
            );
        }
        for term in short {
            if found.len() >= limit {
                break;
            }
            let pattern = format!(
                "%{}%",
                term.replace('\\', "\\\\")
                    .replace('%', "\\%")
                    .replace('_', "\\_")
            );
            let mut stmt = conn.prepare(
                "SELECT id, content, created_at FROM memories WHERE content LIKE ?1 ESCAPE '\\'
                 ORDER BY id DESC LIMIT ?2",
            )?;
            for memory in stmt.query_map(params![pattern, limit as i64], row)? {
                let memory = memory?;
                if found.len() < limit && !found.iter().any(|m| m.id == memory.id) {
                    found.push(memory);
                }
            }
        }
        Ok(found)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_chinese_and_english_by_substring_and_short_terms() {
        let store = Store::open_in_memory().unwrap();
        let tea = store.memory_save("用户喜欢喝乌龙茶，不加糖").unwrap();
        let pi = store
            .memory_save("The home server is a Raspberry Pi 5 running Alpine")
            .unwrap();
        store.memory_save("Weekly meeting is on Tuesday").unwrap();

        let hits = store.memory_search("乌龙茶", 5).unwrap();
        assert_eq!(hits.iter().map(|m| m.id).collect::<Vec<_>>(), vec![tea]);
        // Two-character CJK and short ASCII terms use the substring fallback.
        assert_eq!(
            store
                .memory_search("喝茶 Pi", 5)
                .unwrap()
                .iter()
                .map(|m| m.id)
                .collect::<Vec<_>>(),
            vec![pi]
        );
        assert_eq!(
            store.memory_search("raspberry \"x OR", 5).unwrap()[0].id,
            pi
        );
        assert!(store.memory_search("咖啡", 5).unwrap().is_empty());

        assert!(store.memory_delete(tea).unwrap());
        assert!(store.memory_search("乌龙茶", 5).unwrap().is_empty());
        assert_eq!(store.memory_list(10).unwrap().len(), 2);
        assert!(store.memory_save("  ").is_err());
    }
}
