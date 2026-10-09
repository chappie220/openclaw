//! Scheduled prompts: standard 5-field cron expressions in local time.

use std::str::FromStr;

use anyhow::{Context, Result, bail};
use chrono::{Local, TimeZone};
use croner::Cron;
use rusqlite::params;

use crate::store::{Store, now};

#[derive(Debug, Clone, PartialEq)]
pub struct Job {
    pub id: i64,
    pub name: String,
    pub schedule: String,
    pub session: String,
    pub prompt: String,
    pub next_run: i64,
    pub last_run: Option<i64>,
    pub last_status: Option<String>,
    /// The actor that scheduled it; `None` for jobs from before this was recorded.
    pub created_by: Option<String>,
}

/// The first run strictly after `after` (unix seconds), in local time.
pub fn next_run(schedule: &str, after: i64) -> Result<i64> {
    let cron = Cron::from_str(schedule)
        .with_context(|| format!("invalid cron expression {schedule:?}"))?;
    let after = Local
        .timestamp_opt(after, 0)
        .single()
        .context("invalid timestamp")?;
    let next = cron
        .find_next_occurrence(&after, false)
        .with_context(|| format!("{schedule:?} never runs"))?;
    Ok(next.timestamp())
}

fn job(row: &rusqlite::Row<'_>) -> rusqlite::Result<Job> {
    Ok(Job {
        id: row.get(0)?,
        name: row.get(1)?,
        schedule: row.get(2)?,
        session: row.get(3)?,
        prompt: row.get(4)?,
        next_run: row.get(5)?,
        last_run: row.get(6)?,
        last_status: row.get(7)?,
        created_by: row.get(8)?,
    })
}

const COLUMNS: &str =
    "id, name, schedule, session, prompt, next_run, last_run, last_status, created_by";

impl Store {
    pub fn job_add(
        &self,
        name: &str,
        schedule: &str,
        session: &str,
        prompt: &str,
        created_by: &str,
    ) -> Result<Job> {
        if name.trim().is_empty() || prompt.trim().is_empty() || session.trim().is_empty() {
            bail!("job name, session and prompt must not be empty");
        }
        let next = next_run(schedule, now())?;
        let conn = self.runtime();
        let inserted = conn.execute(
            "INSERT INTO jobs(name, schedule, session, prompt, next_run, created_by)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6) ON CONFLICT(name) DO NOTHING",
            params![name, schedule, session, prompt, next, created_by],
        )?;
        if inserted == 0 {
            bail!("a job named {name:?} already exists; remove it first");
        }
        Ok(conn.query_row(
            &format!("SELECT {COLUMNS} FROM jobs WHERE name = ?1"),
            [name],
            job,
        )?)
    }

    pub fn job_list(&self) -> Result<Vec<Job>> {
        let conn = self.runtime();
        let mut stmt = conn.prepare(&format!("SELECT {COLUMNS} FROM jobs ORDER BY next_run"))?;
        Ok(stmt.query_map([], job)?.collect::<rusqlite::Result<_>>()?)
    }

    #[cfg(test)]
    pub fn job_get(&self, name: &str) -> Result<Option<Job>> {
        use rusqlite::OptionalExtension;
        let conn = self.runtime();
        Ok(conn
            .query_row(
                &format!("SELECT {COLUMNS} FROM jobs WHERE name = ?1"),
                [name],
                job,
            )
            .optional()?)
    }

    pub fn job_remove(&self, name: &str) -> Result<bool> {
        self.job_remove_in(name, None)
    }

    /// Removes job `name` if it is within `scope` (`None`: any job).
    pub fn job_remove_in(&self, name: &str, scope: Option<&str>) -> Result<bool> {
        Ok(self.runtime().execute(
            "DELETE FROM jobs WHERE name = ?1 AND (?2 IS NULL OR created_by = ?2)",
            params![name, scope],
        )? > 0)
    }

    /// Claims every due job and advances it past `at` before it runs, so a
    /// crash or long outage triggers at most one catch-up run per job.
    pub fn job_claim_due(&self, at: i64) -> Result<Vec<Job>> {
        let mut conn = self.runtime();
        let tx = conn.transaction()?;
        let due: Vec<Job> = {
            let mut stmt = tx.prepare(&format!(
                "SELECT {COLUMNS} FROM jobs WHERE next_run <= ?1 ORDER BY next_run"
            ))?;
            stmt.query_map([at], job)?
                .collect::<rusqlite::Result<_>>()?
        };
        for job in &due {
            // A schedule that became unparseable is parked far in the future, not retried every tick.
            let next = next_run(&job.schedule, at).unwrap_or(i64::MAX);
            tx.execute(
                "UPDATE jobs SET next_run = ?2, last_run = ?3 WHERE id = ?1",
                params![job.id, next, at],
            )?;
        }
        tx.commit()?;
        Ok(due)
    }

    pub fn job_record(&self, id: i64, status: &str) -> Result<()> {
        let status: String = status.chars().take(500).collect();
        self.runtime().execute(
            "UPDATE jobs SET last_status = ?2 WHERE id = ?1",
            params![id, status],
        )?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn computes_the_next_minute_boundary() {
        let base = 1_700_000_000; // 2023-11-14T22:13:20Z
        let next = next_run("* * * * *", base).unwrap();
        assert_eq!(next, 1_700_000_040);
        assert!(next_run("*/15 * * * *", base).unwrap() % 900 == 0);
        assert!(next_run("not a cron", base).is_err());
    }

    #[test]
    fn claims_a_missed_job_once_and_reschedules_it() {
        let store = Store::open_in_memory().unwrap();
        let job = store
            .job_add("tea", "0 15 * * *", "main", "提醒我喝茶", "cli")
            .unwrap();
        assert!(
            store
                .job_add("tea", "0 15 * * *", "main", "x", "cli")
                .is_err()
        );
        assert!(store.job_claim_due(job.next_run - 1).unwrap().is_empty());
        // Three days late: one run, then the next run is after the claim time.
        let late = job.next_run + 3 * 86_400;
        let claimed = store.job_claim_due(late).unwrap();
        assert_eq!(claimed.len(), 1);
        assert!(store.job_claim_due(late).unwrap().is_empty());
        let after = store.job_get("tea").unwrap().unwrap();
        assert!(after.next_run > late && after.last_run == Some(late));
        store.job_record(after.id, "ok").unwrap();
        assert_eq!(
            store
                .job_get("tea")
                .unwrap()
                .unwrap()
                .last_status
                .as_deref(),
            Some("ok")
        );
        assert!(!store.job_remove_in("tea", Some("qq:X")).unwrap());
        assert!(store.job_remove("tea").unwrap());
    }
}
