use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use atim_core::error::Result;
use atim_core::message::{NewMessage, SessionId};
use tokio::sync::{Mutex, mpsc};

/// Polling interval for JSONL file changes.
const DEFAULT_POLL_INTERVAL: Duration = Duration::from_secs(2);

/// Save byte offsets to disk every N cycles.
const SAVE_INTERVAL_CYCLES: u32 = 30;

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
    /// Path to state.json (window_states, a secondary source of session_ids).
    state_path: PathBuf,
    /// Path to monitor_state.json (persisted byte offsets).
    monitor_state_path: PathBuf,
    /// Known jsonl path per session_id (cached after first search).
    jsonl_cache: Arc<Mutex<HashMap<String, PathBuf>>>,
    /// Byte offsets per session (shared with state persistence).
    byte_offsets: Arc<Mutex<HashMap<String, u64>>>,
    /// Poll interval for JSONL files.
    poll_interval: Duration,
    /// Snapshot of the last known session IDs — used to detect new sessions.
    last_known_sessions: Vec<String>,
    /// Cycle counter for periodic offset saving.
    save_counter: u32,
    /// Path to sessions.json (V2 mirror, replaces state.json window_states).
    sessions_path: PathBuf,
}

/// Resolve the path to a session's JSONL file.
///
/// Checks multiple agent-specific paths:
/// 1. `~/.claude/projects/<slug>/<session_id>.jsonl` (Claude Code)
/// 2. `~/.copilot/session-state/<session_id>/events.jsonl` (Copilot CLI)
pub async fn resolve_jsonl(session_id: &str) -> Option<PathBuf> {
    let home = std::env::var("HOME").ok()?;

    // 1. Check Claude Code paths
    let claude_dir = Path::new(&home).join(".claude").join("projects");
    if claude_dir.exists() {
        let mut dir = tokio::fs::read_dir(&claude_dir).await.ok()?;
        while let Some(entry) = dir.next_entry().await.ok()? {
            if entry.file_type().await.map(|t| t.is_dir()).unwrap_or(false) {
                let path = entry.path().join(format!("{session_id}.jsonl"));
                if path.exists() {
                    return Some(path);
                }
            }
        }
    }

    // 2. Check Copilot CLI paths
    let copilot_path = Path::new(&home)
        .join(".copilot")
        .join("session-state")
        .join(session_id)
        .join("events.jsonl");
    if copilot_path.exists() {
        return Some(copilot_path);
    }

    None
}

impl SessionMonitor {
    /// Create a new session monitor.
    ///
    /// * `atim_dir` — the `~/.atim` directory (contains session_map.json, state.json, sessions.json)
    /// * `byte_offsets` — shared offset map, usually loaded from monitor_state.json
    /// * `poll_interval_secs` — how often to poll (default 2.0)
    pub fn new(
        atim_dir: PathBuf,
        byte_offsets: Arc<Mutex<HashMap<String, u64>>>,
        poll_interval_secs: f64,
    ) -> Self {
        Self {
            session_map_path: atim_dir.join("session_map.json"),
            state_path: atim_dir.join("state.json"),
            monitor_state_path: atim_dir.join("monitor_state.json"),
            sessions_path: atim_dir.join("sessions.json"),
            jsonl_cache: Arc::new(Mutex::new(HashMap::new())),
            byte_offsets,
            poll_interval: if poll_interval_secs > 0.0 {
                Duration::from_secs_f64(poll_interval_secs)
            } else {
                DEFAULT_POLL_INTERVAL
            },
            last_known_sessions: Vec::new(),
            save_counter: 0,
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

            // 3. Periodically persist byte offsets
            self.save_counter += 1;
            if self.save_counter.is_multiple_of(SAVE_INTERVAL_CYCLES) {
                self.save_offsets().await;
            }

            tokio::time::sleep(self.poll_interval).await;
        }
    }

