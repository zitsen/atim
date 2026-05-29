use serde::{Deserialize, Serialize};

use crate::message::SessionId;

// ── V2 types — session-driven design ──

/// A known session identified by its stable session_id (UUID).
/// session_id is the real identity — window_id (@id) is ephemeral.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SessionInfo {
    pub session_id: String,
    pub cwd: String,
    pub agent_type: String,
}

/// Current tmux window to session mapping. Transient — rebuilt on restart.
/// window_id is just a transient property, never a persistent identity.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct WindowBinding {
    pub window_id: String,
    pub session_id: String,
    pub cwd: String,
    pub agent_type: String,
    pub window_name: String,
}

/// A stable chat-to-session binding. References session_id directly,
/// not window_id — so message routing doesn't depend on ephemeral @id.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChatBinding {
    pub user_id: i64,
    pub thread_id: i64,
    pub chat_id: i64,
    pub display_name: String,
    pub group_chat_id: Option<i64>,
    pub topic_name: Option<String>,
    pub session_id: String,
}

/// Server runtime state — all in-memory mappings derived from the DB.
#[derive(Debug, Clone, Default)]
pub struct RuntimeState {
    /// Known sessions keyed by session_id.
    pub sessions: std::collections::HashMap<String, SessionInfo>,
    /// Active tmux window bindings keyed by window_id.
    pub window_bindings: std::collections::HashMap<String, WindowBinding>,
    /// Stable chat bindings (ordered, most recent last).
    pub chat_bindings: Vec<ChatBinding>,
}

impl RuntimeState {
    /// Resolve WindowBinding for the given (user_id, thread_id) via ChatBinding → session_id.
    pub fn resolve_window_binding(&self, user_id: i64, thread_id: i64) -> Option<&WindowBinding> {
        let cb = self
            .chat_bindings
            .iter()
            .find(|b| b.user_id == user_id && b.thread_id == thread_id)?;
        if cb.session_id.is_empty() {
            return None;
        }
        self.window_bindings
            .values()
            .find(|wb| wb.session_id == cb.session_id)
    }

    /// Resolve window_id string for the given (user_id, thread_id).
    pub fn resolve_window_id(&self, user_id: i64, thread_id: i64) -> Option<&str> {
        self.resolve_window_binding(user_id, thread_id)
            .map(|wb| wb.window_id.as_str())
    }

    /// Iterate all (ChatBinding, WindowBinding) pairs joined by session_id.
    /// Skips bindings with empty session_id or no matching window.
    pub fn resolved_bindings(&self) -> Vec<(&ChatBinding, &WindowBinding)> {
        self.chat_bindings
            .iter()
            .filter_map(|cb| {
                if cb.session_id.is_empty() {
                    None
                } else {
                    self.window_bindings
                        .values()
                        .find(|wb| wb.session_id == cb.session_id)
                        .map(|wb| (cb, wb))
                }
            })
            .collect()
    }

    /// Find a ChatBinding matching (user_id, thread_id), then look up its window_id.
    /// Returns (ChatBinding, window_id) or None.
    pub fn chat_binding_with_window(
        &self,
        user_id: i64,
        thread_id: i64,
    ) -> Option<(&ChatBinding, &str)> {
        let cb = self
            .chat_bindings
            .iter()
            .find(|b| b.user_id == user_id && b.thread_id == thread_id)?;
        let wid = self.resolve_window_id(user_id, thread_id)?;
        Some((cb, wid))
    }
}

// ── Monitor / agent types (unchanged) ──

/// Information about a Claude/Copilot/Codex session.
#[derive(Debug, Clone)]
pub struct AgentSession {
    pub session_id: SessionId,
    pub summary: String,
    pub message_count: usize,
    pub file_path: String,
    pub agent_kind: super::message::AgentKind,
}

/// Tracked session for the monitor (byte offset tracking).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TrackedSession {
    pub session_id: String,
    pub file_path: String,
    pub last_byte_offset: u64,
}

/// State for user window offset tracking.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UserWindowOffset {
    pub user_id: i64,
    pub window_id: String,
    pub offset: u64,
}
