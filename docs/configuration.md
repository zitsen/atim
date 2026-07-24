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
command = "claude"      # agent CLI command (claude, copilot, codex, mimo)

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
base_url = "https://api.openai.com/v1"
```

## Environment Variable Overrides

Any setting in `config.toml` can be overridden by the corresponding environment variable.

| Variable                 | Default   | Description                                           |
| ------------------------ | --------- | ----------------------------------------------------- |
| `ATIM_DIR`               | `~/.atim` | Data directory                                        |
| `ATIM_TMUX_SESSION`      | `atim`    | Target tmux session                                   |
| `ATIM_AGENT_COMMAND`     | `claude`  | Agent CLI command                                     |
| `ATIM_IM_BACKEND`        | —         | IM backend to use (`feishu` or `telegram`)            |
| `ATIM_FEISHU_APP_ID`     | —         | Feishu App ID                                         |
| `ATIM_FEISHU_APP_SECRET` | —         | Feishu App Secret                                     |
| `ATIM_TELEGRAM_TOKEN`    | —         | Telegram Bot API token                                |
| `ATIM_ALLOWED_USERS`     | —         | Comma-separated Telegram user IDs (empty = allow all) |
| `ATIM_OPENAI_API_KEY`    | —         | For voice message transcription                       |
| `ATIM_OPENAI_BASE_URL`   | `https://api.openai.com/v1` | OpenAI-compatible API endpoint  |

## Data Directory Structure

`~/.atim/` contains:

| File                 | Purpose                                    |
| -------------------- | ------------------------------------------ |
| `config.toml`        | Main configuration file                    |
| `store.db`           | SQLite database (sessions, bindings, etc.) |
| `monitor_state.json` | Session byte-offset tracking               |

## Agent Configuration

Atim auto-detects the agent type from the running process in tmux. Supported agents:

| Agent      | Command   | Session Support |
| ---------- | --------- | --------------- |
| Claude Code| `claude`  | ✅ Yes          |
| Copilot CLI| `copilot` | ✅ Yes          |
| Codex CLI  | `codex`   | ❌ No           |
| Mimo       | `mimo`    | ✅ Yes          |
