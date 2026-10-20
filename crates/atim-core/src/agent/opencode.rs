//! OpenCode agent (sst/opencode) — TUI coding agent with a SQLite session store.
//!
//! Launch/resume go through the `opencode` CLI (`opencode`, `opencode -s <id>`),
//! while session discovery and listing read OpenCode's own database
//! (`~/.local/share/opencode/opencode.db`) directly. Output delivery happens
//! through the monitor's database polling (`poll_opencode_db` in atim-monitor):
//! assistant text parts become clean chat messages instead of terminal redraws.

use std::path::Path;

use super::AgentParser;
use super::trait_def::{Agent, AgentId, DetectedSession, OutputSource};
use crate::error::Result;
use crate::message::AgentKind;

/// OpenCode agent — <https://github.com/sst/opencode>.
pub struct OpenCodeAgent;

impl Agent for OpenCodeAgent {
    fn id(&self) -> AgentId {
        AgentId::OpenCode
    }

    fn new_session_command(&self) -> String {
        opencode_bin()
    }

    /// The TUI accepts `-s/--session <id>` to continue a session.
    fn resume_command(&self, session_id: &str) -> Option<String> {
        Some(format!("{} -s {session_id}", opencode_bin()))
    }

    fn supports_sessions(&self) -> bool {
        true
    }

    /// OpenCode keeps sessions in its own SQLite database; the monitor reads
    /// that for delivery and discovery, so from atim's perspective it is a
    /// structured-log agent (not pane-capture).
    fn output_source(&self) -> OutputSource {
        OutputSource::JsonlFiles
    }

    fn parser(&self) -> Box<dyn AgentParser> {
        Box::new(OpenCodeParser)
    }

    fn graceful_shutdown_keys(&self) -> Vec<&'static str> {
        vec!["C-c"]
    }

    /// Most recent session in this directory that atim has not seen.
    ///
    /// Reads OpenCode's session store directly (read-only), matching the
    /// session's `directory` column against the canonical cwd.
    fn discover_session(
        &self,
        cwd: &str,
        known_ids: &std::collections::HashSet<String>,
        _min_mtime: std::time::SystemTime,
    ) -> Result<Option<String>> {
        let db_path = match opencode_db_path() {
            Some(p) => p,
            None => return Ok(None),
        };
        let db = rusqlite::Connection::open_with_flags(
            &db_path,
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
        )
        .map_err(|e| crate::error::Error::State(format!("opencode open: {e}")))?;

        let canonical = std::fs::canonicalize(cwd)
            .map(|p| p.to_string_lossy().to_string())
            .unwrap_or_else(|_| cwd.to_string());

        let mut stmt = db
            .prepare(
                "SELECT id FROM session WHERE directory = ?1
                 AND id NOT IN (SELECT value FROM json_each(?2))
                 ORDER BY time_updated DESC LIMIT 1",
            )
            .map_err(|e| crate::error::Error::State(format!("opencode prepare: {e}")))?;

        let known_json = serde_json::to_string(&known_ids.iter().collect::<Vec<_>>())
            .unwrap_or_else(|_| "[]".to_string());

        let result = stmt
            .query_row(rusqlite::params![canonical, known_json], |row| {
                row.get::<_, String>(0)
            })
            .ok();

        if result.is_some() {
            tracing::info!(
                "Discovered opencode session via DB (cwd={canonical}): {:?}",
                result
            );
        }
        Ok(result)
    }

    /// OpenCode does not expose its session through lsof the way Claude does.
    fn discover_session_by_pid(&self, _window_id: &str) -> Result<Option<String>> {
        Ok(None)
    }

    /// Sessions in this directory, newest first (for the resume picker).
    fn scan_sessions(&self, path: &Path) -> Result<Vec<DetectedSession>> {
        let canonical = std::fs::canonicalize(path)
            .map(|p| p.to_string_lossy().to_string())
            .unwrap_or_else(|_| path.to_string_lossy().to_string());

        let db_path = match opencode_db_path() {
            Some(p) => p,
            None => return Ok(vec![]),
        };
        let db = rusqlite::Connection::open_with_flags(
            &db_path,
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
        )
        .map_err(|e| crate::error::Error::State(format!("opencode open: {e}")))?;

        let mut stmt = db
            .prepare(
                "SELECT id, title, directory, time_updated FROM session
                 WHERE directory = ?1
                 ORDER BY time_updated DESC LIMIT 25",
            )
            .map_err(|e| crate::error::Error::State(format!("opencode prepare: {e}")))?;

        let sessions: Vec<DetectedSession> = stmt
            .query_map(rusqlite::params![canonical], |row| {
                let id: String = row.get(0)?;
                let title: String = row.get(1)?;
                let dir: String = row.get(2)?;
                let updated: i64 = row.get(3)?;
                Ok(DetectedSession {
                    id,
                    project_slug: std::path::Path::new(&dir)
                        .file_name()
                        .map(|n| n.to_string_lossy().to_string())
                        .unwrap_or_default(),
                    summary: title,
                    timestamp: format_timestamp_ms(updated),
                    message_count: 0,
                })
            })
            .map_err(|e| crate::error::Error::State(format!("opencode query: {e}")))?
            .filter_map(|r| r.ok())
            .collect();

        Ok(sessions)
    }
}

