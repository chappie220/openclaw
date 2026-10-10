//! What the CLI says, in each language: command and option help, and the
//! output of commands. See `i18n` for how the language is chosen.

use clap::{Command, CommandFactory};

use crate::i18n::{Lang, Tr, tr};

/// Help for each command, by its path (`""` is the program itself).
const COMMANDS: &[(&str, Tr)] = &[
    ("", tr("Single-binary OpenClaw", "单文件 OpenClaw")),
    (
        "chat",
        tr("Interactive chat in the terminal", "在终端里交互聊天"),
    ),
    (
        "ask",
        tr(
            "Send one message and print the reply",
            "发送一条消息并打印回复",
        ),
    ),
    (
        "serve",
        tr(
            "Run the Gateway: Web UI and WebSocket API",
            "运行 Gateway：Web UI 和 WebSocket API",
        ),
    ),
    (
        "service",
        tr(
            "Install or remove the OpenRC system service (run as root)",
            "安装或移除 OpenRC 系统服务（需要 root）",
        ),
    ),
    (
        "service install",
        tr(
            "Write /etc/init.d/openclaw-rs, enable it, and start it",
            "写入 /etc/init.d/openclaw-rs，启用并启动它",
        ),
    ),
    (
        "service uninstall",
        tr(
            "Stop and remove the service; state is kept",
            "停止并移除服务；状态数据保留",
        ),
    ),
    (
        "cron",
        tr(
            "Manage scheduled prompts (run by `serve`)",
            "管理定时任务（由 `serve` 运行）",
        ),
    ),
    (
        "cron add",
        tr(
            "Add a job: 5-field cron schedule in local time",
            "添加任务：本地时间的 5 段 cron 表达式",
        ),
    ),
    ("cron list", tr("List jobs", "列出任务")),
    ("cron remove", tr("Remove a job", "删除任务")),
    ("mail", tr("Mail channel helpers", "邮件渠道工具")),
    (
        "mail check",
        tr(
            "Log in to the configured IMAP and SMTP servers and report what works",
            "登录配置的 IMAP 和 SMTP 服务器并报告结果",
        ),
    ),
    (
        "mail queue",
        tr(
            "Show inbound mail by state, and every message not yet answered",
            "按状态显示收到的邮件，以及所有尚未回复的邮件",
        ),
    ),
    (
        "mail retry",
        tr(
            "Queue a failed or uncertain message again; a stored reply is resent under the same Message-ID, otherwise the turn runs again",
            "重新排队一封失败或状态不明的邮件；已生成的回复用同一个 Message-ID 重发，否则重新运行这一轮",
        ),
    ),
    (
        "identity",
        tr(
            "Show, set or reset the agent's identity and soul",
            "查看、设置或重置智能体的身份和灵魂",
        ),
    ),
    ("identity show", tr("Show the identity", "显示身份")),
    (
        "identity set",
        tr(
            "Set the identity directly instead of in a conversation",
            "直接设置身份，不通过对话",
        ),
    ),
    (
        "identity reset",
        tr(
            "Forget the identity; the next conversation sets it up again",
            "忘掉身份；下一次对话时重新设置",
        ),
    ),
    (
        "identity drafts",
        tr(
            "Show the identity draft waiting for approval, from any channel",
            "显示等待批准的身份草稿（来自任意渠道）",
        ),
    ),
    (
        "identity approve",
        tr(
            "Save identity draft <id> exactly as shown by `identity drafts`",
            "按 `identity drafts` 显示的内容原样保存身份草稿 <id>",
        ),
    ),
    (
        "identity reject",
        tr("Discard identity draft <id>", "丢弃身份草稿 <id>"),
    ),
    ("memory", tr("Manage long-term memory", "管理长期记忆")),
    ("memory add", tr("Save a fact", "保存一条记忆")),
    (
        "memory search",
        tr("Search saved facts", "搜索已保存的记忆"),
    ),
    ("memory list", tr("Show the newest facts", "显示最新的记忆")),
    (
        "memory delete",
        tr("Delete a fact by id", "按编号删除一条记忆"),
    ),
    (
        "config",
        tr(
            "Edit config.toml interactively: language, model, tools, gateway, QQ, email, search, access",
            "交互式编辑 config.toml：语言、模型、工具、Gateway、QQ、邮件、搜索、权限",
        ),
    ),
    (
        "completions",
        tr(
            "Print a shell completion script: bash, zsh, fish, elvish or powershell",
            "输出 shell 自动补全脚本：bash、zsh、fish、elvish 或 powershell",
        ),
    ),
    (
        "usage",
        tr(
            "Tokens and cost of model calls, per session",
            "按会话统计模型调用的 token 和费用",
        ),
    ),
    ("sessions", tr("List or delete sessions", "列出或删除会话")),
    ("sessions list", tr("List sessions", "列出会话")),
    (
        "sessions delete",
        tr("Delete a session and its messages", "删除会话及其消息"),
    ),
    (
        "help",
        tr(
            "Print this message or the help of the given subcommand(s)",
            "显示此帮助，或指定子命令的帮助",
        ),
    ),
];

