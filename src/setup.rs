//! `openclaw-rs config`: an interactive editor for config.toml, in English
//! or Chinese.
//!
//! Edits the file in place with `toml_edit`, so comments and keys it does not
//! know survive. Every change is checked by parsing the whole file as a
//! `Config`; a change that does not parse is undone on the spot.

use std::io::{BufRead, Write};
use std::path::Path;

use anyhow::{Context, Result, bail};
use toml_edit::{Array, DocumentMut, Item, Value};

use crate::config::Config;
pub use crate::i18n::Lang;
use crate::i18n::{Tr, tr};

#[derive(Clone, Copy)]
enum Kind {
    Text,
    Int,
    /// US dollars; 0 means no limit.
    Money,
    Bool,
    Choice(&'static [&'static str]),
    /// Comma-separated strings.
    List,
    /// Read without echo; `env` overrides the file when set.
    Secret {
        env: &'static str,
        generate: bool,
    },
}

const BROKEN: Tr = tr(
    "warning: the current file does not load ({}); fix the field it names below",
    "警告：当前配置文件无法加载（{}）；请在下面修改它指出的字段",
);
const EDITING: Tr = tr("Editing {}", "正在编辑 {}");
const HOW: Tr = tr(
    "In each field: Enter keeps the value, - resets it to the default, ? explains it.",
    "每个字段：回车保持不变，- 恢复默认值，? 查看说明。",
);
const SAVE: Tr = tr("Save and exit", "保存并退出");
const QUIT: Tr = tr("Quit without saving", "不保存退出");
const PICK_MENU: Tr = tr("Pick a number, s or q.", "请输入编号、s 或 q。");
const NO_CHANGES: Tr = tr("No changes.", "没有改动。");
const NOT_SAVED: Tr = tr("Quit without saving.", "已退出，未保存。");
const SAVED: Tr = tr("Saved {}.", "已保存 {}。");
const RESTART: Tr = tr(
    "Restart a running `serve` (rc-service openclaw-rs restart) to apply it.",
    "如果 `serve` 正在运行，重启后生效（rc-service openclaw-rs restart）。",
);
const RESULT_INVALID: Tr = tr(
    "not saved: the result does not load: {}",
    "未保存：修改后的配置无法加载：{}",
);
const CANNOT_RESET: Tr = tr("cannot reset: {}", "无法恢复默认：{}");
const NOT_ACCEPTED: Tr = tr("not accepted: {}", "不接受这个值：{}");
const NOT_NUMBER: Tr = tr("{} is not a whole number", "{} 不是整数");
const YES_NO: Tr = tr("answer y or n", "请回答 y 或 n");
const PICK_ONE: Tr = tr("pick one of: {}", "请从这些中选一个：{}");
const DEFAULT: Tr = tr("default", "默认");
const EMPTY: Tr = tr("empty", "空");
const NONE: Tr = tr("none", "无");
const FROM_ENV: Tr = tr("from ${}", "来自 ${}");
const IN_FILE: Tr = tr("set in this file", "已在本文件中设置");
const NOT_SET: Tr = tr("not set", "未设置");

struct Field {
    path: &'static [&'static str],
    label: Tr,
    help: Tr,
    kind: Kind,
}

struct Section {
    title: Tr,
    fields: &'static [Field],
}

const PERMISSION: &[&str] = &["allow", "ask", "deny"];
const SECURITY: &[&str] = &["tls", "starttls", "none"];

