//! `backup` and `restore`: the config, the three state databases (identity
//! and memories, chats, jobs and queues) and the workspace in one `.tar.gz`,
//! so the agent can move to another host or come back after a broken SD card.
//!
//! Databases are copied with `VACUUM INTO`, which gives a consistent snapshot
//! while the Gateway keeps running. Restoring checks the whole archive in a
//! staging directory before anything is replaced, and moves what it replaces
//! aside instead of deleting it.

use std::fs::File;
use std::io::{BufWriter, Write};
use std::path::{Component, Path, PathBuf};

use anyhow::{Context, Result, bail};
use rusqlite::{Connection, OpenFlags};
use serde::{Deserialize, Serialize};

use crate::config::Config;
use crate::i18n::{Tr, tr};

const MANIFEST: &str = "openclaw-backup.json";
const CONFIG: &str = "config.toml";
const WORKSPACE: &str = "workspace";
const DATABASES: [&str; 3] = ["soul.sqlite", "chats.sqlite", "runtime.sqlite"];
const FORMAT: u32 = 1;

const WROTE: Tr = tr("Backup written to {}", "备份已写入 {}");
const SECRETS: Tr = tr(
    "It holds config.toml with your keys and passwords: keep it private (it is only readable by you).",
    "里面有包含 key 和密码的 config.toml，请妥善保管（文件只有你自己能读）。",
);
const RESTORED: Tr = tr("Restored from {}", "已从 {} 恢复");
const MOVED_ASIDE: Tr = tr("What it replaced was moved to {}", "被替换的文件已移到 {}");
const ITEM_CONFIG: Tr = tr("config", "配置");
const ITEM_DATABASE: Tr = tr("database {}", "数据库 {}");
const ITEM_WORKSPACE: Tr = tr("workspace: {} file(s)", "工作区：{} 个文件");
const SKIPPED_LINKS: Tr = tr(
    "skipped {} symbolic link(s) or special file(s)",
    "跳过了 {} 个符号链接或特殊文件",
);
const EXISTS: Tr = tr(
    "{} already has a config or state; add --force to replace it (the current files are moved aside, not deleted)",
    "{} 里已经有配置或数据；加 --force 替换（现有文件会被移到一旁，不会删除）",
);
const RUNNING: Tr = tr(
    "a Gateway is running at {}: stop it first (doas rc-service openclaw-rs stop)",
    "{} 上有正在运行的 Gateway：请先停止（doas rc-service openclaw-rs stop）",
);
const NOT_A_BACKUP: Tr = tr(
    "{} is not an openclaw-rs backup (no {})",
    "{} 不是 openclaw-rs 的备份（没有 {}）",
);
const NEWER_FORMAT: Tr = tr(
    "{} was made by a newer openclaw-rs (format {}); update this one first",
    "{} 是更新版本的 openclaw-rs 做的（格式 {}）；请先升级",
);
const BAD_DATABASE: Tr = tr("{} in the backup is damaged: {}", "备份里的 {} 已损坏：{}");
const OUT_EXISTS: Tr = tr("{} already exists", "{} 已存在");

#[cfg(test)]
pub const ALL: &[Tr] = &[
    WROTE,
    SECRETS,
    RESTORED,
    MOVED_ASIDE,
    ITEM_CONFIG,
    ITEM_DATABASE,
    ITEM_WORKSPACE,
    SKIPPED_LINKS,
    EXISTS,
    RUNNING,
    NOT_A_BACKUP,
    NEWER_FORMAT,
    BAD_DATABASE,
    OUT_EXISTS,
];

/// Where this host keeps what a backup holds.
pub struct Paths {
    pub state: PathBuf,
    pub config: PathBuf,
    pub workspace: PathBuf,
}

impl Paths {
    /// The workspace comes from the config when it loads, else the default.
    pub fn new(state: &Path, config: &Path) -> Self {
        Self {
            state: state.to_owned(),
            config: config.to_owned(),
            workspace: workspace_of(Config::load(config).ok().as_ref(), state),
        }
    }
}

fn workspace_of(config: Option<&Config>, state: &Path) -> PathBuf {
    config
        .and_then(|c| c.tools.workspace.clone())
        .unwrap_or_else(|| state.join(WORKSPACE))
}