    /// Collect all known session_ids from session_map.json (transient hook output)
    /// and sessions.json (V2 store mirror).
    async fn collect_known_sessions(&self) -> Vec<String> {
        let mut sessions = Vec::new();

        // Primary source: session_map.json (SessionStart hook writes here).
        // This is a transient pipe — server deletes it after reading, but
        // the monitor may catch it before the server does.
        if self.session_map_path.exists()
            && let Ok(data) = tokio::fs::read_to_string(&self.session_map_path).await
            && let Ok(map) = serde_json::from_str::<HashMap<String, String>>(&data)
        {
            for sid in map.values() {
                if !sessions.contains(sid) {
                    sessions.push(sid.clone());
                }
            }
        }

        // Secondary source: sessions.json (V2 mirror written by save_runtime).
        // Contains all known sessions keyed by session_id.
        if self.sessions_path.exists()
            && let Ok(data) = tokio::fs::read_to_string(&self.sessions_path).await
            && let Ok(parsed) = serde_json::from_str::<
                std::collections::HashMap<String, atim_core::session::SessionInfo>,
            >(&data)
        {
            for sid in parsed.keys() {
                if !sessions.contains(sid) {
                    sessions.push(sid.clone());
                }
            }
        }

        // Tertiary source: state.json window_states (V1 legacy, for backward compat).
        if self.state_path.exists()
            && let Ok(data) = tokio::fs::read_to_string(&self.state_path).await
            && let Ok(state) = serde_json::from_str::<serde_json::Value>(&data)
            && let Some(ws) = state.get("window_states").and_then(|v| v.as_object())
        {
            for entry in ws.values() {
                if let Some(sid) = entry.get("session_id").and_then(|v| v.as_str())
                    && !sid.is_empty()
                    && !sessions.contains(&sid.to_string())
                {
                    sessions.push(sid.to_string());
                }
            }
        }

        sessions
    }

