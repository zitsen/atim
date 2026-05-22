use std::os::unix::io::AsRawFd;
use std::path::{Path, PathBuf};

use tokio::io::AsyncWriteExt;

use atim_core::error::{Error, Result};
use atim_core::session::{ServerState, ThreadBinding};

/// Manages atomic read/write of server state to disk.
///
/// All writes use a temp-file-plus-rename strategy for crash safety:
///   1. Write to `{path}.tmp`
///   2. `fsync` the temp file
///   3. Rename temp → target (atomic on Linux)
pub struct StateManager {
    state_file: PathBuf,
    session_map_file: PathBuf,
    monitor_state_file: PathBuf,
}

impl StateManager {
    pub fn new(
        state_file: PathBuf,
        session_map_file: PathBuf,
        monitor_state_file: PathBuf,
    ) -> Self {
        Self {
            state_file,
            session_map_file,
            monitor_state_file,
        }
    }

    // ── Server state ──

    /// Load the full server state from disk.
    ///
    /// Returns the default state (empty) if the file doesn't exist yet.
    pub async fn load_state(&self) -> Result<ServerState> {
        if !self.state_file.exists() {
            return Ok(ServerState::default());
        }
        let data = atomic_read(&self.state_file).await?;
        serde_json::from_str(&data).map_err(|e| Error::State(format!("parse error: {e}")))
    }

    /// Save the full server state to disk atomically.
    pub async fn save_state(&self, state: &ServerState) -> Result<()> {
        let data = serde_json::to_string_pretty(state)?;
        atomic_write(&self.state_file, data.as_bytes()).await
    }

    // ── Thread bindings ──

    /// Load thread bindings (convenience wrapper over `load_state`).
    pub async fn load_bindings(&self) -> Result<Vec<ThreadBinding>> {
        let state = self.load_state().await?;
        Ok(state.thread_bindings)
    }

    // ── Session map ──

    /// Read the session map (window_id → session_id JSON object).
    ///
    /// The session map is written by `atim hook` when Claude Code
    /// creates a new session. It maps tmux window IDs to active session IDs.
    pub async fn load_session_map(&self) -> Result<std::collections::HashMap<String, String>> {
        if !self.session_map_file.exists() {
            return Ok(std::collections::HashMap::new());
        }
        let data = atomic_read(&self.session_map_file).await?;
        serde_json::from_str(&data)
            .map_err(|e| Error::State(format!("session map parse error: {e}")))
    }

    /// Write the session map atomically.
    pub async fn save_session_map(
        &self,
        map: &std::collections::HashMap<String, String>,
    ) -> Result<()> {
        let data = serde_json::to_string_pretty(map)?;
        atomic_write(&self.session_map_file, data.as_bytes()).await
    }

    /// Remove stale entries from the session map (for windows that no longer exist).
    pub async fn clean_session_map<F>(&self, is_alive: F) -> Result<()>
    where
        F: Fn(&str) -> bool,
    {
        let mut map = self.load_session_map().await?;
        let before = map.len();
        map.retain(|window_id, _| is_alive(window_id));
        if map.len() != before {
            self.save_session_map(&map).await?;
        }
        Ok(())
    }

    // ── Monitor state (byte offsets) ──

    /// Load monitor byte offsets (session_id → last byte offset).
    pub async fn load_monitor_offsets(&self) -> Result<std::collections::HashMap<String, u64>> {
        if !self.monitor_state_file.exists() {
            return Ok(std::collections::HashMap::new());
        }
        let data = atomic_read(&self.monitor_state_file).await?;
        serde_json::from_str(&data)
            .map_err(|e| Error::State(format!("monitor state parse error: {e}")))
    }

    /// Save monitor byte offsets atomically.
    pub async fn save_monitor_offsets(
        &self,
        offsets: &std::collections::HashMap<String, u64>,
    ) -> Result<()> {
        let data = serde_json::to_string_pretty(offsets)?;
        atomic_write(&self.monitor_state_file, data.as_bytes()).await
    }
}

/// Acquire an exclusive file lock (via flock).
fn lock_exclusive(file: &tokio::fs::File) -> Result<()> {
    let fd = file.as_raw_fd();
    let ret = unsafe { libc::flock(fd, libc::LOCK_EX) };
    if ret != 0 {
        return Err(Error::State(format!(
            "flock(LOCK_EX) failed: {}",
            std::io::Error::last_os_error()
        )));
    }
    Ok(())
}

/// Acquire a shared file lock (via flock).
fn lock_shared(file: &tokio::fs::File) -> Result<()> {
    let fd = file.as_raw_fd();
    let ret = unsafe { libc::flock(fd, libc::LOCK_SH) };
    if ret != 0 {
        return Err(Error::State(format!(
            "flock(LOCK_SH) failed: {}",
            std::io::Error::last_os_error()
        )));
    }
    Ok(())
}

