/// Flood control — per-chat rate limiter with 429 backoff.
///
/// Tracks message frequency per chat and applies delays to stay under
/// Telegram's rate limits. On 429 responses, sets a backoff timer.
///
/// Only *tool chatter* — [`ImAdapter::send_chatter`] and
/// [`ImAdapter::edit_chatter`] — may be shed when the backlog runs deep.
/// Everything else is paced but never lost: a tool storm can postpone a reply,
/// but it cannot eat one.
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::Mutex;
use tokio::time::Instant;

use async_trait::async_trait;
use atim_core::card::Card;
use atim_core::error::{Error, Result};
use atim_core::im::ImAdapter;
use atim_core::message::{CheckItem, ImEvent, MessageId, MessageTarget};
use tokio::sync::mpsc;

/// Maximum messages per chat within the time window.
const MAX_MSG_PER_WINDOW: usize = 15;
/// Sliding window duration.
const WINDOW_SECS: Duration = Duration::from_secs(60);
/// Minimum interval between messages to the same chat.
const MIN_INTERVAL: Duration = Duration::from_millis(200);
/// Maximum total delay before shedding expendable tool chatter.
const MAX_DELAY: Duration = Duration::from_secs(10);

/// How to handle one outbound message, given how long the limiter wants to wait.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Verdict {
    /// Send it, after waiting out the given pacing delay.
    Send(Duration),
    /// Shed it — expendable content buried behind a deeper backlog.
    Drop,
}

