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
    /// Saves a memory as the owner.
    pub fn memory_save(&self, content: &str) -> Result<i64> {
        self.memory_save_by(content, None)
    }

    /// Saves a memory recorded as `created_by`'s.
    pub fn memory_save_by(&self, content: &str, created_by: Option<&str>) -> Result<i64> {
        let content = content.trim();
        if content.is_empty() {
            bail!("memory content is empty");
        }
        if content.chars().count() > MAX_MEMORY_CHARS {
            bail!("memory is longer than {MAX_MEMORY_CHARS} characters; save a shorter summary");
        }
        let conn = self.soul();
        conn.execute(
            "INSERT INTO memories(content, created_at, created_by) VALUES (?1, ?2, ?3)",
            params![content, now(), created_by],
        )?;
        Ok(conn.last_insert_rowid())
    }

    pub fn memory_delete(&self, id: i64) -> Result<bool> {
        self.memory_delete_in(id, None)
    }

    /// Deletes memory `id` if it is within `scope` (`None`: any memory).
    pub fn memory_delete_in(&self, id: i64, scope: Option<&str>) -> Result<bool> {
        Ok(self.soul().execute(
            "DELETE FROM memories WHERE id = ?1 AND (?2 IS NULL OR created_by = ?2)",
            params![id, scope],
        )? > 0)
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
        self.memory_search_in(query, limit, None)
    }

    /// Like [`Store::memory_search`], limited to memories saved by `scope`.
    pub fn memory_search_in(
        &self,
        query: &str,
        limit: usize,
        scope: Option<&str>,
    ) -> Result<Vec<Memory>> {
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
                 WHERE memories_fts MATCH ?1 AND (?3 IS NULL OR m.created_by = ?3)
                 ORDER BY bm25(memories_fts) LIMIT ?2",
            )?;
            found.extend(
                stmt.query_map(params![expr, limit as i64, scope], row)?
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
                 AND (?3 IS NULL OR created_by = ?3) ORDER BY id DESC LIMIT ?2",
            )?;
            for memory in stmt.query_map(params![pattern, limit as i64, scope], row)? {
                let memory = memory?;
                if found.len() < limit && !found.iter().any(|m| m.id == memory.id) {
                    found.push(memory);
                }
            }
        }
        Ok(found)
    }
}

/// Common English words that say nothing about which memory is relevant.
const STOPWORDS: &[&str] = &[
    "the", "and", "for", "you", "are", "was", "were", "what", "when", "where", "which", "who",
    "how", "why", "this", "that", "these", "those", "with", "have", "has", "had", "can", "could",
    "would", "should", "will", "please", "about", "from", "your", "yours", "mine", "our", "they",
    "them", "their", "there", "here", "not", "but", "all", "any", "some", "just", "also", "into",
    "than", "then", "too", "very", "does", "did", "done", "its", "let", "get", "got", "tell",
    "know", "want", "need", "like", "make", "today",
];

/// Most terms taken from one message, so a pasted document stays cheap.
const MAX_RECALL_TERMS: usize = 48;

/// Chinese function characters; a trigram with one of them is mostly glue
/// (我想要, 一杯乌, 龙茶吧) and says little about the topic.
const CJK_GLUE: &str = "我你您他她它们的了吗呢吧啊呀是在有和与就都也还要想会能可去来这那么什怎样个一不没很太好请帮给把被让着过到对从为";

fn is_cjk(c: char) -> bool {
    matches!(c as u32,
        0x3040..=0x30FF | 0x3400..=0x4DBF | 0x4E00..=0x9FFF | 0xAC00..=0xD7AF | 0xF900..=0xFAFF)
}

/// Search terms in `text`: ASCII words of three or more letters that are not
/// stopwords, and every run of three CJK characters without a function
/// character, since CJK has no spaces and the trigram index matches
/// three-character substrings.
fn recall_terms(text: &str) -> Vec<String> {
    let mut terms: Vec<String> = Vec::new();
    let push = |term: String, terms: &mut Vec<String>| {
        if terms.len() < MAX_RECALL_TERMS && !terms.contains(&term) {
            terms.push(term);
        }
    };
    let lower = text.to_lowercase();
    let mut word = String::new();
    let mut cjk: Vec<char> = Vec::new();
    for c in lower.chars().chain(std::iter::once(' ')) {
        if c.is_ascii_alphanumeric() {
            word.push(c);
        } else if !word.is_empty() {
            if word.len() >= TRIGRAM && !STOPWORDS.contains(&word.as_str()) {
                push(std::mem::take(&mut word), &mut terms);
            }
            word.clear();
        }
        if is_cjk(c) {
            cjk.push(c);
        } else if !cjk.is_empty() {
            for window in cjk.windows(TRIGRAM) {
                if !window.iter().any(|c| CJK_GLUE.contains(*c)) {
                    push(window.iter().collect(), &mut terms);
                }
            }
            cjk.clear();
        }
    }
    terms
}