#[derive(Serialize, Deserialize)]
struct Manifest {
    format: u32,
    version: String,
    created: String,
    workspace: bool,
}

/// What a backup or restore covered, one line each.
#[derive(Debug)]
pub struct Summary {
    pub lines: Vec<String>,
}

impl Summary {
    pub fn print(&self) {
        for line in &self.lines {
            println!("{line}");
        }
    }
}

/// Default file name in the current directory.
pub fn default_name() -> PathBuf {
    PathBuf::from(format!(
        "openclaw-backup-{}.tar.gz",
        chrono::Local::now().format("%Y%m%d-%H%M%S")
    ))
}

pub fn backup(paths: &Paths, out: &Path, with_workspace: bool) -> Result<Summary> {
    if out.exists() {
        bail!(OUT_EXISTS.with(&[&out.display().to_string()]));
    }
    let scratch = paths.state.join(format!(".backup-{}", std::process::id()));
    std::fs::create_dir_all(&scratch)
        .with_context(|| format!("cannot create {}", scratch.display()))?;
    let tmp = out.with_file_name(format!(
        ".{}.tmp",
        out.file_name().and_then(|n| n.to_str()).unwrap_or("backup")
    ));
    let result = write_archive(paths, &tmp, &scratch, with_workspace);
    let _ = std::fs::remove_dir_all(&scratch);
    let mut items = match result {
        Ok(items) => items,
        Err(err) => {
            let _ = std::fs::remove_file(&tmp);
            return Err(err);
        }
    };
    std::fs::rename(&tmp, out).with_context(|| format!("cannot write {}", out.display()))?;
    let mut lines = vec![WROTE.with(&[&out.display().to_string()])];
    lines.extend(items.drain(..).map(|i| format!("  {i}")));
    lines.push(SECRETS.now().to_owned());
    Ok(Summary { lines })
}

fn write_archive(
    paths: &Paths,
    tmp: &Path,
    scratch: &Path,
    with_workspace: bool,
) -> Result<Vec<String>> {
    use std::os::unix::fs::OpenOptionsExt;
    let file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(tmp)
        .with_context(|| format!("cannot create {}", tmp.display()))?;
    let gz = flate2::write::GzEncoder::new(BufWriter::new(file), flate2::Compression::default());
    let mut tar = tar::Builder::new(gz);
    let mut items = Vec::new();

    let manifest = serde_json::to_vec_pretty(&Manifest {
        format: FORMAT,
        version: env!("CARGO_PKG_VERSION").to_owned(),
        created: chrono::Local::now().to_rfc3339(),
        workspace: with_workspace,
    })?;
    append_bytes(&mut tar, MANIFEST, &manifest, 0o644)?;

    if paths.config.is_file() {
        tar.append_path_with_name(&paths.config, CONFIG)
            .with_context(|| format!("cannot read {}", paths.config.display()))?;
        items.push(ITEM_CONFIG.now().to_owned());
    }
    for name in DATABASES {
        let live = paths.state.join(name);
        if !live.is_file() {
            continue;
        }
        let copy = scratch.join(name);
        snapshot(&live, &copy)?;
        tar.append_path_with_name(&copy, name)?;
        items.push(ITEM_DATABASE.with(&[name]));
    }
    if with_workspace && paths.workspace.is_dir() {
        let (files, skipped) = append_tree(&mut tar, &paths.workspace, Path::new(WORKSPACE))?;
        items.push(ITEM_WORKSPACE.with(&[&files.to_string()]));
        if skipped > 0 {
            items.push(SKIPPED_LINKS.with(&[&skipped.to_string()]));
        }
    }
    let gz = tar.into_inner()?;
    let mut writer = gz.finish()?;
    writer.flush()?;
    writer
        .into_inner()
        .map_err(|e| e.into_error())?
        .sync_all()?;
    Ok(items)
}

/// A consistent copy, even while the Gateway writes to `live`.
fn snapshot(live: &Path, copy: &Path) -> Result<()> {
    let conn = Connection::open_with_flags(live, OpenFlags::SQLITE_OPEN_READ_ONLY)
        .with_context(|| format!("cannot open {}", live.display()))?;
    conn.busy_timeout(std::time::Duration::from_secs(30))?;
    conn.execute("VACUUM INTO ?1", [copy.to_string_lossy()])
        .with_context(|| format!("cannot copy {}", live.display()))?;
    Ok(())
}