const SECTIONS: &[Section] = &[
    Section {
        title: tr("Language", "语言"),
        fields: &[Field {
            path: &["language"],
            label: tr("Language", "语言"),
            help: tr(
                "Language of CLI output, this editor and the program's own chat replies (/compact, /identity, errors). Reset (-) to follow the system locale. The Web UI follows the browser and has its own switch.",
                "CLI 输出、本编辑器和程序自己的聊天回复（/compact、/identity、错误提示）的语言。输入 - 恢复为跟随系统语言。Web UI 跟随浏览器语言，并有自己的切换按钮。",
            ),
            kind: Kind::Choice(&["en", "zh"]),
        }],
    },
    Section {
        title: tr("Model and context", "模型与上下文"),
        fields: &[
            Field {
                path: &["model", "model"],
                label: tr("Model", "模型"),
                help: tr(
                    "Required, there is no default: any OpenRouter model id, e.g. anthropic/claude-sonnet-4.5.",
                    "必填，没有默认值：任意 OpenRouter 模型 id，例如 anthropic/claude-sonnet-4.5。",
                ),
                kind: Kind::Text,
            },
            Field {
                path: &["model", "api_key"],
                label: tr("OpenRouter API key", "OpenRouter API key"),
                help: tr(
                    "Stored in plain text in config.toml (mode 0600); OPENROUTER_API_KEY wins when set.",
                    "以明文存在 config.toml（权限 0600）；设了 OPENROUTER_API_KEY 时以环境变量为准。",
                ),
                kind: Kind::Secret {
                    env: "OPENROUTER_API_KEY",
                    generate: false,
                },
            },
            Field {
                path: &["model", "fallbacks"],
                label: tr("Fallback models", "备用模型"),
                help: tr(
                    "Comma-separated; OpenRouter tries them in order when the model fails.",
                    "用逗号分隔；主模型失败时 OpenRouter 按顺序尝试。",
                ),
                kind: Kind::List,
            },
            Field {
                path: &["model", "max_retries"],
                label: tr("Retries on transient errors", "临时错误重试次数"),
                help: tr(
                    "Connection errors, HTTP 408/429/5xx, and streams that fail before any text.",
                    "连接错误、HTTP 408/429/5xx，以及还没输出文字就失败的流。",
                ),
                kind: Kind::Int,
            },
            Field {
                path: &["model", "prompt_cache"],
                label: tr("Prompt cache breakpoints", "Prompt 缓存标记"),
                help: tr(
                    "auto: for anthropic/ and google/ models; on: always (e.g. openrouter/auto routing to Claude); off: never.",
                    "auto：只对 anthropic/ 和 google/ 模型加；on：总是加（例如 openrouter/auto 会路由到 Claude 时）；off：不加。",
                ),
                kind: Kind::Choice(&["auto", "on", "off"]),
            },
            Field {
                path: &["agent", "context_tokens"],
                label: tr("Context budget (tokens)", "上下文预算（token）"),
                help: tr(
                    "Per model call: system prompt, tools, summary and history. Keep it below the model's window.",
                    "每次调用模型的总预算：系统提示、工具、摘要和历史。要小于模型的上下文窗口。",
                ),
                kind: Kind::Int,
            },
            Field {
                path: &["agent", "summary_model"],
                label: tr("Summary model", "摘要模型"),
                help: tr(
                    "Model that writes the context summary, e.g. a cheaper one. Reset (-) to use the main model.",
                    "写上下文摘要的模型，例如更便宜的模型。输入 - 恢复为主模型。",
                ),
                kind: Kind::Text,
            },
            Field {
                path: &["agent", "vision_model"],
                label: tr("Image model", "图片模型"),
                help: tr(
                    "Model for messages with images (files people send, browser screenshots). Reset (-) to use the main model; set it when the main model has no image input.",
                    "处理带图片消息（用户发来的图片、浏览器截图）的模型。输入 - 恢复为主模型；主模型不支持图片输入时需要设置。",
                ),
                kind: Kind::Text,
            },
            Field {
                path: &["agent", "audio_model"],
                label: tr("Audio model", "音频模型"),
                help: tr(
                    "Model that transcribes voice messages (email, Web UI, QQ when QQ sends no transcript). Reset (-) to use the main model; set it when the main model has no audio input.",
                    "把语音消息转成文字的模型（邮件、Web UI，以及 QQ 没给识别结果时）。输入 - 恢复为主模型；主模型不支持音频输入时需要设置。",
                ),
                kind: Kind::Text,
            },
            Field {
                path: &["agent", "recall_limit"],
                label: tr("Memories recalled per message", "每条消息自动召回的记忆数"),
                help: tr(
                    "Saved memories that share words with a message are shown to the model with it. 0 turns this off.",
                    "与消息有相同词语的已保存记忆，会随消息一起交给模型。设为 0 关闭。",
                ),
                kind: Kind::Int,
            },
            Field {
                path: &["agent", "max_steps"],
                label: tr("Max model calls per turn", "每轮最多调用模型次数"),
                help: tr("Stops a runaway tool loop.", "防止工具调用无限循环。"),
                kind: Kind::Int,
            },
            Field {
                path: &["guide", "enabled"],
                label: tr("Add messages to a running turn", "对话中插入补充消息"),
                help: tr(
                    "Messages sent while a turn runs join it at a good moment instead of waiting; see README \"Guided conversation\".",
                    "回复进行中发来的消息会在合适的时机加入当前这一轮，而不是排队等待；见 README 的 \"Guided conversation\"。",
                ),
                kind: Kind::Bool,
            },
            Field {
                path: &["guide", "provider"],
                label: tr("Model that picks the moment", "判断插入时机的模型"),
                help: tr(
                    "openrouter: TypeSafe's Jev decision model with your OpenRouter key; off: insert at the next step.",
                    "openrouter：通过 OpenRouter 密钥使用 TypeSafe 的 Jev 决策模型；off：在下一步直接插入。",
                ),
                kind: Kind::Choice(&["off", "openrouter", "openrouter-chat", "typesafe"]),
            },
        ],
    },
    Section {
        title: tr("Tools", "工具"),
        fields: &[
            Field {
                path: &["tools", "shell"],
                label: tr("Shell commands", "Shell 命令"),
                help: tr(
                    "allow: run without asking; ask: a person approves each one; deny: no shell tool.",
                    "allow：直接运行；ask：每条都要人批准；deny：不提供 shell 工具。",
                ),
                kind: Kind::Choice(PERMISSION),
            },
            Field {
                path: &["tools", "write"],
                label: tr("File writes and edits", "写入和修改文件"),
                help: tr(
                    "Reads are always allowed inside the workspace.",
                    "工作目录内的读取总是允许。",
                ),
                kind: Kind::Choice(PERMISSION),
            },
            Field {
                path: &["tools", "identity"],
                label: tr("Identity changes", "修改身份"),
                help: tr(
                    "deny removes identity_set; otherwise every draft needs a person's approval.",
                    "deny 会去掉 identity_set；否则每份草稿都要人批准。",
                ),
                kind: Kind::Choice(PERMISSION),
            },
            Field {
                path: &["tools", "workspace"],
                label: tr("Workspace directory", "工作目录"),
                help: tr(
                    "Where files and shell commands run. Reset (-) for <state dir>/workspace.",
                    "文件和 shell 命令所在目录。输入 - 恢复为 <状态目录>/workspace。",
                ),
                kind: Kind::Text,
            },
            Field {
                path: &["tools", "review", "provider"],
                label: tr("Shell command auto-review", "Shell 命令自动审查"),
                help: tr(
                    "A model rates each `ask` command; see README \"Command auto-review\".",
                    "由模型给每条需要批准的命令打分；见 README 的 \"Command auto-review\"。",
                ),
                kind: Kind::Choice(&["off", "openrouter", "openrouter-chat", "typesafe"]),
            },
        ],
    },
    Section {
        title: tr("Gateway and Web UI", "Gateway 与 Web UI"),
        fields: &[
            Field {
                path: &["gateway", "bind"],
                label: tr("Listen address", "监听地址"),
                help: tr(
                    "e.g. 127.0.0.1:18789, or 0.0.0.0:18789 for the LAN (needs a token).",
                    "例如 127.0.0.1:18789；局域网访问用 0.0.0.0:18789（需要 token）。",
                ),
                kind: Kind::Text,
            },
            Field {
                path: &["gateway", "token"],
                label: tr("Access token", "访问 token"),
                help: tr(
                    "Required unless bound to loopback; OPENCLAW_RS_TOKEN wins when set. Type + to generate one.",
                    "不是只监听本机时必须设置；设了 OPENCLAW_RS_TOKEN 时以环境变量为准。输入 + 自动生成。",
                ),
                kind: Kind::Secret {
                    env: "OPENCLAW_RS_TOKEN",
                    generate: true,
                },
            },
        ],
    },
    Section {
        title: tr("QQ bot", "QQ 机器人"),
        fields: &[
            Field {
                path: &["qq", "enabled"],
                label: tr("Enabled", "启用"),
                help: tr(
                    "Connects to QQ over WebSocket when `serve` runs.",
                    "运行 `serve` 时通过 WebSocket 连接 QQ。",
                ),
                kind: Kind::Bool,
            },
            Field {
                path: &["qq", "app_id"],
                label: tr("AppID", "AppID"),
                help: tr(
                    "From the QQ Open Platform (q.qq.com).",
                    "在 QQ 开放平台（q.qq.com）获取。",
                ),
                kind: Kind::Text,
            },
            Field {
                path: &["qq", "app_secret"],
                label: tr("AppSecret", "AppSecret"),
                help: tr(
                    "QQ_APP_SECRET wins when set.",
                    "设了 QQ_APP_SECRET 时以环境变量为准。",
                ),
                kind: Kind::Secret {
                    env: "QQ_APP_SECRET",
                    generate: false,
                },
            },
            Field {
                path: &["qq", "allow"],
                label: tr("Allowed openids", "允许的 openid"),
                help: tr(
                    "Comma-separated user or group openids; empty lets everyone chat. The log prints each sender's openid.",
                    "用逗号分隔的用户或群 openid；留空则所有人都能聊天。日志里会打印每个发送者的 openid。",
                ),
                kind: Kind::List,
            },
        ],
    },
    Section {
        title: tr("Email", "邮件"),
        fields: &[
            Field {
                path: &["mail", "enabled"],
                label: tr("Enabled", "启用"),
                help: tr(
                    "Polls IMAP and answers by SMTP when `serve` runs.",
                    "运行 `serve` 时轮询 IMAP，并通过 SMTP 回复。",
                ),
                kind: Kind::Bool,
            },
            Field {
                path: &["mail", "imap_host"],
                label: tr("IMAP host", "IMAP 服务器"),
                help: tr("e.g. imap.qq.com", "例如 imap.qq.com"),
                kind: Kind::Text,
            },
            Field {
                path: &["mail", "imap_port"],
                label: tr("IMAP port", "IMAP 端口"),
                help: tr("993 for TLS.", "TLS 用 993。"),
                kind: Kind::Int,
            },
            Field {
                path: &["mail", "imap_security"],
                label: tr("IMAP security", "IMAP 加密方式"),
                help: tr(
                    "none is only accepted for loopback hosts.",
                    "none 只能用于本机地址。",
                ),
                kind: Kind::Choice(SECURITY),
            },
            Field {
                path: &["mail", "smtp_host"],
                label: tr("SMTP host", "SMTP 服务器"),
                help: tr("e.g. smtp.qq.com", "例如 smtp.qq.com"),
                kind: Kind::Text,
            },
            Field {
                path: &["mail", "smtp_port"],
                label: tr("SMTP port", "SMTP 端口"),
                help: tr(
                    "465 for TLS, 587 for STARTTLS.",
                    "TLS 用 465，STARTTLS 用 587。",
                ),
                kind: Kind::Int,
            },
            Field {
                path: &["mail", "smtp_security"],
                label: tr("SMTP security", "SMTP 加密方式"),
                help: tr(
                    "none is only accepted for loopback hosts.",
                    "none 只能用于本机地址。",
                ),
                kind: Kind::Choice(SECURITY),
            },
            Field {
                path: &["mail", "username"],
                label: tr("Username", "用户名"),
                help: tr("Usually the full address.", "通常是完整的邮箱地址。"),
                kind: Kind::Text,
            },
            Field {
                path: &["mail", "password"],
                label: tr("Password", "密码"),
                help: tr(
                    "Many providers (QQ Mail, 163) need an app authorization code. MAIL_PASSWORD wins when set.",
                    "很多邮箱（QQ 邮箱、163）要填授权码。设了 MAIL_PASSWORD 时以环境变量为准。",
                ),
                kind: Kind::Secret {
                    env: "MAIL_PASSWORD",
                    generate: false,
                },
            },
            Field {
                path: &["mail", "from"],
                label: tr("Reply-from address", "回复的发件地址"),
                help: tr("Reset (-) to use the username.", "输入 - 恢复为用户名。"),
                kind: Kind::Text,
            },
            Field {
                path: &["mail", "allow"],
                label: tr("Allowed senders", "允许的发件人"),
                help: tr(
                    "Comma-separated addresses or @domain; required.",
                    "用逗号分隔的地址或 @域名；必填。",
                ),
                kind: Kind::List,
            },
        ],
    },
    Section {
        title: tr("Web search", "网页搜索"),
        fields: &[
            Field {
                path: &["search", "provider"],
                label: tr("Provider", "搜索服务"),
                help: tr(
                    "openrouter: billed per search with the OpenRouter key; searxng: your own instance; off: no web_search tool.",
                    "openrouter：用 OpenRouter key 按次计费；searxng：自己的实例；off：不提供 web_search 工具。",
                ),
                kind: Kind::Choice(&["openrouter", "searxng", "off"]),
            },
            Field {
                path: &["search", "searxng_url"],
                label: tr("SearXNG URL", "SearXNG 地址"),
                help: tr(
                    "e.g. http://127.0.0.1:8888, with the JSON format enabled.",
                    "例如 http://127.0.0.1:8888，需要开启 JSON 格式。",
                ),
                kind: Kind::Text,
            },
            Field {
                path: &["search", "model"],
                label: tr("Search model", "搜索模型"),
                help: tr(
                    "Model that runs OpenRouter searches. Reset (-) to use the main model.",
                    "执行 OpenRouter 搜索的模型。输入 - 恢复为主模型。",
                ),
                kind: Kind::Text,
            },
        ],
    },
    Section {
        title: tr("Browser", "浏览器"),
        fields: &[
            Field {
                path: &["browser", "enabled"],
                label: tr("Browser tool", "浏览器工具"),
                help: tr(
                    "Uses a Chromium, Chrome, Edge or Brave already installed on this host; none is bundled. Without one there is no browser tool.",
                    "使用本机已安装的 Chromium、Chrome、Edge 或 Brave，不自带浏览器；本机没有时就不提供浏览器工具。",
                ),
                kind: Kind::Bool,
            },
            Field {
                path: &["browser", "executable"],
                label: tr("Browser path", "浏览器路径"),
                help: tr(
                    "Reset (-) to use the first of chromium, chromium-browser, google-chrome, microsoft-edge, brave found on PATH.",
                    "输入 - 恢复为自动查找 PATH 中的 chromium、chromium-browser、google-chrome、microsoft-edge、brave。",
                ),
                kind: Kind::Text,
            },
            Field {
                path: &["browser", "cdp_url"],
                label: tr("Running browser", "已运行的浏览器"),
                help: tr(
                    "DevTools address of a browser started with --remote-debugging-port, e.g. http://127.0.0.1:9222; empty starts one when needed.",
                    "用 --remote-debugging-port 启动的浏览器的调试地址，例如 http://127.0.0.1:9222；留空则在需要时自动启动。",
                ),
                kind: Kind::Text,
            },
            Field {
                path: &["browser", "headless"],
                label: tr("Headless", "无窗口运行"),
                help: tr(
                    "n shows a window, which needs a display.",
                    "选 n 会显示窗口，需要图形界面。",
                ),
                kind: Kind::Bool,
            },
            Field {
                path: &["browser", "idle_secs"],
                label: tr("Close when idle (s)", "空闲关闭（秒）"),
                help: tr(
                    "The browser is closed after this long without use, freeing its memory.",
                    "这么久不用就关闭浏览器，释放内存。",
                ),
                kind: Kind::Int,
            },
        ],
    },
    Section {
        title: tr("Reading web pages", "读取网页"),
        fields: &[
            Field {
                path: &["fetch", "enabled"],
                label: tr("web_fetch tool", "web_fetch 工具"),
                help: tr(
                    "Downloads a page and reads its text without a browser: fast and light. Pages that need JavaScript still need the browser.",
                    "不用浏览器，直接下载网页读取文字，又快又省资源。需要 JavaScript 的页面仍然要用浏览器。",
                ),
                kind: Kind::Bool,
            },
            Field {
                path: &["fetch", "private_network"],
                label: tr("Local network", "局域网"),
                help: tr(
                    "y lets owners read pages on this host and the local network (router, NAS). Guests never can.",
                    "选 y 允许 owner 读取本机和局域网里的页面（路由器、NAS）。访客始终不行。",
                ),
                kind: Kind::Bool,
            },
        ],
    },
    Section {
        title: tr("Spending limits", "花费上限"),
        fields: &[
            Field {
                path: &["limits", "daily_usd"],
                label: tr(
                    "Daily limit, whole agent (USD)",
                    "整个 agent 每日上限（美元）",
                ),
                help: tr(
                    "Model calls stop for everyone, owners included, once this much was spent today. 0: no limit.",
                    "今天花到这个数后，所有人（包括 owner）的模型调用都会停止。0 表示不限。",
                ),
                kind: Kind::Money,
            },
            Field {
                path: &["limits", "monthly_usd"],
                label: tr(
                    "Monthly limit, whole agent (USD)",
                    "整个 agent 每月上限（美元）",
                ),
                help: tr(
                    "The same per calendar month. 0: no limit.",
                    "同上，按自然月计算。0 表示不限。",
                ),
                kind: Kind::Money,
            },
            Field {
                path: &["limits", "guest_daily_usd"],
                label: tr("Daily limit per guest (USD)", "每个访客每日上限（美元）"),
                help: tr(
                    "What each QQ or email sender who is not an owner may spend a day. 0: no limit.",
                    "每个不是 owner 的 QQ 或邮件发送者每天最多能花多少。0 表示不限。",
                ),
                kind: Kind::Money,
            },
            Field {
                path: &["limits", "turn_usd"],
                label: tr("Limit per turn (USD)", "每轮上限（美元）"),
                help: tr(
                    "Stops one reply that keeps calling tools once it cost this much. 0: no limit.",
                    "一次回复不停调用工具、花到这个数时就停下。0 表示不限。",
                ),
                kind: Kind::Money,
            },
        ],
    },
    Section {
        title: tr("Updates", "更新"),
        fields: &[
            Field {
                path: &["update", "check"],
                label: tr("Look for new versions", "检查新版本"),
                help: tr(
                    "The Gateway checks GitHub Releases once a day and logs a new version.",
                    "Gateway 每天检查一次 GitHub Releases，发现新版本就写进日志。",
                ),
                kind: Kind::Bool,
            },
            Field {
                path: &["update", "auto"],
                label: tr("Install updates on their own", "自动安装更新"),
                help: tr(
                    "The Gateway installs a new version (checksum checked, old binary kept) and restarts when no turn is running. The service account must be able to write the binary.",
                    "Gateway 自动安装新版本（会校验 checksum，保留旧版本），并在没有对话进行时重启。服务账号需要能写入程序文件。",
                ),
                kind: Kind::Bool,
            },
        ],
    },
    Section {
        title: tr("Skills", "Skills"),
        fields: &[Field {
            path: &["skills", "enabled"],
            label: tr("Skills", "Skills"),
            help: tr(
                "Task instructions in workspace/skills/<name>/SKILL.md that the agent loads when a request needs them. Manage them with openclaw-rs skills.",
                "放在 workspace/skills/<名字>/SKILL.md 里的任务说明，需要时 agent 会自动加载。用 openclaw-rs skills 管理。",
            ),
            kind: Kind::Bool,
        }],
    },
    Section {
        title: tr("Access", "权限"),
        fields: &[Field {
            path: &["access", "owners"],
            label: tr("Owners", "Owner"),
            help: tr(
                "Comma-separated qq:<openid> or mail:<address> treated like the terminal: every tool, /compact, identity approval.",
                "用逗号分隔的 qq:<openid> 或 mail:<地址>，和终端同等权限：所有工具、/compact、批准身份。",
            ),
            kind: Kind::List,
        }],
    },
];

