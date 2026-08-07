use std::path::{Path, PathBuf};

use regex::Regex;

use super::AgentParser;
use super::trait_def::{Agent, AgentId, DetectedSession, OutputSource};
use crate::error::Result;
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
        top: &[
            r"^\s*Bash command\s*$",
            r"^\s*This command requires approval",
        ],
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
            if let Some(c) = line.chars().next()
                && STATUS_SPINNERS.contains(&c)
            {
                return Some(line[1..].trim().to_string());
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
    let top_re: Vec<Regex> = def.top.iter().map(|p| Regex::new(p).unwrap()).collect();
    let bottom_re: Vec<Regex> = def.bottom.iter().map(|p| Regex::new(p).unwrap()).collect();

    let top_idx = lines
        .iter()
        .position(|l| top_re.iter().any(|r| r.is_match(l)))?;
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

// ── ClaudeAgent ──

/// Claude Code agent implementation.
pub struct ClaudeAgent;

impl Agent for ClaudeAgent {
    fn id(&self) -> AgentId {
        AgentId::ClaudeCode
    }

    fn new_session_command(&self) -> String {
        std::env::var("ATIM_AGENT_COMMAND")
            .or_else(|_| std::env::var("AGENT_COMMAND"))
            .unwrap_or_else(|_| "claude".into())
    }

    fn resume_command(&self, session_id: &str) -> Option<String> {
        Some(format!(
            "{} --resume {session_id}",
            self.new_session_command()
        ))
    }

    fn supports_sessions(&self) -> bool {
        true
    }

    fn has_session_start_hook(&self) -> bool {
        true
    }

    fn output_source(&self) -> OutputSource {
        OutputSource::JsonlFiles
    }

    fn parser(&self) -> Box<dyn AgentParser> {
        Box::new(ClaudeParser)
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
        discover_session_by_slug(cwd, known_ids)
    }

    fn discover_session_by_pid(&self, window_id: &str) -> Result<Option<String>> {
        discover_by_pid_lsof(window_id)
    }

    fn scan_sessions(&self, path: &Path) -> Result<Vec<DetectedSession>> {
        let canonical = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
        scan_claude_session_files(Some(&canonical))
    }
}

// ── Session discovery implementation (Claude-specific) ──

/// Discover a Claude Code session by project-slug matching against
/// `~/.claude/projects/<slug>/`.
pub(crate) fn discover_session_by_slug(
    cwd: &str,
    known_ids: &std::collections::HashSet<String>,
) -> Result<Option<String>> {
    let slug: String = cwd.split('/').collect::<Vec<_>>().join("-");
    let proj_dir = claude_projects_dir()
        .ok_or_else(|| crate::error::Error::NotFound("no claude projects dir".into()))?
        .join("projects")
        .join(&slug);
    if !proj_dir.is_dir() {
        tracing::debug!("No claude project dir at {:?} for cwd {cwd}", proj_dir);
        return Ok(None);
    }

    let mut candidates: Vec<(std::time::SystemTime, String)> = Vec::new();
    let entries = std::fs::read_dir(&proj_dir).map_err(crate::error::Error::Io)?;

    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("jsonl") {
            continue;
        }
        let stem = match path.file_stem().and_then(|s| s.to_str()) {
            Some(s) => s.to_string(),
            None => continue,
        };
        if stem.len() != 36 || !stem.contains('-') {
            continue;
        }
        if known_ids.contains(&stem) {
            continue;
        }
        let mtime = entry
            .metadata()
            .ok()
            .and_then(|m| m.modified().ok())
            .unwrap_or(std::time::SystemTime::UNIX_EPOCH);
        candidates.push((mtime, stem));
    }

    candidates.sort_by_key(|b| std::cmp::Reverse(b.0));
    let result = candidates.into_iter().next().map(|(_, sid)| sid);
    if result.is_some() {
        tracing::info!("Discovered session via project-slug matching (cwd={cwd}, slug={slug})");
    }
    Ok(result)
}

