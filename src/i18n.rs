//! Languages of what people read: CLI output and help, the config editor,
//! and the program's own chat replies (`/compact`, `/identity`, errors).
//! Logs and anything the model reads stay in English.
//!
//! The language is chosen once at startup: `--lang`, else `language` in
//! config.toml, else the locale (`LC_ALL`, `LC_MESSAGES`, `LANG`).

use std::sync::atomic::{AtomicU8, Ordering};

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize, clap::ValueEnum)]
#[serde(rename_all = "lowercase")]
pub enum Lang {
    #[default]
    En,
    Zh,
}

impl Lang {
    /// From the locale: Chinese for `zh*`, otherwise English.
    pub fn detect() -> Self {
        let locale = ["LC_ALL", "LC_MESSAGES", "LANG"]
            .iter()
            .filter_map(|name| std::env::var(name).ok())
            .find(|value| !value.is_empty())
            .unwrap_or_default();
        Self::from_locale(&locale)
    }

    pub fn from_locale(locale: &str) -> Self {
        if locale.to_ascii_lowercase().starts_with("zh") {
            Lang::Zh
        } else {
            Lang::En
        }
    }
}

static CURRENT: AtomicU8 = AtomicU8::new(0);

/// Sets the process's language; called once at startup.
pub fn set(lang: Lang) {
    CURRENT.store(lang as u8, Ordering::Relaxed);
}

/// The process's language; English until `set` is called.
pub fn current() -> Lang {
    match CURRENT.load(Ordering::Relaxed) {
        1 => Lang::Zh,
        _ => Lang::En,
    }
}

/// One text in every language.
#[derive(Debug, Clone, Copy)]
pub struct Tr {
    pub en: &'static str,
    pub zh: &'static str,
}

pub const fn tr(en: &'static str, zh: &'static str) -> Tr {
    Tr { en, zh }
}

impl Tr {
    pub fn get(self, lang: Lang) -> &'static str {
        match lang {
            Lang::En => self.en,
            Lang::Zh => self.zh,
        }
    }

    /// The text with each `{}` replaced by the next of `args`.
    pub fn fill(self, lang: Lang, args: &[&str]) -> String {
        let mut out = String::new();
        let mut rest = self.get(lang);
        for arg in args {
            let Some(at) = rest.find("{}") else { break };
            out.push_str(&rest[..at]);
            out.push_str(arg);
            rest = &rest[at + 2..];
        }
        out.push_str(rest);
        out
    }

    /// In the process's language.
    pub fn now(self) -> &'static str {
        self.get(current())
    }

    /// Filled, in the process's language.
    pub fn with(self, args: &[&str]) -> String {
        self.fill(current(), args)
    }
}

/// The program's own replies in chats (QQ, email, Web UI, CLI).
pub mod chat {
    use super::{Tr, tr};