/// Terminal I/O, abstracted so the editor can be driven by tests.
pub struct Term<R, W> {
    pub(crate) input: R,
    pub(crate) out: W,
    /// Turn echo off while a secret is typed; only for a real terminal.
    pub(crate) hide_secrets: bool,
    pub(crate) lang: Lang,
}

impl<R: BufRead, W: Write> Term<R, W> {
    /// One trimmed line, or `None` at end of input.
    pub(crate) fn ask(&mut self, prompt: &str) -> Result<Option<String>> {
        write!(self.out, "{prompt}")?;
        self.out.flush()?;
        let mut line = String::new();
        if self.input.read_line(&mut line)? == 0 {
            return Ok(None);
        }
        Ok(Some(line.trim().to_owned()))
    }

    pub(crate) fn secret(&mut self, prompt: &str) -> Result<Option<String>> {
        if !self.hide_secrets {
            return self.ask(prompt);
        }
        let _echo = EchoOff::new();
        let line = self.ask(prompt);
        writeln!(self.out)?;
        line
    }

    pub(crate) fn say(&mut self, text: &str) -> Result<()> {
        writeln!(self.out, "{text}")?;
        Ok(())
    }

    pub(crate) fn tell(&mut self, text: Tr, args: &[&str]) -> Result<()> {
        let line = text.fill(self.lang, args);
        self.say(&line)
    }
}

