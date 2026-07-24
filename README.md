# Atim

**AI Agent through IM** (Atim, pronounced like "Atom") — Talk to Claude Code (and soon other AI coding agents) through Telegram or Feishu.

```
Telegram / Feishu  ↔  tmux  ↔  Claude Code
```

Atim bridges an IM chat directly to a tmux window running an AI coding agent. Type in Feishu or Telegram — it reaches the agent's terminal. The agent responds — you see it in the same chat.

## Quick Install

Download and install to ~/.local/bin with `install.sh` with `curl`:

```bash
curl -fsSL https://raw.githubusercontent.com/zitsen/atim/main/install.sh | bash
```

Or with `wget`:

```bash
wget -qO- https://raw.githubusercontent.com/zitsen/atim/main/install.sh | bash
```

Install to a custom path:

```bash
curl -fsSL https://raw.githubusercontent.com/zitsen/atim/main/install.sh | bash -s -- -b /usr/local/bin
```

The installer downloads a statically-linked musl binary from the latest GitHub release.

## Features

- **Multi-IM**: Feishu/Lark (beta) + Telegram (stable)
- **Multi-agent**: Claude Code, Copilot CLI, Codex CLI — auto-detected by pane inspection
- **Topic isolation**: Each Telegram topic groups → a dedicated tmux window
- **Interactive UIs**: Inline keyboards for directory browsing, window picking, session selection
- **Voice messages**: Whisper transcription via OpenAI API
- **Efficient monitoring**: Byte-offset tracking of Claude Code JSONL session logs
- **Flood control**: Per-chat rate limiting with content-aware message merging

## Architecture

```
  ┌──────────────┐     ┌──────────────────────────┐     ┌──────────────┐
  │  Telegram    │     │     Atim Server          │     │   Feishu     │
  │  Bot API     │ <-> │                          │ <-> │   Bot API    │
  └──────────────┘     │  ┌──────┐  ┌───────────┐ │     └──────────────┘
                       │  │ Queue│  │  Monitor  │ │
                       │  └──┬───┘  └─────┬─────┘ │
                       │     │            │       │
                       │  ┌──┴────────────┴───┐   │
                       │  │    State + Tmux   │   │
                       │  └────────┬──────────┘   │
                       └───────────┼──────────────┘
                                   │
                          ┌────────┴────────┐
                          │  tmux session   │
                          │  ┌────────────┐ │
                          │  │ Claude Code│ │
                          │  │ / Copilot  │ │
                          │  │ / Codex    │ │
                          │  └────────────┘ │
                          └─────────────────┘
```

## Quick Start



