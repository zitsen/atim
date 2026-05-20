/// Flood control — per-chat rate limiter with 429 backoff.
///
/// Tracks message frequency per chat and applies delays to stay under
/// Telegram's rate limits. On 429 responses, sets a backoff timer.
/// Status messages are dropped when the queue is deeply backed up.
use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::sync::Mutex;

use async_trait::async_trait;
use atim_core::error::Result;
use atim_core::im::ImAdapter;
use atim_core::message::{Button, ImEvent, MessageId, MessageTarget};
use tokio::sync::mpsc;

/// Maximum messages per chat within the time window.
const MAX_MSG_PER_WINDOW: usize = 15;
/// Sliding window duration.
const WINDOW_SECS: Duration = Duration::from_secs(60);
/// Minimum interval between messages to the same chat.
const MIN_INTERVAL: Duration = Duration::from_millis(200);
/// Maximum total delay before dropping a message.
const MAX_DELAY: Duration = Duration::from_secs(10);

/// Wraps an [`ImAdapter`] with per-chat rate limiting.
pub struct FloodControlledAdapter {
    inner: Arc<dyn ImAdapter>,
    /// Per-chat send timestamps (sliding window for rate calculation).
    timestamps: Mutex<HashMap<i64, Vec<Instant>>>,
    /// Per-chat backoff expiry (set after 429 responses).
    backoffs: Mutex<HashMap<i64, Instant>>,
}

impl FloodControlledAdapter {
    pub fn new(inner: Arc<dyn ImAdapter>) -> Self {
        Self {
            inner,
            timestamps: Mutex::new(HashMap::new()),
            backoffs: Mutex::new(HashMap::new()),
        }
    }

    /// Check if a chat is currently in backoff.
    async fn chat_blocked(&self, chat_id: i64) -> Option<Duration> {
        let backoffs = self.backoffs.lock().await;
        if let Some(until) = backoffs.get(&chat_id) {
            let remaining = until.saturating_duration_since(Instant::now());
            if !remaining.is_zero() {
                return Some(remaining);
            }
        }
        None
    }

    /// Apply rate limiting delay for a chat.
    /// Returns `true` if the message should be dropped (status message + deep backoff).
    async fn rate_limit(&self, chat_id: i64, is_status: bool) -> bool {
        // Check backoff first
        if let Some(remaining) = self.chat_blocked(chat_id).await {
            if is_status && remaining > Duration::from_secs(3) {
                // Drop status messages during long backoff
                return true;
            }
            if remaining > MAX_DELAY {
                // Past the max wait — drop anything
                return true;
            }
            tokio::time::sleep(remaining).await;
            return false;
        }

        // Sliding window check
        let mut timestamps = self.timestamps.lock().await;
        let now = Instant::now();
        let window_start = now.checked_sub(WINDOW_SECS).unwrap_or(now);

        // Remove old entries
        let entries = timestamps.entry(chat_id).or_default();
        entries.retain(|t| *t > window_start);

        if entries.len() >= MAX_MSG_PER_WINDOW {
            // At the limit — delay until some of the window expires
            let oldest = entries[0];
            let wait = oldest
                .checked_add(WINDOW_SECS)
                .unwrap_or(now)
                .saturating_duration_since(now);
            if is_status && wait > Duration::from_secs(3) {
                return true; // drop status
            }
            if wait > MAX_DELAY {
                return true; // drop anything past max delay
            }
            tokio::time::sleep(wait).await;
        }

        // Minimum interval check
        if let Some(last) = entries.last() {
            let since_last = now.saturating_duration_since(*last);
            if since_last < MIN_INTERVAL {
                tokio::time::sleep(MIN_INTERVAL - since_last).await;
            }
        }

        entries.push(Instant::now());
        false
    }

    /// Record a 429 response and set backoff for the chat.
    pub async fn record_backoff(&self, chat_id: i64, retry_after_secs: u64) {
        let retry_after = Duration::from_secs(retry_after_secs.min(30));
        let until = Instant::now() + retry_after;
        tracing::warn!("Rate limited (429) on chat {chat_id}, backing off for {retry_after_secs}s");
        self.backoffs.lock().await.insert(chat_id, until);
    }

    /// Record a message send timestamp (call after successful send).
    pub async fn record_send(&self, chat_id: i64) {
        let mut timestamps = self.timestamps.lock().await;
        let now = Instant::now();
        let window_start = now.checked_sub(WINDOW_SECS).unwrap_or(now);
        let entries = timestamps.entry(chat_id).or_default();
        entries.retain(|t| *t > window_start);
        entries.push(now);
    }

    async fn get_chat_id(target: &MessageTarget) -> i64 {
        target.chat_id.0
    }
}

#[async_trait]
impl ImAdapter for FloodControlledAdapter {
    async fn run(&self, tx: mpsc::UnboundedSender<ImEvent>) -> Result<()> {
        self.inner.run(tx).await
    }

    async fn send_message(&self, target: &MessageTarget, text: &str) -> Result<MessageId> {
        let chat_id = Self::get_chat_id(target).await;
        if self.rate_limit(chat_id, false).await {
            // Should not drop content messages — return a fake MessageId
            // and log a warning
            tracing::warn!("Flood control dropping message to chat {chat_id} (past max delay)");
        }
        let result = self.inner.send_message(target, text).await;
        if result.is_ok() {
            self.record_send(chat_id).await;
        }
        result
    }

