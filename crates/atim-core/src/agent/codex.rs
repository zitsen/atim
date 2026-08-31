use super::AgentParser;
use super::trait_def::{Agent, AgentId, OutputSource};
use crate::message::{AgentKind, InteractiveUi, UiKind};

/// Codex CLI spinner characters (common TUI spinner set).
const STATUS_SPINNERS: &[char] = &[
    '⠋', '⠙', '⠹', '⠸', '⠼', '⠴', '⠦', '⠧', '⠇', '⠏', '◐', '◓', '◑', '◒',
];

/// Patterns for Codex CLI interactive UIs.
struct UiPatternDef {
    kind: UiKind,
    /// Line must contain ALL of these strings (AND match).
    contains: &'static [&'static str],
}

const UI_PATTERNS: &[UiPatternDef] = &[
    // ── Permission/approval prompts ──
    UiPatternDef {
        kind: UiKind::PermissionPrompt,
        contains: &["Allow", "to run"],
    },
    UiPatternDef {
        kind: UiKind::PermissionPrompt,
        contains: &["Proceed?", "[y/N]"],
    },
    UiPatternDef {
        kind: UiKind::PermissionPrompt,
        contains: &["Are you sure", "[y/N]"],
    },
    // ── Selection menus ──
    UiPatternDef {
        kind: UiKind::AskUserQuestion,
        contains: &["Use ↑/↓ to"],
    },
    UiPatternDef {
        kind: UiKind::AskUserQuestion,
        contains: &["❯"],
    },
    // ── File picker / multi-select ──
    UiPatternDef {
        kind: UiKind::AskUserQuestion,
        contains: &["[space]", "to select"],
    },
];

/// Parser for Codex CLI terminal output.
pub struct CodexParser;

impl AgentParser for CodexParser {
    fn detect(pane_text: &str, process_name: &str) -> AgentKind {
        // Check process name first (most reliable)
        if process_name.contains("codex") {
            return AgentKind::CodexCli;
        }
        // Check pane text for Codex CLI markers
        if pane_text.contains("Codex") || pane_text.contains("codex") {
            // Avoid false positives: "Codex" alone is not enough — verify
            // with additional context (e.g. TUI elements)
            if pane_text.contains("codex")
                || pane_text.contains("openai")
                || pane_text.contains("How can I help")
            {
                return AgentKind::CodexCli;
            }
        }
        AgentKind::Unknown
    }

    fn parse_status(&self, pane_text: &str) -> Option<String> {
        let lines: Vec<&str> = pane_text.lines().collect();
        let start = lines.len().saturating_sub(8);

        // Codex CLI status appears in the bottom portion of the TUI
        for line in lines[start..].iter().rev() {
            let trimmed = line.trim();
            if trimmed.is_empty() {
                continue;
            }
            if let Some(_c) = trimmed.chars().next() {
                if STATUS_SPINNERS.contains(&_c) {
                    let text: String = trimmed.chars().skip(1).collect();
                    let text = text.trim().to_string();
                    if !text.is_empty() {
                        return Some(text);
                    }
                }
                // Also look for bracket-enclosed status: "[processing something]"
                if trimmed.starts_with('[')
                    && trimmed.len() > 3
                    && let Some(end) = trimmed.find(']')
                    && end > 1
                    && end < 30
                {
                    return Some(trimmed[..=end].to_string());
                }
            }
        }
        None
    }

    fn detect_interactive(&self, pane_text: &str) -> Option<InteractiveUi> {
        let lines: Vec<&str> = pane_text.lines().collect();
        let last = lines.len().saturating_sub(1);

        for def in UI_PATTERNS {
            // All `contains` strings must match on any line in the pane.
            let matched: bool = def
                .contains
                .iter()
                .all(|pat| lines.iter().any(|l| l.contains(pat)));
            if !matched {
                continue;
            }

            // Find content boundaries: from first match line to last match line.
            let first = lines
                .iter()
                .position(|l| def.contains.iter().any(|pat| l.contains(pat)))?;

            // Collect content until we hit a non-matching line after the start.
            let mut end = last;
            for (idx, line) in lines[first..=last].iter().enumerate() {
                let i = first + idx;
                end = i;
                if line.trim().is_empty() && idx > 1 {
                    end = i.saturating_sub(1);
                    break;
                }
            }

            let content = lines[first..=end].join("\n").trim().to_string();
            if content.is_empty() {
                continue;
            }

            return Some(InteractiveUi {
                kind: def.kind,
                content,
            });
        }
        None
    }
}

