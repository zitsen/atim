use std::path::Path;

use crate::error::Result;
use crate::message::AgentKind;

use super::AgentParser;

// ── Output source ──

/// How this agent produces output that Atim reads.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OutputSource {
    /// JSONL log files (Claude Code).
    JsonlFiles,
    /// Tmux pane capture with line-diff forwarding (Copilot CLI, Codex CLI).
    PaneCapture,
}

// ── Identity ──

/// Stable agent identifier — maps to `AgentKind` for backward compat dispatch.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AgentId {
    ClaudeCode,
    CopilotCli,
    CodexCli,
    MimoCode,
}

impl AgentId {
    pub fn name(&self) -> &'static str {
        match self {
            AgentId::ClaudeCode => "claude",
            AgentId::CopilotCli => "copilot",
            AgentId::CodexCli => "codex",
            AgentId::MimoCode => "mimo",
        }
    }

    pub fn kind(&self) -> AgentKind {
        match self {
            AgentId::ClaudeCode => AgentKind::ClaudeCode,
            AgentId::CopilotCli => AgentKind::CopilotCli,
            AgentId::CodexCli => AgentKind::CodexCli,
            AgentId::MimoCode => AgentKind::MimoCode,
        }
    }
}

impl std::str::FromStr for AgentId {
    type Err = ();
    fn from_str(s: &str) -> std::result::Result<Self, Self::Err> {
        match s {
            "claude" => Ok(AgentId::ClaudeCode),
            "copilot" => Ok(AgentId::CopilotCli),
            "codex" => Ok(AgentId::CodexCli),
            "mimo" => Ok(AgentId::MimoCode),
            _ => Err(()),
        }
    }
}

// ── Detected session ──

/// A discovered session (returned by session scanning).
#[derive(Debug, Clone)]
pub struct DetectedSession {
    pub id: String,
    pub project_slug: String,
    pub summary: String,
    pub timestamp: String,
    pub message_count: usize,
}

// ── Agent trait ──

/// Unified agent definition — covers launch, session lifecycle,
/// hook, response reading, and interactive UI detection.
///
/// Each agent variant (Claude Code, Copilot CLI, Codex CLI) implements
/// this trait. The system selects an agent per-window and supports
/// runtime switching via `/switch`.
pub trait Agent: Send + Sync {
    // ── Identity ──

    /// Stable identifier (e.g. "claude", "copilot", "codex").
    fn id(&self) -> AgentId;

    fn name(&self) -> &'static str {
        self.id().name()
    }

    fn kind(&self) -> AgentKind {
        self.id().kind()
    }

    // ── Launch configuration ──

    /// Command to execute in a fresh tmux pane for a new session.
    fn new_session_command(&self) -> String;

    /// Command to resume an existing session, if supported.
    fn resume_command(&self, session_id: &str) -> Option<String>;

    /// Extra CLI args appended on every invocation.
    fn extra_args(&self) -> Vec<String> {
        vec![]
    }

    /// Environment variables that MUST be set for this agent.
    fn required_env(&self) -> Vec<(&str, &str)> {
        vec![]
    }

    // ── Session lifecycle ──

    /// Whether this agent supports tracked sessions (JSONL logs, etc.).
    fn supports_sessions(&self) -> bool {
        false
    }

    /// Whether this agent uses a session-start hook.
    fn has_session_start_hook(&self) -> bool {
        false
    }

    /// Install the session-start hook, if applicable.
    fn install_hook(&self) -> Result<()> {
        Err(crate::error::Error::Unsupported(
            "hook not supported for this agent".into(),
        ))
    }

    /// Given a working directory, find the most recent untracked session_id.
    fn discover_session(
        &self,
        _cwd: &str,
        _known_ids: &std::collections::HashSet<String>,
    ) -> Result<Option<String>> {
        Err(crate::error::Error::Unsupported(
            "session discovery not supported for this agent".into(),
        ))
    }

    /// Trace a pane PID with lsof (or equivalent) to find the active session file.
    fn discover_session_by_pid(&self, _window_id: &str) -> Result<Option<String>> {
        Err(crate::error::Error::Unsupported(
            "session PID discovery not supported for this agent".into(),
        ))
    }

    /// List all available sessions for the resume/session picker.
    fn scan_sessions(&self, _path: &Path) -> Result<Vec<DetectedSession>> {
        Err(crate::error::Error::Unsupported(
            "session scanning not supported for this agent".into(),
        ))
    }

    // ── Response reading ──

    /// How this agent's terminal output is consumed.
    fn output_source(&self) -> OutputSource;

    /// Return the terminal output parser for this agent.
    fn parser(&self) -> Box<dyn AgentParser>;

    // ── Runtime switching ──

    /// Keys to gracefully terminate this agent during `/switch`.
    fn graceful_shutdown_keys(&self) -> Vec<&'static str> {
        vec!["C-c"]
    }
}
