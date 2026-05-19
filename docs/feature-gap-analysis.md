# Feature Gap Analysis: atim vs ccbot

Date: 2026-05-18
Baseline: ccbot (Python) → atim (Rust)

## Priority Legend

- **P0**: Core functionality — users cannot effectively use the bot without these
- **P1**: Major UX — significant usability improvement
- **P2**: Polish — nice-to-have enhancements

---

## P0 — Core Functionality

### P0.1 Telegram Forum Topics Lifecycle

**ccbot**: Operates exclusively in supergroup forum topics mode. One topic = one tmux window = one Claude session. Detects topic creation, deletion (via 60s poll probe), and rename. Cleans up window+binding on topic close.

**atim**: Has `ImEventKind::TopicCreated/Edited` variants but no cleanup on close/delete, no topic deletion detection, no rename sync.

**Required**:
- Handle `forum_topic_closed` → kill window + unbind
- Handle topic deletion detection (periodic probe via `unpin_all_forum_topic_messages` or equivalent)
- Sync topic rename → tmux window name
- Bind lifecycle: unbound topic → create window → bind → accept messages

### P0.2 Directory Browser + Session Picker

**ccbot**: When a new topic sends its first message, an inline keyboard UI lets users navigate directories and either create a new Claude session or resume an existing one. Supports pagination, "up" navigation, and cancel.

**atim**: No directory browsing. Only supports `resume_session_id` in config for manual session selection.

**Required**:
- Inline keyboard directory navigation (pages, up/down, select, cancel)
- Scan for existing Claude Code sessions in selected directory
- Session picker with resume/new options

### P0.3 Window Picker

**ccbot**: When binding a topic and unbound windows exist, shows a picker to attach to a running session instead of creating a new one.

**atim**: No window picker.

**Required**:
- Detect unbound tmux windows
- Inline keyboard list of available windows
- Bind selected window to topic

### P0.4 Startup Re-Resolution

**ccbot**: On restart, re-maps persisted window IDs against live tmux windows by matching display names. Auto-migrates old-format state. Cleans up orphaned `session_map.json` entries.

**atim**: No startup re-resolution. Stale window IDs cause silent failures.

**Required**:
- On server startup, validate all persisted window IDs against tmux
- Re-resolve by display name if window ID changed
- Clean up orphaned state entries

### P0.5 Inline Keyboard Callback Validation

**ccbot**: Stale browser/picker/session callbacks from a different topic are detected and rejected with "topic mismatch" alerts.

**atim**: No callback validation. Stale callbacks could cross-bind topics.

**Required**:
- Embed `(user_id, chat_id, thread_id)` context in callback data
- Validate on receipt before dispatching

---

## P1 — Major UX

### P1.1 Terminal Screenshot (PNG)

**ccbot**: Renders tmux pane content as PNG with ANSI color support (16/256/RGB) and 3-tier font fallback (Latin, CJK, symbols). Inline keyboard provides navigation keys (up/down/left/right/enter/esc/tab/space/ctrl-c).

**atim**: No screenshot capability.

### P1.2 Tool_use/Tool_result In-Place Editing

**ccbot**: Sends tool_use summary as a message, then edits it in-place when tool_result arrives. Gives a clean sequential display.

**atim**: Sends tool_use and tool_result as separate messages.

### P1.3 Status → Content Conversion

**ccbot**: The "Claude is working..." status message is edited in-place when actual content arrives, reducing visual clutter.

**atim**: No status message management.

### P1.4 Message Merging

**ccbot**: Consecutive content messages for the same window are merged before sending (up to 3800 chars). Tool_use/tool_result breaks the merge chain.

**atim**: Each parsed entry is sent as a separate message.

### P1.5 Tool_use/Tool_result Smart Summaries

**ccbot**: Different tools get different summary stats (e.g., "Read 42 lines", "Wrote 15 lines", "Found 5 matches"). `Edit` tool generates unified diff.

**atim**: No smart summaries.

### P1.6 Markdown Table → Card Conversion

**ccbot**: Converts markdown tables to card-style key-value text since Telegram doesn't support tables.

**atim**: Tables are rendered as raw markdown in HTML.

### P1.7 `/usage` Command

**ccbot**: Sends `/usage` to Claude Code, captures the modal output, parses it, and sends the result to the user. Dismisses the modal afterward.

**atim**: No `/usage` support.

### P1.8 `!` Command Output Capture

**ccbot**: When user sends `!<command>`, starts a background task reading tmux pane output every second, sending/editing results in Telegram.

**atim**: No `!` command support.

---

## P2 — Polish

### P2.1 Voice Message Transcription

**ccbot**: Downloads voice OGG, transcribes via OpenAI `gpt-4o-transcribe`, forwards text to Claude.

### P2.2 Interactive UI Navigation

**ccbot**: Inline keyboard for navigating AskUserQuestion, Permission prompts, Plan mode, Restore checkpoint, Settings. Supports arrows, space, tab, enter, esc.

### P2.3 Flood Control Queue

**ccbot**: Per-user async queues with `AIORateLimiter`. On 429 `RetryAfter`, delays content messages, drops status messages. Max wait: 10s.

### P2.4 Hook File Locking

**ccbot**: `fcntl.flock()` for concurrent-safe writes to `session_map.json`.

### P2.5 Topic Rename Sync

**ccbot**: Renaming a Telegram topic updates the tmux window name and display name mapping.

---

## Implementation Status

| Feature | Priority | Status | Target |
|---------|----------|--------|--------|
| Forum Topics Lifecycle | P0 | ✅ | 2026-05-18 |
| Directory Browser | P0 | ✅ | 2026-05-18 |
| Callback Validation | P0 | ✅ | 2026-05-18 |
| Startup Re-Resolution | P0 | ✅ | 2026-05-18 |
| Window Picker | P0 | ✅ | 2026-05-18 |
| Terminal Screenshot | P1 | ✅ | 2026-05-18 |
| Tool In-Place Editing | P1 | ✅ | 2026-05-18 |
| Status→Content | P1 | ✅ | 2026-05-18 |
| Message Merging | P1 | ✅ | 2026-05-18 |
| Smart Summaries | P1 | ✅ | 2026-05-18 |
| Table→Card | P1 | ✅ | 2026-05-18 |
| `/usage` Command | P1 | ✅ | 2026-05-18 |
| `!` Command Capture | P1 | ✅ | 2026-05-18 |
| Voice Transcription | P2 | ✅ | 2026-05-18 |
| Interactive UI Nav | P2 | ✅ | 2026-05-18 |
| Flood Control | P2 | ✅ | 2026-05-18 |
| Hook File Locking | P2 | ✅ | 2026-05-18 |
| Topic Rename Sync | P2 | ✅ | 2026-05-18 |