fn append_bytes<W: Write>(
    tar: &mut tar::Builder<W>,
    name: &str,
    data: &[u8],
    mode: u32,
) -> Result<()> {
    let mut header = tar::Header::new_gnu();
    header.set_size(data.len() as u64);
    header.set_mode(mode);
    header.set_mtime(chrono::Utc::now().timestamp().max(0) as u64);
    header.set_entry_type(tar::EntryType::Regular);
    tar.append_data(&mut header, name, data)?;
    Ok(())
}

/// Regular files and directories under `dir`; links and devices are left
/// out, since a link could point anywhere on the host where it is restored.
fn append_tree<W: Write>(
    tar: &mut tar::Builder<W>,
    dir: &Path,
    name: &Path,
) -> Result<(usize, usize)> {
    let (mut files, mut skipped) = (0, 0);
    let mut entries: Vec<_> = std::fs::read_dir(dir)
        .with_context(|| format!("cannot read {}", dir.display()))?
        .collect::<std::io::Result<_>>()?;
    entries.sort_by_key(|e| e.file_name());
    tar.append_dir(name, dir)?;
    for entry in entries {
        let kind = entry.file_type()?;
        let path = entry.path();
        let inner = name.join(entry.file_name());
        if kind.is_dir() {
            let (f, s) = append_tree(tar, &path, &inner)?;
            files += f;
            skipped += s;
        } else if kind.is_file() {
            tar.append_path_with_name(&path, &inner)
                .with_context(|| format!("cannot read {}", path.display()))?;
            files += 1;
        } else {
            skipped += 1;
        }
    }
    Ok((files, skipped))
}

/// Refuses while a Gateway listens on `bind`: it would keep writing to the
/// databases being replaced.
pub fn ensure_stopped(bind: &str) -> Result<()> {
    let Ok(addr) = bind.trim().parse::<std::net::SocketAddr>() else {
        return Ok(());
    };
    let probe = if addr.ip().is_unspecified() {
        std::net::SocketAddr::new(std::net::Ipv4Addr::LOCALHOST.into(), addr.port())
    } else {
        addr
    };
    if std::net::TcpStream::connect_timeout(&probe, std::time::Duration::from_millis(500)).is_ok() {
        bail!(RUNNING.with(&[&probe.to_string()]));
    }
    Ok(())
}

pub fn restore(paths: &Paths, archive: &Path, force: bool) -> Result<Summary> {
    let staging = paths.state.join(format!(".restore-{}", std::process::id()));
    std::fs::create_dir_all(&paths.state)
        .with_context(|| format!("cannot create {}", paths.state.display()))?;
    let _ = std::fs::remove_dir_all(&staging);
    std::fs::create_dir_all(&staging)?;
    let result = restore_from(paths, archive, &staging, force);
    let _ = std::fs::remove_dir_all(&staging);
    result
}

