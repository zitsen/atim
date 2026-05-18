# Aim Usage Guide

## What It Does

Aim bridges an IM chat (Telegram topic) directly to a tmux window running an AI coding agent. You type in Telegram — it reaches the agent's terminal. The agent responds — you see it in Telegram.

```
Telegram topic  ↔  tmux window  ↔  Claude Code / Copilot CLI / Codex CLI
```

## Quick Start

```bash
# 1. Build
cargo build --release

# 2. Set up Telegram bot token
export AIM_TELEGRAM_TOKEN="123456:ABC-DEF1234ghIkl-zyx57W2v1u123ew11"

# 3. Allow your Telegram user ID
export AIM_ALLOWED_USERS="123456789"

# 4. Make sure tmux is running
tmux new-session -d -s aim

# 5. Start Aim
cargo run --release
```

## Prerequisites

| Requirement | Notes |
|-------------|-------|
| **tmux** | Aim controls agents through tmux windows. Must have a running session. |
| **Telegram Bot Token** | Create one via [@BotFather](https://t.me/botfather). Enable **Inline Mode** and **Groups** if needed. |
| **Rust 1.85+** | Edition 2024. Check with `rustc --version`. |
| **Your Telegram User ID** | Message [@userinfobot](https://t.me/userinfobot) to get it. |

## Configuration

All config is through environment variables. Create a `.env` file or export directly:

### Required

| Variable | Default | Description |
|----------|---------|-------------|
| `AIM_TELEGRAM_TOKEN` | — | Telegram Bot API token |
| `AIM_ALLOWED_USERS` | — | Comma-separated Telegram user IDs (empty = allow all) |

### Optional

| Variable | Default | Description |
|----------|---------|-------------|
| `AIM_DIR` | `~/.aim` | Data directory for state files |
| `AIM_TMUX_SESSION` | `aim` | Target tmux session name |
| `AIM_AGENT_COMMAND` | `claude` | Command to start the AI agent in new windows |
| `AIM_MONITOR_POLL_INTERVAL` | `2.0` | Seconds between session log polls |
| `AIM_SHOW_USER_MESSAGES` | `true` | Echo user messages back in Telegram |
| `AIM_SHOW_TOOL_CALLS` | `true` | Show tool_use blocks in Telegram |
| `AIM_SHOW_HIDDEN_DIRS` | `false` | Show hidden files in directory listings |
| `AIM_OPENAI_API_KEY` | — | For voice message transcription |
| `AIM_OPENAI_BASE_URL` | `https://api.openai.com/v1` | OpenAI-compatible API endpoint |

Legacy fallbacks are supported: `TELEGRAM_BOT_TOKEN`, `ALLOWED_USERS`, `OPENAI_API_KEY`, etc.

## Usage Flow

### 1. Create a Telegram group with your bot

Add the bot as an admin, and **enable Topics** (Forum mode) in the group settings.

### 2. Open a topic — a tmux window is created

When you send a message in a topic where no binding exists yet, Aim:
1. Creates a new tmux window named `aim-<user_id>`
2. Runs `claude` (or your configured agent command) in that window
3. Sends your message to the agent
4. Records the binding: topic → window

Each topic maps to exactly one tmux window and one agent session.

### 3. Talk to the agent

Every message you send in that topic goes directly to the agent's terminal input.

The monitor polls the agent's session log (`~/.claude/sessions/*.jsonl`) and forwards new output back to your topic.

### 4. Close the topic — binding is cleaned up

Closing or deleting the topic removes the binding. The tmux window stays alive (kill it manually with `/kill` if needed).

## Session Hook (Optional)

For session tracking, install the hook so Claude Code registers each session:

```bash
# Run once — creates ~/.config/claude/hooks/SessionStart
aim hook --install
```

This writes a `session_map.json` when Claude Code starts a new session, mapping tmux window IDs to session UUIDs. The monitor uses this to know which JSONL files to watch.

## Voice Messages

If `AIM_OPENAI_API_KEY` is set, voice messages are transcribed via Whisper before being sent to the agent.

## Architecture

```
Telegram Bot API
  ↓  getUpdates (long poll)
TelegramAdapter::run()
  ↓  ImEvent { Text / CallbackQuery / etc }
Server event loop
  ↓  resolve thread binding → send_keys
tmux window (claude/copilot/codex)
  ↓  session.jsonl
Monitor (byte-offset polling)
  ↓  NewMessage
MessageQueue (merge + truncate)
  ↓  send_message / edit_message
Telegram topic
```

## Extending: Adding an IM Backend

See [im-interface.md](im-interface.md) for the `ImAdapter` trait and step-by-step guide for adding Feishu, Discord, Slack, etc.