/// Disables terminal echo on stdin until dropped.
struct EchoOff(Option<libc::termios>);

impl EchoOff {
    fn new() -> Self {
        // SAFETY: termios is plain data; tcgetattr fills it or fails.
        unsafe {
            let mut term: libc::termios = std::mem::zeroed();
            if libc::tcgetattr(libc::STDIN_FILENO, &mut term) != 0 {
                return Self(None);
            }
            let saved = term;
            term.c_lflag &= !libc::ECHO;
            libc::tcsetattr(libc::STDIN_FILENO, libc::TCSANOW, &term);
            Self(Some(saved))
        }
    }
}

impl Drop for EchoOff {
    fn drop(&mut self) {
        if let Some(saved) = &self.0 {
            // SAFETY: restores the attributes read in `new`.
            unsafe {
                libc::tcsetattr(libc::STDIN_FILENO, libc::TCSANOW, saved);
            }
        }
    }
}

/// The process's own terminal.
pub fn terminal(lang: Lang) -> Term<std::io::StdinLock<'static>, std::io::Stdout> {
    // SAFETY: isatty has no preconditions.
    let hide = unsafe { libc::isatty(libc::STDIN_FILENO) } == 1;
    Term {
        input: std::io::stdin().lock(),
        out: std::io::stdout(),
        hide_secrets: hide,
        lang,
    }
}

