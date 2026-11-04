use atim_core::agent::{self, AgentParser};
use atim_core::message::{AgentKind, InteractiveUi};

/// Parses raw terminal output for status and interactive UI detection.
///
/// Wraps the agent-specific parsers from `atim-core` and auto-detects
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
        let re = regex::Regex::new(
            r"\x1B\[[0-9;]*[a-zA-Z]|\x1B\][0-9;]*[^\x1B]*\x1B\\|[\x00-\x08\x0B\x0C\x0E-\x1F]",
        )
        .unwrap();
        re.replace_all(text, "").to_string()
    }

    /// Whether the agent's input box is empty.
    ///
    /// Claude Code draws its prompt between two full-width horizontal rules at
    /// the bottom of the pane. `Some(true)` means the box holds nothing but the
    /// prompt marker — i.e. the message that was just sent has been submitted.
    /// `Some(false)` means a draft is still sitting there: the trailing Enter
    /// was dropped. `None` means the layout is not recognized (a dialog is
    /// open, the agent is between renders, or this is not a Claude-style TUI),
    /// and the caller must not act on it.
    pub fn input_area_is_empty(pane_text: &str) -> Option<bool> {
        let clean = Self::strip_ansi(pane_text);
        let lines: Vec<&str> = clean.lines().collect();

        let is_rule = |l: &&str| {
            let t = l.trim();
            t.chars().count() >= 10 && t.chars().all(|c| matches!(c, '─' | '━' | '═'))
        };
        let rules: Vec<usize> = lines
            .iter()
            .enumerate()
            .filter(|(_, l)| is_rule(l))
            .map(|(i, _)| i)
            .collect();
        if rules.len() < 2 {
            return None;
        }
        let (start, end) = (rules[rules.len() - 2], rules[rules.len() - 1]);

        // The prompt marker is `❯` (U+276F) in current builds, `>` in older
        // ones; older layouts may also prefix the box with `│`.
        let mut draft = String::new();
        for line in &lines[start + 1..end] {
            draft.push_str(line);
        }
        let content = draft
            .trim_start()
            .trim_start_matches(['❯', '>', '│'])
            .trim_matches(|c: char| c.is_whitespace() || c == '\u{a0}');
        Some(content.is_empty())
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

    #[test]
    fn test_input_area_empty_when_submitted() {
        let pane = "\
✻ Worked for 1m 56s · done 10:57 AM

──────────────────────────────────────────────────────────────
❯\u{a0}
──────────────────────────────────────────────────────────────
  huolinhe@host:/home/huolinhe/code/workspace
  ⏵⏵ bypass permissions on (shift+tab to cycle)";
        assert_eq!(TerminalParser::input_area_is_empty(pane), Some(true));
    }

    #[test]
    fn test_input_area_detects_unsubmitted_draft() {
        let pane = "\
✻ Worked for 1m 56s · done 10:57 AM

──────────────────────────────────────────────────────────────
❯ https://project.feishu.cn/taosdata_td/feature/detail/7117102738 继续review
──────────────────────────────────────────────────────────────
  huolinhe@host:/home/huolinhe/code/workspace";
        assert_eq!(TerminalParser::input_area_is_empty(pane), Some(false));
    }

    #[test]
    fn test_input_area_none_without_rules() {
        // A dialog or a non-Claude TUI: the caller must not act on a guess.
        let pane = "Do you want to proceed?\n❯ 1. Yes\n  2. No";
        assert_eq!(TerminalParser::input_area_is_empty(pane), None);
    }

    #[test]
    fn test_input_area_ignores_ansi_and_multiline_drafts() {
        let pane = "\
──────────────────────────────────────────────────────────────
\x1b[36m❯ line one\x1b[0m
line two
──────────────────────────────────────────────────────────────
";
        assert_eq!(TerminalParser::input_area_is_empty(pane), Some(false));
    }
}
