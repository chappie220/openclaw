//! Skills: instructions for particular tasks, in the `SKILL.md` format of
//! Agent Skills and OpenClaw, so most published skills work as they are.
//!
//! Each skill is a directory `<workspace>/skills/<name>/` holding a
//! `SKILL.md` (front matter with `name` and `description`, then the steps)
//! and whatever scripts and references it needs. Every turn the system
//! prompt lists only the names and descriptions; the model loads a skill's
//! full text with the `skill` tool when a request matches it. Being in the
//! workspace, skills are in backups, and the model can run their scripts and
//! write new ones with the file tools.
//!
//! A skill can say what it needs (OpenClaw's `metadata.openclaw.requires`:
//! `bins`, `anyBins`, `env`, and `os`); one whose needs are not met is left
//! out, and `skills list` and `doctor` say why.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde_json::Value;

use crate::config::SkillsConfig;
use crate::i18n::{Tr, tr};

pub const DIR: &str = "skills";
const FILE: &str = "SKILL.md";
/// Largest SKILL.md read; longer ones are cut.
const MAX_SKILL_BYTES: usize = 128 * 1024;
/// Description characters shown in the system prompt.
const MAX_DESCRIPTION_CHARS: usize = 300;
/// Skills listed in the system prompt; the rest are named as a count.
const MAX_LISTED: usize = 100;

const NONE: Tr = tr(
    "No skills installed. Install one with `openclaw-rs skills install <directory or git URL>`, or put it in {}",
    "还没有安装 skill。用 `openclaw-rs skills install <目录或 git 地址>` 安装，或者直接放到 {}",
);
const READY: Tr = tr("ready", "可用");
const OFF: Tr = tr("off (skills.disabled)", "已关闭（skills.disabled）");
const NEEDS: Tr = tr("needs {}", "缺少 {}");
const BROKEN: Tr = tr("broken: {}", "有问题：{}");
const INSTALLED: Tr = tr("Installed {} in {}", "已安装 {}，位置 {}");
const REMOVED: Tr = tr("Removed {}", "已删除 {}");
const EXISTS: Tr = tr(
    "{} is already installed; add --force to replace it",
    "{} 已经安装过了；加 --force 替换",
);
const NOT_FOUND: Tr = tr("no skill named {}", "没有叫 {} 的 skill");
const NO_SKILL_IN: Tr = tr(
    "{} has no SKILL.md, and no directories with one",
    "{} 里没有 SKILL.md，子目录里也没有",
);
const BAD_NAME: Tr = tr(
    "{} cannot be a skill name: use letters, digits, - and _",
    "{} 不能作为 skill 名：请用字母、数字、- 和 _",
);
const NO_GIT: Tr = tr(
    "cannot run git to download {} (install it: doas apk add git)",
    "无法运行 git 下载 {}（请安装：doas apk add git）",
);
const GIT_FAILED: Tr = tr("git could not download {}: {}", "git 无法下载 {}：{}");

#[cfg(test)]
pub const ALL: &[Tr] = &[
    NONE,
    READY,
    OFF,
    NEEDS,
    BROKEN,
    INSTALLED,
    REMOVED,
    EXISTS,
    NOT_FOUND,
    NO_SKILL_IN,
    BAD_NAME,
    NO_GIT,
    GIT_FAILED,
];

/// The skills directory and which skills are turned off.
#[derive(Debug, Clone)]
pub struct Skills {
    pub dir: PathBuf,
    disabled: Vec<String>,
}

/// One skill directory as found.
#[derive(Debug, Clone, PartialEq)]
pub struct Skill {
    pub name: String,
    pub description: String,
    pub dir: PathBuf,
    pub state: State,
}

#[derive(Debug, Clone, PartialEq)]
pub enum State {
    Ready,
    /// Named in `skills.disabled`.
    Off,
    /// Programs, environment variables or an OS it needs.
    Missing(Vec<String>),
    /// SKILL.md cannot be read or has no description.
    Broken(String),
}

