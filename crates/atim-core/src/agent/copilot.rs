use super::AgentParser;
use crate::message::{AgentKind, InteractiveUi, UiKind};

/// Copilot CLI spinner characters (Braille dots).
const STATUS_SPINNERS: &[char] = &[
    '⠋', '⠙', '⠹', '⠸', '⠼', '⠴', '⠦', '⠧', '⠇', '⠏',
];

/// Patterns for Copilot CLI interactive UIs.
struct UiPatternDef {
    kind: UiKind,
    /// Line must contain ALL of these strings (AND match).
    contains: &'static [&'static str],
}

const UI_PATTERNS: &[UiPatternDef] = &[
    // ── Permission / approval prompts ──
    UiPatternDef {
        kind: UiKind::PermissionPrompt,
        contains: &["? ", "Do you want to continue"],
    },
    UiPatternDef {
        kind: UiKind::PermissionPrompt,
        contains: &["? ", "[y/N]"],
    },
    // ── AskUserQuestion: select menus ──
    UiPatternDef {
        kind: UiKind::AskUserQuestion,
        contains: &["Use ↑↓ to move"],
    },
    UiPatternDef {
        kind: UiKind::AskUserQuestion,
        contains: &["Use arrows to move"],
    },
    // ── AskUserQuestion: general `?` prompts with selection list ──
    UiPatternDef {
        kind: UiKind::AskUserQuestion,
        contains: &["? ", "❯"],
    },
    // ── AskUserQuestion: numbered suggestion list ──
    UiPatternDef {
        kind: UiKind::AskUserQuestion,
        contains: &["? ", "Suggest"],
    },
    // ── Main TUI screen / overlay (dismissable with Escape) ──
    UiPatternDef {
        kind: UiKind::Unknown,
        contains: &["esc close"],
    },
];

/// Parser for Copilot CLI terminal output.
pub struct CopilotParser;

impl AgentParser for CopilotParser {
    fn detect(pane_text: &str, process_name: &str) -> AgentKind {
        // Check process name first (most reliable)
        if process_name.contains("copilot") || process_name.contains("gh") {
            // `gh` is too generic alone — verify with pane text
            if process_name.contains("copilot") || pane_text.contains("copilot") {
                return AgentKind::CopilotCli;
            }
        }
        // Check pane text for Copilot CLI markers
        if pane_text.contains("GitHub Copilot")
            || pane_text.contains("Copilot CLI")
            || pane_text.lines().any(|l| {
                let t = l.trim();
                (t.starts_with("? ") && t.len() > 3 && t.as_bytes().get(2).map_or(false, |&c| c.is_ascii_uppercase() || c == b'['))
                    || t.starts_with("❯ ") && t.len() > 2
            })
        {
            return AgentKind::CopilotCli;
        }
        AgentKind::Unknown
    }

    fn parse_status(&self, pane_text: &str) -> Option<String> {
        let lines: Vec<&str> = pane_text.lines().collect();
        let start = lines.len().saturating_sub(8);
        for line in lines[start..].iter().rev() {
            let trimmed = line.trim();
            if trimmed.is_empty() {
                continue;
            }
            if let Some(c) = trimmed.chars().next() {
                if STATUS_SPINNERS.contains(&c) {
                    // Skip the multi-byte spinner character, get the rest
                    let text: String = trimmed.chars().skip(1).collect();
                    let text = text.trim().to_string();
                    if !text.is_empty() {
                        return Some(text);
                    }
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
            let matched: bool = def.contains.iter().all(|pat| {
                lines.iter().any(|l| l.contains(pat))
            });
            if !matched {
                continue;
            }

            // Find content boundaries: from first match line to last match line.
            let first = lines.iter().position(|l| {
                def.contains.iter().any(|pat| l.contains(pat))
                // line doesn't need to match ALL patterns, just at least one
            })?;

            // Collect content until we hit a non-matching line after the start.
            let mut end = first;
            for i in first..=last {
                end = i;
                if lines[i].trim().is_empty() && i > first + 1 {
                    // Empty line ends the block
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_detect_by_process_name() {
        assert_eq!(
            CopilotParser::detect("", "copilot"),
            AgentKind::CopilotCli
        );
    }

    #[test]
    fn test_detect_by_pane_text() {
        let text = "Welcome to GitHub Copilot CLI!";
        assert_eq!(
            CopilotParser::detect(text, "bash"),
            AgentKind::CopilotCli
        );
    }

    #[test]
    fn test_detect_gh_process() {
        // `gh` alone is not enough — need copilot in pane text too
        assert_eq!(
            CopilotParser::detect("some text with copilot", "gh"),
            AgentKind::CopilotCli
        );
    }

    #[test]
    fn test_parse_status_braille_spinner() {
        let text = "⠋ Loading suggestions\nsome output\n";
        let parser = CopilotParser;
        let status = parser.parse_status(text);
        assert_eq!(status.as_deref(), Some("Loading suggestions"));
    }

    #[test]
    fn test_detect_interactive_question_select() {
        let text = "\n? What would you like to do? [Use arrows to move]\n❯  Suggest code\n  Explain code\n";
        let parser = CopilotParser;
        let ui = parser.detect_interactive(text);
        assert!(ui.is_some());
        assert_eq!(ui.as_ref().unwrap().kind, UiKind::AskUserQuestion);
    }

    #[test]
    fn test_detect_interactive_confirm() {
        let text = "? Do you want to continue? [y/N]\n";
        let parser = CopilotParser;
        let ui = parser.detect_interactive(text);
        assert!(ui.is_some());
        assert_eq!(ui.as_ref().unwrap().kind, UiKind::PermissionPrompt);
    }

    #[test]
    fn test_no_false_positive_on_normal_text() {
        let text = "Just some normal shell output\n$ ls -la\n";
        let parser = CopilotParser;
        assert!(parser.detect_interactive(text).is_none());
        assert_eq!(CopilotParser::detect(text, "bash"), AgentKind::Unknown);
    }

    #[test]
    fn test_detect_interactive_with_arrows_pattern() {
        let text = "\n? Choose an option\n❯  Option 1\n  Option 2\n";
        let parser = CopilotParser;
        let ui = parser.detect_interactive(text);
        assert!(ui.is_some());
        assert_eq!(ui.as_ref().unwrap().kind, UiKind::AskUserQuestion);
    }
}
