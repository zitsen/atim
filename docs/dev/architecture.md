# Atim Architecture

**Atim** (AI Agent through IM) — Rust bridge that remotely controls AI coding CLI agents (Claude Code, Copilot CLI, Codex CLI, Mimo) via tmux.

## Core Design Principle

> **tmux as the universal control layer** — Atim operates on tmux, not on SDKs. Any CLI agent can be controlled without code changes, because we read its output and send keystrokes at the terminal level.

## Architecture Diagram

```
  ┌─────────────────┐    ┌─────────────────────┐    ┌─────────────────┐
  │   Telegram       │    │     IM Adapter      │    │    Feishu       │
  │   Bot API        │◀──▶│      (trait)        │◀──▶│    Bot API      │
  └─────────────────┘    └──────────┬──────────┘    └─────────────────┘
                                    │
                      ┌─────────────┴─────────────┐
                      │       atim-queue            │
                      │  Message Queue + Merging   │
                      └─────────────┬─────────────┘
                                    │
         ┌──────────────────────────┼──────────────────────────┐
         │                          │                          │
         ▼                          ▼                          ▼
  ┌──────────────┐       ┌──────────────────┐       ┌────────────────┐
  │  atim-tmux    │       │  atim-monitor     │       │  atim-state     │
  │  Tmux Ctrl    │       │  Session Monitor  │       │  Session State  │
  │  + Parser     │       │  (async poll)     │       │  + Persistence  │
  └──────┬───────┘       └────────┬─────────┘       └────────────────┘
         │                        │
         ▼                        ▼
  ┌──────────────┐       ┌────────────────┐
  │  atim-parser │       │  atim-parser   │
  │  Terminal UI │       │  JSONL / DB    │
  │  Detection   │       │  (agent output)│
  └──────────────┘       └────────────────┘
         │
         ▼
  ┌────────────────────────────────────────────┐
  │            tmux session                     │
  │  ┌──────────┐  ┌──────────┐  ┌──────────┐ │
  │  │  Claude   │  │  Copilot │  │  Codex   │ │
  │  │  Code     │  │  CLI     │  │  CLI     │ │
  │  │  / Mimo   │  │          │  │          │ │
  │  └──────────┘  └──────────┘  └──────────┘ │
  └────────────────────────────────────────────┘
```

## Crate Map

| Crate | Layer | Dependencies |
|-------|-------|-------------|
| **atim-core** | Foundation (config, errors, IM trait, agent abstraction) | — |
| **atim-tmux** | Terminal I/O, screenshot rendering | atim-core |
| **atim-parser** | JSONL log and terminal output parsing | atim-core |
| **atim-im** | Telegram + Feishu adapter implementations | atim-core |
| **atim-queue** | Per-user async message queues with flood control | atim-core, atim-im |
| **atim-state** | Thread binding and window state persistence (SQLite) | atim-core |
| **atim-monitor** | Session log polling with byte-offset tracking | atim-parser, atim-state, atim-queue |
| **atim-bin** | Entry point — server + CLI | everything |

## Trait Design

### IM Adapter (`atim-core`)

```rust
#[async_trait]
pub trait ImAdapter: Send + Sync {
    async fn run(self: Box<Self>, tx: mpsc::UnboundedSender<ImEvent>) -> Result<()>;
    async fn send_message(&self, target: &MessageTarget, text: &str) -> Result<MessageId>;
    async fn edit_message(&self, target: &MessageTarget, msg_id: &MessageId, text: &str) -> Result<()>;
    async fn send_photo(&self, target: &MessageTarget, filename: &str, data: &[u8]) -> Result<MessageId>;
    async fn send_keyboard(&self, target: &MessageTarget, text: &str, buttons: &[Vec<Button>]) -> Result<MessageId>;
    async fn delete_message(&self, target: &MessageTarget, msg_id: &MessageId) -> Result<()>;
    async fn edit_keyboard(&self, target: &MessageTarget, msg_id: &MessageId, buttons: &[Vec<Button>]) -> Result<()>;
    async fn send_check_card(&self, target: &MessageTarget, ...) -> Result<MessageId>;
    async fn send_chat_action(&self, target: &MessageTarget, action: &str) -> Result<()>;
    async fn answer_callback(&self, callback_id: &str, text: &str) -> Result<()>;
    async fn add_reaction(&self, target: &MessageTarget, msg_id: &MessageId, emoji: &str) -> Result<()>;
    async fn send_kv_table(&self, target: &MessageTarget, title: &str, kv: &[(String, String)]) -> Result<MessageId>;
}
```