impl Skill {
    /// The state as a person reads it.
    pub fn status(&self) -> String {
        match &self.state {
            State::Ready => READY.now().into(),
            State::Off => OFF.now().into(),
            State::Missing(what) => NEEDS.with(&[&what.join(", ")]),
            State::Broken(why) => BROKEN.with(&[why]),
        }
    }
}

impl Skills {
    /// Whether or not `skills.enabled` is on: the agent checks that.
    pub fn new(workspace: &Path, config: &SkillsConfig) -> Self {
        Self {
            dir: workspace.join(DIR),
            disabled: config.disabled.clone(),
        }
    }

    /// Every skill directory, by name.
    pub fn scan(&self) -> Vec<Skill> {
        let Ok(entries) = std::fs::read_dir(&self.dir) else {
            return Vec::new();
        };
        let mut skills: Vec<Skill> = entries
            .filter_map(|e| e.ok())
            .filter(|e| e.path().is_dir() && !e.file_name().to_string_lossy().starts_with('.'))
            .filter(|e| e.path().join(FILE).is_file())
            .map(|e| self.read(&e.path()))
            .collect();
        skills.sort_by(|a, b| a.name.cmp(&b.name));
        skills
    }

    pub fn ready(&self) -> Vec<Skill> {
        self.scan()
            .into_iter()
            .filter(|s| s.state == State::Ready)
            .collect()
    }

    fn read(&self, dir: &Path) -> Skill {
        let dir_name = dir
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        let mut skill = Skill {
            name: dir_name.clone(),
            description: String::new(),
            dir: dir.to_owned(),
            state: State::Ready,
        };
        let text = match read_capped(&dir.join(FILE)) {
            Ok(text) => text,
            Err(err) => {
                skill.state = State::Broken(format!("{err:#}"));
                return skill;
            }
        };
        let (front, _) = front_matter(&text);
        if let Some(name) = front.get("name").filter(|n| !n.trim().is_empty()) {
            skill.name = name.trim().to_owned();
        }
        skill.description = front
            .get("description")
            .map(|d| d.split_whitespace().collect::<Vec<_>>().join(" "))
            .unwrap_or_default();
        skill.state = if skill.description.is_empty() {
            State::Broken("SKILL.md has no description".into())
        } else if self
            .disabled
            .iter()
            .any(|d| d == &skill.name || d == &dir_name)
        {
            State::Off
        } else {
            let missing = missing(&Requires::from_front(&front));
            if missing.is_empty() {
                State::Ready
            } else {
                State::Missing(missing)
            }
        };
        skill
    }

    /// The system prompt section: the skills ready to use, and, for someone
    /// who `can_write` files, how to save a new one.
    pub fn prompt(&self, can_write: bool) -> Option<String> {
        let ready = self.ready();
        let mut out = String::new();
        if !ready.is_empty() {
            out.push_str(
                "## Skills\n\nSkills are instructions for particular tasks. When a request matches a \
                 skill's description, load it with the skill tool before you start and follow it. \
                 Load only the skill you need.\n\n",
            );
            for skill in ready.iter().take(MAX_LISTED) {
                let description: String = skill
                    .description
                    .chars()
                    .take(MAX_DESCRIPTION_CHARS)
                    .collect();
                out.push_str(&format!("- {}: {description}\n", skill.name));
            }
            if ready.len() > MAX_LISTED {
                out.push_str(&format!(
                    "- ... and {} more in {}/\n",
                    ready.len() - MAX_LISTED,
                    DIR
                ));
            }
        }
        if can_write {
            if out.is_empty() {
                out.push_str("## Skills\n\n");
            } else {
                out.push('\n');
            }
            out.push_str(&format!(
                "When the user asks you to remember how to do something as a skill, write \
                 {DIR}/<name>/SKILL.md in the workspace (name: lowercase letters, digits and -): \
                 front matter with name and description (what it does and when to use it), then \
                 the steps; put any scripts next to it.\n"
            ));
        }
        (!out.is_empty()).then(|| out.trim_end().to_owned())
    }

