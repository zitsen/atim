use serde::{Deserialize, Serialize};

use crate::message::SessionId;

// ── V1 types (kept for migration) ──

/// Persistent state for a tmux window. V1 legacy — kept for DB migration.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct WindowState {
    pub session_id: String,
    pub cwd: String,
    pub window_name: String,
    pub agent_type: String,
}

/// Thread binding: maps a Telegram/Flybook topic to a tmux window. V1 legacy.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ThreadBinding {
    pub user_id: i64,
    pub thread_id: i64,
    pub chat_id: i64,
    pub window_id: String,
    pub display_name: String,
    pub group_chat_id: Option<i64>,
    #[serde(default)]
    pub topic_name: Option<String>,
}

/// Full persisted state. V1 legacy.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ServerState {
    pub window_states: std::collections::HashMap<String, WindowState>,
    pub thread_bindings: Vec<ThreadBinding>,
    pub window_display_names: std::collections::HashMap<String, String>,
    pub user_window_offsets:
        std::collections::HashMap<String, std::collections::HashMap<String, u64>>,
}

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
