//! Built-in tools: shell and workspace files, gated by per-category permissions.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use serde::Deserialize;
use serde::de::DeserializeOwned;
use serde_json::json;
use tokio::io::{AsyncRead, AsyncReadExt};

use crate::access::{self, Actor, Capability};
use crate::agent::Tools;
use crate::browser::{Browser, BrowserArgs};
use crate::config::{Permission, ToolsConfig};
use crate::fetch::Fetcher;
use crate::identity::Identity;
use crate::llm::{ToolCall, ToolSpec};
use crate::review::{Reviewer, Verdict};
use crate::search::Searcher;
use crate::store::Store;

/// Asks a person whether a gated action may run. Front ends supply their own.
#[async_trait]
pub trait Approver: Send + Sync {
    async fn approve(&self, tool: &str, summary: &str) -> bool;
}

tokio::task_local! {
    /// The person who can answer for the turn running on this task.
    static TURN_APPROVER: Arc<dyn Approver>;
}

/// Runs a turn whose `ask` permissions are answered by `approver`.
pub async fn with_approver<F: std::future::Future>(
    approver: Arc<dyn Approver>,
    turn: F,
) -> F::Output {
    TURN_APPROVER.scope(approver, turn).await
}

/// Prompts on the controlling terminal, so piped stdin cannot answer for the user.
pub struct TerminalApprover;

#[async_trait]
impl Approver for TerminalApprover {
    async fn approve(&self, tool: &str, summary: &str) -> bool {
        let prompt = format!(
            "\n{}\n  {summary}\n[y/N] ",
            crate::cli_text::ALLOW_TOOL.with(&[tool])
        );
        tokio::task::spawn_blocking(move || {
            use std::io::{BufRead, Write};
            let Ok(tty) = std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .open("/dev/tty")
            else {
                return false;
            };
            let mut writer = &tty;
            if writer
                .write_all(prompt.as_bytes())
                .and_then(|_| writer.flush())
                .is_err()
            {
                return false;
            }
            let mut answer = String::new();
            std::io::BufReader::new(&tty).read_line(&mut answer).is_ok()
                && matches!(answer.trim(), "y" | "Y" | "yes" | "是")
        })
        .await
        .unwrap_or(false)
    }
}

pub struct BuiltinTools {
    workspace: PathBuf,
    config: ToolsConfig,
    store: Store,
    search: Option<Searcher>,
    browser: Option<Browser>,
    fetch: Option<Fetcher>,
    review: Option<Reviewer>,
}

#[derive(Deserialize)]
struct ShellArgs {
    command: String,
}

#[derive(Deserialize)]
struct ReadArgs {
    path: String,
    #[serde(default)]
    offset: Option<usize>,
    #[serde(default)]
    limit: Option<usize>,
}

#[derive(Deserialize)]
struct WriteArgs {
    path: String,
    content: String,
}

#[derive(Deserialize)]
struct EditArgs {
    path: String,
    old_text: String,
    new_text: String,
}

#[derive(Deserialize)]
struct MemorySaveArgs {
    content: String,
}

#[derive(Deserialize)]
struct MemorySearchArgs {
    query: String,
    #[serde(default)]
    limit: Option<usize>,
}

#[derive(Deserialize)]
struct CronAddArgs {
    name: String,
    schedule: String,
    prompt: String,
}

#[derive(Deserialize)]
struct CronRemoveArgs {
    name: String,
}

#[derive(Deserialize)]
struct IdentitySetArgs {
    name: String,
    creature: String,
    vibe: String,
    #[serde(default)]
    emoji: Option<String>,
    soul: String,
}

#[derive(Deserialize)]
struct WebSearchArgs {
    query: String,
}

#[derive(Deserialize)]
struct WebFetchArgs {
    url: String,
    #[serde(default)]
    offset: Option<usize>,
}

#[derive(Deserialize)]
struct MemoryDeleteArgs {
    id: i64,
}

#[derive(Deserialize)]
struct ListArgs {
    #[serde(default)]
    path: Option<String>,
}

impl BuiltinTools {
    pub fn new(workspace: PathBuf, config: ToolsConfig, store: Store) -> std::io::Result<Self> {
        std::fs::create_dir_all(&workspace)?;
        Ok(Self {
            workspace,
            config,
            store,
            search: None,
            browser: None,
            fetch: None,
            review: None,
        })
    }

    pub fn with_search(mut self, search: Option<Searcher>) -> Self {
        self.search = search;
        self
    }

    pub fn with_browser(mut self, browser: Option<Browser>) -> Self {
        self.browser = browser;
        self
    }

    pub fn with_fetch(mut self, fetch: Option<Fetcher>) -> Self {
        self.fetch = fetch;
        self
    }

    /// Reviews `shell` commands set to `ask` before anyone is asked.
    pub fn with_review(mut self, review: Option<Reviewer>) -> Self {
        self.review = review;
        self
    }

    /// Returns the auto-review note when the reviewer let the command run.
    async fn permit_shell(&self, command: &str) -> Result<Option<String>, String> {
        let review = match (&self.review, self.config.shell) {
            (Some(review), Permission::Ask) => review,
            _ => {
                return self
                    .permit(self.config.shell, "shell", command)
                    .await
                    .map(|_| None);
            }
        };
        let verdict = review.review(command, &self.workspace).await;
        let preview: String = command.chars().take(200).collect();
        match verdict {
            Verdict::Allow(note) => {
                eprintln!("auto-review allowed shell ({note}): {preview}");
                Ok(Some(format!("auto-review allowed this command ({note})")))
            }
            Verdict::Deny(note) => {
                eprintln!("auto-review declined shell ({note}): {preview}");
                Err(format!(
                    "error: auto-review declined this command as dangerous ({note}). Do not try \
                     the same outcome through a workaround or another tool; use a clearly safer \
                     command, or explain the risk and ask the user to run it themselves."
                ))
            }
            Verdict::Ask(note) => self
                .permit(
                    Permission::Ask,
                    "shell",
                    &format!("{command}\n  auto-review: {note}"),
                )
                .await
                .map(|_| None),
        }
    }

