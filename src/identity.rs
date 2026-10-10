//! The agent's identity, in OpenClaw's persona format: an `IDENTITY.md`
//! record (name, creature, vibe, emoji) and a `SOUL.md` voice, chosen on
//! first start and shared by every session and channel.

use anyhow::{Result, bail};
use rusqlite::{OptionalExtension, params};

use crate::i18n::chat;
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
of\". When the user has said who you should be, call identity_set with the \
draft. It saves nothing by itself: the program shows the user the exact draft \
and only their approval saves it. If they reject it, ask what to change and \
propose a revised draft.";

/// Sent until an identity exists, to someone who cannot set one up.
const NO_IDENTITY: &str = "\
## Identity
You have no saved identity yet, and only the owner can set one up. Do not \
invent a persona; just help with the request as a plain assistant.";

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
             or honesty. When the user asks to change who you are, propose the \
             change with identity_set; it takes effect once they approve it.",
            self.identity_md(),
            self.soul
        )
    }
}

/// The configured base prompt followed by the identity, or by first-start
/// instructions when none is set yet and this sender `can_set` one.
pub fn system_prompt(base: &str, identity: Option<&Identity>, can_set: bool) -> String {
    let section = match identity {
        Some(identity) => identity.prompt(),
        None if can_set => FIRST_START.to_owned(),
        None => NO_IDENTITY.to_owned(),
    };
    if base.trim().is_empty() {
        section
    } else {
        format!("{}\n\n{section}", base.trim_end())
    }
}

/// Handles `/identity ...` typed by a person, before any model sees it, so
/// only the program decides what counts as approval. `None` if `text` is not
/// such a command.
pub fn command(store: &Store, actor: &crate::access::Actor, text: &str) -> Option<String> {
    let line = text.lines().next()?.trim();
    let rest = line.strip_prefix("/identity")?;
    if !rest.is_empty() && !rest.starts_with(char::is_whitespace) {
        return None;
    }
    if !actor.can(crate::access::Capability::Identity) {
        return Some(chat::IDENTITY_OWNER_ONLY.now().into());
    }
    let words: Vec<&str> = rest.split_whitespace().collect();
    let decide = |approve: bool, id: &str, code: Option<&&str>| -> String {
        let Ok(id) = id.trim_start_matches('#').parse::<i64>() else {
            return chat::NOT_A_DRAFT.with(&[&format!("{id:?}")]);
        };
        match store.identity_decide(id, approve, code.copied(), &actor.id) {
            Ok(d) if approve => chat::DRAFT_APPROVED.with(&[&d.id.to_string(), &d.identity.name]),
            Ok(d) => chat::DRAFT_REJECTED.with(&[&d.id.to_string()]),
            Err(err) => format!("{err:#}"),
        }
    };
    Some(match words.as_slice() {
        ["approve", id, code @ ..] => decide(true, id, code.first()),
        ["reject", id, ..] => decide(false, id, None),
        [] | ["show"] => match store.identity_drafts_awaiting() {
            Ok(drafts) if drafts.is_empty() => chat::NO_DRAFT.now().into(),
            Ok(drafts) => drafts
                .iter()
                .map(|d| format!("{}\n\n{}", d.render(), approval_hint(d)))
                .collect::<Vec<_>>()
                .join("\n\n"),
            Err(err) => format!("{err:#}"),
        },
        _ => chat::IDENTITY_USAGE.now().into(),
    })
}

