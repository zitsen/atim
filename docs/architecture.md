# Aim Architecture

**Aim** (AI Agent through IM) — Rust rewrite of CCBot. A multi-IM bridge that remotely controls AI coding CLI agents (Claude Code, Copilot CLI, Codex CLI) via tmux.

## Core Design Principle

> **tmux as the universal control layer** — Aim operates on tmux, not on SDKs. Any CLI agent can be controlled without code changes, because we read its output and send keystrokes at the terminal level.

## Architecture Diagram

```
  ┌─────────────────┐    ┌─────────────────────┐    ┌─────────────────┐
  │   Telegram       │    │     IM Adapter      │    │    Feishu       │
  │   Bot API        │◀──▶│      (trait)        │◀──▶│    Bot API      │
  └─────────────────┘    └──────────┬──────────┘    └─────────────────┘
                                    │
                      ┌─────────────┴─────────────┐
                      │       aim-queue            │
                      │  Message Queue + Merging   │
                      └─────────────┬─────────────┘
                                    │
         ┌──────────────────────────┼──────────────────────────┐
         │                          │                          │
         ▼                          ▼                          ▼
  ┌──────────────┐       ┌──────────────────┐       ┌────────────────┐
  │  aim-tmux    │       │  aim-monitor     │       │  aim-state     │
  │  Tmux Ctrl    │       │  Session Monitor  │       │  Session State  │
  │  + Parser     │       │  (async poll)     │       │  + Persistence  │
  └──────┬───────┘       └────────┬─────────┘       └────────────────┘
         │                        │
         ▼                        ▼
  ┌──────────────┐       ┌────────────────┐
  │  aim-parser │       │  aim-parser   │
  │  Terminal UI │       │  JSONL Parser  │
  │  Detection   │       │  (agent output)│
  └──────────────┘       └────────────────┘
         │
         ▼
  ┌────────────────────────────────────────────┐
  │            tmux session                     │
  │  ┌──────────┐  ┌──────────┐  ┌──────────┐ │
  │  │  Claude   │  │  Copilot │  │  Codex   │ │
  │  │  Code     │  │  CLI     │  │  CLI     │ │
  │  └──────────┘  └──────────┘  └──────────┘ │
  └────────────────────────────────────────────┘
```

## Crate Map

| Crate | Layer | Dependencies | Lines (est.) |
|-------|-------|-------------|-------------|
| **aim-core** | Foundation | — | 500 |
| **aim-tmux** | Terminal I/O | aim-core | 600 |
| **aim-parser** | Output Parsing | aim-core | 800 |
| **aim-im** | IM Adapter | aim-core | 1200 |
| **aim-queue** | Message Pipeline | aim-core, aim-im | 500 |
| **aim-state** | Persistence | aim-core | 700 |
| **aim-monitor** | Session Watching | aim-parser, aim-state, aim-queue | 400 |
| **aim-hook** | Session Hook | aim-state | 200 |
| **aim-bin** | Entry Point | everything | 300 |

## Trait Design

### IM Adapter (`aim-core`)

```rust
/// Unified IM interface — Telegram and Feishu are impls.
#[async_trait]
pub trait ImAdapter: Send + Sync {
    /// Start the bot and return a receiver for inbound messages.
    async fn run(self: Box<Self>, tx: mpsc::UnboundedSender<ImEvent>) -> Result<()>;

    /// Send text message to a chat/thread.
    async fn send_message(&self, target: &MessageTarget, text: &str) -> Result<MessageId>;

    /// Edit an existing message.
    async fn edit_message(&self, target: &MessageTarget, msg_id: &MessageId, text: &str) -> Result<()>;

    /// Send a photo/image.
    async fn send_photo(&self, target: &MessageTarget, filename: &str, data: &[u8]) -> Result<MessageId>;

    /// Send keyboard markup for interactive UI navigation.
    async fn send_keyboard(&self, target: &MessageTarget, text: &str, buttons: &[Vec<Button>]) -> Result<MessageId>;

    /// Delete a message.
    async fn delete_message(&self, target: &MessageTarget, msg_id: &MessageId) -> Result<()>;
}
```

### Agent Parser (`aim-core`)

```rust
/// Agent-specific output format detection and parsing.
pub trait AgentParser: Send + Sync {
    /// Detect which agent is running in a pane.
    fn detect(pane_text: &str, process_name: &str) -> AgentKind;

    /// Parse the status line from terminal output.
    fn parse_status(&self, pane_text: &str) -> Option<String>;

    /// Detect if terminal is showing an interactive UI (question/prompt).
    fn detect_interactive(&self, pane_text: &str) -> Option<InteractiveUi>;

    /// Parse the current prompt state (e.g. waiting for input).
    fn parse_prompt_state(&self, pane_text: &str) -> PromptState;
}
```