    /// Proposes an identity. Nothing is saved until a person approves this
    /// exact draft: here when the turn has someone to ask, otherwise later
    /// with `/identity approve`, which the program handles, not the model.
    async fn identity_set(&self, actor: &Actor, args: IdentitySetArgs) -> Result<String, String> {
        if self.config.identity == Permission::Deny {
            return Err("error: identity_set is disabled by configuration".into());
        }
        let session = crate::agent::current_session()
            .ok_or("error: identity_set can only be used in a conversation")?;
        let proposed = Identity {
            name: args.name,
            creature: args.creature,
            vibe: args.vibe,
            emoji: args.emoji,
            soul: args.soul,
            ..Identity::default()
        };
        let draft = self
            .store
            .identity_propose(&proposed, &actor.id, &session)
            .map_err(|e| format!("error: {e:#}"))?;
        let Ok(approver) = TURN_APPROVER.try_with(Arc::clone) else {
            return Ok(format!(
                "identity draft #{} is NOT saved yet. After your reply the user is shown the exact \
                 draft and how to approve or reject it; tell them briefly that it needs their \
                 approval, and do not act as the new identity until it is approved.",
                draft.id
            ));
        };
        let approved = approver.approve("identity_set", &draft.render()).await;
        let decided = self
            .store
            .identity_decide(draft.id, approved, Some(&draft.hash), &actor.id)
            .map_err(|e| format!("error: {e:#}"))?;
        if approved {
            Ok(format!(
                "identity saved: you are now {}; it applies to every conversation from your next reply",
                decided.identity.name
            ))
        } else {
            Err(format!(
                "error: the user rejected identity draft #{}; nothing was saved. Ask what to \
                 change, then propose a revised draft.",
                draft.id
            ))
        }
    }

    async fn web_search(&self, args: WebSearchArgs) -> Result<String, String> {
        let search = self.search.as_ref().ok_or("error: web search is off")?;
        let found = search
            .search(&args.query)
            .await
            .map_err(|e| format!("error: {e:#}"))?;
        Ok(truncate(found.as_bytes(), self.config.max_output_bytes))
    }

    async fn browser(&self, args: BrowserArgs) -> Result<String, String> {
        let Some(browser) = &self.browser else {
            return Err("error: no browser is available on this host".into());
        };
        let session = crate::agent::current_session().unwrap_or_else(|| "main".into());
        browser
            .run(&session, args)
            .await
            .map_err(|err| format!("error: {err:#}"))
    }

    async fn web_fetch(&self, actor: &Actor, args: WebFetchArgs) -> Result<String, String> {
        let fetch = self.fetch.as_ref().ok_or("error: web_fetch is off")?;
        fetch
            .fetch(&args.url, args.offset.unwrap_or(0), actor.owner)
            .await
            .map_err(|err| format!("error: {err:#}"))
    }

    fn memory_save(&self, actor: &Actor, args: MemorySaveArgs) -> Result<String, String> {
        // The owner's memories stay unattributed, like those saved from the terminal.
        let id = self
            .store
            .memory_save_by(&args.content, actor.scope())
            .map_err(|e| format!("error: {e:#}"))?;
        Ok(format!("saved memory #{id}"))
    }

    fn memory_search(&self, actor: &Actor, args: MemorySearchArgs) -> Result<String, String> {
        let limit = args.limit.unwrap_or(10).clamp(1, 50);
        let hits = self
            .store
            .memory_search_in(&args.query, limit, actor.scope())
            .map_err(|e| format!("error: {e:#}"))?;
        if hits.is_empty() {
            return Ok("no matching memories".into());
        }
        Ok(hits
            .iter()
            .map(|m| format!("#{} {}", m.id, m.content))
            .collect::<Vec<_>>()
            .join("\n"))
    }

    fn cron_add(&self, actor: &Actor, args: CronAddArgs) -> Result<String, String> {
        // Always this conversation: nobody schedules into another person's session.
        let session = crate::agent::current_session()
            .ok_or("error: cron_add can only schedule from a conversation")?;
        let job = self
            .store
            .job_add(
                &args.name,
                &args.schedule,
                &session,
                &args.prompt,
                &actor.id,
            )
            .map_err(|e| format!("error: {e:#}"))?;
        Ok(format!(
            "scheduled {:?}; next run {}",
            job.name,
            format_time(job.next_run)
        ))
    }

    fn cron_list(&self, actor: &Actor) -> Result<String, String> {
        let mut jobs = self.store.job_list().map_err(|e| format!("error: {e:#}"))?;
        jobs.retain(|j| actor.owns(j.created_by.as_deref()));
        if jobs.is_empty() {
            return Ok("no scheduled jobs".into());
        }
        Ok(jobs
            .iter()
            .map(|j| {
                format!(
                    "{} [{}] session={} next={} prompt={:?}",
                    j.name,
                    j.schedule,
                    j.session,
                    format_time(j.next_run),
                    j.prompt
                )
            })
            .collect::<Vec<_>>()
            .join("\n"))
    }

    fn cron_remove(&self, actor: &Actor, args: CronRemoveArgs) -> Result<String, String> {
        match self.store.job_remove_in(&args.name, actor.scope()) {
            Ok(true) => Ok(format!("removed job {:?}", args.name)),
            Ok(false) => Err(format!("error: no job named {:?}", args.name)),
            Err(e) => Err(format!("error: {e:#}")),
        }
    }

    fn memory_delete(&self, actor: &Actor, args: MemoryDeleteArgs) -> Result<String, String> {
        match self.store.memory_delete_in(args.id, actor.scope()) {
            Ok(true) => Ok(format!("deleted memory #{}", args.id)),
            Ok(false) => Err(format!("error: no memory #{}", args.id)),
            Err(e) => Err(format!("error: {e:#}")),
        }
    }

    fn resolve(&self, path: &str) -> PathBuf {
        let path = Path::new(path);
        if path.is_absolute() {
            path.to_owned()
        } else {
            self.workspace.join(path)
        }
    }

    async fn permit(
        &self,
        permission: Permission,
        tool: &str,
        summary: &str,
    ) -> Result<(), String> {
        match permission {
            Permission::Allow => Ok(()),
            Permission::Deny => Err(format!("error: {tool} is disabled by configuration")),
            Permission::Ask => {
                let Ok(approver) = TURN_APPROVER.try_with(Arc::clone) else {
                    return Err(format!(
                        "error: {tool} needs approval, but nobody can answer for this conversation"
                    ));
                };
                if approver.approve(tool, summary).await {
                    Ok(())
                } else {
                    Err(format!("error: the user declined {tool}"))
                }
            }
        }
    }

