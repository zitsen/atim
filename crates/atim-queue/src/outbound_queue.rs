/// Bounded outbound queue between the event loop and the delivery task.
///
/// A tool storm can produce hundreds of session-output batches a minute. The
/// queue is bounded so that cannot become unbounded memory growth, but
/// overflowing must not cost the user a reply: eviction prefers the oldest
/// *tool* batch, and a batch carrying assistant text is the last thing to go.
use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};

use atim_core::message::{ContentType, NewMessage};
use tokio::sync::{Mutex, Notify};

/// FIFO of session-output batches with tool-first eviction on overflow.
pub struct OutboundQueue {
    inner: Mutex<VecDeque<Vec<NewMessage>>>,
    /// Wakes the consumer when a batch lands or the queue closes.
    notify: Notify,
    closed: AtomicBool,
    cap: usize,
}

impl OutboundQueue {
    /// Create a queue holding at most `cap` batches.
    pub fn new(cap: usize) -> Self {
        Self {
            inner: Mutex::new(VecDeque::new()),
            notify: Notify::new(),
            closed: AtomicBool::new(false),
            cap: cap.max(1),
        }
    }

    /// Enqueue a batch without blocking, shedding as needed to stay bounded.
    ///
    /// In-memory only — safe to call from the event loop. A tool-only batch
    /// arriving to a full queue is shed outright. A batch carrying assistant
    /// text evicts the oldest tool batch to make room; only a queue full of
    /// nothing but replies drops a reply.
    pub async fn push(&self, batch: Vec<NewMessage>) {
        {
            let mut q = self.inner.lock().await;
            if q.len() >= self.cap {
                if is_tool_only(&batch) {
                    drop(q);
                    tracing::warn!("outbound queue full — shed a tool-only batch");
                    return;
                }
                match q.iter().position(|b| is_tool_only(b)) {
                    Some(pos) => {
                        q.remove(pos);
                    }
                    None => {
                        q.pop_front();
                        tracing::error!(
                            "outbound queue full of replies — dropped the oldest batch"
                        );
                    }
                }
            }
            q.push_back(batch);
        }
        self.notify.notify_one();
    }

    /// Take the oldest batch, waiting for one to arrive.
    ///
    /// Returns `None` once the queue is closed and drained.
    pub async fn pop(&self) -> Option<Vec<NewMessage>> {
        loop {
            {
                let mut q = self.inner.lock().await;
                if let Some(batch) = q.pop_front() {
                    return Some(batch);
                }
                if self.closed.load(Ordering::SeqCst) {
                    return None;
                }
            }
            // Lock is released before waiting, so `push`/`close` cannot block
            // on a parked consumer. `notify_one` stores a permit when nobody is
            // waiting yet, so a wake-up cannot be lost in that window.
            self.notify.notified().await;
        }
    }

    /// Mark the queue closed and wake the consumer so it can drain and exit.
    pub fn close(&self) {
        self.closed.store(true, Ordering::SeqCst);
        self.notify.notify_one();
    }
}

/// Whether every entry in a batch is tool output — the safe thing to shed.
///
/// Assistant text is what the user is actually waiting for, so a batch
/// containing any is never considered pure chatter.
fn is_tool_only(batch: &[NewMessage]) -> bool {
    batch.iter().all(|m| m.content_type != ContentType::Text)
}

#[cfg(test)]
mod tests {
    use super::*;
    use atim_core::message::SessionId;
    use std::sync::Arc;

    fn msg(content_type: ContentType, text: &str) -> NewMessage {
        NewMessage {
            session_id: SessionId("sid".to_string()),
            text: text.to_string(),
            is_complete: true,
            content_type,
            tool_use_id: None,
            role: "assistant".to_string(),
            tool_name: None,
            image_data: None,
            raw_input: None,
        }
    }

    fn tool(text: &str) -> NewMessage {
        msg(ContentType::ToolUse, text)
    }

    fn reply(text: &str) -> NewMessage {
        msg(ContentType::Text, text)
    }

    /// Text of every message in one batch, in order.
    fn batch_texts(batch: &[NewMessage]) -> Vec<String> {
        batch.iter().map(|m| m.text.clone()).collect()
    }

