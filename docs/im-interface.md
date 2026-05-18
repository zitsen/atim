# IM Interface Documentation — Aim

**Aim** (AI Agent through IM) is a bridge between IM platforms and AI coding agents.
This document describes how to add a new IM backend.

## Trait Definition

All IM backends implement the `ImAdapter` trait defined in `aim-core/src/im.rs`:

```rust
#[async_trait]
pub trait ImAdapter: Send + Sync {
    /// Start the bot and begin receiving events.
    ///
    /// Events are emitted into `tx` as they arrive. This method should block
    /// for the lifetime of the application.
    async fn run(self: Box<Self>, tx: mpsc::UnboundedSender<ImEvent>) -> Result<()>;

    /// Send a text message to a chat/thread.
    async fn send_message(&self, target: &MessageTarget, text: &str) -> Result<MessageId>;

    /// Edit an existing message in-place.
    async fn edit_message(&self, target: &MessageTarget, msg_id: &MessageId, text: &str) -> Result<()>;

    /// Send a photo/document to a chat/thread.
    async fn send_photo(&self, target: &MessageTarget, filename: &str, data: &[u8]) -> Result<MessageId>;

    /// Send an inline keyboard markup.
    async fn send_keyboard(&self, target: &MessageTarget, text: &str, buttons: &[Vec<Button>]) -> Result<MessageId>;

    /// Delete a message.
    async fn delete_message(&self, target: &MessageTarget, msg_id: &MessageId) -> Result<()>;

    /// Edit an existing message's keyboard markup.
    async fn edit_keyboard(&self, target: &MessageTarget, msg_id: &MessageId, buttons: &[Vec<Button>]) -> Result<()>;
}
```

## Core Types

```rust
/// Platform-neutral user identifier.
pub struct UserId(pub i64);

/// Platform-neutral chat/group identifier.
pub struct ChatId(pub i64);

/// Platform-neutral message identifier.
pub struct MessageId(pub i32);

/// Target for outbound messages: chat + optional thread/topic.
pub struct MessageTarget {
    pub chat_id: ChatId,
    pub thread_id: Option<ThreadId>,
}

/// An event received from an IM platform.
pub struct ImEvent {
    pub user_id: UserId,
    pub target: MessageTarget,
    pub kind: ImEventKind,
}

/// Supported inbound event types.
pub enum ImEventKind {
    Text(String),
    Photo { caption: Option<String>, data: Vec<u8>, mime_type: String },
    Voice(Vec<u8>),
    CallbackQuery { data: String, msg_id: MessageId },
    TopicClosed,
    TopicEdited { new_name: String },
}

/// Inline keyboard button.
pub struct Button {
    pub text: String,
    pub callback_data: String,
}
```

## Adding a New IM Backend

1. **Create a new module** in `crates/aim-im/src/`, e.g. `feishu.rs` or `discord.rs`

2. **Implement `ImAdapter`** — all 7 methods are required:
   - `run()` — enter the event loop, parse inbound messages into `ImEvent`, forward through `tx`
   - `send_message()` — basic text output to a chat/thread
   - `edit_message()` — update a previously sent message
   - `send_photo()` — send an image or file
   - `send_keyboard()` — send interactive buttons
   - `delete_message()` — remove a message
   - `edit_keyboard()` — update buttons on an existing message

3. **Register the backend** in `crates/aim-bin/src/main.rs`:
   ```rust
   let im_adapter: Box<dyn ImAdapter> = match im_backend {
       "telegram" => Box::new(TelegramAdapter::new(token)),
       "feishu"   => Box::new(FeishuAdapter::new(config)),
       _          => return Err("unknown IM backend"),
   };
   ```

## Event Flow

```
IM Platform (Telegram/Feishu/etc.)
  ↓  inbound message
ImAdapter::run()  →  tx.send(ImEvent)
  ↓
Server event loop  →  resolve thread binding  →  TmuxManager::send_keys()
  ↓  agent responds
SessionMonitor  →  parses JSONL  →  MessageQueue  →  ImAdapter::send_message()
```

## Inbound Event Mapping

Each IM backend must map platform-specific events to `ImEventKind`:

| Platform Event     | ImEventKind              |
|--------------------|--------------------------|
| Plain text message | `Text(String)`           |
| Image/photo        | `Photo { caption, data, mime_type }` |
| Voice message      | `Voice(Vec<u8>)`         |
| Button press       | `CallbackQuery { data, msg_id }` |
| Topic closed       | `TopicClosed`            |
| Topic renamed      | `TopicEdited { new_name }` |

## Required Configuration

Each IM backend reads its config from environment variables in `Config`:

```rust
pub struct Config {
    // ── Telegram ──
    pub telegram_bot_token: String,
    pub allowed_users: Vec<i64>,

    // ── Future backends add fields here ──
    // pub feishu_app_id: String,
    // pub feishu_app_secret: String,
    // pub discord_bot_token: String,
}
```

## Currently Implemented

| Backend   | Module              | Status     |
|-----------|---------------------|------------|
| Telegram  | `aim-im::telegram`  | Implemented (long-polling, inline keyboards, topic support) |
| Feishu    | —                   | Planned (Phase 3) |
| Discord   | —                   | Future     |
| Slack     | —                   | Future     |
