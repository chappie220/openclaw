//! What model calls cost: provider-reported tokens and USD per call, and how
//! far our token estimate is from the provider's count.

use anyhow::Result;
use rusqlite::params;

use crate::i18n::{Lang, Tr, tr};
use crate::llm::Usage;
use crate::store::{Store, now};

/// Usage of one session over a period.
#[derive(Debug, Clone, PartialEq)]
pub struct SessionUsage {
    pub session: String,
    pub calls: u64,
    pub usage: Usage,
}

impl Store {
    /// Records one call; `kind` is `turn` or `summary`.
    pub fn record_usage(
        &self,
        session: &str,
        kind: &str,
        model: Option<&str>,
        usage: &Usage,
    ) -> Result<()> {
        self.runtime().execute(
            "INSERT INTO usage(session, kind, model, prompt_tokens, cached_tokens,
               cache_write_tokens, completion_tokens, cost, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
            params![
                session,
                kind,
                model,
                usage.prompt_tokens as i64,
                usage.cached_tokens as i64,
                usage.cache_write_tokens as i64,
                usage.completion_tokens as i64,
                usage.cost,
                now()
            ],
        )?;
        Ok(())
    }

    /// Totals per session since `since` (unix seconds), costliest first.
    pub fn usage_since(&self, since: i64) -> Result<Vec<SessionUsage>> {
        let conn = self.runtime();
        let mut stmt = conn.prepare(
            "SELECT session, COUNT(*), SUM(prompt_tokens), SUM(cached_tokens),
               SUM(cache_write_tokens), SUM(completion_tokens), SUM(cost)
             FROM usage WHERE created_at >= ?1
             GROUP BY session ORDER BY SUM(cost) DESC, SUM(prompt_tokens) DESC",
        )?;
        let rows = stmt.query_map([since], |row| {
            let int = |i: usize| row.get::<_, i64>(i).map(|v| v.max(0) as u64);
            Ok(SessionUsage {
                session: row.get(0)?,
                calls: int(1)?,
                usage: Usage {
                    prompt_tokens: int(2)?,
                    cached_tokens: int(3)?,
                    cache_write_tokens: int(4)?,
                    completion_tokens: int(5)?,
                    cost: row.get(6)?,
                },
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// Folds one call's provider count over our estimate into the session's
    /// ratio, which scales its context budget.
    pub fn update_token_ratio(&self, session_id: i64, estimated: usize, actual: u64) -> Result<()> {
        if estimated == 0 || actual == 0 {
            return Ok(());
        }
        let sample = (actual as f64 / estimated as f64).clamp(MIN_RATIO, MAX_RATIO);
        self.chats().execute(
            "UPDATE sessions SET token_ratio =
               CASE WHEN token_ratio IS NULL THEN ?2 ELSE 0.7 * token_ratio + 0.3 * ?2 END
             WHERE id = ?1",
            params![session_id, sample],
        )?;
        Ok(())
    }
}

/// Bounds on the estimate correction, so one odd report cannot shrink or
/// blow up the budget.
pub const MIN_RATIO: f64 = 0.5;
pub const MAX_RATIO: f64 = 4.0;

fn tokens(n: u64) -> String {
    match n {
        0..1_000 => n.to_string(),
        1_000..1_000_000 => format!("{:.1}k", n as f64 / 1e3),
        _ => format!("{:.2}M", n as f64 / 1e6),
    }
}

const HEADERS: [Tr; 6] = [
    tr("session", "会话"),
    tr("calls", "调用"),
    tr("input", "输入"),
    tr("cached", "缓存"),
    tr("output", "输出"),
    tr("cost", "费用"),
];
const TOTAL: Tr = tr("total", "合计");
const NAME_WIDTH: usize = 28;
const WIDTHS: [usize; 5] = [6, 9, 7, 9, 10];

/// Terminal columns of `text`: CJK and other wide characters take two.
fn width(text: &str) -> usize {
    text.chars().map(|c| if c.is_ascii() { 1 } else { 2 }).sum()
}

fn pad_right(text: &str, to: usize) -> String {
    format!("{text}{}", " ".repeat(to.saturating_sub(width(text))))
}

fn pad_left(text: &str, to: usize) -> String {
    format!("{}{text}", " ".repeat(to.saturating_sub(width(text))))
}

fn line(name: &str, cells: [String; 5]) -> String {
    let name: String = name
        .chars()
        .scan(0, |used, c| {
            *used += width(&c.to_string());
            (*used <= NAME_WIDTH).then_some(c)
        })
        .collect();
    let mut out = pad_right(&name, NAME_WIDTH);
    for (cell, to) in cells.iter().zip(WIDTHS) {
        out.push(' ');
        out.push_str(&pad_left(cell, to));
    }
    out.push('\n');
    out
}

/// A plain-text table of `rows` with a total line, in `lang`.
pub fn report(rows: &[SessionUsage], lang: Lang) -> String {
    let [session, rest @ ..] = HEADERS.map(|h| h.get(lang).to_owned());
    let mut out = line(&session, rest);
    let mut total = SessionUsage {
        session: TOTAL.get(lang).into(),
        calls: 0,
        usage: Usage::default(),
    };
    for row in rows {
        total.calls += row.calls;
        total.usage += row.usage;
    }
    for row in rows.iter().chain(std::iter::once(&total)) {
        let u = &row.usage;
        let cached = if u.prompt_tokens == 0 {
            "-".into()
        } else {
            format!(
                "{:.0}%",
                100.0 * u.cached_tokens as f64 / u.prompt_tokens as f64
            )
        };
        out.push_str(&line(
            &row.session,
            [
                row.calls.to_string(),
                tokens(u.prompt_tokens),
                cached,
                tokens(u.completion_tokens),
                format!("${:.4}", u.cost),
            ],
        ));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn totals_usage_per_session() {
        let store = Store::open_in_memory().unwrap();
        let call = |prompt, cached, cost| Usage {
            prompt_tokens: prompt,
            cached_tokens: cached,
            completion_tokens: 10,
            cost,
            ..Usage::default()
        };
        store
            .record_usage("a", "turn", Some("x/y"), &call(1000, 800, 0.01))
            .unwrap();
        store
            .record_usage("a", "summary", None, &call(500, 0, 0.002))
            .unwrap();
        store
            .record_usage("b", "turn", None, &call(100, 0, 0.05))
            .unwrap();
        let rows = store.usage_since(0).unwrap();
        assert_eq!(rows[0].session, "b");
        assert_eq!(rows[1].calls, 2);
        assert_eq!(rows[1].usage.prompt_tokens, 1500);
        assert_eq!(rows[1].usage.cached_tokens, 800);
        assert!(store.usage_since(now() + 10).unwrap().is_empty());
        let table = report(&rows, Lang::En);
        assert!(table.contains("total"));
        assert!(table.contains("1.6k"), "{table}");
        assert!(table.contains("$0.0620"), "{table}");
    }

    #[test]
    fn chinese_headers_stay_aligned() {
        let rows = vec![SessionUsage {
            session: "qq:群聊".into(),
            calls: 3,
            usage: Usage {
                prompt_tokens: 2000,
                cost: 0.5,
                ..Usage::default()
            },
        }];
        let table = report(&rows, Lang::Zh);
        assert!(table.starts_with("会话"), "{table}");
        assert!(table.contains("合计"), "{table}");
        let widths: Vec<usize> = table.lines().map(width).collect();
        assert!(widths.iter().all(|w| *w == widths[0]), "{table}");
        crate::i18n::assert_complete(&HEADERS);
    }

    #[test]
    fn ratio_is_smoothed_and_bounded() {
        let store = Store::open_in_memory().unwrap();
        let id = store.session_id("s").unwrap();
        assert_eq!(store.context(id).unwrap().token_ratio, None);
        store.update_token_ratio(id, 1000, 2000).unwrap();
        assert_eq!(store.context(id).unwrap().token_ratio, Some(2.0));
        store.update_token_ratio(id, 1000, 1000).unwrap();
        let ratio = store.context(id).unwrap().token_ratio.unwrap();
        assert!((ratio - 1.7).abs() < 1e-9, "{ratio}");
        store.update_token_ratio(id, 1, 1_000_000).unwrap();
        let ratio = store.context(id).unwrap().token_ratio.unwrap();
        assert!(ratio <= MAX_RATIO);
    }
}
