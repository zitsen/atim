use super::AgentParser;
use crate::message::{AgentKind, InteractiveUi};

/// Parser for Copilot CLI terminal output.
pub struct CopilotParser;

impl AgentParser for CopilotParser {
    fn detect(_pane_text: &str, process_name: &str) -> AgentKind {
        if process_name.contains("copilot") {
            AgentKind::CopilotCli
        } else {
            AgentKind::Unknown
        }
    }

    fn parse_status(&self, _pane_text: &str) -> Option<String> {
        // TODO: Copilot CLI status line patterns
        None
    }

    fn detect_interactive(&self, _pane_text: &str) -> Option<InteractiveUi> {
        // TODO: Copilot CLI interactive prompt patterns
        None
    }
}
