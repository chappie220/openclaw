//! OpenRC system service (Alpine and other non-systemd Linux hosts).

use std::ffi::{CStr, CString};
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail};

const NAME: &str = "openclaw-rs";
const INIT_SCRIPT: &str = "/etc/init.d/openclaw-rs";
const CONF_FILE: &str = "/etc/conf.d/openclaw-rs";
const LOG_FILE: &str = "/var/log/openclaw-rs.log";
/// Secrets copied from the installing shell into the owner-only conf.d file.
const FORWARDED_ENV: &[&str] = &[
    "OPENROUTER_API_KEY",
    "OPENCLAW_RS_TOKEN",
    "QQ_APP_SECRET",
    "MAIL_PASSWORD",
    "TYPESAFE_API_KEY",
];

pub struct Account {
    pub name: String,
    pub uid: u32,
    pub gid: u32,
    pub home: PathBuf,
}

pub fn lookup_account(name: &str) -> Result<Account> {
    let c_name = CString::new(name).context("account name contains NUL")?;
    // SAFETY: getpwnam returns a pointer into static storage that stays valid
    // until the next passwd call; every field is copied out before returning.
    unsafe {
        let pw = libc::getpwnam(c_name.as_ptr());
        if pw.is_null() {
            bail!(crate::cli_text::NO_ACCOUNT.with(&[&format!("{name:?}")]));
        }
        let home = CStr::from_ptr((*pw).pw_dir).to_string_lossy().into_owned();
        Ok(Account {
            name: name.to_owned(),
            uid: (*pw).pw_uid,
            gid: (*pw).pw_gid,
            home: PathBuf::from(home),
        })
    }
}

/// openrc-run evaluates these assignments, so values must be single-quoted shell words.
fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', r"'\''"))
}

pub fn render_init_script(binary: &Path, account: &Account) -> String {
    let user = format!("{}:{}", account.uid, account.gid);
    format!(
        r#"#!/sbin/openrc-run
# Managed by `openclaw-rs service install`; rerun it to rewrite this file.

description="OpenClaw Gateway (Rust)"
supervisor=supervise-daemon
command={command}
command_args="serve"
command_user={user}
directory={home}
output_log={LOG_FILE}
error_log={LOG_FILE}
respawn_delay=5
respawn_max=10
respawn_period=300
retry="TERM/30/KILL/5"

depend() {{
	need net
	use dns logger
	after firewall
}}

start_pre() {{
	checkpath -f -o {user} -m 0640 {LOG_FILE}
}}
"#,
        command = shell_quote(&binary.display().to_string()),
        user = shell_quote(&user),
        home = shell_quote(&account.home.display().to_string()),
    )
}

pub fn render_conf(account: &Account, env: &[(String, String)]) -> String {
    let mut lines = vec![
        "# Managed by `openclaw-rs service install`. Owner-only: may contain secrets.".to_owned(),
        format!(
            "export HOME={}",
            shell_quote(&account.home.display().to_string())
        ),
        format!(
            "export OPENCLAW_RS_HOME={}",
            shell_quote(&account.home.join(".openclaw-rs").display().to_string())
        ),
    ];
    for (key, value) in env {
        lines.push(format!("export {key}={}", shell_quote(value)));
    }
    lines.push(String::new());
    lines.join("\n")
}

fn require_root(action: &str) -> Result<()> {
    // SAFETY: geteuid has no preconditions.
    if unsafe { libc::geteuid() } != 0 {
        bail!(crate::cli_text::SERVICE_NEEDS_ROOT.with(&[action]));
    }
    if !Path::new("/sbin/openrc-run").exists() {
        bail!(crate::cli_text::SERVICE_NO_OPENRC.now());
    }
    Ok(())
}

fn run(program: &str, args: &[&str]) -> Result<()> {
    let status = Command::new(program)
        .args(args)
        .status()
        .with_context(|| format!("cannot run {program}"))?;
    if !status.success() {
        bail!("{program} {} failed ({status})", args.join(" "));
    }
    Ok(())
}

fn write_file(path: &str, contents: &str, mode: u32) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let tmp = format!("{path}.tmp");
    std::fs::write(&tmp, contents).with_context(|| format!("cannot write {tmp}"))?;
    std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(mode))?;
    std::fs::rename(&tmp, path).with_context(|| format!("cannot replace {path}"))?;
    Ok(())
}

pub fn install(user: &str) -> Result<()> {
    require_root("service install")?;
    let account = lookup_account(user)?;
    let binary = std::env::current_exe()?.canonicalize()?;
    let env: Vec<(String, String)> = FORWARDED_ENV
        .iter()
        .filter_map(|key| {
            std::env::var(key)
                .ok()
                .filter(|v| !v.is_empty())
                .map(|v| ((*key).to_owned(), v))
        })
        .collect();
    if !env.iter().any(|(key, _)| key == "OPENROUTER_API_KEY") {
        eprintln!("{}", crate::cli_text::SERVICE_NO_KEY.with(&[CONF_FILE]));
    }
    write_file(CONF_FILE, &render_conf(&account, &env), 0o600)?;
    write_file(INIT_SCRIPT, &render_init_script(&binary, &account), 0o755)?;
    run("rc-update", &["add", NAME, "default"])?;
    run("rc-service", &[NAME, "restart"])?;
    println!(
        "{}",
        crate::cli_text::SERVICE_INSTALLED.with(&[
            INIT_SCRIPT,
            &account.name,
            &account.home.display().to_string()
        ])
    );
    println!("{}", crate::cli_text::SERVICE_LOGS.with(&[LOG_FILE, NAME]));
    Ok(())
}

pub fn uninstall() -> Result<()> {
    require_root("service uninstall")?;
    if !Path::new(INIT_SCRIPT).exists() {
        println!(
            "{}",
            crate::cli_text::SERVICE_NOT_INSTALLED.with(&[INIT_SCRIPT])
        );
        return Ok(());
    }
    // Stop and runlevel removal fail harmlessly when already done.
    let _ = run("rc-service", &[NAME, "stop"]);
    let _ = run("rc-update", &["del", NAME, "default"]);
    for path in [INIT_SCRIPT, CONF_FILE] {
        if let Err(err) = std::fs::remove_file(path)
            && err.kind() != std::io::ErrorKind::NotFound
        {
            return Err(err).with_context(|| format!("cannot remove {path}"));
        }
    }
    println!(
        "{}",
        crate::cli_text::SERVICE_REMOVED.with(&[INIT_SCRIPT, CONF_FILE, LOG_FILE])
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn renders_quoted_service_files() {
        let account = Account {
            name: "pi".into(),
            uid: 1000,
            gid: 1000,
            home: "/home/pi".into(),
        };
        let script = render_init_script(Path::new("/usr/local/bin/openclaw-rs"), &account);
        assert!(script.contains("command='/usr/local/bin/openclaw-rs'"));
        assert!(script.contains("command_user='1000:1000'"));
        let conf = render_conf(&account, &[("OPENROUTER_API_KEY".into(), "sk-'x".into())]);
        assert!(conf.contains("export OPENCLAW_RS_HOME='/home/pi/.openclaw-rs'"));
        assert!(conf.contains(r"export OPENROUTER_API_KEY='sk-'\''x'"));
    }

    #[test]
    fn looks_up_root() {
        let root = lookup_account("root").unwrap();
        assert_eq!(root.uid, 0);
        assert!(lookup_account("no-such-user-xyz").is_err());
    }
}