fn restore_from(paths: &Paths, archive: &Path, staging: &Path, force: bool) -> Result<Summary> {
    let shown = archive.display().to_string();
    let skipped = unpack(archive, staging)?;

    let manifest: Manifest = std::fs::read(staging.join(MANIFEST))
        .ok()
        .and_then(|data| serde_json::from_slice(&data).ok())
        .with_context(|| NOT_A_BACKUP.with(&[&shown, MANIFEST]))?;
    if manifest.format > FORMAT {
        bail!(NEWER_FORMAT.with(&[&shown, &manifest.format.to_string()]));
    }
    for name in DATABASES {
        let path = staging.join(name);
        if path.is_file() {
            check_database(&path)
                .map_err(|e| anyhow::anyhow!(BAD_DATABASE.with(&[name, &format!("{e:#}")])))?;
        }
    }
    let staged_config = staging.join(CONFIG);
    let config = if staged_config.is_file() {
        Some(Config::load(&staged_config)?)
    } else {
        None
    };
    let staged_workspace = staging.join(WORKSPACE);
    let has_workspace = staged_workspace.is_dir();
    // The restored config decides where its workspace lives.
    let workspace = match &config {
        Some(config) => workspace_of(Some(config), &paths.state),
        None => paths.workspace.clone(),
    };

    let occupied = paths.config.exists()
        || DATABASES.iter().any(|n| paths.state.join(n).exists())
        || (has_workspace && dir_has_files(&workspace));
    if occupied && !force {
        bail!(EXISTS.with(&[&paths.state.display().to_string()]));
    }

    let aside = paths.state.join(format!(
        "before-restore-{}",
        chrono::Local::now().format("%Y%m%d-%H%M%S")
    ));
    let mut moved = false;
    let mut lines = vec![RESTORED.with(&[&shown])];
    if config.is_some() {
        moved |= place(&staged_config, &paths.config, &aside.join(CONFIG))?;
        set_private(&paths.config)?;
        lines.push(format!("  {}", ITEM_CONFIG.now()));
    }
    for name in DATABASES {
        let staged = staging.join(name);
        if !staged.is_file() {
            continue;
        }
        let live = paths.state.join(name);
        // The journal of the database being replaced must not be replayed into the new one.
        for suffix in ["-wal", "-shm", "-journal"] {
            let side = paths.state.join(format!("{name}{suffix}"));
            moved |= place_aside(&side, &aside.join(format!("{name}{suffix}")))?;
        }
        moved |= place(&staged, &live, &aside.join(name))?;
        lines.push(format!("  {}", ITEM_DATABASE.with(&[name])));
    }
    if has_workspace {
        let (files, replaced) = merge_tree(&staged_workspace, &workspace, &aside.join(WORKSPACE))?;
        moved |= replaced;
        lines.push(format!("  {}", ITEM_WORKSPACE.with(&[&files.to_string()])));
    }
    if skipped > 0 {
        lines.push(format!("  {}", SKIPPED_LINKS.with(&[&skipped.to_string()])));
    }
    if moved {
        lines.push(MOVED_ASIDE.with(&[&aside.display().to_string()]));
    }
    Ok(Summary { lines })
}

/// Unpacks only what a backup holds, as plain files and directories under
/// `staging`; returns how many other entries (links, devices) were skipped.
fn unpack(archive: &Path, staging: &Path) -> Result<usize> {
    let file = File::open(archive).with_context(|| format!("cannot open {}", archive.display()))?;
    let mut tar = tar::Archive::new(flate2::read::GzDecoder::new(std::io::BufReader::new(file)));
    let mut skipped = 0;
    let entries = tar
        .entries()
        .with_context(|| format!("{} is not a .tar.gz file", archive.display()))?;
    for entry in entries {
        let mut entry = entry
            .with_context(|| format!("{} is damaged or not a .tar.gz file", archive.display()))?;
        let path = entry.path()?.into_owned();
        let Some(relative) = allowed_path(&path) else {
            skipped += 1;
            continue;
        };
        let target = staging.join(relative);
        match entry.header().entry_type() {
            tar::EntryType::Directory => std::fs::create_dir_all(&target)?,
            tar::EntryType::Regular | tar::EntryType::Continuous => {
                if let Some(parent) = target.parent() {
                    std::fs::create_dir_all(parent)?;
                }
                let mut out = File::create(&target)
                    .with_context(|| format!("cannot write {}", target.display()))?;
                std::io::copy(&mut entry, &mut out)
                    .with_context(|| format!("{} is damaged", archive.display()))?;
            }
            _ => skipped += 1,
        }
    }
    Ok(skipped)
}

/// The entry's path when it is one a backup writes: the manifest, the config,
/// a database or something under `workspace/`, with no `..` or root.
fn allowed_path(path: &Path) -> Option<PathBuf> {
    let mut parts = Vec::new();
    for component in path.components() {
        match component {
            Component::Normal(part) => parts.push(part),
            Component::CurDir => {}
            _ => return None,
        }
    }
    let first = parts.first()?.to_str()?;
    let single = parts.len() == 1;
    let known = (single && (first == MANIFEST || first == CONFIG || DATABASES.contains(&first)))
        || first == WORKSPACE;
    known.then(|| parts.iter().collect())
}

fn check_database(path: &Path) -> Result<()> {
    let conn = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    let verdict: String = conn.query_row("PRAGMA integrity_check", [], |r| r.get(0))?;
    if verdict != "ok" {
        bail!("{verdict}");
    }
    Ok(())
}