/// Trace a tmux pane PID to find an open Claude Code JSONL file via lsof.
pub(crate) fn discover_by_pid_lsof(window_id: &str) -> Result<Option<String>> {
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
                if let Some(path) = line.strip_prefix('n')
                    && path.ends_with(".jsonl")
                    && path.contains(".claude")
                    && let Some(stem) = std::path::Path::new(path).file_stem()
                {
                    let sid = stem.to_string_lossy().to_string();
                    if sid.len() == 36 && sid.contains('-') {
                        tracing::info!(
                            "Discovered session {sid} for window {window_id} via lsof (PID {pid})"
                        );
                        return Ok(Some(sid));
                    }
                }
            }
        }
    }
    Ok(None)
}

/// Maximum sessions shown in the unfiltered fallback.
const FALLBACK_MAX_SESSIONS: usize = 25;

/// Scan `~/.claude/projects/` for session JSONL files.
///
/// When a working directory is provided, tries slug-filtered scanning first
/// (only sessions whose project directory name matches the path-derived slug).
/// If no sessions match the slug, falls back to scanning all projects — limited
/// to the most recent `FALLBACK_MAX_SESSIONS` — so the user never gets an empty
/// picker when the slug heuristic misses (bind mounts, canonicalization, etc.).
pub(crate) fn scan_claude_session_files(cwd: Option<&Path>) -> Result<Vec<DetectedSession>> {
    let claude_dir = claude_projects_dir()
        .ok_or_else(|| crate::error::Error::NotFound("no claude projects dir".into()))?;
    let projects_dir = claude_dir.join("projects");

    let target_slug = cwd.map(|p| {
        p.iter()
            .filter_map(|c| c.to_str())
            .map(|s| if s == "/" { "" } else { s })
            .collect::<Vec<_>>()
            .join("-")
    });

    // Fast path: try ~/.claude.json for the target cwd first.
    // This avoids scanning JSONL files for summary/timestamp when we already
    // have metadata from Claude Code's own state file.
    if let Some(canonical) = cwd.and_then(|p| std::fs::canonicalize(p).ok())
        && let Some(fast_session) =
            fast_session_from_claude_json(&canonical, target_slug.as_deref())
    {
        let mut sessions = scan_projects_dir(&projects_dir, target_slug.as_deref())?;
        // Deduplicate: remove JSONL-derived entry with the same ID
        sessions.retain(|s| s.id != fast_session.id);
        sessions.sort_by(|a, b| b.timestamp.cmp(&a.timestamp));
        sessions.insert(0, fast_session);
        return Ok(sessions);
    }

    // Slow path: scan JSONL files as before.
    let mut sessions = scan_projects_dir(&projects_dir, target_slug.as_deref())?;

    // Fallback: slug filter yielded nothing despite a slug being specified.
    // Return the most recent sessions across all projects instead.
    if sessions.is_empty() && target_slug.is_some() {
        tracing::debug!(
            "No sessions found for slug {:?}, falling back to full scan (capped at {})",
            target_slug,
            FALLBACK_MAX_SESSIONS,
        );
        sessions = scan_projects_dir(&projects_dir, None)?;
        sessions.sort_by(|a, b| b.timestamp.cmp(&a.timestamp));
        sessions.truncate(FALLBACK_MAX_SESSIONS);
    } else {
        sessions.sort_by(|a, b| b.timestamp.cmp(&a.timestamp));
    }

    Ok(sessions)
}

