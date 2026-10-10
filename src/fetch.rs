//! `web_fetch`: downloads a page and turns its HTML into text, without a
//! browser. Pages on this host and the local network are refused unless
//! `fetch.private_network` allows them for owners, so the tool cannot be used
//! to reach the router or other devices.

use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use reqwest::Url;
use reqwest::dns::{Addrs, Name, Resolve, Resolving};

use crate::config::FetchConfig;

/// Redirects followed before giving up.
const MAX_REDIRECTS: usize = 5;
/// Pages kept so that reading on with `offset` does not download again.
const CACHE_PAGES: usize = 8;
const CACHE_TTL: Duration = Duration::from_secs(600);

pub struct Fetcher {
    /// Resolves names to public addresses only.
    public: reqwest::Client,
    /// For owners when `private_network` is on.
    any: reqwest::Client,
    config: FetchConfig,
    cache: Mutex<Vec<Page>>,
}

#[derive(Clone)]
struct Page {
    /// Address as asked for, the cache key.
    asked: String,
    fetched: Instant,
    private: bool,
    url: String,
    title: String,
    text: String,
    /// The body was longer than `max_bytes`.
    cut: bool,
}

impl Fetcher {
    /// `None` when the tool is turned off.
    pub fn new(config: &FetchConfig) -> Result<Option<Self>> {
        if !config.enabled {
            return Ok(None);
        }
        let build = |public: bool| {
            let builder = reqwest::Client::builder()
                .connect_timeout(Duration::from_secs(15))
                .timeout(Duration::from_secs(config.timeout_secs.max(1)))
                // Each hop is checked before it is followed.
                .redirect(reqwest::redirect::Policy::none())
                .user_agent(concat!(
                    "Mozilla/5.0 (compatible; OpenClaw/",
                    env!("CARGO_PKG_VERSION"),
                    ")"
                ));
            if public {
                builder.dns_resolver(PublicOnly)
            } else {
                builder
            }
            .build()
            .context("cannot build HTTP client for web_fetch")
        };
        Ok(Some(Self {
            public: build(true)?,
            any: build(false)?,
            config: config.clone(),
            cache: Mutex::new(Vec::new()),
        }))
    }

    /// The page's text from character `offset` on, at most `max_chars` of it.
    /// `private` lets this call reach the local network (owners, when allowed).
    pub async fn fetch(&self, url: &str, offset: usize, private: bool) -> Result<String> {
        let private = private && self.config.private_network;
        let asked = web_url(url)?;
        let cached = self
            .cache
            .lock()
            .unwrap()
            .iter()
            .find(|p| p.asked == asked.as_str() && p.private == private)
            .filter(|p| p.fetched.elapsed() < CACHE_TTL)
            .cloned();
        let page = match cached {
            Some(page) => page,
            None => {
                let page = self.download(asked, private).await?;
                let mut cache = self.cache.lock().unwrap();
                cache.retain(|p| p.asked != page.asked && p.fetched.elapsed() < CACHE_TTL);
                if cache.len() >= CACHE_PAGES {
                    cache.remove(0);
                }
                cache.push(page.clone());
                page
            }
        };
        Ok(render(
            &page,
            offset,
            self.config.max_chars.max(500),
            self.config.max_bytes,
        ))
    }

    async fn download(&self, asked: Url, private: bool) -> Result<Page> {
        let http = if private { &self.any } else { &self.public };
        let mut url = asked.clone();
        let mut hops = 0;
        let mut response = loop {
            if !private {
                check_public(&url).await?;
            }
            let response = http
                .get(url.clone())
                .header(
                    reqwest::header::ACCEPT,
                    "text/html,application/xhtml+xml,text/plain;q=0.9,*/*;q=0.5",
                )
                .send()
                .await
                .map_err(|e| anyhow::anyhow!("cannot fetch {url}: {}", error_chain(&e)))?;
            if !response.status().is_redirection() {
                break response;
            }
            let next = response
                .headers()
                .get(reqwest::header::LOCATION)
                .and_then(|v| v.to_str().ok())
                .and_then(|location| url.join(location).ok())
                .with_context(|| format!("{url} redirects without a usable Location"))?;
            hops += 1;
            if hops > MAX_REDIRECTS {
                bail!("{asked} redirects more than {MAX_REDIRECTS} times");
            }
            url = web_url(next.as_str())?;
        };
        let status = response.status();
        if !status.is_success() {
            bail!("{url} answered HTTP {status}");
        }
        let content_type = response
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_ascii_lowercase();
        let kind = kind_of(&content_type);
        if kind == Kind::Other {
            bail!(
                "{url} is {}, not a text page; use the browser or shell to handle it",
                content_type.split(';').next().unwrap_or("").trim()
            );
        }
        let mut body = Vec::new();
        let mut cut = false;
        while let Some(chunk) = response
            .chunk()
            .await
            .map_err(|e| anyhow::anyhow!("reading {url} failed: {}", error_chain(&e)))?
        {
            let room = self.config.max_bytes.saturating_sub(body.len());
            if chunk.len() >= room {
                body.extend_from_slice(&chunk[..room]);
                cut = chunk.len() > room;
                if cut {
                    break;
                }
            } else {
                body.extend_from_slice(&chunk);
            }
        }
        let html = match kind {
            Kind::Html => true,
            Kind::Text => false,
            // No type given: HTML if it looks like it.
            Kind::Unknown => body
                .iter()
                .find(|b| !b.is_ascii_whitespace())
                .is_some_and(|b| *b == b'<'),
            Kind::Other => unreachable!(),
        };
        let source = decode(&body, &content_type, html);
        let (title, text) = if html {
            html_to_text(&source, &url)
        } else {
            (String::new(), source.trim().to_owned())
        };
        Ok(Page {
            asked: asked.into(),
            fetched: Instant::now(),
            private,
            url: url.into(),
            title,
            text,
            cut,
        })
    }
}