fn dir_has_files(dir: &Path) -> bool {
    std::fs::read_dir(dir).is_ok_and(|mut entries| entries.next().is_some())
}

/// Moves `from` to `to`, first moving an existing `to` to `aside`.
/// Returns whether something was moved aside.
fn place(from: &Path, to: &Path, aside: &Path) -> Result<bool> {
    let moved = place_aside(to, aside)?;
    if let Some(parent) = to.parent().filter(|p| !p.as_os_str().is_empty()) {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("cannot create {}", parent.display()))?;
    }
    move_file(from, to)?;
    Ok(moved)
}

fn place_aside(path: &Path, aside: &Path) -> Result<bool> {
    if std::fs::symlink_metadata(path).is_err() {
        return Ok(false);
    }
    if let Some(parent) = aside.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("cannot create {}", parent.display()))?;
    }
    move_file(path, aside)?;
    Ok(true)
}

/// A rename, or a copy when `to` is on another filesystem.
fn move_file(from: &Path, to: &Path) -> Result<()> {
    if std::fs::rename(from, to).is_ok() {
        return Ok(());
    }
    std::fs::copy(from, to).with_context(|| format!("cannot write {}", to.display()))?;
    std::fs::remove_file(from).with_context(|| format!("cannot remove {}", from.display()))
}

/// Files from `from` into `to`; files it replaces go to `aside`, others in
/// `to` stay. Returns the file count and whether anything was moved aside.
fn merge_tree(from: &Path, to: &Path, aside: &Path) -> Result<(usize, bool)> {
    std::fs::create_dir_all(to).with_context(|| format!("cannot create {}", to.display()))?;
    let (mut files, mut moved) = (0, false);
    for entry in std::fs::read_dir(from)? {
        let entry = entry?;
        let (src, dst) = (entry.path(), to.join(entry.file_name()));
        let parked = aside.join(entry.file_name());
        if entry.file_type()?.is_dir() {
            if std::fs::symlink_metadata(&dst).is_ok_and(|m| !m.is_dir()) {
                moved |= place_aside(&dst, &parked)?;
            }
            let (f, m) = merge_tree(&src, &dst, &parked)?;
            files += f;
            moved |= m;
        } else {
            if std::fs::symlink_metadata(&dst).is_ok_and(|m| m.is_dir()) {
                if let Some(parent) = parked.parent() {
                    std::fs::create_dir_all(parent)?;
                }
                std::fs::rename(&dst, &parked)
                    .with_context(|| format!("cannot move {} aside", dst.display()))?;
                moved = true;
            }
            moved |= place(&src, &dst, &parked)?;
            files += 1;
        }
    }
    Ok((files, moved))
}