    #[tokio::test]
    async fn test_batches_pass_through_in_order() {
        let q = OutboundQueue::new(4);
        q.push(vec![tool("a")]).await;
        q.push(vec![reply("b")]).await;

        assert_eq!(batch_texts(&q.pop().await.unwrap()), vec!["a"]);
        assert_eq!(batch_texts(&q.pop().await.unwrap()), vec!["b"]);
    }

    #[tokio::test]
    async fn test_tool_only_batch_is_shed_when_full() {
        let q = OutboundQueue::new(2);
        q.push(vec![tool("keep-1")]).await;
        q.push(vec![tool("keep-2")]).await;
        q.push(vec![tool("shed-me")]).await;
        q.close();

        assert_eq!(batch_texts(&q.pop().await.unwrap()), vec!["keep-1"]);
        assert_eq!(batch_texts(&q.pop().await.unwrap()), vec!["keep-2"]);
        assert!(
            q.pop().await.is_none(),
            "the overflowing tool batch should have been shed"
        );
    }

    #[tokio::test]
    async fn test_reply_evicts_the_oldest_tool_batch() {
        // Full of tool output when a reply lands: the reply gets in by shedding
        // tool chatter, not by losing the reply.
        let q = OutboundQueue::new(2);
        q.push(vec![tool("old-tool")]).await;
        q.push(vec![tool("newer-tool")]).await;
        q.push(vec![reply("the answer")]).await;

        assert_eq!(batch_texts(&q.pop().await.unwrap()), vec!["newer-tool"]);
        assert_eq!(batch_texts(&q.pop().await.unwrap()), vec!["the answer"]);
    }

    #[tokio::test]
    async fn test_oldest_reply_goes_when_nothing_but_replies_remain() {
        // Nothing but replies left to shed — the oldest goes (logged as an
        // error). The newest is what the user is waiting on and it stays.
        let q = OutboundQueue::new(2);
        q.push(vec![reply("old answer")]).await;
        q.push(vec![reply("mid answer")]).await;
        q.push(vec![reply("latest answer")]).await;

        assert_eq!(batch_texts(&q.pop().await.unwrap()), vec!["mid answer"]);
        assert_eq!(batch_texts(&q.pop().await.unwrap()), vec!["latest answer"]);
    }

    #[tokio::test]
    async fn test_mixed_batch_counts_as_a_reply() {
        // A batch with both tool output and text is not pure chatter, so it
        // earns eviction of tool output rather than being shed itself.
        let q = OutboundQueue::new(1);
        q.push(vec![tool("tool-1")]).await;
        q.push(vec![tool("tool-2"), reply("answer")]).await;

        assert_eq!(
            batch_texts(&q.pop().await.unwrap()),
            vec!["tool-2", "answer"]
        );
    }

    #[test]
    fn test_is_tool_only_matches_the_shed_rule() {
        assert!(is_tool_only(&[]));
        assert!(is_tool_only(&[
            tool("a"),
            msg(ContentType::ToolResult, "b")
        ]));
        assert!(!is_tool_only(&[tool("a"), reply("b")]));
    }

    #[tokio::test]
    async fn test_close_drains_then_ends_the_consumer() {
        let q = OutboundQueue::new(4);
        q.push(vec![tool("pending")]).await;
        q.close();

        assert_eq!(batch_texts(&q.pop().await.unwrap()), vec!["pending"]);
        assert!(q.pop().await.is_none());
    }

    #[tokio::test]
    async fn test_close_wakes_a_waiting_consumer() {
        // Regression: `close` must reach a consumer parked in `pop`. Using
        // `notify_one` (which stores a permit when nobody is waiting yet) rather
        // than `notify_waiters` is what makes this hold across the
        // check-then-wait race.
        let q = Arc::new(OutboundQueue::new(4));
        let consumer = tokio::spawn({
            let q = Arc::clone(&q);
            async move { q.pop().await }
        });

        // Give the consumer a chance to park before closing.
        tokio::task::yield_now().await;
        q.close();

        assert!(consumer.await.unwrap().is_none());
    }
}
