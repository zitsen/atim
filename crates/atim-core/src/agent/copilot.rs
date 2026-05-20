use std::path::{Path, PathBuf};

use super::AgentParser;
use super::trait_def::{Agent, AgentId, OutputSource};
use crate::error::Result;
use crate::message::{AgentKind, InteractiveUi, UiKind};

/// Copilot CLI spinner characters (Braille dots).
const STATUS_SPINNERS: &[char] = &['⠋', '⠙', '⠹', '⠸', '⠼', '⠴', '⠦', '⠧', '⠇', '⠏'];

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
                (t.starts_with("? ")
                    && t.len() > 3
                    && t.as_bytes()
                        .get(2)
                        .is_some_and(|&c| c.is_ascii_uppercase() || c == b'['))
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
            if let Some(c) = trimmed.chars().next()
                && STATUS_SPINNERS.contains(&c)
            {
                // Skip the multi-byte spinner character, get the rest
                let text: String = trimmed.chars().skip(1).collect();
                let text = text.trim().to_string();
                if !text.is_empty() {
                    return Some(text);
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
            let first = lines.iter().position(|l| {
                def.contains.iter().any(|pat| l.contains(pat))
                // line doesn't need to match ALL patterns, just at least one
            })?;

            // Collect content until we hit a non-matching line after the start.
            let mut end = last;
            for (idx, line) in lines[first..=last].iter().enumerate() {
                let i = first + idx;
                end = i;
                if line.trim().is_empty() && idx > 1 {
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

// ── CopilotAgent ──

/// Copilot CLI agent implementation.
///
/// Uses JSONL-based output (`~/.copilot/session-state/<uuid>/events.jsonl`)
/// and supports session resume.
pub struct CopilotAgent;

impl Agent for CopilotAgent {
    fn id(&self) -> AgentId {
        AgentId::CopilotCli
    }

    fn new_session_command(&self) -> String {
        "copilot".into()
    }

    fn resume_command(&self, session_id: &str) -> Option<String> {
        Some(format!("copilot --resume={session_id} --allow-all-tools"))
    }

    fn extra_args(&self) -> Vec<String> {
        vec!["--allow-all-tools".into()]
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
        Box::new(CopilotParser)
    }

    fn graceful_shutdown_keys(&self) -> Vec<&'static str> {
        vec!["C-c"]
    }

    // ── Session discovery ──

    fn discover_session(
        &self,
        cwd: &str,
        known_ids: &std::collections::HashSet<String>,
    ) -> Result<Option<String>> {
        discover_by_cwd(cwd, known_ids)
    }

    fn discover_session_by_pid(&self, window_id: &str) -> Result<Option<String>> {
        discover_by_pid_lsof(window_id)
    }

    fn scan_sessions(&self, _path: &Path) -> Result<Vec<crate::agent::DetectedSession>> {
        scan_copilot_session_files()
    }
}

// ── Copilot sessions directory ──

/// Resolve the Copilot session-state directory: `~/.copilot/session-state/`.
fn copilot_sessions_dir() -> Option<PathBuf> {
    std::env::var("HOME")
        .ok()
        .map(|h| PathBuf::from(h).join(".copilot").join("session-state"))
}

// ── Session discovery implementation ──

/// Discover a Copilot session by tracing pane PID with lsof.
///
/// Looks for open files matching `.copilot/session-state/<uuid>/events.jsonl`.
fn discover_by_pid_lsof(window_id: &str) -> Result<Option<String>> {
    use std::process::Command;

    let output = Command::new("tmux")
        .args(["display-message", "-t", window_id, "-p", "#{pane_pid}"])
        .output()
        .map_err(crate::error::Error::Io)?;
    let pane_pid = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if pane_pid.is_empty() || pane_pid == "0" {
        return Ok(None);
    }

    let mut all_pids = vec![pane_pid];
    let mut idx = 0;
    while idx < all_pids.len() {
        if let Ok(child_out) = Command::new("pgrep").args(["-P", &all_pids[idx]]).output() {
            for child in String::from_utf8_lossy(&child_out.stdout).lines() {
                let c = child.trim().to_string();
                if !c.is_empty() && !all_pids.contains(&c) {
                    all_pids.push(c);
                }
            }
        }
        idx += 1;
    }

    for pid in &all_pids {
        if let Ok(lsof_out) = Command::new("lsof").args(["-p", pid, "-F", "n"]).output() {
            let out = String::from_utf8_lossy(&lsof_out.stdout);
            for line in out.lines() {
                if let Some(path) = line.strip_prefix('n') {
                    // Look for Copilot session files: events.jsonl under .copilot/session-state/<uuid>/
                    if path.contains(".copilot")
                        && path.contains("session-state")
                        && path.ends_with("events.jsonl")
                    {
                        // Extract UUID from path: .../session-state/<uuid>/events.jsonl
                        if let Some(parent) = std::path::Path::new(path).parent()
                            && let Some(sid) = parent.file_name().and_then(|n| n.to_str())
                                && sid.len() == 36 && sid.contains('-') {
                                    tracing::info!(
                                        "Discovered Copilot session {sid} for window {window_id} via lsof (PID {pid})"
                                    );
                                    return Ok(Some(sid.to_string()));
                                }
                    }
                }
            }
        }
    }
    Ok(None)
}

/// Discover a Copilot session by matching cwd against session.start events
/// in `~/.copilot/session-state/<uuid>/events.jsonl`.
fn discover_by_cwd(
    cwd: &str,
    known_ids: &std::collections::HashSet<String>,
) -> Result<Option<String>> {
    let sess_dir = match copilot_sessions_dir() {
        Some(d) => d,
        None => return Ok(None),
    };
    if !sess_dir.is_dir() {
        return Ok(None);
    }

    let mut candidates: Vec<(std::time::SystemTime, String)> = Vec::new();
    let entries = match std::fs::read_dir(&sess_dir) {
        Ok(d) => d,
        Err(e) => {
            tracing::debug!("Cannot read copilot session dir: {e}");
            return Ok(None);
        }
    };

    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        let uuid = match path.file_name().and_then(|n| n.to_str()) {
            Some(u) if u.len() == 36 && u.contains('-') => u.to_string(),
            _ => continue,
        };
        if known_ids.contains(&uuid) {
            continue;
        }

        // Check if this session started in the given cwd
        let events_path = path.join("events.jsonl");
        if !events_path.exists() {
            continue;
        }
        if let Ok(content) = std::fs::read_to_string(&events_path)
            && let Some(line) = content.lines().next()
                && let Ok(val) = serde_json::from_str::<serde_json::Value>(line) {
                    let session_cwd = val["data"]["context"]["cwd"]
                        .as_str()
                        .unwrap_or("");
                    if session_cwd == cwd {
                        let mtime = entry
                            .metadata()
                            .ok()
                            .and_then(|m| m.modified().ok())
                            .unwrap_or(std::time::SystemTime::UNIX_EPOCH);
                        candidates.push((mtime, uuid));
                    }
                }
    }

    candidates.sort_by_key(|b| std::cmp::Reverse(b.0));
    let result = candidates.into_iter().next().map(|(_, sid)| sid);
    if result.is_some() {
        tracing::info!("Discovered Copilot session via cwd matching (cwd={cwd})");
    }
    Ok(result)
}

/// Scan `~/.copilot/session-state/` for all sessions.
fn scan_copilot_session_files() -> Result<Vec<crate::agent::DetectedSession>> {
    let sess_dir = match copilot_sessions_dir() {
        Some(d) => d,
        None => {
            return Err(crate::error::Error::NotFound(
                "no copilot session-state dir".into(),
            ))
        }
    };

    let mut sessions = Vec::new();
    let entries = match std::fs::read_dir(&sess_dir) {
        Ok(d) => d,
        Err(e) => return Err(crate::error::Error::Io(e)),
    };

    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        let uuid = match path.file_name().and_then(|n| n.to_str()) {
            Some(u) if u.len() == 36 && u.contains('-') => u.to_string(),
            _ => continue,
        };

        let events_path = path.join("events.jsonl");
        if !events_path.exists() {
            continue;
        }

        let content = match std::fs::read_to_string(&events_path) {
            Ok(c) => c,
            Err(_) => continue,
        };

        let summary = extract_session_summary(&content);
        let timestamp = extract_timestamp(&content);
        let message_count = estimate_message_count(&content);

        // Figure out a project slug from the session.start cwd
        let slug = extract_cwd_slug(&content);

        sessions.push(crate::agent::DetectedSession {
            id: uuid,
            project_slug: slug,
            summary,
            timestamp,
            message_count,
        });
    }

    sessions.sort_by(|a, b| b.timestamp.cmp(&a.timestamp));
    Ok(sessions)
}

/// Extract the first user message text from Copilot JSONL as a summary.
fn extract_session_summary(content: &str) -> String {
    for line in content.lines() {
        if let Ok(val) = serde_json::from_str::<serde_json::Value>(line)
            && val["type"].as_str() == Some("user.message")
                && let Some(text) = val["data"]["content"].as_str() {
                    let t = text.trim();
                    if !t.is_empty() && t.len() > 3 {
                        let mut summary = t.to_string();
                        if summary.len() > 200 {
                            let end = summary
                                .char_indices()
                                .nth(197)
                                .map(|(i, _)| i)
                                .unwrap_or(summary.len());
                            summary.truncate(end);
                            summary.push('…');
                        }
                        return summary;
                    }
                }
    }
    String::new()
}

/// Extract the first timestamp from Copilot JSONL content.
fn extract_timestamp(content: &str) -> String {
    for line in content.lines() {
        if let Ok(val) = serde_json::from_str::<serde_json::Value>(line)
            && let Some(ts) = val["timestamp"].as_str()
                && !ts.is_empty() {
                    return ts.to_string();
                }
    }
    String::new()
}

/// Count user/assistant message events in Copilot JSONL content.
fn estimate_message_count(content: &str) -> usize {
    let mut count = 0;
    for line in content.lines() {
        if let Ok(val) = serde_json::from_str::<serde_json::Value>(line) {
            match val["type"].as_str() {
                Some("user.message") | Some("assistant.message") => count += 1,
                _ => {}
            }
        }
    }
    count
}

/// Extract a project slug from Copilot's session.start cwd.
fn extract_cwd_slug(content: &str) -> String {
    for line in content.lines() {
        if let Ok(val) = serde_json::from_str::<serde_json::Value>(line)
            && val["type"].as_str() == Some("session.start")
                && let Some(cwd) = val["data"]["context"]["cwd"].as_str() {
                    return cwd.split('/').filter(|s| !s.is_empty()).collect::<Vec<_>>().join("-");
                }
    }
    String::new()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::trait_def::OutputSource;

    #[test]
    fn test_detect_by_process_name() {
        assert_eq!(CopilotParser::detect("", "copilot"), AgentKind::CopilotCli);
    }

    #[test]
    fn test_detect_by_pane_text() {
        let text = "Welcome to GitHub Copilot CLI!";
        assert_eq!(CopilotParser::detect(text, "bash"), AgentKind::CopilotCli);
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
    fn test_copilot_agent_identity() {
        let agent = CopilotAgent;
        assert_eq!(agent.name(), "copilot");
        assert_eq!(agent.kind(), AgentKind::CopilotCli);
        assert!(agent.supports_sessions());
        assert!(!agent.has_session_start_hook());
        assert_eq!(agent.output_source(), OutputSource::JsonlFiles);
    }

    #[test]
    fn test_copilot_agent_no_hook() {
        let agent = CopilotAgent;
        assert!(!agent.has_session_start_hook());
    }

    #[test]
    fn test_copilot_agent_resume_command() {
        let agent = CopilotAgent;
        let cmd = agent.resume_command("abc-def-123");
        assert!(cmd.is_some());
        let cmd = cmd.unwrap();
        assert!(cmd.contains("copilot"));
        assert!(cmd.contains("--resume"));
        assert!(cmd.contains("abc-def-123"));
    }

    #[test]
    fn test_copilot_agent_extra_args() {
        let agent = CopilotAgent;
        let args = agent.extra_args();
        assert!(args.contains(&"--allow-all-tools".to_string()));
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

    #[test]
    fn test_extract_session_summary() {
        let jsonl = r#"{"type":"session.start","data":{"sessionId":"abc"},"id":"s1","timestamp":"2026-01-01T00:00:00Z"}
    {"type":"user.message","data":{"content":"hello world"},"id":"u1","timestamp":"2026-01-01T00:00:01Z"}
    {"type":"assistant.message","data":{"content":"hi there"},"id":"a1","timestamp":"2026-01-01T00:00:02Z"}"#;
        assert_eq!(extract_session_summary(jsonl), "hello world");
    }

    #[test]
    fn test_extract_session_summary_empty() {
        assert_eq!(extract_session_summary(""), "");
    }

    #[test]
    fn test_estimate_message_count() {
        let jsonl = r#"{"type":"user.message"}
    {"type":"assistant.message"}
    {"type":"user.message"}"#;
        assert_eq!(estimate_message_count(jsonl), 3);
    }

    #[test]
    fn test_extract_cwd_slug() {
        let jsonl = r#"{"type":"session.start","data":{"context":{"cwd":"/home/user/projects/my-app"}},"id":"s1"}"#;
        assert_eq!(extract_cwd_slug(jsonl), "home-user-projects-my-app");
    }

    #[test]
    fn test_extract_cwd_slug_empty() {
        assert_eq!(extract_cwd_slug(""), "");
    }
}