/// Try to read the last session info for `cwd` from `~/.claude.json`.
///
/// Returns a `DetectedSession` with the stored session ID and metadata,
/// avoiding the need to read JSONL files for summary/timestamp extraction.
fn fast_session_from_claude_json(cwd: &Path, target_slug: Option<&str>) -> Option<DetectedSession> {
    let claude_json_path = claude_projects_dir()?.join("../../.claude.json");
    // Canonicalize to handle the `..` path component
    let claude_json_path = std::fs::canonicalize(&claude_json_path).ok()?;
    let content = std::fs::read_to_string(&claude_json_path).ok()?;
    let parsed: serde_json::Value = serde_json::from_str(&content).ok()?;
    let projects = parsed.get("projects")?.as_object()?;
    let cwd_str = cwd.to_str()?;
    let proj = projects.get(cwd_str)?;
    let session_id = proj.get("lastSessionId")?.as_str()?;
    if session_id.is_empty() {
        return None;
    }
    let slug = target_slug.unwrap_or("").to_string();
    let mut summary = proj
        .get("lastSessionFirstPrompt")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    // If ~/.claude.json doesn't have the summary, try reading the JSONL file
    if summary.is_empty() {
        let jsonl_path = claude_projects_dir()?
            .join("projects")
            .join(&slug)
            .join(format!("{}.jsonl", session_id));
        if let Ok(content) = std::fs::read_to_string(&jsonl_path) {
            summary = extract_session_summary(&content);
        }
    }
    let timestamp = proj
        .get("lastSessionModified")
        .and_then(|v| v.as_i64())
        .map(|ms| {
            // Convert unix ms to ISO 8601 string for consistency with JSONL timestamps
            use std::time::{Duration, UNIX_EPOCH};
            let secs = ms / 1000;
            let nsecs = ((ms % 1000) * 1_000_000) as u32;
            let dt = UNIX_EPOCH + Duration::new(secs as u64, nsecs);
            chrono::DateTime::<chrono::Utc>::from(dt)
                .format("%Y-%m-%dT%H:%M:%SZ")
                .to_string()
        })
        .unwrap_or_default();
    Some(DetectedSession {
        id: session_id.to_string(),
        project_slug: slug,
        summary,
        timestamp,
        message_count: 0, // unknown from claude.json
    })
}

/// Scan one or all project directories under `projects_dir`.
///
/// If `filter_slug` is `Some`, only the project directory whose name matches
/// exactly is scanned. If `None`, all project directories are scanned.
fn scan_projects_dir(
    projects_dir: &Path,
    filter_slug: Option<&str>,
) -> Result<Vec<DetectedSession>> {
    let mut sessions = Vec::new();
    let projects = match std::fs::read_dir(projects_dir) {
        Ok(d) => d,
        Err(e) => return Err(crate::error::Error::Io(e)),
    };

    for project_dir in projects.flatten() {
        let proj_path = project_dir.path();
        if !proj_path.is_dir() {
            continue;
        }
        let slug = proj_path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("")
            .to_string();

        if let Some(target) = filter_slug
            && slug != target
        {
            continue;
        }

        let entries = match std::fs::read_dir(&proj_path) {
            Ok(d) => d,
            Err(_) => continue,
        };

        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("jsonl") {
                continue;
            }
            let session_id = match path.file_stem().and_then(|s| s.to_str()) {
                Some(s) if s.len() == 36 && s.contains('-') => s.to_string(),
                _ => continue,
            };

            let content = match std::fs::read_to_string(&path) {
                Ok(c) => c,
                Err(_) => continue,
            };

            let summary = extract_session_summary(&content);
            let timestamp = extract_timestamp(&content);
            let message_count = estimate_message_count(&content);

            sessions.push(DetectedSession {
                id: session_id,
                project_slug: slug.clone(),
                summary,
                timestamp,
                message_count,
            });
        }
    }

    Ok(sessions)
}

/// Resolve the claude projects directory: `~/.claude` or `$CLAUDE_DIR`.
pub fn claude_projects_dir() -> Option<PathBuf> {
    let base = std::env::var("CLAUDE_DIR")
        .ok()
        .map(PathBuf::from)
        .or_else(|| home_dir().map(|h| PathBuf::from(h).join(".claude")))?;
    Some(base)
}