/// Help for each argument, by command path and argument id; `*` matches any
/// command.
const ARGS: &[(&str, &str, Tr)] = &[
    (
        "*",
        "config",
        tr(
            "Config file (default: <state dir>/config.toml)",
            "配置文件（默认：<状态目录>/config.toml）",
        ),
    ),
    (
        "*",
        "lang",
        tr(
            "Language of output and help (default: `language` in config.toml, else the locale)",
            "输出和帮助的语言（默认：config.toml 里的 language，否则跟随系统语言）",
        ),
    ),
    ("*", "help", tr("Print help", "显示帮助")),
    ("*", "version", tr("Print version", "显示版本")),
    ("*", "session", tr("Session name", "会话名")),
    ("ask", "message", tr("Message to send", "要发送的消息")),
    (
        "serve",
        "bind",
        tr(
            "Listen address (default: gateway.bind, 127.0.0.1:18789)",
            "监听地址（默认：gateway.bind，127.0.0.1:18789）",
        ),
    ),
    (
        "service install",
        "user",
        tr(
            "Account the Gateway runs as; its home holds the state",
            "运行 Gateway 的账户；状态数据放在它的主目录",
        ),
    ),
    ("cron add", "name", tr("Job name", "任务名")),
    (
        "cron add",
        "schedule",
        tr(
            "e.g. \"*/30 * * * *\" or \"0 9 * * 1-5\"",
            "例如 \"*/30 * * * *\" 或 \"0 9 * * 1-5\"",
        ),
    ),
    ("cron add", "prompt", tr("Prompt to run", "要运行的提示词")),
    ("cron remove", "name", tr("Job name", "任务名")),
    (
        "mail retry",
        "id",
        tr(
            "Message number from `mail queue`",
            "`mail queue` 里的邮件编号",
        ),
    ),
    ("identity set", "name", tr("Name", "名字")),
    (
        "identity set",
        "creature",
        tr(
            "What the agent is: an AI, a robot, a familiar",
            "智能体是什么：AI、机器人、使魔……",
        ),
    ),
    (
        "identity set",
        "vibe",
        tr("One line on how it comes across", "一句话描述它给人的感觉"),
    ),
    ("identity set", "emoji", tr("One emoji", "一个表情符号")),
    (
        "identity set",
        "soul",
        tr(
            "SOUL.md text: voice, stance, style, boundaries",
            "SOUL.md 内容：语气、立场、风格、边界",
        ),
    ),
    (
        "identity set",
        "soul_file",
        tr(
            "Read the soul from a SOUL.md file",
            "从 SOUL.md 文件读取灵魂",
        ),
    ),
    ("identity approve", "id", tr("Draft number", "草稿编号")),
    (
        "identity approve",
        "code",
        tr(
            "The draft's code, to be sure it is the version you read",
            "草稿的验证码，确保批准的就是你看到的版本",
        ),
    ),
    ("identity reject", "id", tr("Draft number", "草稿编号")),
    (
        "memory add",
        "content",
        tr("The fact to save", "要保存的内容"),
    ),
    (
        "memory search",
        "query",
        tr("Words to search for", "搜索词"),
    ),
    ("*", "limit", tr("Most results to show", "最多显示几条")),
    ("memory delete", "id", tr("Memory number", "记忆编号")),
    (
        "usage",
        "days",
        tr("How many days back to count", "统计最近多少天"),
    ),
    ("sessions delete", "name", tr("Session name", "会话名")),
    (
        "completions",
        "shell",
        tr(
            "Shell to complete for; see README \"Shell completion\" for where the script goes",
            "要补全的 shell；脚本放在哪里见 README 的 \"Shell completion\"",
        ),
    ),
    (
        "help",
        "subcommand",
        tr("The subcommand whose help to show", "要显示帮助的子命令"),
    ),
];