    pub const FAILED: Tr = tr("Something went wrong: {}", "出错了：{}");
    pub const EMPTY: Tr = tr("(no reply)", "（无回复内容）");
    pub const COMPACT_OWNER_ONLY: Tr = tr(
        "Only the owner can compact this conversation.",
        "只有 owner 能压缩这段对话。",
    );
    pub const COMPACT_NOTHING: Tr = tr("Nothing to compact yet.", "还没有可以压缩的内容。");
    pub const COMPACTED: Tr = tr(
        "Compacted {} messages into the summary (about {} tokens).",
        "已把 {} 条消息压缩进摘要（约 {} token）。",
    );
    pub const IDENTITY_OWNER_ONLY: Tr = tr(
        "Only the owner can approve or reject identity drafts.",
        "只有 owner 能批准或拒绝身份草稿。",
    );
    pub const NOT_A_DRAFT: Tr = tr("{} is not a draft number", "{} 不是草稿编号");
    pub const DRAFT_APPROVED: Tr = tr(
        "Identity draft #{} approved: I am now {}.",
        "身份草稿 #{} 已批准：我现在是{}。",
    );
    pub const DRAFT_REJECTED: Tr = tr(
        "Identity draft #{} rejected; nothing was saved. Tell me what to change.",
        "身份草稿 #{} 已拒绝，没有保存任何内容。告诉我要改什么。",
    );
    pub const NO_DRAFT: Tr = tr("No identity draft is waiting.", "没有等待批准的身份草稿。");
    pub const IDENTITY_USAGE: Tr = tr(
        "Usage: /identity [show] | /identity approve <n> [code] | /identity reject <n>",
        "用法：/identity [show] | /identity approve <编号> [验证码] | /identity reject <编号>",
    );
    pub const APPROVAL_HINT: Tr = tr(
        "Reply \"/identity approve {} {}\" to save it, or \"/identity reject {}\".",
        "回复“/identity approve {} {}”保存，或回复“/identity reject {}”拒绝。",
    );
    pub const DRAFT_TITLE: Tr = tr("Identity draft #{} (code {})", "身份草稿 #{}（验证码 {}）");
    pub const STOPPED: Tr = tr("Stopped.", "已停止。");
    pub const STOPPING: Tr = tr("Stopping the current turn.", "正在停止当前这一轮。");
    pub const NOTHING_RUNNING: Tr = tr(
        "Nothing is running in this conversation.",
        "这段对话里没有正在运行的任务。",
    );
    pub const STOP_NOT_ALLOWED: Tr = tr(
        "Only the owner or whoever started it can stop this turn.",
        "只有 owner 或发起这一轮的人能停止它。",
    );
    pub const FOLLOW_UP_QUEUED: Tr = tr(
        "Got it. I'll add this to what I'm working on at a good moment.",
        "收到，我会在合适的时机把这条加入正在进行的对话。",
    );
    pub const FOLLOW_UP_ADDED: Tr = tr(
        "Got it, I'm taking that into account now.",
        "收到，我现在把这条考虑进去。",
    );

    #[cfg(test)]
    pub const ALL: &[Tr] = &[
        FAILED,
        EMPTY,
        COMPACT_OWNER_ONLY,
        COMPACT_NOTHING,
        COMPACTED,
        IDENTITY_OWNER_ONLY,
        NOT_A_DRAFT,
        DRAFT_APPROVED,
        DRAFT_REJECTED,
        NO_DRAFT,
        IDENTITY_USAGE,
        APPROVAL_HINT,
        DRAFT_TITLE,
        STOPPED,
        STOPPING,
        NOTHING_RUNNING,
        STOP_NOT_ALLOWED,
        FOLLOW_UP_QUEUED,
        FOLLOW_UP_ADDED,
    ];
}

/// Checks that each text exists in both languages with the same placeholders.
#[cfg(test)]
pub fn assert_complete(texts: &[Tr]) {
    for text in texts {
        assert!(!text.en.is_empty() && !text.zh.is_empty(), "{}", text.en);
        assert_eq!(
            text.en.matches("{}").count(),
            text.zh.matches("{}").count(),
            "placeholders differ: {}",
            text.en
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn picks_chinese_from_the_locale() {
        assert_eq!(Lang::from_locale("zh_CN.UTF-8"), Lang::Zh);
        assert_eq!(Lang::from_locale("zh_TW"), Lang::Zh);
        assert_eq!(Lang::from_locale("en_US.UTF-8"), Lang::En);
        assert_eq!(Lang::from_locale("C"), Lang::En);
        assert_eq!(Lang::from_locale(""), Lang::En);
    }

    #[test]
    fn chat_replies_have_both_languages() {
        assert_complete(chat::ALL);
    }

    #[test]
    fn fills_placeholders_in_order() {
        let t = tr("{} of {}", "{}/{}");
        assert_eq!(t.fill(Lang::En, &["1", "2"]), "1 of 2");
        assert_eq!(t.fill(Lang::Zh, &["1", "2"]), "1/2");
        assert_eq!(t.fill(Lang::En, &["1"]), "1 of {}");
    }
}
