//! `openclaw-rs init`: guided first-time setup. It asks only what a first
//! run needs (language, OpenRouter key, main model, an image model when the
//! main one cannot see images), checks the key and offers models from
//! OpenRouter's catalog, then saves config.toml. Everything else keeps its
//! default and stays in `openclaw-rs config`.
//!
//! `chat`, `ask` and `serve` start it on their own when run in a terminal
//! with no model or key, so nobody has to edit a file before the first reply.

use std::io::{BufRead, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result};
use async_trait::async_trait;
use serde_json::Value as Json;
use toml_edit::{DocumentMut, Value};

use crate::config::Config;
use crate::i18n::{Lang, Tr, tr};
use crate::setup::{self, Term};

/// Models shown per search.
const SHOWN: usize = 12;

/// A model from the catalog, with what the agent needs to know about it.
#[derive(Debug, Clone, PartialEq)]
pub struct ModelInfo {
    pub id: String,
    pub name: String,
    pub context: u64,
    /// US dollars per million tokens, in and out; `None` when not listed.
    pub price: Option<(f64, f64)>,
    pub images: bool,
    /// Supports tool calls, without which the agent cannot use its tools.
    pub tools: bool,
}

/// What checking an API key found.
#[derive(Debug, Clone, PartialEq)]
pub enum KeyCheck {
    Valid,
    /// The server refused it; the text says why.
    Invalid(String),
    /// It could not be checked (offline, another server).
    Unknown(String),
}

/// Where models and key checks come from; OpenRouter in real use.
#[async_trait]
pub trait Catalog: Send + Sync {
    async fn check_key(&self, key: &str) -> KeyCheck;
    async fn models(&self) -> Result<Vec<ModelInfo>>;
}

/// OpenRouter's (or a compatible server's) `/key` and `/models`.
pub struct OpenRouter {
    http: reqwest::Client,
    base_url: String,
}

impl OpenRouter {
    pub fn new(base_url: &str) -> Result<Self> {
        Ok(Self {
            http: reqwest::Client::builder()
                .connect_timeout(Duration::from_secs(10))
                .timeout(Duration::from_secs(30))
                .build()
                .context("cannot build HTTP client")?,
            base_url: base_url.trim_end_matches('/').to_owned(),
        })
    }
}

#[async_trait]
impl Catalog for OpenRouter {
    async fn check_key(&self, key: &str) -> KeyCheck {
        let response = self
            .http
            .get(format!("{}/key", self.base_url))
            .bearer_auth(key)
            .send()
            .await;
        match response {
            Ok(r) if r.status().is_success() => KeyCheck::Valid,
            Ok(r) if matches!(r.status().as_u16(), 401 | 403) => {
                let status = r.status();
                let body: Json = r.json().await.unwrap_or_default();
                let message = body
                    .pointer("/error/message")
                    .and_then(Json::as_str)
                    .unwrap_or_default();
                KeyCheck::Invalid(format!("HTTP {status} {message}").trim().to_owned())
            }
            Ok(r) => KeyCheck::Unknown(format!("HTTP {}", r.status())),
            Err(err) => KeyCheck::Unknown(err.without_url().to_string()),
        }
    }

    async fn models(&self) -> Result<Vec<ModelInfo>> {
        let body: Json = self
            .http
            .get(format!("{}/models", self.base_url))
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;
        Ok(parse_models(&body))
    }
}

fn parse_models(body: &Json) -> Vec<ModelInfo> {
    let price = |m: &Json, k: &str| {
        m.pointer(&format!("/pricing/{k}"))
            .and_then(Json::as_str)
            .and_then(|p| p.parse::<f64>().ok())
            .map(|p| p * 1_000_000.0)
    };
    body.get("data")
        .and_then(Json::as_array)
        .into_iter()
        .flatten()
        .filter_map(|m| {
            let id = m.get("id")?.as_str()?.to_owned();
            let has = |path: &str, what: &str| {
                m.pointer(path)
                    .and_then(Json::as_array)
                    .is_some_and(|a| a.iter().any(|v| v.as_str() == Some(what)))
            };
            Some(ModelInfo {
                name: m
                    .get("name")
                    .and_then(Json::as_str)
                    .unwrap_or(&id)
                    .to_owned(),
                context: m.get("context_length").and_then(Json::as_u64).unwrap_or(0),
                price: price(m, "prompt").zip(price(m, "completion")),
                images: has("/architecture/input_modalities", "image"),
                tools: has("/supported_parameters", "tools"),
                id,
            })
        })
        .collect()
}