/// Clap's generated `help` subcommands mirror the real ones: `cron help add`
/// describes `cron add`, and `cron help` (or `cron help help`) is help itself.
fn real_path(path: &str) -> String {
    let words: Vec<&str> = path.split(' ').filter(|w| !w.is_empty()).collect();
    let Some(at) = words.iter().position(|w| *w == "help") else {
        return path.to_owned();
    };
    let rest = &words[at + 1..];
    if rest.is_empty() || rest == ["help"] {
        return "help".into();
    }
    words[..at]
        .iter()
        .chain(rest)
        .copied()
        .collect::<Vec<_>>()
        .join(" ")
}

fn command_help(path: &str) -> Option<Tr> {
    let path = real_path(path);
    COMMANDS.iter().find(|(p, _)| *p == path).map(|(_, t)| *t)
}

fn arg_help(path: &str, id: &str) -> Option<Tr> {
    let path = real_path(path);
    let find = |p: &str| {
        ARGS.iter()
            .find(|(ap, aid, _)| *ap == p && *aid == id)
            .map(|(_, _, t)| *t)
    };
    find(&path).or_else(|| find("*"))
}

/// The CLI parser with help in `lang`.
pub fn command<T: CommandFactory>(lang: Lang) -> Command {
    let mut cmd = T::command();
    // Builds the generated help and version flags and the help subcommand,
    // and copies global options into subcommands, so all can be translated.
    cmd.build();
    localize(cmd, "", lang)
}

fn localize(mut cmd: Command, path: &str, lang: Lang) -> Command {
    if let Some(help) = command_help(path) {
        cmd = cmd.about(help.get(lang));
    }
    let ids: Vec<(String, bool)> = cmd
        .get_arguments()
        .map(|a| (a.get_id().to_string(), a.is_positional()))
        .collect();
    for (id, positional) in ids {
        let help = arg_help(path, &id);
        cmd = cmd.mut_arg(&id, |mut arg| {
            if let Some(help) = help {
                arg = arg.help(help.get(lang));
            }
            if lang == Lang::Zh {
                arg = arg.help_heading(if positional { "参数" } else { "选项" });
            }
            arg
        });
    }
    if lang == Lang::Zh {
        cmd = cmd.subcommand_help_heading("命令").help_template(
            "{before-help}{about-with-newline}\n用法: {usage}\n\n{all-args}{after-help}",
        );
    }
    let names: Vec<String> = cmd
        .get_subcommands()
        .map(|c| c.get_name().to_owned())
        .collect();
    for name in names {
        let sub = if path.is_empty() {
            name.clone()
        } else {
            format!("{path} {name}")
        };
        cmd = cmd.mut_subcommand(&name, |c| localize(c, &sub, lang));
    }
    cmd
}

/// Writes a completion script for `shell`, registered for the program's
/// name, with descriptions in the current language.
pub fn completions<T: CommandFactory>(shell: clap_complete::Shell, lang: Lang) -> String {
    let mut cmd = command::<T>(lang);
    let bin = cmd.get_name().to_owned();
    let mut out = Vec::new();
    clap_complete::generate(shell, &mut cmd, &bin, &mut out);
    let script = String::from_utf8_lossy(&out).into_owned();
    if shell != clap_complete::Shell::Bash {
        return script;
    }
    // clap_complete's bash script spells a hyphenated name two ways: the
    // state it assigns (`openclaw__rs__subcmd__cron`) and the branch that
    // should match it (`openclaw__subcmd__rs__subcmd__cron`), so nothing
    // below the top level would complete. Use the assigned spelling.
    let assigned = bin.replace('-', "__");
    let branch = bin.replace('-', "__subcmd__");
    script.replace(&branch, &assigned)
}

