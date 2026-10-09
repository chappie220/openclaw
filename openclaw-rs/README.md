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
| QQ channel (official QQ Bot API) | phase 6 |

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
under `supervise-daemon` with automatic restart. The two variables above are
copied into `/etc/conf.d/openclaw-rs` (mode 0600).

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
system_prompt = "You are OpenClaw, a helpful personal assistant."
max_steps = 25       # model calls per turn before giving up
history_limit = 200  # recent messages sent to the model

[tools]
# workspace = "/path"   # default: <state dir>/workspace
shell = "ask"           # allow | ask | deny
write = "ask"           # write_file and edit_file; reads are always allowed
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