/// Terminal-output parser for OpenCode.
///
/// Phase 1: OpenCode's TUI is a full-screen bubbletea-style app whose approval
/// and status rendering does not match any of atim's pane patterns yet, so
/// neither status parsing nor pane-based interactive detection fires — the user
/// reads and answers in the tmux window itself.
pub struct OpenCodeParser;

impl AgentParser for OpenCodeParser {
    fn detect(pane_text: &str, process_name: &str) -> AgentKind {
        if process_name.contains("opencode") || pane_text.contains("opencode") {
            AgentKind::OpenCode
        } else {
            AgentKind::Unknown
        }
    }

    fn parse_status(&self, _pane_text: &str) -> Option<String> {
        None
    }

    fn detect_interactive(&self, _pane_text: &str) -> Option<crate::message::InteractiveUi> {
        None
    }
}

/// Path to OpenCode's SQLite session store.
pub fn opencode_db_path() -> Option<std::path::PathBuf> {
    let home = home::home_dir()?;
    let p = home.join(".local/share/opencode/opencode.db");
    p.exists().then_some(p)
}

/// Convert OpenCode's unix-ms timestamp to an ISO 8601 string.
fn format_timestamp_ms(ms: i64) -> String {
    use std::time::{Duration, UNIX_EPOCH};
    let dt = UNIX_EPOCH + Duration::new((ms / 1000) as u64, ((ms % 1000) * 1_000_000) as u32);
    chrono::DateTime::<chrono::Utc>::from(dt)
        .format("%Y-%m-%dT%H:%M:%SZ")
        .to_string()
}

/// Resolve the OpenCode binary path.
fn opencode_bin() -> String {
    if let Ok(cmd) = std::env::var("ATIM_OPENCODE_COMMAND") {
        return cmd;
    }
    // The official installer puts it here.
    if let Some(home) = home::home_dir() {
        let p = home.join(".opencode/bin/opencode");
        if p.exists() {
            return p.to_string_lossy().to_string();
        }
    }
    "opencode".into()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_new_session_command_is_the_tui() {
        // opencode_bin() may return the installer's full path (~/.opencode/bin),
        // so assert on the command name rather than the exact string.
        let cmd = OpenCodeAgent.new_session_command();
        assert!(cmd.ends_with("opencode"), "unexpected command: {cmd}");
    }

    #[test]
    fn test_resume_uses_the_session_flag() {
        let resume = OpenCodeAgent.resume_command("ses_abc123").expect("resume");
        assert!(resume.ends_with("-s ses_abc123"), "unexpected: {resume}");
        assert!(resume.contains("opencode"));
    }

    #[test]
    fn test_sessions_are_supported() {
        assert!(OpenCodeAgent.supports_sessions());
        assert_eq!(OpenCodeAgent.output_source(), OutputSource::JsonlFiles);
    }

    #[test]
    fn test_identity_and_kind() {
        assert_eq!(OpenCodeAgent.id().name(), "opencode");
        assert_eq!(
            OpenCodeAgent.id().kind(),
            crate::message::AgentKind::OpenCode
        );
    }

    #[test]
    fn test_parser_detects_opencode_by_process_name() {
        assert_eq!(
            OpenCodeParser::detect("some pane text", "opencode"),
            crate::message::AgentKind::OpenCode
        );
        assert_eq!(
            OpenCodeParser::detect("some pane text", "zsh"),
            crate::message::AgentKind::Unknown
        );
    }

    #[test]
    fn test_parser_detects_opencode_by_pane_text() {
        assert_eq!(
            OpenCodeParser::detect("opencode v1.14.48", "zsh"),
            crate::message::AgentKind::OpenCode
        );
    }

    #[test]
    fn test_no_status_line_or_interactive_detection_in_phase_1() {
        let parser = OpenCodeParser;
        assert_eq!(parser.parse_status("anything"), None);
        assert!(parser.detect_interactive("anything").is_none());
    }

    #[test]
    fn test_db_path_points_at_the_opencode_store() {
        // Only asserts the location, not existence — CI has no opencode install.
        let p = opencode_db_path();
        if let Some(path) = p {
            assert!(path.to_string_lossy().contains(".local/share/opencode"));
        }
    }
}
