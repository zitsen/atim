use std::path::Path;

use super::AgentParser;
use super::claude::ClaudeParser;
use super::trait_def::{Agent, AgentId, OutputSource};
use crate::error::Result;
use crate::message::AgentKind;

/// mimo code agent — Claude Code compatible CLI.
pub struct MimoAgent;

impl Agent for MimoAgent {
    fn id(&self) -> AgentId {
        AgentId::MimoCode
    }

    fn new_session_command(&self) -> String {
        mimo_bin()
    }

    fn resume_command(&self, session_id: &str) -> Option<String> {
        Some(format!("{} --resume {session_id}", mimo_bin()))
    }

    fn supports_sessions(&self) -> bool {
        true
    }

    fn output_source(&self) -> OutputSource {
        OutputSource::JsonlFiles
    }

    fn parser(&self) -> Box<dyn AgentParser> {
        // Reuse Claude Code parser — same JSONL format
        Box::new(ClaudeParser)
    }

    fn graceful_shutdown_keys(&self) -> Vec<&'static str> {
        vec!["C-c"]
    }

    fn discover_session(
        &self,
        cwd: &str,
        known_ids: &std::collections::HashSet<String>,
    ) -> Result<Option<String>> {
        // Query mimo SQLite DB for the most recent session in this directory.
        let db_path = match mimo_db_path() {
            Some(p) => p,
            None => return Ok(None),
        };
        let db = rusqlite::Connection::open_with_flags(
            &db_path,
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
        )
        .map_err(|e| crate::error::Error::State(format!("mimo open: {e}")))?;

        // Normalize cwd to match mimo's directory column
        let canonical = std::fs::canonicalize(cwd)
            .map(|p| p.to_string_lossy().to_string())
            .unwrap_or_else(|_| cwd.to_string());

        let mut stmt = db
            .prepare(
                "SELECT id FROM session WHERE directory = ?1
                 AND id NOT IN (SELECT value FROM json_each(?2))
                 ORDER BY time_created DESC LIMIT 1",
            )
            .map_err(|e| crate::error::Error::State(format!("mimo prepare: {e}")))?;

        let known_json = serde_json::to_string(&known_ids.iter().collect::<Vec<_>>())
            .unwrap_or_else(|_| "[]".to_string());

        let result = stmt
            .query_row(rusqlite::params![canonical, known_json], |row| {
                row.get::<_, String>(0)
            })
            .ok();

        if result.is_some() {
            tracing::info!(
                "Discovered mimo session via DB (cwd={canonical}): {:?}",
                result
            );
        }
        Ok(result)
    }

    fn discover_session_by_pid(&self, _window_id: &str) -> Result<Option<String>> {
        // mimo doesn't expose session info via lsof
        Ok(None)
    }

    fn scan_sessions(&self, path: &Path) -> Result<Vec<super::trait_def::DetectedSession>> {
        let canonical = std::fs::canonicalize(path)
            .map(|p| p.to_string_lossy().to_string())
            .unwrap_or_else(|_| path.to_string_lossy().to_string());

        let db_path = match mimo_db_path() {
            Some(p) => p,
            None => return Ok(vec![]),
        };
        let db = rusqlite::Connection::open_with_flags(
            &db_path,
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
        )
        .map_err(|e| crate::error::Error::State(format!("mimo open: {e}")))?;

        let mut stmt = db
            .prepare(
                "SELECT id, title, directory, time_created FROM session
                 WHERE directory = ?1
                 ORDER BY time_created DESC LIMIT 25",
            )
            .map_err(|e| crate::error::Error::State(format!("mimo prepare: {e}")))?;

        let sessions: Vec<super::trait_def::DetectedSession> = stmt
            .query_map(rusqlite::params![canonical], |row| {
                let id: String = row.get(0)?;
                let title: String = row.get(1)?;
                let dir: String = row.get(2)?;
                let ts: i64 = row.get(3)?;
                Ok(super::trait_def::DetectedSession {
                    id,
                    project_slug: std::path::Path::new(&dir)
                        .file_name()
                        .map(|n| n.to_string_lossy().to_string())
                        .unwrap_or_default(),
                    summary: title,
                    timestamp: format_timestamp(ts),
                    message_count: 0,
                })
            })
            .map_err(|e| crate::error::Error::State(format!("mimo query: {e}")))?
            .filter_map(|r| r.ok())
            .collect();

        Ok(sessions)
    }
}

/// Path to the mimo SQLite database.
fn mimo_db_path() -> Option<std::path::PathBuf> {
    let home = std::env::var("HOME").ok()?;
    let p = std::path::PathBuf::from(home).join(".local/share/mimocode/mimocode.db");
    if p.exists() { Some(p) } else { None }
}

/// Convert mimo's unix-ms timestamp to ISO 8601 string.
fn format_timestamp(ms: i64) -> String {
    use std::time::{Duration, UNIX_EPOCH};
    let dt = UNIX_EPOCH + Duration::new((ms / 1000) as u64, ((ms % 1000) * 1_000_000) as u32);
    chrono::DateTime::<chrono::Utc>::from(dt)
        .format("%Y-%m-%dT%H:%M:%SZ")
        .to_string()
}

/// Resolve the mimo binary path.
fn mimo_bin() -> String {
    // Check env override first
    if let Ok(cmd) = std::env::var("ATIM_MIMO_COMMAND") {
        return cmd;
    }
    // Default: ~/.mimocode/bin/mimo
    if let Ok(home) = std::env::var("HOME") {
        let p = format!("{home}/.mimocode/bin/mimo");
        if std::path::Path::new(&p).exists() {
            return p;
        }
    }
    // Fallback: bare command
    "mimo".into()
}