/// Cross-platform home directory (HOME on Unix, USERPROFILE on Windows).
fn home_dir() -> Option<String> {
    home::home_dir().map(|p| p.to_string_lossy().into_owned())
}

/// Extract the first user text from JSONL content as a summary.
fn extract_session_summary(content: &str) -> String {
    let mut summary = String::new();
    for line in content.lines() {
        if let Ok(val) = serde_json::from_str::<serde_json::Value>(line) {
            let role = val["message"]["role"].as_str().unwrap_or("");
            if role == "user" {
                let content_val = &val["message"]["content"];
                // New format: content is a string
                if let Some(text) = content_val.as_str()
                    && !text.is_empty()
                    && !text.starts_with('<')
                // skip system XML echoes
                {
                    summary = text.to_string();
                    break;
                }
                // Old format: content is an array of blocks
                if let Some(blocks) = content_val.as_array() {
                    for block in blocks {
                        let text = block["text"].as_str().unwrap_or("");
                        if !text.is_empty() && text.len() > 3 && !text.starts_with('<')
                        // skip system XML echoes
                        {
                            summary = text.to_string();
                            break;
                        }
                    }
                }
            }
        }
        if !summary.is_empty() {
            break;
        }
    }

    if summary.len() > 200 {
        let end = summary
            .char_indices()
            .nth(197)
            .map(|(i, _)| i)
            .unwrap_or(summary.len());
        summary.truncate(end);
        summary.push('…');
    }
    summary
}

/// Extract the first timestamp from JSONL content.
fn extract_timestamp(content: &str) -> String {
    for line in content.lines() {
        if let Ok(val) = serde_json::from_str::<serde_json::Value>(line)
            && let Some(ts) = val["timestamp"].as_str()
            && !ts.is_empty()
        {
            return ts.to_string();
        }
    }
    String::new()
}

/// Count user/assistant type lines in JSONL content.
fn estimate_message_count(content: &str) -> usize {
    let mut count = 0;
    for line in content.lines() {
        if let Ok(val) = serde_json::from_str::<serde_json::Value>(line) {
            let role = val["message"]["role"].as_str().unwrap_or("");
            if role == "user" || role == "assistant" {
                count += 1;
            }
        }
    }
    count
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

    #[test]
    fn test_claude_agent_identity() {
        let agent = ClaudeAgent;
        assert_eq!(agent.name(), "claude");
        assert_eq!(agent.kind(), AgentKind::ClaudeCode);
        assert!(agent.supports_sessions());
        assert!(agent.has_session_start_hook());
        assert_eq!(agent.output_source(), OutputSource::JsonlFiles);
    }

    #[test]
    fn test_claude_agent_resume_command() {
        let agent = ClaudeAgent;
        let cmd = agent.resume_command("abc-def-123");
        assert!(cmd.is_some());
        let cmd = cmd.unwrap();
        assert!(cmd.contains("claude"));
        assert!(cmd.contains("--resume"));
        assert!(cmd.contains("abc-def-123"));
    }

    #[test]
    fn test_extract_session_summary() {
        let jsonl = r#"{"timestamp":"2025-01-01T00:00:00Z","message":{"role":"user","content":[{"type":"text","text":"hello world"}]}}
{"timestamp":"2025-01-01T00:00:01Z","message":{"role":"assistant","content":[{"type":"text","text":"hi there"}]}}"#;
        assert_eq!(extract_session_summary(jsonl), "hello world");
    }

    #[test]
    fn test_extract_session_summary_empty() {
        assert_eq!(extract_session_summary(""), "");
    }

    #[test]
    fn test_estimate_message_count() {
        let jsonl = r#"{"message":{"role":"user"}}
{"message":{"role":"assistant"}}
{"message":{"role":"user"}}"#;
        assert_eq!(estimate_message_count(jsonl), 3);
    }
}
