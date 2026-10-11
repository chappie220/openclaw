//! Updates from GitHub Releases: `openclaw-rs update` by hand, or the
//! Gateway on its own with `update.auto`.
//!
//! A release carries one binary per target (`openclaw-rs-<target>`) and a
//! `SHA256SUMS` file. The download must match its checksum and run
//! `--version` before it replaces the current binary, which is kept as
//! `<binary>.old` for `update --rollback`. The Gateway restarts itself by
//! executing the new binary in place once no turn is running, so the
//! process (and the OpenRC supervisor's view of it) stays the same.

use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::i18n::{Tr, tr};

pub const VERSION: &str = env!("CARGO_PKG_VERSION");
/// The target triple this binary was built for.
pub const TARGET: &str = env!("OPENCLAW_TARGET");
const SUMS: &str = "SHA256SUMS";
const MAX_BINARY_BYTES: usize = 64 * 1024 * 1024;

/// `[update]`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct UpdateConfig {
    /// The Gateway looks for a new release every `check_hours`.
    pub check: bool,
    /// It also installs it and restarts itself once no turn is running.
    pub auto: bool,
    pub check_hours: u64,
    /// GitHub repository the releases come from (`owner/name`).
    pub repo: String,
    /// GitHub API origin; for GitHub Enterprise or tests.
    pub api_url: String,
}

impl Default for UpdateConfig {
    fn default() -> Self {
        Self {
            check: true,
            auto: false,
            check_hours: 24,
            repo: "chappie220/openclaw".into(),
            api_url: "https://api.github.com".into(),
        }
    }
}

const CURRENT: Tr = tr("This is openclaw-rs {} ({})", "当前是 openclaw-rs {}（{}）");
const LATEST: Tr = tr("Latest release: {}", "最新版本：{}");
const UP_TO_DATE: Tr = tr("Already up to date.", "已经是最新版本。");
const AVAILABLE: Tr = tr(
    "Version {} is available: run `openclaw-rs update` to install it.",
    "有新版本 {}：运行 `openclaw-rs update` 安装。",
);
const DOWNLOADING: Tr = tr("Downloading {} ...", "正在下载 {} ...");
const UPDATED: Tr = tr(
    "Updated {} to {}; the previous binary is kept as {}.",
    "已把 {} 更新到 {}；旧版本保存在 {}。",
);
const RESTARTED: Tr = tr("Restarted the service.", "已重启服务。");
const RESTART_HINT: Tr = tr(
    "A Gateway is running: restart it to use the new version (doas rc-service openclaw-rs restart).",
    "Gateway 正在运行：重启后才会用上新版本（doas rc-service openclaw-rs restart）。",
);
const ROLLED_BACK: Tr = tr(
    "Rolled back: {} is the previous binary again ({}).",
    "已回退：{} 恢复为上一个版本（{}）。",
);
const NO_OLD: Tr = tr("No previous binary at {}.", "没有找到旧版本 {}。");
const NO_ASSET: Tr = tr(
    "Release {} has no binary for {} (expected {}).",
    "版本 {} 里没有适用于 {} 的程序（应为 {}）。",
);
const NOT_WRITABLE: Tr = tr(
    "Cannot replace {}: {}. Run the update as the binary's owner (doas openclaw-rs update), or keep the binary where the service account can write it.",
    "无法替换 {}：{}。请用程序文件的所有者运行（doas openclaw-rs update），或把程序放在服务账号能写入的位置。",
);

#[cfg(test)]
pub const ALL: &[Tr] = &[
    CURRENT,
    LATEST,
    UP_TO_DATE,
    AVAILABLE,
    DOWNLOADING,
    UPDATED,
    RESTARTED,
    RESTART_HINT,
    ROLLED_BACK,
    NO_OLD,
    NO_ASSET,
    NOT_WRITABLE,
];

/// The newest release.
#[derive(Debug, Clone, PartialEq)]
pub struct Release {
    pub version: String,
    /// Asset name and download URL.
    assets: Vec<(String, String)>,
}

impl Release {
    pub fn is_newer(&self) -> bool {
        newer(&self.version, VERSION)
    }

    /// Whether it has a binary for this target.
    pub fn has_binary(&self) -> bool {
        self.asset(&asset_name()).is_some()
    }