// ── CodexAgent ──

/// Codex CLI agent implementation.
pub struct CodexAgent;

impl Agent for CodexAgent {
    fn id(&self) -> AgentId {
        AgentId::CodexCli
    }

    fn new_session_command(&self) -> String {
        "codex".into()
    }

    fn resume_command(&self, _session_id: &str) -> Option<String> {
        None
    }

    fn extra_args(&self) -> Vec<String> {
        vec!["--yolo".into()]
    }

    fn supports_sessions(&self) -> bool {
        true
    }

    fn has_session_start_hook(&self) -> bool {
        false
    }

    fn output_source(&self) -> OutputSource {
        OutputSource::JsonlFiles
    }

    fn parser(&self) -> Box<dyn AgentParser> {
        Box::new(CodexParser)
    }

    fn graceful_shutdown_keys(&self) -> Vec<&'static str> {
        vec!["C-c"]
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::trait_def::OutputSource;

    #[test]
    fn test_detect_by_process_name() {
        assert_eq!(CodexParser::detect("", "codex"), AgentKind::CodexCli);
    }

    #[test]
    fn test_detect_by_pane_text() {
        let text = "Codex CLI — How can I help you today?";
        assert_eq!(CodexParser::detect(text, "bash"), AgentKind::CodexCli);
    }

    #[test]
    fn test_parse_status_braille_spinner() {
        let text = "⠋ Gathering context for your request\n";
        let parser = CodexParser;
        let status = parser.parse_status(text);
        assert_eq!(
            status.as_deref(),
            Some("Gathering context for your request")
        );
    }

    #[test]
    fn test_parse_status_bracket_style() {
        let text = "[processing request]\nsome output\n";
        let parser = CodexParser;
        let status = parser.parse_status(text);
        assert_eq!(status.as_deref(), Some("[processing request]"));
    }

    #[test]
    fn test_detect_interactive_permission() {
        let text = "Allow Codex to run this command?\nProceed? [y/N]\n";
        let parser = CodexParser;
        let ui = parser.detect_interactive(text);
        assert!(ui.is_some());
        assert_eq!(ui.as_ref().unwrap().kind, UiKind::PermissionPrompt);
    }

    #[test]
    fn test_detect_interactive_selection() {
        let text = "Select files to include (Use ↑/↓ to navigate):\n❯  src/main.rs\n  src/lib.rs\n";
        let parser = CodexParser;
        let ui = parser.detect_interactive(text);
        assert!(ui.is_some());
        assert_eq!(ui.as_ref().unwrap().kind, UiKind::AskUserQuestion);
    }

    #[test]
    fn test_codex_agent_identity() {
        let agent = CodexAgent;
        assert_eq!(agent.name(), "codex");
        assert_eq!(agent.kind(), AgentKind::CodexCli);
        assert!(agent.supports_sessions());
        assert!(!agent.has_session_start_hook());
        assert_eq!(agent.output_source(), OutputSource::JsonlFiles);
    }

    #[test]
    fn test_codex_agent_no_resume() {
        let agent = CodexAgent;
        assert!(agent.resume_command("any-id").is_none());
    }

    #[test]
    fn test_codex_agent_launch_command() {
        let agent = CodexAgent;
        assert_eq!(agent.new_session_command(), "codex");
    }

    #[test]
    fn test_no_false_positive_on_normal_text() {
        let text = "Just some normal shell output\n$ ls -la\n";
        let parser = CodexParser;
        assert!(parser.detect_interactive(text).is_none());
        assert_eq!(CodexParser::detect(text, "bash"), AgentKind::Unknown);
    }
}
