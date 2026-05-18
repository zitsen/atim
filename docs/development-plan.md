# Aim Development Plan

> **Aim** = AI Agent through IM

## Overview

Rust rewrite of CCBot with multi-agent and multi-IM support. Total estimated effort: **4-6 weeks**.

## Phase 1: Core + Telegram (Weeks 1-2)

**Goal**: Feature parity with original CCBot (Telegram only, Claude Code only).

### Step 1.1 — Workspace & Core Types (Day 1)

**Files**: `crates/aim-core/src/{lib,im,agent,message,session,error}.rs`

- [ ] Cargo workspace with all crates
- [ ] Core type definitions (`ImAdapter`, `AgentParser` traits)
- [ ] Message types (`ImEvent`, `MessageTarget`, `InteractiveUi`)
- [ ] Error type hierarchy
- [ ] Configuration loading (env vars + `.env`)

### Step 1.2 — Tmux Manager (Days 2-3)

**Files**: `crates/aim-tmux/src/lib.rs`

- [ ] `TmuxManager` — async wrapper around tmux CLI commands
- [ ] List windows, find by ID/name
- [ ] Capture pane (plain + ANSI via `-e` flag)
- [ ] Send keys (literal + special keys, with Enter delay)
- [ ] Create window, kill window, rename window
- [ ] Scrub sensitive env vars from tmux session

### Step 1.3 — JSONL & Terminal Parser (Days 3-5)

**Files**: `crates/aim-parser/src/{lib,jsonl,terminal}.rs`

- [ ] `JsonlParser` — read Claude Code session JSONL files
- [ ] Byte-offset incremental reading
- [ ] `tool_use`/`tool_result` pairing
- [ ] Edit diff generation (`old_string`/`new_string`)
- [ ] Image extraction from base64 tool results
- [ ] `TerminalParser` — interactive UI detection
- [ ] Status line parsing (spinner detection)
- [ ] Pane chrome stripping (bottom status bar)
- [ ] Bash output extraction

### Step 1.4 — Session State & Persistence (Days 5-6)

**Files**: `crates/aim-state/src/lib.rs`

- [ ] `WindowState`, `ClaudeSession` types
- [ ] Thread binding: topic → window_id mapping
- [ ] User window offsets for unread tracking
- [ ] Atomic JSON write (temp file + rename)
- [ ] Stale window ID re-resolution on startup
- [ ] Session map loading (hook-generated)
- [ ] Display name management

### Step 1.5 — IM Adapter Trait + Telegram (Days 6-9)

**Files**: `crates/aim-im/src/{lib,telegram,feishu}.rs`

- [ ] `ImAdapter` trait definition (in aim-core)
- [ ] Telegram adapter using `teloxide`
- [ ] Inline keyboard support for interactive UI
- [ ] Photo download and forwarding
- [ ] Voice transcription integration (OpenAI API)
- [ ] Topic lifecycle (create/close/rename)
- [ ] Rate limiting + flood control

### Step 1.6 — Session Monitor (Days 9-10)

**Files**: `crates/aim-monitor/src/lib.rs`

- [ ] Async polling loop (configurable interval)
- [ ] mtime cache to skip unchanged files
- [ ] Byte-offset tracking across restarts
- [ ] Session map change detection
- [ ] New message callback → queue
- [ ] Status line polling (1s interval)

### Step 1.7 — Message Queue (Days 10-11)

**Files**: `crates/aim-queue/src/lib.rs`

- [ ] Per-user message queue + worker
- [ ] Content message merging (3800 char limit)
- [ ] Status → content conversion (edit in place)
- [ ] tool_use → tool_result editing (track msg_id)
- [ ] Flood control pause/resume
- [ ] Graceful shutdown (drain queue)

### Step 1.8 — Hook Binary (Day 11)

**Files**: `crates/aim-hook/src/main.rs`

- [ ] Read JSON from stdin (Claude Code SessionStart)
- [ ] Validate session_id (UUID format)
- [ ] Get tmux window_id via `display-message`
- [ ] Atomic write to session_map.json with file locking
- [ ] `--install` subcommand for auto-config

### Step 1.9 — Main Binary & Integration (Days 12-14)

**Files**: `crates/aim-bin/src/{main,config}.rs`

- [ ] Config loading (env, .env, CLI args)
- [ ] Logging setup (tracing + env-filter)
- [ ] Signal handling (SIGTERM → graceful shutdown)
- [ ] Bot bootstrap flow:
  ```
  Load state → resolve stale IDs → start IM adapter
  → start monitor → enter poll loop
  ```
