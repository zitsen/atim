use super::AgentParser;
use crate::message::{AgentKind, InteractiveUi};

/// Parser for Codex CLI terminal output.
pub struct CodexParser;

impl AgentParser for CodexParser {
    fn detect(_pane_text: &str, process_name: &str) -> AgentKind {
        if process_name.contains("codex") {
            AgentKind::CodexCli
        } else {
            AgentKind::Unknown
        }
    }

    fn parse_status(&self, _pane_text: &str) -> Option<String> {
        // TODO: Codex CLI status patterns
        None
    }

    fn detect_interactive(&self, _pane_text: &str) -> Option<InteractiveUi> {
        // TODO: Codex CLI interactive patterns
        None
    }
}
