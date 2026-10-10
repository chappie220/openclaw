//! `browser`: drives a Chromium-family browser already installed on the host
//! (Chromium, Chrome, Edge, Brave) over the DevTools protocol. Nothing is
//! bundled: without a browser on the host the tool is simply not offered.
//!
//! The browser starts on first use with its own profile under the state
//! directory, gives each conversation its own tab, and is closed again after
//! `browser.idle_secs` without use so it does not hold memory on a small host.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex as SyncMutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};
use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::sync::{Mutex, mpsc, oneshot};
use tokio_tungstenite::tungstenite::Message;

use crate::config::BrowserConfig;

/// Names looked up on `PATH`, in order.
const ON_PATH: &[&str] = &[
    "chromium",
    "chromium-browser",
    "google-chrome-stable",
    "google-chrome",
    "chrome",
    "microsoft-edge-stable",
    "microsoft-edge",
    "brave-browser",
    "brave",
];

/// macOS app bundles, which are not on `PATH`.
const APPS: &[&str] = &[
    "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome",
    "/Applications/Chromium.app/Contents/MacOS/Chromium",
    "/Applications/Microsoft Edge.app/Contents/MacOS/Microsoft Edge",
    "/Applications/Brave Browser.app/Contents/MacOS/Brave Browser",
];

/// Where screenshots are saved, relative to the workspace.
const SCREENSHOTS: &str = "screenshots";

/// Interactive elements listed per page, so the list stays readable.
const MAX_ELEMENTS: usize = 150;

pub struct Browser {
    config: BrowserConfig,
    /// The browser to start; `None` when attaching to `cdp_url`.
    executable: Option<PathBuf>,
    profile: PathBuf,
    workspace: PathBuf,
    running: Arc<Mutex<Option<Running>>>,
    last_used: Arc<SyncMutex<Instant>>,
}

/// One browser action, as the model asks for it.
#[derive(Debug, serde::Deserialize)]
pub struct BrowserArgs {
    pub action: String,
    #[serde(default)]
    pub url: Option<String>,
    #[serde(default, rename = "ref")]
    pub element: Option<u32>,
    #[serde(default)]
    pub selector: Option<String>,
    #[serde(default)]
    pub text: Option<String>,
    #[serde(default)]
    pub submit: bool,
    #[serde(default)]
    pub offset: Option<usize>,
    #[serde(default)]
    pub full_page: bool,
}

impl Browser {
    /// `None` when the browser is turned off or none is installed.
    pub fn new(config: &BrowserConfig, state: &Path, workspace: &Path) -> Option<Self> {
        if !config.enabled {
            return None;
        }
        let attach = config
            .cdp_url
            .as_ref()
            .is_some_and(|u| !u.trim().is_empty());
        let executable = if attach {
            None
        } else {
            Some(match &config.executable {
                Some(path) if !path.as_os_str().is_empty() => path.clone(),
                _ => find_installed()?,
            })
        };
        Some(Self {
            config: config.clone(),
            executable,
            profile: state.join("browser"),
            workspace: workspace.to_owned(),
            running: Arc::new(Mutex::new(None)),
            last_used: Arc::new(SyncMutex::new(Instant::now())),
        })
    }

    /// What the tool runs, for its description.
    pub fn describe(&self) -> String {
        match &self.executable {
            Some(path) => path.display().to_string(),
            None => self.config.cdp_url.clone().unwrap_or_default(),
        }
    }

    /// Runs one action in the tab of `session` and describes the page after it.
    pub async fn run(&self, session: &str, args: BrowserArgs) -> Result<String> {
        let timeout = Duration::from_secs(self.config.timeout_secs.max(1));
        let mut running = self.running.lock().await;
        self.touch();
        if running.as_ref().is_none_or(|r| r.cdp.is_closed()) {
            *running = None;
            *running = Some(tokio::time::timeout(timeout, self.start()).await.map_err(
                |_| anyhow!("the browser did not start within {}s", timeout.as_secs()),
            )??);
            self.spawn_reaper();
        }
        let browser = running.as_mut().expect("started above");
        let result = match tokio::time::timeout(timeout, self.act(browser, session, &args)).await {
            Ok(result) => result,
            Err(_) => Err(anyhow!(
                "browser action timed out after {}s",
                timeout.as_secs()
            )),
        };
        // A tab that crashed or was closed is replaced on the next action.
        if let Err(err) = &result
            && err.to_string().contains("No session with given id")
        {
            browser.tabs.remove(session);
        }
        self.touch();
        result
    }

