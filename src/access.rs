//! Who a turn acts for, and what they may make the agent do.
//!
//! Every turn runs as an [`Actor`] set by the channel that received it: the
//! terminal and the authenticated Web UI act as the owner; QQ and email
//! senders are guests unless `access.owners` names them. Tools check the
//! actor's capabilities when they run, so a model that calls a tool it was
//! not offered is still refused.

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};

/// A group of tools that can be granted on its own.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Capability {
    /// `shell`, still subject to `tools.shell`.
    Shell,
    /// `read_file` and `list_dir`: anything the Gateway's user can read.
    FilesRead,
    /// `write_file` and `edit_file`, still subject to `tools.write`.
    FilesWrite,
    /// Save, search and delete memories; non-owners only see their own.
    Memory,
    /// Add, list and remove scheduled jobs; non-owners only see their own.
    Cron,
    /// Propose and change the agent's identity, shared by every channel.
    Identity,
    /// `web_search`, which spends the search provider's credits.
    WebSearch,
}

impl Capability {
    pub const ALL: [Capability; 7] = [
        Capability::Shell,
        Capability::FilesRead,
        Capability::FilesWrite,
        Capability::Memory,
        Capability::Cron,
        Capability::Identity,
        Capability::WebSearch,
    ];

    /// The capability a built-in tool needs; `None` for tools anyone may call.
    pub fn for_tool(tool: &str) -> Option<Capability> {
        Some(match tool {
            "shell" => Capability::Shell,
            "read_file" | "list_dir" => Capability::FilesRead,
            "write_file" | "edit_file" => Capability::FilesWrite,
            "memory_save" | "memory_search" | "memory_delete" => Capability::Memory,
            "cron_add" | "cron_list" | "cron_remove" => Capability::Cron,
            "identity_set" | "identity_propose" => Capability::Identity,
            "web_search" => Capability::WebSearch,
            _ => return None,
        })
    }
}

/// The authenticated sender of a turn, as its channel identified them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Actor {
    /// `cli`, `web`, `qq:<openid>` or `mail:<address>`.
    pub id: String,
    pub owner: bool,
    pub capabilities: BTreeSet<Capability>,
}

/// Actor id of the local terminal.
pub const CLI: &str = "cli";
/// Actor id of the token-authenticated Web UI.
pub const WEB: &str = "web";

impl Actor {
    /// Someone with the host's own authority: every capability.
    pub fn owner(id: &str) -> Self {
        Self {
            id: id.to_owned(),
            owner: true,
            capabilities: Capability::ALL.into_iter().collect(),
        }
    }

    pub fn can(&self, capability: Capability) -> bool {
        self.capabilities.contains(&capability)
    }

    /// Whether this actor may see or change a record another actor created.
    /// Records from before ownership was tracked belong to the owner.
    pub fn owns(&self, created_by: Option<&str>) -> bool {
        self.owner || created_by == Some(self.id.as_str())
    }

    /// The `created_by` filter for store queries: `None` sees everything.
    pub fn scope(&self) -> Option<&str> {
        (!self.owner).then_some(self.id.as_str())
    }
}

tokio::task_local! {
    static CURRENT_ACTOR: Actor;
}

/// Runs `turn` on behalf of `actor`.
pub async fn with_actor<F: std::future::Future>(actor: Actor, turn: F) -> F::Output {
    CURRENT_ACTOR.scope(actor, turn).await
}

/// The actor of the turn running on this task; `None` outside any turn,
/// which tools treat as having no capabilities at all.
pub fn current() -> Option<Actor> {
    CURRENT_ACTOR.try_with(Clone::clone).ok()
}

/// `[access]`: which channel senders are trusted with what.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct AccessConfig {
    /// Senders treated like the terminal: `qq:<openid>` or `mail:<address>`.
    pub owners: Vec<String>,
    /// What any other sender may do.
    pub guest: Vec<Capability>,
    /// Extra capabilities by sender (`qq:<openid>`, `mail:<address>`), by
    /// session (`qq:group:<group_openid>`, `mail:<address>`), or by channel
    /// (`qq:*`, `mail:*`).
    pub grants: BTreeMap<String, Vec<Capability>>,
}

