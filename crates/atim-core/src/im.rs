use async_trait::async_trait;
use tokio::sync::mpsc;

use crate::error::Result;
use crate::message::{CheckItem, ImEvent, MessageId, MessageTarget};

/// Unified IM interface — Telegram and Feishu implement this trait.
///
/// The bot is started by calling `run()`, which begins listening for inbound
/// events and forwards them through the `tx` channel.  Outbound operations
/// are called directly on the adapter.
///
/// All methods must be Send + Sync to support multi-threaded dispatch.
#[async_trait]
pub trait ImAdapter: Send + Sync {
    /// Start the bot and begin receiving events.
    ///
    /// Events are emitted into `tx` as they arrive. This method should block
    /// for the lifetime of the application (typically polls the Telegram/Feishu API).
    async fn run(&self, tx: mpsc::UnboundedSender<ImEvent>) -> Result<()>;

    /// Send a text message to a chat/thread.
    ///
    /// Essential content — assistant replies, command output the user asked
    /// for. Never dropped by rate limiting.
    async fn send_message(&self, target: &MessageTarget, text: &str) -> Result<MessageId>;

    /// Send expendable tool chatter (a tool call or a tool result).
    ///
    /// Indistinguishable from [`ImAdapter::send_message`] on the wire, but
    /// marked as droppable so flood control can shed it during a tool storm —
    /// e.g. hundreds of identical `git status` results a minute — instead of
    /// burying the chat. Implementations that do not rate limit may simply
    /// delegate; only the flood-controlled wrapper needs to tell them apart.
    ///
    /// Returns [`crate::error::Error::Dropped`] when the message was shed.
    async fn send_chatter(&self, target: &MessageTarget, text: &str) -> Result<MessageId> {
        self.send_message(target, text).await
    }

    /// Edit an existing message in-place.
    async fn edit_message(
        &self,
        target: &MessageTarget,
        msg_id: &MessageId,
        text: &str,
    ) -> Result<()>;

    /// Edit an expendable tool chatter message in-place.
    ///
    /// The droppable counterpart of [`ImAdapter::edit_message`]; see
    /// [`ImAdapter::send_chatter`]. Returns [`crate::error::Error::Dropped`]
    /// when the update was shed.
    async fn edit_chatter(
        &self,
        target: &MessageTarget,
        msg_id: &MessageId,
        text: &str,
    ) -> Result<()> {
        self.edit_message(target, msg_id, text).await
    }

    /// Send a photo/document to a chat/thread.
    async fn send_photo(
        &self,
        target: &MessageTarget,
        filename: &str,
        data: &[u8],
    ) -> Result<MessageId>;

    /// Send a structured card (header + markdown + buttons/list/dropdown).
    async fn send_card(
        &self,
        target: &MessageTarget,
        card: &crate::card::Card,
    ) -> Result<MessageId>;

    /// Delete a message.
    async fn delete_message(&self, target: &MessageTarget, msg_id: &MessageId) -> Result<()>;

    /// Edit an existing message's card content in-place.
    async fn edit_card(
        &self,
        target: &MessageTarget,
        msg_id: &MessageId,
        card: &crate::card::Card,
    ) -> Result<()>;

    /// Send a structured check report card.
    ///
    /// Feishu renders a rich interactive card; Telegram uses formatted text fallback.
    async fn send_check_card(
        &self,
        target: &MessageTarget,
        title: &str,
        items: &[CheckItem],
    ) -> Result<MessageId>;

    /// Send a chat action (typing indicator, etc.).
    ///
    /// Used as a lightweight probe to check if a topic exists — Telegram
    /// returns an error for deleted topics.
    async fn send_chat_action(&self, target: &MessageTarget) -> Result<()>;

    /// Answer a callback query with a brief notification.
    ///
    /// Shows a toast/modal to the user and dismisses the loading state on the
    /// inline keyboard button. The `callback_query_id` comes from the original
    /// `CallbackQuery` event.
    async fn answer_callback(&self, callback_query_id: &str, text: &str) -> Result<()>;

    /// Add an emoji reaction to a message.
    async fn add_reaction(
        &self,
        target: &MessageTarget,
        message_id: &str,
        emoji: &str,
    ) -> Result<()>;

    /// Send a key-value table card.
    async fn send_kv_table(
        &self,
        target: &MessageTarget,
        title: &str,
        rows: &[(String, String)],
    ) -> Result<MessageId>;
}