    fn touch(&self) {
        *self.last_used.lock().expect("last_used lock") = Instant::now();
    }

    /// Closes the browser once it has been idle for `idle_secs`.
    fn spawn_reaper(&self) {
        let running = Arc::clone(&self.running);
        let last_used = Arc::clone(&self.last_used);
        let idle = Duration::from_secs(self.config.idle_secs.max(10));
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(Duration::from_secs(10)).await;
                let mut guard = running.lock().await;
                let Some(browser) = guard.as_mut() else {
                    return;
                };
                if browser.cdp.is_closed() {
                    *guard = None;
                    return;
                }
                if last_used.lock().expect("last_used lock").elapsed() >= idle {
                    browser.close_tabs().await;
                    *guard = None;
                    eprintln!("browser: closed after {}s idle", idle.as_secs());
                    return;
                }
            }
        });
    }

    async fn start(&self) -> Result<Running> {
        let Some(executable) = &self.executable else {
            let url = self.config.cdp_url.as_deref().unwrap_or_default();
            let ws = devtools_ws_url(url).await?;
            return Ok(Running {
                cdp: Cdp::connect(&ws).await?,
                tabs: HashMap::new(),
                _process: None,
            });
        };
        std::fs::create_dir_all(&self.profile)
            .with_context(|| format!("cannot create {}", self.profile.display()))?;
        let mut command = tokio::process::Command::new(executable);
        command
            .args(launch_args(&self.config, &self.profile))
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            // Own process group, so closing it also ends the renderers.
            .process_group(0);
        #[cfg(target_os = "linux")]
        // SAFETY: prctl is async-signal-safe; it only asks the kernel to end
        // the browser if the Gateway dies without closing it.
        unsafe {
            command.pre_exec(|| {
                libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGTERM);
                Ok(())
            });
        }
        let mut child = command
            .spawn()
            .with_context(|| format!("cannot start {}", executable.display()))?;
        let process = BrowserProcess(child.id());
        let stderr = child.stderr.take().expect("piped stderr");
        let mut lines = BufReader::new(stderr).lines();
        let mut seen = Vec::new();
        let ws = loop {
            match lines.next_line().await? {
                Some(line) => {
                    if let Some(url) = line.split("DevTools listening on ").nth(1) {
                        break url.trim().to_owned();
                    }
                    if seen.len() < 5 {
                        seen.push(line);
                    }
                }
                None => bail!(
                    "{} exited before it was ready: {}",
                    executable.display(),
                    seen.join(" | ")
                ),
            }
        };
        // Keep draining the browser's log so it never blocks on a full pipe.
        tokio::spawn(async move { while let Ok(Some(_)) = lines.next_line().await {} });
        eprintln!("browser: started {}", executable.display());
        Ok(Running {
            cdp: Cdp::connect(&ws).await?,
            tabs: HashMap::new(),
            _process: Some((child, process)),
        })
    }

    async fn act(
        &self,
        browser: &mut Running,
        session: &str,
        args: &BrowserArgs,
    ) -> Result<String> {
        let tab = browser.tab(session).await?;
        let cdp = &browser.cdp;
        match args.action.as_str() {
            "open" => {
                let url = web_url(args.url.as_deref().unwrap_or_default())?;
                let result = cdp
                    .call(Some(&tab), "Page.navigate", json!({"url": url}))
                    .await?;
                if let Some(error) = result.get("errorText").and_then(Value::as_str) {
                    bail!("cannot open {url}: {error}");
                }
                wait_for_load(cdp, &tab).await;
                self.snapshot(cdp, &tab, 0).await
            }
            "read" => self.snapshot(cdp, &tab, args.offset.unwrap_or(0)).await,
            "click" => {
                let selector = target_selector(args)?;
                let (x, y) = locate(cdp, &tab, &selector).await?;
                for kind in ["mousePressed", "mouseReleased"] {
                    cdp.call(
                        Some(&tab),
                        "Input.dispatchMouseEvent",
                        json!({"type": kind, "x": x, "y": y, "button": "left", "clickCount": 1}),
                    )
                    .await?;
                }
                settle(cdp, &tab).await;
                self.snapshot(cdp, &tab, 0).await
            }
            "type" => {
                let selector = target_selector(args)?;
                let text = args.text.as_deref().context("type needs text")?;
                let focused = evaluate(
                    cdp,
                    &tab,
                    &format!(
                        "(() => {{ const e = document.querySelector({}); if (!e) return false; \
                         e.scrollIntoView({{block: 'center'}}); e.focus(); \
                         if ('value' in e) e.value = ''; else if (e.isContentEditable) e.textContent = ''; \
                         return true; }})()",
                        serde_json::to_string(&selector)?
                    ),
                )
                .await?;
                if focused != Value::Bool(true) {
                    bail!("no element matches {selector}; read the page again for current refs");
                }
                cdp.call(Some(&tab), "Input.insertText", json!({"text": text}))
                    .await?;
                if args.submit {
                    for kind in ["keyDown", "keyUp"] {
                        cdp.call(
                            Some(&tab),
                            "Input.dispatchKeyEvent",
                            json!({"type": kind, "key": "Enter", "code": "Enter",
                                   "windowsVirtualKeyCode": 13, "text": "\r"}),
                        )
                        .await?;
                    }
                }
                settle(cdp, &tab).await;
                self.snapshot(cdp, &tab, 0).await
            }
            "back" => {
                evaluate(cdp, &tab, "history.back()").await?;
                settle(cdp, &tab).await;
                self.snapshot(cdp, &tab, 0).await
            }
            "screenshot" => {
                // A whole page is large, so it goes as JPEG; providers also
                // refuse images taller than about 8000 px.
                let (format, params) = if args.full_page {
                    let metrics = cdp
                        .call(Some(&tab), "Page.getLayoutMetrics", json!({}))
                        .await?;
                    let size = metrics.get("cssContentSize").cloned().unwrap_or_default();
                    let dim = |k: &str| size.get(k).and_then(Value::as_f64).unwrap_or(0.0);
                    let clip = json!({"x": 0, "y": 0, "width": dim("width"),
                                      "height": dim("height").min(8000.0), "scale": 1});
                    let params = json!({"format": "jpeg", "quality": 80,
                                        "captureBeyondViewport": true, "clip": clip});
                    ("jpg", params)
                } else {
                    ("png", json!({"format": "png"}))
                };
                let shot = cdp
                    .call(Some(&tab), "Page.captureScreenshot", params)
                    .await?;
                let data = shot
                    .get("data")
                    .and_then(Value::as_str)
                    .context("the browser returned no image")?;
                use base64::Engine;
                let image = base64::engine::general_purpose::STANDARD.decode(data)?;
                tokio::fs::create_dir_all(self.workspace.join(SCREENSHOTS)).await?;
                let relative = format!(
                    "{SCREENSHOTS}/{}.{format}",
                    chrono::Local::now().format("%Y%m%d-%H%M%S%.3f")
                );
                let path = self.workspace.join(&relative);
                tokio::fs::write(&path, &image).await?;
                Ok(format!(
                    "{}\nScreenshot saved ({} KB) to {}.",
                    crate::attachments::tool_image_line(&relative),
                    image.len() / 1024,
                    path.display()
                ))
            }
            other => bail!("unknown browser action {other:?}"),
        }
    }

    /// Title, address, a window of the visible text, and numbered elements.
    async fn snapshot(&self, cdp: &Cdp, tab: &str, offset: usize) -> Result<String> {
        let page = evaluate(cdp, tab, &snapshot_script(MAX_ELEMENTS)).await?;
        Ok(render_snapshot(
            &page,
            offset,
            self.config.max_chars.max(500),
        ))
    }
}