impl Default for AccessConfig {
    fn default() -> Self {
        Self {
            owners: Vec::new(),
            guest: vec![Capability::WebSearch],
            grants: BTreeMap::new(),
        }
    }
}

impl AccessConfig {
    /// The actor for sender `id` writing in `session`.
    pub fn resolve(&self, id: &str, session: &str) -> Actor {
        if id == CLI || id == WEB {
            return Actor::owner(id);
        }
        let id_lower = id.to_lowercase();
        if self
            .owners
            .iter()
            .any(|o| o.trim().to_lowercase() == id_lower)
        {
            return Actor::owner(id);
        }
        let channel = id
            .split_once(':')
            .map(|(channel, _)| format!("{channel}:*"));
        let mut capabilities: BTreeSet<Capability> = self.guest.iter().copied().collect();
        for (key, granted) in &self.grants {
            let key = key.trim().to_lowercase();
            if key == id_lower || key == session.to_lowercase() || Some(&key) == channel.as_ref() {
                capabilities.extend(granted.iter().copied());
            }
        }
        Actor {
            id: id.to_owned(),
            owner: false,
            capabilities,
        }
    }

    /// Capabilities every sender on `channel` gets without being named, which
    /// is what an open allow-list hands to strangers.
    pub fn unnamed(&self, channel: &str) -> BTreeSet<Capability> {
        let mut capabilities: BTreeSet<Capability> = self.guest.iter().copied().collect();
        if let Some(granted) = self.grants.get(&format!("{channel}:*")) {
            capabilities.extend(granted.iter().copied());
        }
        capabilities
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn channels_resolve_to_owner_guest_or_granted() {
        let config: AccessConfig = toml::from_str(
            r#"
            owners = ["qq:BOSS", "mail:Me@Example.org"]
            guest = []
            [grants]
            "qq:FRIEND" = ["memory"]
            "qq:group:G1" = ["web_search"]
            "mail:*" = ["cron"]
            "#,
        )
        .unwrap();
        assert!(config.resolve(CLI, "main").owner);
        assert!(config.resolve(WEB, "main").owner);
        assert!(config.resolve("qq:BOSS", "qq:c2c:BOSS").owner);
        assert!(
            config
                .resolve("mail:me@example.org", "mail:me@example.org")
                .owner
        );

        let stranger = config.resolve("qq:X", "qq:c2c:X");
        assert!(!stranger.owner && stranger.capabilities.is_empty());
        let friend = config.resolve("qq:FRIEND", "qq:c2c:FRIEND");
        assert_eq!(friend.capabilities, [Capability::Memory].into());
        // A group grant applies to every member writing in that group only.
        let member = config.resolve("qq:M", "qq:group:G1");
        assert_eq!(member.capabilities, [Capability::WebSearch].into());
        assert!(
            config
                .resolve("qq:M", "qq:group:G2")
                .capabilities
                .is_empty()
        );
        let mailer = config.resolve("mail:a@b.c", "mail:a@b.c");
        assert_eq!(mailer.capabilities, [Capability::Cron].into());
        assert_eq!(config.unnamed("mail"), [Capability::Cron].into());
    }

    #[test]
    fn guests_only_own_their_records() {
        let guest = AccessConfig::default().resolve("qq:X", "qq:c2c:X");
        assert!(guest.can(Capability::WebSearch) && !guest.can(Capability::Shell));
        assert!(guest.owns(Some("qq:X")));
        assert!(!guest.owns(Some("qq:Y")) && !guest.owns(None));
        assert_eq!(guest.scope(), Some("qq:X"));
        let owner = Actor::owner(CLI);
        assert!(owner.owns(None) && owner.owns(Some("qq:Y")));
        assert_eq!(owner.scope(), None);
    }
}