    async fn shell(&self, args: ShellArgs) -> Result<String, String> {
        let reviewed = self.permit_shell(&args.command).await?;
        let mut child = tokio::process::Command::new("sh")
            .arg("-c")
            .arg(&args.command)
            .current_dir(&self.workspace)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            // Own process group so a timeout can stop every descendant.
            .process_group(0)
            .kill_on_drop(true)
            .spawn()
            .map_err(|err| format!("error: cannot start shell: {err}"))?;
        // Kills the whole group if this future is dropped or times out mid-run.
        let group = child.id().map(GroupKill);
        let stdout = child.stdout.take().expect("piped stdout");
        let stderr = child.stderr.take().expect("piped stderr");
        let cap = self.config.max_output_bytes;
        let run = async {
            // Both pipes drain concurrently into fixed-size buffers, so neither a
            // blocked writer nor endless output can grow memory past the cap.
            let (out, err, status) = tokio::join!(
                Capture::drain(stdout, cap),
                Capture::drain(stderr, cap),
                child.wait()
            );
            Ok::<_, String>((
                status.map_err(|e| e.to_string())?,
                out.map_err(|e| e.to_string())?,
                err.map_err(|e| e.to_string())?,
            ))
        };
        let timeout = Duration::from_secs(self.config.shell_timeout_secs);
        let (status, out, err) = match tokio::time::timeout(timeout, run).await {
            Ok(result) => result.map_err(|e| format!("error: {e}"))?,
            Err(_) => {
                drop(group);
                return Err(format!(
                    "error: command timed out after {}s and was killed",
                    timeout.as_secs()
                ));
            }
        };
        // Finished normally: leave deliberately detached descendants alone.
        std::mem::forget(group);
        let code = status
            .code()
            .map_or_else(|| "killed by signal".into(), |c| c.to_string());
        let mut text = reviewed.map(|note| format!("{note}\n")).unwrap_or_default();
        text.push_str(&format!("exit code: {code}\n"));
        if out.total > 0 {
            text.push_str(&format!("stdout:\n{}\n", out.render()));
        }
        if err.total > 0 {
            text.push_str(&format!("stderr:\n{}\n", err.render()));
        }
        Ok(text)
    }

    async fn read_file(&self, args: ReadArgs) -> Result<String, String> {
        let path = self.resolve(&args.path);
        let bytes = tokio::fs::read(&path)
            .await
            .map_err(|e| format!("error: {}: {e}", path.display()))?;
        let text = String::from_utf8_lossy(&bytes);
        let offset = args.offset.unwrap_or(0);
        let limit = args.limit.unwrap_or(2000);
        let total = text.lines().count();
        let selected: Vec<String> = text
            .lines()
            .enumerate()
            .skip(offset)
            .take(limit)
            .map(|(i, line)| format!("{:>6}\t{line}", i + 1))
            .collect();
        let mut out = truncate(selected.join("\n").as_bytes(), self.config.max_output_bytes);
        if offset + selected.len() < total {
            out.push_str(&format!(
                "\n[{} more lines; continue with offset {}]",
                total - offset - selected.len(),
                offset + selected.len()
            ));
        }
        Ok(out)
    }

    async fn write_file(&self, args: WriteArgs) -> Result<String, String> {
        let path = self.resolve(&args.path);
        let summary = format!("write {} ({} bytes)", path.display(), args.content.len());
        self.permit(self.config.write, "write_file", &summary)
            .await?;
        if let Some(parent) = path.parent() {
            tokio::fs::create_dir_all(parent)
                .await
                .map_err(|e| format!("error: {e}"))?;
        }
        tokio::fs::write(&path, args.content.as_bytes())
            .await
            .map_err(|e| format!("error: {}: {e}", path.display()))?;
        Ok(format!(
            "wrote {} bytes to {}",
            args.content.len(),
            path.display()
        ))
    }

    async fn edit_file(&self, args: EditArgs) -> Result<String, String> {
        let path = self.resolve(&args.path);
        let text = tokio::fs::read_to_string(&path)
            .await
            .map_err(|e| format!("error: {}: {e}", path.display()))?;
        match text.matches(&args.old_text).count() {
            0 => {
                return Err(
                    "error: old_text was not found; read the file and copy the text exactly".into(),
                );
            }
            1 => {}
            n => {
                return Err(format!(
                    "error: old_text matches {n} places; include more surrounding text"
                ));
            }
        }
        let summary = format!("edit {}", path.display());
        self.permit(self.config.write, "edit_file", &summary)
            .await?;
        tokio::fs::write(&path, text.replacen(&args.old_text, &args.new_text, 1))
            .await
            .map_err(|e| format!("error: {}: {e}", path.display()))?;
        Ok(format!("edited {}", path.display()))
    }

    async fn list_dir(&self, args: ListArgs) -> Result<String, String> {
        let path = self.resolve(args.path.as_deref().unwrap_or("."));
        let mut entries = tokio::fs::read_dir(&path)
            .await
            .map_err(|e| format!("error: {}: {e}", path.display()))?;
        let mut names = Vec::new();
        while let Some(entry) = entries
            .next_entry()
            .await
            .map_err(|e| format!("error: {e}"))?
        {
            let suffix = if entry.file_type().await.is_ok_and(|t| t.is_dir()) {
                "/"
            } else {
                ""
            };
            names.push(format!("{}{suffix}", entry.file_name().to_string_lossy()));
        }
        names.sort();
        if names.is_empty() {
            return Ok(format!("{} is empty", path.display()));
        }
        Ok(truncate(
            names.join("\n").as_bytes(),
            self.config.max_output_bytes,
        ))
    }
}

fn format_time(unix: i64) -> String {
    use chrono::TimeZone;
    chrono::Local.timestamp_opt(unix, 0).single().map_or_else(
        || "never".into(),
        |t| t.format("%Y-%m-%d %H:%M %Z").to_string(),
    )
}

fn parse<T: DeserializeOwned>(call: &ToolCall) -> Result<T, String> {
    serde_json::from_str(&call.function.arguments)
        .map_err(|err| format!("error: invalid arguments for {}: {err}", call.function.name))
}

/// SIGKILLs a process group when dropped.
struct GroupKill(u32);

impl Drop for GroupKill {
    fn drop(&mut self) {
        // SAFETY: signalling a process group we created; failure is harmless.
        unsafe { libc::kill(-(self.0 as i32), libc::SIGKILL) };
    }
}