/// A started (or attached) browser and the tab of each conversation.
struct Running {
    cdp: Cdp,
    /// Conversation → DevTools session of its tab.
    tabs: HashMap<String, Tab>,
    /// Held only so dropping `Running` ends the browser.
    _process: Option<(tokio::process::Child, BrowserProcess)>,
}

struct Tab {
    target: String,
    session: String,
}

impl Running {
    async fn tab(&mut self, session: &str) -> Result<String> {
        if let Some(tab) = self.tabs.get(session) {
            return Ok(tab.session.clone());
        }
        let created = self
            .cdp
            .call(None, "Target.createTarget", json!({"url": "about:blank"}))
            .await?;
        let target = created
            .get("targetId")
            .and_then(Value::as_str)
            .context("the browser opened no tab")?
            .to_owned();
        let attached = self
            .cdp
            .call(
                None,
                "Target.attachToTarget",
                json!({"targetId": target, "flatten": true}),
            )
            .await?;
        let id = attached
            .get("sessionId")
            .and_then(Value::as_str)
            .context("cannot attach to the new tab")?
            .to_owned();
        self.tabs.insert(
            session.to_owned(),
            Tab {
                target,
                session: id.clone(),
            },
        );
        Ok(id)
    }

    /// Closes this program's tabs; matters when attached to someone's own browser.
    async fn close_tabs(&mut self) {
        for (_, tab) in self.tabs.drain() {
            let close = self
                .cdp
                .call(None, "Target.closeTarget", json!({"targetId": tab.target}));
            let _ = tokio::time::timeout(Duration::from_secs(2), close).await;
        }
    }
}