/// reqwest's own message hides the reason ("error sending request").
fn error_chain(err: &(dyn std::error::Error + 'static)) -> String {
    let mut parts = vec![err.to_string()];
    let mut source = err.source();
    while let Some(err) = source {
        let text = err.to_string();
        if !parts.iter().any(|p| p.contains(&text)) {
            parts.push(text);
        }
        source = err.source();
    }
    parts.join(": ")
}

fn render(page: &Page, offset: usize, max_chars: usize, max_bytes: usize) -> String {
    let text: Vec<char> = page.text.chars().collect();
    let start = offset.min(text.len());
    let end = (start + max_chars).min(text.len());
    let mut out = String::new();
    if !page.title.is_empty() {
        out.push_str(&format!("Title: {}\n", page.title));
    }
    out.push_str(&format!("URL: {}\n\n", page.url));
    out.extend(&text[start..end]);
    if text.is_empty() {
        out.push_str("(no text on this page; it may need JavaScript, try the browser)");
    }
    if start > 0 || end < text.len() {
        out.push_str(&format!(
            "\n\n[text characters {start}-{end} of {}{}]",
            text.len(),
            if end < text.len() {
                format!("; fetch again with offset={end} for more")
            } else {
                String::new()
            }
        ));
    }
    if page.cut && end == text.len() {
        out.push_str(&format!(
            "\n[the page is larger than {} KB; only its start was read]",
            max_bytes / 1000
        ));
    }
    out
}

/// Only web pages, and a scheme is optional: `example.com/a` is https.
fn web_url(input: &str) -> Result<Url> {
    let input = input.trim();
    if input.is_empty() {
        bail!("web_fetch needs a url");
    }
    let with_scheme = if input.contains("://") {
        input.to_owned()
    } else {
        format!("https://{input}")
    };
    let mut url = Url::parse(&with_scheme).with_context(|| format!("invalid url {input:?}"))?;
    match url.scheme() {
        "http" | "https" => {}
        other => bail!("only http and https pages can be fetched, not {other}:"),
    }
    url.set_fragment(None);
    Ok(url)
}

/// Refuses names that point at this host or the local network. The resolver
/// checks again when connecting, so a name cannot change its answer between
/// the two; this check also covers a configured proxy, which resolves names
/// itself.
async fn check_public(url: &Url) -> Result<()> {
    let refused = || {
        anyhow::anyhow!(
            "{url} is on this host or the local network, which web_fetch does not reach"
        )
    };
    let host = url
        .host_str()
        .with_context(|| format!("{url} has no host"))?;
    if let Ok(ip) = host
        .trim_start_matches('[')
        .trim_end_matches(']')
        .parse::<IpAddr>()
    {
        return if is_public(ip) {
            Ok(())
        } else {
            Err(refused())
        };
    }
    let name = host.trim_end_matches('.').to_ascii_lowercase();
    if name == "localhost" || name.ends_with(".localhost") {
        return Err(refused());
    }
    let port = url.port_or_known_default().unwrap_or(443);
    let addrs: Vec<SocketAddr> = tokio::net::lookup_host((name.as_str(), port))
        .await
        .with_context(|| format!("cannot find the address of {name}"))?
        .collect();
    if addrs.iter().any(|a| !is_public(a.ip())) {
        return Err(refused());
    }
    Ok(())
}

/// DNS that only hands out public addresses.
struct PublicOnly;

impl Resolve for PublicOnly {
    fn resolve(&self, name: Name) -> Resolving {
        let host = name.as_str().to_owned();
        Box::pin(async move {
            let addrs: Vec<SocketAddr> =
                tokio::net::lookup_host((host.as_str(), 0)).await?.collect();
            if addrs.is_empty() || addrs.iter().any(|a| !is_public(a.ip())) {
                return Err(format!("{host} is on this host or the local network").into());
            }
            Ok(Box::new(addrs.into_iter()) as Addrs)
        })
    }
}

/// An address on the internet, not this host, the LAN or a reserved range.
fn is_public(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(ip) => is_public_v4(ip),
        IpAddr::V6(ip) => {
            if let Some(v4) = ip.to_ipv4_mapped() {
                return is_public_v4(v4);
            }
            let s = ip.segments();
            // NAT64 carries an IPv4 address in its last 32 bits.
            if s[..6] == [0x64, 0xff9b, 0, 0, 0, 0] {
                let [a, b] = s[6].to_be_bytes();
                let [c, d] = s[7].to_be_bytes();
                return is_public_v4(Ipv4Addr::new(a, b, c, d));
            }
            !(ip.is_unspecified()
                || ip.is_loopback()
                || ip.is_multicast()
                || (s[0] & 0xfe00) == 0xfc00 // unique local
                || (s[0] & 0xffc0) == 0xfe80 // link local
                || (s[0] == 0x2001 && s[1] == 0x0db8)) // documentation
        }
    }
}