    /// The full text of a ready skill, for the model.
    pub fn load(&self, name: &str) -> Result<String> {
        let skills = self.scan();
        let skill = skills
            .iter()
            .find(|s| s.name == name)
            .or_else(|| {
                skills
                    .iter()
                    .find(|s| s.dir.file_name().is_some_and(|n| n == name))
            })
            .with_context(|| {
                let ready: Vec<&str> = skills
                    .iter()
                    .filter(|s| s.state == State::Ready)
                    .map(|s| s.name.as_str())
                    .collect();
                format!("no skill named {name:?}; available: {}", ready.join(", "))
            })?;
        match &skill.state {
            State::Ready => {}
            State::Off => bail!("{name} is turned off in skills.disabled"),
            State::Missing(what) => {
                bail!("{name} cannot be used here: it needs {}", what.join(", "))
            }
            State::Broken(why) => bail!("{name} is broken: {why}"),
        }
        let text = read_capped(&skill.dir.join(FILE))?;
        let (_, body) = front_matter(&text);
        let dir = skill.dir.display().to_string();
        let mut files = Vec::new();
        list_files(&skill.dir, Path::new(""), &mut files, 50);
        files.retain(|f| f != FILE);
        let mut out = format!(
            "Skill {} in {dir} (paths in it are relative to this directory; run its scripts from there).\n",
            skill.name
        );
        if !files.is_empty() {
            out.push_str(&format!("Files: {}\n", files.join(", ")));
        }
        out.push('\n');
        out.push_str(body.replace("{baseDir}", &dir).trim());
        Ok(out)
    }
}

fn read_capped(path: &Path) -> Result<String> {
    let mut data =
        std::fs::read(path).with_context(|| format!("cannot read {}", path.display()))?;
    data.truncate(MAX_SKILL_BYTES);
    Ok(String::from_utf8_lossy(&data).into_owned())
}

/// Files under `dir`, relative, at most `cap`.
fn list_files(dir: &Path, prefix: &Path, out: &mut Vec<String>, cap: usize) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    let mut entries: Vec<_> = entries.filter_map(|e| e.ok()).collect();
    entries.sort_by_key(|e| e.file_name());
    for entry in entries {
        if out.len() >= cap {
            return;
        }
        let name = entry.file_name();
        if name.to_string_lossy().starts_with('.') {
            continue;
        }
        let relative = prefix.join(&name);
        match entry.file_type() {
            Ok(t) if t.is_dir() => list_files(&entry.path(), &relative, out, cap),
            Ok(t) if t.is_file() => out.push(relative.display().to_string()),
            _ => {}
        }
    }
}

/// The top-level keys of a YAML front matter block and the text after it.
/// Handles what SKILL.md files use: `key: value` (plain or quoted), block
/// scalars (`|`, `>`), and a key whose value is on the indented lines below
/// it (OpenClaw writes `metadata` as JSON there).
pub fn front_matter(text: &str) -> (BTreeMap<String, String>, &str) {
    let mut keys = BTreeMap::new();
    let text = text.trim_start_matches('\u{feff}');
    let Some(rest) = text
        .strip_prefix("---\n")
        .or_else(|| text.strip_prefix("---\r\n"))
    else {
        return (keys, text);
    };
    let mut lines = Vec::new();
    let mut body = None;
    let mut offset = 0;
    for line in rest.split_inclusive('\n') {
        offset += line.len();
        if line.trim_end() == "---" {
            body = Some(&rest[offset..]);
            break;
        }
        lines.push(line.trim_end_matches(['\n', '\r']));
    }
    // No closing line: not front matter.
    let Some(body) = body else {
        return (keys, text);
    };
    let mut i = 0;
    while i < lines.len() {
        let line = lines[i];
        i += 1;
        if line.starts_with([' ', '\t', '#']) || line.trim().is_empty() {
            continue;
        }
        let Some((key, value)) = line.split_once(':') else {
            continue;
        };
        let value = value.trim();
        let mut block = Vec::new();
        while i < lines.len() && (lines[i].starts_with([' ', '\t']) || lines[i].trim().is_empty()) {
            block.push(lines[i].trim());
            i += 1;
        }
        let value = match value {
            "|" | "|-" | "|+" => block.join("\n").trim().to_owned(),
            ">" | ">-" | ">+" => block.join(" ").trim().to_owned(),
            "" => block.join("\n").trim().to_owned(),
            plain => {
                // A plain scalar may go on over indented lines.
                let mut value = unquote(plain);
                for more in block.iter().filter(|l| !l.is_empty()) {
                    value.push(' ');
                    value.push_str(more);
                }
                value
            }
        };
        keys.insert(key.trim().to_owned(), value);
    }
    (keys, body)
}

