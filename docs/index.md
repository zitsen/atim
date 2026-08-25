# Atim

**AI Agent through IM** — Talk to Claude Code (and other AI coding agents) through Telegram or Feishu.

```
Telegram / Feishu  ↔  tmux  ↔  Claude Code / Copilot / Codex / Mimo
```

Atim bridges an IM chat directly to a tmux window running an AI coding agent. Type in Feishu or Telegram — it reaches the agent's terminal. The agent responds — you see it in the same chat.

## Features

- **Multi-IM**: Feishu/Lark + Telegram
- **Multi-agent**: Claude Code, Copilot CLI, Codex CLI, Mimo — auto-detected
- **Topic isolation**: Each Telegram topic / Feishu topic group maps to a dedicated tmux window
- **Session recovery**: Auto-recovers after restart, resume from where you left off
- **Voice messages**: Whisper transcription via OpenAI API
- **Lightweight**: Rust single binary + tmux, minimal resource usage

## Quick Install

```bash
curl -fsSL https://raw.githubusercontent.com/zitsen/atim/main/install.sh | bash
```

See [Installation](install.md) for Windows and manual build instructions.

## Next Steps

- [Quick Start](quickstart.md) — get running in 5 minutes
- [Feishu Setup](feishu.md) · [Telegram Setup](telegram.md)
- [Slash Commands](commands.md) — full command reference
- [Configuration](configuration.md) — all config options
- [Features](features.md) — advanced features in detail
- [Architecture (dev)](dev/architecture.md) — design and internals

## Project Structure

| Crate | Purpose |
| --- | --- |
| `atim-core` | Config, error types, IM trait, agent abstraction |
| `atim-im` | Telegram + Feishu adapter implementations |
| `atim-tmux` | tmux window lifecycle, screenshot rendering |
| `atim-parser` | JSONL log and terminal output parsing |
| `atim-monitor` | Session log polling with byte-offset tracking |
| `atim-queue` | Per-user async message queues with flood control |
| `atim-state` | Thread binding and window state persistence (SQLite) |
| `atim-bin` | Entry point — server + CLI |