fn is_public_v4(ip: Ipv4Addr) -> bool {
    let [a, b, c, _] = ip.octets();
    !(ip.is_unspecified()
        || ip.is_loopback()
        || ip.is_private()
        || ip.is_link_local()
        || ip.is_broadcast()
        || ip.is_multicast()
        || ip.is_documentation()
        || a == 0
        || (a == 100 && (64..128).contains(&b)) // carrier-grade NAT
        || (a == 192 && b == 0 && c == 0)
        || (a == 198 && (18..20).contains(&b)) // benchmarking
        || a >= 240)
}

#[derive(Debug, PartialEq, Eq)]
enum Kind {
    Html,
    Text,
    Unknown,
    Other,
}

fn kind_of(content_type: &str) -> Kind {
    let mime = content_type.split(';').next().unwrap_or("").trim();
    match mime {
        "" => Kind::Unknown,
        "text/html" | "application/xhtml+xml" => Kind::Html,
        "application/json"
        | "application/xml"
        | "application/javascript"
        | "application/x-javascript"
        | "application/x-ndjson"
        | "application/rss+xml"
        | "application/atom+xml" => Kind::Text,
        m if m.starts_with("text/") || m.ends_with("+json") || m.ends_with("+xml") => Kind::Text,
        _ => Kind::Other,
    }
}

/// Bytes to text by the byte order mark, the Content-Type charset or a
/// `<meta>` charset, in that order; UTF-8 otherwise.
fn decode(body: &[u8], content_type: &str, html: bool) -> String {
    if let Some((encoding, bom)) = encoding_rs::Encoding::for_bom(body) {
        return encoding
            .decode_without_bom_handling(&body[bom..])
            .0
            .into_owned();
    }
    let label = charset_param(content_type).or_else(|| {
        html.then(|| meta_charset(&body[..body.len().min(4096)]))
            .flatten()
    });
    let encoding = label
        .and_then(|l| encoding_rs::Encoding::for_label(l.as_bytes()))
        .unwrap_or(encoding_rs::UTF_8);
    encoding.decode_without_bom_handling(body).0.into_owned()
}

fn charset_param(content_type: &str) -> Option<String> {
    content_type.split(';').skip(1).find_map(|p| {
        let (k, v) = p.split_once('=')?;
        (k.trim() == "charset").then(|| v.trim().trim_matches(['"', '\'']).to_owned())
    })
}

/// `<meta charset="gbk">` or `<meta http-equiv=... content="...; charset=gbk">`.
fn meta_charset(head: &[u8]) -> Option<String> {
    let head = String::from_utf8_lossy(head).to_ascii_lowercase();
    let mut rest = head.as_str();
    while let Some(at) = rest.find("<meta") {
        rest = &rest[at + 5..];
        let tag = &rest[..rest.find('>').unwrap_or(rest.len())];
        if let Some(at) = tag.find("charset=") {
            let value = tag[at + 8..].trim_start_matches(['"', '\'']);
            let end = value
                .find(|c: char| !(c.is_ascii_alphanumeric() || c == '-' || c == '_'))
                .unwrap_or(value.len());
            if end > 0 {
                return Some(value[..end].to_owned());
            }
        }
    }
    None
}

/// Elements whose content is never page text.
const SKIPPED: &[&str] = &[
    "script", "style", "noscript", "template", "svg", "math", "canvas", "iframe", "object",
    "select", "nav", "footer", "aside", "dialog",
];
/// Their content is raw text up to the closing tag.
const RAW_TEXT: &[&str] = &["script", "style", "noscript", "iframe", "textarea", "title"];
const VOID: &[&str] = &[
    "area", "base", "br", "col", "embed", "hr", "img", "input", "link", "meta", "source", "track",
    "wbr",
];

/// The title and readable text of an HTML page, with links as Markdown.
/// When the page marks its content with `<main>` or `<article>`, only that
/// is kept, unless it holds too little of the text.
fn html_to_text(html: &str, base: &Url) -> (String, String) {
    let full = Converter::new(html, base, false).run();
    let lower = html.to_ascii_lowercase();
    let marks_content = ["<main", "<article"].iter().any(|tag| {
        lower.match_indices(tag).any(|(at, _)| {
            lower[at + tag.len()..]
                .chars()
                .next()
                .is_some_and(|c| c == '>' || c.is_ascii_whitespace())
        })
    });
    if marks_content {
        let focused = Converter::new(html, base, true).run();
        let (f, a) = (focused.1.chars().count(), full.1.chars().count());
        if f >= 300 || f * 3 >= a {
            return (full.0, focused.1);
        }
    }
    full
}