/// The head and tail of a stream, at most `cap` bytes however much is read.
struct Capture {
    head: Vec<u8>,
    head_cap: usize,
    /// Ring buffer of the latest bytes past the head; `tail_start` is the oldest.
    tail: Vec<u8>,
    tail_cap: usize,
    tail_start: usize,
    total: u64,
}

impl Capture {
    fn new(cap: usize) -> Self {
        let head_cap = cap / 2;
        Self {
            head: Vec::new(),
            head_cap,
            tail: Vec::new(),
            tail_cap: cap - head_cap,
            tail_start: 0,
            total: 0,
        }
    }

    async fn drain(mut reader: impl AsyncRead + Unpin, cap: usize) -> std::io::Result<Self> {
        let mut capture = Self::new(cap);
        let mut buf = [0u8; 8192];
        loop {
            match reader.read(&mut buf).await? {
                0 => return Ok(capture),
                n => capture.push(&buf[..n]),
            }
        }
    }

    fn push(&mut self, bytes: &[u8]) {
        self.total += bytes.len() as u64;
        let take = (self.head_cap - self.head.len()).min(bytes.len());
        self.head.extend_from_slice(&bytes[..take]);
        let rest = &bytes[take..];
        if rest.len() >= self.tail_cap {
            self.tail.clear();
            self.tail
                .extend_from_slice(&rest[rest.len() - self.tail_cap..]);
            self.tail_start = 0;
            return;
        }
        for &b in rest {
            if self.tail.len() < self.tail_cap {
                self.tail.push(b);
            } else {
                self.tail[self.tail_start] = b;
                self.tail_start = (self.tail_start + 1) % self.tail_cap;
            }
        }
    }

    fn render(&self) -> String {
        let mut tail = self.tail[self.tail_start..].to_vec();
        tail.extend_from_slice(&self.tail[..self.tail_start]);
        let omitted = self.total - (self.head.len() + tail.len()) as u64;
        if omitted == 0 {
            let mut all = self.head.clone();
            all.extend_from_slice(&tail);
            return String::from_utf8_lossy(&all).into_owned();
        }
        format!(
            "{}\n[... {omitted} bytes omitted ...]\n{}",
            String::from_utf8_lossy(&self.head),
            String::from_utf8_lossy(&tail)
        )
    }
}

/// Keeps the head and tail of long output, where errors usually are.
fn truncate(bytes: &[u8], cap: usize) -> String {
    if bytes.len() <= cap {
        return String::from_utf8_lossy(bytes).into_owned();
    }
    let half = cap / 2;
    format!(
        "{}\n[... {} bytes omitted ...]\n{}",
        String::from_utf8_lossy(&bytes[..half]),
        bytes.len() - 2 * half,
        String::from_utf8_lossy(&bytes[bytes.len() - half..])
    )
}

#[async_trait]
impl Tools for BuiltinTools {
    fn specs(&self) -> Vec<ToolSpec> {
        let mut specs = self.all_specs();
        // Offer only what this turn's actor may use; `call` enforces it regardless.
        let actor = access::current();
        specs.retain(|spec| {
            Capability::for_tool(&spec.function.name)
                .is_none_or(|cap| actor.as_ref().is_some_and(|a| a.can(cap)))
        });
        specs
    }

    async fn call(&self, call: &ToolCall) -> String {
        let name = call.function.name.as_str();
        // Fail closed: a call outside any turn has no authority at all.
        let Some(actor) = access::current() else {
            return format!("error: {name} has no authenticated caller");
        };
        if let Some(cap) = Capability::for_tool(name)
            && !actor.can(cap)
        {
            eprintln!("refused {name} for {} (no {cap:?} permission)", actor.id);
            return format!(
                "error: {name} is not permitted for this sender. Do not try to reach the same \
                 result another way; tell them the owner has to grant it."
            );
        }
        self.dispatch(&actor, call).await.unwrap_or_else(|err| err)
    }
}

