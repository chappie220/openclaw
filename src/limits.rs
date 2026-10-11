//! Spending limits: model calls stop once the agent, or one sender, has
//! spent what `[limits]` allows today or this month, so a chatty guest, a
//! runaway tool loop or a scheduled job cannot drain the OpenRouter balance.
//!
//! Spending is what OpenRouter reports per call (`usage.cost`), recorded
//! with the sender it was for. A call's cost is only known once it is done,
//! so a limit can be passed by the last call before it is checked again.

use std::collections::BTreeMap;

use anyhow::Result;
use chrono::{Datelike, Local, TimeZone};
use rusqlite::params;
use serde::{Deserialize, Serialize};

use crate::access::Actor;
use crate::i18n::{Tr, tr};
use crate::store::Store;

/// `[limits]`, in US dollars; unset or 0 means no limit.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct LimitsConfig {
    /// What the whole agent may spend per day (local time), owners included.
    pub daily_usd: Option<f64>,
    /// What the whole agent may spend per calendar month.
    pub monthly_usd: Option<f64>,
    /// What each sender who is not an owner may spend per day.
    pub guest_daily_usd: Option<f64>,
    /// Per-day limits for named senders (`qq:<openid>`, `mail:<address>`),
    /// instead of `guest_daily_usd`; owners named here are limited too.
    pub senders: BTreeMap<String, f64>,
    /// What one turn may spend before it is stopped.
    pub turn_usd: Option<f64>,
}

impl Default for LimitsConfig {
    fn default() -> Self {
        Self {
            daily_usd: None,
            monthly_usd: None,
            guest_daily_usd: Some(DEFAULT_GUEST_DAILY_USD),
            senders: BTreeMap::new(),
            turn_usd: None,
        }
    }
}

pub const DEFAULT_GUEST_DAILY_USD: f64 = 0.5;

fn set(limit: Option<f64>) -> Option<f64> {
    limit.filter(|l| *l > 0.0)
}

/// Which limit was reached.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Daily,
    Monthly,
    Sender,
    Turn,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Exceeded {
    pub kind: Kind,
    pub spent: f64,
    pub limit: f64,
}

const DAILY: Tr = tr(
    "Today's spending limit is reached (${} of ${}); the agent answers again after midnight.",
    "今天的花费已达上限（${} / ${}），午夜之后才能继续回复。",
);
const MONTHLY: Tr = tr(
    "This month's spending limit is reached (${} of ${}); the agent answers again next month.",
    "本月的花费已达上限（${} / ${}），下个月才能继续回复。",
);
const SENDER: Tr = tr(
    "You have used up today's allowance (${} of ${}); try again after midnight.",
    "你今天的额度已用完（${} / ${}），午夜之后再试吧。",
);
const TURN: Tr = tr(
    "Stopped: this turn spent ${}, its limit is ${}.",
    "已停止：这一轮花费了 ${}，上限是 ${}。",
);
const OWNER_HINT: Tr = tr(
    "Raise or remove {} in config.toml to go on now.",
    "如需现在继续，请在 config.toml 里调高或删除 {}。",
);

const TODAY: Tr = tr("Spent today: ${}", "今天已花费：${}");
const THIS_MONTH: Tr = tr("Spent this month: ${}", "本月已花费：${}");
const OF_LIMIT: Tr = tr(" (limit ${})", "（上限 ${}）");
const GUEST_LIMIT: Tr = tr(
    "Each guest may spend ${} a day (limits.guest_daily_usd).",
    "每个访客每天最多花费 ${}（limits.guest_daily_usd）。",
);
const NO_GUEST_LIMIT: Tr = tr(
    "Guests have no daily limit (limits.guest_daily_usd = 0).",
    "访客没有每日上限（limits.guest_daily_usd = 0）。",
);

#[cfg(test)]
pub const ALL: &[Tr] = &[
    DAILY,
    MONTHLY,
    SENDER,
    TURN,
    OWNER_HINT,
    TODAY,
    THIS_MONTH,
    OF_LIMIT,
    GUEST_LIMIT,
    NO_GUEST_LIMIT,
];