    fn asset(&self, name: &str) -> Option<&str> {
        self.assets
            .iter()
            .find(|(n, _)| n == name)
            .map(|(_, url)| url.as_str())
    }
}

/// The asset name of this target's binary.
pub fn asset_name() -> String {
    format!("openclaw-rs-{TARGET}")
}

/// Whether version `a` is newer than `b` (`1.2.10` > `1.2.9`; a
/// pre-release such as `1.3.0-rc1` is older than `1.3.0`).
pub fn newer(a: &str, b: &str) -> bool {
    fn parts(v: &str) -> (Vec<u64>, bool) {
        let v = v.trim().trim_start_matches('v');
        let (core, pre) = match v.split_once(['-', '+']) {
            Some((core, rest)) => (core, v.as_bytes()[core.len()] == b'-' && !rest.is_empty()),
            None => (v, false),
        };
        let mut numbers: Vec<u64> = core.split('.').map(|n| n.parse().unwrap_or(0)).collect();
        numbers.resize(3, 0);
        (numbers, pre)
    }
    let (a_core, a_pre) = parts(a);
    let (b_core, b_pre) = parts(b);
    match a_core.cmp(&b_core) {
        std::cmp::Ordering::Greater => true,
        std::cmp::Ordering::Less => false,
        std::cmp::Ordering::Equal => b_pre && !a_pre,
    }
}

fn http() -> Result<reqwest::Client> {
    reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(20))
        .timeout(Duration::from_secs(300))
        .user_agent(format!("openclaw-rs/{VERSION}"))
        .build()
        .context("cannot build HTTP client for updates")
}

/// The latest release of `config.repo`.
pub async fn latest(config: &UpdateConfig) -> Result<Release> {
    let url = format!(
        "{}/repos/{}/releases/latest",
        config.api_url.trim_end_matches('/'),
        config.repo.trim_matches('/')
    );
    let response = http()?
        .get(&url)
        .header("Accept", "application/vnd.github+json")
        .send()
        .await
        .with_context(|| format!("cannot reach {url}"))?;
    let status = response.status();
    if status == reqwest::StatusCode::NOT_FOUND {
        bail!("{} has no published release yet", config.repo);
    }
    let body: Value = response
        .json()
        .await
        .with_context(|| format!("{url} did not answer with JSON"))?;
    if !status.is_success() {
        let message = body
            .get("message")
            .and_then(Value::as_str)
            .unwrap_or_default();
        bail!("{url} answered HTTP {status}: {message}");
    }
    let version = body
        .get("tag_name")
        .and_then(Value::as_str)
        .context("the release has no tag")?
        .trim_start_matches('v')
        .to_owned();
    let assets = body
        .get("assets")
        .and_then(Value::as_array)
        .map(|assets| {
            assets
                .iter()
                .filter_map(|a| {
                    Some((
                        a.get("name")?.as_str()?.to_owned(),
                        a.get("browser_download_url")?.as_str()?.to_owned(),
                    ))
                })
                .collect()
        })
        .unwrap_or_default();
    Ok(Release { version, assets })
}

async fn download(client: &reqwest::Client, url: &str) -> Result<Vec<u8>> {
    let mut response = client
        .get(url)
        .send()
        .await
        .with_context(|| format!("cannot download {url}"))?;
    if !response.status().is_success() {
        bail!("{url} answered HTTP {}", response.status());
    }
    let mut data = Vec::new();
    while let Some(chunk) = response.chunk().await.context("download interrupted")? {
        data.extend_from_slice(&chunk);
        if data.len() > MAX_BINARY_BYTES {
            bail!("{url} is larger than {} MB", MAX_BINARY_BYTES / 1024 / 1024);
        }
    }
    Ok(data)
}

pub fn sha256_hex(data: &[u8]) -> String {
    let digest = aws_lc_rs::digest::digest(&aws_lc_rs::digest::SHA256, data);
    digest.as_ref().iter().map(|b| format!("{b:02x}")).collect()
}

/// The checksum `sums` (a `sha256sum` listing) gives for `name`.
fn expected_sum<'a>(sums: &'a str, name: &str) -> Option<&'a str> {
    sums.lines().find_map(|line| {
        let (sum, file) = line.trim().split_once(char::is_whitespace)?;
        (file.trim().trim_start_matches('*') == name).then_some(sum)
    })
}