const WELCOME: Tr = tr(
    "Welcome to OpenClaw. A few questions and you are ready; everything else can be changed later with `openclaw-rs config`.",
    "欢迎使用 OpenClaw。回答几个问题就能开始用；其他设置以后可以用 `openclaw-rs config` 修改。",
);
const STEP: Tr = tr("\n[{}/4] {}", "\n[{}/4] {}");
const STEP_LANGUAGE: Tr = tr("Language", "语言");
const STEP_KEY: Tr = tr("OpenRouter API key", "OpenRouter API key");
const STEP_MODEL: Tr = tr("Main model", "主模型");
const STEP_IMAGES: Tr = tr("Images", "图片");
const PICK_LANGUAGE: Tr = tr("1) 中文  2) English [{}]: ", "1) 中文  2) English [{}]: ");
const KEY_FROM_ENV: Tr = tr(
    "Using the key in $OPENROUTER_API_KEY.",
    "使用环境变量 $OPENROUTER_API_KEY 里的 key。",
);
const KEY_HOW: Tr = tr(
    "The agent's models run on OpenRouter. Create a key at https://openrouter.ai/keys and paste it here (it is not shown while typing).",
    "智能体的模型通过 OpenRouter 调用。请在 https://openrouter.ai/keys 创建 key 并粘贴到这里（输入时不显示）。",
);
const KEY_PROMPT: Tr = tr("API key: ", "API key：");
const KEY_KEEP_PROMPT: Tr = tr(
    "API key (Enter keeps the one in config.toml): ",
    "API key（回车保留 config.toml 里已有的）：",
);
const KEY_CHECKING: Tr = tr("Checking the key...", "正在验证 key……");
const KEY_OK: Tr = tr("✓ The key works.", "✓ key 可用。");
const KEY_BAD: Tr = tr(
    "✗ OpenRouter refused this key ({}). Paste it again.",
    "✗ OpenRouter 拒绝了这个 key（{}），请重新输入。",
);
const KEY_UNCHECKED: Tr = tr(
    "! Could not check the key ({}); it is kept anyway.",
    "! 无法验证 key（{}），先保留它。",
);
const KEY_ENV_BAD: Tr = tr(
    "✗ OpenRouter refused the key in $OPENROUTER_API_KEY ({}). Fix the variable and run `openclaw-rs init` again.",
    "✗ OpenRouter 拒绝了 $OPENROUTER_API_KEY 里的 key（{}）。请修正环境变量后重新运行 `openclaw-rs init`。",
);
const KEY_NEEDED: Tr = tr("A key is required.", "必须填写 key。");
const LOADING_MODELS: Tr = tr("Loading the model list...", "正在获取模型列表……");
const NO_CATALOG: Tr = tr(
    "! Could not load the model list ({}); type a model id instead.",
    "! 无法获取模型列表（{}），请直接输入模型 id。",
);
const KEEP_MODEL: Tr = tr("Keep the model {}? [Y/n]: ", "保留模型 {}？[Y/n]：");
const MODEL_HOW: Tr = tr(
    "Search by name or maker (claude, gpt, gemini, qwen, deepseek...); Enter lists the newest. Only models that can use tools are shown. 🖼 = can see images.",
    "按名称或厂商搜索（claude、gpt、gemini、qwen、deepseek……）；直接回车列出最新的。只显示能调用工具的模型。🖼 = 能看图片。",
);
const SEARCH_PROMPT: Tr = tr("Search: ", "搜索：");
const NO_MATCH: Tr = tr(
    "No model matches; try another word.",
    "没有匹配的模型，换个词试试。",
);
const PICK_PROMPT: Tr = tr(
    "Number, or a full model id; Enter searches again: ",
    "输入编号或完整模型 id；回车重新搜索：",
);
const ID_PROMPT: Tr = tr(
    "Model id (e.g. anthropic/claude-sonnet-4.5): ",
    "模型 id（例如 anthropic/claude-sonnet-4.5）：",
);
const UNKNOWN_ID: Tr = tr(
    "{} is not in the list. Use it anyway? [y/N]: ",
    "{} 不在列表里。仍然使用它？[y/N]：",
);
const NO_TOOLS: Tr = tr(
    "{} cannot use tools, so the agent could not run commands, read files or browse. Use it anyway? [y/N]: ",
    "{} 不能调用工具，智能体将无法执行命令、读文件或浏览网页。仍然使用它？[y/N]：",
);
const CHOSEN: Tr = tr("✓ Model: {}", "✓ 模型：{}");
const SEES_IMAGES: Tr = tr(
    "✓ {} can see images, so it also handles pictures people send and browser screenshots.",
    "✓ {} 能看图片，用户发来的图片和浏览器截图也交给它。",
);
const BLIND: Tr = tr(
    "{} cannot see images. Pick a model for pictures people send and for browser screenshots? Without one they are only listed by name. [Y/n]: ",
    "{} 不能看图片。要为用户发来的图片和浏览器截图选一个图片模型吗？不选的话图片只按文件名列出。[Y/n]：",
);
const IMAGES_UNKNOWN: Tr = tr(
    "Whether {} can see images is unknown; if not, set agent.vision_model later with `openclaw-rs config`.",
    "不确定 {} 能否看图片；如果不能，以后可以用 `openclaw-rs config` 设置 agent.vision_model。",
);
const VISION_CHOSEN: Tr = tr("✓ Image model: {}", "✓ 图片模型：{}");
const BROWSER_FOUND: Tr = tr(
    "✓ Browser: {} (used when the agent needs a real web page).",
    "✓ 浏览器：{}（智能体需要打开真实网页时使用）。",
);
const BROWSER_MISSING: Tr = tr(
    "· No browser found, so the agent has no browser tool. Install Chromium to add one (apk add chromium / apt install chromium) and restart.",
    "· 没有找到浏览器，所以智能体没有浏览器工具。安装 Chromium 即可启用（apk add chromium / apt install chromium），然后重启。",
);
const ABORTED: Tr = tr(
    "Setup ended; nothing was saved.",
    "设置已结束，没有保存任何内容。",
);
const DONE: Tr = tr(
    "Saved {}.\nNext: `openclaw-rs chat` to talk in the terminal, `openclaw-rs serve` for the Web UI, `openclaw-rs config` for QQ, email and everything else.",
    "已保存 {}。\n接下来：`openclaw-rs chat` 在终端里聊天，`openclaw-rs serve` 启动 Web UI，`openclaw-rs config` 设置 QQ、邮件和其他选项。",
);

