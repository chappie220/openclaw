//! `openclaw-rs doctor`: checks that everything configured actually works
//! (the config file, the key and its credit, the models and what they can
//! do, the browser, search, the Gateway, QQ, email, the service) and says
//! what to change for each problem. With `--offline` it only reads the
//! config and the host.
//!
//! The model checks make two tiny calls (a few hundred tokens): one asks the
//! main model to call a tool, one shows the image model a small picture.

use std::net::{SocketAddr, TcpStream};
use std::path::Path;
use std::time::{Duration, Instant};

use anyhow::Result;

use crate::config::{Config, DEFAULT_BASE_URL, SearchProvider};
use crate::i18n::{Tr, tr};
use crate::onboard::{Catalog, KeyCheck, ModelInfo};

/// An 8×8 red PNG, for the image check.
const RED_PNG: &str = "data:image/png;base64,iVBORw0KGgoAAAANSUhEUgAAAAgAAAAICAIAAABLbSncAAAAEklEQVR4nGP4z8CAFWEXHbQSACj/P8Fu7N9hAAAAAElFTkSuQmCC";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Status {
    Ok,
    Warn,
    Fail,
    Info,
}

/// One line of the report.
#[derive(Debug, Clone, PartialEq)]
pub struct Finding {
    pub status: Status,
    pub text: String,
}

fn ok(text: impl Into<String>) -> Finding {
    Finding {
        status: Status::Ok,
        text: text.into(),
    }
}
fn warn(text: impl Into<String>) -> Finding {
    Finding {
        status: Status::Warn,
        text: text.into(),
    }
}
fn fail(text: impl Into<String>) -> Finding {
    Finding {
        status: Status::Fail,
        text: text.into(),
    }
}
fn info(text: impl Into<String>) -> Finding {
    Finding {
        status: Status::Info,
        text: text.into(),
    }
}

const TITLE: Tr = tr("OpenClaw doctor", "OpenClaw 自检");
const OFFLINE_NOTE: Tr = tr(
    "Offline: nothing is contacted; only the config and this host are checked.",
    "离线模式：不联网，只检查配置和本机。",
);
const S_CONFIG: Tr = tr("Config", "配置");
const S_MODELS: Tr = tr("Models", "模型");
const S_BROWSER: Tr = tr("Browser", "浏览器");
const S_FETCH: Tr = tr("Reading web pages", "读取网页");
const S_SEARCH: Tr = tr("Web search", "网页搜索");
const S_MCP: Tr = tr("MCP servers", "MCP 服务器");
const S_SKILLS: Tr = tr("Skills", "Skills");
const S_GATEWAY: Tr = tr("Gateway", "Gateway");
const S_QQ: Tr = tr("QQ", "QQ");
const S_MAIL: Tr = tr("Email", "邮件");
const S_ACCESS: Tr = tr("Access", "权限");
const S_LIMITS: Tr = tr("Spending limits", "花费上限");
const S_SERVICE: Tr = tr("Service", "服务");

const CONFIG_OK: Tr = tr("{} loads", "{} 可以正常加载");
const CONFIG_MISSING: Tr = tr(
    "{} does not exist yet: run `openclaw-rs init`",
    "{} 还不存在：运行 `openclaw-rs init`",
);
const CONFIG_BROKEN: Tr = tr(
    "{} does not load: {}. Fix that field with `openclaw-rs config`",
    "{} 无法加载：{}。用 `openclaw-rs config` 修改它指出的字段",
);
const WORKSPACE_OK: Tr = tr("workspace {} is writable", "工作区 {} 可写");
const WORKSPACE_BAD: Tr = tr("workspace {} is not writable: {}", "工作区 {} 不可写：{}");

