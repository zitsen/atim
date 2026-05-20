use serde::{Deserialize, Serialize};

// ── Identifiers ──

/// Telegram/Flybook user ID.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct UserId(pub i64);

/// Chat/group ID (negative for supergroups).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ChatId(pub i64);

/// Message identifier within a chat.
///
/// Telegram uses numeric IDs; Feishu uses string IDs.
/// `String` accommodates both.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct MessageId(pub String);

/// Tmux window identifier (e.g. "@0", "@12").
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct WindowId(pub String);

/// Claude Code session identifier (UUID).
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct SessionId(pub String);

/// Topic/thread identifier within a forum chat.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ThreadId(pub i64);

// ── Targets ──

/// Where to send a message: chat + optional thread.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct MessageTarget {
    pub chat_id: ChatId,
    pub thread_id: Option<ThreadId>,
}

// ── Inbound events from IM ──

/// An event received from an IM platform.
#[derive(Debug, Clone)]
pub struct ImEvent {
    pub user_id: UserId,
    pub target: MessageTarget,
    pub kind: ImEventKind,
}

#[derive(Debug, Clone)]
pub enum ImEventKind {
    /// Plain text message.
    Text {
        text: String,
        /// True if the user explicitly @-mentioned the bot (relevant for group chats).
        is_mention: bool,
        /// True if the message was sent in a group chat (vs P2P).
        is_group: bool,
    },
    /// Photo with optional caption.
    Photo {
        caption: Option<String>,
        data: Vec<u8>,
        mime_type: String,
    },
    /// Voice message (OGG OPUS).
    Voice(Vec<u8>),
    /// Inline keyboard button press.
    CallbackQuery {
        data: String,
        msg_id: MessageId,
        /// The IM platform's internal callback query ID (for answerCallbackQuery).
        callback_query_id: Option<String>,
    },
    /// Forum topic was created.
    TopicCreated { name: String },
    /// Forum topic was closed/deleted.
    TopicClosed,
    /// Forum topic was renamed (topic title changed).
    TopicEdited { new_name: String },
}

impl ImEventKind {
    pub fn variant_name(&self) -> &'static str {
        match self {
            ImEventKind::Text { .. } => "Text",
            ImEventKind::Photo { .. } => "Photo",
            ImEventKind::Voice(..) => "Voice",
            ImEventKind::CallbackQuery { .. } => "CallbackQuery",
            ImEventKind::TopicCreated { .. } => "TopicCreated",
            ImEventKind::TopicClosed => "TopicClosed",
            ImEventKind::TopicEdited { .. } => "TopicEdited",
        }
    }
}

// ── IM UI widgets ──

/// A button in an inline keyboard.
#[derive(Debug, Clone)]
pub struct Button {
    pub text: String,
    pub callback_data: String,
}

// ── Agent / Session types ──

/// Type of AI coding agent running in a pane.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AgentKind {
    ClaudeCode,
    CopilotCli,
    CodexCli,
    Unknown,
}

impl AgentKind {
    pub fn name(&self) -> &'static str {
        match self {
            AgentKind::ClaudeCode => "claude",
            AgentKind::CopilotCli => "copilot",
            AgentKind::CodexCli => "codex",
            AgentKind::Unknown => "unknown",
        }
    }
}

// ── Interactive UI types ──

/// An interactive UI detected in the terminal.
#[derive(Debug, Clone)]
pub struct InteractiveUi {
    pub kind: UiKind,
    pub content: String,
}

/// The kind of interactive UI.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UiKind {
    AskUserQuestion,
    ExitPlanMode,
    PermissionPrompt,
    BashApproval,
    RestoreCheckpoint,
    Settings,
    Unknown,
}

// ── Message types for agent output ──

/// A parsed entry from the agent's session log.
#[derive(Debug, Clone)]
pub struct ParsedEntry {
    pub role: String, // "user" | "assistant"
    pub text: String,
    pub content_type: ContentType,
    pub tool_use_id: Option<String>,
    pub tool_name: Option<String>,
    pub timestamp: Option<String>,
    pub image_data: Option<Vec<(String, Vec<u8>)>>, // (media_type, raw_bytes)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ContentType {
    Text,
    Thinking,
    ToolUse,
    ToolResult,
    LocalCommand,
}

impl std::fmt::Display for ContentType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ContentType::Text => write!(f, "text"),
            ContentType::Thinking => write!(f, "thinking"),
            ContentType::ToolUse => write!(f, "tool_use"),
            ContentType::ToolResult => write!(f, "tool_result"),
            ContentType::LocalCommand => write!(f, "local_command"),
        }
    }
}

// ── New message from monitor ──

/// A new message detected by the session monitor.
#[derive(Debug, Clone)]
pub struct NewMessage {
    pub session_id: SessionId,
    pub text: String,
    pub is_complete: bool,
    pub content_type: ContentType,
    pub tool_use_id: Option<String>,
    pub role: String,
    pub tool_name: Option<String>,
    pub image_data: Option<Vec<(String, Vec<u8>)>>,
}