#[cfg(test)]
pub const ALL: &[Tr] = &[
    WELCOME,
    STEP,
    STEP_LANGUAGE,
    STEP_KEY,
    STEP_MODEL,
    STEP_IMAGES,
    PICK_LANGUAGE,
    KEY_FROM_ENV,
    KEY_HOW,
    KEY_PROMPT,
    KEY_KEEP_PROMPT,
    KEY_CHECKING,
    KEY_OK,
    KEY_BAD,
    KEY_UNCHECKED,
    KEY_ENV_BAD,
    KEY_NEEDED,
    LOADING_MODELS,
    NO_CATALOG,
    KEEP_MODEL,
    MODEL_HOW,
    SEARCH_PROMPT,
    NO_MATCH,
    PICK_PROMPT,
    ID_PROMPT,
    UNKNOWN_ID,
    NO_TOOLS,
    CHOSEN,
    SEES_IMAGES,
    BLIND,
    IMAGES_UNKNOWN,
    VISION_CHOSEN,
    BROWSER_FOUND,
    BROWSER_MISSING,
    ABORTED,
    DONE,
];

/// Whether `config` lacks what a model call needs, so setup should run.
pub fn needed(config: &Config) -> bool {
    config.model_id().is_err() || config.api_key().is_err()
}

/// Whether both ends of the process are a terminal someone can answer in.
pub fn interactive() -> bool {
    // SAFETY: isatty has no preconditions.
    unsafe { libc::isatty(libc::STDIN_FILENO) == 1 && libc::isatty(libc::STDOUT_FILENO) == 1 }
}

