# AGENTS.md — Project Conventions & Memory

## Commit Style

Use [Conventional Commits](https://www.conventionalcommits.org/):

| Type       | Usage                                  |
|------------|----------------------------------------|
| `feat:`    | New feature                           |
| `fix:`     | Bug fix                               |
| `docs:`    | Documentation only                     |
| `refactor:`| Code change without fix/feature        |
| `test:`    | Adding/updating tests                  |
| `chore:`   | Build, CI, deps, tooling              |
| `perf:`    | Performance improvement               |
| `style:`   | Formatting, missing semicolons, etc.   |

Format:
```
<type>: <short description>

<optional body with details>
```

Examples:
```
feat: add voice message transcription via OpenAI
fix: handle JSONL file truncation in monitor
docs: document Telegram proxy configuration
```

## Project Overview

**atim** — Rust IM-to-Claude-Code bridge. Telegram/Feishu messages routed to Claude Code sessions running in tmux windows.

### Key Architecture
- `atim-core`: Error types, IM trait, message types, config, agent abstraction
- `atim-im`: Telegram + Feishu adapters (Markdown→HTML rendering)
- `atim-monitor`: Polls Claude Code JSONL session logs for new responses
- `atim-parser`: JSONL and terminal output parsers
- `atim-queue`: Per-user async message queues with ordering
- `atim-state`: State persistence (thread bindings, window states)
- `atim-tmux`: tmux window lifecycle management
- `atim`: Main binary — `atim` runs the server, `atim hook` is the Claude Code session hook

### Response Pipeline
```
IM → atim → tmux send-keys → Claude Code → JSONL log → 
atim-monitor → atim → ImAdapter::send_message → IM
```

## Key Decisions

- **Telegram parse mode**: HTML (not MarkdownV2) — Telegram's HTML subset is more predictable
- **pulldown-cmark 0.12**: For Markdown→Telegram-HTML conversion, event-based rendering
- **JSONL format v2.1.143**: Nested `{type, message: {role, content: [...]}}` structure
- **Byte-offset tracking**: Per-session, persisted in `monitor_state.json`
- **State files** under `~/.atim/`: `state.json`, `session_map.json`, `monitor_state.json`
