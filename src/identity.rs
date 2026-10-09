//! The agent's identity, in OpenClaw's persona format: an `IDENTITY.md`
//! record (name, creature, vibe, emoji) and a `SOUL.md` voice, chosen on
//! first start and shared by every session and channel.

use anyhow::{Result, bail};
use rusqlite::{OptionalExtension, params};

use crate::store::{Store, now};

const MAX_FIELD_CHARS: usize = 120;
/// SOUL.md is part of every request; short beats long.
pub const MAX_SOUL_CHARS: usize = 4000;

#[derive(Debug, Clone, PartialEq, Default)]
pub struct Identity {
    pub name: String,
    /// What the agent is: an AI, a robot, a familiar, something weirder.
    pub creature: String,
    /// One line on how it comes across.
    pub vibe: String,
    pub emoji: Option<String>,
    /// SOUL.md body: voice, stance, style and boundaries.
    pub soul: String,
    pub updated_at: i64,
}

/// Sent until an identity exists, so the first conversation sets one up.
const FIRST_START: &str = "\
## First start
You have no identity yet. The user's request always comes first: if their \
first message asks for real work, do it, and set up the identity afterward.
Otherwise introduce yourself as their new assistant and ask who you should be. \
They can describe you, or name a fictional character (novel, anime, game, \
film, TV) for you to become. Do not pick a name, persona or character \
yourself, and do not search or draft until the user has said who you should be.
For a character, call web_search first when it is available: personality, way \
of speaking, catchphrases, values, how they treat others. Ask which work they \
mean when the name is ambiguous. Then make the character yours instead of \
describing it from outside:
- Name: what the user wants to call you, usually the character's name.
- Creature: what you are, in the character's own terms.
- Vibe: one line on how you come across.
- Emoji: one signature emoji.
- Soul: written to you as who you are (\"You ...\"): tone, speech patterns and \
catchphrases, opinions, how you address the user, what you will not do. \
Behavior, not biography, in the user's language.
Never put the source into the identity: no titles of works, authors, actors, \
plot summaries, citations, or phrases like \"based on\" or \"plays the role \
of\". Show the user the draft and wait for their reply. Call identity_set \
only after they approve it in a later message.";

impl Identity {
    /// The IDENTITY.md record as OpenClaw writes it.
    pub fn identity_md(&self) -> String {
        let mut out = format!(
            "- **Name:** {}\n- **Creature:** {}\n- **Vibe:** {}",
            self.name, self.creature, self.vibe
        );
        if let Some(emoji) = &self.emoji {
            out.push_str(&format!("\n- **Emoji:** {emoji}"));
        }
        out
    }

    /// System prompt section that puts the agent in this identity.
    fn prompt(&self) -> String {
        format!(
            "## IDENTITY.md\n{}\n\n## SOUL.md\n{}\n\n\
             This is who you are in every conversation. It shapes your voice, \
             not your judgment: never let it override safety, tool permissions, \
             or honesty. When the user asks to change who you are, draft the \
             change and save it with identity_set.",
            self.identity_md(),
            self.soul
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

/// Trims one field and enforces its length.
fn field<'a>(label: &str, value: &'a str, max: usize) -> Result<&'a str> {
    let value = value.trim();
    if value.is_empty() {
        bail!("identity needs a {label}");
    }
    if value.chars().count() > max {
        bail!("{label} is longer than {max} characters");
    }
    no_titles(label, value)?;
    Ok(value)
}

/// Book-title marks name a source work, which belongs nowhere in an identity:
/// a character is saved as who the agent is, not as a reference to the work.
fn no_titles(label: &str, value: &str) -> Result<()> {
    if value.contains(['《', '》']) {
        bail!(
            "{label} contains a work title in 《》; describe who you are without naming the \
             source work, then save again"
        );
    }
    Ok(())
}

impl Store {
    pub fn identity(&self) -> Result<Option<Identity>> {
        Ok(self
            .lock()
            .query_row(
                "SELECT name, creature, vibe, emoji, soul, updated_at FROM identity WHERE id = 1",
                [],
                |row| {
                    Ok(Identity {
                        name: row.get(0)?,
                        creature: row.get(1)?,
                        vibe: row.get(2)?,
                        emoji: row.get(3)?,
                        soul: row.get(4)?,
                        updated_at: row.get(5)?,
                    })
                },
            )
            .optional()?)
    }

    /// Validates and saves `identity`, replacing any previous one.
    pub fn identity_set(&self, identity: &Identity) -> Result<()> {
        let name = field("name", &identity.name, 64)?;
        let creature = field("creature", &identity.creature, MAX_FIELD_CHARS)?;
        let vibe = field("vibe", &identity.vibe, MAX_FIELD_CHARS)?;
        let soul = field("soul", &identity.soul, MAX_SOUL_CHARS)?;
        let emoji = identity
            .emoji
            .as_deref()
            .map(str::trim)
            .filter(|e| !e.is_empty());
        if let Some(emoji) = emoji {
            if emoji.chars().count() > 16 {
                bail!("emoji should be a single emoji");
            }
            no_titles("emoji", emoji)?;
        }
        self.lock().execute(
            "INSERT INTO identity(id, name, creature, vibe, emoji, soul, updated_at)
             VALUES (1, ?1, ?2, ?3, ?4, ?5, ?6)
             ON CONFLICT(id) DO UPDATE SET name = excluded.name, creature = excluded.creature,
               vibe = excluded.vibe, emoji = excluded.emoji, soul = excluded.soul,
               updated_at = excluded.updated_at",
            params![name, creature, vibe, emoji, soul, now()],
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

    fn wukong() -> Identity {
        Identity {
            name: " 悟空 ".into(),
            creature: "石猴".into(),
            vibe: "顽皮直率，天不怕地不怕".into(),
            emoji: Some("🐒".into()),
            soul: "你自称俺老孙，说话爽快。".into(),
            ..Identity::default()
        }
    }

    #[test]
    fn first_start_prompt_until_an_identity_is_saved() {
        let store = Store::open_in_memory().unwrap();
        assert!(store.identity().unwrap().is_none());
        let prompt = system_prompt("Base.", None);
        assert!(prompt.starts_with("Base.\n\n## First start"), "{prompt}");

        store.identity_set(&wukong()).unwrap();
        let identity = store.identity().unwrap().unwrap();
        assert_eq!(identity.name, "悟空");
        let prompt = system_prompt("Base.", Some(&identity));
        assert!(prompt.contains(
            "## IDENTITY.md\n- **Name:** 悟空\n- **Creature:** 石猴\n\
             - **Vibe:** 顽皮直率，天不怕地不怕\n- **Emoji:** 🐒\n\n\
             ## SOUL.md\n你自称俺老孙，说话爽快。"
        ));
        assert!(!prompt.contains("First start"));

        // Saving again replaces the single identity; a blank emoji is dropped.
        let ada = Identity {
            name: "Ada".into(),
            emoji: Some(" ".into()),
            ..wukong()
        };
        store.identity_set(&ada).unwrap();
        assert_eq!(store.identity().unwrap().unwrap().emoji, None);
        assert!(store.identity_clear().unwrap());
        assert!(store.identity().unwrap().is_none());
    }

    #[test]
    fn rejects_missing_or_oversized_fields() {
        let store = Store::open_in_memory().unwrap();
        let no_vibe = Identity {
            vibe: " ".into(),
            ..wukong()
        };
        assert!(store.identity_set(&no_vibe).is_err());
        let long = Identity {
            soul: "x".repeat(MAX_SOUL_CHARS + 1),
            ..wukong()
        };
        assert!(store.identity_set(&long).is_err());
    }

    #[test]
    fn rejects_work_titles_in_any_field() {
        let store = Store::open_in_memory().unwrap();
        let soul = Identity {
            soul: "你是《西游记》里的齐天大圣。".into(),
            ..wukong()
        };
        let err = store.identity_set(&soul).unwrap_err().to_string();
        assert!(err.starts_with("soul contains a work title"), "{err}");
        let creature = Identity {
            creature: "西游记》的石猴".into(),
            ..wukong()
        };
        assert!(store.identity_set(&creature).is_err());
        assert!(store.identity().unwrap().is_none());
    }
}