/// Ends the browser's whole process group when dropped.
struct BrowserProcess(Option<u32>);

impl Drop for BrowserProcess {
    fn drop(&mut self) {
        if let Some(pid) = self.0 {
            // SAFETY: signalling the process group we created; failure is harmless.
            unsafe { libc::kill(-(pid as i32), libc::SIGTERM) };
        }
    }
}

type Pending = Arc<SyncMutex<Option<HashMap<u64, oneshot::Sender<Result<Value, String>>>>>>;

/// A DevTools protocol connection: commands go out, replies are matched by id.
struct Cdp {
    outgoing: mpsc::UnboundedSender<Message>,
    /// `None` once the connection is gone.
    pending: Pending,
    next_id: AtomicU64,
}

impl Cdp {
    async fn connect(url: &str) -> Result<Self> {
        let (socket, _) = tokio_tungstenite::connect_async(url)
            .await
            .with_context(|| format!("cannot connect to the browser at {url}"))?;
        let (mut sink, mut stream) = socket.split();
        let (outgoing, mut queue) = mpsc::unbounded_channel::<Message>();
        tokio::spawn(async move {
            while let Some(message) = queue.recv().await {
                if sink.send(message).await.is_err() {
                    break;
                }
            }
        });
        let pending: Pending = Arc::new(SyncMutex::new(Some(HashMap::new())));
        let replies = Arc::clone(&pending);
        tokio::spawn(async move {
            while let Some(Ok(message)) = stream.next().await {
                let Message::Text(text) = message else {
                    continue;
                };
                let Ok(reply) = serde_json::from_str::<Value>(&text) else {
                    continue;
                };
                let Some(id) = reply.get("id").and_then(Value::as_u64) else {
                    continue;
                };
                let waiter = replies
                    .lock()
                    .expect("pending lock")
                    .as_mut()
                    .and_then(|p| p.remove(&id));
                if let Some(waiter) = waiter {
                    let result = match reply.get("error") {
                        Some(error) => Err(error
                            .get("message")
                            .and_then(Value::as_str)
                            .unwrap_or("unknown error")
                            .to_owned()),
                        None => Ok(reply.get("result").cloned().unwrap_or_default()),
                    };
                    let _ = waiter.send(result);
                }
            }
            // Wakes every waiter with "connection closed".
            replies.lock().expect("pending lock").take();
        });
        Ok(Self {
            outgoing,
            pending,
            next_id: AtomicU64::new(1),
        })
    }