impl Store {
    /// Memories within `scope` that share terms with `text`, best first, for
    /// recall at the start of a turn. With more than three terms a memory
    /// must share at least two, so one common word does not pull it in.
    pub fn memory_recall_in(
        &self,
        text: &str,
        limit: usize,
        scope: Option<&str>,
    ) -> Result<Vec<Memory>> {
        let terms = recall_terms(text);
        if terms.is_empty() || limit == 0 {
            return Ok(Vec::new());
        }
        let expr = terms
            .iter()
            .map(|t| format!("\"{}\"", t.replace('"', "\"\"")))
            .collect::<Vec<_>>()
            .join(" OR ");
        let candidates: Vec<Memory> = {
            let conn = self.soul();
            let mut stmt = conn.prepare(
                "SELECT m.id, m.content, m.created_at FROM memories_fts f
                 JOIN memories m ON m.id = f.rowid
                 WHERE memories_fts MATCH ?1 AND (?3 IS NULL OR m.created_by = ?3)
                 ORDER BY bm25(memories_fts) LIMIT ?2",
            )?;
            stmt.query_map(params![expr, (limit * 4) as i64, scope], row)?
                .collect::<rusqlite::Result<_>>()?
        };
        let needed = if terms.len() > 3 { 2 } else { 1 };
        let mut scored: Vec<(usize, Memory)> = candidates
            .into_iter()
            .map(|m| {
                let content = m.content.to_lowercase();
                let shared = terms
                    .iter()
                    .filter(|t| content.contains(t.as_str()))
                    .count();
                (shared, m)
            })
            .filter(|(shared, _)| *shared >= needed)
            .collect();
        // Stable, so equal scores keep the full-text ranking.
        scored.sort_by_key(|(shared, _)| std::cmp::Reverse(*shared));
        Ok(scored.into_iter().take(limit).map(|(_, m)| m).collect())
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

    #[test]
    fn recall_terms_cover_words_and_cjk_trigrams() {
        assert_eq!(
            recall_terms("What does the Raspberry Pi run? 我想喝乌龙茶"),
            ["raspberry", "run", "喝乌龙", "乌龙茶"]
        );
        assert!(recall_terms("hi 你好").is_empty());
        assert!(recall_terms(&"字".repeat(500)).len() <= 1);
    }

    #[test]
    fn recalls_memories_that_share_enough_with_the_message() {
        let store = Store::open_in_memory().unwrap();
        let tea = store.memory_save("用户喜欢喝乌龙茶，不加糖").unwrap();
        let pi = store
            .memory_save("The home server is a Raspberry Pi 5 running Alpine")
            .unwrap();
        store
            .memory_save("Weekly meeting is on Tuesday at 10")
            .unwrap();
        let ids = |text: &str, scope| -> Vec<i64> {
            store
                .memory_recall_in(text, 5, scope)
                .unwrap()
                .iter()
                .map(|m| m.id)
                .collect()
        };
        assert_eq!(ids("帮我泡一杯乌龙茶吧", None), [tea]);
        assert_eq!(
            ids("Is the Alpine server on the Raspberry Pi still up?", None),
            [pi]
        );
        // One shared word among many is not enough.
        assert!(ids("Which server should I buy for my office next year?", None).is_empty());
        assert!(ids("你好", None).is_empty());
        // A guest recalls only their own memories.
        let own = store
            .memory_save_by("访客喜欢乌龙茶和绿茶", Some("qq:X"))
            .unwrap();
        assert_eq!(ids("推荐一款乌龙茶", Some("qq:X")), [own]);
    }

    #[test]
    fn scoped_memories_are_private_to_their_creator() {
        let store = Store::open_in_memory().unwrap();
        let owner = store.memory_save("主人的银行卡密码提示").unwrap();
        let guest = store.memory_save_by("访客喜欢猫咪", Some("qq:X")).unwrap();
        assert!(
            store
                .memory_search_in("银行卡", 5, Some("qq:X"))
                .unwrap()
                .is_empty()
        );
        assert!(
            store
                .memory_search_in("卡", 5, Some("qq:X"))
                .unwrap()
                .is_empty()
        );
        assert_eq!(
            store.memory_search_in("猫咪", 5, Some("qq:X")).unwrap()[0].id,
            guest
        );
        assert!(!store.memory_delete_in(owner, Some("qq:X")).unwrap());
        assert!(!store.memory_delete_in(guest, Some("qq:Y")).unwrap());
        assert!(store.memory_delete_in(guest, Some("qq:X")).unwrap());
        assert!(store.memory_delete(owner).unwrap());
    }
}