/// How a person approves `draft` from a chat channel.
pub fn approval_hint(draft: &Draft) -> String {
    let id = draft.id.to_string();
    chat::APPROVAL_HINT.with(&[&id, &draft.hash, &id])
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

/// Checks every field and returns them trimmed, as they would be saved.
fn validated(identity: &Identity) -> Result<Identity> {
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
    Ok(Identity {
        name: name.into(),
        creature: creature.into(),
        vibe: vibe.into(),
        emoji: emoji.map(Into::into),
        soul: soul.into(),
        updated_at: 0,
    })
}

fn write_identity(conn: &rusqlite::Connection, identity: &Identity) -> Result<()> {
    conn.execute(
        "INSERT INTO identity(id, name, creature, vibe, emoji, soul, updated_at)
         VALUES (1, ?1, ?2, ?3, ?4, ?5, ?6)
         ON CONFLICT(id) DO UPDATE SET name = excluded.name, creature = excluded.creature,
           vibe = excluded.vibe, emoji = excluded.emoji, soul = excluded.soul,
           updated_at = excluded.updated_at",
        params![
            identity.name,
            identity.creature,
            identity.vibe,
            identity.emoji,
            identity.soul,
            now()
        ],
    )?;
    Ok(())
}

/// A proposed identity and where its approval stands.
#[derive(Debug, Clone, PartialEq)]
pub struct Draft {
    pub id: i64,
    pub identity: Identity,
    /// Short code identifying the exact fields, shown next to the draft number.
    pub hash: String,
    pub proposed_by: String,
    pub session: String,
    pub state: String,
}

impl Draft {
    /// The draft exactly as stored, for a person to approve.
    pub fn render(&self) -> String {
        format!(
            "{}\n{}\n\nSOUL.md:\n{}",
            chat::DRAFT_TITLE.with(&[&self.id.to_string(), &self.hash]),
            self.identity.identity_md(),
            self.identity.soul
        )
    }
}

const DRAFT_COLUMNS: &str =
    "id, name, creature, vibe, emoji, soul, hash, proposed_by, session, state";

fn draft(row: &rusqlite::Row<'_>) -> rusqlite::Result<Draft> {
    Ok(Draft {
        id: row.get(0)?,
        identity: Identity {
            name: row.get(1)?,
            creature: row.get(2)?,
            vibe: row.get(3)?,
            emoji: row.get(4)?,
            soul: row.get(5)?,
            updated_at: 0,
        },
        hash: row.get(6)?,
        proposed_by: row.get(7)?,
        session: row.get(8)?,
        state: row.get(9)?,
    })
}

/// FNV-1a over the fields: stable across builds, short enough to type.
fn draft_hash(identity: &Identity) -> String {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for field in [
        identity.name.as_str(),
        &identity.creature,
        &identity.vibe,
        identity.emoji.as_deref().unwrap_or(""),
        &identity.soul,
    ] {
        for byte in field.bytes().chain([0xff]) {
            hash ^= u64::from(byte);
            hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
        }
    }
    format!("{:06x}", hash & 0xff_ffff)
}

impl Store {
    pub fn identity(&self) -> Result<Option<Identity>> {
        Ok(self
            .soul()
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

    /// Validates and saves `identity`, replacing any previous one. Only the
    /// owner's own command line and approved drafts reach this.
    pub fn identity_set(&self, identity: &Identity) -> Result<()> {
        let identity = validated(identity)?;
        write_identity(&self.soul(), &identity)
    }

    /// Records a proposed identity for approval; older open drafts are superseded.
    pub fn identity_propose(&self, identity: &Identity, by: &str, session: &str) -> Result<Draft> {
        let identity = validated(identity)?;
        let hash = draft_hash(&identity);
        let mut conn = self.soul();
        let tx = conn.transaction()?;
        let ts = now();
        tx.execute(
            "UPDATE identity_drafts SET state = 'superseded', decided_at = ?1
             WHERE state = 'awaiting'",
            [ts],
        )?;
        tx.execute(
            "INSERT INTO identity_drafts(name, creature, vibe, emoji, soul, hash, proposed_by,
                                         session, state, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, 'awaiting', ?9)",
            params![
                identity.name,
                identity.creature,
                identity.vibe,
                identity.emoji,
                identity.soul,
                hash,
                by,
                session,
                ts
            ],
        )?;
        let id = tx.last_insert_rowid();
        tx.commit()?;
        Ok(Draft {
            id,
            identity,
            hash,
            proposed_by: by.into(),
            session: session.into(),
            state: "awaiting".into(),
        })
    }

    /// Drafts still waiting for a decision (at most one).
    pub fn identity_drafts_awaiting(&self) -> Result<Vec<Draft>> {
        let conn = self.soul();
        let mut stmt = conn.prepare(&format!(
            "SELECT {DRAFT_COLUMNS} FROM identity_drafts WHERE state = 'awaiting' ORDER BY id"
        ))?;
        Ok(stmt
            .query_map([], draft)?
            .collect::<rusqlite::Result<_>>()?)
    }

    /// The open draft proposed in `session` that its channel has not shown
    /// yet, marking it shown.
    pub fn identity_draft_to_announce(&self, session: &str) -> Result<Option<Draft>> {
        let conn = self.soul();
        let found = conn
            .query_row(
                &format!(
                    "SELECT {DRAFT_COLUMNS} FROM identity_drafts
                     WHERE state = 'awaiting' AND announced = 0 AND session = ?1"
                ),
                [session],
                draft,
            )
            .optional()?;
        if let Some(d) = &found {
            conn.execute(
                "UPDATE identity_drafts SET announced = 1 WHERE id = ?1",
                [d.id],
            )?;
        }
        Ok(found)
    }

    /// Approves or rejects draft `id` on behalf of `by`. Only an open draft
    /// can be decided, and only once; `hash`, when given, must match what
    /// the person was shown. Approval saves the exact stored fields.
    pub fn identity_decide(
        &self,
        id: i64,
        approve: bool,
        hash: Option<&str>,
        by: &str,
    ) -> Result<Draft> {
        let mut conn = self.soul();
        let tx = conn.transaction()?;
        let found = tx
            .query_row(
                &format!("SELECT {DRAFT_COLUMNS} FROM identity_drafts WHERE id = ?1"),
                [id],
                draft,
            )
            .optional()?;
        let Some(mut found) = found else {
            bail!("there is no identity draft #{id}");
        };
        match found.state.as_str() {
            "awaiting" => {}
            "superseded" => bail!("identity draft #{id} was replaced by a newer draft"),
            state => bail!("identity draft #{id} was already {state}"),
        }
        if hash.is_some_and(|h| !h.eq_ignore_ascii_case(&found.hash)) {
            bail!(
                "identity draft #{id} has code {}, not {}",
                found.hash,
                hash.unwrap_or("")
            );
        }
        // The row is never updated, but check the fields are still what was shown.
        if draft_hash(&found.identity) != found.hash {
            bail!("identity draft #{id} does not match its code; propose it again");
        }
        let state = if approve { "committed" } else { "rejected" };
        if approve {
            write_identity(&tx, &validated(&found.identity)?)?;
        }
        tx.execute(
            "UPDATE identity_drafts SET state = ?2, decided_by = ?3, decided_at = ?4
             WHERE id = ?1",
            params![id, state, by, now()],
        )?;
        tx.commit()?;
        found.state = state.into();
        Ok(found)
    }

    /// Forgets the identity; the next conversation runs the first-start setup again.
    pub fn identity_clear(&self) -> Result<bool> {
        Ok(self.soul().execute("DELETE FROM identity", [])? > 0)
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
        let prompt = system_prompt("Base.", None, true);
        assert!(prompt.starts_with("Base.\n\n## First start"), "{prompt}");

        store.identity_set(&wukong()).unwrap();
        let identity = store.identity().unwrap().unwrap();
        assert_eq!(identity.name, "悟空");
        let prompt = system_prompt("Base.", Some(&identity), true);
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
