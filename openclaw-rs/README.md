# openclaw-rs

Single-binary Rust rewrite of OpenClaw. No plugins: every feature is built in.

## Scope

| Area | Status |
|---|---|
| OpenRouter chat (streaming, tool calls) | phase 1 ✅ |
| Sessions + history in SQLite | phase 1 ✅ |
| CLI (`chat`, `ask`, `sessions`) | phase 1 ✅ |
| Tools: shell and files, with approval | phase 2 ✅ |
| Long-term memory search (SQLite FTS5) | phase 3 ✅ |
| Gateway HTTP/WebSocket + Web UI | phase 4 ✅ |
| Scheduled tasks / heartbeat, OpenRC service | phase 5 ✅ |
| QQ channel (official QQ Bot API) | phase 6 ✅ |
| Email channel (IMAP in, SMTP out) | ✅ |
| Identity setup on first start, web search | ✅ |

## Build for Raspberry Pi (Alpine, aarch64)

Alpine uses musl, so build a static binary. Cross-compile from any Linux host:

```sh
rustup target add aarch64-unknown-linux-musl
pip install ziglang && cargo install cargo-zigbuild
cargo zigbuild --release --target aarch64-unknown-linux-musl
scp target/aarch64-unknown-linux-musl/release/openclaw-rs pi:/usr/local/bin/
```

Or build on the Pi itself: `apk add cargo build-base && cargo build --release`.

## Usage

```sh
export OPENROUTER_API_KEY=sk-or-...
cargo build --release
./target/release/openclaw-rs ask "hello"
./target/release/openclaw-rs chat --session work
./target/release/openclaw-rs sessions
./target/release/openclaw-rs memory add "Prefers answers in Chinese"
./target/release/openclaw-rs memory search chinese
```

## Gateway and Web UI

```sh
export OPENCLAW_RS_TOKEN=$(head -c 24 /dev/urandom | base64)
./target/release/openclaw-rs serve            # http://127.0.0.1:18789/#token=<token>
```

The Web UI streams replies, lists sessions, and shows an approve/deny card
for every `ask` tool. Turns run one at a time per session and keep running if
the browser disconnects; an approval nobody answers within five minutes is
declined. Without a token the Gateway only binds to loopback.

```toml
[gateway]
bind = "127.0.0.1:18789"
# token = "..."   # prefer OPENCLAW_RS_TOKEN
```

## Scheduled jobs

`serve` runs jobs on standard 5-field cron schedules in the host's local time.
A job's prompt runs as a turn in its session; tools set to `ask` are declined
because nobody is there to approve. A job missed while the Gateway was down
runs once at startup, then follows its schedule. A heartbeat is a job:

```sh
openclaw-rs cron add heartbeat "*/30 * * * *" -s main "Check my notes and reminders"
openclaw-rs cron list
openclaw-rs cron remove heartbeat
```

The model can also create, list and remove jobs (`cron_add`, `cron_list`,
`cron_remove`); a job it creates runs in the conversation that asked for it.

## OpenRC service

```sh
export OPENROUTER_API_KEY=sk-or-... OPENCLAW_RS_TOKEN=...
doas openclaw-rs service install --user pi   # or sudo -E
rc-service openclaw-rs status
tail -f /var/log/openclaw-rs.log
doas openclaw-rs service uninstall           # keeps state and logs
```

The service runs as the given account with state in its `~/.openclaw-rs`,
under `supervise-daemon` with automatic restart. `OPENROUTER_API_KEY`, `OPENCLAW_RS_TOKEN`, `QQ_APP_SECRET` and `MAIL_PASSWORD`
from the installing shell are copied into `/etc/conf.d/openclaw-rs` (mode 0600).

## QQ (official bot)