struct Converter<'a> {
    html: &'a str,
    lower: String,
    base: &'a Url,
    /// Keep only text inside `<main>` / `<article>`.
    focus: bool,
    focus_depth: usize,
    out: String,
    space: bool,
    pre: usize,
    title: String,
    /// Open link: its address and where its text starts in `out`.
    link: Option<(String, Option<usize>)>,
    /// Item counters of open lists, `None` for `<ul>`.
    lists: Vec<Option<usize>>,
    /// A cell started: `|` goes before its text if the row has some.
    cell: bool,
}

impl<'a> Converter<'a> {
    fn new(html: &'a str, base: &'a Url, focus: bool) -> Self {
        Self {
            html,
            lower: html.to_ascii_lowercase(),
            base,
            focus,
            focus_depth: 0,
            out: String::new(),
            space: false,
            pre: 0,
            title: String::new(),
            link: None,
            lists: Vec::new(),
            cell: false,
        }
    }

    fn run(mut self) -> (String, String) {
        let html = self.html;
        let mut at = 0;
        while at < html.len() {
            let Some(lt) = html[at..].find('<').map(|i| at + i) else {
                self.text(&html[at..]);
                break;
            };
            self.text(&html[at..lt]);
            at = self.tag(lt);
        }
        self.close_link();
        let mut text = String::new();
        let mut blank = 0;
        for line in self.out.lines() {
            let line = line.trim_end();
            if line.trim().is_empty() {
                blank += 1;
                continue;
            }
            if is_empty_item(line) {
                continue;
            }
            if !text.is_empty() {
                text.push_str(if blank > 0 { "\n\n" } else { "\n" });
            }
            blank = 0;
            text.push_str(line);
        }
        (collapse(&decode_entities(&self.title)), text)
    }

    /// Handles the markup at `lt` (a `<`) and returns where text resumes.
    fn tag(&mut self, lt: usize) -> usize {
        let html = self.html;
        let rest = &html[lt..];
        if let Some(comment) = rest.strip_prefix("<!--") {
            return comment.find("-->").map_or(html.len(), |i| lt + 4 + i + 3);
        }
        if rest.starts_with("<!") || rest.starts_with("<?") {
            return rest.find('>').map_or(html.len(), |i| lt + i + 1);
        }
        let closing = rest.starts_with("</");
        let name_start = lt + if closing { 2 } else { 1 };
        let name_len = html[name_start..]
            .find(|c: char| !(c.is_ascii_alphanumeric() || c == '-' || c == ':'))
            .unwrap_or(html.len() - name_start);
        if name_len == 0 || !html.as_bytes()[name_start].is_ascii_alphabetic() {
            self.text("<");
            return lt + 1;
        }
        let name = self.lower[name_start..name_start + name_len].to_owned();
        let (end, attrs, self_closing) = parse_attrs(html, name_start + name_len);
        if closing {
            self.close(&name);
            return end;
        }
        let hidden = attrs.iter().any(|(k, v)| {
            k == "hidden"
                || (k == "aria-hidden" && v == "true")
                // Links to the page in other languages, a long list on Wikipedia.
                || (k == "hreflang" && name == "a") || (k == "role" && v == "navigation")
        });
        let void = VOID.contains(&name.as_str());
        if self_closing && !void {
            return end;
        }
        if RAW_TEXT.contains(&name.as_str()) {
            let close = format!("</{name}");
            let stop = self.lower[end..]
                .find(&close)
                .map_or(html.len(), |i| end + i);
            if name == "title" && self.title.is_empty() {
                self.title = html[end..stop].to_owned();
            } else if name == "textarea" && !hidden {
                self.text(&html[end..stop]);
            }
            return html[stop..].find('>').map_or(html.len(), |i| stop + i + 1);
        }
        if (SKIPPED.contains(&name.as_str()) || hidden) && !void {
            return self.skip_element(&name, end);
        }
        self.open(&name, &attrs);
        end
    }

    /// Jumps past the end of the element `name` whose start tag ends at `from`.
    fn skip_element(&self, name: &str, from: usize) -> usize {
        let (open, close) = (format!("<{name}"), format!("</{name}"));
        let mut depth = 1;
        let mut at = from;
        loop {
            // Never closed (an omitted end tag): only the start tag is dropped.
            let Some(close_at) = self.lower[at..].find(&close).map(|i| at + i) else {
                return from;
            };
            // Nested opens are only looked for up to the close, so each part
            // of the page is scanned once.
            let next_open = self.lower[at..close_at]
                .match_indices(&open)
                .map(|(i, _)| at + i)
                .find(|&i| {
                    self.lower[i + open.len()..]
                        .chars()
                        .next()
                        .is_some_and(|c| c == '>' || c == '/' || c.is_ascii_whitespace())
                });
            match next_open {
                Some(open_at) => {
                    depth += 1;
                    at = open_at + open.len();
                }
                None => {
                    depth -= 1;
                    at = self.html[close_at..]
                        .find('>')
                        .map_or(self.html.len(), |i| close_at + i + 1);
                    if depth == 0 {
                        return at;
                    }
                }
            }
        }
    }