fn unquote(value: &str) -> String {
    if value.len() >= 2 && value.starts_with('"') && value.ends_with('"') {
        serde_json::from_str(value).unwrap_or_else(|_| value[1..value.len() - 1].to_owned())
    } else if value.len() >= 2 && value.starts_with('\'') && value.ends_with('\'') {
        value[1..value.len() - 1].replace("''", "'")
    } else {
        value.to_owned()
    }
}

/// What a skill needs to work on this host.
#[derive(Debug, Default, PartialEq)]
struct Requires {
    /// Programs that must all be on PATH.
    bins: Vec<String>,
    /// Programs of which one must be on PATH.
    any_bins: Vec<String>,
    env: Vec<String>,
    /// `linux`, `darwin`, `win32`.
    os: Vec<String>,
}

impl Requires {
    /// From `metadata` (JSON, or YAML written as JSON with trailing commas),
    /// under `openclaw` or one of its earlier names.
    fn from_front(front: &BTreeMap<String, String>) -> Self {
        let Some(metadata) = front
            .get("metadata")
            .and_then(|m| serde_json::from_str::<Value>(&without_trailing_commas(m)).ok())
        else {
            return Self::default();
        };
        let Some(ours) = ["openclaw", "clawdbot", "clawdis", "moltbot"]
            .iter()
            .find_map(|k| metadata.get(*k))
        else {
            return Self::default();
        };
        let strings = |v: Option<&Value>| -> Vec<String> {
            match v {
                Some(Value::Array(items)) => items
                    .iter()
                    .filter_map(Value::as_str)
                    .map(str::to_owned)
                    .collect(),
                Some(Value::String(s)) => vec![s.clone()],
                _ => Vec::new(),
            }
        };
        let requires = ours.get("requires");
        Self {
            bins: strings(requires.and_then(|r| r.get("bins"))),
            any_bins: strings(requires.and_then(|r| r.get("anyBins"))),
            env: strings(requires.and_then(|r| r.get("env"))),
            os: strings(ours.get("os")),
        }
    }
}

/// `{"a": [1, 2,], }` as JSON.
fn without_trailing_commas(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut in_string = false;
    let mut escaped = false;
    for c in text.chars() {
        if in_string {
            in_string = !(c == '"' && !escaped);
            escaped = c == '\\' && !escaped;
        } else if c == '"' {
            in_string = true;
        } else if c == '}' || c == ']' {
            let kept = out.trim_end().len();
            if out[..kept].ends_with(',') {
                out.truncate(kept - 1);
            }
        }
        out.push(c);
    }
    out
}

fn missing(requires: &Requires) -> Vec<String> {
    let mut missing = Vec::new();
    let os = match std::env::consts::OS {
        "macos" => "darwin",
        "windows" => "win32",
        other => other,
    };
    if !requires.os.is_empty() && !requires.os.iter().any(|o| o == os) {
        missing.push(format!("OS {}", requires.os.join("/")));
    }
    missing.extend(requires.bins.iter().filter(|b| !on_path(b)).cloned());
    if !requires.any_bins.is_empty() && !requires.any_bins.iter().any(|b| on_path(b)) {
        missing.push(requires.any_bins.join(" | "));
    }
    missing.extend(
        requires
            .env
            .iter()
            .filter(|e| std::env::var_os(e).is_none_or(|v| v.is_empty()))
            .map(|e| format!("${e}")),
    );
    missing
}