1. Create a bot on the [QQ Open Platform](https://q.qq.com), note the AppID and
   AppSecret, and request the group and private-chat message permission.
2. Under 事件订阅与回调地址 (event subscription and callback URL), choose **WebSocket**. It needs no public address,
   which suits a Pi at home.
3. Configure and restart `serve`:

```toml
[qq]
enabled = true
app_id = "102xxxxxx"
# app_secret = "..."      # prefer QQ_APP_SECRET
allow = []                # user/group openids allowed to talk to the bot
```

Each private chat and each group is its own session (`qq:c2c:<openid>`,
`qq:group:<openid>`). The log prints every sender's openid, so you can fill in
`allow`. QQ users cannot approve tools, so `ask` tools are declined there; if
`tools.shell` or `tools.write` is `allow`, the Gateway refuses to start QQ
until `allow` is set. Replies go out as passive replies (up to 4 messages per
private message, 5 per group message, about 1500 characters each) and fall
back to active messages once QQ's reply window has passed. Scheduled jobs whose
session is a QQ session send their result to that chat.

## Email

The bot is a mail client of a third-party provider: it reads over IMAP (993)
and sends over SMTP submission (465 or 587). It needs no mail server of its
own and never uses port 25, which most hosts block. Each sender address is its
own session (`mail:<address>`).

For QQ Mail, 163/126/yeah.net, Gmail, iCloud and Aliyun the servers are filled
in from the address, so this is enough:

```toml
[mail]
enabled = true
username = "mybot@qq.com"
# password = "..."            # prefer MAIL_PASSWORD: the provider's app authorization code
allow = ["me@example.com", "@family.cn"]   # required: addresses or @domains
poll_secs = 60
```

Enable IMAP/SMTP in the mailbox settings and create an authorization code
(QQ Mail: 设置 → 账户 → POP3/IMAP/SMTP 服务; 163: 设置 → POP3/SMTP/IMAP; Gmail: an app
password). Then verify before starting the service:

```sh
MAIL_PASSWORD=... openclaw-rs mail check
# ✓ IMAP imap.qq.com:993: login ok, INBOX has 3 unread
# ✓ SMTP smtp.qq.com:465: login ok
```

Other providers (company mailboxes) set the servers explicitly; any field
you set overrides the preset:

```toml
imap_host = "imap.example.com"   # imap_port = 993
smtp_host = "smtp.example.com"
smtp_port = 587
smtp_security = "starttls"       # "tls" for 465
```

Outlook/Hotmail personal accounts only allow OAuth sign-in and are not supported.

- `allow` is required, because anyone could otherwise spend your model
  credits by email. Replies always go to `From`, never `Reply-To`.
- Auto-replies, mailing lists (`Auto-Submitted`, `Precedence`, `List-Id`) and
  mail from the bot's own address are skipped, and replies carry
  `Auto-Submitted: auto-replied`, so two robots cannot loop.
- Each poll marks fetched mail as read and handles at most 20 messages;
  Message-IDs are remembered, so a re-delivered copy is answered once.
- Quoted history (`>` lines, `On … wrote:`, `原始邮件`) is removed before the model
  sees the mail. GBK/GB18030 and other legacy charsets are decoded.
- The client sends the IMAP `ID` command, which 163/126 require.
- Security `"none"` is accepted only for loopback hosts, for local bridges such
  as Proton Bridge.
- Email has no way to approve tools, so `ask` tools are declined.
- Scheduled jobs whose session is a mail session send their result as an email.

## Identity

On first start the agent has no identity. Its first conversation, in any
channel, sets one up (unless you ask for real work first, which comes first).
You describe who it should be, or name a fictional character (novel, anime,
game, film) and it searches the web for that character's personality, way of
speaking, catchphrases and values, shows you a draft, and saves it after you
agree.

The identity follows OpenClaw's persona files: an `IDENTITY.md` record and a
`SOUL.md` voice. A character becomes the agent's own identity instead of a
reference to it: no titles of works, authors, actors, plot summaries, citations
or "based on" lines.

```text
> 变成孙悟空
[tool web_search {"query": "孙悟空 性格 口头禅"}]
[tool identity_set {"name": "悟空", "creature": "石猴", "vibe": "顽皮直率，天不怕地不怕", "emoji": "🐒", "soul": "你自称俺老孙，说话爽快，称用户为师父。..."}]
```

It is stored in `state.sqlite` and shared by every session, the Web UI, QQ
and email. The first one is saved without asking; later changes through the
`identity_set` tool follow `tools.identity` (`ask` by default, so QQ and email
users cannot change it). From the shell:

```sh
openclaw-rs identity                       # print as IDENTITY.md and SOUL.md
openclaw-rs identity set --name 悟空 --creature 石猴 --vibe "顽皮直率" --emoji 🐒 --soul-file SOUL.md
openclaw-rs identity reset                 # the next conversation sets it up again
```

## Web search

The model gets `web_search` for current information and character research.
By default it runs through OpenRouter's [web plugin](https://openrouter.ai/docs/guides/features/plugins/web-search)
with the same API key, which works wherever OpenRouter does and is billed per
search. A self-hosted [SearXNG](https://docs.searxng.org) instance (with the
`json` format enabled) is free:

```toml
[search]
provider = "openrouter"   # openrouter | searxng | off
# model = "..."           # model that runs OpenRouter searches (default: model.model)
# searxng_url = "http://127.0.0.1:8888"
max_results = 5
```

## Memory

Long-term memory is a SQLite FTS5 index with the trigram tokenizer, so Chinese
text matches by substring without word segmentation. Space-separated terms
match any; terms shorter than three characters fall back to `LIKE`. The model
gets `memory_save`, `memory_search` and `memory_delete` tools.

State lives in `~/.openclaw-rs` (override with `OPENCLAW_RS_HOME`):

- `config.toml`: optional settings
- `state.sqlite`: sessions, messages, memories and scheduled jobs

```toml
[model]
model = "openrouter/auto"            # any OpenRouter model id
base_url = "https://openrouter.ai/api/v1"
request_timeout_secs = 300

[agent]
system_prompt = "You are a helpful personal assistant running on OpenClaw."
max_steps = 25       # model calls per turn before giving up
history_limit = 200  # recent messages sent to the model

[tools]
# workspace = "/path"   # default: <state dir>/workspace
shell = "ask"           # allow | ask | deny
write = "ask"           # write_file and edit_file; reads are always allowed
identity = "ask"        # identity_set after the first identity exists
shell_timeout_secs = 120
max_output_bytes = 16384
```

Tools: `read_file`, `list_dir`, `write_file`, `edit_file` (one exact, unique
replacement) and `shell` (`sh -c` in the workspace; the whole process group is
killed on timeout). `ask` prompts on the controlling terminal; with no terminal
the action is declined and the model is told so. Tools set to `deny` are not
offered to the model at all.

## Development

```sh
cargo test
cargo clippy --all-targets
```