/// Runs setup on the process's terminal. Returns the language picked, or
/// `None` when it ended without saving.
pub async fn run(path: &Path, lang: Lang) -> Result<Option<Lang>> {
    let config = Config::load(path).unwrap_or_default();
    let catalog = OpenRouter::new(&config.model.base_url)?;
    let browser = crate::browser::find_installed().filter(|_| config.browser.enabled);
    let mut term = setup::terminal(lang);
    // Typed answers come from a blocking terminal; that is all this process does now.
    wizard(path, &mut term, &catalog, browser).await
}

/// The setup itself, on any terminal and catalog.
pub async fn wizard<R: BufRead, W: Write>(
    path: &Path,
    term: &mut Term<R, W>,
    catalog: &dyn Catalog,
    browser: Option<PathBuf>,
) -> Result<Option<Lang>> {
    let mut doc = setup::load(path)?;
    let config = setup::parse(&doc).unwrap_or_default();
    term.tell(WELCOME, &[])?;

    // 1. Language.
    step(term, 1, STEP_LANGUAGE)?;
    let current = match term.lang {
        Lang::Zh => "1",
        Lang::En => "2",
    };
    let Some(answer) = term.ask(&PICK_LANGUAGE.fill(term.lang, &[current]))? else {
        return aborted(term);
    };
    term.lang = match answer.as_str() {
        "1" | "zh" | "中文" => Lang::Zh,
        "2" | "en" | "English" | "english" => Lang::En,
        _ => term.lang,
    };
    let lang_name = match term.lang {
        Lang::Zh => "zh",
        Lang::En => "en",
    };
    setup::set(&mut doc, &["language"], Value::from(lang_name));

    // 2. Key.
    step(term, 2, STEP_KEY)?;
    match std::env::var("OPENROUTER_API_KEY")
        .ok()
        .filter(|k| !k.trim().is_empty())
    {
        Some(key) => {
            term.tell(KEY_FROM_ENV, &[])?;
            term.tell(KEY_CHECKING, &[])?;
            match catalog.check_key(&key).await {
                KeyCheck::Valid => term.tell(KEY_OK, &[])?,
                KeyCheck::Invalid(why) => {
                    term.tell(KEY_ENV_BAD, &[&why])?;
                    return aborted(term);
                }
                KeyCheck::Unknown(why) => term.tell(KEY_UNCHECKED, &[&why])?,
            }
        }
        None => {
            let existing = config
                .model
                .api_key
                .clone()
                .filter(|k| !k.trim().is_empty());
            if existing.is_none() {
                term.tell(KEY_HOW, &[])?;
            }
            loop {
                let prompt = if existing.is_some() {
                    KEY_KEEP_PROMPT
                } else {
                    KEY_PROMPT
                };
                let Some(typed) = term.secret(prompt.get(term.lang))? else {
                    return aborted(term);
                };
                let key = match (typed.is_empty(), &existing) {
                    (false, _) => typed,
                    (true, Some(key)) => key.clone(),
                    (true, None) => {
                        term.tell(KEY_NEEDED, &[])?;
                        continue;
                    }
                };
                term.tell(KEY_CHECKING, &[])?;
                match catalog.check_key(&key).await {
                    KeyCheck::Valid => term.tell(KEY_OK, &[])?,
                    KeyCheck::Invalid(why) => {
                        term.tell(KEY_BAD, &[&why])?;
                        continue;
                    }
                    KeyCheck::Unknown(why) => term.tell(KEY_UNCHECKED, &[&why])?,
                }
                setup::set(&mut doc, &["model", "api_key"], Value::from(key.as_str()));
                break;
            }
        }
    }

    // 3. Main model.
    step(term, 3, STEP_MODEL)?;
    term.tell(LOADING_MODELS, &[])?;
    let models = match catalog.models().await {
        Ok(models) => models,
        Err(err) => {
            term.tell(NO_CATALOG, &[&format!("{err:#}")])?;
            Vec::new()
        }
    };
    let current = config.model.model.trim().to_owned();
    let mut model = None;
    if !current.is_empty() {
        let Some(answer) = term.ask(&KEEP_MODEL.fill(term.lang, &[&current]))? else {
            return aborted(term);
        };
        if !is_no(&answer) {
            model = Some(current);
        }
    }
    let model = match model {
        Some(model) => model,
        None => match pick(term, &models, false).await? {
            Some(model) => model,
            None => return aborted(term),
        },
    };
    setup::set(&mut doc, &["model", "model"], Value::from(model.as_str()));
    term.tell(CHOSEN, &[&model])?;

    // 4. Images, and the browser that needs no setting.
    step(term, 4, STEP_IMAGES)?;
    match models.iter().find(|m| m.id == model) {
        Some(info) if info.images => term.tell(SEES_IMAGES, &[&model])?,
        Some(_) => {
            let Some(answer) = term.ask(&BLIND.fill(term.lang, &[&model]))? else {
                return aborted(term);
            };
            if !is_no(&answer) {
                let Some(vision) = pick(term, &models, true).await? else {
                    return aborted(term);
                };
                setup::set(
                    &mut doc,
                    &["agent", "vision_model"],
                    Value::from(vision.as_str()),
                );
                term.tell(VISION_CHOSEN, &[&vision])?;
            }
        }
        None => term.tell(IMAGES_UNKNOWN, &[&model])?,
    }
    match &browser {
        Some(path) => term.tell(BROWSER_FOUND, &[&path.display().to_string()])?,
        None => term.tell(BROWSER_MISSING, &[])?,
    }

    save(path, &doc)?;
    term.say("")?;
    term.tell(DONE, &[&path.display().to_string()])?;
    Ok(Some(term.lang))
}