/// Runs the editor on `path` with the process's terminal.
pub fn run(path: &Path, lang: Lang) -> Result<()> {
    edit(path, &mut terminal(lang))
}

pub(crate) fn load(path: &Path) -> Result<DocumentMut> {
    match std::fs::read_to_string(path) {
        Ok(text) => text
            .parse()
            .with_context(|| format!("{} is not valid TOML; fix it by hand first", path.display())),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(DocumentMut::new()),
        Err(err) => Err(err).with_context(|| format!("cannot read {}", path.display())),
    }
}

pub(crate) fn parse(doc: &DocumentMut) -> Result<Config> {
    Ok(toml::from_str(&doc.to_string())?)
}

/// The menu loop: edits `path` and saves it when asked.
pub fn edit<R: BufRead, W: Write>(path: &Path, term: &mut Term<R, W>) -> Result<()> {
    let mut doc = load(path)?;
    if let Err(err) = parse(&doc) {
        term.tell(BROKEN, &[&format!("{err:#}")])?;
    }
    let original = doc.to_string();
    term.tell(EDITING, &[&path.display().to_string()])?;
    term.tell(HOW, &[])?;
    loop {
        term.say("")?;
        for (i, section) in SECTIONS.iter().enumerate() {
            term.say(&format!("  {}) {}", i + 1, section.title.get(term.lang)))?;
        }
        term.say(&format!("  s) {}", SAVE.get(term.lang)))?;
        term.say(&format!("  q) {}", QUIT.get(term.lang)))?;
        let Some(choice) = term.ask("> ")? else {
            return finish(path, &doc, &original, term, false);
        };
        match choice.as_str() {
            "s" | "S" => return finish(path, &doc, &original, term, true),
            "q" | "Q" => return finish(path, &doc, &original, term, false),
            other => match other
                .parse::<usize>()
                .ok()
                .and_then(|n| SECTIONS.get(n.wrapping_sub(1)))
            {
                Some(section) => {
                    if !edit_section(&mut doc, section, term)? {
                        return finish(path, &doc, &original, term, false);
                    }
                }
                None => term.tell(PICK_MENU, &[])?,
            },
        }
    }
}