pub const ERROR: Tr = tr("error: {}", "错误：{}");
pub const FIRST_START: Tr = tr(
    "First start: the agent has no identity yet and will ask who it should be. Describe it, or name a fictional character for it to look up and become (e.g. \"be Sun Wukong\"). `openclaw-rs identity set` works too.",
    "首次启动：智能体还没有身份，会先问你它该是谁。描述一下，或者说出一个虚构角色让它去查并成为它（例如“做孙悟空”）。也可以用 `openclaw-rs identity set`。",
);
pub const CHAT_BANNER: Tr = tr(
    "{} · session {} · model {} · empty line or Ctrl-D to quit",
    "{} · 会话 {} · 模型 {} · 空行或 Ctrl-D 退出",
);
pub const NOTHING_TO_SEND: Tr = tr("nothing to send", "没有要发送的内容");
pub const TOOL_START: Tr = tr("[tool {} {}]", "[工具 {} {}]");
pub const TOOL_END: Tr = tr("[tool {} → {} bytes]", "[工具 {} → {} 字节]");
pub const ALLOW_TOOL: Tr = tr("Allow {}?", "允许 {}？");
pub const MAIL_RETRY_NONE: Tr = tr(
    "no failed or uncertain message #{}; see `openclaw-rs mail queue`",
    "没有编号为 #{} 的失败或状态不明的邮件；见 `openclaw-rs mail queue`",
);
pub const MAIL_REQUEUED: Tr = tr(
    "queued #{} again; the running Gateway picks it up on its next poll",
    "#{} 已重新排队；运行中的 Gateway 下次轮询时会处理",
);
pub const MAIL_CHECK_FAILED: Tr = tr("mail check failed", "邮件检查失败");
pub const NO_MAIL: Tr = tr("no mail received yet", "还没有收到邮件");
pub const MAIL_ENTRY: Tr = tr(
    "#{} {} {} from {} attempts={} updated {}",
    "#{} {} {} 来自 {} 尝试={} 更新于 {}",
);
pub const QQ_OPEN_TO_STRANGERS: Tr = tr(
    "access.guest or access.grants.\"qq:*\" lets any QQ user run commands or write files; list trusted openids in qq.allow (the log shows each sender's openid), or grant those capabilities to named senders only",
    "access.guest 或 access.grants.\"qq:*\" 让任何 QQ 用户都能运行命令或写文件；请在 qq.allow 里列出可信的 openid（日志会显示每个发送者的 openid），或只把这些权限授予指定的发送者",
);
pub const NO_OWNERS: Tr = tr(
    "access: no access.owners, so every QQ and email sender is a guest that can only use {}; add yourself as \"qq:<openid>\" or \"mail:<address>\" to use memory, cron, files or shell from there",
    "权限：没有设置 access.owners，所以每个 QQ 和邮件发送者都是访客，只能使用 {}；把自己加为 \"qq:<openid>\" 或 \"mail:<地址>\"，才能从那里使用记忆、定时任务、文件或 shell",
);
pub const NEVER: Tr = tr("never", "从未");
pub const CRON_ADDED: Tr = tr("added {} · next run {}", "已添加 {} · 下次运行 {}");
pub const CRON_ROW: Tr = tr(
    "{}\t{}\tsession={}\tnext={}\tlast={}\t{}",
    "{}\t{}\t会话={}\t下次={}\t上次={}\t{}",
);
pub const NO_JOB: Tr = tr("no job named {}", "没有名为 {} 的任务");
pub const REMOVED: Tr = tr("removed {}", "已删除 {}");
pub const MEMORY_SAVED: Tr = tr("saved #{}", "已保存 #{}");
pub const NO_MEMORY: Tr = tr("no memory #{}", "没有编号为 #{} 的记忆");
pub const DELETED_NUMBER: Tr = tr("deleted #{}", "已删除 #{}");
pub const CANNOT_READ: Tr = tr("cannot read {}", "无法读取 {}");
pub const NO_IDENTITY: Tr = tr(
    "no identity yet; the next conversation sets one up",
    "还没有身份；下一次对话时会设置",
);
pub const IDENTITY_SAVED: Tr = tr("identity saved", "身份已保存");
pub const IDENTITY_REMOVED: Tr = tr(
    "identity removed; the next conversation sets it up again",
    "身份已删除；下一次对话时会重新设置",
);
pub const NO_IDENTITY_TO_REMOVE: Tr = tr("no identity to remove", "没有可删除的身份");
pub const SESSION_ROW: Tr = tr("{}\t{} messages\tupdated {}", "{}\t{} 条消息\t更新于 {}");
pub const NO_SESSION: Tr = tr("no session named {}", "没有名为 {} 的会话");
pub const DELETED: Tr = tr("deleted {}", "已删除 {}");
pub const SERVICE_NEEDS_ROOT: Tr = tr(
    "{} writes system files; run it with sudo or doas",
    "{} 会写系统文件；请用 sudo 或 doas 运行",
);
pub const SERVICE_NO_OPENRC: Tr = tr(
    "OpenRC is not installed (/sbin/openrc-run missing); on systemd hosts run `openclaw-rs serve` from a unit instead",
    "没有安装 OpenRC（缺少 /sbin/openrc-run）；systemd 系统请在 unit 里运行 `openclaw-rs serve`",
);
pub const SERVICE_NO_KEY: Tr = tr(
    "warning: OPENROUTER_API_KEY is not set; add it to {} or the account's config.toml",
    "警告：没有设置 OPENROUTER_API_KEY；请把它加到 {} 或该账户的 config.toml",
);
pub const SERVICE_INSTALLED: Tr = tr(
    "installed {} (runs as {}, state in {}/.openclaw-rs)",
    "已安装 {}（以 {} 运行，状态数据在 {}/.openclaw-rs）",
);
pub const SERVICE_LOGS: Tr = tr(
    "logs: {} · status: rc-service {} status",
    "日志：{} · 状态：rc-service {} status",
);
pub const SERVICE_NOT_INSTALLED: Tr = tr("{} is not installed", "{} 没有安装");
pub const SERVICE_REMOVED: Tr = tr(
    "removed {} and {}; state and {} were kept",
    "已删除 {} 和 {}；状态数据和 {} 已保留",
);
pub const NO_ACCOUNT: Tr = tr("no account named {}", "没有名为 {} 的账户");