impl BuiltinTools {
    fn all_specs(&self) -> Vec<ToolSpec> {
        let workspace = self.workspace.display();
        let mut specs = vec![
            ToolSpec::function(
                "read_file",
                &format!(
                    "Read a text file with line numbers. Relative paths resolve against {workspace}."
                ),
                json!({"type": "object", "properties": {
                    "path": {"type": "string"},
                    "offset": {"type": "integer", "description": "First line to return (0-based)"},
                    "limit": {"type": "integer", "description": "Maximum lines (default 2000)"}
                }, "required": ["path"]}),
            ),
            ToolSpec::function(
                "list_dir",
                "List a directory; directories end with '/'.",
                json!({"type": "object", "properties": {"path": {"type": "string"}}}),
            ),
        ];
        specs.push(ToolSpec::function(
            "memory_save",
            "Save a durable fact for future conversations (preferences, names, decisions). One fact per call, under 2000 characters.",
            json!({"type": "object", "properties": {"content": {"type": "string"}}, "required": ["content"]}),
        ));
        specs.push(ToolSpec::function(
            "memory_search",
            "Search saved memories. Space-separated terms match any; terms of 3+ characters are matched as substrings, including Chinese.",
            json!({"type": "object", "properties": {
                "query": {"type": "string"}, "limit": {"type": "integer", "description": "Default 10, max 50"}
            }, "required": ["query"]}),
        ));
        specs.push(ToolSpec::function(
            "memory_delete",
            "Delete a saved memory by its #id when it is wrong or outdated.",
            json!({"type": "object", "properties": {"id": {"type": "integer"}}, "required": ["id"]}),
        ));
        specs.push(ToolSpec::function(
            "cron_add",
            "Schedule a prompt to run in this conversation on a 5-field cron schedule (minute hour day month weekday, server local time). Use it for reminders and recurring checks; the result shows the next run time.",
            json!({"type": "object", "properties": {
                "name": {"type": "string", "description": "Unique job name"},
                "schedule": {"type": "string", "description": "e.g. '0 9 * * 1-5' for 09:00 on weekdays"},
                "prompt": {"type": "string", "description": "What to do when the job runs"}
            }, "required": ["name", "schedule", "prompt"]}),
        ));
        specs.push(ToolSpec::function(
            "cron_list",
            "List scheduled jobs with their next run time.",
            json!({"type": "object", "properties": {}}),
        ));
        specs.push(ToolSpec::function(
            "cron_remove",
            "Remove a scheduled job by name.",
            json!({"type": "object", "properties": {"name": {"type": "string"}}, "required": ["name"]}),
        ));
        if self.config.identity != Permission::Deny {
            specs.push(ToolSpec::function(
            "identity_set",
            "Propose your identity (IDENTITY.md fields and SOUL.md) for every conversation. It is saved only after the user approves this exact draft, which the program shows them; never propose an identity they did not ask for or describe. Write it as who you are: never name a source work, author or actor, summarize plot, cite pages, or say you are based on or playing someone.",
            json!({"type": "object", "properties": {
                "name": {"type": "string", "description": "What the user calls you"},
                "creature": {"type": "string", "description": "What you are, e.g. an AI, a robot, a familiar"},
                "vibe": {"type": "string", "description": "One line on how you come across"},
                "emoji": {"type": "string", "description": "One signature emoji"},
                "soul": {"type": "string", "description": "SOUL.md, addressed to you as 'You ...', under 4000 characters: tone, speech patterns and catchphrases, opinions, how you address the user, boundaries. Behavior, not biography."}
            }, "required": ["name", "creature", "vibe", "soul"]}),
        ));
        }
        if self.search.is_some() {
            specs.push(ToolSpec::function(
                "web_search",
                "Search the web and get the relevant facts with source URLs. Use it for current information and to research a fictional character before playing them.",
                json!({"type": "object", "properties": {"query": {"type": "string"}}, "required": ["query"]}),
            ));
        }
        if self.fetch.is_some() {
            specs.push(ToolSpec::function(
                "web_fetch",
                "Download a web page and read its text, with links as Markdown. Fast and light: use it to read \
                 pages found with web_search or links the user sends. Pages that only show content after \
                 JavaScript runs, or need logging in or clicking, need the browser instead.",
                json!({"type": "object", "properties": {
                    "url": {"type": "string", "description": "http(s) address"},
                    "offset": {"type": "integer", "description": "first text character, to read on in long pages"}
                }, "required": ["url"]}),
            ));
        }
        if let Some(browser) = &self.browser {
            specs.push(ToolSpec::function(
                "browser",
                &format!(
                    "Use a real web browser ({}) for pages that need JavaScript, clicking or forms. \
                     Each conversation has its own tab that keeps its page between calls. open, click, type \
                     and back return the page's title, URL, text and numbered elements; pass an element's \
                     number as ref to click or type into it. Take a screenshot to see the layout, \
                     charts or images. Prefer web_search for plain lookups and web_fetch for reading plain pages.",
                    browser.describe()
                ),
                json!({"type": "object", "properties": {
                    "action": {"type": "string", "enum": ["open", "read", "click", "type", "back", "screenshot"]},
                    "url": {"type": "string", "description": "open: http(s) address"},
                    "ref": {"type": "integer", "description": "click/type: element number from the last page listing"},
                    "selector": {"type": "string", "description": "click/type: CSS selector, instead of ref"},
                    "text": {"type": "string", "description": "type: text to enter (replaces the field's content)"},
                    "submit": {"type": "boolean", "description": "type: press Enter afterwards"},
                    "offset": {"type": "integer", "description": "read: first text character, to page through long pages"},
                    "full_page": {"type": "boolean", "description": "screenshot: whole page (up to 8000 px, JPEG) instead of the visible part. You see the image during this turn; it is saved in the workspace"}
                }, "required": ["action"]}),
            ));
        }
        if self.config.write != Permission::Deny {
            specs.push(ToolSpec::function(
                "write_file",
                "Create or overwrite a file with the given content.",
                json!({"type": "object", "properties": {
                    "path": {"type": "string"}, "content": {"type": "string"}
                }, "required": ["path", "content"]}),
            ));
            specs.push(ToolSpec::function(
                "edit_file",
                "Replace one exact, unique occurrence of old_text with new_text.",
                json!({"type": "object", "properties": {
                    "path": {"type": "string"}, "old_text": {"type": "string"}, "new_text": {"type": "string"}
                }, "required": ["path", "old_text", "new_text"]}),
            ));
        }
        if self.config.shell != Permission::Deny {
            specs.push(ToolSpec::function(
                "shell",
                &format!(
                    "Run a command with sh -c in {workspace}. Times out after {}s. Returns exit code, stdout and stderr.",
                    self.config.shell_timeout_secs
                ),
                json!({"type": "object", "properties": {"command": {"type": "string"}}, "required": ["command"]}),
            ));
        }
        specs
    }

