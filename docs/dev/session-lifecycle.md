# Session Lifecycle Design

## Overview

User sessions have a strong binding: **one IM conversation ↔ one Agent session**. The IM abstraction layer uses `thread_id` to connect or reconnect sessions, and `thread_name` (or `chat_name` for Feishu groups/P2P) as the session display name.

## Core Principles

- **thread_id** is the stable unique identifier for routing
- **display_name** is derived from the IM context (thread name / chat name) but allows renaming
- **agent session_id** is the Agent-side session identifier (for Claude: UUID via JSONL)
- When the same IM thread reconnects after any disruption, the binding is re-established to the **same** agent session if possible

## Session States

```
                     ┌─────────────────────────┐
                     │     No Binding           │
                     │  (first time user/       │
                     │   thread)                │
                     └────────┬────────────────┘
                              │
                     ┌────────▼────────────────┐
                     │  Setup Flow              │
                     │  1. Choose Agent type    │
                     │  2. Choose working dir   │
                     │  3. Create tmux window   │
                     │  4. Launch Agent         │
                     │  5. Pick session/new     │
                     └────────┬────────────────┘
                              │
                     ┌────────▼────────────────┐
          ┌──────────│   Bound (Active)         │──────────┐
          │          │  thread_id ↔ window_id   │          │
          │          │  ↔ agent session_id      │          │
          │          └────────┬──────────────────┘          │
          │                   │                             │
          │          ┌────────▼──────────────────┐          │
          │          │  Message arrives           │          │
          │          └────────┬──────────────────┘          │
          │                   │                             │
          │         ┌─────────┴─────────┐                   │
          │         │                   │                   │
          │    ┌────▼────┐        ┌─────▼─────┐            │
          │    │ window  │        │ window    │            │
          │    │ exists  │        │ dead      │            │
          │    └────┬────┘        └─────┬─────┘            │
          │         │                   │                   │
          │    ┌─────┴─────────┐  ┌─────┴──────────┐       │
          │    │ check match   │  │ Prompt user:    │       │
          │    ├── all OK ──►  │  │ recover or new  │       │
          │    ├── chat_name   │  └────────────────┘       │
          │    │   mismatch    │                            │
          │    ├── agent_type  │                            │
          │    │   mismatch    │                            │
          │    └───────────────┘                            │
          │                                                 │
          └─────────────────────────────────────────────────┘
```

## Message Arrival Routing

When a user sends a message and a binding exists:

### 3.1 Available Conversation Loop

All conditions met:
- `thread_id` found in binding
- `window_id` exists in tmux
- tmux window **name matches** binding `display_name`
- window process **agent_type matches** binding agent type

→ Forward message directly to the agent.

### 3.2 Chat Name Mismatch

Binding exists, tmux window is alive, but target chat name differs from binding display name.

- **Cause**: IM group/chat was renamed, or binding created with stale name.
- **Action**: Prompt with inline buttons: **Rename** (update binding + tmux window name) or **New Session**.
- **Fallback**: Auto-accept rename after timeout.

### 3.3 Agent Type Mismatch

Binding exists, tmux window is alive, but running agent type differs from binding.

- **Cause**: Agent manually switched or tmux window repurposed.
- **Action**: Prompt with inline buttons: **Update Binding** or **New Session**.

### 3.4 Window Dead (Binding Orphaned)

Binding exists but tmux window no longer exists.

- **Cause**: tmux session killed, window closed, or system restart.
- **Action**: Prompt with inline buttons: **Recover** or **New Session**.

## New Session Setup Flow

1. **Choose Agent Type** — Inline keyboard (claude/copilot/codex/mimo)
2. **Choose Working Directory** — Directory browser with zoxide quick-jump
3. **Create tmux Window** — `tmux_mgr.new_window()` in chosen directory
4. **Launch Agent** — `tmux_mgr.send_line()` with agent launch command
5. **Choose New or Resume Session** — If agent supports sessions, show picker with existing sessions + "New" option
6. **Notify User** — "Session ready!"

## Session Recovery Flow

1. **Create tmux Window** — in original `cwd`
2. **Launch Agent** — using stored agent type
3. **Resume Session** — if session_id exists, use `agent.resume_command(session_id)`
4. **Update Binding** — re-key: old window_id → new window_id
5. **Notify User** — "Session ready!"

## Key Data Structures

### ChatBinding (persisted in SQLite)

```rust
pub struct ChatBinding {
    pub user_id: String,
    pub thread_id: String,
    pub chat_id: String,
    pub session_id: String,
    pub display_name: String,
}
```

### WindowBinding (persisted in SQLite)

```rust
pub struct WindowBinding {
    pub session_id: String,
    pub window_id: String,
}
```

### SessionInfo (persisted in SQLite)

```rust
pub struct SessionInfo {
    pub session_id: String,
    pub cwd: String,
    pub window_name: String,
    pub agent_type: String,  // "claude", "copilot", "codex", "mimo"
}
```

## Session Discovery Priority (for rebind)

Non-disruptive methods to find a session's UUID:

1. **PID via lsof** — trace open file handles to find JSONL UUID
2. **Pane text scan** — regex for UUID in captured pane output
3. **session_map** — last resort, cached mapping from hook