- [ ] Integration test: end-to-end message flow
- [ ] Integration test: session recovery after restart

**Deliverable**: `aim` binary that replaces `ccbot` for Telegram + Claude Code.

---

## Phase 2: Multi-Agent Support (Week 3)

**Goal**: Support Copilot CLI and Codex CLI alongside Claude Code.

### Step 2.1 — Agent Detection

- [ ] Detect agent type from pane process + output patterns
- [ ] Auto-select parser based on detected agent
- [ ] CLI command configuration per agent type

### Step 2.2 — Copilot CLI Parser

- [ ] Status line detection
- [ ] Interactive prompt detection
- [ ] Session management patterns

### Step 2.3 — Codex CLI Parser

- [ ] Task status detection
- [ ] Result parsing
- [ ] Interaction patterns

### Step 2.4 — Per-Agent Configuration

- [ ] Agent-specific tmux window naming
- [ ] Agent-specific hook registration
- [ ] Agent-specific command prefix

**Deliverable**: User can configure `AGENT_TYPE=claude|copilot|codex` and Aim adapts automatically.

---

## Phase 3: Feishu IM (Weeks 4-5)

**Goal**: Feishu (Lark) as a second IM backend alongside Telegram.

### Step 3.1 — Feishu Adapter

- [ ] Feishu event subscription (webhook receiver)
- [ ] Message receiving and sending API
- [ ] Image upload/download
- [ ] Topic/chat threading mapping

### Step 3.2 — Rich Content

- [ ] Markdown ↔ Feishu rich text conversion
- [ ] Inline keyboard via Feishu interactive cards
- [ ] File/photo attachment support

### Step 3.3 — Multi-IM Runtime

- [ ] Support running Telegram + Feishu simultaneously
- [ ] Per-user IM preference
- [ ] Unified event routing

**Deliverable**: `IM_BACKEND=telegram|feishu|both` config option.

---

## Phase 4: Polish & Hardening (Week 6)

- [ ] Auto-reconnect with exponential backoff (IM adapter)
- [ ] Disk-backed message queue (survive IM outage)
- [ ] Prometheus metrics (/metrics endpoint)
- [ ] Health check endpoint
- [ ] Systemd service file
- [ ] Release workflow (GitHub Actions)
- [ ] Comprehensive error handling audit
- [ ] Performance benchmarks vs Python CCBot

---

## Code Size Estimates per Phase

| Phase | Crate | New Code (Rust) |
|-------|-------|----------------|
| 1.1 | aim-core | ~500 |
| 1.2 | aim-tmux | ~600 |
| 1.3 | aim-parser | ~800 |
| 1.4 | aim-state | ~700 |
| 1.5 | aim-im | ~1200 |
| 1.6 | aim-monitor | ~400 |
| 1.7 | aim-queue | ~500 |
| 1.8 | aim-hook | ~200 |
| 1.9 | aim-bin | ~300 |
| 2 | Multi-Agent | ~1500 |
| 3 | Feishu IM | ~2000 |
| 4 | Polish | ~500 |
| **Total** | | **~10,000** |

## Key Dependencies

```toml
# Core
tokio = "1"                 # Async runtime
tower = "0.5"               # Service middleware (retry, backoff)
serde = "1" + serde_json    # Serialization
tracing = "0.1"             # Structured logging
thiserror = "2"             # Error types
async-trait = "0.1"         # Async trait support

# Tmux
# — direct CLI invocation via tokio::process::Command

# IM (Telegram)
teloxide = "0.13"           # Telegram Bot API

# IM (Feishu) — Phase 3
reqwest = { version = "0.12", features = ["json"] }  # HTTP client
jsonwebtoken = "9"          # JWT for Feishu auth (Phase 3)

# Parsing
regex = "1"                 # Pattern matching

# State
chrono = "0.4"              # Timestamps
uuid = "1"                  # Session IDs
```

## Quick Start

```bash
# Development
cargo build 2>&1 | head -5

# Test
cargo test

# Run (dev mode)
AIM_TELEGRAM_TOKEN=xxx AIM_ALLOWED_USERS=123 cargo run

# Release build
cargo build --release
```

## Testing Strategy

- **Unit tests**: each parser module, state transitions, message queue
- **Integration tests**: with real tmux (CI with tmux installed)
- **Smoke test**: compare JSONL parsing output with Python CCBot
- **Chaos test**: kill tmux window mid-session, verify recovery