    async fn dispatch(&self, actor: &Actor, call: &ToolCall) -> Result<String, String> {
        match call.function.name.as_str() {
            "shell" => match parse(call) {
                Ok(args) => self.shell(args).await,
                Err(e) => Err(e),
            },
            "read_file" => match parse(call) {
                Ok(args) => self.read_file(args).await,
                Err(e) => Err(e),
            },
            "write_file" => match parse(call) {
                Ok(args) => self.write_file(args).await,
                Err(e) => Err(e),
            },
            "edit_file" => match parse(call) {
                Ok(args) => self.edit_file(args).await,
                Err(e) => Err(e),
            },
            "list_dir" => match parse(call) {
                Ok(args) => self.list_dir(args).await,
                Err(e) => Err(e),
            },
            "memory_save" => parse(call).and_then(|args| self.memory_save(actor, args)),
            "memory_search" => parse(call).and_then(|args| self.memory_search(actor, args)),
            "memory_delete" => parse(call).and_then(|args| self.memory_delete(actor, args)),
            "identity_set" => match parse(call) {
                Ok(args) => self.identity_set(actor, args).await,
                Err(e) => Err(e),
            },
            "web_search" => match parse(call) {
                Ok(args) => self.web_search(args).await,
                Err(e) => Err(e),
            },
            "browser" => match parse(call) {
                Ok(args) => self.browser(args).await,
                Err(e) => Err(e),
            },
            "web_fetch" => match parse(call) {
                Ok(args) => self.web_fetch(actor, args).await,
                Err(e) => Err(e),
            },
            "cron_add" => parse(call).and_then(|args| self.cron_add(actor, args)),
            "cron_list" => self.cron_list(actor),
            "cron_remove" => parse(call).and_then(|args| self.cron_remove(actor, args)),
            other => Err(format!("error: unknown tool {other}")),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::llm::FunctionCall;

    struct Answer(bool);

    #[async_trait]
    impl Approver for Answer {
        async fn approve(&self, _tool: &str, _summary: &str) -> bool {
            self.0
        }
    }

    fn call(name: &str, args: serde_json::Value) -> ToolCall {
        ToolCall {
            id: "c".into(),
            kind: "function".into(),
            function: FunctionCall {
                name: name.into(),
                arguments: args.to_string(),
            },
        }
    }

    /// Runs `turn` as the terminal's owner.
    async fn owner<F: std::future::Future>(turn: F) -> F::Output {
        access::with_actor(Actor::owner(access::CLI), turn).await
    }

    /// Calls tools as a turn whose approvals `Answer` decides.
    struct Scoped(BuiltinTools, Arc<dyn Approver>);

    impl Scoped {
        async fn call(&self, call: &ToolCall) -> String {
            owner(with_approver(self.1.clone(), self.0.call(call))).await
        }
        fn specs(&self) -> Vec<ToolSpec> {
            self.0.specs()
        }
    }

    fn tools(dir: &Path, config: ToolsConfig, approve: bool) -> Scoped {
        let tools =
            BuiltinTools::new(dir.to_owned(), config, Store::open_in_memory().unwrap()).unwrap();
        Scoped(tools, Arc::new(Answer(approve)))
    }

    #[tokio::test]
    async fn files_round_trip_through_write_edit_and_read() {
        let dir = tempfile::tempdir().unwrap();
        let t = tools(dir.path(), ToolsConfig::default(), true);
        let out = t
            .call(&call(
                "write_file",
                json!({"path": "a/b.txt", "content": "one\ntwo\n"}),
            ))
            .await;
        assert!(out.starts_with("wrote"), "{out}");
        let out = t
            .call(&call(
                "edit_file",
                json!({"path": "a/b.txt", "old_text": "two", "new_text": "2"}),
            ))
            .await;
        assert!(out.starts_with("edited"), "{out}");
        assert_eq!(
            t.call(&call("read_file", json!({"path": "a/b.txt"}))).await,
            "     1\tone\n     2\t2"
        );
        assert_eq!(t.call(&call("list_dir", json!({}))).await, "a/");
        let out = t
            .call(&call(
                "edit_file",
                json!({"path": "a/b.txt", "old_text": "zzz", "new_text": ""}),
            ))
            .await;
        assert!(out.contains("not found"), "{out}");
    }

    #[tokio::test]
    async fn declined_and_denied_actions_do_not_run() {
        let dir = tempfile::tempdir().unwrap();
        let declined = tools(dir.path(), ToolsConfig::default(), false);
        let out = declined
            .call(&call("shell", json!({"command": "touch ran"})))
            .await;
        assert!(out.contains("declined"), "{out}");
        assert!(!dir.path().join("ran").exists());

        let config = ToolsConfig {
            shell: Permission::Deny,
            ..ToolsConfig::default()
        };
        let denied = tools(dir.path(), config, true);
        assert!(!denied.specs().iter().any(|s| s.function.name == "shell"));
        assert!(
            denied
                .call(&call("shell", json!({"command": "true"})))
                .await
                .contains("disabled")
        );
    }

    #[tokio::test]
    async fn shell_reports_output_and_kills_on_timeout() {
        let dir = tempfile::tempdir().unwrap();
        let config = ToolsConfig {
            shell: Permission::Allow,
            shell_timeout_secs: 1,
            ..ToolsConfig::default()
        };
        let t = tools(dir.path(), config, false);
        let out = t
            .call(&call(
                "shell",
                json!({"command": "pwd; echo oops >&2; exit 3"}),
            ))
            .await;
        assert!(out.starts_with("exit code: 3"), "{out}");
        assert!(
            out.contains(&dir.path().display().to_string()) && out.contains("oops"),
            "{out}"
        );
        // The background child holds the pipe open; only a group kill ends the wait.
        let out = t
            .call(&call("shell", json!({"command": "sleep 30 & sleep 30"})))
            .await;
        assert!(out.contains("timed out"), "{out}");
    }

    #[tokio::test]
    async fn ask_without_an_approver_is_declined() {
        let dir = tempfile::tempdir().unwrap();
        let t = BuiltinTools::new(
            dir.path().to_owned(),
            ToolsConfig::default(),
            Store::open_in_memory().unwrap(),
        )
        .unwrap();
        let out = owner(t.call(&call("shell", json!({"command": "touch ran"})))).await;
        assert!(out.contains("nobody can answer"), "{out}");
        assert!(!dir.path().join("ran").exists());
    }

    fn set_identity(name: &str) -> ToolCall {
        call(
            "identity_set",
            json!({"name": name, "creature": "AI", "vibe": "curious", "soul": "You ask why."}),
        )
    }

    /// Calls `c` as the owner in session `main`, with `approver` if any.
    async fn as_owner_in_main(
        t: &BuiltinTools,
        approver: Option<Arc<dyn Approver>>,
        c: &ToolCall,
    ) -> String {
        let turn = crate::agent::with_session("main".into(), async {
            match approver {
                Some(a) => with_approver(a, t.call(c)).await,
                None => t.call(c).await,
            }
        });
        owner(turn).await
    }

    #[tokio::test]
    async fn identity_set_never_saves_without_an_approval_of_that_exact_draft() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open_in_memory().unwrap();
        // Even "allow" does not let the model save an identity on its own.
        let config = ToolsConfig {
            identity: Permission::Allow,
            ..ToolsConfig::default()
        };
        let t = BuiltinTools::new(dir.path().to_owned(), config, store.clone()).unwrap();
        let owner_actor = Actor::owner(access::CLI);

        // No approver, as on QQ or email: the first identity is only a draft.
        let out = as_owner_in_main(&t, None, &set_identity("Ada")).await;
        assert!(out.contains("NOT saved"), "{out}");
        assert!(store.identity().unwrap().is_none());
        let ada = store.identity_drafts_awaiting().unwrap().remove(0);

        // A revision supersedes it, so approving the old draft is refused.
        as_owner_in_main(&t, None, &set_identity("Eve")).await;
        let eve = store.identity_drafts_awaiting().unwrap().remove(0);
        let out = crate::identity::command(
            &store,
            &owner_actor,
            &format!("/identity approve {}", ada.id),
        );
        assert!(out.unwrap().contains("replaced by a newer draft"));
        // A code from another version is refused too.
        let out = crate::identity::command(
            &store,
            &owner_actor,
            &format!("/identity approve {} {}", eve.id, ada.hash),
        );
        assert!(out.unwrap().contains("has code"));
        // A guest cannot approve.
        let guest = crate::access::AccessConfig::default().resolve("qq:X", "qq:c2c:X");
        let out =
            crate::identity::command(&store, &guest, &format!("/identity approve {}", eve.id));
        assert!(out.unwrap().contains("Only the owner"));
        assert!(store.identity().unwrap().is_none());

        let out = crate::identity::command(
            &store,
            &owner_actor,
            &format!("/identity approve {} {}", eve.id, eve.hash),
        )
        .unwrap();
        assert!(out.contains("approved"), "{out}");
        assert_eq!(store.identity().unwrap().unwrap().name, "Eve");
        // Replaying the approval does nothing more.
        let out = crate::identity::command(
            &store,
            &owner_actor,
            &format!("/identity approve {}", eve.id),
        );
        assert!(out.unwrap().contains("already committed"));

        // With someone to ask, the exact draft is put to them.
        let out = as_owner_in_main(&t, Some(Arc::new(Answer(false))), &set_identity("Zed")).await;
        assert!(out.contains("rejected"), "{out}");
        assert_eq!(store.identity().unwrap().unwrap().name, "Eve");
        let out = as_owner_in_main(&t, Some(Arc::new(Answer(true))), &set_identity("Zed")).await;
        assert!(out.starts_with("identity saved"), "{out}");
        assert_eq!(store.identity().unwrap().unwrap().name, "Zed");
        // web_search is only offered when a provider is configured.
        assert!(
            !owner(async { t.specs() })
                .await
                .iter()
                .any(|s| s.function.name == "web_search")
        );
    }

    #[tokio::test]
    async fn identity_set_can_be_turned_off() {
        let dir = tempfile::tempdir().unwrap();
        let config = ToolsConfig {
            identity: Permission::Deny,
            ..ToolsConfig::default()
        };
        let store = Store::open_in_memory().unwrap();
        let t = BuiltinTools::new(dir.path().to_owned(), config, store.clone()).unwrap();
        let out = as_owner_in_main(&t, Some(Arc::new(Answer(true))), &set_identity("Ada")).await;
        assert!(out.contains("disabled"), "{out}");
        assert!(store.identity_drafts_awaiting().unwrap().is_empty());
    }

    #[tokio::test]
    async fn memory_tools_save_search_and_delete() {
        let dir = tempfile::tempdir().unwrap();
        let t = tools(dir.path(), ToolsConfig::default(), false);
        let saved = t
            .call(&call("memory_save", json!({"content": "生日是三月五日"})))
            .await;
        assert_eq!(saved, "saved memory #1");
        let found = t
            .call(&call("memory_search", json!({"query": "三月五日"})))
            .await;
        assert_eq!(found, "#1 生日是三月五日");
        assert_eq!(
            t.call(&call("memory_delete", json!({"id": 1}))).await,
            "deleted memory #1"
        );
        assert!(
            t.call(&call("memory_delete", json!({"id": 1})))
                .await
                .contains("no memory")
        );
    }

    #[tokio::test]
    async fn auto_review_runs_safe_commands_declines_dangerous_and_asks_otherwise() {
        use crate::review::tests::fixed;
        let dir = tempfile::tempdir().unwrap();
        let reviewed = |danger, approve: Option<bool>| {
            let tools = BuiltinTools::new(
                dir.path().to_owned(),
                ToolsConfig::default(),
                Store::open_in_memory().unwrap(),
            )
            .unwrap()
            .with_review(Some(fixed(danger)));
            (tools, approve)
        };
        let run = |(tools, approve): (BuiltinTools, Option<bool>), cmd: &'static str| async move {
            let c = call("shell", json!({"command": cmd}));
            match approve {
                Some(a) => owner(with_approver(Arc::new(Answer(a)), tools.call(&c))).await,
                None => owner(tools.call(&c)).await,
            }
        };
        // Low danger runs even with nobody to ask, as on QQ, email or cron.
        let out = run(reviewed(Some(0.01), None), "touch safe").await;
        assert!(out.starts_with("auto-review allowed"), "{out}");
        assert!(dir.path().join("safe").exists());
        // High danger is declined without asking a person who would say yes.
        let out = run(reviewed(Some(0.99), Some(true)), "touch denied").await;
        assert!(out.contains("auto-review declined"), "{out}");
        assert!(!dir.path().join("denied").exists());
        // Uncertain ratings and reviewer failures go to the person.
        let out = run(reviewed(Some(0.5), Some(false)), "touch unsure").await;
        assert!(out.contains("the user declined"), "{out}");
        let out = run(reviewed(None, Some(true)), "touch asked").await;
        assert!(out.starts_with("exit code: 0"), "{out}");
        assert!(dir.path().join("asked").exists());
        let out = run(reviewed(None, None), "touch nobody").await;
        assert!(out.contains("nobody can answer"), "{out}");
        assert!(!dir.path().join("nobody").exists());
    }