    fn is_closed(&self) -> bool {
        self.pending.lock().expect("pending lock").is_none()
    }

    async fn call(&self, session: Option<&str>, method: &str, params: Value) -> Result<Value> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let (reply, answer) = oneshot::channel();
        match self.pending.lock().expect("pending lock").as_mut() {
            Some(pending) => pending.insert(id, reply),
            None => bail!("the browser connection is closed"),
        };
        let mut message = json!({"id": id, "method": method, "params": params});
        if let Some(session) = session {
            message["sessionId"] = json!(session);
        }
        self.outgoing
            .send(Message::Text(message.to_string().into()))
            .map_err(|_| anyhow!("the browser connection is closed"))?;
        match answer.await {
            Ok(Ok(result)) => Ok(result),
            Ok(Err(error)) => bail!("{method}: {error}"),
            Err(_) => bail!("the browser connection closed during {method}"),
        }
    }
}

/// The first Chromium-family browser on `PATH`, else in `/Applications`.
pub fn find_installed() -> Option<PathBuf> {
    let path = std::env::var_os("PATH").unwrap_or_default();
    let dirs: Vec<PathBuf> = std::env::split_paths(&path).collect();
    ON_PATH
        .iter()
        .flat_map(|name| dirs.iter().map(move |dir| dir.join(name)))
        .chain(APPS.iter().map(PathBuf::from))
        .find(|candidate| is_executable(candidate))
}

fn is_executable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(path).is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
}

fn launch_args(config: &BrowserConfig, profile: &Path) -> Vec<String> {
    let mut args = vec![
        "--remote-debugging-port=0".to_owned(),
        format!("--user-data-dir={}", profile.display()),
        "--no-first-run".into(),
        "--no-default-browser-check".into(),
        "--disable-background-networking".into(),
        "--disable-sync".into(),
        "--disable-extensions".into(),
        "--disable-dev-shm-usage".into(),
        "--mute-audio".into(),
        "--window-size=1280,900".into(),
    ];
    if config.headless {
        args.push("--headless=new".into());
        args.push("--disable-gpu".into());
    }
    // Chromium refuses to start its sandbox as root (containers, some Pi setups).
    // SAFETY: geteuid has no preconditions.
    if unsafe { libc::geteuid() } == 0 {
        args.push("--no-sandbox".into());
    }
    args.extend(config.args.iter().cloned());
    args.push("about:blank".into());
    args
}

/// The browser-level WebSocket of a running browser: `ws://…` as given, or
/// looked up from an `http://host:port` DevTools endpoint.
async fn devtools_ws_url(url: &str) -> Result<String> {
    let url = url.trim().trim_end_matches('/');
    if url.starts_with("ws://") || url.starts_with("wss://") {
        return Ok(url.to_owned());
    }
    let version: Value = reqwest::Client::new()
        .get(format!("{url}/json/version"))
        .timeout(Duration::from_secs(10))
        .send()
        .await
        .with_context(|| format!("no browser answers at {url}"))?
        .json()
        .await
        .with_context(|| format!("{url}/json/version is not a DevTools endpoint"))?;
    version
        .get("webSocketDebuggerUrl")
        .and_then(Value::as_str)
        .map(str::to_owned)
        .with_context(|| format!("{url} reports no webSocketDebuggerUrl"))
}

/// Only web pages: `file:`, `chrome:` and script URLs would reach past the web.
fn web_url(input: &str) -> Result<String> {
    let input = input.trim();
    if input.is_empty() {
        bail!("open needs a url");
    }
    let with_scheme = if input.contains("://") {
        input.to_owned()
    } else {
        format!("https://{input}")
    };
    let url =
        reqwest::Url::parse(&with_scheme).with_context(|| format!("invalid url {input:?}"))?;
    match url.scheme() {
        "http" | "https" => Ok(url.into()),
        other => bail!("only http and https pages can be opened, not {other}:"),
    }
}