const NO_MODEL: Tr = tr(
    "model.model is not set: run `openclaw-rs init`",
    "没有设置 model.model：运行 `openclaw-rs init`",
);
const MODEL_IS: Tr = tr("main model: {}", "主模型：{}");
const NO_KEY: Tr = tr(
    "no API key: set OPENROUTER_API_KEY or run `openclaw-rs init`",
    "没有 API key：设置 OPENROUTER_API_KEY 或运行 `openclaw-rs init`",
);
const KEY_FROM_ENV: Tr = tr(
    "API key from $OPENROUTER_API_KEY",
    "API key 来自 $OPENROUTER_API_KEY",
);
const KEY_FROM_FILE: Tr = tr("API key from config.toml", "API key 来自 config.toml");
const KEY_OK: Tr = tr("the key works", "key 可用");
const KEY_OK_CREDIT: Tr = tr("the key works ({})", "key 可用（{}）");
const KEY_BAD: Tr = tr(
    "the key was refused ({}): create a new one at https://openrouter.ai/keys and run `openclaw-rs init`",
    "key 被拒绝（{}）：在 https://openrouter.ai/keys 新建一个，再运行 `openclaw-rs init`",
);
const KEY_UNCHECKED: Tr = tr("could not check the key: {}", "无法验证 key：{}");
const CATALOG_FAILED: Tr = tr(
    "could not load the model list ({}); model capabilities are not checked",
    "无法获取模型列表（{}），不检查模型能力",
);
const NOT_LISTED: Tr = tr(
    "{} ({}) is not on OpenRouter's model list: check the id with `openclaw-rs init`",
    "{}（{}）不在 OpenRouter 的模型列表里：用 `openclaw-rs init` 检查 id",
);
const NO_TOOLS: Tr = tr(
    "{} cannot call tools: the agent cannot run commands, read files, search or browse",
    "{} 不能调用工具：智能体无法执行命令、读文件、搜索或浏览网页",
);
const SEES_IMAGES: Tr = tr("{} sees images", "{} 能看图片");
const BLIND_NO_VISION: Tr = tr(
    "{} cannot see images and agent.vision_model is not set: pictures and screenshots are only listed by name",
    "{} 不能看图片，也没有设置 agent.vision_model：图片和截图只按文件名列出",
);
const VISION_IS: Tr = tr("image model: {}", "图片模型：{}");
const VISION_BLIND: Tr = tr(
    "the image model {} cannot see images: pick another with `openclaw-rs init`",
    "图片模型 {} 不能看图片：用 `openclaw-rs init` 另选一个",
);
const HEARS: Tr = tr("{} transcribes voice messages", "{} 能把语音消息转成文字");
const DEAF_NO_AUDIO: Tr = tr(
    "{} cannot take audio and agent.audio_model is not set: voice messages from email or the Web UI stay untranscribed (QQ's own transcripts still work)",
    "{} 不能接收音频，也没有设置 agent.audio_model：邮件和 Web UI 发来的语音不会转成文字（QQ 自带的识别结果仍然可用）",
);
const AUDIO_DEAF: Tr = tr(
    "the audio model {} cannot take audio: pick one whose input includes audio",
    "音频模型 {} 不能接收音频：请换一个支持音频输入的模型",
);
const ROLE_AUDIO: Tr = tr("agent.audio_model", "agent.audio_model");
const ROLE_MAIN: Tr = tr("model.model", "model.model");
const ROLE_SUMMARY: Tr = tr("agent.summary_model", "agent.summary_model");
const ROLE_VISION: Tr = tr("agent.vision_model", "agent.vision_model");
const ROLE_SEARCH: Tr = tr("search.model", "search.model");
const CALL_TOOLS_OK: Tr = tr(
    "{} answered in {} s and called a tool",
    "{} 在 {} 秒内回复，并调用了工具",
);
const CALL_NO_TOOL: Tr = tr(
    "{} answered in {} s but did not call the tool it was asked to; tools may not work with it",
    "{} 在 {} 秒内回复，但没有按要求调用工具；这个模型可能用不了工具",
);
const CALL_FAILED: Tr = tr("calling {} failed: {}", "调用 {} 失败：{}");
const IMAGE_OK: Tr = tr(
    "{} looked at a test picture and said: {}",
    "{} 看了一张测试图片，回答：{}",
);
const IMAGE_FAILED: Tr = tr("showing {} a picture failed: {}", "给 {} 看图片失败：{}");

const BROWSER_OFF: Tr = tr(
    "turned off (browser.enabled = false)",
    "已关闭（browser.enabled = false）",
);
const BROWSER_MISSING: Tr = tr(
    "no Chromium-family browser found, so there is no browser tool: apk add chromium (or apt install chromium)",
    "没有找到 Chromium 系浏览器，所以没有浏览器工具：apk add chromium（或 apt install chromium）",
);
const BROWSER_FOUND: Tr = tr("found {}", "找到 {}");
const BROWSER_STARTS: Tr = tr("starts and opens a page: {}", "能启动并打开页面：{}");
const BROWSER_FAILED: Tr = tr("cannot start: {}", "无法启动：{}");

const FETCH_OFF: Tr = tr(
    "turned off (fetch.enabled = false): no web_fetch tool",
    "已关闭（fetch.enabled = false）：没有 web_fetch 工具",
);
const FETCH_ON: Tr = tr("web_fetch is on", "web_fetch 已开启");
const FETCH_OK: Tr = tr("reads {}: {}", "能读取 {}：{}");
const FETCH_FAILED: Tr = tr(
    "cannot read {}: {} (is this host online, or does it need a proxy?)",
    "无法读取 {}：{}（本机能上网吗？是否需要代理？）",
);
const FETCH_PRIVATE: Tr = tr(
    "owners may also read pages on this host and the local network (fetch.private_network)",
    "owner 还可以读取本机和局域网里的页面（fetch.private_network）",
);
const MCP_NONE: Tr = tr(
    "none configured ([mcp.servers.<name>] in config.toml)",
    "没有配置（在 config.toml 里加 [mcp.servers.<名字>]）",
);
const MCP_OFF: Tr = tr("{}: turned off", "{}：已关闭");
const MCP_CONFIGURED: Tr = tr("{}: {}", "{}：{}");
const MCP_OK: Tr = tr("{}: {} tool(s): {}", "{}：{} 个工具：{}");
const MCP_NO_TOOLS: Tr = tr(
    "{}: started but offers no tools (check its tools filter)",
    "{}：已启动，但没有提供工具（检查 tools 过滤设置）",
);
const MCP_FAILED: Tr = tr("{}: {}", "{}：{}");
const MCP_GUESTS: Tr = tr(
    "guests may use MCP tools (mcp in access.guest): they can do whatever the servers can",
    "访客可以使用 MCP 工具（access.guest 里有 mcp）：服务器能做的事他们都能做",
);
const SKILLS_OFF: Tr = tr(
    "turned off (skills.enabled = false)",
    "已关闭（skills.enabled = false）",
);
const SKILLS_NONE: Tr = tr(
    "none installed (openclaw-rs skills install <directory or git URL>, or put them in {})",
    "还没有安装（openclaw-rs skills install <目录或 git 地址>，或者放到 {}）",
);
const SKILL_READY: Tr = tr("{}: ready", "{}：可用");
const SKILL_UNUSABLE: Tr = tr("{}: {}", "{}：{}");
const SKILLS_GUESTS: Tr = tr(
    "guests may load skills (skills in access.guest): they can read every skill's instructions",
    "访客可以加载 skill（access.guest 里有 skills）：所有 skill 的说明他们都能读到",
);
const LIMIT_IS: Tr = tr("{}: ${}", "{}：${}");
const LIMIT_NONE_TOTAL: Tr = tr(
    "no limit for the whole agent: set limits.daily_usd or limits.monthly_usd to protect the balance",
    "整个 agent 没有总上限：设置 limits.daily_usd 或 limits.monthly_usd 可以保护余额",
);
const LIMIT_NONE_GUESTS: Tr = tr(
    "QQ or email is on and guests have no limit: anyone who writes can spend your balance (set limits.guest_daily_usd)",
    "QQ 或邮件已开启，但访客没有上限：任何发消息的人都能花你的余额（设置 limits.guest_daily_usd）",
);
const LIMIT_NO_COSTS: Tr = tr(
    "{} may not report what calls cost; limits only count the costs it reports",
    "{} 可能不报告调用费用；上限只能统计它报告的费用",
);
const SEARCH_OFF: Tr = tr("off: no web_search tool", "已关闭：没有 web_search 工具");
const SEARCH_OPENROUTER: Tr = tr(
    "OpenRouter's web plugin, billed per search with the same key",
    "使用 OpenRouter 的 web 插件，用同一个 key 按次计费",
);
const SEARXNG_OK: Tr = tr("SearXNG at {} answers", "SearXNG（{}）可用");
const SEARXNG_FAILED: Tr = tr(
    "SearXNG at {} does not answer with JSON: {}",
    "SearXNG（{}）没有返回 JSON：{}",
);
const SEARXNG_NO_URL: Tr = tr(
    "search.provider is searxng but search.searxng_url is not set",
    "search.provider 是 searxng，但没有设置 search.searxng_url",
);