/// Downloads this target's binary of `release` and checks its checksum.
pub async fn fetch(release: &Release) -> Result<Vec<u8>> {
    let name = asset_name();
    let url = release
        .asset(&name)
        .with_context(|| NO_ASSET.with(&[&release.version, TARGET, &name]))?;
    let sums_url = release
        .asset(SUMS)
        .with_context(|| format!("release {} has no {SUMS}", release.version))?;
    let client = http()?;
    let sums = String::from_utf8_lossy(&download(&client, sums_url).await?).into_owned();
    let expected = expected_sum(&sums, &name)
        .with_context(|| format!("{SUMS} of release {} does not list {name}", release.version))?
        .to_ascii_lowercase();
    let data = download(&client, url).await?;
    let actual = sha256_hex(&data);
    if actual != expected {
        bail!("{name} does not match its checksum (got {actual}, expected {expected})");
    }
    Ok(data)
}

pub fn old_path(binary: &Path) -> PathBuf {
    let mut name = binary.file_name().unwrap_or_default().to_owned();
    name.push(".old");
    binary.with_file_name(name)
}

/// Replaces `binary` with `data` once it runs and reports `version`;
/// the current binary is kept as `<binary>.old`.
pub fn install(binary: &Path, data: &[u8], version: &str) -> Result<()> {
    use std::io::Write;
    use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
    let dir = binary.parent().context("the binary has no directory")?;
    let staged = dir.join(format!(".openclaw-rs-update-{}", std::process::id()));
    let not_writable = |err: &dyn std::fmt::Display| {
        NOT_WRITABLE.with(&[&binary.display().to_string(), &err.to_string()])
    };
    let result = (|| -> Result<()> {
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o755)
            .open(&staged)
            .map_err(|e| anyhow::anyhow!(not_writable(&e)))?;
        file.write_all(data)?;
        file.sync_all()?;
        drop(file);
        std::fs::set_permissions(&staged, std::fs::Permissions::from_mode(0o755))?;
        let output = std::process::Command::new(&staged)
            .arg("--version")
            .output()
            .context("the new binary does not run on this host")?;
        let said = String::from_utf8_lossy(&output.stdout);
        if !output.status.success() || !said.contains(version) {
            bail!(
                "the new binary does not run as version {version} (it said {:?})",
                said.trim()
            );
        }
        let old = old_path(binary);
        std::fs::copy(binary, &old).map_err(|e| anyhow::anyhow!(not_writable(&e)))?;
        std::fs::rename(&staged, binary).map_err(|e| anyhow::anyhow!(not_writable(&e)))?;
        Ok(())
    })();
    let _ = std::fs::remove_file(&staged);
    result
}

/// Puts `<binary>.old` back, keeping the replaced one as `.old` instead.
pub fn rollback(binary: &Path) -> Result<String> {
    let old = old_path(binary);
    if !old.is_file() {
        bail!(NO_OLD.with(&[&old.display().to_string()]));
    }
    let output = std::process::Command::new(&old)
        .arg("--version")
        .output()
        .context("the previous binary does not run")?;
    let version = String::from_utf8_lossy(&output.stdout).trim().to_owned();
    let swap = binary.with_file_name(format!(".openclaw-rs-rollback-{}", std::process::id()));
    let fail = |e: std::io::Error| {
        anyhow::anyhow!(NOT_WRITABLE.with(&[&binary.display().to_string(), &e.to_string()]))
    };
    std::fs::rename(binary, &swap).map_err(fail)?;
    std::fs::rename(&old, binary).map_err(fail)?;
    std::fs::rename(&swap, &old).map_err(fail)?;
    Ok(version)
}

/// `openclaw-rs update`.
pub async fn run(
    config: &UpdateConfig,
    check_only: bool,
    rollback_only: bool,
    bind: &str,
) -> Result<()> {
    let binary = std::env::current_exe().context("cannot find this binary")?;
    if rollback_only {
        let version = rollback(&binary)?;
        println!(
            "{}",
            ROLLED_BACK.with(&[&binary.display().to_string(), &version])
        );
        restart_service(bind);
        return Ok(());
    }
    println!("{}", CURRENT.with(&[VERSION, TARGET]));
    let release = latest(config).await?;
    println!("{}", LATEST.with(&[&release.version]));
    if !release.is_newer() {
        println!("{}", UP_TO_DATE.now());
        return Ok(());
    }
    if check_only {
        println!("{}", AVAILABLE.with(&[&release.version]));
        return Ok(());
    }
    println!("{}", DOWNLOADING.with(&[&asset_name()]));
    let data = fetch(&release).await?;
    install(&binary, &data, &release.version)?;
    println!(
        "{}",
        UPDATED.with(&[
            &binary.display().to_string(),
            &release.version,
            &old_path(&binary).display().to_string()
        ])
    );
    restart_service(bind);
    Ok(())
}

