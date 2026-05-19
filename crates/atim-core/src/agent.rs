use crate::message::{AgentKind, InteractiveUi};

/// Agent-specific output format detection and parsing.
///
/// Each agent (Claude Code, Copilot CLI, Codex CLI) has slightly different
/// terminal output formats. Implementations of this trait normalize them
/// into Aim's unified types.
pub trait AgentParser: Send + Sync {
    /// Detect which agent is running based on pane text and process name.
    fn detect(pane_text: &str, process_name: &str) -> AgentKind
    where
        Self: Sized;

    /// Parse the status line (spinner + working text) from terminal output.
    ///
    /// Returns `None` if no status line is visible.
    fn parse_status(&self, pane_text: &str) -> Option<String>;

    /// Detect and extract an interactive UI from terminal output.
    fn detect_interactive(&self, pane_text: &str) -> Option<InteractiveUi>;
}

// ── Built-in parsers ──

pub mod claude;
pub mod copilot;
pub mod codex;

/// Select the appropriate parser for a given agent kind.
pub fn parser_for(kind: AgentKind) -> Box<dyn AgentParser> {
    match kind {
        AgentKind::ClaudeCode => Box::new(claude::ClaudeParser),
        AgentKind::CopilotCli => Box::new(copilot::CopilotParser),
        AgentKind::CodexCli => Box::new(codex::CodexParser),
        AgentKind::Unknown => Box::new(claude::ClaudeParser), // default
    }
}
