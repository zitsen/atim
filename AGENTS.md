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

## Build & Test

```bash
cargo build --release --package atim
systemctl --user restart atim
```

Check service health: `systemctl --user is-active atim`

## Release

1. Bump `version` in workspace `Cargo.toml`.
2. Commit with `chore: bump version to <x.y.z>`.
3. Tag: `git tag v<x.y.z>`.
4. Push: `git push origin main --tags`.
5. Create GitHub release: `gh release create v<x.y.z> --title "v<x.y.z>" --notes "<changelog>"`.

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

### Session Discovery (for rebind)
Priority order (non-disruptive, no commands sent to agent):
1. **PID via lsof** — trace open file handles to find JSONL UUID
2. **Pane text scan** — regex for UUID in captured pane output
3. **session_map.json** — last resort, cached mapping

### Response Routing
Monitor resolves `session_id → window_id → thread_binding` to determine where to send Claude Code output. Uses `.rfind()` (most recently created binding) to pick the correct Feishu group when multiple bindings exist for one window.

### Session Exclusivity
`/rebind` enforces exclusive binding: if a session is already bound to another window, it warns and steals (clears old binding's session_id).

## Key Decisions

- **Telegram parse mode**: HTML (not MarkdownV2) — Telegram's HTML subset is more predictable
- **pulldown-cmark 0.12**: For Markdown→Telegram-HTML conversion, event-based rendering
- **JSONL format v2.1.143**: Nested `{type, message: {role, content: [...]}}` structure
- **Byte-offset tracking**: Per-session, persisted in `monitor_state.json`
- **State files** under `~/.atim/`:
  - `state.json` — window_states + thread_bindings
  - `session_map.json` — window_id → session UUID mapping
  - `monitor_state.json` — session UUID → byte offset for incremental JSONL reading
- **Session filtering**: canonicalize path first, slug-match exact, fall back to capped scan (25 most recent) across all projects

## JSONL Format Reference

atim 同时解析两种 agent session JSONL 格式（`atim-parser` 包）：

| 格式 | 来源 | 文件路径模式 | 解析器 |
|------|------|------------|--------|
| Codex rollout | `openai/codex` (codex-rs) | `~/.codex/sessions/.../rollout-*.jsonl` | `codex_jsonl.rs` |
| Claude session | Anthropic Claude Code CLI | `~/.claude/projects/<hash>/<uuid>.jsonl` | `jsonl.rs` |

**关键差异**：Codex 的 `command` 是 `Vec<String>`（argv 数组），Claude 的是纯字符串；
Codex 输出字段是 `aggregated_output`/`stdout`（裸字符串），Claude 是 `tool_result.content[]`（ContentBlock 数组）。

详细字段对照与优化计划见：
- [`references/codex-jsonl-schema.md`](references/codex-jsonl-schema.md) — Codex rollout 格式权威参考（基于 `openai/codex` 源码）
- [`references/claude-jsonl-schema.md`](references/claude-jsonl-schema.md) — Claude session 格式参考（基于 atim 逆向分析）
- [`references/optimization-plan.md`](references/optimization-plan.md) — 解析器后续优化计划（P1/P2/P3 优先级）