/// Restarts the OpenRC service when this is root and it is running;
/// otherwise says to restart a running Gateway.
fn restart_service(bind: &str) {
    // SAFETY: geteuid has no preconditions.
    let root = unsafe { libc::geteuid() } == 0;
    let service_running = || {
        std::process::Command::new("rc-service")
            .args(["openclaw-rs", "status"])
            .output()
            .is_ok_and(|o| o.status.success())
    };
    if root && Path::new("/etc/init.d/openclaw-rs").exists() && service_running() {
        let restarted = std::process::Command::new("rc-service")
            .args(["openclaw-rs", "restart"])
            .status()
            .is_ok_and(|s| s.success());
        if restarted {
            println!("{}", RESTARTED.now());
            return;
        }
    }
    if crate::backup::ensure_stopped(bind).is_err() {
        println!("{}", RESTART_HINT.now());
    }
}

/// What the Gateway does with updates, in the background.
pub async fn watch(config: UpdateConfig, idle: impl Fn() -> bool + Send + 'static) {
    if !config.check {
        return;
    }
    // Resolved now: once replaced, /proc/self/exe names a deleted file.
    let Ok(binary) = std::env::current_exe() else {
        return;
    };
    let every = Duration::from_secs(config.check_hours.max(1) * 3600);
    let mut announced = String::new();
    tokio::time::sleep(Duration::from_secs(600)).await;
    loop {
        match latest(&config).await {
            Ok(release) if release.is_newer() && config.auto => {
                match fetch(&release)
                    .await
                    .and_then(|data| install(&binary, &data, &release.version))
                {
                    Ok(()) => {
                        eprintln!(
                            "update: installed {} (was {VERSION}); restarting once no turn is running",
                            release.version
                        );
                        while !idle() {
                            tokio::time::sleep(Duration::from_secs(30)).await;
                        }
                        restart(&binary);
                    }
                    Err(err) => eprintln!("update: cannot install {}: {err:#}", release.version),
                }
            }
            Ok(release) if release.is_newer() && announced != release.version => {
                eprintln!(
                    "update: version {} is available (this is {VERSION}); run `openclaw-rs update`, or set update.auto = true",
                    release.version
                );
                announced = release.version;
            }
            Ok(_) => {}
            Err(err) => eprintln!("update: cannot check for a new version: {err:#}"),
        }
        tokio::time::sleep(every).await;
    }
}

/// Runs `binary` in place of this process with the same arguments, after
/// ending the process groups of its children (browser, MCP servers).
fn restart(binary: &Path) {
    use std::os::unix::process::CommandExt;
    end_children();
    let err = std::process::Command::new(binary)
        .args(std::env::args_os().skip(1))
        .exec();
    eprintln!("update: cannot restart into {}: {err}", binary.display());
}