const GATEWAY_BAD_BIND: Tr = tr(
    "gateway.bind {} is not an address: {}",
    "gateway.bind {} 不是有效地址：{}",
);
const GATEWAY_OPEN: Tr = tr(
    "gateway.bind {} is reachable from other hosts without a token, so `serve` refuses to start: set OPENCLAW_RS_TOKEN or gateway.token",
    "gateway.bind {} 可被其他设备访问但没有 token，`serve` 会拒绝启动：设置 OPENCLAW_RS_TOKEN 或 gateway.token",
);
const GATEWAY_LOCAL: Tr = tr(
    "listens on {} (this host only)",
    "监听 {}（只有本机能访问）",
);
const GATEWAY_TOKEN: Tr = tr(
    "listens on {}, protected by a token",
    "监听 {}，有 token 保护",
);
const GATEWAY_RUNNING: Tr = tr(
    "a Gateway is running: http://{}/",
    "Gateway 正在运行：http://{}/",
);
const GATEWAY_STOPPED: Tr = tr(
    "no Gateway is running (start it with `openclaw-rs serve`)",
    "Gateway 没有运行（用 `openclaw-rs serve` 启动）",
);

const OFF: Tr = tr("off", "未启用");
const QQ_NO_APP_ID: Tr = tr("qq.app_id is empty", "qq.app_id 为空");
const QQ_NO_SECRET: Tr = tr(
    "no app secret: set QQ_APP_SECRET or qq.app_secret",
    "没有 app secret：设置 QQ_APP_SECRET 或 qq.app_secret",
);
const QQ_OPEN: Tr = tr(
    "qq.allow is empty: anyone who finds the bot can talk to it",
    "qq.allow 为空：任何找到这个机器人的人都能和它对话",
);
const QQ_FAILED: Tr = tr("cannot log in: {}", "无法登录：{}");
const MAIL_FAILED: Tr = tr("cannot check: {}", "无法检查：{}");
const NO_OWNERS: Tr = tr(
    "access.owners is empty, so on QQ and email even you are a guest: add your qq:<openid> or mail:<address>",
    "access.owners 为空，所以在 QQ 和邮件里你自己也只是访客：把你的 qq:<openid> 或 mail:<地址> 加进去",
);
const ACCESS_OK: Tr = tr("owners: {}", "owner：{}");
const SERVICE_INSTALLED: Tr = tr(
    "installed; log: /var/log/openclaw-rs.log. It runs as its own account with that account's config, which may not be this one",
    "已安装；日志：/var/log/openclaw-rs.log。服务以自己的账户运行，用的是那个账户的配置，不一定是这一份",
);
const SERVICE_MISSING: Tr = tr(
    "not installed (doas openclaw-rs service install --user <account>)",
    "未安装（doas openclaw-rs service install --user <账户>）",
);
const SUMMARY_OK: Tr = tr("All good: no problems found.", "一切正常：没有发现问题。");
const SUMMARY: Tr = tr("{} problem(s), {} warning(s).", "{} 个问题，{} 个警告。");

