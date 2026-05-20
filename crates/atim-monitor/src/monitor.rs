use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use atim_core::error::Result;
use atim_core::message::{NewMessage, SessionId};
use tokio::sync::{Mutex, mpsc};

/// Polling interval for JSONL file changes.
const DEFAULT_POLL_INTERVAL: Duration = Duration::from_secs(2);

/// Result produced by the monitor.
pub enum MonitorEvent {
    /// New messages from a session JSONL.
    NewMessages(Vec<NewMessage>),
    /// The session map has changed (new session created).
    SessionMapChanged,
}

/// Monitors session JSONL logs for new messages via byte-offset polling.
///
/// JSONL files are stored by Claude Code under `~/.claude/projects/<slug>/`.
/// The monitor searches this directory tree for session files.
pub struct SessionMonitor {
    /// Path to the session_map.json (window_id → session_id).
    session_map_path: PathBuf,
    /// Known jsonl path per session_id (cached after first search).
    jsonl_cache: Arc<Mutex<HashMap<String, PathBuf>>>,
    /// Byte offsets per session (shared with state persistence).
    byte_offsets: Arc<Mutex<HashMap<String, u64>>>,
    /// Poll interval for JSONL files.
    poll_interval: Duration,
    /// Snapshot of the last known session IDs — used to detect new sessions.
    last_known_sessions: Vec<String>,
}

/// Resolve the path to a session's JSONL file by searching ~/.claude/projects/.
async fn resolve_jsonl(session_id: &str) -> Option<PathBuf> {
    let home = std::env::var("HOME").ok()?;
    let projects_dir = Path::new(&home).join(".claude").join("projects");
    if !projects_dir.exists() {
        return None;
    }
    let mut dir = tokio::fs::read_dir(&projects_dir).await.ok()?;
    while let Some(entry) = dir.next_entry().await.ok()? {
        if entry.file_type().await.map(|t| t.is_dir()).unwrap_or(false) {
            let path = entry.path().join(format!("{session_id}.jsonl"));
            if path.exists() {
                return Some(path);
            }
        }
    }
    None
}

impl SessionMonitor {
    /// Create a new session monitor.
    ///
    /// * `atim_dir` — the `~/.atim` directory (contains session_map.json)
    /// * `byte_offsets` — shared offset map, usually loaded from monitor_state.json
    /// * `poll_interval_secs` — how often to poll (default 2.0)
    pub fn new(
        atim_dir: PathBuf,
        byte_offsets: Arc<Mutex<HashMap<String, u64>>>,
        poll_interval_secs: f64,
    ) -> Self {
        Self {
            session_map_path: atim_dir.join("session_map.json"),
            jsonl_cache: Arc::new(Mutex::new(HashMap::new())),
            byte_offsets,
            poll_interval: if poll_interval_secs > 0.0 {
                Duration::from_secs_f64(poll_interval_secs)
            } else {
                DEFAULT_POLL_INTERVAL
            },
            last_known_sessions: Vec::new(),
        }
    }

    /// Run the monitor loop, sending events into the provided channel.
    ///
    /// This loops forever (or until the channel closes / an unrecoverable error).
    pub async fn run(mut self, tx: mpsc::UnboundedSender<MonitorEvent>) -> Result<()> {
        loop {
            // 1. Check for new sessions in the session map
            if let Err(e) = self.check_session_map(&tx).await {
                tracing::warn!("Session map check failed: {e}");
            }

            // 2. Poll all known session JSONL files
            if let Err(e) = self.poll_sessions(&tx).await {
                tracing::warn!("Session poll failed: {e}");
            }

            tokio::time::sleep(self.poll_interval).await;
        }
    }