fn step<R: BufRead, W: Write>(term: &mut Term<R, W>, n: usize, title: Tr) -> Result<()> {
    let title = title.get(term.lang);
    term.tell(STEP, &[&n.to_string(), title])
}

fn aborted<R: BufRead, W: Write>(term: &mut Term<R, W>) -> Result<Option<Lang>> {
    term.say("")?;
    term.tell(ABORTED, &[])?;
    Ok(None)
}

fn is_no(answer: &str) -> bool {
    matches!(answer.to_lowercase().as_str(), "n" | "no" | "否" | "不")
}

fn is_yes(answer: &str) -> bool {
    matches!(answer.to_lowercase().as_str(), "y" | "yes" | "是" | "要")
}

fn save(path: &Path, doc: &DocumentMut) -> Result<()> {
    let text = doc.to_string();
    toml::from_str::<Config>(&text).context("the new settings do not load")?;
    setup::write_private(path, &text)
}

/// Lets the person search the catalog and pick a model; `images` only offers
/// models that can see images. `None` when input ended.
async fn pick<R: BufRead, W: Write>(
    term: &mut Term<R, W>,
    models: &[ModelInfo],
    images: bool,
) -> Result<Option<String>> {
    if models.is_empty() {
        loop {
            let Some(id) = term.ask(ID_PROMPT.get(term.lang))? else {
                return Ok(None);
            };
            if !id.is_empty() {
                return Ok(Some(id));
            }
        }
    }
    term.tell(MODEL_HOW, &[])?;
    loop {
        let Some(query) = term.ask(SEARCH_PROMPT.get(term.lang))? else {
            return Ok(None);
        };
        let found = search(models, &query, images);
        if found.is_empty() {
            term.tell(NO_MATCH, &[])?;
            continue;
        }
        for (i, m) in found.iter().enumerate() {
            term.say(&format!("  {:>2}) {}", i + 1, describe(m, term.lang)))?;
        }
        let Some(answer) = term.ask(PICK_PROMPT.get(term.lang))? else {
            return Ok(None);
        };
        if answer.is_empty() {
            continue;
        }
        let chosen = match answer.parse::<usize>() {
            Ok(n) => match found.get(n.wrapping_sub(1)) {
                Some(m) => (*m).clone(),
                None => continue,
            },
            Err(_) => match models.iter().find(|m| m.id == answer) {
                Some(m) => m.clone(),
                None => {
                    let Some(yes) = term.ask(&UNKNOWN_ID.fill(term.lang, &[&answer]))? else {
                        return Ok(None);
                    };
                    if is_yes(&yes) {
                        return Ok(Some(answer));
                    }
                    continue;
                }
            },
        };
        if !chosen.tools {
            let Some(yes) = term.ask(&NO_TOOLS.fill(term.lang, &[&chosen.id]))? else {
                return Ok(None);
            };
            if !is_yes(&yes) {
                continue;
            }
        }
        return Ok(Some(chosen.id));
    }
}