    async fn edit_message(
        &self,
        target: &MessageTarget,
        msg_id: &MessageId,
        text: &str,
    ) -> Result<()> {
        let chat_id = Self::get_chat_id(target).await;
        if self.rate_limit(chat_id, false).await {
            return Ok(()); // silently drop edit
        }
        let result = self.inner.edit_message(target, msg_id, text).await;
        if result.is_ok() {
            self.record_send(chat_id).await;
        }
        result
    }

    async fn send_photo(
        &self,
        target: &MessageTarget,
        filename: &str,
        data: &[u8],
    ) -> Result<MessageId> {
        let chat_id = Self::get_chat_id(target).await;
        if self.rate_limit(chat_id, false).await {
            return self
                .inner
                .send_message(target, "[photo dropped by flood control]")
                .await;
        }
        let result = self.inner.send_photo(target, filename, data).await;
        if result.is_ok() {
            self.record_send(chat_id).await;
        }
        result
    }

    async fn send_keyboard(
        &self,
        target: &MessageTarget,
        text: &str,
        buttons: &[Vec<Button>],
    ) -> Result<MessageId> {
        let chat_id = Self::get_chat_id(target).await;
        if self.rate_limit(chat_id, true).await {
            // Drop keyboard — it's interactive UI, safe to drop
            return self.inner.send_message(target, text).await;
        }
        let result = self.inner.send_keyboard(target, text, buttons).await;
        if result.is_ok() {
            self.record_send(chat_id).await;
        }
        result
    }

    async fn delete_message(&self, target: &MessageTarget, msg_id: &MessageId) -> Result<()> {
        // Don't rate-limit deletes — they're lightweight
        self.inner.delete_message(target, msg_id).await
    }

    async fn edit_keyboard(
        &self,
        target: &MessageTarget,
        msg_id: &MessageId,
        buttons: &[Vec<Button>],
    ) -> Result<()> {
        let chat_id = Self::get_chat_id(target).await;
        if self.rate_limit(chat_id, true).await {
            return Ok(());
        }
        let result = self.inner.edit_keyboard(target, msg_id, buttons).await;
        if result.is_ok() {
            self.record_send(chat_id).await;
        }
        result
    }

    async fn send_chat_action(&self, target: &MessageTarget) -> Result<()> {
        // Don't rate-limit chat actions (they're lightweight probes)
        self.inner.send_chat_action(target).await
    }

    async fn answer_callback(&self, callback_query_id: &str, text: &str) -> Result<()> {
        // Don't rate-limit callback answers
        self.inner.answer_callback(callback_query_id, text).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A mock IM adapter for testing rate limiting.
    struct MockAdapter {
        send_count: std::sync::Mutex<usize>,
    }

    #[async_trait]
    impl ImAdapter for MockAdapter {
        async fn run(&self, _tx: mpsc::UnboundedSender<ImEvent>) -> Result<()> {
            Ok(())
        }
        async fn send_message(&self, _target: &MessageTarget, _text: &str) -> Result<MessageId> {
            *self.send_count.lock().unwrap() += 1;
            Ok(MessageId("mock:1".into()))
        }
        async fn edit_message(
            &self,
            _target: &MessageTarget,
            _msg_id: &MessageId,
            _text: &str,
        ) -> Result<()> {
            Ok(())
        }
        async fn send_photo(
            &self,
            _target: &MessageTarget,
            _filename: &str,
            _data: &[u8],
        ) -> Result<MessageId> {
            Ok(MessageId("mock:1".into()))
        }
        async fn send_keyboard(
            &self,
            _target: &MessageTarget,
            _text: &str,
            _buttons: &[Vec<Button>],
        ) -> Result<MessageId> {
            Ok(MessageId("mock:1".into()))
        }
        async fn delete_message(&self, _target: &MessageTarget, _msg_id: &MessageId) -> Result<()> {
            Ok(())
        }
        async fn edit_keyboard(
            &self,
            _target: &MessageTarget,
            _msg_id: &MessageId,
            _buttons: &[Vec<Button>],
        ) -> Result<()> {
            Ok(())
        }
        async fn send_chat_action(&self, _target: &MessageTarget) -> Result<()> {
            Ok(())
        }
        async fn answer_callback(&self, _callback_query_id: &str, _text: &str) -> Result<()> {
            Ok(())
        }
    }

    #[tokio::test]
    async fn test_rate_limit_allows_fast_messages() {
        let inner = Arc::new(MockAdapter {
            send_count: std::sync::Mutex::new(0),
        });
        let controller = FloodControlledAdapter::new(inner.clone());
        let target = MessageTarget {
            chat_id: atim_core::message::ChatId(12345),
            thread_id: None,
        };

        // Send 5 messages quickly — should be allowed
        for _ in 0..5 {
            controller.send_message(&target, "test").await.unwrap();
        }
        assert_eq!(*inner.send_count.lock().unwrap(), 5);
    }

    #[tokio::test]
    async fn test_chat_blocked_check() {
        let inner = Arc::new(MockAdapter {
            send_count: std::sync::Mutex::new(0),
        });
        let controller = FloodControlledAdapter::new(inner);

        // Initially not blocked
        assert!(controller.chat_blocked(99999).await.is_none());

        // Set a backoff
        controller
            .backoffs
            .lock()
            .await
            .insert(99999, Instant::now() + Duration::from_secs(1));
        assert!(controller.chat_blocked(99999).await.is_some());
    }
}