#[cfg(test)]
mod tests {
    use super::*;

    const MESSAGES: &[Tr] = &[
        ERROR,
        FIRST_START,
        CHAT_BANNER,
        NOTHING_TO_SEND,
        TOOL_START,
        TOOL_END,
        ALLOW_TOOL,
        MAIL_RETRY_NONE,
        MAIL_REQUEUED,
        MAIL_CHECK_FAILED,
        NO_MAIL,
        MAIL_ENTRY,
        QQ_OPEN_TO_STRANGERS,
        NO_OWNERS,
        NEVER,
        CRON_ADDED,
        CRON_ROW,
        NO_JOB,
        REMOVED,
        MEMORY_SAVED,
        NO_MEMORY,
        DELETED_NUMBER,
        CANNOT_READ,
        NO_IDENTITY,
        IDENTITY_SAVED,
        IDENTITY_REMOVED,
        NO_IDENTITY_TO_REMOVE,
        SESSION_ROW,
        NO_SESSION,
        DELETED,
        SERVICE_NEEDS_ROOT,
        SERVICE_NO_OPENRC,
        SERVICE_NO_KEY,
        SERVICE_INSTALLED,
        SERVICE_LOGS,
        SERVICE_NOT_INSTALLED,
        SERVICE_REMOVED,
        NO_ACCOUNT,
    ];

    #[test]
    fn every_message_has_both_languages() {
        crate::i18n::assert_complete(MESSAGES);
        let all: Vec<Tr> = COMMANDS
            .iter()
            .map(|(_, t)| *t)
            .chain(ARGS.iter().map(|(_, _, t)| *t))
            .collect();
        crate::i18n::assert_complete(&all);
    }