impl Exceeded {
    /// What the person is told, in their language; owners also learn which
    /// setting to change.
    pub fn message(&self, owner: bool) -> String {
        let text = match self.kind {
            Kind::Daily => DAILY,
            Kind::Monthly => MONTHLY,
            Kind::Sender => SENDER,
            Kind::Turn => TURN,
        };
        let mut out = text.with(&[&money(self.spent), &money(self.limit)]);
        if owner {
            let key = match self.kind {
                Kind::Daily => "limits.daily_usd",
                Kind::Monthly => "limits.monthly_usd",
                Kind::Sender => "limits.senders",
                Kind::Turn => "limits.turn_usd",
            };
            out.push(' ');
            out.push_str(&OWNER_HINT.with(&[key]));
        }
        out
    }

    /// The note left in the history for the model.
    pub fn note(&self) -> String {
        let what = match self.kind {
            Kind::Daily => "the agent's daily",
            Kind::Monthly => "the agent's monthly",
            Kind::Sender => "this sender's daily",
            Kind::Turn => "this turn's",
        };
        format!(
            "[Stopped: {what} spending limit was reached (${} of ${}).]",
            money(self.spent),
            money(self.limit)
        )
    }
}

pub fn money(usd: f64) -> String {
    let text = if usd >= 1.0 {
        format!("{usd:.2}")
    } else {
        format!("{usd:.4}")
    };
    text.trim_end_matches('0').trim_end_matches('.').to_owned()
}

/// Unix time of the last local midnight.
pub fn day_start() -> i64 {
    local_start(Local::now().date_naive())
}

/// Unix time of the first of this month, local time.
pub fn month_start() -> i64 {
    let today = Local::now().date_naive();
    local_start(today.with_day(1).unwrap_or(today))
}

fn local_start(date: chrono::NaiveDate) -> i64 {
    let midnight = date.and_hms_opt(0, 0, 0).expect("midnight exists");
    Local
        .from_local_datetime(&midnight)
        .earliest()
        .map_or_else(|| midnight.and_utc().timestamp(), |t| t.timestamp())
}

impl LimitsConfig {
    /// The per-day limit for `actor`, if any.
    pub fn sender_limit(&self, actor: &Actor) -> Option<f64> {
        match self.senders.get(&actor.id) {
            Some(limit) => set(Some(*limit)),
            None if actor.owner => None,
            None => set(self.guest_daily_usd),
        }
    }

    pub fn turn_limit(&self) -> Option<f64> {
        set(self.turn_usd)
    }

    /// The first limit `actor` has reached, if any.
    pub fn check(&self, store: &Store, actor: &Actor) -> Result<Option<Exceeded>> {
        let day = day_start();
        if let Some(limit) = set(self.daily_usd) {
            let spent = store.spent_since(day, None)?;
            if spent >= limit {
                return Ok(Some(Exceeded {
                    kind: Kind::Daily,
                    spent,
                    limit,
                }));
            }
        }
        if let Some(limit) = set(self.monthly_usd) {
            let spent = store.spent_since(month_start(), None)?;
            if spent >= limit {
                return Ok(Some(Exceeded {
                    kind: Kind::Monthly,
                    spent,
                    limit,
                }));
            }
        }
        if let Some(limit) = self.sender_limit(actor) {
            let spent = store.spent_since(day, Some(&actor.id))?;
            if spent >= limit {
                return Ok(Some(Exceeded {
                    kind: Kind::Sender,
                    spent,
                    limit,
                }));
            }
        }
        Ok(None)
    }

    /// Today's and this month's spending against the limits, for `usage`.
    pub fn summary(&self, store: &Store) -> Result<String> {
        let line = |text: Tr, spent: f64, limit: Option<f64>| {
            let mut out = text.with(&[&money(spent)]);
            if let Some(limit) = set(limit) {
                out.push_str(&OF_LIMIT.with(&[&money(limit)]));
            }
            out
        };
        let mut out = format!(
            "{}\n{}\n",
            line(TODAY, store.spent_since(day_start(), None)?, self.daily_usd),
            line(
                THIS_MONTH,
                store.spent_since(month_start(), None)?,
                self.monthly_usd
            ),
        );
        out.push_str(&match set(self.guest_daily_usd) {
            Some(limit) => GUEST_LIMIT.with(&[&money(limit)]),
            None => NO_GUEST_LIMIT.now().to_owned(),
        });
        out.push('\n');
        Ok(out)
    }
}