#[cfg(test)]
pub const ALL: &[Tr] = &[
    TITLE,
    OFFLINE_NOTE,
    S_CONFIG,
    S_MODELS,
    S_BROWSER,
    S_FETCH,
    S_SEARCH,
    S_MCP,
    S_SKILLS,
    S_GATEWAY,
    S_QQ,
    S_MAIL,
    S_ACCESS,
    S_LIMITS,
    S_SERVICE,
    CONFIG_OK,
    CONFIG_MISSING,
    CONFIG_BROKEN,
    WORKSPACE_OK,
    WORKSPACE_BAD,
    NO_MODEL,
    MODEL_IS,
    NO_KEY,
    KEY_FROM_ENV,
    KEY_FROM_FILE,
    KEY_OK,
    KEY_OK_CREDIT,
    KEY_BAD,
    KEY_UNCHECKED,
    CATALOG_FAILED,
    NOT_LISTED,
    NO_TOOLS,
    SEES_IMAGES,
    BLIND_NO_VISION,
    VISION_IS,
    VISION_BLIND,
    HEARS,
    DEAF_NO_AUDIO,
    AUDIO_DEAF,
    ROLE_AUDIO,
    ROLE_MAIN,
    ROLE_SUMMARY,
    ROLE_VISION,
    ROLE_SEARCH,
    CALL_TOOLS_OK,
    CALL_NO_TOOL,
    CALL_FAILED,
    IMAGE_OK,
    IMAGE_FAILED,
    BROWSER_OFF,
    BROWSER_MISSING,
    BROWSER_FOUND,
    BROWSER_STARTS,
    BROWSER_FAILED,
    FETCH_OFF,
    FETCH_ON,
    FETCH_OK,
    FETCH_FAILED,
    FETCH_PRIVATE,
    MCP_NONE,
    MCP_OFF,
    MCP_CONFIGURED,
    MCP_OK,
    MCP_NO_TOOLS,
    MCP_FAILED,
    MCP_GUESTS,
    LIMIT_IS,
    LIMIT_NONE_TOTAL,
    LIMIT_NONE_GUESTS,
    LIMIT_NO_COSTS,
    SKILLS_OFF,
    SKILLS_NONE,
    SKILL_READY,
    SKILL_UNUSABLE,
    SKILLS_GUESTS,
    SEARCH_OFF,
    SEARCH_OPENROUTER,
    SEARXNG_OK,
    SEARXNG_FAILED,
    SEARXNG_NO_URL,
    GATEWAY_BAD_BIND,
    GATEWAY_OPEN,
    GATEWAY_LOCAL,
    GATEWAY_TOKEN,
    GATEWAY_RUNNING,
    GATEWAY_STOPPED,
    OFF,
    QQ_NO_APP_ID,
    QQ_NO_SECRET,
    QQ_OPEN,
    QQ_FAILED,
    MAIL_FAILED,
    NO_OWNERS,
    ACCESS_OK,
    SERVICE_INSTALLED,
    SERVICE_MISSING,
    SUMMARY_OK,
    SUMMARY,
];

/// Prints findings as they come and counts them.
struct Report {
    problems: usize,
    warnings: usize,
}

impl Report {
    fn section(&self, title: Tr) {
        println!("\n{}", title.now());
    }

    fn add(&mut self, findings: impl IntoIterator<Item = Finding>) {
        for finding in findings {
            let mark = match finding.status {
                Status::Ok => "✓",
                Status::Warn => {
                    self.warnings += 1;
                    "!"
                }
                Status::Fail => {
                    self.problems += 1;
                    "✗"
                }
                Status::Info => "·",
            };
            println!("  {mark} {}", finding.text);
        }
    }
}

/// Runs every check on `path` and prints the report; `Ok(false)` when it
/// found problems.
pub async fn run(path: &Path, state: &Path, offline: bool) -> Result<bool> {
    let mut report = Report {
        problems: 0,
        warnings: 0,
    };
    println!("{}", TITLE.now());
    if offline {
        println!("{}", OFFLINE_NOTE.now());
    }

    report.section(S_CONFIG);
    let shown = path.display().to_string();
    let config = if !path.exists() {
        report.add([fail(CONFIG_MISSING.with(&[&shown]))]);
        Config::default()
    } else {
        match Config::load(path) {
            Ok(config) => {
                report.add([ok(CONFIG_OK.with(&[&shown]))]);
                config
            }
            Err(err) => {
                report.add([fail(
                    CONFIG_BROKEN.with(&[&shown, &format!("{:#}", err.root_cause())]),
                )]);
                return Ok(finish(&report));
            }
        }
    };
    let workspace = config
        .tools
        .workspace
        .clone()
        .unwrap_or_else(|| state.join("workspace"));
    report.add([writable(&workspace)]);

    report.section(S_MODELS);
    report.add(key_findings(&config));
    let key = config.api_key().ok();
    let catalog = crate::onboard::OpenRouter::new(&config.model.base_url)?;
    let mut models = None;
    let mut refused = false;
    if !offline && let Some(key) = &key {
        report.add([match catalog.check_key(key).await {
            KeyCheck::Valid(Some(credit)) => ok(KEY_OK_CREDIT.with(&[&credit])),
            KeyCheck::Valid(None) => ok(KEY_OK.now()),
            KeyCheck::Invalid(why) => {
                refused = true;
                fail(KEY_BAD.with(&[&why]))
            }
            KeyCheck::Unknown(why) => warn(KEY_UNCHECKED.with(&[&why])),
        }]);
        match catalog.models().await {
            Ok(list) => models = Some(list),
            Err(err) => report.add([warn(CATALOG_FAILED.with(&[&format!("{err:#}")]))]),
        }
    }
    // Ids on other servers need not be OpenRouter's, so only OpenRouter's list is binding.
    let binding = models
        .as_deref()
        .filter(|_| config.model.base_url.trim_end_matches('/') == DEFAULT_BASE_URL);
    report.add(model_findings(&config, binding));
    // With a refused key every call fails the same way, so none is made.
    if !offline
        && !refused
        && let (Some(key), Ok(model)) = (&key, config.model_id())
    {
        report.add([call_findings(&config, key, model).await]);
        let vision = config
            .agent
            .vision_model
            .as_deref()
            .filter(|m| !m.trim().is_empty())
            .unwrap_or(model);
        let sees = binding.is_none_or(|list| list.iter().any(|m| m.id == vision && m.images));
        if sees {
            report.add([image_findings(&config, key, vision).await]);
        }
    }

    report.section(S_BROWSER);
    report.add(browser_findings(&config, &workspace, state, offline).await);

    report.section(S_FETCH);
    report.add(fetch_findings(&config, offline).await);

    report.section(S_SEARCH);
    report.add(search_findings(&config, offline).await);

    report.section(S_MCP);
    report.add(mcp_findings(&config, &workspace, offline).await);

    report.section(S_SKILLS);
    report.add(skills_findings(&config, &workspace));

    report.section(S_GATEWAY);
    report.add(gateway_findings(&config));

    report.section(S_QQ);
    report.add(qq_findings(&config, offline).await);

    report.section(S_MAIL);
    report.add(mail_findings(&config, offline).await);

    report.section(S_ACCESS);
    report.add(access_findings(&config));

    report.section(S_LIMITS);
    report.add(limits_findings(&config));

    report.section(S_SERVICE);
    report.add([if Path::new("/etc/init.d/openclaw-rs").exists() {
        ok(SERVICE_INSTALLED.now())
    } else {
        info(SERVICE_MISSING.now())
    }]);

    Ok(finish(&report))
}