fn on_path(program: &str) -> bool {
    use std::os::unix::fs::PermissionsExt;
    let executable = |p: &Path| {
        p.metadata()
            .is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
    };
    if program.contains('/') {
        return executable(Path::new(program));
    }
    std::env::var_os("PATH")
        .is_some_and(|path| std::env::split_paths(&path).any(|dir| executable(&dir.join(program))))
}

/// Whether `name` is safe as a directory name.
fn valid_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 64
        && !name.starts_with(['.', '-'])
        && name
            .chars()
            .all(|c| c.is_alphanumeric() || matches!(c, '-' | '_' | '.'))
}

/// Installs the skills in `source` (a directory, or a git URL, including
/// GitHub's `.../tree/<branch>/<path>` links): the skill there, or every
/// directory under it, or under its `skills/`, that holds a SKILL.md.
pub fn install(skills: &Skills, source: &str, force: bool) -> Result<Vec<String>> {
    std::fs::create_dir_all(&skills.dir)
        .with_context(|| format!("cannot create {}", skills.dir.display()))?;
    let local = Path::new(source);
    if local.is_dir() {
        return install_from(skills, local, force);
    }
    if !source.contains("://") && !source.starts_with("git@") {
        bail!("{source}: no such directory");
    }
    let (repo, branch, sub) = parse_git_url(source);
    let staging = skills.dir.join(format!(".download-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&staging);
    let mut git = std::process::Command::new("git");
    git.args(["clone", "--depth", "1", "--quiet"]);
    if let Some(branch) = &branch {
        git.args(["--branch", branch]);
    }
    git.arg(&repo).arg(&staging);
    git.env("GIT_TERMINAL_PROMPT", "0");
    let output = git
        .output()
        .map_err(|_| anyhow::anyhow!(NO_GIT.with(&[source])));
    let result = output.and_then(|output| {
        if !output.status.success() {
            bail!(GIT_FAILED.with(&[source, String::from_utf8_lossy(&output.stderr).trim()]));
        }
        install_from(skills, &staging.join(sub.as_deref().unwrap_or("")), force)
    });
    let _ = std::fs::remove_dir_all(&staging);
    result
}

/// Repository, branch and path inside it.
fn parse_git_url(url: &str) -> (String, Option<String>, Option<String>) {
    for host in ["https://github.com/", "https://gitlab.com/"] {
        if let Some(rest) = url.strip_prefix(host) {
            let parts: Vec<&str> = rest.trim_end_matches('/').split('/').collect();
            let marker = parts.iter().position(|p| *p == "tree" || *p == "blob");
            if let Some(at) = marker
                && at >= 2
                && parts.len() > at + 1
            {
                let repo_parts: Vec<&str> =
                    parts[..at].iter().copied().filter(|p| *p != "-").collect();
                let mut sub = parts[at + 2..].join("/");
                if sub.ends_with(FILE) {
                    sub = sub.trim_end_matches(FILE).trim_end_matches('/').to_owned();
                }
                return (
                    format!("{host}{}", repo_parts.join("/")),
                    Some(parts[at + 1].to_owned()),
                    (!sub.is_empty()).then_some(sub),
                );
            }
        }
    }
    (url.to_owned(), None, None)
}

fn install_from(skills: &Skills, source: &Path, force: bool) -> Result<Vec<String>> {
    let found = find_skills(source);
    if found.is_empty() {
        bail!(NO_SKILL_IN.with(&[&source.display().to_string()]));
    }
    // Check every name before anything is copied.
    let mut plan = Vec::new();
    for dir in found {
        let text = read_capped(&dir.join(FILE))?;
        let (front, _) = front_matter(&text);
        let name = front
            .get("name")
            .map(|n| n.trim().to_owned())
            .filter(|n| !n.is_empty())
            .or_else(|| dir.file_name().map(|n| n.to_string_lossy().into_owned()))
            .unwrap_or_default();
        if !valid_name(&name) {
            bail!(BAD_NAME.with(&[&format!("{name:?}")]));
        }
        let target = skills.dir.join(&name);
        if target.exists() && !force {
            bail!(EXISTS.with(&[&name]));
        }
        plan.push((name, dir, target));
    }
    let mut installed = Vec::new();
    for (name, dir, target) in plan {
        let staging = skills.dir.join(format!(".installing-{name}"));
        let _ = std::fs::remove_dir_all(&staging);
        copy_tree(&dir, &staging)?;
        if target.exists() {
            std::fs::remove_dir_all(&target)
                .with_context(|| format!("cannot replace {}", target.display()))?;
        }
        std::fs::rename(&staging, &target)
            .with_context(|| format!("cannot move {} into place", target.display()))?;
        installed.push(name);
    }
    Ok(installed)
}

fn find_skills(source: &Path) -> Vec<PathBuf> {
    if source.join(FILE).is_file() {
        return vec![source.to_owned()];
    }
    for base in [source.to_owned(), source.join(DIR)] {
        let Ok(entries) = std::fs::read_dir(&base) else {
            continue;
        };
        let mut found: Vec<PathBuf> = entries
            .filter_map(|e| e.ok())
            .map(|e| e.path())
            .filter(|p| {
                p.is_dir()
                    && p.join(FILE).is_file()
                    && !p
                        .file_name()
                        .is_some_and(|n| n.to_string_lossy().starts_with('.'))
            })
            .collect();
        if !found.is_empty() {
            found.sort();
            return found;
        }
    }
    Vec::new()
}

/// Copies regular files and directories; skips links, `.git` and special files.
fn copy_tree(from: &Path, to: &Path) -> Result<()> {
    std::fs::create_dir_all(to).with_context(|| format!("cannot create {}", to.display()))?;
    for entry in
        std::fs::read_dir(from).with_context(|| format!("cannot read {}", from.display()))?
    {
        let entry = entry?;
        if entry.file_name() == ".git" {
            continue;
        }
        let target = to.join(entry.file_name());
        let kind = entry.file_type()?;
        if kind.is_dir() {
            copy_tree(&entry.path(), &target)?;
        } else if kind.is_file() {
            std::fs::copy(entry.path(), &target)
                .with_context(|| format!("cannot copy {}", entry.path().display()))?;
        }
    }
    Ok(())
}

pub fn remove(skills: &Skills, name: &str) -> Result<()> {
    let skill = skills
        .scan()
        .into_iter()
        .find(|s| s.name == name || s.dir.file_name().is_some_and(|n| n == name))
        .with_context(|| NOT_FOUND.with(&[name]))?;
    std::fs::remove_dir_all(&skill.dir)
        .with_context(|| format!("cannot remove {}", skill.dir.display()))?;
    println!("{}", REMOVED.with(&[&skill.name]));
    Ok(())
}

/// `skills list`.
pub fn print_list(skills: &Skills) {
    let all = skills.scan();
    if all.is_empty() {
        println!("{}", NONE.with(&[&skills.dir.display().to_string()]));
        return;
    }
    // Terminal columns: CJK characters take two.
    let columns = |text: &str| -> usize {
        text.chars()
            .map(|c| if c > '\u{2e7f}' { 2 } else { 1 })
            .sum()
    };
    let pad = |text: &str, width: usize| {
        format!("{text}{}", " ".repeat(width.saturating_sub(columns(text))))
    };
    let statuses: Vec<String> = all.iter().map(Skill::status).collect();
    let names = all.iter().map(|s| columns(&s.name)).max().unwrap_or(0);
    let width = statuses.iter().map(|s| columns(s)).max().unwrap_or(0);
    for (skill, status) in all.iter().zip(&statuses) {
        let mark = if skill.state == State::Ready {
            "✓"
        } else {
            "✗"
        };
        let description: String = skill.description.chars().take(80).collect();
        println!(
            "{mark} {}  {}  {description}",
            pad(&skill.name, names),
            pad(status, width)
        );
    }
}

pub fn print_installed(skills: &Skills, names: &[String]) {
    println!(
        "{}",
        INSTALLED.with(&[&names.join(", "), &skills.dir.display().to_string()])
    );
    for skill in skills.scan().iter().filter(|s| names.contains(&s.name)) {
        if skill.state != State::Ready {
            println!("  ✗ {}: {}", skill.name, skill.status());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn texts_have_both_languages() {
        crate::i18n::assert_complete(ALL);
    }

    fn skills(dir: &Path, disabled: &[&str]) -> Skills {
        Skills::new(
            dir,
            &SkillsConfig {
                enabled: true,
                disabled: disabled.iter().map(|s| s.to_string()).collect(),
            },
        )
    }

    fn write(path: &Path, text: &str) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, text).unwrap();
    }

    #[test]
    fn front_matter_reads_what_skill_files_use() {
        let text = "---\nname: weather\ndescription: \"Get the weather: today and tomorrow\"\n\
                    homepage: https://wttr.in\nlong: >\n  folded\n  text\nmetadata:\n  {\n    \"openclaw\":\n      {\n        \"requires\": { \"bins\": [\"curl\"], },\n      },\n  }\n---\n# Weather\nUse curl.\n";
        let (front, body) = front_matter(text);
        assert_eq!(front["name"], "weather");
        assert_eq!(front["description"], "Get the weather: today and tomorrow");
        assert_eq!(front["homepage"], "https://wttr.in");
        assert_eq!(front["long"], "folded text");
        assert_eq!(body, "# Weather\nUse curl.\n");
        let requires = Requires::from_front(&front);
        assert_eq!(requires.bins, ["curl"]);

        let (front, body) = front_matter("# No front matter\n");
        assert!(front.is_empty());
        assert_eq!(body, "# No front matter\n");
        let (front, _) = front_matter(
            "---\nname: x\ndescription: one\n  two\nmetadata: {\"clawdbot\":{\"os\":[\"darwin\"]}}\n---\n",
        );
        assert_eq!(front["description"], "one two");
        assert_eq!(Requires::from_front(&front).os, ["darwin"]);
    }

    #[test]
    fn trailing_commas_go_but_commas_in_strings_stay() {
        assert_eq!(
            without_trailing_commas(r#"{"a": ["x,]", "y",], }"#),
            r#"{"a": ["x,]", "y"]}"#
        );
    }

    #[test]
    fn scan_reports_each_state_and_the_prompt_lists_ready_ones() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join(DIR);
        write(
            &root.join("weather/SKILL.md"),
            "---\nname: weather\ndescription: Weather for a city.\n---\nRun {baseDir}/w.sh <city>.\n",
        );
        write(&root.join("weather/w.sh"), "#!/bin/sh\n");
        write(
            &root.join("needs/SKILL.md"),
            "---\nname: needs\ndescription: Needs things.\nmetadata: {\"openclaw\":{\"requires\":{\"bins\":[\"no-such-program-xyz\"],\"env\":[\"NO_SUCH_VAR_XYZ\"]}}}\n---\n",
        );
        write(&root.join("broken/SKILL.md"), "no front matter\n");
        write(
            &root.join("quiet/SKILL.md"),
            "---\ndescription: Turned off.\n---\n",
        );
        write(&root.join("not-a-skill/README.md"), "x");
        let skills = skills(dir.path(), &["quiet"]);
        let all = skills.scan();
        let states: Vec<(&str, &State)> = all.iter().map(|s| (s.name.as_str(), &s.state)).collect();
        assert_eq!(
            states,
            [
                (
                    "broken",
                    &State::Broken("SKILL.md has no description".into())
                ),
                (
                    "needs",
                    &State::Missing(vec![
                        "no-such-program-xyz".into(),
                        "$NO_SUCH_VAR_XYZ".into()
                    ])
                ),
                ("quiet", &State::Off),
                ("weather", &State::Ready),
            ]
        );

        let prompt = skills.prompt(false).unwrap();
        assert!(
            prompt.contains("- weather: Weather for a city."),
            "{prompt}"
        );
        assert!(
            !prompt.contains("needs") && !prompt.contains("SKILL.md"),
            "{prompt}"
        );
        assert!(
            skills
                .prompt(true)
                .unwrap()
                .contains("skills/<name>/SKILL.md")
        );

        let loaded = skills.load("weather").unwrap();
        let base = root.join("weather").display().to_string();
        assert!(
            loaded.contains(&format!("Run {base}/w.sh <city>.")),
            "{loaded}"
        );
        assert!(loaded.contains("Files: w.sh"), "{loaded}");
        assert!(!loaded.contains("description:"), "{loaded}");
        let err = skills.load("needs").unwrap_err().to_string();
        assert!(err.contains("no-such-program-xyz"), "{err}");
        let err = skills.load("nope").unwrap_err().to_string();
        assert!(err.contains("available: weather"), "{err}");
    }

    #[test]
    fn no_skills_and_no_writing_means_no_prompt() {
        let dir = tempfile::tempdir().unwrap();
        let skills = skills(dir.path(), &[]);
        assert_eq!(skills.prompt(false), None);
        assert!(skills.prompt(true).is_some());
    }

    #[test]
    fn install_copies_one_or_many_and_remove_deletes() {
        let dir = tempfile::tempdir().unwrap();
        let skills = skills(&dir.path().join("ws"), &[]);
        let source = dir.path().join("pack");
        write(
            &source.join("skills/a/SKILL.md"),
            "---\nname: alpha\ndescription: A.\n---\n",
        );
        write(&source.join("skills/a/tool.sh"), "echo");
        write(&source.join("skills/a/.git/HEAD"), "x");
        write(
            &source.join("skills/b/SKILL.md"),
            "---\ndescription: B.\n---\n",
        );
        let installed = install(&skills, source.to_str().unwrap(), false).unwrap();
        assert_eq!(installed, ["alpha", "b"]);
        assert!(skills.dir.join("alpha/tool.sh").is_file());
        assert!(!skills.dir.join("alpha/.git").exists());
        let err = install(&skills, source.to_str().unwrap(), false).unwrap_err();
        assert!(err.to_string().contains("--force"), "{err}");
        install(&skills, source.join("skills/a").to_str().unwrap(), true).unwrap();

        write(
            &source.join("bad/SKILL.md"),
            "---\nname: ../evil\ndescription: x\n---\n",
        );
        assert!(install(&skills, source.join("bad").to_str().unwrap(), false).is_err());
        assert!(install(&skills, dir.path().to_str().unwrap(), false).is_err());

        remove(&skills, "alpha").unwrap();
        assert!(!skills.dir.join("alpha").exists());
        assert!(remove(&skills, "alpha").is_err());
    }

    #[test]
    fn github_tree_links_become_repo_branch_and_path() {
        assert_eq!(
            parse_git_url("https://github.com/openclaw/openclaw/tree/main/skills/weather"),
            (
                "https://github.com/openclaw/openclaw".into(),
                Some("main".into()),
                Some("skills/weather".into())
            )
        );
        assert_eq!(
            parse_git_url("https://github.com/o/r/blob/dev/x/SKILL.md"),
            (
                "https://github.com/o/r".into(),
                Some("dev".into()),
                Some("x".into())
            )
        );
        assert_eq!(
            parse_git_url("https://github.com/o/r.git"),
            ("https://github.com/o/r.git".into(), None, None)
        );
    }
}