### Agent Parser (`atim-core`)

```rust
pub trait AgentParser: Send + Sync {
    fn detect(pane_text: &str, process_name: &str) -> AgentKind;
    fn parse_status(&self, pane_text: &str) -> Option<String>;
    fn detect_interactive(&self, pane_text: &str) -> Option<InteractiveUi>;
    fn parse_prompt_state(&self, pane_text: &str) -> PromptState;
}
```

### Core Types (`atim-core`)

```rust
pub enum AgentKind {
    ClaudeCode,
    CopilotCli,
    CodexCli,
    MimoCode,
    Unknown,
}

pub enum ImEventKind {
    Text { text: String, is_mention: bool, is_group: bool, message_id: Option<String> },
    Photo { caption: Option<String>, data: Vec<u8>, mime_type: String },
    Voice(Vec<u8>),
    CallbackQuery { data: String, msg_id: MessageId },
    TopicCreated { topic_name: String },
    TopicClosed,
    TopicEdited { new_name: String },
    BotAdded,
}

pub struct MessageId(pub String);
```

## Data Flow

### Outbound (User → Agent via tmux)

```
User sends "fix the bug" in Telegram topic
  → IM Adapter receives Message(text)
  → Emits ImEvent { kind: Text, text: "fix the bug" }
  → State router resolves thread_binding → window_id
  → atim-tmux.send_keys(window_id, "fix the bug")
  → Agent sees input in tmux pane
```

### Inbound (Agent → User via IM)

```
Agent writes to session JSONL (or Mimo's SQLite DB)
  → atim-monitor polls JSONL / DB (byte offset tracking)
  → atim-parser parses new entries (text, tool_use, tool_result)
  → Detects Edit tool → formats diff card
  → Detects interactive UI → sends inline keyboard
  → atim-queue enqueues for delivery
  → IM Adapter sends message to correct topic
```

## Multi-Agent Strategy

Each tmux window runs exactly one agent CLI. Atim detects the agent type by:

1. **Pane process inspection** — read `pane_current_command` from tmux
2. **Directory detection** — `.claude/` → Claude Code, `.github/copilot/` → Copilot CLI
3. **Output format matching** — first output line patterns differentiate agents

AgentParser implementations:
- `ClaudeParser` — JSONL-based session tracking, AskUserQuestion/ExitPlanMode/PermissionPrompt detection
- `CopilotParser` — Copilot CLI patterns (session management, multi-turn)
- `CodexParser` — Codex CLI patterns (task definitions, results); uses pane capture instead of JSONL
- `MimoParser` — reuses ClaudeParser logic, polls Mimo's SQLite database

## Persistence

All state stored in `~/.atim/store.db` (SQLite):

| Table | Purpose |
|-------|---------|
| chat_bindings | IM conversation ↔ session mappings |
| window_bindings | session ↔ tmux window mappings |
| session_info | Session metadata (agent type, cwd, display name) |
| monitor_state | Byte offsets per session JSONL |

Legacy `state.json` / `session_map.json` paths now resolve to `store.db`.

## Startup Recovery

1. Load persisted state from `store.db`
2. Re-resolve stale window IDs against live tmux windows
3. Clean up session_map entries for dead windows
4. Initialize monitor byte offsets (prevent duplicate notifications)
5. Consume hook session_map for newly discovered sessions
6. Start IM adapter → enter poll loop

## Shutdown

1. Save all state
2. Stop monitor poll loop
3. Drain and persist message queue
4. IM adapter graceful shutdown
