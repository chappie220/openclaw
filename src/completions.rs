//! `openclaw-rs completions install`: puts the completion script where the
//! user's shell loads it, and adds the loading lines to its rc file only
//! when needed and only after asking.

use std::io::{BufRead, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use clap_complete::Shell;

use crate::i18n::{Lang, Tr, tr};

const BEGIN: &str = "# >>> openclaw-rs completion >>>";
const END: &str = "# <<< openclaw-rs completion <<<";

const UNKNOWN_SHELL: Tr = tr(
    "cannot tell the shell from $SHELL ({}); pass --shell bash, zsh, fish, elvish or powershell",
    "无法从 $SHELL（{}）判断 shell；请用 --shell 指定 bash、zsh、fish、elvish 或 powershell",
);
const WROTE: Tr = tr("Wrote {}", "已写入 {}");
const WILL_ADD: Tr = tr(
    "To load it, these lines go at the end of {}:",
    "要加载它，需要在 {} 末尾加上：",
);
const CONFIRM: Tr = tr("Add them? [Y/n] ", "要加上吗？[Y/n] ");
const ADDED: Tr = tr("Added them to {}", "已加到 {}");
const ALREADY: Tr = tr("{} already loads it", "{} 已经会加载它");
const SKIPPED: Tr = tr(
    "Skipped; add those lines yourself to load completion.",
    "已跳过；需要自己加上这些行才能加载补全。",
);
const BASH_PACKAGE: Tr = tr(
    "The bash-completion package is not installed, so ~/.bashrc has to load the script itself (or install bash-completion).",
    "没有安装 bash-completion 软件包，所以要由 ~/.bashrc 直接加载脚本（也可以安装 bash-completion）。",
);
const ZSH_CACHE: Tr = tr(
    "If it does not show up, delete ~/.zcompdump and start zsh again.",
    "如果没有生效，删除 ~/.zcompdump 后重新打开 zsh。",
);
const DONE: Tr = tr(
    "Open a new {} (or run `exec {}`) to use it. Run this again after upgrading.",
    "打开一个新的 {}（或运行 `exec {}`）即可使用。升级后再运行一次。",
);

/// Where things live in the user's home, from the environment.
#[derive(Debug, Clone)]
pub struct Home {
    pub home: PathBuf,
    /// `$XDG_DATA_HOME`, else `~/.local/share`.
    pub data: PathBuf,
    /// `$XDG_CONFIG_HOME`, else `~/.config`.
    pub config: PathBuf,
    /// `$ZDOTDIR`, else `~`.
    pub zdotdir: PathBuf,
}

impl Home {
    pub fn from_env() -> Result<Self> {
        let var = |name: &str| {
            std::env::var_os(name)
                .filter(|v| !v.is_empty())
                .map(PathBuf::from)
        };
        let home = var("HOME").context("HOME is not set")?;
        Ok(Self {
            data: var("XDG_DATA_HOME").unwrap_or_else(|| home.join(".local/share")),
            config: var("XDG_CONFIG_HOME").unwrap_or_else(|| home.join(".config")),
            zdotdir: var("ZDOTDIR").unwrap_or_else(|| home.clone()),
            home,
        })
    }
}

/// The shell named by a `$SHELL` path such as `/bin/zsh`.
pub fn shell_from_path(path: &str) -> Option<Shell> {
    let name = Path::new(path).file_name()?.to_str()?;
    Some(match name {
        "bash" => Shell::Bash,
        "zsh" => Shell::Zsh,
        "fish" => Shell::Fish,
        "elvish" => Shell::Elvish,
        "pwsh" | "powershell" => Shell::PowerShell,
        _ => return None,
    })
}

/// Whether the bash-completion package, which loads per-command scripts
/// from ~/.local/share/bash-completion/completions, is installed.
fn bash_completion_installed() -> bool {
    [
        "/usr/share/bash-completion/bash_completion",
        "/etc/bash_completion",
        "/usr/local/share/bash-completion/bash_completion",
        "/opt/homebrew/etc/profile.d/bash_completion.sh",
    ]
    .iter()
    .any(|p| Path::new(p).exists())
}

/// What installing for one shell does.
#[derive(Debug)]
struct Plan {
    /// The completion script and where it goes.
    file: Option<PathBuf>,
    /// Lines for the end of an rc file, unless the file already has them.
    rc: Option<(PathBuf, String)>,
    /// Why the rc lines are needed, shown before them.
    why: Option<Tr>,
    /// A tip shown at the end.
    tip: Option<Tr>,
}

fn plan(shell: Shell, home: &Home, bash_package: bool) -> Plan {
    match shell {
        Shell::Fish => Plan {
            file: Some(home.config.join("fish/completions/openclaw-rs.fish")),
            rc: None,
            why: None,
            tip: None,
        },
        Shell::Bash => {
            let file = home.data.join("bash-completion/completions/openclaw-rs");
            let rc = (!bash_package).then(|| {
                let quoted = file.display().to_string().replace('"', "\\\"");
                (
                    home.home.join(".bashrc"),
                    format!("[ -f \"{quoted}\" ] && . \"{quoted}\""),
                )
            });
            Plan {
                why: rc.as_ref().map(|_| BASH_PACKAGE),
                file: Some(file),
                rc,
                tip: None,
            }
        }
        Shell::Zsh => Plan {
            file: Some(home.home.join(".zfunc/_openclaw-rs")),
            rc: Some((
                home.zdotdir.join(".zshrc"),
                "fpath=(~/.zfunc $fpath)\nautoload -Uz compinit && compinit".into(),
            )),
            why: None,
            tip: Some(ZSH_CACHE),
        },
        Shell::Elvish => Plan {
            file: None,
            rc: Some((
                home.config.join("elvish/rc.elv"),
                "eval (openclaw-rs completions elvish | slurp)".into(),
            )),
            why: None,
            tip: None,
        },
        _ => Plan {
            file: None,
            rc: Some((
                home.config
                    .join("powershell/Microsoft.PowerShell_profile.ps1"),
                "openclaw-rs completions powershell | Out-String | Invoke-Expression".into(),
            )),
            why: None,
            tip: None,
        },
    }
}

/// Whether `rc` already loads completion: our block, or for zsh an fpath
/// line that already includes ~/.zfunc.
fn already_loaded(rc: &str, shell: Shell) -> bool {
    rc.contains(BEGIN)
        || (shell == Shell::Zsh
            && rc.lines().any(|l| {
                !l.trim_start().starts_with('#') && l.contains("fpath") && l.contains(".zfunc")
            }))
}

fn write_file(path: &Path, text: &str) -> Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).with_context(|| format!("cannot create {}", dir.display()))?;
    }
    std::fs::write(path, text).with_context(|| format!("cannot write {}", path.display()))
}