/// Release a file lock (via flock LOCK_UN).
fn unlock(file: &tokio::fs::File) {
    let fd = file.as_raw_fd();
    unsafe { libc::flock(fd, libc::LOCK_UN) };
}

/// Atomically write data to a file using temp + rename, with exclusive flock.
async fn atomic_write(path: &Path, data: &[u8]) -> Result<()> {
    let tmp_path = path.with_extension("tmp");

    // Ensure parent directory exists
    if let Some(parent) = path.parent() {
        tokio::fs::create_dir_all(parent).await?;
    }

    {
        let mut file = tokio::fs::File::create(&tmp_path).await?;
        lock_exclusive(&file)?;
        file.write_all(data).await?;
        file.sync_all().await?;
        unlock(&file);
    }

    tokio::fs::rename(&tmp_path, path).await?;

    // Sync the parent directory to ensure the rename is on disk
    if let Some(parent) = path.parent()
        && let Ok(parent_file) = tokio::fs::File::open(parent).await
    {
        parent_file.sync_all().await.ok();
    }

    Ok(())
}

/// Read a file's contents with a shared flock, for concurrent-safe reads.
async fn atomic_read(path: &Path) -> Result<String> {
    let file = tokio::fs::File::open(path).await?;
    lock_shared(&file)?;
    let data = tokio::fs::read_to_string(path).await?;
    unlock(&file);
    Ok(data)
}

#[cfg(test)]
mod tests {
    use super::*;
    use atim_core::session::WindowState;
    use std::collections::HashMap;

    #[tokio::test]
    async fn test_atomic_write_and_read() {
        let dir = std::env::temp_dir().join("atim-test-state");
        let _ = tokio::fs::remove_dir_all(&dir).await;
        tokio::fs::create_dir_all(&dir).await.unwrap();

        let path = dir.join("state.json");

        // Write state
        let state = ServerState {
            window_states: HashMap::from([(
                "@0".into(),
                WindowState {
                    session_id: "sess_1".into(),
                    cwd: "/home".into(),
                    window_name: "test".into(),
                    agent_type: "claude".into(),
                },
            )]),
            thread_bindings: vec![],
            window_display_names: HashMap::new(),
            user_window_offsets: HashMap::new(),
        };

        let mgr = StateManager::new(
            path.clone(),
            dir.join("session_map.json"),
            dir.join("monitor_state.json"),
        );
        mgr.save_state(&state).await.unwrap();

        // Read back
        let loaded = mgr.load_state().await.unwrap();
        assert_eq!(loaded.window_states.len(), 1);
        assert_eq!(loaded.window_states.get("@0").unwrap().session_id, "sess_1");

        tokio::fs::remove_dir_all(&dir).await.unwrap();
    }

    #[tokio::test]
    async fn test_atomic_read_with_flock() {
        let dir = std::env::temp_dir().join("atim-test-flock-read");
        let _ = tokio::fs::remove_dir_all(&dir).await;
        tokio::fs::create_dir_all(&dir).await.unwrap();

        let path = dir.join("flock_test.json");
        let content = r#"{"key": "value"}"#;
        tokio::fs::write(&path, content).await.unwrap();

        // Read with shared flock
        let data = atomic_read(&path).await.unwrap();
        assert_eq!(data, r#"{"key": "value"}"#);

        tokio::fs::remove_dir_all(&dir).await.unwrap();
    }

    #[tokio::test]
    async fn test_concurrent_atomic_writes() {
        let dir = std::env::temp_dir().join("atim-test-concurrent");
        let _ = tokio::fs::remove_dir_all(&dir).await;
        tokio::fs::create_dir_all(&dir).await.unwrap();

        let path = dir.join("concurrent.json");

        // Write initial content
        tokio::fs::write(&path, "initial").await.unwrap();

        // Three concurrent reads using atomic_read (shared flock)
        let data1 = atomic_read(&path).await.unwrap();
        let data2 = atomic_read(&path).await.unwrap();
        let data3 = atomic_read(&path).await.unwrap();
        assert_eq!(data1, "initial");
        assert_eq!(data2, "initial");
        assert_eq!(data3, "initial");

        // Verify atomic_write produces valid JSON with exclusive lock
        for i in 0..3 {
            let data = serde_json::json!({"writer": i});
            atomic_write(&path, &serde_json::to_vec(&data).unwrap())
                .await
                .unwrap();
            let read_back = tokio::fs::read_to_string(&path).await.unwrap();
            let parsed: serde_json::Value = serde_json::from_str(&read_back).unwrap();
            assert_eq!(parsed["writer"], i);
        }

        tokio::fs::remove_dir_all(&dir).await.unwrap();
    }
}