fn end_children() {
    let me = std::process::id().to_string();
    let Ok(entries) = std::fs::read_dir("/proc") else {
        return;
    };
    let mut children = Vec::new();
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(pid) = name.to_str().and_then(|n| n.parse::<i32>().ok()) else {
            continue;
        };
        let Ok(stat) = std::fs::read_to_string(entry.path().join("stat")) else {
            continue;
        };
        // After the command name in parentheses: state, then the parent pid.
        let ppid = stat
            .rsplit_once(')')
            .and_then(|(_, rest)| rest.split_whitespace().nth(1));
        if ppid == Some(me.as_str()) {
            children.push(pid);
        }
    }
    for pid in &children {
        // SAFETY: kill has no memory preconditions; a negative pid names the
        // child's process group (each child leads its own), else the child.
        unsafe {
            if libc::kill(-pid, libc::SIGTERM) != 0 {
                libc::kill(*pid, libc::SIGTERM);
            }
        }
    }
    if !children.is_empty() {
        std::thread::sleep(Duration::from_secs(2));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn texts_have_both_languages() {
        crate::i18n::assert_complete(ALL);
    }

    #[test]
    fn versions_compare_by_number() {
        assert!(newer("0.1.1", "0.1.0"));
        assert!(newer("v0.2.0", "0.1.9"));
        assert!(newer("1.2.10", "1.2.9"));
        assert!(newer("1.0", "0.9.9"));
        assert!(!newer("0.1.0", "0.1.0"));
        assert!(!newer("0.1.0", "0.2.0"));
        assert!(!newer("1.3.0-rc1", "1.3.0"));
        assert!(newer("1.3.0", "1.3.0-rc1"));
        assert!(!newer("1.3.0+build", "1.3.0"));
    }

    #[test]
    fn checksums_are_found_by_name() {
        let sums = "abc  openclaw-rs-x86_64-unknown-linux-musl\ndef *openclaw-rs-aarch64-unknown-linux-musl\n";
        assert_eq!(
            expected_sum(sums, "openclaw-rs-aarch64-unknown-linux-musl"),
            Some("def")
        );
        assert_eq!(expected_sum(sums, "openclaw-rs-armv7"), None);
        assert_eq!(
            sha256_hex(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    /// A stand-in for the GitHub API and its downloads.
    async fn github(binary: Vec<u8>, sum: String) -> UpdateConfig {
        use axum::{Json, Router, routing::get};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let name = asset_name();
        let release = serde_json::json!({
            "tag_name": "v99.0.0",
            "assets": [
                {"name": name, "browser_download_url": format!("{base}/download/bin")},
                {"name": SUMS, "browser_download_url": format!("{base}/download/sums")},
            ],
        });
        let sums = format!("{sum}  {name}\n");
        let app = Router::new()
            .route(
                "/repos/o/r/releases/latest",
                get(move || async move { Json(release) }),
            )
            .route("/download/bin", get(move || async move { binary }))
            .route("/download/sums", get(move || async move { sums }));
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        UpdateConfig {
            repo: "o/r".into(),
            api_url: base,
            ..UpdateConfig::default()
        }
    }

    const NEW_BINARY: &[u8] = b"#!/bin/sh\necho openclaw-rs 99.0.0\n";

    #[tokio::test]
    async fn a_release_is_downloaded_checked_installed_and_rolled_back() {
        let config = github(NEW_BINARY.to_vec(), sha256_hex(NEW_BINARY)).await;
        let release = latest(&config).await.unwrap();
        assert_eq!(release.version, "99.0.0");
        assert!(release.is_newer());
        let data = fetch(&release).await.unwrap();

        let dir = tempfile::tempdir().unwrap();
        let binary = dir.path().join("openclaw-rs");
        std::fs::write(&binary, "#!/bin/sh\necho openclaw-rs 0.1.0\n").unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&binary, std::fs::Permissions::from_mode(0o755)).unwrap();
        install(&binary, &data, &release.version).unwrap();
        assert_eq!(std::fs::read(&binary).unwrap(), NEW_BINARY);
        assert!(
            std::fs::read_to_string(old_path(&binary))
                .unwrap()
                .contains("0.1.0")
        );

        assert_eq!(rollback(&binary).unwrap(), "openclaw-rs 0.1.0");
        assert!(std::fs::read_to_string(&binary).unwrap().contains("0.1.0"));
        assert_eq!(std::fs::read(old_path(&binary)).unwrap(), NEW_BINARY);
        // Only the two binaries are left.
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 2);
    }

    #[tokio::test]
    async fn a_bad_checksum_or_a_binary_that_does_not_run_is_refused() {
        let config = github(NEW_BINARY.to_vec(), sha256_hex(b"something else")).await;
        let release = latest(&config).await.unwrap();
        let err = fetch(&release).await.unwrap_err().to_string();
        assert!(err.contains("checksum"), "{err}");

        let dir = tempfile::tempdir().unwrap();
        let binary = dir.path().join("openclaw-rs");
        std::fs::write(&binary, "current").unwrap();
        let err = install(&binary, b"#!/bin/sh\necho openclaw-rs 1.0.0\n", "99.0.0")
            .unwrap_err()
            .to_string();
        assert!(err.contains("does not run as version 99.0.0"), "{err}");
        assert_eq!(std::fs::read_to_string(&binary).unwrap(), "current");
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 1);
    }
}
