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
        // Reuse Claude's session discovery — same ~/.claude/projects/ layout
        super::claude::discover_session_by_slug(cwd, known_ids)
    }

    fn discover_session_by_pid(&self, window_id: &str) -> Result<Option<String>> {
        super::claude::discover_by_pid_lsof(window_id)
    }

    fn scan_sessions(&self, path: &Path) -> Result<Vec<super::trait_def::DetectedSession>> {
        let canonical = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
        super::claude::scan_claude_session_files(Some(&canonical))
    }
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
