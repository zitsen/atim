use regex::Regex;

use super::AgentParser;
use crate::message::{AgentKind, InteractiveUi, UiKind};

/// Patterns for Claude Code interactive UIs.
static UI_PATTERNS: &[UiPatternDef] = &[
    UiPatternDef {
        kind: UiKind::ExitPlanMode,
        top: &[
            r"^\s*Would you like to proceed\?",
            r"^\s*Claude has written up a plan",
        ],
        bottom: &[r"^\s*ctrl-g to edit in ", r"^\s*Esc to (cancel|exit)"],
    },
    UiPatternDef {
        kind: UiKind::AskUserQuestion,
        top: &[r"^\s*←\s+[☐✔☒]"],
        bottom: &[],
    },
    UiPatternDef {
        kind: UiKind::AskUserQuestion,
        top: &[r"^\s*[☐✔☒]"],
        bottom: &[r"^\s*Enter to select"],
    },
    UiPatternDef {
        kind: UiKind::PermissionPrompt,
        top: &[
            r"^\s*Do you want to proceed\?",
            r"^\s*Do you want to make this edit",
            r"^\s*Do you want to create \S",
            r"^\s*Do you want to delete \S",
        ],
        bottom: &[r"^\s*Esc to cancel"],
    },
    UiPatternDef {
        kind: UiKind::PermissionPrompt,
        top: &[r"^\s*❯\s*1\.\s*Yes"],
        bottom: &[],
    },
    UiPatternDef {
        kind: UiKind::BashApproval,
        top: &[r"^\s*Bash command\s*$", r"^\s*This command requires approval"],
        bottom: &[r"^\s*Esc to cancel"],
    },
    UiPatternDef {
        kind: UiKind::RestoreCheckpoint,
        top: &[r"^\s*Restore the code"],
        bottom: &[r"^\s*Enter to continue"],
    },
    UiPatternDef {
        kind: UiKind::Settings,
        top: &[r"Settings:.*tab to cycle", r"Select model"],
        bottom: &[
            r"Esc to cancel",
            r"Esc to exit",
            r"Enter to confirm",
            r"Type to filter",
        ],
    },
];

struct UiPatternDef {
    kind: UiKind,
    top: &'static [&'static str],
    bottom: &'static [&'static str],
}

/// Claude Code spinners.
const STATUS_SPINNERS: &[char] = &['·', '✻', '✽', '✶', '✳', '✢'];

/// Parser for Claude Code terminal output.
pub struct ClaudeParser;

impl AgentParser for ClaudeParser {
    fn detect(pane_text: &str, process_name: &str) -> AgentKind {
        if process_name.contains("claude")
            || pane_text.contains("Claude Code")
            || pane_text.contains("anthropic")
        {
            AgentKind::ClaudeCode
        } else {
            AgentKind::Unknown
        }
    }

    fn parse_status(&self, pane_text: &str) -> Option<String> {
        let lines: Vec<&str> = pane_text.lines().collect();
        // Find chrome separator (──── line) in last 10 lines
        let search_start = lines.len().saturating_sub(10);
        let chrome_idx = lines[search_start..]
            .iter()
            .position(|l| l.trim().len() >= 20 && l.trim().chars().all(|c| c == '─'))
            .map(|i| search_start + i)?;

        // Check lines just above the separator for spinner
        for i in (0..chrome_idx).rev().take(4) {
            let line = lines[i].trim();
            if line.is_empty() {
                continue;
            }
            if let Some(c) = line.chars().next() {
                if STATUS_SPINNERS.contains(&c) {
                    return Some(line[1..].trim().to_string());
                }
            }
            return None;
        }
        None
    }

    fn detect_interactive(&self, pane_text: &str) -> Option<InteractiveUi> {
        let lines: Vec<&str> = pane_text.lines().collect();
        for def in UI_PATTERNS {
            if let Some(content) = try_extract(&lines, def) {
                return Some(InteractiveUi {
                    kind: def.kind,
                    content,
                });
            }
        }
        None
    }
}

fn try_extract(lines: &[&str], def: &UiPatternDef) -> Option<String> {
    let top_re: Vec<Regex> = def
        .top
        .iter()
        .map(|p| Regex::new(p).unwrap())
        .collect();
    let bottom_re: Vec<Regex> = def
        .bottom
        .iter()
        .map(|p| Regex::new(p).unwrap())
        .collect();

    let top_idx = lines.iter().position(|l| top_re.iter().any(|r| r.is_match(l)))?;
    let bottom_idx = if bottom_re.is_empty() {
        // No bottom pattern → use last non-empty line
        lines[top_idx + 1..]
            .iter()
            .rposition(|l| !l.trim().is_empty())
            .map(|i| top_idx + 1 + i)?
    } else {
        lines[top_idx + 1..]
            .iter()
            .position(|l| bottom_re.iter().any(|r| r.is_match(l)))
            .map(|i| top_idx + 1 + i)?
    };

    if bottom_idx - top_idx < 2 {
        return None;
    }

    Some(lines[top_idx..=bottom_idx].join("\n"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_detect() {
        let text = "Hello from Claude Code";
        assert_eq!(ClaudeParser::detect(text, "claude"), AgentKind::ClaudeCode);
    }

    #[test]
    fn test_detect_interactive_ask() {
        let text = "\n☐ Option 1\n☐ Option 2\nEnter to select\n";
        let parser = ClaudeParser;
        assert!(parser.detect_interactive(text).is_some());
    }
}