fn finish(report: &Report) -> bool {
    println!();
    if report.problems == 0 && report.warnings == 0 {
        println!("{}", SUMMARY_OK.now());
    } else {
        println!(
            "{}",
            SUMMARY.with(&[&report.problems.to_string(), &report.warnings.to_string()])
        );
    }
    report.problems == 0
}

fn writable(dir: &Path) -> Finding {
    let shown = dir.display().to_string();
    let probe = dir.join(".doctor-probe");
    let result = std::fs::create_dir_all(dir)
        .and_then(|_| std::fs::write(&probe, b"ok"))
        .and_then(|_| std::fs::remove_file(&probe));
    match result {
        Ok(()) => ok(WORKSPACE_OK.with(&[&shown])),
        Err(err) => fail(WORKSPACE_BAD.with(&[&shown, &err.to_string()])),
    }
}

fn key_findings(config: &Config) -> Vec<Finding> {
    let mut findings = vec![match config.model_id() {
        Ok(model) => ok(MODEL_IS.with(&[model])),
        Err(_) => fail(NO_MODEL.now()),
    }];
    let from_env = std::env::var("OPENROUTER_API_KEY").is_ok_and(|k| !k.trim().is_empty());
    findings.push(match (from_env, config.api_key().is_ok()) {
        (true, _) => info(KEY_FROM_ENV.now()),
        (false, true) => info(KEY_FROM_FILE.now()),
        (false, false) => fail(NO_KEY.now()),
    });
    findings
}

/// What the catalog says about each configured model; `models` is `None`
/// when it is unknown (offline, or another server).
fn model_findings(config: &Config, models: Option<&[ModelInfo]>) -> Vec<Finding> {
    let set = |m: &Option<String>| m.clone().filter(|m| !m.trim().is_empty());
    let main = config.model_id().ok().map(str::to_owned);
    let vision = set(&config.agent.vision_model);
    let audio = set(&config.agent.audio_model);
    let mut findings = Vec::new();
    if let Some(vision) = &vision {
        findings.push(info(VISION_IS.with(&[vision])));
    }
    let Some(models) = models else {
        return findings;
    };
    let find = |id: &str| models.iter().find(|m| m.id == id);
    let search = (config.search.provider == SearchProvider::Openrouter)
        .then(|| set(&config.search.model))
        .flatten();
    for (role, id) in [
        (ROLE_MAIN, &main),
        (ROLE_SUMMARY, &set(&config.agent.summary_model)),
        (ROLE_VISION, &vision),
        (ROLE_AUDIO, &audio),
        (ROLE_SEARCH, &search),
    ] {
        if let Some(id) = id
            && find(id).is_none()
        {
            findings.push(fail(NOT_LISTED.with(&[id, role.now()])));
        }
    }
    if let Some(info) = main.as_deref().and_then(find) {
        if !info.tools {
            findings.push(fail(NO_TOOLS.with(&[&info.id])));
        }
        match (&vision, info.images) {
            (None, true) => findings.push(ok(SEES_IMAGES.with(&[&info.id]))),
            (None, false) => findings.push(warn(BLIND_NO_VISION.with(&[&info.id]))),
            (Some(_), _) => {}
        }
        match (&audio, info.audio) {
            (None, true) => findings.push(ok(HEARS.with(&[&info.id]))),
            (None, false) => findings.push(warn(DEAF_NO_AUDIO.with(&[&info.id]))),
            (Some(_), _) => {}
        }
    }
    if let Some(info) = audio.as_deref().and_then(find) {
        findings.push(if info.audio {
            ok(HEARS.with(&[&info.id]))
        } else {
            fail(AUDIO_DEAF.with(&[&info.id]))
        });
    }
    if let Some(info) = vision.as_deref().and_then(find) {
        findings.push(if info.images {
            ok(SEES_IMAGES.with(&[&info.id]))
        } else {
            fail(VISION_BLIND.with(&[&info.id]))
        });
    }
    findings
}

fn client(config: &Config, key: &str, model: &str) -> Result<crate::llm::Client> {
    crate::llm::Client::new(
        &crate::config::ModelConfig {
            model: model.to_owned(),
            fallbacks: Vec::new(),
            max_retries: 0,
            request_timeout_secs: 60,
            ..config.model.clone()
        },
        key.to_owned(),
    )
}

