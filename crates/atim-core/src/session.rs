use serde::{Deserialize, Serialize};

use crate::message::SessionId;

/// Persistent state for a tmux window.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WindowState {
    pub session_id: String,
    pub cwd: String,
    pub window_name: String,
    /// Agent type: "claude", "copilot", "codex", or "" (defaults to "claude" for backward compat).
    #[serde(default)]
    pub agent_type: String,
}

impl Default for WindowState {
    fn default() -> Self {
        Self {
            session_id: String::new(),
            cwd: String::new(),
            window_name: String::new(),
            agent_type: String::new(),
        }
    }
}

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

/// Thread binding: maps a Telegram/Flybook topic to a tmux window.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ThreadBinding {
    pub user_id: i64,
    pub thread_id: i64,
    pub chat_id: i64,
    pub window_id: String,
    pub display_name: String,
    pub group_chat_id: Option<i64>,
    /// The forum topic title (from Telegram forum_topic_created).
    #[serde(default)]
    pub topic_name: Option<String>,
}

/// Full persisted state.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServerState {
    pub window_states: std::collections::HashMap<String, WindowState>,
    pub thread_bindings: Vec<ThreadBinding>,
    pub window_display_names: std::collections::HashMap<String, String>,
    pub user_window_offsets: std::collections::HashMap<String, std::collections::HashMap<String, u64>>, // user_id -> window_id -> offset
}

impl Default for ServerState {
    fn default() -> Self {
        Self {
            window_states: std::collections::HashMap::new(),
            thread_bindings: Vec::new(),
            window_display_names: std::collections::HashMap::new(),
            user_window_offsets: std::collections::HashMap::new(),
        }
    }
}