    /// Check the session map for new or removed sessions.
    async fn check_session_map(&mut self, tx: &mpsc::UnboundedSender<MonitorEvent>) -> Result<()> {
        if !self.session_map_path.exists() {
            return Ok(());
        }

        let data = tokio::fs::read_to_string(&self.session_map_path).await?;
        let map: HashMap<String, String> = serde_json::from_str(&data)
            .map_err(|e| atim_core::error::Error::Parse(format!("invalid session map: {e}")))?;

        let current_sessions: Vec<String> = map.values().cloned().collect();

        // Detect new sessions
        let mut found_new = false;
        for session_id in &current_sessions {
            if !self.last_known_sessions.contains(session_id) {
                found_new = true;
                // Register initial byte offset (file end, so we don't replay history)
                let path = resolve_jsonl(session_id).await;
                match path {
                    Some(p) => {
                        if let Ok(meta) = tokio::fs::metadata(&p).await {
                            let file_len = meta.len();
                            let mut offsets = self.byte_offsets.lock().await;
                            offsets.entry(session_id.clone()).or_insert(file_len);
                            // Cache the resolved path
                            self.jsonl_cache.lock().await.insert(session_id.clone(), p);
                            tracing::info!(
                                "[monitor] New session {session_id}: jsonl exists, offset set to {file_len}",
                            );
                        }
                    }
                    None => {
                        tracing::info!(
                            "[monitor] New session {session_id}: no jsonl yet, waiting...",
                        );
                    }
                }
                let _ = tx.send(MonitorEvent::SessionMapChanged);
            }
        }

        if found_new {
            tracing::info!(
                "New sessions detected: {:?}",
                current_sessions
                    .iter()
                    .filter(|s| !self.last_known_sessions.contains(*s))
                    .collect::<Vec<_>>()
            );
        }

        // Clean up stale sessions
        let mut offsets = self.byte_offsets.lock().await;
        offsets.retain(|id, _| current_sessions.contains(id));
        drop(offsets);

        // CRITICAL: actually track known sessions so we don't re-detect them next cycle
        self.last_known_sessions = current_sessions;

        Ok(())
    }

    /// Poll all known sessions for new JSONL content.
    async fn poll_sessions(&self, tx: &mpsc::UnboundedSender<MonitorEvent>) -> Result<()> {
        // Collect sessions from byte_offsets + catch any from session_map not yet tracked
        let mut session_ids: Vec<String> = {
            let offsets = self.byte_offsets.lock().await;
            offsets.keys().cloned().collect()
        };
        // Check if any sessions in the map are missing from byte_offsets (jsonl appeared late)
        if self.session_map_path.exists() {
            if let Ok(data) = tokio::fs::read_to_string(&self.session_map_path).await {
                if let Ok(map) = serde_json::from_str::<HashMap<String, String>>(&data) {
                    for sid in map.values() {
                        if !session_ids.contains(sid) {
                            if let Some(path) = resolve_jsonl(sid).await {
                                if let Ok(meta) = tokio::fs::metadata(&path).await {
                                    let file_len = meta.len();
                                    let mut offsets = self.byte_offsets.lock().await;
                                    offsets.entry(sid.clone()).or_insert(file_len);
                                    self.jsonl_cache.lock().await.insert(sid.clone(), path);
                                    tracing::info!(
                                        "[monitor] Late-caught session {sid}: jsonl exists, offset set to {file_len}",
                                    );
                                }
                                session_ids.push(sid.clone());
                            }
                        }
                    }
                }
            }
        }

        // Resolve jsonl path for each session — try cache first, then search
        async fn jsonl_path(
            cache: &Arc<Mutex<HashMap<String, PathBuf>>>,
            session_id: &str,
        ) -> Option<PathBuf> {
            let cache_lock = cache.lock().await;
            if let Some(path) = cache_lock.get(session_id) {
                if path.exists() {
                    return Some(path.clone());
                }
            }
            drop(cache_lock);
            // Cache miss — search and cache the result
            if let Some(path) = resolve_jsonl(session_id).await {
                cache
                    .lock()
                    .await
                    .insert(session_id.to_string(), path.clone());
                Some(path)
            } else {
                None
            }
        }

        for session_id in &session_ids {
            let path = match jsonl_path(&self.jsonl_cache, session_id).await {
                Some(p) => p,
                None => continue,
            };

            let offset = {
                let offsets = self.byte_offsets.lock().await;
                offsets.get(session_id).copied().unwrap_or(0)
            };

            let (entries, file_size) =
                atim_parser::jsonl::JsonlParser::read_new(&path, offset).await?;

            if !entries.is_empty() {
                let mut offsets = self.byte_offsets.lock().await;
                offsets.insert(session_id.clone(), file_size);

                tracing::info!(
                    "[monitor] Parsed {} new entries from {session_id}.jsonl (offset {offset} → {file_size})",
                    entries.len(),
                );

                let mut messages = Vec::new();
                for entry in entries {
                    let is_complete = matches!(
                        entry.content_type,
                        atim_core::message::ContentType::ToolResult
                            | atim_core::message::ContentType::Text
                            | atim_core::message::ContentType::ToolUse
                    );
                    messages.push(NewMessage {
                        session_id: SessionId(session_id.clone()),
                        text: entry.text,
                        is_complete,
                        content_type: entry.content_type,
                        tool_use_id: entry.tool_use_id,
                        role: entry.role,
                        tool_name: entry.tool_name,
                        image_data: entry.image_data,
                    });
                }

                if !messages.is_empty() {
                    let _ = tx.send(MonitorEvent::NewMessages(messages));
                }
            }
        }

        Ok(())
    }
}
