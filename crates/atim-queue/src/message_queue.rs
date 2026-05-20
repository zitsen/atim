use std::collections::VecDeque;

use atim_core::message::{MessageId, MessageTarget};

/// Maximum Telegram message length.
const MAX_MSG_LEN: usize = 3800;
/// Maximum characters to keep from previous content when editing.
const MERGE_CONTEXT: usize = 500;

/// A message in the queue waiting to be sent or edited.
#[derive(Debug, Clone)]
pub struct QueueMessage {
    pub target: MessageTarget,
    pub text: String,
    pub is_status: bool, // true = intermittent status, false = final content
    pub sent_msg_id: Option<MessageId>,
}

/// Per-user message queue with content merging.
///
/// Telegram has a 3800-character message limit and edits status messages
/// in-place. This queue manages the flow:
/// - Status messages overwrite each other (only the latest is shown)
/// - Tool results are appended to the pending content message
/// - When a content message gets too long, it's sent and a new one starts
pub struct MessageQueue {
    /// Pending messages waiting to be sent.
    pending: VecDeque<QueueMessage>,
    /// The last status message sent (for in-place editing).
    last_status: Option<(MessageTarget, MessageId)>,
    /// Pending content being accumulated before sending.
    pending_content: Option<PendingContent>,
}

/// Accumulated but not-yet-sent content.
#[derive(Debug, Clone)]
struct PendingContent {
    target: MessageTarget,
    text: String,
}

impl Default for MessageQueue {
    fn default() -> Self {
        Self::new()
    }
}

impl MessageQueue {
    pub fn new() -> Self {
        Self {
            pending: VecDeque::new(),
            last_status: None,
            pending_content: None,
        }
    }

    /// Enqueue a status update.
    ///
    /// If a status message was already sent to this target, the new status
    /// will edit it in-place. Otherwise, a new message is sent.
    pub fn enqueue_status(&mut self, target: MessageTarget, text: String) {
        // Check if we already have a pending status for this target — replace it
        if let Some(existing) = self
            .pending
            .iter_mut()
            .find(|m| m.is_status && m.target == target)
        {
            existing.text = text;
            return;
        }

        self.pending.push_back(QueueMessage {
            target,
            text,
            is_status: true,
            sent_msg_id: None,
        });
    }

    /// Enqueue content (tool result, assistant text) for delivery.
    ///
    /// Multiple content fragments for the same target are merged into a single
    /// message, with old content trimmed to stay under the length limit.
    pub fn enqueue_content(&mut self, target: MessageTarget, text: String) {
        // Merge with existing pending content for the same target
        if let Some(ref mut pc) = self.pending_content
            && pc.target == target {
                let merged = Self::merge_content(&pc.text, &text);
                if merged.len() <= MAX_MSG_LEN {
                    pc.text = merged;
                    return;
                }
                // Merged result would be too long — flush existing as final,
                // start a new segment
                self.flush_pending_content();
            }

        self.pending_content = Some(PendingContent { target, text });
    }

    /// Flush any accumulated pending content into the pending queue.
    fn flush_pending_content(&mut self) {
        if let Some(pc) = self.pending_content.take() {
            self.pending.push_back(QueueMessage {
                target: pc.target,
                text: pc.text,
                is_status: false,
                sent_msg_id: None,
            });
        }
    }

    /// Get the next message to send (or None if the queue is empty).
    ///
    /// Flushes pending content first.
    pub fn dequeue(&mut self) -> Option<QueueMessage> {
        self.flush_pending_content();
        self.pending.pop_front()
    }

    /// Mark a message as sent, recording its message ID.
    ///
    /// For status messages, this allows future status updates to edit in-place.
    pub fn mark_sent(&mut self, msg: &QueueMessage, msg_id: MessageId) {
        if msg.is_status {
            self.last_status = Some((msg.target.clone(), msg_id));
        }
    }

    /// Get the last sent status message ID for a target, if any.
    pub fn last_status_id(&self, target: &MessageTarget) -> Option<MessageId> {
        self.last_status
            .as_ref()
            .filter(|(t, _)| t == target)
            .map(|(_, id)| id.clone())
    }

    /// Check if the queue has pending items (including pending content).
    pub fn has_pending(&self) -> bool {
        self.pending_content.is_some() || !self.pending.is_empty()
    }

    /// Merge two content fragments: keep the end of the old content plus the new text.
    fn merge_content(old: &str, new: &str) -> String {
        if old.is_empty() {
            return new.to_string();
        }
        let suffix = if old.len() > MERGE_CONTEXT {
            &old[old.len() - MERGE_CONTEXT..]
        } else {
            old
        };

        // Check if the suffix already ends with new's prefix (continuation)
        if new.starts_with(suffix) {
            new.to_string()
        } else {
            format!("…{}\n{}", suffix, new)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use atim_core::message::{ChatId, ThreadId};

    fn test_target() -> MessageTarget {
        MessageTarget {
            chat_id: ChatId(-123),
            thread_id: Some(ThreadId(456)),
        }
    }

    #[test]
    fn test_status_replaces_previous() {
        let mut q = MessageQueue::new();
        let t = test_target();
        q.enqueue_status(t.clone(), "loading...".into());
        q.enqueue_status(t.clone(), "done!".into());
        let msg = q.dequeue().unwrap();
        assert_eq!(msg.text, "done!");
    }

    #[test]
    fn test_content_merge() {
        let mut q = MessageQueue::new();
        let t = test_target();
        q.enqueue_content(t.clone(), "Hello ".into());
        q.enqueue_content(t.clone(), "World".into());
        let msg = q.dequeue().unwrap();
        assert_eq!(msg.text, "…Hello \nWorld");
    }
}