/// Models matching every word of `query` in their id or name, newest first
/// as the catalog lists them. Models without tools, and `:batch` variants
/// (asynchronous batch jobs, not chat), are only taken when typed by id.
fn search<'a>(models: &'a [ModelInfo], query: &str, images: bool) -> Vec<&'a ModelInfo> {
    let words: Vec<String> = query.split_whitespace().map(str::to_lowercase).collect();
    models
        .iter()
        .filter(|m| m.tools && (!images || m.images) && !m.id.ends_with(":batch"))
        .filter(|m| {
            let text = format!("{} {}", m.id, m.name).to_lowercase();
            words.iter().all(|w| text.contains(w.as_str()))
        })
        .take(SHOWN)
        .collect()
}

fn describe(m: &ModelInfo, lang: Lang) -> String {
    let mut parts = vec![m.name.clone()];
    if m.context > 0 {
        let context = if m.context >= 1_000_000 {
            format!("{}M", m.context / 1_000_000)
        } else {
            format!("{}K", m.context / 1000)
        };
        parts.push(match lang {
            Lang::En => format!("{context} context"),
            Lang::Zh => format!("{context} 上下文"),
        });
    }
    if let Some((input, output)) = m.price {
        parts.push(if input == 0.0 && output == 0.0 {
            match lang {
                Lang::En => "free".into(),
                Lang::Zh => "免费".into(),
            }
        } else {
            let unit = match lang {
                Lang::En => "per M tokens in/out",
                Lang::Zh => "每百万 token 输入/输出",
            };
            format!("${} / ${} {unit}", money(input), money(output))
        });
    }
    let eye = if m.images { " 🖼" } else { "" };
    format!("{}{eye}  ({})", m.id, parts.join(" · "))
}