<details>
<summary><strong>Configure Feishu Bot</strong></summary>
Open the [Feishu Launcher](https://open.feishu.cn/page/launcher) to create a bot in one click:

![Feishu Launcher](assets/feishu-launcher.png)

Enter any name (e.g. "AtimBot"), optionally upload an avatar, and click **Create**. After creation, you'll see your App ID and App Secret:

![App Credentials](assets/feishu-app-credentials.png)

For bot management, you can find your app in the [console](https://open.feishu.cn/app):

![App List](assets/feishu-apps-list.png)

Set `~/.atim/.env` with App ID and Secret:

```bash
ATIM_IM_BACKEND=feishu
ATIM_FEISHU_APP_ID=cli_xxxx
ATIM_FEISHU_APP_SECRET=xxxx
```

Then run `atim`.
</details>

<details>
<summary><strong>Configure Telegram Bot</strong></summary>
First create Telegram bot via [@BotFather](https://t.me/botfather).

Get your User ID from [@userinfobot](https://t.me/userinfobot).

Set `~/.atim/.env`:

```bash
ATIM_IM_BACKEND=telegram
ATIM_TELEGRAM_TOKEN="123456:ABC-DEF1234ghIkl-zyx57W2v1u123ew11"
ATIM_ALLOWED_USERS="123456789"
```

Then run `atim`.
</details>

### Prerequisites

| Requirement | Notes                                   |
| ----------- | --------------------------------------- |
| **tmux**    | Atim manages agent windows through tmux |
| **zoxide**  | Optional. For fast directory search     |

**Windows users**: Atim runs via WSL2. See the [Windows installation guide](https://zitsen.github.io/atim/windows/).

## Configuration

Atim reads configuration from `~/.atim/config.toml` (with environment variables as overrides).
On first run, a legacy `~/.atim/.env` file is automatically migrated to `config.toml`.

### config.toml

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
command = "claude"

[tmux]
session = "atim"

[monitor]
poll_interval = "2.0"

[display]
show_user_messages = "true"
show_tool_calls = "true"
show_hidden_dirs = false

[openai]
api_key = "..."
base_url = "https://api.openai.com/v1"
```

### Environment variable overrides

Any setting in `config.toml` can be overridden by the corresponding environment variable.
See the table below for the variable names:

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

## Usage

1. **Add the bot to a Feishu group chat** (or create a Telegram group with Topics enabled).
2. **Send a message** in the chat — Atim creates a tmux window and starts the agent.
3. **Chat with the agent** — every message goes to the agent's terminal.
4. **Close/archive the group** when done — the binding is cleaned up.

### Slash Commands

Send these in the IM chat to control the agent session:

| Command                | Description                                                      |
| ---------------------- | ---------------------------------------------------------------- |
| `/ss` or `/screenshot` | Capture a screenshot of the tmux terminal                        |
| `/usage`               | Show Claude Code usage/quota info                                |
| `/switch <agent>`      | Switch to a different agent (`claude`, `copilot`, `codex`)       |
| `/esc` or `/dismiss`   | Send Escape key to dismiss modals/help screens                   |
| `/enter`               | Send Enter key to confirm modals/selections                      |
| `/rebind`              | Re-detect the agent session and update bindings                  |
| `/rebind agent`        | Re-bind the agent type (e.g. `claude`, `copilot`, `codex`)      |
| `/unbind`              | Unbind session, send `/quit` to agent, and close the tmux window |
| `/clear`               | Clear Claude Code conversation and rebind new session UUID       |
| `/status`              | Show Claude Code session status info                             |
| `!<command>`           | Run a shell command in the agent's tmux window and stream output |

During directory browsing, text input acts as a `zoxide` query to jump to matching directories.

### Session Hook (recommended)

Install the SessionStart hook for reliable session tracking:

```bash
atim hook --install
```

This registers each Claude Code session UUID so Atim knows which JSONL logs to watch.

### Service Management

Atim can be managed as a systemd service:

```bash
# Install the service unit
atim service --install

# Start/stop/restart/status (user-level by default)
atim service --start
atim service --stop
atim service --restart
atim service --status

# System-level service (requires root)
atim service --system --install
atim service --system --start
```

## Project Structure

| Crate          | Purpose                                                         |
| -------------- | --------------------------------------------------------------- |
| `atim-core`    | Config, error types, IM trait, message types, agent abstraction |
| `atim-im`      | Telegram + Feishu adapter implementations                       |
| `atim-tmux`    | tmux window lifecycle and terminal I/O                          |
| `atim-parser`  | JSONL log and terminal output parsing                           |
| `atim-monitor` | Session log polling with byte-offset tracking                   |
| `atim-queue`   | Per-user async message queues with flood control                |
| `atim-state`   | Thread binding and window state persistence                     |
| `atim-bin`     | Entry point — server + CLI                                      |

## Extending

Atim is designed for extensibility:

- **New IM backend**: implement the `ImAdapter` trait — see `crates/atim-im/src/` for examples
- **New agent**: implement the `AgentParser` trait — see `crates/atim-core/src/agent/` for examples

## Acknowledgments

- **[ccbot](https://github.com/six-ddc/ccbot)** — Original Telegram-to-Claude-Code bridge that inspired this project
- **[openlark](https://github.com/openlark)** — Feishu/Lark IM integration SDK

## License

MIT
