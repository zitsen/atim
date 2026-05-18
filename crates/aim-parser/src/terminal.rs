use aim_core::agent::{self, AgentParser};
use aim_core::message::{AgentKind, InteractiveUi};

/// Parses raw terminal output for status and interactive UI detection.
///
/// Wraps the agent-specific parsers from `aim-core` and auto-detects
/// which agent parser to use based on the output content.
pub struct TerminalParser;

impl TerminalParser {
    /// Detect which agent kind is running based on pane text and process name.
    pub fn detect_agent(pane_text: &str, process_name: &str) -> AgentKind {
        // Try each agent's detect() in order
        if agent::claude::ClaudeParser::detect(pane_text, process_name) == AgentKind::ClaudeCode {
            return AgentKind::ClaudeCode;
        }
        if agent::copilot::CopilotParser::detect(pane_text, process_name) == AgentKind::CopilotCli {
            return AgentKind::CopilotCli;
        }
        if agent::codex::CodexParser::detect(pane_text, process_name) == AgentKind::CodexCli {
            return AgentKind::CodexCli;
        }
        AgentKind::Unknown
    }

    /// Parse the status line (spinner + working text) from terminal output.
    pub fn parse_status(pane_text: &str, agent_kind: AgentKind) -> Option<String> {
        let parser = agent::parser_for(agent_kind);
        parser.parse_status(pane_text)
    }

    /// Detect and extract an interactive UI from terminal output.
    pub fn detect_interactive(pane_text: &str, agent_kind: AgentKind) -> Option<InteractiveUi> {
        let parser = agent::parser_for(agent_kind);
        parser.detect_interactive(pane_text)
    }

    /// Strip ANSI escape codes from terminal text.
    pub fn strip_ansi(text: &str) -> String {
        let re = regex::Regex::new(r"\x1B\[[0-9;]*[a-zA-Z]|\x1B\][0-9;]*[^\x1B]*\x1B\\|[\x00-\x08\x0B\x0C\x0E-\x1F]").unwrap();
        re.replace_all(text, "").to_string()
    }

    /// Strip leading blank lines and trailing tmux status bar.
    ///
    /// Tmux appends its status bar line at the bottom. We strip it by
    /// looking for the last line that looks like a status bar (contains
    /// "[0] " or similar patterns).
    pub fn strip_pane_chrome(text: &str) -> String {
        let text = text.trim_start();

        // Find the last line — if it looks like a tmux status bar, remove it
        if let Some(last_newline) = text.rfind('\n') {
            let last_line = text[last_newline + 1..].trim();
            // Tmux status bars often match patterns like "[0] 0:bash*"
            if last_line.len() < 80 && last_line.contains('[') && last_line.contains(']') {
                return text[..last_newline].trim_end().to_string();
            }
        }

        text.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_strip_ansi() {
        let with_ansi = "\x1B[32mgreen\x1B[0m";
        assert_eq!(TerminalParser::strip_ansi(with_ansi), "green");
    }

    #[test]
    fn test_strip_pane_chrome() {
        let text = "Hello\nWorld\n[0] 0:bash*";
        assert_eq!(TerminalParser::strip_pane_chrome(text), "Hello\nWorld");
    }
}