fn set_private(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
        .with_context(|| format!("cannot protect {}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::Store;

    fn host(root: &Path) -> Paths {
        Paths {
            state: root.join("state"),
            config: root.join("state/config.toml"),
            workspace: root.join("state/workspace"),
        }
    }

    #[test]
    fn texts_have_both_languages() {
        crate::i18n::assert_complete(ALL);
    }

    #[test]
    fn a_backup_restores_on_another_host() {
        let dir = tempfile::tempdir().unwrap();
        let old = host(&dir.path().join("old"));
        std::fs::create_dir_all(old.workspace.join("notes")).unwrap();
        std::fs::write(&old.config, "[model]\nmodel = \"x/y\"\n").unwrap();
        std::fs::write(old.workspace.join("notes/a.md"), "hello").unwrap();
        std::os::unix::fs::symlink("/etc/passwd", old.workspace.join("link")).unwrap();
        {
            // Open while the backup runs, as a running Gateway would be.
            let store = Store::open(&old.state).unwrap();
            store.memory_save("likes tea").unwrap();
            let file = dir.path().join("b.tar.gz");
            let summary = backup(&old, &file, true).unwrap();
            let text = summary.lines.join("\n");
            assert!(text.contains("soul.sqlite") && text.contains("1"), "{text}");
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&file).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600);
            assert!(backup(&old, &file, true).is_err(), "never overwrites");

            let new = host(&dir.path().join("new"));
            let summary = restore(&new, &file, false).unwrap();
            assert!(!summary.lines.iter().any(|l| l.contains("before-restore")));
            let store = Store::open(&new.state).unwrap();
            let found = store.memory_search("tea", 5).unwrap();
            assert_eq!(found.len(), 1);
            assert_eq!(
                std::fs::read_to_string(new.workspace.join("notes/a.md")).unwrap(),
                "hello"
            );
            assert!(std::fs::symlink_metadata(new.workspace.join("link")).is_err());
            let mode = std::fs::metadata(&new.config).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600);
            assert!(!new.state.read_dir().unwrap().any(|e| {
                e.unwrap()
                    .file_name()
                    .to_string_lossy()
                    .starts_with(".restore")
            }));

            // Over existing state only with --force, which keeps the old files.
            std::fs::write(new.workspace.join("notes/a.md"), "changed").unwrap();
            std::fs::write(new.workspace.join("mine.txt"), "keep").unwrap();
            assert!(
                restore(&new, &file, false)
                    .unwrap_err()
                    .to_string()
                    .contains("--force")
            );
            let summary = restore(&new, &file, true).unwrap();
            let aside = summary.lines.last().unwrap();
            assert!(aside.contains("before-restore"), "{aside}");
            let aside_dir = std::fs::read_dir(&new.state)
                .unwrap()
                .map(|e| e.unwrap().path())
                .find(|p| p.to_string_lossy().contains("before-restore"))
                .unwrap();
            assert_eq!(
                std::fs::read_to_string(aside_dir.join("workspace/notes/a.md")).unwrap(),
                "changed"
            );
            assert!(aside_dir.join("soul.sqlite").is_file());
            assert_eq!(
                std::fs::read_to_string(new.workspace.join("notes/a.md")).unwrap(),
                "hello"
            );
            assert_eq!(
                std::fs::read_to_string(new.workspace.join("mine.txt")).unwrap(),
                "keep"
            );
        }
    }

    #[test]
    fn hostile_or_foreign_archives_are_refused() {
        let dir = tempfile::tempdir().unwrap();
        let paths = host(dir.path());
        // Paths outside the backup layout are never written.
        let file = dir.path().join("evil.tar.gz");
        {
            let gz = flate2::write::GzEncoder::new(
                File::create(&file).unwrap(),
                flate2::Compression::fast(),
            );
            let mut tar = tar::Builder::new(gz);
            let manifest = br#"{"format":1,"version":"0","created":"x","workspace":true}"#;
            append_bytes(&mut tar, MANIFEST, manifest, 0o644).unwrap();
            append_bytes(&mut tar, "other.txt", b"x", 0o644).unwrap();
            let mut header = tar::Header::new_gnu();
            header.set_size(1);
            header.set_mode(0o644);
            header.set_entry_type(tar::EntryType::Regular);
            // `append_data` refuses `..`, so write the name into the header directly.
            header.as_gnu_mut().unwrap().name[..16].copy_from_slice(b"workspace/../../");
            header.set_cksum();
            tar.append(&header, &b"x"[..]).unwrap();
            let mut link = tar::Header::new_gnu();
            link.set_entry_type(tar::EntryType::Symlink);
            link.set_size(0);
            tar.append_link(&mut link, "workspace/l", "/etc").unwrap();
            tar.into_inner().unwrap().finish().unwrap();
        }
        let summary = restore(&paths, &file, false).unwrap();
        assert!(
            summary.lines.iter().any(|l| l.contains('3')),
            "{:?}",
            summary.lines
        );
        assert!(!dir.path().join("other.txt").exists());
        assert!(!paths.state.join("other.txt").exists());
        assert!(std::fs::symlink_metadata(paths.workspace.join("l")).is_err());

        let plain = dir.path().join("plain.tar.gz");
        std::fs::write(&plain, b"not a backup").unwrap();
        assert!(restore(&paths, &plain, true).is_err());
    }

    #[test]
    fn archive_paths_are_checked() {
        for ok in [
            "config.toml",
            "soul.sqlite",
            "./workspace/a/b.txt",
            "workspace",
        ] {
            assert!(allowed_path(Path::new(ok)).is_some(), "{ok}");
        }
        for bad in [
            "/etc/passwd",
            "../x",
            "workspace/../../x",
            "config.toml/x",
            "other",
            "soul.sqlite-wal",
        ] {
            assert!(allowed_path(Path::new(bad)).is_none(), "{bad}");
        }
    }
}
