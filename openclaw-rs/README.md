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
| Gateway HTTP/WebSocket + Web UI | phase 4 |
| Scheduled tasks / heartbeat, OpenRC service | phase 5 |
| QQ channel (official QQ Bot API) | phase 6 |

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

Long-term memory is a SQLite FTS5 index with the trigram tokenizer, so Chinese
text matches by substring without word segmentation. Space-separated terms
match any; terms shorter than three characters fall back to `LIKE`. The model
gets `memory_save`, `memory_search` and `memory_delete` tools.

State lives in `~/.openclaw-rs` (override with `OPENCLAW_RS_HOME`):

- `config.toml`: optional settings
- `state.sqlite`: sessions, messages and memories

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