    fn visible(&self) -> bool {
        !self.focus || self.focus_depth > 0
    }

    fn open(&mut self, name: &str, attrs: &[(String, String)]) {
        if matches!(name, "main" | "article") {
            self.focus_depth += 1;
        }
        if !self.visible() {
            return;
        }
        match name {
            "br" => self.newline(),
            "hr" => {
                self.block(2);
                self.out.push_str("---");
                self.block(2);
            }
            "h1" | "h2" | "h3" | "h4" | "h5" | "h6" => {
                self.close_link();
                self.block(2);
                let level = (name.as_bytes()[1] - b'0') as usize;
                self.out.push_str(&"#".repeat(level));
                self.out.push(' ');
            }
            "ul" | "ol" => {
                self.block(if self.lists.is_empty() { 2 } else { 1 });
                self.lists.push((name == "ol").then_some(0));
            }
            "li" => {
                self.block(1);
                let indent = "  ".repeat(self.lists.len().saturating_sub(1));
                self.out.push_str(&indent);
                match self.lists.last_mut() {
                    Some(Some(n)) => {
                        *n += 1;
                        self.out.push_str(&format!("{n}. "));
                    }
                    _ => self.out.push_str("- "),
                }
            }
            "tr" => {
                self.block(1);
                self.cell = false;
            }
            "td" | "th" => self.cell = true,
            "pre" => {
                self.block(2);
                self.pre += 1;
            }
            "a" => {
                self.close_link();
                let href = attrs
                    .iter()
                    .find(|(k, _)| k == "href")
                    .map(|(_, v)| decode_entities(v));
                self.link = href
                    .and_then(|h| link_target(self.base, &h))
                    .map(|url| (url, None));
            }
            "img" => {
                // An image link's only text is often the image's description.
                if self.link.is_some()
                    && let Some((_, alt)) = attrs.iter().find(|(k, _)| k == "alt")
                {
                    let alt = decode_entities(alt);
                    self.text(&alt);
                }
            }
            _ if is_block(name) => self.block(block_gap(name)),
            _ => {}
        }
    }

    fn close(&mut self, name: &str) {
        let was_visible = self.visible();
        if matches!(name, "main" | "article") {
            self.focus_depth = self.focus_depth.saturating_sub(1);
        }
        if !was_visible {
            return;
        }
        match name {
            "a" => self.close_link(),
            "ul" | "ol" => {
                self.lists.pop();
                self.block(if self.lists.is_empty() { 2 } else { 1 });
            }
            "pre" => {
                self.pre = self.pre.saturating_sub(1);
                self.block(2);
            }
            "h1" | "h2" | "h3" | "h4" | "h5" | "h6" => {
                self.close_link();
                self.block(2);
            }
            _ if is_block(name) => self.block(block_gap(name)),
            _ => {}
        }
    }

    fn text(&mut self, raw: &str) {
        if raw.is_empty() || !self.visible() {
            return;
        }
        let text = decode_entities(raw);
        if self.pre > 0 {
            self.start_link_text();
            self.out.push_str(&text);
            return;
        }
        for c in text.chars() {
            if c.is_whitespace() {
                self.space = true;
                continue;
            }
            if self.cell {
                self.cell = false;
                let line = &self.out[self.out.rfind('\n').map_or(0, |i| i + 1)..];
                if !line.trim().is_empty() {
                    self.out.truncate(self.out.trim_end_matches(' ').len());
                    self.out.push_str(" | ");
                }
                self.space = false;
            }
            if self.space && !self.out.is_empty() && !self.out.ends_with(['\n', ' ']) {
                self.out.push(' ');
            }
            self.space = false;
            self.start_link_text();
            self.out.push(c);
        }
    }

    fn start_link_text(&mut self) {
        if let Some((_, start @ None)) = &mut self.link {
            *start = Some(self.out.len());
        }
    }

    /// `text` becomes `[text](url)`, unless it is empty, spans lines or is
    /// the address itself.
    fn close_link(&mut self) {
        let Some((url, Some(start))) = self.link.take() else {
            return;
        };
        let text = self.out[start..].trim_end();
        if text.is_empty() || text.contains('\n') || text == url {
            return;
        }
        let end = start + text.len();
        self.out.truncate(end);
        self.out.insert(start, '[');
        self.out.push_str(&format!("]({url})"));
    }

    fn newline(&mut self) {
        while self.out.ends_with(' ') {
            self.out.pop();
        }
        self.out.push('\n');
        self.space = false;
    }

