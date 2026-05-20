# Atim

**AI Agent through IM** — Talk to Claude Code (and soon other AI coding agents) through Telegram or Feishu.

```
Telegram / Feishu  ↔  tmux  ↔  Claude Code
```

Atim bridges an IM chat directly to a tmux window running an AI coding agent. Type in Telegram — it reaches the agent's terminal. The agent responds — you see it in Telegram.

## Quick Install

```bash
# Download and install to ~/.local/bin
curl -fsSL https://raw.githubusercontent.com/huolinhe/atim/main/install.sh | bash

# Or with wget
wget -qO- https://raw.githubusercontent.com/huolinhe/atim/main/install.sh | bash

# Install to a custom path
curl -fsSL https://raw.githubusercontent.com/huolinhe/atim/main/install.sh | bash -s -- -b /usr/local/bin
```

The installer downloads a statically-linked musl binary from the latest GitHub release.

## Features

- **Multi-IM**: Telegram (stable) + Feishu/Lark (beta)
- **Multi-agent**: Claude Code, Copilot CLI, Codex CLI — auto-detected by pane inspection
- **Topic isolation**: Each Telegram topic groups → a dedicated tmux window
- **Interactive UIs**: Inline keyboards for directory browsing, window picking, session selection
- **Voice messages**: Whisper transcription via OpenAI API
- **Efficient monitoring**: Byte-offset tracking of Claude Code JSONL session logs
- **Flood control**: Per-chat rate limiting with content-aware message merging

## Architecture

```
  ┌──────────────┐     ┌──────────────────────────┐     ┌──────────────┐
  │  Telegram     │     │     Atim Server          │     │   Feishu     │
  │  Bot API      │◀───▶│                          │◀───▶│   Bot API    │
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

```bash
# 1. Build
cargo build --release

# 2. Set up your IM backend
# Telegram: create a bot via @BotFather, get your user ID from @userinfobot
export ATIM_TELEGRAM_TOKEN="123456:ABC-DEF1234ghIkl-zyx57W2v1u123ew11"
export ATIM_ALLOWED_USERS="123456789"

# 3. Start a tmux session
tmux new-session -d -s atim

# 4. Run Atim
atim
```

### Prerequisites

| Requirement | Notes |
|-------------|-------|
| **tmux** | Atim manages agent windows through tmux |
| **Telegram Bot Token** | Create via [@BotFather](https://t.me/botfather) |
| **Rust 1.85+** | Edition 2024 |
| **Your Telegram User ID** | Get it from [@userinfobot](https://t.me/userinfobot) |

## Configuration

Atim reads from environment variables (or a `~/.atim/.env` file):

| Variable | Default | Description |
|----------|---------|-------------|
| `ATIM_TELEGRAM_TOKEN` | — | Telegram Bot API token |
| `ATIM_ALLOWED_USERS` | — | Comma-separated Telegram user IDs (empty = allow all) |
| `ATIM_DIR` | `~/.atim` | Data directory |
| `ATIM_TMUX_SESSION` | `atim` | Target tmux session |
| `ATIM_AGENT_COMMAND` | `claude` | Agent CLI command |
| `ATIM_FEISHU_APP_ID` | — | Feishu app ID (for Feishu backend) |
| `ATIM_FEISHU_APP_SECRET` | — | Feishu app secret |
| `ATIM_OPENAI_API_KEY` | — | For voice message transcription |

Legacy fallbacks: `TELEGRAM_BOT_TOKEN`, `ALLOWED_USERS`, `OPENAI_API_KEY`, etc.

## Usage

1. **Create a Telegram group** with your bot as admin. Enable **Topics** (Forum mode).
2. **Send a message** in a topic — Atim creates a tmux window and starts Claude Code.
3. **Chat with the agent** — every message goes to the agent's terminal.
4. **Close the topic** when done — the binding is cleaned up.

### Session Hook (recommended)

Install the SessionStart hook for reliable session tracking:

```bash
atim hook --install
```

This registers each Claude Code session UUID so Atim knows which JSONL logs to watch.

## Project Structure

| Crate | Purpose |
|-------|---------|
| `atim-core` | Config, error types, IM trait, message types, agent abstraction |
| `atim-im` | Telegram + Feishu adapter implementations |
| `atim-tmux` | tmux window lifecycle and terminal I/O |
| `atim-parser` | JSONL log and terminal output parsing |
| `atim-monitor` | Session log polling with byte-offset tracking |
| `atim-queue` | Per-user async message queues with flood control |
| `atim-state` | Thread binding and window state persistence |
| `atim-bin` | Entry point — server + CLI |

## Extending

Atim is designed for extensibility:

- **New IM backend**: implement the `ImAdapter` trait — see `crates/atim-im/src/` for examples
- **New agent**: implement the `AgentParser` trait — see `crates/atim-core/src/agent/` for examples

## License

MIT
