# Configuration

Atim reads configuration from `~/.atim/config.toml`. On first run, a legacy `~/.atim/.env` file is automatically migrated to `config.toml`.

## config.toml

```toml
[im]
backend = "feishu"   # "feishu" or "telegram"

[im.feishu]
app_id = "cli_xxxxxxxxxxxxxx"
app_secret = "xxxxxxxxxxxxxxxxxxxxxxxxxx"

[im.telegram]
token = "123456:ABC-DEF1234ghIkl-zyx57W2v1u123ew11"
allowed_users = "123456789"   # comma-separated, empty = allow all

[agent]
command = "claude"             # agent CLI command (claude, copilot, codex, mimo)
default_agent = "claude"       # default agent for new sessions
workdir = "/path/to/project"   # default working directory for new sessions

[agent.claude]                 # per-agent overrides (optional)
command = "claude"
args = ["--dangerously-skip-permissions"]

[agent.copilot]
command = "copilot"
args = ["--allow-all-tools"]

[tmux]
session = "atim"        # tmux session name

[monitor]
poll_interval = "2.0"   # seconds between JSONL polls

[display]
show_user_messages = "true"
show_tool_calls = "true"
show_hidden_dirs = false

[openai]
api_key = "..."                        # for voice message transcription
base_url = "https://api.openai.com/v1" # any OpenAI-compatible endpoint
```

## Environment Variable Overrides

Any setting in `config.toml` can be overridden by the corresponding environment variable.

| Variable | Default | Description |
| --- | --- | --- |
| `ATIM_DIR` | `~/.atim` | Data directory |
| `ATIM_TMUX_SESSION` | `atim` | Target tmux session |
| `ATIM_AGENT_COMMAND` | `claude` | Agent CLI command |
| `ATIM_DEFAULT_AGENT` | `claude` | Default agent for new sessions |
| `ATIM_IM_BACKEND` | — | IM backend (`feishu` or `telegram`) |
| `ATIM_FEISHU_APP_ID` | — | Feishu App ID |
| `ATIM_FEISHU_APP_SECRET` | — | Feishu App Secret |
| `ATIM_FEISHU_WEBHOOK_PORT` | `9090` | Feishu webhook HTTP server port |
| `ATIM_TELEGRAM_TOKEN` | — | Telegram Bot API token |
| `ATIM_ALLOWED_USERS` | — | Comma-separated Telegram user IDs (empty = allow all) |
| `ATIM_OPENAI_API_KEY` | — | For voice message transcription |
| `ATIM_OPENAI_BASE_URL` | `https://api.openai.com/v1` | OpenAI-compatible API endpoint |
| `ATIM_MIMO_COMMAND` | `~/.mimocode/bin/mimo` | Path to Mimo binary |
| `ATIM_MONITOR_POLL_INTERVAL` | `2.0` | Seconds between JSONL polls |
| `ATIM_SHOW_USER_MESSAGES` | `true` | Show user messages in output |
| `ATIM_SHOW_TOOL_CALLS` | `true` | Show tool call details in output |
| `ATIM_SHOW_HIDDEN_DIRS` | `false` | Show hidden directories in browser |

## Per-Agent Configuration

Each agent can have its own command and arguments override in `config.toml`:

```toml
[agent.claude]
command = "claude"
args = ["--dangerously-skip-permissions", "--autocompact", "auto"]

[agent.copilot]
command = "copilot"
args = ["--allow-all-tools"]

[agent.codex]
command = "codex"

[agent.mimo]
command = "mimo"   # or set ATIM_MIMO_COMMAND
```

## Per-Chat Settings

These can be changed at runtime via `/atim config set`:

| Setting | Default | Description |
|---------|---------|-------------|
| `replyAtOnly` | `false` | In group chats, only respond to @-mentions (ignore other messages) |
| `defaultAgent` | from config | Override the default agent for this specific chat |

## Data Directory Structure

`~/.atim/` contains:

| File | Purpose |
| --- | --- |
| `config.toml` | Main configuration file |
| `store.db` | SQLite database (sessions, bindings, monitor state) |

## Agent Auto-Detection

Atim auto-detects the agent type from the running process in tmux. Supported agents:

| Agent | Command | Session Support | Output Source |
| --- | --- | --- | --- |
| Claude Code | `claude` | Yes (JSONL) | JSONL files |
| Copilot CLI | `copilot` | Yes (JSONL) | JSONL files |
| Codex CLI | `codex` | No | Pane capture |
| Mimo | `mimo` | Yes (SQLite) | Mimo's SQLite DB |
