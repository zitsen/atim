use async_trait::async_trait;
use tokio::sync::mpsc;

use crate::error::Result;
use crate::message::{Button, CheckItem, ImEvent, MessageId, MessageTarget};

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
    async fn send_message(&self, target: &MessageTarget, text: &str) -> Result<MessageId>;

    /// Edit an existing message in-place.
    async fn edit_message(
        &self,
        target: &MessageTarget,
        msg_id: &MessageId,
        text: &str,
    ) -> Result<()>;

    /// Send a photo/document to a chat/thread.
    async fn send_photo(
        &self,
        target: &MessageTarget,
        filename: &str,
        data: &[u8],
    ) -> Result<MessageId>;

    /// Send an inline keyboard markup.
    async fn send_keyboard(
        &self,
        target: &MessageTarget,
        text: &str,
        buttons: &[Vec<Button>],
    ) -> Result<MessageId>;

    /// Delete a message.
    async fn delete_message(&self, target: &MessageTarget, msg_id: &MessageId) -> Result<()>;

    /// Edit an existing message's keyboard markup (for interactive UIs).
    async fn edit_keyboard(
        &self,
        target: &MessageTarget,
        msg_id: &MessageId,
        buttons: &[Vec<Button>],
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
}
