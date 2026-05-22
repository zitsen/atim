# Session Lifecycle Design

## Overview

User sessions have a strong binding: **one IM conversation ↔ one Agent session**. The IM abstraction layer uses `thread_id` to connect or reconnect sessions, and `thread_name` (or `chat_name` for Feishu groups/P2P) as the session display name. Thread IDs are the unique identifier; display names can be renamed (e.g., on IM group rename events).

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
          │    ├── all OK ──► 3.1│  │ recover or new │       │
          │    ├── chat_name   │  └────────────────┘       │
          │    │   mismatch ──►│                            │
          │    │   3.2         │                            │
          │    ├── agent_type  │                            │
          │    │   mismatch ──►│                            │
          │    │   3.3         │                            │
          │    └───────────────┘                            │
          │                                                 │
          └─────────────────────────────────────────────────┘
```

## 3. Message Arrival Routing

When a user sends a message and a binding exists:

### 3.1 Available Conversation Loop

All conditions met:
- `thread_id` found in binding
- `window_id` exists in tmux
- tmux window **name matches** binding `display_name`
- window process **agent_type matches** binding `window_state.agent_type`

→ Forward message directly to the agent. No user interaction needed.

### 3.2 Chat Name Mismatch

Binding exists, tmux window is alive, but `target.chat_name` differs from `binding.display_name`.

- **Cause**: The IM group/chat was renamed but the rename event was missed, or the binding was created with a stale name.
- **Action**: Prompt the user with inline buttons:
  - **Rename** — Update `binding.display_name` and `window_state.window_name`, rename the tmux window, then forward the message
  - **New Session** — Treat as a new binding (run setup flow), but keep the old binding for history
- **Fallback**: If the user doesn't respond within a timeout, auto-accept rename

### 3.3 Agent Type Mismatch

Binding exists, tmux window is alive, but the actual running process agent type differs from `window_state.agent_type`.

- **Cause**: The agent was manually switched (e.g., `/switch` in another context) or the tmux window was repurposed.
- **Action**: Prompt the user with inline buttons:
  - **Update Binding** — Change `window_state.agent_type` to match the running agent, then forward the message
  - **New Session** — Create a fresh binding

### 3.4 Window Dead (Binding Orphaned)

Binding exists but tmux window no longer exists (`find_window` returns error).

- **Cause**: tmux session killed, window closed, or system restart.
- **Action**: Prompt the user with inline buttons:
  - **Recover** — Run recovery flow (see section 5)
  - **New Session** — Run new session setup flow (see section 4)

## 4. New Session Setup Flow

When no binding exists for the user+thread:

1. **Choose Agent Type** — Inline keyboard with agent options (claude/copilot/codex). Same as current `send_agent_picker`.
2. **Choose Working Directory** — Directory browser with zoxide quick-jump. Same as current `show_directory_browser`.
3. **Create tmux Window** — `tmux_mgr.new_window()` in the chosen directory.
4. **Launch Agent** — `tmux_mgr.send_line()` with the agent's launch command.
5. **Choose New or Resume Session** — After the agent starts:
   - Call `agent.scan_sessions(cwd)` to list available sessions
   - Show session picker with "New Session" option
   - If user picks an existing session → use `create_and_bind_with_resume`
   - If user picks "New" → notify user session is ready
6. **Notify User** — "Session ready! Send your message to start chatting."

## 5. Session Recovery Flow

When the user chooses "Recover" from a dead-window prompt:

1. **Create tmux Window** — `tmux_mgr.new_window()` in the original `cwd`
2. **Launch Agent** — Use the stored `agent_type` from `window_state`
3. **Resume Session** — If `window_state.session_id` is set:
   - Use `agent.resume_command(session_id)` to restore the session
   - Wait for the agent to resume
4. **Update Binding** — Re-key state: `window_id` in binding → new window_id
5. **Notify User** — "Session ready! Send your message to start chatting."

## Session Rename Handling

### IM-Level Rename Events

- **Telegram**: `TopicEdited` event — already handled in `handle_topic_edited`
- **Feishu group rename**: `im.chatUpdated` webhook event — needs a handler

When a rename event arrives:
- If `binding.display_name` differs from the new name:
  - Update `binding.display_name`
  - Update `window_state.window_name`
  - Rename the tmux window
  - Persist state

### Grace Period for Missed Renames

If a rename event was missed, the chat_name mismatch handler (3.2) covers it on the next user message.

## Key Data Structures

### ThreadBinding (persisted)

```rust
pub struct ThreadBinding {
    pub user_id: i64,
    pub thread_id: i64,
    pub chat_id: i64,
    pub window_id: String,
    pub display_name: String,
    pub group_chat_id: Option<i64>,
    pub topic_name: Option<String>,
}
```

### WindowState (persisted)

```rust
pub struct WindowState {
    pub session_id: String,
    pub cwd: String,
    pub window_name: String,
    pub agent_type: String,  // "claude", "copilot", "codex"
}
```

## Current Code vs Design Gap

| Component | Status | Change Required |
|-----------|--------|----------------|
| 3.1 Forward message | ✅ Done | None |
| 3.2 Chat name mismatch | ✅ Done | User prompt with rename/new/cancel |
| 3.3 Agent type mismatch | ✅ Done | Check + user prompt with rebind/new/cancel |
| 3.4 Window dead | ✅ Done | User prompt with recover/new/cancel |
| 4 New session flow | ✅ Mostly done | Use `agent.scan_sessions()` instead of hardcoded `scan_claude_sessions` |
| 5 Recovery flow | ✅ Done | `handle_recover_session` wired from 3.4 "Recover" button |
| Feishu chat rename | ❌ Not handled | Add `im.chatUpdated` event handler |

## Implementation Priority

1. **3.4 Window dead → user prompt** (highest impact, most common)
2. **3.2 Chat name mismatch → user prompt**
3. **3.3 Agent type mismatch → user prompt**
4. **New session flow: use `agent.scan_sessions()` for Clude+others**
5. **Feishu chat rename event handler**
