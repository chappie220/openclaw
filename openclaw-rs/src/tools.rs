//! Built-in tools: shell and workspace files, gated by per-category permissions.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use serde::Deserialize;
use serde::de::DeserializeOwned;
use serde_json::json;
use tokio::io::AsyncReadExt;

use crate::agent::Tools;
use crate::config::{Permission, ToolsConfig};
use crate::llm::{ToolCall, ToolSpec};
use crate::store::Store;

/// Asks a person whether a gated action may run. Front ends supply their own.
#[async_trait]
pub trait Approver: Send + Sync {
    async fn approve(&self, tool: &str, summary: &str) -> bool;
}

/// Prompts on the controlling terminal, so piped stdin cannot answer for the user.
pub struct TerminalApprover;

#[async_trait]
impl Approver for TerminalApprover {
    async fn approve(&self, tool: &str, summary: &str) -> bool {
        let prompt = format!("\nAllow {tool}?\n  {summary}\n[y/N] ");
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
                && matches!(answer.trim(), "y" | "Y" | "yes")
        })
        .await
        .unwrap_or(false)
    }
}

pub struct BuiltinTools {
    workspace: PathBuf,
    config: ToolsConfig,
    approver: Arc<dyn Approver>,
    store: Store,
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
struct MemoryDeleteArgs {
    id: i64,
}

#[derive(Deserialize)]
struct ListArgs {
    #[serde(default)]
    path: Option<String>,
}

impl BuiltinTools {
    pub fn new(
        workspace: PathBuf,
        config: ToolsConfig,
        approver: Arc<dyn Approver>,
        store: Store,
    ) -> std::io::Result<Self> {
        std::fs::create_dir_all(&workspace)?;
        Ok(Self {
            workspace,
            config,
            approver,
            store,
        })
    }

    fn memory_save(&self, args: MemorySaveArgs) -> Result<String, String> {
        let id = self
            .store
            .memory_save(&args.content)
            .map_err(|e| format!("error: {e:#}"))?;
        Ok(format!("saved memory #{id}"))
    }

    fn memory_search(&self, args: MemorySearchArgs) -> Result<String, String> {
        let limit = args.limit.unwrap_or(10).clamp(1, 50);
        let hits = self
            .store
            .memory_search(&args.query, limit)
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

    fn memory_delete(&self, args: MemoryDeleteArgs) -> Result<String, String> {
        match self.store.memory_delete(args.id) {
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
            Permission::Ask if self.approver.approve(tool, summary).await => Ok(()),
            Permission::Ask => Err(format!("error: the user declined {tool}")),
        }
    }

    async fn shell(&self, args: ShellArgs) -> Result<String, String> {
        self.permit(self.config.shell, "shell", &args.command)
            .await?;
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
        let pid = child.id();
        let mut stdout = child.stdout.take().expect("piped stdout");
        let mut stderr = child.stderr.take().expect("piped stderr");
        let run = async {
            let (mut out, mut err) = (Vec::new(), Vec::new());
            let (read_out, read_err, status) = tokio::join!(
                stdout.read_to_end(&mut out),
                stderr.read_to_end(&mut err),
                child.wait()
            );
            read_out.and(read_err).map_err(|e| e.to_string())?;
            Ok::<_, String>((status.map_err(|e| e.to_string())?, out, err))
        };
        let timeout = Duration::from_secs(self.config.shell_timeout_secs);
        let (status, out, err) = match tokio::time::timeout(timeout, run).await {
            Ok(result) => result.map_err(|e| format!("error: {e}"))?,
            Err(_) => {
                if let Some(pid) = pid {
                    // SAFETY: signalling a process group we created; failure is harmless.
                    unsafe { libc::kill(-(pid as i32), libc::SIGKILL) };
                }
                return Err(format!(
                    "error: command timed out after {}s and was killed",
                    timeout.as_secs()
                ));
            }
        };
        let cap = self.config.max_output_bytes;
        let code = status
            .code()
            .map_or_else(|| "killed by signal".into(), |c| c.to_string());
        let mut text = format!("exit code: {code}\n");
        if !out.is_empty() {
            text.push_str(&format!("stdout:\n{}\n", truncate(&out, cap)));
        }
        if !err.is_empty() {
            text.push_str(&format!("stderr:\n{}\n", truncate(&err, cap)));
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

fn parse<T: DeserializeOwned>(call: &ToolCall) -> Result<T, String> {
    serde_json::from_str(&call.function.arguments)
        .map_err(|err| format!("error: invalid arguments for {}: {err}", call.function.name))
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

    async fn call(&self, call: &ToolCall) -> String {
        let result = match call.function.name.as_str() {
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
            "memory_save" => parse(call).and_then(|args| self.memory_save(args)),
            "memory_search" => parse(call).and_then(|args| self.memory_search(args)),
            "memory_delete" => parse(call).and_then(|args| self.memory_delete(args)),
            other => Err(format!("error: unknown tool {other}")),
        };
        result.unwrap_or_else(|err| err)
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

    fn tools(dir: &Path, config: ToolsConfig, approve: bool) -> BuiltinTools {
        BuiltinTools::new(
            dir.to_owned(),
            config,
            Arc::new(Answer(approve)),
            Store::open_in_memory().unwrap(),
        )
        .unwrap()
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

    #[test]
    fn truncate_keeps_head_and_tail() {
        let text = truncate(b"abcdefghij", 4);
        assert_eq!(text, "ab\n[... 6 bytes omitted ...]\nij");
    }
}