impl Store {
    /// USD spent since `since` (unix seconds), by everyone or by one actor.
    pub fn spent_since(&self, since: i64, actor: Option<&str>) -> Result<f64> {
        let conn = self.runtime();
        let spent: f64 = match actor {
            Some(actor) => conn.query_row(
                "SELECT COALESCE(SUM(cost), 0) FROM usage WHERE created_at >= ?1 AND actor = ?2",
                params![since, actor],
                |row| row.get(0),
            )?,
            None => conn.query_row(
                "SELECT COALESCE(SUM(cost), 0) FROM usage WHERE created_at >= ?1",
                params![since],
                |row| row.get(0),
            )?,
        };
        Ok(spent)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::llm::Usage;

    #[test]
    fn texts_have_both_languages() {
        crate::i18n::assert_complete(ALL);
    }

    fn spend(store: &Store, actor: &str, cost: f64) {
        let usage = Usage {
            cost,
            ..Usage::default()
        };
        store
            .record_usage("s", "turn", Some(actor), None, &usage)
            .unwrap();
    }

    fn guest(id: &str) -> Actor {
        Actor {
            id: id.into(),
            owner: false,
            capabilities: Default::default(),
        }
    }

    #[test]
    fn each_limit_counts_what_it_covers() {
        let store = Store::open_in_memory().unwrap();
        let owner = Actor::owner(crate::access::CLI);
        let limits = LimitsConfig {
            guest_daily_usd: Some(0.1),
            senders: [("qq:friend".to_owned(), 1.0)].into(),
            ..LimitsConfig::default()
        };
        spend(&store, "qq:a", 0.06);
        assert_eq!(limits.check(&store, &guest("qq:a")).unwrap(), None);
        spend(&store, "qq:a", 0.05);
        let hit = limits.check(&store, &guest("qq:a")).unwrap().unwrap();
        assert_eq!(hit.kind, Kind::Sender);
        assert!((hit.spent - 0.11).abs() < 1e-9);
        // Other senders and owners have their own allowance.
        assert_eq!(limits.check(&store, &guest("qq:b")).unwrap(), None);
        spend(&store, "qq:friend", 0.5);
        assert_eq!(limits.check(&store, &guest("qq:friend")).unwrap(), None);
        spend(&store, "cli", 5.0);
        assert_eq!(limits.check(&store, &owner).unwrap(), None);

        // The agent-wide limit counts everyone, owners included.
        let limits = LimitsConfig {
            daily_usd: Some(5.0),
            ..limits
        };
        let hit = limits.check(&store, &owner).unwrap().unwrap();
        assert_eq!(hit.kind, Kind::Daily);
        assert!(hit.message(true).contains("limits.daily_usd"));
        assert!(!hit.message(false).contains("limits."));
        let limits = LimitsConfig {
            daily_usd: None,
            monthly_usd: Some(5.5),
            ..limits
        };
        assert_eq!(
            limits.check(&store, &owner).unwrap().unwrap().kind,
            Kind::Monthly
        );
        // 0 means no limit.
        let limits = LimitsConfig {
            monthly_usd: Some(0.0),
            guest_daily_usd: Some(0.0),
            ..limits
        };
        assert_eq!(limits.check(&store, &guest("qq:a")).unwrap(), None);
    }

    #[test]
    fn old_spending_does_not_count_today() {
        let store = Store::open_in_memory().unwrap();
        spend(&store, "qq:a", 1.0);
        store
            .runtime()
            .execute("UPDATE usage SET created_at = created_at - 3 * 86400", [])
            .unwrap();
        assert_eq!(store.spent_since(day_start(), Some("qq:a")).unwrap(), 0.0);
        assert!(month_start() <= day_start());
    }

    #[test]
    fn money_is_short() {
        assert_eq!(money(0.5), "0.5");
        assert_eq!(money(0.0123), "0.0123");
        assert_eq!(money(0.0), "0");
        assert_eq!(money(12.345), "12.35");
        assert_eq!(money(2.0), "2");
    }
}