fn target_selector(args: &BrowserArgs) -> Result<String> {
    match (args.element, args.selector.as_deref().map(str::trim)) {
        (Some(n), _) => Ok(format!("[data-oc=\"{n}\"]")),
        (None, Some(selector)) if !selector.is_empty() => Ok(selector.to_owned()),
        _ => bail!(
            "{} needs ref (from the element list) or selector",
            args.action
        ),
    }
}

async fn evaluate(cdp: &Cdp, tab: &str, expression: &str) -> Result<Value> {
    let result = cdp
        .call(
            Some(tab),
            "Runtime.evaluate",
            json!({"expression": expression, "returnByValue": true, "awaitPromise": true}),
        )
        .await?;
    if let Some(details) = result.get("exceptionDetails") {
        let text = details
            .pointer("/exception/description")
            .or_else(|| details.get("text"))
            .and_then(Value::as_str)
            .unwrap_or("script error");
        bail!("{text}");
    }
    Ok(result.pointer("/result/value").cloned().unwrap_or_default())
}

/// The viewport centre of the element, scrolled into view.
async fn locate(cdp: &Cdp, tab: &str, selector: &str) -> Result<(f64, f64)> {
    let point = evaluate(
        cdp,
        tab,
        &format!(
            "(() => {{ const e = document.querySelector({}); if (!e) return null; \
             e.scrollIntoView({{block: 'center', inline: 'center'}}); \
             const r = e.getBoundingClientRect(); return [r.x + r.width / 2, r.y + r.height / 2]; }})()",
            serde_json::to_string(selector)?
        ),
    )
    .await?;
    match point.as_array().map(|p| (p[0].as_f64(), p[1].as_f64())) {
        Some((Some(x), Some(y))) => Ok((x, y)),
        _ => bail!("no element matches {selector}; read the page again for current refs"),
    }
}

/// Waits until the page has loaded; a slow page is read as far as it got.
async fn wait_for_load(cdp: &Cdp, tab: &str) {
    for _ in 0..100 {
        tokio::time::sleep(Duration::from_millis(200)).await;
        if let Ok(Value::String(state)) = evaluate(cdp, tab, "document.readyState").await
            && state == "complete"
        {
            break;
        }
    }
    // Give scripts a moment to render what they fetch.
    tokio::time::sleep(Duration::from_millis(300)).await;
}

/// After a click or key press, which may or may not start a navigation.
async fn settle(cdp: &Cdp, tab: &str) {
    tokio::time::sleep(Duration::from_millis(400)).await;
    wait_for_load(cdp, tab).await;
}

/// Numbers the visible interactive elements (`data-oc`) and returns them with
/// the page's title, address and text.
fn snapshot_script(max: usize) -> String {
    format!(
        r#"(() => {{
  document.querySelectorAll('[data-oc]').forEach(e => e.removeAttribute('data-oc'));
  const clean = s => String(s || '').replace(/\s+/g, ' ').trim().slice(0, 80);
  const visible = e => {{
    const r = e.getBoundingClientRect(), s = getComputedStyle(e);
    return r.width > 0 && r.height > 0 && s.visibility !== 'hidden' && s.display !== 'none';
  }};
  const name = e => clean(e.innerText || e.value || e.getAttribute('aria-label') || e.title || e.alt);
  const items = [];
  let n = 0;
  for (const e of document.querySelectorAll('a[href], button, input:not([type=hidden]), textarea, select, [role=button], [role=link], [contenteditable=true]')) {{
    if (n >= {max}) break;
    if (!visible(e)) continue;
    n++;
    e.setAttribute('data-oc', n);
    const tag = e.tagName.toLowerCase();
    const type = tag === 'input' ? (e.type || 'text') : tag;
    let d;
    if (tag === 'a') d = `link "${{name(e)}}" -> ${{e.href}}`;
    else if (['submit', 'button', 'reset', 'image'].includes(type) || tag === 'button' || !(tag === 'input' || tag === 'textarea' || tag === 'select' || e.isContentEditable))
      d = `button "${{name(e)}}"`;
    else {{
      const parts = [type === 'checkbox' || type === 'radio' ? type + (e.checked ? ' (checked)' : '') : type === 'select' ? 'select' : 'field'];
      if (e.name) parts.push(`name=${{clean(e.name)}}`);
      if (e.placeholder) parts.push(`placeholder="${{clean(e.placeholder)}}"`);
      if (e.getAttribute('aria-label')) parts.push(`label="${{clean(e.getAttribute('aria-label'))}}"`);
      if (e.value && type !== 'password' && type !== 'checkbox' && type !== 'radio') parts.push(`value="${{clean(e.value)}}"`);
      d = parts.join(' ');
    }}
    items.push(`[${{n}}] ${{d}}`);
  }}
  const text = (document.body ? document.body.innerText : '').replace(/[ \t]+\n/g, '\n').replace(/\n{{3,}}/g, '\n\n').trim();
  return {{title: document.title, url: location.href, text, items}};
}})()"#
    )
}