/// One call to `model`, without retries, given 90 s.
async fn ask(
    config: &Config,
    key: &str,
    model: &str,
    messages: Vec<crate::llm::ChatMessage>,
    tools: Vec<crate::llm::ToolSpec>,
) -> Result<crate::llm::Completion> {
    let client = client(config, key, model)?;
    let mut ignore = |_: &str| {};
    let call = client.complete(&messages, &tools, &mut ignore);
    match tokio::time::timeout(Duration::from_secs(90), call).await {
        Ok(result) => result,
        Err(_) => anyhow::bail!("no answer within 90 s"),
    }
}

/// Asks the main model to call a tool, the way every turn needs it to.
async fn call_findings(config: &Config, key: &str, model: &str) -> Finding {
    use crate::llm::{ChatMessage, ToolSpec};
    let ping = ToolSpec::function(
        "ping",
        "Checks the connection. Call it when asked to.",
        serde_json::json!({"type": "object", "properties": {}}),
    );
    let started = Instant::now();
    let result = ask(
        config,
        key,
        model,
        vec![ChatMessage::user(
            "Call the ping tool once. Do not write anything else.",
        )],
        vec![ping],
    )
    .await;
    let seconds = format!("{:.1}", started.elapsed().as_secs_f64());
    match result {
        Ok(done) if !done.tool_calls.is_empty() => ok(CALL_TOOLS_OK.with(&[model, &seconds])),
        Ok(_) => warn(CALL_NO_TOOL.with(&[model, &seconds])),
        Err(err) => fail(CALL_FAILED.with(&[model, &format!("{err:#}")])),
    }
}

/// Shows the image model a small red picture.
async fn image_findings(config: &Config, key: &str, model: &str) -> Finding {
    use crate::llm::ChatMessage;
    let mut message = ChatMessage::user("What colour is this picture? Answer in one word.");
    message.images = vec![RED_PNG.to_owned()];
    let result = ask(config, key, model, vec![message], Vec::new()).await;
    match result {
        Ok(done) => {
            let said: String = done.text.trim().chars().take(40).collect();
            ok(IMAGE_OK.with(&[model, &format!("{said:?}")]))
        }
        Err(err) => fail(IMAGE_FAILED.with(&[model, &format!("{err:#}")])),
    }
}

async fn browser_findings(
    config: &Config,
    workspace: &Path,
    state: &Path,
    offline: bool,
) -> Vec<Finding> {
    if !config.browser.enabled {
        return vec![info(BROWSER_OFF.now())];
    }
    // A scratch profile, so a running Gateway's browser is left alone.
    let scratch = tempfile_dir(state);
    let Some(browser) = crate::browser::Browser::new(&config.browser, &scratch, workspace) else {
        return vec![warn(BROWSER_MISSING.now())];
    };
    let mut findings = vec![info(BROWSER_FOUND.with(&[&browser.describe()]))];
    // Starting it is local, but attaching to cdp_url may reach another host.
    let attach = config
        .browser
        .cdp_url
        .as_ref()
        .is_some_and(|u| !u.trim().is_empty());
    if !(offline && attach) {
        findings.push(match browser.check().await {
            Ok(version) => ok(BROWSER_STARTS.with(&[&version])),
            Err(err) => fail(BROWSER_FAILED.with(&[&format!("{err:#}")])),
        });
    }
    let _ = std::fs::remove_dir_all(&scratch);
    findings
}

fn tempfile_dir(state: &Path) -> std::path::PathBuf {
    state.join(format!(".doctor-{}", std::process::id()))
}

async fn fetch_findings(config: &Config, offline: bool) -> Vec<Finding> {
    let fetcher = match crate::fetch::Fetcher::new(&config.fetch) {
        Ok(Some(fetcher)) => fetcher,
        Ok(None) => return vec![info(FETCH_OFF.now())],
        Err(err) => return vec![fail(format!("{err:#}"))],
    };
    let mut findings = Vec::new();
    if offline {
        findings.push(info(FETCH_ON.now()));
    } else {
        const PAGE: &str = "https://example.com";
        findings.push(match fetcher.fetch(PAGE, 0, false).await {
            Ok(page) => {
                let title = page
                    .lines()
                    .next()
                    .and_then(|l| l.strip_prefix("Title: "))
                    .unwrap_or("")
                    .to_owned();
                ok(FETCH_OK.with(&[PAGE, &title]))
            }
            Err(err) => fail(FETCH_FAILED.with(&[PAGE, &format!("{err:#}")])),
        });
    }
    if config.fetch.private_network {
        findings.push(info(FETCH_PRIVATE.now()));
    }
    findings
}

/// Starts each server and lists its tools; offline only says what is set,
/// since starting one may download it (`npx -y ...`).
fn skills_findings(config: &Config, workspace: &Path) -> Vec<Finding> {
    if !config.skills.enabled {
        return vec![info(SKILLS_OFF.now())];
    }
    let skills = crate::skills::Skills::new(workspace, &config.skills);
    let all = skills.scan();
    if all.is_empty() {
        return vec![info(SKILLS_NONE.with(&[&skills.dir.display().to_string()]))];
    }
    let mut findings: Vec<Finding> = all
        .iter()
        .map(|skill| match &skill.state {
            crate::skills::State::Ready => ok(SKILL_READY.with(&[&skill.name])),
            crate::skills::State::Off => info(SKILL_UNUSABLE.with(&[&skill.name, &skill.status()])),
            _ => warn(SKILL_UNUSABLE.with(&[&skill.name, &skill.status()])),
        })
        .collect();
    if config
        .access
        .guest
        .contains(&crate::access::Capability::Skills)
    {
        findings.push(warn(SKILLS_GUESTS.now()));
    }
    findings
}

