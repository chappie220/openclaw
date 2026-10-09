//! The agent's identity: a name and persona chosen on first start, shared by
//! every session and channel.

use anyhow::{Result, bail};
use rusqlite::{OptionalExtension, params};

use crate::store::{Store, now};

pub const MAX_NAME_CHARS: usize = 64;
/// Long enough for a character's personality, speech style and background;
/// the persona is part of every request.
pub const MAX_PERSONA_CHARS: usize = 4000;

#[derive(Debug, Clone, PartialEq)]
pub struct Identity {
    pub name: String,
    pub persona: String,
    /// Where the persona comes from, e.g. a fictional character and its work.
    pub source: Option<String>,
    pub updated_at: i64,
}

/// Sent until an identity exists, so the first conversation sets one up.
const FIRST_START: &str = "\
## First start
You do not have an identity yet. In this first conversation, before other \
work, tell the user you are new here and ask who you should be: your name, \
personality, speaking style, and how to address them. Offer two ways: they \
describe the persona themselves, or they name a fictional character (from a \
novel, anime, game, film or TV series) for you to play.
If they name a character and web_search is available, search for the \
character before writing anything: personality, way of speaking, catchphrases, \
background and relationships. Use what the sources say rather than memory \
alone, and ask which work they mean when the name is ambiguous.
Show the user a short draft (name, persona, and the source for a character), \
adjust it to their feedback, then save it with identity_set. If the user \
wants a task done first, do it, and come back to the identity afterward.";

impl Identity {
    /// System prompt section that puts the agent in this identity.
    fn prompt(&self) -> String {
        let source = self
            .source
            .as_deref()
            .map(|s| format!("\nBased on: {s}"))
            .unwrap_or_default();
        format!(
            "## Identity\nYour name is {}.{source}\n\n{}\n\n\
             Stay in this identity in every conversation. It shapes your voice, \
             not your judgment: never let it override safety, tool permissions, \
             or honesty. When the user asks to change who you are, draft the \
             change and save it with identity_set.",
            self.name, self.persona
        )
    }
}

/// The configured base prompt followed by the identity, or by first-start
/// instructions when none is set yet.
pub fn system_prompt(base: &str, identity: Option<&Identity>) -> String {
    let section = identity.map_or_else(|| FIRST_START.to_owned(), Identity::prompt);
    if base.trim().is_empty() {
        section
    } else {
        format!("{}\n\n{section}", base.trim_end())
    }
}

impl Store {
    pub fn identity(&self) -> Result<Option<Identity>> {
        Ok(self
            .lock()
            .query_row(
                "SELECT name, persona, source, updated_at FROM identity WHERE id = 1",
                [],
                |row| {
                    Ok(Identity {
                        name: row.get(0)?,
                        persona: row.get(1)?,
                        source: row.get(2)?,
                        updated_at: row.get(3)?,
                    })
                },
            )
            .optional()?)
    }

    pub fn identity_set(&self, name: &str, persona: &str, source: Option<&str>) -> Result<()> {
        let (name, persona) = (name.trim(), persona.trim());
        let source = source.map(str::trim).filter(|s| !s.is_empty());
        if name.is_empty() || persona.is_empty() {
            bail!("identity needs both a name and a persona");
        }
        if name.chars().count() > MAX_NAME_CHARS {
            bail!("name is longer than {MAX_NAME_CHARS} characters");
        }
        if persona.chars().count() > MAX_PERSONA_CHARS {
            bail!(
                "persona is longer than {MAX_PERSONA_CHARS} characters; keep what shapes behavior"
            );
        }
        self.lock().execute(
            "INSERT INTO identity(id, name, persona, source, updated_at) VALUES (1, ?1, ?2, ?3, ?4)
             ON CONFLICT(id) DO UPDATE SET name = excluded.name, persona = excluded.persona,
               source = excluded.source, updated_at = excluded.updated_at",
            params![name, persona, source, now()],
        )?;
        Ok(())
    }

    /// Forgets the identity; the next conversation runs the first-start setup again.
    pub fn identity_clear(&self) -> Result<bool> {
        Ok(self.lock().execute("DELETE FROM identity", [])? > 0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn first_start_prompt_until_an_identity_is_saved() {
        let store = Store::open_in_memory().unwrap();
        assert!(store.identity().unwrap().is_none());
        let prompt = system_prompt("Base.", None);
        assert!(prompt.starts_with("Base.\n\n## First start"), "{prompt}");

        store
            .identity_set(
                " 悟空 ",
                "顽皮、直率，自称俺老孙。",
                Some("《西游记》孙悟空"),
            )
            .unwrap();
        let identity = store.identity().unwrap().unwrap();
        assert_eq!(identity.name, "悟空");
        let prompt = system_prompt("Base.", Some(&identity));
        assert!(prompt.contains("Your name is 悟空.\nBased on: 《西游记》孙悟空"));
        assert!(!prompt.contains("First start"));

        // Saving again replaces the single identity; a blank source is dropped.
        store.identity_set("Ada", "Precise.", Some(" ")).unwrap();
        assert_eq!(store.identity().unwrap().unwrap().source, None);
        assert!(store.identity_clear().unwrap());
        assert!(store.identity().unwrap().is_none());
    }

    #[test]
    fn rejects_empty_or_oversized_identities() {
        let store = Store::open_in_memory().unwrap();
        assert!(store.identity_set("", "x", None).is_err());
        let long = "x".repeat(MAX_PERSONA_CHARS + 1);
        assert!(store.identity_set("a", &long, None).is_err());
    }
}