fn finish<R: BufRead, W: Write>(
    path: &Path,
    doc: &DocumentMut,
    original: &str,
    term: &mut Term<R, W>,
    save: bool,
) -> Result<()> {
    let text = doc.to_string();
    if !save || text == original {
        term.tell(
            if text == original {
                NO_CHANGES
            } else {
                NOT_SAVED
            },
            &[],
        )?;
        return Ok(());
    }
    if let Err(err) = toml::from_str::<Config>(&text) {
        bail!(RESULT_INVALID.fill(term.lang, &[&format!("{err:#}")]));
    }
    write_private(path, &text)?;
    term.tell(SAVED, &[&path.display().to_string()])?;
    term.tell(RESTART, &[])?;
    Ok(())
}

/// Writes atomically with mode 0600, since the file may hold secrets.
pub(crate) fn write_private(path: &Path, text: &str) -> Result<()> {
    use std::os::unix::fs::OpenOptionsExt;
    let dir = path
        .parent()
        .filter(|d| !d.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    std::fs::create_dir_all(dir).with_context(|| format!("cannot create {}", dir.display()))?;
    let tmp = dir.join(format!(
        ".{}.tmp",
        path.file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("config.toml")
    ));
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(&tmp)
        .with_context(|| format!("cannot write {}", tmp.display()))?;
    file.write_all(text.as_bytes())?;
    file.sync_all()?;
    std::fs::rename(&tmp, path).with_context(|| format!("cannot replace {}", path.display()))?;
    Ok(())
}

/// Walks one section's fields. `false` when input ended.
fn edit_section<R: BufRead, W: Write>(
    doc: &mut DocumentMut,
    section: &Section,
    term: &mut Term<R, W>,
) -> Result<bool> {
    term.say(&format!("\n[{}]", section.title.get(term.lang)))?;
    let mut index = 0;
    while let Some(field) = section.fields.get(index) {
        let current = describe(doc, field, term.lang);
        let options = match field.kind {
            Kind::Choice(choices) => format!(
                " ({})",
                choices
                    .iter()
                    .enumerate()
                    .map(|(i, c)| format!("{}={c}", i + 1))
                    .collect::<Vec<_>>()
                    .join(" ")
            ),
            Kind::Bool => match term.lang {
                Lang::En => " (y/n)".into(),
                Lang::Zh => " (y/n，是/否)".into(),
            },
            _ => String::new(),
        };
        let prompt = format!("{}{options} [{current}]: ", field.label.get(term.lang));
        let input = match field.kind {
            Kind::Secret { .. } => term.secret(&prompt)?,
            _ => term.ask(&prompt)?,
        };
        let Some(input) = input else {
            return Ok(false);
        };
        match input.as_str() {
            "" => {}
            "?" => {
                term.say(&format!("  {}", field.help.get(term.lang)))?;
                continue;
            }
            "-" => {
                let before = doc.clone();
                remove(doc, field.path);
                if let Err(err) = parse(doc) {
                    *doc = before;
                    term.tell(CANNOT_RESET, &[&format!("{err:#}")])?;
                    continue;
                }
            }
            text => {
                let value = match value_for(field.kind, text, term.lang) {
                    Ok(value) => value,
                    Err(err) => {
                        term.say(&format!("  {err:#}"))?;
                        continue;
                    }
                };
                let before = doc.clone();
                set(doc, field.path, value);
                if let Err(err) = parse(doc) {
                    *doc = before;
                    term.tell(NOT_ACCEPTED, &[&format!("{err:#}")])?;
                    continue;
                }
            }
        }
        index += 1;
    }
    Ok(true)
}

fn value_for(kind: Kind, text: &str, lang: Lang) -> Result<Value> {
    Ok(match kind {
        Kind::Text => Value::from(text),
        Kind::Int => {
            let n: u32 = text
                .replace('_', "")
                .parse()
                .with_context(|| NOT_NUMBER.fill(lang, &[&format!("{text:?}")]))?;
            Value::from(i64::from(n))
        }
        Kind::Money => {
            let n: f64 = text
                .trim_start_matches('$')
                .parse()
                .ok()
                .filter(|n: &f64| n.is_finite() && *n >= 0.0)
                .with_context(|| NOT_NUMBER.fill(lang, &[&format!("{text:?}")]))?;
            Value::from(n)
        }
        Kind::Bool => match text.to_ascii_lowercase().as_str() {
            "y" | "yes" | "true" | "on" | "1" | "是" => Value::from(true),
            "n" | "no" | "false" | "off" | "0" | "否" => Value::from(false),
            _ => bail!(YES_NO.get(lang)),
        },
        Kind::Choice(choices) => {
            let pick = text
                .parse::<usize>()
                .ok()
                .and_then(|n| choices.get(n.wrapping_sub(1)))
                .or_else(|| choices.iter().find(|c| c.eq_ignore_ascii_case(text)));
            match pick {
                Some(choice) => Value::from(*choice),
                None => bail!(PICK_ONE.fill(lang, &[&choices.join(", ")])),
            }
        }
        Kind::List => {
            let mut array = Array::new();
            for item in text.split(',').map(str::trim).filter(|s| !s.is_empty()) {
                array.push(item);
            }
            Value::Array(array)
        }
        Kind::Secret { generate, .. } => {
            if generate && text == "+" {
                Value::from(random_token()?)
            } else {
                Value::from(text)
            }
        }
    })
}

/// 24 random bytes, hex-encoded.
fn random_token() -> Result<String> {
    use std::io::Read;
    let mut bytes = [0u8; 24];
    std::fs::File::open("/dev/urandom")
        .and_then(|mut f| f.read_exact(&mut bytes))
        .context("cannot read /dev/urandom")?;
    Ok(bytes.iter().map(|b| format!("{b:02x}")).collect())
}

/// Sets the value at `path`, keeping the old value's surrounding comments.
pub(crate) fn set(doc: &mut DocumentMut, path: &[&str], mut value: Value) {
    let (key, tables) = path.split_last().expect("paths are never empty");
    let mut item = doc.as_item_mut();
    for name in tables {
        if !item.get(*name).is_some_and(Item::is_table_like) {
            item[*name] = toml_edit::table();
        }
        item = &mut item[*name];
    }
    if let Some(old) = item.get(*key).and_then(Item::as_value) {
        *value.decor_mut() = old.decor().clone();
    } else if tables.is_empty() {
        // A new root key sits above the tables; keep a blank line between.
        value.decor_mut().set_suffix("\n");
    }
    item[*key] = Item::Value(value);
}

fn remove(doc: &mut DocumentMut, path: &[&str]) {
    let (key, tables) = path.split_last().expect("paths are never empty");
    let mut item = doc.as_item_mut();
    for name in tables {
        match item.get_mut(*name) {
            Some(next) => item = next,
            None => return,
        }
    }
    if let Some(table) = item.as_table_like_mut() {
        table.remove(key);
    }
}

/// The value in effect for `field`, defaults included; secrets are never shown.
fn describe(doc: &DocumentMut, field: &Field, lang: Lang) -> String {
    let effective = parse(doc)
        .ok()
        .and_then(|config| toml::Value::try_from(&config).ok());
    let mut value = effective.as_ref();
    for name in field.path {
        value = value.and_then(|v| v.get(*name));
    }
    if let Kind::Secret { env, .. } = field.kind {
        let from_env = std::env::var(env).is_ok_and(|v| !v.trim().is_empty());
        let in_file = value
            .and_then(toml::Value::as_str)
            .is_some_and(|v| !v.is_empty());
        return match (from_env, in_file) {
            (true, _) => FROM_ENV.fill(lang, &[env]),
            (false, true) => IN_FILE.get(lang).into(),
            (false, false) => NOT_SET.get(lang).into(),
        };
    }
    match value {
        None => DEFAULT.get(lang).into(),
        Some(toml::Value::String(s)) if s.is_empty() => EMPTY.get(lang).into(),
        Some(toml::Value::String(s)) => s.clone(),
        Some(toml::Value::Array(items)) if items.is_empty() => NONE.get(lang).into(),
        Some(toml::Value::Array(items)) => items
            .iter()
            .map(|i| i.as_str().map_or_else(|| i.to_string(), str::to_owned))
            .collect::<Vec<_>>()
            .join(", "),
        Some(other) => other.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn drive(path: &Path, script: &str) -> String {
        drive_in(Lang::En, path, script)
    }

    fn drive_in(lang: Lang, path: &Path, script: &str) -> String {
        let mut term = Term {
            input: script.as_bytes(),
            out: Vec::new(),
            hide_secrets: false,
            lang,
        };
        edit(path, &mut term).unwrap();
        String::from_utf8(term.out).unwrap()
    }

    #[test]
    fn edits_keep_comments_and_unknown_keys() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(
            &path,
            "# my notes\n[model]\nmodel = \"x/old\"  # keep me\n\n[custom]\nthing = 1\n",
        )
        .unwrap();
        // Model (section 2): new id, keep key, two fallbacks, keep retries, cache by number,
        // a bad then a good budget, reset summary model, keep image and audio
        // models, recall, max steps and both guide fields.
        let script = "2\na/new\n\nb/one, c/two ,\n\n3\nlots\n32000\n-\n\n\n\n\n\n\ns\n";
        let out = drive(&path, script);
        assert!(out.contains("\"lots\" is not a whole number"), "{out}");
        assert!(out.contains("Saved"), "{out}");
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("# my notes"));
        assert!(text.contains("model = \"a/new\"  # keep me"), "{text}");
        assert!(text.contains("[custom]\nthing = 1"));
        let config: Config = toml::from_str(&text).unwrap();
        assert_eq!(config.model.model, "a/new");
        assert_eq!(config.model.fallbacks, ["b/one", "c/two"]);
        assert_eq!(config.model.prompt_cache, crate::config::PromptCache::Off);
        assert_eq!(config.agent.context_tokens, 32000);
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
    }

    #[test]
    fn shows_current_values_and_never_secrets() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(
            &path,
            "[qq]\napp_secret = \"hunter2\"\nallow = [\"A\", \"B\"]\n",
        )
        .unwrap();
        let out = drive(&path, "5\n?\n\n\n\n\nq\n");
        assert!(out.contains("Enabled (y/n) [false]"), "{out}");
        assert!(out.contains("Connects to QQ over WebSocket"), "{out}");
        assert!(out.contains("Allowed openids [A, B]"), "{out}");
        assert!(!out.contains("hunter2"), "{out}");
        assert!(out.contains("No changes."));
    }

    #[test]
    fn quitting_or_running_out_of_input_saves_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        let out = drive(&path, "4\n0.0.0.0:1\n+\nq\n");
        assert!(out.contains("Quit without saving."), "{out}");
        assert!(!path.exists());
        drive(&path, "5\ny\n");
        assert!(!path.exists());
    }

    #[test]
    fn generates_a_token_and_sets_nested_tables() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        drive(&path, "4\n\n+\n3\n\n\n\n\nopenrouter\ns\n");
        let config: Config = toml::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        let token = config.gateway.token.unwrap();
        assert_eq!(token.len(), 48);
        assert!(token.chars().all(|c| c.is_ascii_hexdigit()));
        assert_eq!(
            config.tools.review.provider,
            crate::config::ReviewProvider::Openrouter
        );
    }

    #[test]
    fn a_new_root_key_goes_above_the_tables() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(&path, "[model]\nmodel = \"x/y\"\n").unwrap();
        drive(&path, "1\nzh\ns\n");
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.starts_with("language = \"zh\"\n\n[model]"), "{text}");
        let config: Config = toml::from_str(&text).unwrap();
        assert_eq!(config.language, Some(Lang::Zh));
    }

    #[test]
    fn speaks_chinese() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        let out = drive_in(
            Lang::Zh,
            &path,
            "5\n?\n是\n\n\n\n99\n6\n\n\nabc\n\n\n\n\n\n\n\n\n\ns\n",
        );
        assert!(out.contains("2) 模型与上下文"), "{out}");
        assert!(out.contains("s) 保存并退出"), "{out}");
        assert!(out.contains("启用 (y/n，是/否) [false]"), "{out}");
        assert!(
            out.contains("运行 `serve` 时通过 WebSocket 连接 QQ。"),
            "{out}"
        );
        assert!(out.contains("AppSecret [未设置]"), "{out}");
        assert!(out.contains("请输入编号、s 或 q。"), "{out}");
        assert!(out.contains("\"abc\" 不是整数"), "{out}");
        assert!(out.contains("已保存"), "{out}");
        let config: Config = toml::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert!(config.qq.enabled);
    }

    #[test]
    fn every_text_has_both_languages() {
        let mut texts = vec![
            BROKEN,
            EDITING,
            HOW,
            SAVE,
            QUIT,
            PICK_MENU,
            NO_CHANGES,
            NOT_SAVED,
            SAVED,
            RESTART,
            RESULT_INVALID,
            CANNOT_RESET,
            NOT_ACCEPTED,
            NOT_NUMBER,
            YES_NO,
            PICK_ONE,
            DEFAULT,
            EMPTY,
            NONE,
            FROM_ENV,
            IN_FILE,
            NOT_SET,
        ];
        for section in SECTIONS {
            texts.push(section.title);
            for field in section.fields {
                texts.extend([field.label, field.help]);
            }
        }
        crate::i18n::assert_complete(&texts);
        assert_eq!(SAVED.fill(Lang::Zh, &["a.toml"]), "已保存 a.toml。");
    }

    #[test]
    fn rejects_a_choice_outside_the_list() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        let out = drive(&path, "3\nmaybe\n9\nask\n\n\n\n\ns\n");
        assert_eq!(
            out.matches("pick one of: allow, ask, deny").count(),
            2,
            "{out}"
        );
        assert!(
            std::fs::read_to_string(&path)
                .unwrap()
                .contains("shell = \"ask\"")
        );
    }
}