async fn mcp_findings(config: &Config, workspace: &Path, offline: bool) -> Vec<Finding> {
    let servers = &config.mcp.servers;
    if servers.is_empty() {
        return vec![info(MCP_NONE.now())];
    }
    let mut findings: Vec<Finding> = servers
        .iter()
        .filter(|(_, s)| !s.enabled)
        .map(|(name, _)| info(MCP_OFF.with(&[name])))
        .collect();
    if offline {
        for (name, server) in servers.iter().filter(|(_, s)| s.enabled) {
            let what = match (&server.command, &server.url) {
                (Some(command), _) => format!("{command} {}", server.args.join(" ")),
                (None, Some(url)) => url.clone(),
                (None, None) => String::new(),
            };
            findings.push(info(MCP_CONFIGURED.with(&[name, what.trim()])));
        }
    } else {
        let mcp = crate::mcp::Mcp::connect(&config.mcp, workspace).await;
        for status in mcp.statuses() {
            findings.push(match (&status.error, status.tools.len()) {
                (Some(err), _) => fail(MCP_FAILED.with(&[&status.name, err])),
                (None, 0) => warn(MCP_NO_TOOLS.with(&[&status.name])),
                (None, n) => {
                    ok(MCP_OK.with(&[&status.name, &n.to_string(), &status.tools.join(", ")]))
                }
            });
        }
    }
    if config
        .access
        .guest
        .contains(&crate::access::Capability::Mcp)
    {
        findings.push(warn(MCP_GUESTS.now()));
    }
    findings
}

async fn search_findings(config: &Config, offline: bool) -> Vec<Finding> {
    match config.search.provider {
        SearchProvider::Off => vec![info(SEARCH_OFF.now())],
        SearchProvider::Openrouter => vec![info(SEARCH_OPENROUTER.now())],
        SearchProvider::Searxng => {
            let Some(url) = config
                .search
                .searxng_url
                .as_deref()
                .map(|u| u.trim().trim_end_matches('/'))
                .filter(|u| !u.is_empty())
            else {
                return vec![fail(SEARXNG_NO_URL.now())];
            };
            if offline {
                return Vec::new();
            }
            let answer = async {
                let response = reqwest::Client::new()
                    .get(reqwest::Url::parse_with_params(
                        &format!("{url}/search"),
                        [("q", "openclaw"), ("format", "json")],
                    )?)
                    .timeout(Duration::from_secs(20))
                    .send()
                    .await?
                    .error_for_status()?;
                response.json::<serde_json::Value>().await?;
                anyhow::Ok(())
            };
            vec![match answer.await {
                Ok(()) => ok(SEARXNG_OK.with(&[url])),
                Err(err) => fail(SEARXNG_FAILED.with(&[url, &format!("{err:#}")])),
            }]
        }
    }
}

fn gateway_findings(config: &Config) -> Vec<Finding> {
    let bind = config.gateway.bind.trim();
    let addr: SocketAddr = match bind.parse() {
        Ok(addr) => addr,
        Err(err) => return vec![fail(GATEWAY_BAD_BIND.with(&[bind, &err.to_string()]))],
    };
    let mut findings = vec![
        match (config.gateway.token().is_some(), addr.ip().is_loopback()) {
            (false, false) => fail(GATEWAY_OPEN.with(&[bind])),
            (false, true) => ok(GATEWAY_LOCAL.with(&[bind])),
            (true, _) => ok(GATEWAY_TOKEN.with(&[bind])),
        },
    ];
    let probe = if addr.ip().is_unspecified() {
        SocketAddr::new(std::net::Ipv4Addr::LOCALHOST.into(), addr.port())
    } else {
        addr
    };
    findings.push(
        if TcpStream::connect_timeout(&probe, Duration::from_millis(500)).is_ok() {
            info(GATEWAY_RUNNING.with(&[&probe.to_string()]))
        } else {
            info(GATEWAY_STOPPED.now())
        },
    );
    findings
}

async fn qq_findings(config: &Config, offline: bool) -> Vec<Finding> {
    let qq = &config.qq;
    if !qq.enabled {
        return vec![info(OFF.now())];
    }
    let mut findings = Vec::new();
    if qq.app_id.trim().is_empty() {
        findings.push(fail(QQ_NO_APP_ID.now()));
    }
    if qq.app_secret().is_none() {
        findings.push(fail(QQ_NO_SECRET.now()));
    }
    if qq.allow.is_empty() {
        findings.push(warn(QQ_OPEN.now()));
    }
    if offline || findings.iter().any(|f| f.status == Status::Fail) {
        return findings;
    }
    let checked = match crate::qq::QqBot::new(qq.clone()) {
        Ok(bot) => bot.check().await,
        Err(err) => Err(err),
    };
    findings.insert(
        0,
        match checked {
            Ok(detail) => ok(detail),
            Err(err) => fail(QQ_FAILED.with(&[&format!("{err:#}")])),
        },
    );
    findings
}

async fn mail_findings(config: &Config, offline: bool) -> Vec<Finding> {
    if !config.mail.enabled {
        return vec![info(OFF.now())];
    }
    if offline {
        return Vec::new();
    }
    match crate::mail::MailBot::connect_only(config.mail.clone()) {
        Ok(bot) => bot
            .check()
            .await
            .into_iter()
            .map(|(target, result)| match result {
                Ok(detail) => ok(format!("{target}: {detail}")),
                Err(err) => fail(format!("{target}: {err:#}")),
            })
            .collect(),
        Err(err) => vec![fail(MAIL_FAILED.with(&[&format!("{err:#}")]))],
    }
}