    /// Ends the line, with `gap - 1` blank lines after it.
    fn block(&mut self, gap: usize) {
        self.space = false;
        if self.out.is_empty() {
            return;
        }
        while self.out.ends_with(' ') {
            self.out.pop();
        }
        let have = self.out.len() - self.out.trim_end_matches('\n').len();
        for _ in have..gap {
            self.out.push('\n');
        }
    }
}

/// A list item or table row left with no text, like `- |` or `3. |`.
fn is_empty_item(line: &str) -> bool {
    let line = line.trim_start();
    let rest = match line.strip_prefix("- ") {
        Some(rest) => rest,
        None => line
            .trim_start_matches(|c: char| c.is_ascii_digit())
            .strip_prefix(". ")
            .unwrap_or(line),
    };
    rest.chars()
        .all(|c| c.is_whitespace() || "-|•·".contains(c))
}

fn is_block(name: &str) -> bool {
    matches!(
        name,
        "p" | "div"
            | "section"
            | "article"
            | "main"
            | "header"
            | "table"
            | "form"
            | "blockquote"
            | "figure"
            | "figcaption"
            | "dl"
            | "dt"
            | "dd"
            | "address"
            | "details"
            | "summary"
            | "fieldset"
            | "caption"
            | "body"
            | "center"
    )
}

fn block_gap(name: &str) -> usize {
    match name {
        "p" | "table" | "blockquote" | "figure" | "dl" | "section" | "article" | "header" => 2,
        _ => 1,
    }
}

/// An absolute http(s) address for a link, or `None` for script, mail and
/// same-page links and absurdly long addresses.
fn link_target(base: &Url, href: &str) -> Option<String> {
    let href = href.trim();
    if href.is_empty() || href.starts_with('#') {
        return None;
    }
    let url = base.join(href).ok()?;
    if !matches!(url.scheme(), "http" | "https") {
        return None;
    }
    let url = String::from(url);
    (url.len() <= 200).then_some(url)
}

/// Attributes of a start tag whose name ends at `from`: the position after
/// the tag, `(name, value)` pairs with lower-case names, and whether it ends
/// in `/>`.
fn parse_attrs(html: &str, from: usize) -> (usize, Vec<(String, String)>, bool) {
    let bytes = html.as_bytes();
    let mut attrs = Vec::new();
    let mut at = from;
    let mut self_closing = false;
    loop {
        while at < bytes.len() && bytes[at].is_ascii_whitespace() {
            at += 1;
        }
        if at >= bytes.len() {
            return (html.len(), attrs, self_closing);
        }
        match bytes[at] {
            b'>' => return (at + 1, attrs, self_closing),
            b'/' => {
                self_closing = true;
                at += 1;
                continue;
            }
            _ => {}
        }
        self_closing = false;
        let name_start = at;
        while at < bytes.len() && !bytes[at].is_ascii_whitespace() && !b"=>/".contains(&bytes[at]) {
            at += 1;
        }
        let name = html[name_start..at].to_ascii_lowercase();
        while at < bytes.len() && bytes[at].is_ascii_whitespace() {
            at += 1;
        }
        let mut value = String::new();
        if at < bytes.len() && bytes[at] == b'=' {
            at += 1;
            while at < bytes.len() && bytes[at].is_ascii_whitespace() {
                at += 1;
            }
            if at < bytes.len() && (bytes[at] == b'"' || bytes[at] == b'\'') {
                let quote = bytes[at] as char;
                let end = html[at + 1..]
                    .find(quote)
                    .map_or(html.len(), |i| at + 1 + i);
                value = html[at + 1..end].to_owned();
                at = (end + 1).min(html.len());
            } else {
                let start = at;
                while at < bytes.len() && !bytes[at].is_ascii_whitespace() && bytes[at] != b'>' {
                    at += 1;
                }
                value = html[start..at].to_owned();
            }
        }
        if !name.is_empty() {
            attrs.push((name, value));
        }
        if at == name_start {
            at += 1;
        }
    }
}