    /// Every command and argument of the real CLI has help in the table.
    #[test]
    fn every_command_and_argument_is_translated() {
        fn walk(cmd: &Command, path: &str, missing: &mut Vec<String>) {
            if command_help(path).is_none() {
                missing.push(format!("command {path:?}"));
            }
            for arg in cmd.get_arguments() {
                if arg_help(path, arg.get_id().as_str()).is_none() {
                    missing.push(format!("argument {path:?} {}", arg.get_id()));
                }
            }
            for sub in cmd.get_subcommands() {
                let next = if path.is_empty() {
                    sub.get_name().to_owned()
                } else {
                    format!("{path} {}", sub.get_name())
                };
                walk(sub, &next, missing);
            }
        }
        let mut cmd = crate::Cli::command();
        cmd.build();
        let mut missing = Vec::new();
        walk(&cmd, "", &mut missing);
        assert!(missing.is_empty(), "untranslated: {missing:#?}");
    }

    #[test]
    fn generated_help_commands_map_to_real_ones() {
        assert_eq!(real_path("cron help add"), "cron add");
        assert_eq!(real_path("cron help"), "help");
        assert_eq!(real_path("cron help help"), "help");
        assert_eq!(real_path("help memory search"), "memory search");
        assert_eq!(real_path("memory search"), "memory search");
    }

    #[test]
    fn help_is_in_chinese() {
        let mut cmd = command::<crate::Cli>(Lang::Zh);
        let top = cmd.render_help().to_string();
        assert!(top.contains("用法: "), "{top}");
        assert!(top.contains("命令"), "{top}");
        assert!(top.contains("在终端里交互聊天"), "{top}");
        assert!(top.contains("显示帮助"), "{top}");
        let mut add = command::<crate::Cli>(Lang::Zh);
        let add = add
            .find_subcommand_mut("cron")
            .unwrap()
            .find_subcommand_mut("add")
            .unwrap()
            .render_help()
            .to_string();
        assert!(add.contains("任务名"), "{add}");
        assert!(add.contains("配置文件"), "{add}");
        let en = command::<crate::Cli>(Lang::En).render_help().to_string();
        assert!(en.contains("Interactive chat in the terminal"), "{en}");
        assert!(!en.contains("用法"), "{en}");
    }

    #[test]
    fn completion_scripts_cover_every_shell() {
        use clap_complete::Shell;
        for lang in [Lang::En, Lang::Zh] {
            for shell in [
                Shell::Bash,
                Shell::Zsh,
                Shell::Fish,
                Shell::Elvish,
                Shell::PowerShell,
            ] {
                let script = completions::<crate::Cli>(shell, lang);
                for word in ["chat", "serve", "usage", "completions", "lang"] {
                    assert!(script.contains(word), "{shell} lacks {word}");
                }
                if lang == Lang::Zh && shell == Shell::Zsh {
                    assert!(
                        script.contains("在终端里交互聊天"),
                        "zsh shows descriptions"
                    );
                }
            }
        }
    }

    /// Every state the bash script moves to has a branch that handles it.
    #[test]
    fn bash_completion_reaches_subcommands() {
        let script = completions::<crate::Cli>(clap_complete::Shell::Bash, Lang::En);
        let states: Vec<&str> = script
            .lines()
            .filter_map(|l| l.trim().strip_prefix("cmd=\""))
            .filter_map(|l| l.strip_suffix('"'))
            .filter(|state| !state.is_empty())
            .collect();
        assert!(states.contains(&"openclaw__rs__subcmd__cron"));
        for state in states {
            assert!(
                script.contains(&format!("        {state})")),
                "no branch for {state}"
            );
        }
    }

    #[test]
    fn parses_the_same_in_every_language() {
        use clap::FromArgMatches;
        for lang in [Lang::En, Lang::Zh] {
            let matches = command::<crate::Cli>(lang)
                .try_get_matches_from(["openclaw-rs", "--lang", "zh", "usage", "--days", "3"])
                .unwrap();
            let cli = crate::Cli::from_arg_matches(&matches).unwrap();
            assert!(matches!(cli.command, crate::Command::Usage { days: 3 }));
        }
    }
}