/// Installs completion for `shell` (or the one in `$SHELL`). `script`
/// renders the completion script; `yes` answers the rc-file question.
#[allow(clippy::too_many_arguments)]
pub fn install<R: BufRead, W: Write>(
    shell: Option<Shell>,
    shell_env: Option<&str>,
    home: &Home,
    bash_package: Option<bool>,
    yes: bool,
    lang: Lang,
    script: &dyn Fn(Shell) -> String,
    input: &mut R,
    out: &mut W,
) -> Result<()> {
    let shell = match shell.or_else(|| shell_env.and_then(shell_from_path)) {
        Some(shell) => shell,
        None => bail!(UNKNOWN_SHELL.fill(lang, &[shell_env.unwrap_or("")])),
    };
    let plan = plan(
        shell,
        home,
        bash_package.unwrap_or_else(bash_completion_installed),
    );
    if let Some(file) = &plan.file {
        write_file(file, &script(shell))?;
        writeln!(out, "{}", WROTE.fill(lang, &[&file.display().to_string()]))?;
    }
    if let Some((rc, lines)) = &plan.rc {
        let current = match std::fs::read_to_string(rc) {
            Ok(text) => text,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => String::new(),
            Err(err) => return Err(err).with_context(|| format!("cannot read {}", rc.display())),
        };
        let shown = rc.display().to_string();
        if already_loaded(&current, shell) {
            writeln!(out, "{}", ALREADY.fill(lang, &[&shown]))?;
        } else {
            let block = format!("{BEGIN}\n{lines}\n{END}\n");
            if let Some(why) = plan.why {
                writeln!(out, "{}", why.get(lang))?;
            }
            writeln!(out, "{}\n\n{block}", WILL_ADD.fill(lang, &[&shown]))?;
            let agreed = yes || {
                write!(out, "{}", CONFIRM.get(lang))?;
                out.flush()?;
                let mut answer = String::new();
                input.read_line(&mut answer)? > 0
                    && matches!(
                        answer.trim().to_lowercase().as_str(),
                        "" | "y" | "yes" | "是"
                    )
            };
            if !agreed {
                writeln!(out, "{}", SKIPPED.get(lang))?;
                return Ok(());
            }
            let mut text = current;
            if !text.is_empty() && !text.ends_with('\n') {
                text.push('\n');
            }
            if !text.is_empty() {
                text.push('\n');
            }
            text.push_str(&block);
            write_file(rc, &text)?;
            writeln!(out, "{}", ADDED.fill(lang, &[&shown]))?;
        }
    }
    let name = shell.to_string();
    writeln!(out, "{}", DONE.fill(lang, &[&name, &name]))?;
    if let Some(tip) = plan.tip {
        writeln!(out, "{}", tip.get(lang))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn home(dir: &Path) -> Home {
        Home {
            home: dir.to_path_buf(),
            data: dir.join(".local/share"),
            config: dir.join(".config"),
            zdotdir: dir.to_path_buf(),
        }
    }

    fn run(
        dir: &Path,
        shell: Option<Shell>,
        env: Option<&str>,
        bash_pkg: bool,
        answer: &str,
    ) -> Result<String> {
        let mut out = Vec::new();
        install(
            shell,
            env,
            &home(dir),
            Some(bash_pkg),
            false,
            Lang::En,
            &|s| format!("script for {s}\n"),
            &mut answer.as_bytes(),
            &mut out,
        )?;
        Ok(String::from_utf8(out).unwrap())
    }

    #[test]
    fn detects_the_shell_from_its_path() {
        assert_eq!(shell_from_path("/bin/zsh"), Some(Shell::Zsh));
        assert_eq!(shell_from_path("/usr/local/bin/fish"), Some(Shell::Fish));
        assert_eq!(shell_from_path("/usr/bin/pwsh"), Some(Shell::PowerShell));
        assert_eq!(shell_from_path("/bin/ash"), None);
        let dir = tempfile::tempdir().unwrap();
        let err = run(dir.path(), None, Some("/bin/ash"), true, "").unwrap_err();
        assert!(err.to_string().contains("--shell"), "{err}");
    }

    #[test]
    fn fish_only_needs_the_file() {
        let dir = tempfile::tempdir().unwrap();
        let out = run(dir.path(), None, Some("/usr/bin/fish"), true, "").unwrap();
        let file = dir.path().join(".config/fish/completions/openclaw-rs.fish");
        assert_eq!(std::fs::read_to_string(file).unwrap(), "script for fish\n");
        assert!(out.contains("exec fish"), "{out}");
        assert!(!out.contains("[Y/n]"), "{out}");
    }

    #[test]
    fn bash_with_the_package_only_needs_the_file() {
        let dir = tempfile::tempdir().unwrap();
        run(dir.path(), Some(Shell::Bash), None, true, "").unwrap();
        assert!(
            dir.path()
                .join(".local/share/bash-completion/completions/openclaw-rs")
                .exists()
        );
        assert!(!dir.path().join(".bashrc").exists());
    }

    #[test]
    fn bash_without_the_package_sources_it_from_bashrc() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(".bashrc"), "alias ll='ls -l'").unwrap();
        let out = run(dir.path(), Some(Shell::Bash), None, false, "\n").unwrap();
        assert!(
            out.contains("bash-completion package is not installed"),
            "{out}"
        );
        let rc = std::fs::read_to_string(dir.path().join(".bashrc")).unwrap();
        assert!(
            rc.starts_with("alias ll='ls -l'\n\n# >>> openclaw-rs completion >>>\n"),
            "{rc}"
        );
        assert!(
            rc.contains("bash-completion/completions/openclaw-rs\" ] && . \""),
            "{rc}"
        );
    }

    #[test]
    fn zsh_asks_before_touching_zshrc_and_adds_once() {
        let dir = tempfile::tempdir().unwrap();
        let zshrc = dir.path().join(".zshrc");
        std::fs::write(&zshrc, "export EDITOR=vi\n").unwrap();
        let out = run(dir.path(), None, Some("/bin/zsh"), true, "n\n").unwrap();
        assert!(out.contains("Skipped"), "{out}");
        assert_eq!(
            std::fs::read_to_string(&zshrc).unwrap(),
            "export EDITOR=vi\n"
        );
        assert!(dir.path().join(".zfunc/_openclaw-rs").exists());

        let out = run(dir.path(), None, Some("/bin/zsh"), true, "y\n").unwrap();
        assert!(out.contains("fpath=(~/.zfunc $fpath)"), "{out}");
        assert!(out.contains("Added them"), "{out}");
        let once = std::fs::read_to_string(&zshrc).unwrap();
        assert_eq!(once.matches(BEGIN).count(), 1);

        let out = run(dir.path(), None, Some("/bin/zsh"), true, "").unwrap();
        assert!(out.contains("already loads it"), "{out}");
        assert_eq!(std::fs::read_to_string(&zshrc).unwrap(), once);
    }

    #[test]
    fn zsh_with_its_own_zfunc_fpath_is_left_alone() {
        let dir = tempfile::tempdir().unwrap();
        let zshrc = dir.path().join(".zshrc");
        std::fs::write(
            &zshrc,
            "fpath+=(~/.zfunc)\nautoload -U compinit; compinit\n",
        )
        .unwrap();
        let out = run(dir.path(), Some(Shell::Zsh), None, true, "").unwrap();
        assert!(out.contains("already loads it"), "{out}");
        assert!(!std::fs::read_to_string(&zshrc).unwrap().contains(BEGIN));
    }

    #[test]
    fn end_of_input_declines() {
        let dir = tempfile::tempdir().unwrap();
        let out = run(dir.path(), Some(Shell::Elvish), None, true, "").unwrap();
        assert!(out.contains("Skipped"), "{out}");
        assert!(!dir.path().join(".config/elvish/rc.elv").exists());
    }

    #[test]
    fn yes_skips_the_question() {
        let dir = tempfile::tempdir().unwrap();
        let mut out = Vec::new();
        install(
            Some(Shell::PowerShell),
            None,
            &home(dir.path()),
            Some(true),
            true,
            Lang::Zh,
            &|_| String::new(),
            &mut "".as_bytes(),
            &mut out,
        )
        .unwrap();
        let out = String::from_utf8(out).unwrap();
        assert!(out.contains("已加到"), "{out}");
        let profile = dir
            .path()
            .join(".config/powershell/Microsoft.PowerShell_profile.ps1");
        assert!(
            std::fs::read_to_string(profile)
                .unwrap()
                .contains("Invoke-Expression")
        );
    }

    #[test]
    fn every_message_has_both_languages() {
        crate::i18n::assert_complete(&[
            UNKNOWN_SHELL,
            WROTE,
            WILL_ADD,
            CONFIRM,
            ADDED,
            ALREADY,
            SKIPPED,
            BASH_PACKAGE,
            ZSH_CACHE,
            DONE,
        ]);
    }
}
