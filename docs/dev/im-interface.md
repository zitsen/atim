# IM Interface — Developer Guide

How to add a new IM backend to Atim.

## Trait Definition

All IM backends implement the `ImAdapter` trait defined in `atim-core/src/im.rs`:

```rust
#[async_trait]
pub trait ImAdapter: Send + Sync {
    /// Start the bot and begin receiving events.
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

    /// Update buttons on an existing message.
    async fn edit_keyboard(&self, target: &MessageTarget, msg_id: &MessageId, buttons: &[Vec<Button>]) -> Result<()>;

    /// Send a structured check/health card.
    async fn send_check_card(&self, target: &MessageTarget, ...) -> Result<MessageId>;

    /// Send a typing/chat-action indicator.
    async fn send_chat_action(&self, target: &MessageTarget, action: &str) -> Result<()>;

    /// Answer a callback query (acknowledge button press).
    async fn answer_callback(&self, callback_id: &str, text: &str) -> Result<()>;

    /// Add a reaction emoji to a message.
    async fn add_reaction(&self, target: &MessageTarget, msg_id: &MessageId, emoji: &str) -> Result<()>;

    /// Send a key-value table card.
    async fn send_kv_table(&self, target: &MessageTarget, title: &str, kv: &[(String, String)]) -> Result<MessageId>;
}
```

## Core Types

```rust
/// Platform-neutral user identifier.
pub struct UserId(pub i64);

/// Platform-neutral chat/group identifier.
pub struct ChatId(pub i64);

/// Platform-neutral message identifier (String to support Feishu's string IDs).
pub struct MessageId(pub String);

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
    Text { text: String, is_mention: bool, is_group: bool, message_id: Option<String> },
    Photo { caption: Option<String>, data: Vec<u8>, mime_type: String },
    Voice(Vec<u8>),
    CallbackQuery { data: String, msg_id: MessageId },
    TopicCreated { topic_name: String },
    TopicClosed,
    TopicEdited { new_name: String },
    BotAdded,
}

/// Inline keyboard button.
pub struct Button {
    pub text: String,
    pub callback_data: String,
}
```

## Adding a New IM Backend

1. **Create a new module** in `crates/atim-im/src/`, e.g. `discord.rs`

2. **Implement `ImAdapter`** — all methods are required

3. **Register the backend** in `crates/atim-bin/src/main.rs`:
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
SessionMonitor  →  parses JSONL / DB  →  MessageQueue  →  ImAdapter::send_message()
```

## Inbound Event Mapping

| Platform Event     | ImEventKind              |
|--------------------|--------------------------|
| Plain text message | `Text { text, is_mention, is_group, message_id }` |
| Image/photo        | `Photo { caption, data, mime_type }` |
| Voice message      | `Voice(Vec<u8>)`         |
| Button press       | `CallbackQuery { data, msg_id }` |
| Bot added to group | `BotAdded`               |
| Topic created      | `TopicCreated { topic_name }` |
| Topic closed       | `TopicClosed`            |
| Topic renamed      | `TopicEdited { new_name }` |

## Currently Implemented

| Backend   | Module              | Status     |
|-----------|---------------------|------------|
| Telegram  | `atim-im::telegram`  | Implemented (long-polling, inline keyboards, topic support) |
| Feishu    | `atim-im::feishu`    | Implemented (webhook + polling, interactive cards, rich text) |