/// Decide between sending and shedding for a required `wait`.
///
/// Tool chatter is shed rather than queued once the wait passes [`MAX_DELAY`].
/// That is what keeps a runaway tool loop — the same `git status` result
/// hundreds of times over — from burying the chat. Essential content is never
/// shed, however long the wait: the invariant `decide(_, false) != Drop` is
/// what makes "a flood cannot eat a reply" hold.
fn decide(wait: Duration, dropable: bool) -> Verdict {
    if dropable && wait > MAX_DELAY {
        Verdict::Drop
    } else {
        Verdict::Send(wait)
    }
}

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
    ///
    /// Returns [`Verdict::Drop`] only for `dropable` (tool chatter) content;
    /// for essential content the verdict is always `Send`. The caller is
    /// responsible for honouring it — see [`ImAdapter::send_message`] versus
    /// [`ImAdapter::send_chatter`].
    async fn rate_limit(&self, chat_id: i64, dropable: bool) -> Verdict {
        // A send during a 429 backoff only earns another 429 and re-arms the
        // timer, so the backoff has to be waited out — not skipped past. (The
        // old code returned early once the remaining backoff exceeded
        // MAX_DELAY, which turned a 30s backoff into a zero-delay retry loop
        // that never let the backoff expire.)
        if let Some(remaining) = self.chat_blocked(chat_id).await {
            match decide(remaining, dropable) {
                Verdict::Drop => return Verdict::Drop,
                Verdict::Send(wait) => tokio::time::sleep(wait).await,
            }
        }

        // Sliding-window pacing: the longer of "until a slot frees up" and
        // "until the minimum interval has elapsed".
        let pacing = {
            let mut timestamps = self.timestamps.lock().await;
            let now = Instant::now();
            let window_start = now.checked_sub(WINDOW_SECS).unwrap_or(now);

            // Remove old entries
            let entries = timestamps.entry(chat_id).or_default();
            entries.retain(|t| *t > window_start);

            // Minimum interval between messages to the same chat.
            let interval_wait = entries
                .last()
                .map(|last| MIN_INTERVAL.saturating_sub(now.saturating_duration_since(*last)))
                .unwrap_or(Duration::ZERO);

            // At the limit — until some of the window expires.
            let window_wait = if entries.len() >= MAX_MSG_PER_WINDOW {
                entries[0]
                    .checked_add(WINDOW_SECS)
                    .unwrap_or(now)
                    .saturating_duration_since(now)
            } else {
                Duration::ZERO
            };

            interval_wait.max(window_wait)
        };

        // Nothing is recorded here — `record_send` owns the ledger, and only
        // counts messages that actually went out.
        let verdict = decide(pacing, dropable);
        if let Verdict::Send(wait) = verdict
            && !wait.is_zero()
        {
            tokio::time::sleep(wait).await;
        }
        verdict
    }

    /// Record a 429 response and set backoff for the chat.
    pub async fn record_backoff(&self, chat_id: i64, retry_after_secs: u64) {
        let retry_after = Duration::from_secs(retry_after_secs.min(30));
        let until = Instant::now() + retry_after;
        tracing::warn!("Rate limited (429) on chat {chat_id}, backing off for {retry_after_secs}s");
        self.backoffs.lock().await.insert(chat_id, until);
    }

    /// Record a message send timestamp (call after successful send).
    ///
    /// Sole owner of the sliding-window ledger — [`rate_limit`] only reads it,
    /// so a paced-but-failed send is not double-counted against the chat.
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
        // Essential — `decide` never sheds this, so the verdict is always `Send`.
        self.rate_limit(chat_id, false).await;
        let result = self.inner.send_message(target, text).await;
        if result.is_ok() {
            self.record_send(chat_id).await;
        }
        result
    }

    async fn send_chatter(&self, target: &MessageTarget, text: &str) -> Result<MessageId> {
        let chat_id = Self::get_chat_id(target).await;
        if self.rate_limit(chat_id, true).await == Verdict::Drop {
            tracing::debug!("Flood control shed tool chatter for chat {chat_id}");
            return Err(Error::Dropped);
        }
        let result = self.inner.send_chatter(target, text).await;
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
        // Essential — this is how a status message becomes the final reply.
        self.rate_limit(chat_id, false).await;
        let result = self.inner.edit_message(target, msg_id, text).await;
        if result.is_ok() {
            self.record_send(chat_id).await;
        }
        result
    }

    async fn edit_chatter(
        &self,
        target: &MessageTarget,
        msg_id: &MessageId,
        text: &str,
    ) -> Result<()> {
        let chat_id = Self::get_chat_id(target).await;
        if self.rate_limit(chat_id, true).await == Verdict::Drop {
            tracing::debug!("Flood control shed tool chatter edit for chat {chat_id}");
            return Err(Error::Dropped);
        }
        let result = self.inner.edit_chatter(target, msg_id, text).await;
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
        // Essential — a screenshot the user explicitly asked for.
        self.rate_limit(chat_id, false).await;
        let result = self.inner.send_photo(target, filename, data).await;
        if result.is_ok() {
            self.record_send(chat_id).await;
        }
        result
    }

    async fn send_card(&self, target: &MessageTarget, card: &Card) -> Result<MessageId> {
        let chat_id = Self::get_chat_id(target).await;
        // Card UI is essential for setup flows (browser, session picker,
        // agent picker) and for AskUserQuestion. Never shed.
        self.rate_limit(chat_id, false).await;
        let result = self.inner.send_card(target, card).await;
        if result.is_ok() {
            self.record_send(chat_id).await;
        }
        result
    }

    async fn delete_message(&self, target: &MessageTarget, msg_id: &MessageId) -> Result<()> {
        // Don't rate-limit deletes — they're lightweight
        self.inner.delete_message(target, msg_id).await
    }

    async fn edit_card(
        &self,
        target: &MessageTarget,
        msg_id: &MessageId,
        card: &Card,
    ) -> Result<()> {
        let chat_id = Self::get_chat_id(target).await;
        // Essential — interactive card state the user is responding to.
        self.rate_limit(chat_id, false).await;
        let result = self.inner.edit_card(target, msg_id, card).await;
        if result.is_ok() {
            self.record_send(chat_id).await;
        }
        result
    }

    async fn send_chat_action(&self, target: &MessageTarget) -> Result<()> {
        // Don't rate-limit chat actions (they're lightweight probes)
        self.inner.send_chat_action(target).await
    }

    async fn send_check_card(
        &self,
        target: &MessageTarget,
        title: &str,
        items: &[CheckItem],
    ) -> Result<MessageId> {
        let chat_id = Self::get_chat_id(target).await;
        // Essential — the report the user asked for via `/check`.
        self.rate_limit(chat_id, false).await;
        let result = self.inner.send_check_card(target, title, items).await;
        if result.is_ok() {
            self.record_send(chat_id).await;
        }
        result
    }

    async fn answer_callback(&self, callback_query_id: &str, text: &str) -> Result<()> {
        // Don't rate-limit callback answers
        self.inner.answer_callback(callback_query_id, text).await
    }

    async fn add_reaction(
        &self,
        target: &MessageTarget,
        message_id: &str,
        emoji: &str,
    ) -> Result<()> {
        // Don't rate-limit reactions
        self.inner.add_reaction(target, message_id, emoji).await
    }

    async fn send_kv_table(
        &self,
        target: &MessageTarget,
        title: &str,
        rows: &[(String, String)],
    ) -> Result<MessageId> {
        self.inner.send_kv_table(target, title, rows).await
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
        async fn send_card(&self, _target: &MessageTarget, _card: &Card) -> Result<MessageId> {
            Ok(MessageId("mock:1".into()))
        }
        async fn delete_message(&self, _target: &MessageTarget, _msg_id: &MessageId) -> Result<()> {
            Ok(())
        }
        async fn edit_card(
            &self,
            _target: &MessageTarget,
            _msg_id: &MessageId,
            _card: &Card,
        ) -> Result<()> {
            Ok(())
        }
        async fn send_chat_action(&self, _target: &MessageTarget) -> Result<()> {
            Ok(())
        }
        async fn send_check_card(
            &self,
            _target: &MessageTarget,
            _title: &str,
            _items: &[CheckItem],
        ) -> Result<MessageId> {
            Ok(MessageId("mock:1".into()))
        }
        async fn answer_callback(&self, _callback_query_id: &str, _text: &str) -> Result<()> {
            Ok(())
        }
        async fn add_reaction(
            &self,
            _target: &MessageTarget,
            _message_id: &str,
            _emoji: &str,
        ) -> Result<()> {
            Ok(())
        }
        async fn send_kv_table(
            &self,
            _target: &MessageTarget,
            _title: &str,
            _rows: &[(String, String)],
        ) -> Result<MessageId> {
            Ok(MessageId("mock:1".into()))
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
            chat_name: None,
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

    /// Target for the shed-vs-send tests below.
    fn target() -> MessageTarget {
        MessageTarget {
            chat_id: atim_core::message::ChatId(7),
            thread_id: None,
            chat_name: None,
        }
    }

    /// A controller over a fresh mock, for one-off send assertions.
    fn controller_with() -> (Arc<MockAdapter>, FloodControlledAdapter) {
        let inner = Arc::new(MockAdapter {
            send_count: std::sync::Mutex::new(0),
        });
        let controller = FloodControlledAdapter::new(inner.clone());
        (inner, controller)
    }

    // ── decide: the shed/send policy in isolation ──

    #[test]
    fn test_essential_content_is_never_shed() {
        // The invariant behind "a flood can postpone a reply but cannot eat
        // one" — no wait is long enough to drop essential content.
        for wait in [
            Duration::ZERO,
            MAX_DELAY,
            Duration::from_secs(60),
            Duration::from_secs(3600),
        ] {
            assert_eq!(decide(wait, false), Verdict::Send(wait));
        }
    }

    #[test]
    fn test_chatter_is_shed_only_past_max_delay() {
        assert_eq!(decide(Duration::ZERO, true), Verdict::Send(Duration::ZERO));
        // Right at the cap it is still worth sending.
        assert_eq!(decide(MAX_DELAY, true), Verdict::Send(MAX_DELAY));
        assert_eq!(
            decide(MAX_DELAY + Duration::from_millis(1), true),
            Verdict::Drop
        );
    }

    // ── 429 backoff ──

    #[tokio::test(start_paused = true)]
    async fn test_chatter_is_shed_behind_a_long_backoff() {
        // A 30s 429 backoff is past MAX_DELAY. Chatter is shed on the spot
        // rather than queueing behind the block.
        let (inner, controller) = controller_with();
        controller.record_backoff(7, 30).await;

        assert!(matches!(
            controller.send_chatter(&target(), "git status").await,
            Err(Error::Dropped)
        ));
        assert!(matches!(
            controller
                .edit_chatter(&target(), &MessageId("mock:1".into()), "…")
                .await,
            Err(Error::Dropped)
        ));
        assert_eq!(*inner.send_count.lock().unwrap(), 0);
    }

    #[tokio::test(start_paused = true)]
    async fn test_reply_survives_the_backoff_that_sheds_chatter() {
        // Same 30s backoff, but this is a reply the user is waiting on: it
        // must go out, just later.
        let (inner, controller) = controller_with();
        controller.record_backoff(7, 30).await;

        controller
            .send_message(&target(), "here is the answer")
            .await
            .unwrap();
        assert_eq!(*inner.send_count.lock().unwrap(), 1);
    }

    #[tokio::test(start_paused = true)]
    async fn test_backoff_is_waited_out_before_sending() {
        // Regression: the limiter used to skip any backoff longer than
        // MAX_DELAY without sleeping, so a 30s backoff turned into a
        // zero-delay retry loop that kept earning 429s and re-arming its own
        // timer — the backoff could never expire.
        let (_inner, controller) = controller_with();
        let start = Instant::now();
        controller.record_backoff(7, 30).await;

        controller.send_message(&target(), "reply").await.unwrap();

        assert!(
            start.elapsed() >= Duration::from_secs(30),
            "sent after {:?}, before the backoff expired",
            start.elapsed()
        );
    }

    // ── sliding window ──

    #[tokio::test(start_paused = true)]
    async fn test_chatter_is_shed_once_the_window_is_full() {
        // Fill the window so the next send would have to wait ~WINDOW_SECS,
        // far past MAX_DELAY.
        let (inner, controller) = controller_with();
        for _ in 0..MAX_MSG_PER_WINDOW {
            controller.record_send(7).await;
        }

        assert!(matches!(
            controller.send_chatter(&target(), "git status").await,
            Err(Error::Dropped)
        ));
        assert_eq!(*inner.send_count.lock().unwrap(), 0);
    }

    #[tokio::test(start_paused = true)]
    async fn test_reply_survives_a_full_window() {
        let (inner, controller) = controller_with();
        for _ in 0..MAX_MSG_PER_WINDOW {
            controller.record_send(7).await;
        }

        controller
            .send_message(&target(), "final answer")
            .await
            .unwrap();
        assert_eq!(*inner.send_count.lock().unwrap(), 1);
    }

    #[tokio::test(start_paused = true)]
    async fn test_chatter_still_goes_out_when_the_chat_is_quiet() {
        // Shedding is a pressure valve, not a filter: with no backlog the
        // tool output the user is following still arrives.
        let (inner, controller) = controller_with();

        controller
            .send_chatter(&target(), "⚙️ Bash: cargo build")
            .await
            .unwrap();
        assert_eq!(*inner.send_count.lock().unwrap(), 1);
    }

    #[tokio::test(start_paused = true)]
    async fn test_shedding_does_not_consume_window_quota() {
        // A shed message never reached the API, so it must not push the chat
        // further into the window — otherwise the drops would throttle the
        // replies they are meant to protect.
        let (inner, controller) = controller_with();
        for _ in 0..MAX_MSG_PER_WINDOW {
            controller.record_send(7).await;
        }
        for _ in 0..5 {
            assert!(matches!(
                controller.send_chatter(&target(), "git status").await,
                Err(Error::Dropped)
            ));
        }

        // Drain the window, then a reply must still get through on the first
        // attempt rather than being paced out behind phantom sends.
        let before = *inner.send_count.lock().unwrap();
        controller
            .send_message(&target(), "final answer")
            .await
            .unwrap();
        assert_eq!(*inner.send_count.lock().unwrap(), before + 1);
    }
}
