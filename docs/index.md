# Atim

**AI Agent through IM** (pronounced like "Atom")

Talk to Claude Code (and other AI coding agents) through Telegram or Feishu.

```
Telegram / Feishu  ↔  tmux  ↔  Claude Code
```

Atim bridges an IM chat directly to a tmux window running an AI coding agent. Type in Feishu or Telegram — it reaches the agent's terminal. The agent responds — you see it in the same chat.

## Features

- **Multi-IM**: Feishu/Lark + Telegram
- **Multi-agent**: Claude Code, Copilot CLI, Codex CLI, Mimo — auto-detected by pane inspection
- **Topic isolation**: Each Telegram topic / Feishu topic group maps to a dedicated tmux window
- **Interactive UIs**: Inline keyboards for directory browsing, window picking, session selection
- **Voice messages**: Whisper transcription via OpenAI API
- **Efficient monitoring**: Byte-offset tracking of Claude Code JSONL session logs
- **Flood control**: Per-chat rate limiting with content-aware message merging

## Quick Install

=== "curl"

    ```bash
    curl -fsSL https://raw.githubusercontent.com/zitsen/atim/main/install.sh | bash
    ```

=== "wget"

    ```bash
    wget -qO- https://raw.githubusercontent.com/zitsen/atim/main/install.sh | bash
    ```

=== "Custom path"

    ```bash
    curl -fsSL https://raw.githubusercontent.com/zitsen/atim/main/install.sh | bash -s -- -b /usr/local/bin
    ```

The installer downloads a statically-linked musl binary from the latest GitHub release.

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

## Project Structure

| Crate          | Purpose                                                         |
| -------------- | --------------------------------------------------------------- |
| `atim-core`    | Config, error types, IM trait, message types, agent abstraction |
| `atim-im`      | Telegram + Feishu adapter implementations                       |
| `atim-tmux`    | tmux window lifecycle and terminal I/O                          |
| `atim-parser`  | JSONL log and terminal output parsing                           |
| `atim-monitor` | Session log polling with byte-offset tracking                   |
| `atim-queue`   | Per-user async message queues with flood control                |
| `atim-state`   | Thread binding and window state persistence (SQLite)            |
| `atim-bin`     | Entry point — server + CLI                                      |