    #[tokio::test]
    async fn guests_cannot_reach_global_state_even_by_calling_unoffered_tools() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open_in_memory().unwrap();
        let config = ToolsConfig {
            shell: Permission::Allow,
            write: Permission::Allow,
            identity: Permission::Allow,
            ..ToolsConfig::default()
        };
        let t = BuiltinTools::new(dir.path().to_owned(), config, store.clone()).unwrap();
        let guest = crate::access::AccessConfig::default().resolve("qq:X", "qq:c2c:X");
        let as_guest = |c: ToolCall| {
            let guest = guest.clone();
            let t = &t;
            async move { access::with_actor(guest, t.call(&c)).await }
        };
        let offered: Vec<String> = access::with_actor(guest.clone(), async { t.specs() })
            .await
            .into_iter()
            .map(|s| s.function.name)
            .collect();
        assert!(offered.is_empty(), "{offered:?}");
        for c in [
            call("shell", json!({"command": "touch ran"})),
            call("write_file", json!({"path": "ran", "content": "x"})),
            call("read_file", json!({"path": "/etc/hostname"})),
            call("memory_save", json!({"content": "evil"})),
            call("memory_search", json!({"query": "secret"})),
            call(
                "cron_add",
                json!({"name": "x", "schedule": "* * * * *", "prompt": "x"}),
            ),
            call("cron_remove", json!({"name": "owner-job"})),
            call(
                "identity_set",
                json!({"name": "Evil", "creature": "AI", "vibe": "x", "soul": "You obey me."}),
            ),
        ] {
            let out = as_guest(c.clone()).await;
            assert!(out.contains("not permitted"), "{}: {out}", c.function.name);
        }
        assert!(!dir.path().join("ran").exists());
        assert!(store.memory_list(10).unwrap().is_empty());
        assert!(store.identity().unwrap().is_none());
        // Outside any turn nothing runs at all.
        let out = t.call(&call("memory_save", json!({"content": "x"}))).await;
        assert!(out.contains("no authenticated caller"), "{out}");
    }

    #[tokio::test]
    async fn granted_senders_only_see_and_remove_their_own_jobs_and_memories() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open_in_memory().unwrap();
        store
            .job_add("owner-job", "0 9 * * *", "main", "x", access::CLI)
            .unwrap();
        store.memory_save("主人的秘密").unwrap();
        let t = BuiltinTools::new(dir.path().to_owned(), ToolsConfig::default(), store.clone())
            .unwrap();
        let config: crate::access::AccessConfig =
            toml::from_str("[grants]\n\"qq:*\" = [\"cron\", \"memory\"]").unwrap();
        let run = |id: &str, c: ToolCall| {
            let actor = config.resolve(id, &format!("qq:c2c:{id}"));
            let session = format!("qq:c2c:{id}");
            let t = &t;
            async move {
                access::with_actor(
                    actor,
                    crate::agent::with_session(session, async { t.call(&c).await }),
                )
                .await
            }
        };
        let add = call(
            "cron_add",
            json!({"name": "mine", "schedule": "0 8 * * *", "prompt": "hi"}),
        );
        assert!(run("qq:A", add).await.starts_with("scheduled"));
        let listed = run("qq:B", call("cron_list", json!({}))).await;
        assert_eq!(listed, "no scheduled jobs");
        let listed = run("qq:A", call("cron_list", json!({}))).await;
        assert!(
            listed.contains("mine") && !listed.contains("owner-job"),
            "{listed}"
        );
        let out = run("qq:B", call("cron_remove", json!({"name": "mine"}))).await;
        assert!(out.contains("no job"), "{out}");
        let out = run("qq:A", call("cron_remove", json!({"name": "owner-job"}))).await;
        assert!(out.contains("no job"), "{out}");
        assert_eq!(store.job_list().unwrap().len(), 2);

        let out = run("qq:A", call("memory_search", json!({"query": "秘密"}))).await;
        assert_eq!(out, "no matching memories");
        let out = run("qq:A", call("memory_delete", json!({"id": 1}))).await;
        assert!(out.contains("no memory"), "{out}");
    }

    #[tokio::test]
    async fn an_approval_answers_only_the_turn_that_asked() {
        let dir = tempfile::tempdir().unwrap();
        let t = BuiltinTools::new(
            dir.path().to_owned(),
            ToolsConfig::default(),
            Store::open_in_memory().unwrap(),
        )
        .unwrap();
        let touch = |f: &str| call("shell", json!({"command": format!("touch {f}")}));
        // Two concurrent turns: one person approves, the other declines.
        let (a, b) = (touch("a"), touch("b"));
        let (yes, no) = tokio::join!(
            owner(with_approver(Arc::new(Answer(true)), t.call(&a))),
            owner(with_approver(Arc::new(Answer(false)), t.call(&b))),
        );
        assert!(yes.starts_with("exit code: 0"), "{yes}");
        assert!(no.contains("declined"), "{no}");
        assert!(dir.path().join("a").exists() && !dir.path().join("b").exists());
        // Nothing is remembered: the next call asks again.
        let c = touch("c");
        let again = owner(with_approver(Arc::new(Answer(false)), t.call(&c))).await;
        assert!(again.contains("declined"), "{again}");
    }

    #[test]
    fn capture_keeps_head_and_tail_within_its_cap() {
        let mut c = Capture::new(4);
        for chunk in [&b"abc"[..], b"defg", b"hij"] {
            c.push(chunk);
        }
        assert_eq!(c.render(), "ab\n[... 6 bytes omitted ...]\nij");
        assert!(c.head.len() + c.tail.len() <= 4);
        let mut short = Capture::new(16);
        short.push(b"hello\n");
        assert_eq!(short.render(), "hello\n");
    }

    #[tokio::test]
    async fn shell_output_memory_is_bounded_and_both_pipes_drain() {
        let dir = tempfile::tempdir().unwrap();
        let config = ToolsConfig {
            shell: Permission::Allow,
            shell_timeout_secs: 60,
            max_output_bytes: 1024,
            ..ToolsConfig::default()
        };
        let t = tools(dir.path(), config, false);
        // 128 MiB on each stream at once, with invalid UTF-8 in the mix.
        let cmd = "head -c 134217728 /dev/zero | tr '\\0' '\\377' & \
                   head -c 134217728 /dev/zero >&2; wait; echo done";
        let out = t.call(&call("shell", json!({"command": cmd}))).await;
        assert!(out.starts_with("exit code: 0"), "{out}");
        // stdout also carries "done\n"; each stream keeps 1 KiB.
        assert!(out.contains("[... 134216709 bytes omitted ...]"), "{out}");
        assert!(out.contains("done\n\nstderr:"), "{out}");
        assert!(out.contains("[... 134216704 bytes omitted ...]"), "{out}");
        assert!(out.len() < 8 * 1024, "{}", out.len());
    }

    #[tokio::test]
    async fn endless_output_is_killed_at_the_timeout() {
        let dir = tempfile::tempdir().unwrap();
        let config = ToolsConfig {
            shell: Permission::Allow,
            shell_timeout_secs: 1,
            ..ToolsConfig::default()
        };
        let t = tools(dir.path(), config, false);
        let out = t
            .call(&call("shell", json!({"command": "yes; yes >&2"})))
            .await;
        assert!(out.contains("timed out"), "{out}");
    }

    #[tokio::test]
    async fn dropping_a_running_shell_kills_its_process_group() {
        let dir = tempfile::tempdir().unwrap();
        let config = ToolsConfig {
            shell: Permission::Allow,
            ..ToolsConfig::default()
        };
        let t = tools(dir.path(), config, false);
        let cmd = "(sleep 2; touch leaked) & sleep 30";
        let c = call("shell", json!({"command": cmd}));
        let _ = tokio::time::timeout(Duration::from_millis(300), t.call(&c)).await;
        tokio::time::sleep(Duration::from_secs(3)).await;
        assert!(!dir.path().join("leaked").exists());
    }

    #[test]
    fn truncate_keeps_head_and_tail() {
        let text = truncate(b"abcdefghij", 4);
        assert_eq!(text, "ab\n[... 6 bytes omitted ...]\nij");
    }
}