    /// Check the session map for new or removed sessions.
    async fn check_session_map(&mut self, tx: &mpsc::UnboundedSender<MonitorEvent>) -> Result<()> {
        let current_sessions: Vec<String> = self.collect_known_sessions().await;

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

        // Clean up stale sessions — remove sessions whose JSONL no longer exists.
        // This avoids purging filesystem-discovered sessions (which wouldn't be in
        // current_sessions since they're not in session_map.json or state.json).
        {
            let mut offsets = self.byte_offsets.lock().await;
            let tracked: Vec<String> = offsets.keys().cloned().collect();
            for id in &tracked {
                if !current_sessions.contains(id) && resolve_jsonl(id).await.is_none() {
                    offsets.remove(id);
                    tracing::debug!("[monitor] Removed stale session {id}");
                }
            }
        }

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
        // Check if any sessions from session_map.json or state.json are missing from
        // byte_offsets (jsonl appeared late, or session not in session_map).
        let all_known = self.collect_known_sessions().await;
        for sid in &all_known {
            if !session_ids.contains(sid)
                && let Some(path) = resolve_jsonl(sid).await
            {
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

        // Phase 3: Scan filesystem for untracked JSONL files.
        // 3a: Claude Code sessions — ~/.claude/projects/<slug>/<session_id>.jsonl.
        if let Ok(home) = std::env::var("HOME") {
            let claude_dir = Path::new(&home).join(".claude").join("projects");
            if claude_dir.exists()
                && let Ok(mut dir) = tokio::fs::read_dir(&claude_dir).await
            {
                while let Ok(Some(entry)) = dir.next_entry().await {
                    let Ok(ft) = entry.file_type().await else {
                        continue;
                    };
                    if !ft.is_dir() {
                        continue;
                    }
                    let slug_dir = entry.path();
                    let Ok(mut slug_reader) = tokio::fs::read_dir(&slug_dir).await else {
                        continue;
                    };
                    while let Ok(Some(file_entry)) = slug_reader.next_entry().await {
                        let path = file_entry.path();
                        if path.extension().and_then(|e| e.to_str()) != Some("jsonl") {
                            continue;
                        }
                        let Some(session_id) = path.file_stem().and_then(|s| s.to_str()) else {
                            continue;
                        };
                        let sid_str = session_id.to_string();
                        // Skip if already tracked
                        if session_ids.contains(&sid_str) || all_known.contains(&sid_str) {
                            continue;
                        }
                        {
                            let offsets = self.byte_offsets.lock().await;
                            if offsets.contains_key(&sid_str) {
                                continue;
                            }
                        }
                        // Only track files modified within the last hour
                        if let Ok(meta) = tokio::fs::metadata(&path).await {
                            let is_recent = meta
                                .modified()
                                .map(|m| m.elapsed().map(|e| e.as_secs() < 3600).unwrap_or(true))
                                .unwrap_or(true);
                            if !is_recent {
                                continue;
                            }
                            let file_len = meta.len();
                            let mut offsets = self.byte_offsets.lock().await;
                            offsets.insert(sid_str.clone(), file_len);
                            self.jsonl_cache
                                .lock()
                                .await
                                .insert(sid_str.clone(), path.clone());
                            tracing::info!(
                                "[monitor] Discovered untracked session {sid_str} via fs scan (slug: {:?})",
                                slug_dir.file_name(),
                            );
                            session_ids.push(sid_str);
                        }
                    }
                }
            }
        }

        // 3b: Copilot CLI sessions — ~/.copilot/session-state/<uuid>/events.jsonl.
        if let Ok(home) = std::env::var("HOME") {
            let copilot_dir = Path::new(&home).join(".copilot").join("session-state");
            if copilot_dir.is_dir()
                && let Ok(mut dir) = tokio::fs::read_dir(&copilot_dir).await
            {
                while let Ok(Some(entry)) = dir.next_entry().await {
                    let Ok(ft) = entry.file_type().await else {
                        continue;
                    };
                    if !ft.is_dir() {
                        continue;
                    }
                    let fname = entry.file_name();
                    let Some(sid) = fname.to_str() else {
                        continue;
                    };
                    let sid_str = sid.to_string();
                    if sid.len() != 36 || !sid.contains('-') {
                        continue;
                    }
                    let path = entry.path().join("events.jsonl");
                    if session_ids.contains(&sid_str) || all_known.contains(&sid_str) {
                        continue;
                    }
                    {
                        let offsets = self.byte_offsets.lock().await;
                        if offsets.contains_key(&sid_str) {
                            continue;
                        }
                    }
                    if let Ok(meta) = tokio::fs::metadata(&path).await {
                        let is_recent = meta
                            .modified()
                            .map(|m| m.elapsed().map(|e| e.as_secs() < 3600).unwrap_or(true))
                            .unwrap_or(true);
                        if !is_recent {
                            continue;
                        }
                        let file_len = meta.len();
                        let mut offsets = self.byte_offsets.lock().await;
                        offsets.insert(sid_str.clone(), file_len);
                        self.jsonl_cache
                            .lock()
                            .await
                            .insert(sid_str.clone(), path.clone());
                        tracing::info!(
                            "[monitor] Discovered untracked Copilot session {sid_str} via fs scan"
                        );
                        session_ids.push(sid_str);
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
            if let Some(path) = cache_lock.get(session_id)
                && path.exists()
            {
                return Some(path.clone());
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

            let (entries, new_offset) = atim_parser::read_jsonl(&path, offset).await?;

            // Always advance the byte offset past processed data, even when
            // entries is empty (e.g. thinking-only or metadata-only lines).
            // This prevents getting stuck on lines that never produce entries.
            if new_offset > offset {
                let mut offsets = self.byte_offsets.lock().await;
                offsets.insert(session_id.clone(), new_offset);
            }

            if !entries.is_empty() {
                tracing::info!(
                    "[monitor] Parsed {} new entries from {session_id}.jsonl (offset {offset} → {new_offset})",
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

    /// Save current byte offsets to disk atomically.
    async fn save_offsets(&self) {
        let offsets = self.byte_offsets.lock().await;
        if let Ok(data) = serde_json::to_string_pretty(&*offsets) {
            // Write to temp then rename for atomicity
            let tmp_path = self.monitor_state_path.with_extension("json.tmp");
            if tokio::fs::write(&tmp_path, &data).await.is_ok()
                && tokio::fs::rename(&tmp_path, &self.monitor_state_path)
                    .await
                    .is_ok()
            {
                tracing::debug!("[monitor] Saved {} byte offsets", offsets.len());
            }
        }
    }
}