fn money(dollars: f64) -> String {
    let text = if dollars > 0.0 && dollars < 0.1 {
        format!("{dollars:.3}")
    } else {
        format!("{dollars:.2}")
    };
    text.trim_end_matches('0').trim_end_matches('.').to_owned()
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use super::*;

    struct Fake {
        valid: &'static str,
        models: Vec<ModelInfo>,
        checked: Mutex<Vec<String>>,
    }

    #[async_trait]
    impl Catalog for Fake {
        async fn check_key(&self, key: &str) -> KeyCheck {
            self.checked.lock().unwrap().push(key.to_owned());
            if key == self.valid {
                KeyCheck::Valid
            } else {
                KeyCheck::Invalid("HTTP 401".into())
            }
        }
        async fn models(&self) -> Result<Vec<ModelInfo>> {
            Ok(self.models.clone())
        }
    }

    fn model(id: &str, images: bool, tools: bool) -> ModelInfo {
        ModelInfo {
            id: id.into(),
            name: id.into(),
            context: 200_000,
            price: Some((3.0, 15.0)),
            images,
            tools,
        }
    }

    fn fake() -> Fake {
        Fake {
            valid: "sk-good",
            models: vec![
                model("maker/text-only", false, true),
                model("maker/text-only:batch", false, true),
                model("maker/no-tools", true, false),
                model("other/sees", true, true),
            ],
            checked: Mutex::new(Vec::new()),
        }
    }

    async fn drive(path: &Path, catalog: &Fake, script: &str) -> (Option<Lang>, String) {
        // SAFETY: tests that read OPENROUTER_API_KEY do not run in parallel with this.
        unsafe { std::env::remove_var("OPENROUTER_API_KEY") };
        let mut term = Term {
            input: script.as_bytes(),
            out: Vec::new(),
            hide_secrets: false,
            lang: Lang::En,
        };
        let done = wizard(path, &mut term, catalog, None).await.unwrap();
        (done, String::from_utf8(term.out).unwrap())
    }

    #[tokio::test]
    async fn first_run_sets_language_key_model_and_an_image_model() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        let catalog = fake();
        // Chinese; a refused key, then a good one; search "maker", pick the
        // text-only model; yes to an image model, which only lists "other/sees".
        let script = "1\nsk-bad\nsk-good\nmaker\n1\n\n\n1\n";
        let (done, out) = drive(&path, &catalog, script).await;
        assert_eq!(done, Some(Lang::Zh), "{out}");
        assert!(
            out.contains("OpenRouter 拒绝了这个 key（HTTP 401）"),
            "{out}"
        );
        assert!(out.contains("maker/text-only"), "{out}");
        assert!(!out.contains(":batch"), "{out}");
        // Models without tools are not offered.
        assert!(
            !out.contains("1) maker/no-tools") && !out.contains("2) maker/no-tools"),
            "{out}"
        );
        assert!(out.contains("不能看图片"), "{out}");
        assert!(out.contains("没有找到浏览器"), "{out}");
        assert_eq!(*catalog.checked.lock().unwrap(), ["sk-bad", "sk-good"]);
        let config = Config::load(&path).unwrap();
        assert_eq!(config.language, Some(Lang::Zh));
        assert_eq!(config.model.api_key.as_deref(), Some("sk-good"));
        assert_eq!(config.model.model, "maker/text-only");
        assert_eq!(config.agent.vision_model.as_deref(), Some("other/sees"));
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
    }

    #[tokio::test]
    async fn a_model_that_sees_needs_no_image_model_and_settings_are_kept() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(
            &path,
            "# mine\n[model]\napi_key = \"sk-good\"\n[qq]\nenabled = false\n",
        )
        .unwrap();
        let catalog = fake();
        // English; keep the key; pick by full id.
        let script = "2\n\nsees\nother/sees\n";
        let (done, out) = drive(&path, &catalog, script).await;
        assert_eq!(done, Some(Lang::En), "{out}");
        assert!(out.contains("other/sees can see images"), "{out}");
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("# mine") && text.contains("[qq]"), "{text}");
        let config: Config = toml::from_str(&text).unwrap();
        assert_eq!(config.model.model, "other/sees");
        assert_eq!(config.agent.vision_model, None);
    }

    #[tokio::test]
    async fn ending_early_saves_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        let (done, out) = drive(&path, &fake(), "1\nsk-good\n").await;
        assert_eq!(done, None);
        assert!(out.contains("没有保存"), "{out}");
        assert!(!path.exists());
    }

    #[test]
    fn every_text_has_both_languages() {
        crate::i18n::assert_complete(ALL);
    }

    #[test]
    fn catalog_entries_parse_with_price_images_and_tools() {
        let body = serde_json::json!({"data": [
            {"id": "a/b", "name": "A: B", "context_length": 1_000_000,
             "architecture": {"input_modalities": ["text", "image"]},
             "pricing": {"prompt": "0.000003", "completion": "0.000015"},
             "supported_parameters": ["tools", "temperature"]},
            {"id": "c/d"}
        ]});
        let models = parse_models(&body);
        assert_eq!(models[0].id, "a/b");
        assert!(models[0].images && models[0].tools);
        let (input, output) = models[0].price.unwrap();
        assert!((input - 3.0).abs() < 1e-9 && (output - 15.0).abs() < 1e-9);
        assert_eq!(
            describe(&models[0], Lang::En),
            "a/b 🖼  (A: B · 1M context · $3 / $15 per M tokens in/out)"
        );
        assert!(!models[1].tools && models[1].price.is_none());
        assert_eq!(money(0.004), "0.004");
        assert_eq!(money(2.5), "2.5");
        assert_eq!(search(&models, "", false).len(), 1);
    }
}