fn render_snapshot(page: &Value, offset: usize, max_chars: usize) -> String {
    let field = |k: &str| page.get(k).and_then(Value::as_str).unwrap_or("");
    let text: Vec<char> = field("text").chars().collect();
    let start = offset.min(text.len());
    let end = (start + max_chars).min(text.len());
    let mut out = format!("Title: {}\nURL: {}\n\n", field("title"), field("url"));
    out.extend(&text[start..end]);
    if start > 0 || end < text.len() {
        out.push_str(&format!(
            "\n\n[text characters {start}-{end} of {}{}]",
            text.len(),
            if end < text.len() {
                format!("; read with offset={end} for more")
            } else {
                String::new()
            }
        ));
    }
    let items: Vec<&str> = page
        .get("items")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .collect();
    if !items.is_empty() {
        out.push_str("\n\nElements (use ref with click or type):\n");
        out.push_str(&items.join("\n"));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_web_pages_open() {
        assert_eq!(web_url("example.com/a").unwrap(), "https://example.com/a");
        assert_eq!(web_url(" http://x.test ").unwrap(), "http://x.test/");
        for bad in [
            "file:///etc/passwd",
            "chrome://settings",
            "javascript:alert(1)",
            "",
        ] {
            assert!(web_url(bad).is_err(), "{bad} should be refused");
        }
    }

    #[test]
    fn elements_are_addressed_by_ref_or_selector() {
        let args = |element, selector: Option<&str>| BrowserArgs {
            action: "click".into(),
            url: None,
            element,
            selector: selector.map(str::to_owned),
            text: None,
            submit: false,
            offset: None,
            full_page: false,
        };
        assert_eq!(
            target_selector(&args(Some(3), None)).unwrap(),
            "[data-oc=\"3\"]"
        );
        assert_eq!(target_selector(&args(None, Some("#q"))).unwrap(), "#q");
        assert!(target_selector(&args(None, Some(" "))).is_err());
    }

    #[test]
    fn snapshot_pages_long_text_and_lists_elements() {
        let page = json!({
            "title": "Example", "url": "https://example.com/",
            "text": "abcdefghij", "items": ["[1] link \"More\" -> https://example.com/more"]
        });
        let first = render_snapshot(&page, 0, 4);
        assert!(first.starts_with("Title: Example\nURL: https://example.com/\n\nabcd\n"));
        assert!(first.contains("[text characters 0-4 of 10; read with offset=4 for more]"));
        assert!(first.ends_with("[1] link \"More\" -> https://example.com/more"));
        let last = render_snapshot(&page, 8, 4);
        assert!(last.contains("ij\n\n[text characters 8-10 of 10]"));
        assert!(!render_snapshot(&page, 0, 100).contains("text characters"));
    }

    #[test]
    fn missing_browser_means_no_tool() {
        let dir = tempfile::tempdir().unwrap();
        let off = BrowserConfig {
            enabled: false,
            ..BrowserConfig::default()
        };
        assert!(Browser::new(&off, dir.path(), dir.path()).is_none());
        let attach = BrowserConfig {
            cdp_url: Some("http://127.0.0.1:9222".into()),
            ..BrowserConfig::default()
        };
        let browser = Browser::new(&attach, dir.path(), dir.path()).unwrap();
        assert!(browser.executable.is_none());
    }
}

/// Drives a real installed browser against a local page:
/// `OPENCLAW_TEST_BROWSER=/path/to/chrome cargo test -- --ignored browser`.
#[cfg(test)]
mod live {
    use super::*;

    #[tokio::test]
    #[ignore = "needs a Chromium-family browser on the host"]
    async fn opens_types_clicks_and_screenshots_a_local_page() {
        use axum::{Router, extract::Query, response::Html, routing::get};
        let app = Router::new()
            .route(
                "/",
                get(|| async {
                    Html(
                        "<title>Form</title><p>Hello browser</p>\
                         <form action=/echo><input name=q placeholder=Search></form>\
                         <a href=/echo?q=link>Go</a>",
                    )
                }),
            )
            .route(
                "/echo",
                get(|Query(q): Query<HashMap<String, String>>| async move {
                    Html(format!("<title>Echo</title>you sent {}", q["q"]))
                }),
            );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

        let dir = tempfile::tempdir().unwrap();
        let config = BrowserConfig {
            executable: std::env::var_os("OPENCLAW_TEST_BROWSER").map(PathBuf::from),
            ..BrowserConfig::default()
        };
        let browser = Browser::new(&config, dir.path(), dir.path()).expect("no browser found");
        let args = |action: &str| BrowserArgs {
            action: action.into(),
            url: None,
            element: None,
            selector: None,
            text: None,
            submit: false,
            offset: None,
            full_page: false,
        };

        let page = browser
            .run(
                "s1",
                BrowserArgs {
                    url: Some(format!("http://{addr}/")),
                    ..args("open")
                },
            )
            .await
            .unwrap();
        assert!(
            page.contains("Title: Form") && page.contains("Hello browser"),
            "{page}"
        );
        assert!(
            page.contains("[1] field name=q placeholder=\"Search\""),
            "{page}"
        );
        assert!(page.contains("[2] link \"Go\""), "{page}");

        let typed = browser
            .run(
                "s1",
                BrowserArgs {
                    element: Some(1),
                    text: Some("你好".into()),
                    submit: true,
                    ..args("type")
                },
            )
            .await
            .unwrap();
        assert!(typed.contains("you sent 你好"), "{typed}");

        let back = browser.run("s1", args("back")).await.unwrap();
        assert!(back.contains("Title: Form"), "{back}");
        let clicked = browser
            .run(
                "s1",
                BrowserArgs {
                    element: Some(2),
                    ..args("click")
                },
            )
            .await
            .unwrap();
        assert!(clicked.contains("you sent link"), "{clicked}");

        // Another conversation has its own tab.
        let other = browser.run("s2", args("read")).await.unwrap();
        assert!(other.contains("URL: about:blank"), "{other}");

        let shot = browser.run("s1", args("screenshot")).await.unwrap();
        let path = crate::attachments::tool_image(&shot).unwrap();
        assert!(
            path.starts_with("screenshots/") && path.ends_with(".png"),
            "{shot}"
        );
        let png = std::fs::read(dir.path().join(path)).unwrap();
        assert!(png.starts_with(b"\x89PNG"));
        let full = browser
            .run(
                "s1",
                BrowserArgs {
                    full_page: true,
                    ..args("screenshot")
                },
            )
            .await
            .unwrap();
        let jpg = std::fs::read(
            dir.path()
                .join(crate::attachments::tool_image(&full).unwrap()),
        )
        .unwrap();
        assert!(jpg.starts_with(b"\xff\xd8"));
        let refused = browser
            .run(
                "s1",
                BrowserArgs {
                    url: Some("file:///etc/passwd".into()),
                    ..args("open")
                },
            )
            .await;
        assert!(refused.is_err());
    }
}