fn collapse(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// `&amp;`, `&#39;`, `&#x4e2d;` and the common named entities.
fn decode_entities(text: &str) -> String {
    if !text.contains('&') {
        return text.to_owned();
    }
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(at) = rest.find('&') {
        out.push_str(&rest[..at]);
        rest = &rest[at..];
        let entity = rest[1..]
            .find(';')
            .filter(|&end| end > 0 && end <= 10)
            .map(|end| (&rest[1..end + 1], end + 2));
        match entity.and_then(|(name, len)| entity_char(name).map(|c| (c, len))) {
            Some((c, len)) => {
                out.push(c);
                rest = &rest[len..];
            }
            None => {
                out.push('&');
                rest = &rest[1..];
            }
        }
    }
    out.push_str(rest);
    out
}

fn entity_char(name: &str) -> Option<char> {
    if let Some(number) = name.strip_prefix('#') {
        let code = match number.strip_prefix(['x', 'X']) {
            Some(hex) => u32::from_str_radix(hex, 16).ok()?,
            None => number.parse().ok()?,
        };
        return char::from_u32(code).filter(|c| *c != '\0');
    }
    Some(match name {
        "amp" => '&',
        "lt" => '<',
        "gt" => '>',
        "quot" => '"',
        "apos" => '\'',
        "nbsp" => ' ',
        "copy" => '©',
        "reg" => '®',
        "trade" => '™',
        "mdash" => '—',
        "ndash" => '–',
        "hellip" => '…',
        "laquo" => '«',
        "raquo" => '»',
        "lsquo" => '‘',
        "rsquo" => '’',
        "ldquo" => '“',
        "rdquo" => '”',
        "middot" => '·',
        "bull" => '•',
        "times" => '×',
        "deg" => '°',
        "euro" => '€',
        "yen" => '¥',
        "pound" => '£',
        "larr" => '←',
        "rarr" => '→',
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn convert(html: &str) -> (String, String) {
        html_to_text(html, &Url::parse("https://example.com/dir/page").unwrap())
    }

    #[test]
    fn html_becomes_readable_text_with_links() {
        let (title, text) = convert(
            r##"<!DOCTYPE html><html><head><title> Hello &amp; welcome </title>
            <style>p { color: red }</style><script>var x = "<p>no</p>";</script></head>
            <body><nav><a href="/">Home</a> <a href="/about">About</a></nav>
            <h1>Big   news</h1><p>First <b>bold</b> line.<br>Second line</p>
            <!-- hidden <p>comment</p> -->
            <ul><li>one</li><li>two <a href="x?a=1&amp;b=2">link</a></li></ul>
            <ol><li>first</li><li>second</li></ol>
            <table><tr><th>A</th><th>B</th></tr><tr><td>1</td><td>2</td></tr></table>
            <pre>  keep
   spacing</pre>
            <div hidden>secret</div><p>caf&eacute; &#20013;&#x6587; &copy;</p>
            <a href="javascript:void(0)">js</a> <a href="#top">top</a>
            <footer>Copyright</footer></body></html>"##,
        );
        assert_eq!(title, "Hello & welcome");
        assert_eq!(
            text,
            "# Big news\n\nFirst bold line.\nSecond line\n\n- one\n- two [link](https://example.com/dir/x?a=1&b=2)\n\n\
             1. first\n2. second\n\nA | B\n1 | 2\n\n  keep\n   spacing\n\ncaf&eacute; 中文 ©\n\njs top"
        );
    }

    #[test]
    fn main_content_is_preferred_when_it_holds_the_text() {
        let body = "word ".repeat(100);
        let (_, text) = convert(&format!(
            "<div>Sidebar menu</div><main><p>{body}</p></main><div>More junk</div>"
        ));
        assert!(!text.contains("Sidebar") && !text.contains("junk"));
        assert!(text.starts_with("word word"));
        // A small <article> (a comment widget) does not hide the page.
        let (_, text) = convert(&format!("<p>{body}</p><article>tiny</article><p>tail</p>"));
        assert!(text.contains("tail") && text.contains("tiny"));
    }

    #[test]
    fn nested_skipped_elements_and_odd_markup() {
        let (_, text) = convert(
            "<p>a < b</p><aside><aside>x</aside>still aside</aside><p>after</p>\
             <svg/><p>svg ok</p><select><option>opt</option></select><p>end</p>\
             <a href='/img'><img src=x.png alt=\"Logo\"></a><input value=x>",
        );
        assert_eq!(
            text,
            "a < b\n\nafter\n\nsvg ok\n\nend\n\n[Logo](https://example.com/img)"
        );
    }

    #[test]
    fn layout_leftovers_are_dropped() {
        let (_, text) = convert(
            "<table><tr><td><img src=a.gif></td><td>Story</td><td></td><td>5 points</td></tr>\
             <tr><td>1.</td><td></td></tr></table>\
             <ul><li><a href=/de hreflang=de lang=de>Deutsch</a></li><li>|</li><li>kept</li></ul>",
        );
        assert_eq!(text, "Story | 5 points\n1.\n\n- kept");
    }

    #[test]
    fn large_pages_convert_quickly() {
        let page = "<svg><path/></svg><p>text</p><div hidden>x</div>".repeat(40_000);
        let started = Instant::now();
        let (_, text) = convert(&format!("{page}<p hidden>never closed<p>shown"));
        assert!(text.ends_with("text\n\nnever closed\n\nshown"));
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "{:?}",
            started.elapsed()
        );
    }

    #[test]
    fn charsets_are_honoured() {
        let (gbk, _, _) = encoding_rs::GBK.encode("中文页面");
        assert_eq!(decode(&gbk, "text/html; charset=GBK", true), "中文页面");
        let mut page = b"<html><head><meta charset=\"gb2312\"></head>".to_vec();
        page.extend_from_slice(&gbk);
        assert!(decode(&page, "text/html", true).ends_with("中文页面"));
        let page =
            b"<meta http-equiv=\"Content-Type\" content=\"text/html; charset=windows-1252\">\xe9";
        assert!(decode(page, "text/html", true).ends_with('é'));
        assert_eq!(decode("ok".as_bytes(), "", false), "ok");
        assert_eq!(
            decode(b"\xef\xbb\xbfbom", "text/plain; charset=latin1", false),
            "bom"
        );
    }

    #[test]
    fn only_text_types_are_read() {
        assert_eq!(kind_of("text/html; charset=utf-8"), Kind::Html);
        assert_eq!(kind_of("application/xhtml+xml"), Kind::Html);
        assert_eq!(kind_of("application/json"), Kind::Text);
        assert_eq!(kind_of("application/ld+json"), Kind::Text);
        assert_eq!(kind_of("text/markdown"), Kind::Text);
        assert_eq!(kind_of(""), Kind::Unknown);
        assert_eq!(kind_of("application/pdf"), Kind::Other);
        assert_eq!(kind_of("image/png"), Kind::Other);
    }

    #[test]
    fn local_and_private_addresses_are_not_public() {
        for ip in [
            "127.0.0.1",
            "10.1.2.3",
            "172.16.0.1",
            "192.168.1.1",
            "169.254.169.254",
            "0.0.0.0",
            "100.64.0.1",
            "255.255.255.255",
            "224.0.0.1",
            "::1",
            "::",
            "fe80::1",
            "fd00::1",
            "::ffff:192.168.1.1",
            "64:ff9b::a00:1",
        ] {
            assert!(!is_public(ip.parse().unwrap()), "{ip} should not be public");
        }
        for ip in [
            "1.1.1.1",
            "93.184.215.14",
            "2606:4700::1111",
            "64:ff9b::101:101",
        ] {
            assert!(is_public(ip.parse().unwrap()), "{ip} should be public");
        }
    }

    #[tokio::test]
    async fn the_local_network_is_refused() {
        let fetcher = Fetcher::new(&FetchConfig::default()).unwrap().unwrap();
        for url in [
            "http://127.0.0.1:9/",
            "http://localhost:9/",
            "http://[::1]:9/",
            "http://192.168.1.1/",
            "http://app.localhost/",
            "file:///etc/passwd",
        ] {
            let err = fetcher.fetch(url, 0, true).await.unwrap_err().to_string();
            assert!(
                err.contains("local network") || err.contains("only http"),
                "{url}: {err}"
            );
        }
    }

    #[tokio::test]
    async fn owners_may_reach_the_local_network_when_allowed() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let hits = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let counter = hits.clone();
        tokio::spawn(async move {
            loop {
                let (mut socket, _) = listener.accept().await.unwrap();
                counter.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                let mut request = vec![0; 2048];
                let n = socket.read(&mut request).await.unwrap();
                let request = String::from_utf8_lossy(&request[..n]).to_string();
                let response = if request.starts_with("GET /old") {
                    "HTTP/1.1 302 Found\r\nLocation: /new\r\nContent-Length: 0\r\n\r\n".to_owned()
                } else {
                    let body = format!("<title>Router</title><p>{}</p>", "x".repeat(900));
                    format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nContent-Length: {}\r\n\r\n{body}",
                        body.len()
                    )
                };
                socket.write_all(response.as_bytes()).await.unwrap();
            }
        });
        let config = FetchConfig {
            private_network: true,
            max_chars: 500,
            ..FetchConfig::default()
        };
        let fetcher = Fetcher::new(&config).unwrap().unwrap();
        let url = format!("http://{addr}/old");
        // Guests stay off the local network even when owners may use it.
        assert!(fetcher.fetch(&url, 0, false).await.is_err());
        let page = fetcher.fetch(&url, 0, true).await.unwrap();
        assert!(page.starts_with(&format!("Title: Router\nURL: http://{addr}/new\n\nxxx")));
        assert!(
            page.ends_with("[text characters 0-500 of 900; fetch again with offset=500 for more]")
        );
        let hits_after_first = hits.load(std::sync::atomic::Ordering::SeqCst);
        assert_eq!(hits_after_first, 2, "one redirect, one page");
        let rest = fetcher.fetch(&url, 500, true).await.unwrap();
        assert!(rest.ends_with("[text characters 500-900 of 900]"));
        assert_eq!(
            hits.load(std::sync::atomic::Ordering::SeqCst),
            hits_after_first,
            "reading on comes from the cache"
        );
    }
}

/// Real pages: `OC_FETCH_URL=https://... cargo test fetch::live -- --ignored --nocapture`.
#[cfg(test)]
mod live {
    use super::*;

    #[tokio::test]
    #[ignore]
    async fn reads_a_real_page() {
        let url = std::env::var("OC_FETCH_URL").unwrap_or_else(|_| "https://example.com".into());
        let fetcher = Fetcher::new(&FetchConfig::default()).unwrap().unwrap();
        let page = fetcher.fetch(&url, 0, false).await.unwrap();
        println!("{page}");
        assert!(page.contains("URL: "));
    }
}