fn limits_findings(config: &Config) -> Vec<Finding> {
    let limits = &config.limits;
    let set = |v: Option<f64>| v.filter(|l| *l > 0.0);
    let mut findings = Vec::new();
    for (key, value) in [
        ("limits.daily_usd", limits.daily_usd),
        ("limits.monthly_usd", limits.monthly_usd),
        ("limits.guest_daily_usd", limits.guest_daily_usd),
        ("limits.turn_usd", limits.turn_usd),
    ] {
        if let Some(value) = set(value) {
            findings.push(ok(LIMIT_IS.with(&[key, &crate::limits::money(value)])));
        }
    }
    for (sender, value) in &limits.senders {
        findings.push(info(LIMIT_IS.with(&[
            &format!("limits.senders.\"{sender}\""),
            &crate::limits::money(*value),
        ])));
    }
    if set(limits.daily_usd).is_none() && set(limits.monthly_usd).is_none() {
        findings.push(info(LIMIT_NONE_TOTAL.now()));
    }
    if (config.qq.enabled || config.mail.enabled) && set(limits.guest_daily_usd).is_none() {
        findings.push(warn(LIMIT_NONE_GUESTS.now()));
    }
    let base = config.model.base_url.trim_end_matches('/');
    if base != DEFAULT_BASE_URL {
        findings.push(warn(LIMIT_NO_COSTS.with(&[base])));
    }
    findings
}

fn access_findings(config: &Config) -> Vec<Finding> {
    let owners = &config.access.owners;
    if owners.is_empty() {
        if config.qq.enabled || config.mail.enabled {
            vec![warn(NO_OWNERS.now())]
        } else {
            Vec::new()
        }
    } else {
        vec![ok(ACCESS_OK.with(&[&owners.join(", ")]))]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn model(id: &str, images: bool, tools: bool) -> ModelInfo {
        ModelInfo {
            id: id.into(),
            name: id.into(),
            context: 0,
            price: None,
            images,
            audio: images,
            tools,
        }
    }

    fn statuses(findings: &[Finding]) -> Vec<Status> {
        findings.iter().map(|f| f.status).collect()
    }

    #[test]
    fn every_text_has_both_languages() {
        crate::i18n::assert_complete(ALL);
    }

    #[test]
    fn models_are_checked_against_the_catalog() {
        let catalog = [
            model("a/sees", true, true),
            model("a/blind", false, true),
            model("a/no-tools", true, false),
        ];
        let mut config = Config::default();
        config.model.model = "a/sees".into();
        let found = model_findings(&config, Some(&catalog));
        assert_eq!(statuses(&found), [Status::Ok, Status::Ok], "{found:?}");

        // A main model that cannot see images or hear audio, with no image or
        // audio model, is two warnings.
        config.model.model = "a/blind".into();
        let found = model_findings(&config, Some(&catalog));
        assert_eq!(statuses(&found), [Status::Warn, Status::Warn], "{found:?}");

        // A blind image model, a missing summary model and a model without tools fail.
        config.model.model = "a/no-tools".into();
        config.agent.vision_model = Some("a/blind".into());
        config.agent.summary_model = Some("gone/model".into());
        let found = model_findings(&config, Some(&catalog));
        let text: Vec<&str> = found.iter().map(|f| f.text.as_str()).collect();
        assert!(
            text.iter()
                .any(|t| t.contains("gone/model") && t.contains("agent.summary_model")),
            "{text:?}"
        );
        assert!(
            text.iter()
                .any(|t| t.contains("a/no-tools") && t.contains("tools")),
            "{text:?}"
        );
        assert!(
            text.iter()
                .any(|t| t.contains("a/blind") && t.contains("image model")),
            "{text:?}"
        );
        assert_eq!(found.iter().filter(|f| f.status == Status::Fail).count(), 3);

        // Without a catalog nothing is claimed about capabilities.
        let found = model_findings(&config, None);
        assert_eq!(statuses(&found), [Status::Info]);
    }

    #[test]
    fn an_open_gateway_without_a_token_is_a_problem() {
        // SAFETY: no other test reads this variable.
        unsafe { std::env::remove_var("OPENCLAW_RS_TOKEN") };
        let mut config = Config::default();
        config.gateway.bind = "0.0.0.0:1".into();
        assert_eq!(gateway_findings(&config)[0].status, Status::Fail);
        config.gateway.token = Some("t".into());
        assert_eq!(gateway_findings(&config)[0].status, Status::Ok);
        config.gateway.bind = "nonsense".into();
        assert_eq!(statuses(&gateway_findings(&config)), [Status::Fail]);
    }

    #[tokio::test]
    async fn channels_report_what_is_missing() {
        let mut config = Config::default();
        assert_eq!(statuses(&qq_findings(&config, true).await), [Status::Info]);
        config.qq.enabled = true;
        // SAFETY: no other test reads this variable.
        unsafe { std::env::remove_var("QQ_APP_SECRET") };
        let found = qq_findings(&config, false).await;
        assert_eq!(statuses(&found), [Status::Fail, Status::Fail, Status::Warn]);
        assert_eq!(statuses(&access_findings(&config)), [Status::Warn]);
        config.access.owners = vec!["qq:me".into()];
        assert_eq!(statuses(&access_findings(&config)), [Status::Ok]);
    }

    #[test]
    fn workspace_must_be_writable() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(writable(&dir.path().join("ws")).status, Status::Ok);
        let file = dir.path().join("file");
        std::fs::write(&file, b"x").unwrap();
        assert_eq!(writable(&file.join("ws")).status, Status::Fail);
    }
}