### Core Types (`aim-core`)

```rust
pub struct MessageTarget {
    pub chat_id: ChatId,
    pub thread_id: Option<i64>,
}

pub struct ImEvent {
    pub user_id: UserId,
    pub chat_id: ChatId,
    pub thread_id: Option<i64>,
    pub kind: ImEventKind,
    pub text: Option<String>,
    pub photo: Option<Vec<u8>>,
    pub voice: Option<Vec<u8>>,
}

pub enum ImEventKind {
    Text,
    Photo,
    Voice,
    CallbackQuery { data: String, msg_id: MessageId },
    TopicClosed,
    TopicEdited { new_name: String },
}

pub enum AgentKind {
    ClaudeCode,
    CopilotCli,
    CodexCli,
    Unknown,
}

pub struct InteractiveUi {
    pub kind: UiKind,
    pub content: String,
}

pub enum UiKind {
    AskUserQuestion,
    ExitPlanMode,
    PermissionPrompt,
    Settings,
    Unknown,
}
```

## Data Flow

### Outbound (User → Agent via tmux)

```
User sends "fix the bug" in Telegram topic
  → IM Adapter receives Message(text)
  → Emits ImEvent { kind: Text, text: "fix the bug" }
  → State router resolves thread_binding → window_id
  → aim-tmux.send_keys(window_id, "fix the bug")
  → Agent sees input in tmux pane
```

### Inbound (Agent → User via Telegram)

```
Agent writes to session.jsonl
  → aim-monitor polls JSONL (byte offset tracking)
  → aim-parser parses new entries (text, tool_use, tool_result)
  → Formats response parts
  → aim-queue enqueues for delivery
  → IM Adapter sends message to correct topic

Concurrently:
  aim-monitor also captures tmux pane text
  → Detects interactive UI
  → Sends interactive UI with inline keyboard via IM Adapter
```

## Multi-Agent Strategy

Each tmux window runs exactly one agent CLI. Aim detects the agent type by:

1. **Pane process inspection** — read `pane_current_command` from tmux
2. **Directory detection** — `.claude/` → Claude Code, `.github/copilot/` → Copilot CLI
3. **Output format matching** — first output line patterns differentiate agents

AgentParser trait implementations:
- `ClaudeParser` — regex patterns from original terminal_parser.py (AskUserQuestion, ExitPlanMode, PermissionPrompt, status spinners)
- `CopilotParser` — Copilot CLI specific patterns (session management, multi-turn)
- `CodexParser` — Codex CLI specific patterns (task definitions, results)

## IM State Flow

```
                    ┌─────────────┐
                    │ Inbound Msg │
                    └──────┬──────┘
                           ▼
                    ┌──────────────┐      ┌─────────────────┐
              ┌────▶│ Topic Bound? │──No──▶│ DirectoryBrowse │
              │     └──────┬───────┘      │ or WindowPicker │
              │            │ Yes          └────────┬────────┘
              │            ▼                       │
              │     ┌──────────────┐               │
              │     │ Window Alive?│──No──▶ Unbind │
              │     └──────┬───────┘               │
              │            │ Yes                   │
              │            ▼                       ▼
              │     ┌──────────────────┐    ┌──────────────┐
              │     │ Send to Tmux     │    │ Create/Bind  │
              │     └──────────────────┘    │ Window       │
              │                             └──────────────┘
              │                                      │
              └──────────────────────────────────────┘
```

## Error Handling

- `aim-core` defines `Error` enum with typed variants
- Each crate has its own error type, converting to/from core Error via `From`
- Network failures in IM adapter: exponential backoff, message queued to disk
- Tmux failures (window gone): clear binding, notify user
- JSONL parse failures: skip malformed line, log warning, continue

## Persistence

All state in `~/.aim/` directory (`AIM_DIR` env var override):

| File | Purpose |
|------|---------|
| `state.json` | Thread bindings, window states, display names |
| `session_map.json` | Hook-generated window_id → session_id mapping |
| `monitor_state.json` | Byte offsets per session JSONL |
| `queue/*.json` | Persisted messages during IM outage |

## Startup Recovery

1. Load persisted state from `~/.aim/`
2. Re-resolve stale window IDs against live tmux windows
3. Clean up session_map entries for dead windows
4. Initialize monitor byte offsets (prevent duplicate notifications)
5. Start IM adapter → enter poll loop

## Shutdown

1. Save all state
2. Stop monitor poll loop
3. Drain and persist message queue
4. IM adapter graceful shutdown
