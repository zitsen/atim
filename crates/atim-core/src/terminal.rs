//! Terminal abstraction — Atim controls AI coding agents through a
//! terminal multiplexer. The `TerminalManager` trait abstracts the
//! underlying implementation so Atim can run on Linux (tmux),
//! macOS (tmux), and Windows (ConPTY-based sessions).

use std::collections::HashMap;

use crate::error::Result;
use crate::message::WindowId;

/// Info about a single terminal window.
#[derive(Debug, Clone)]
pub struct WindowInfo {
    pub window_id: WindowId,
    pub name: String,
    pub current_command: String,
}

/// Abstract terminal manager.
///
/// Implementations:
/// - `TmuxManager` (Linux/macOS) — in the `atim-tmux` crate
/// - `WindowsTerminalManager` (Windows) — ConPTY-based, in `atim-tmux` behind `#[cfg(windows)]`
#[async_trait::async_trait]
pub trait TerminalManager: Send + Sync {
    /// The session name (e.g. tmux session name).
    fn session_name(&self) -> &str;

    // ── Window listing ──

    /// List all windows in the session.
    async fn list_windows(&self) -> Result<Vec<WindowInfo>>;

    /// Get a map of window_id → WindowInfo.
    async fn window_map(&self) -> Result<HashMap<String, WindowInfo>> {
        let windows = self.list_windows().await?;
        Ok(windows
            .into_iter()
            .map(|w| (w.window_id.0.clone(), w))
            .collect())
    }

    /// Find a window by its ID.
    async fn find_window(&self, window_id: &WindowId) -> Result<WindowInfo>;

    /// Check if a window exists.
    async fn window_exists(&self, window_id: &WindowId) -> bool {
        self.find_window(window_id).await.is_ok()
    }

    /// List windows across all sessions.
    /// Each result includes the session name.
    async fn list_all_windows(&self) -> Result<Vec<(WindowInfo, String)>>;

    // ── Window management ──

    /// Create a new window in the session. Returns the new window ID.
    async fn new_window(&self, name: &str, cwd: &str) -> Result<WindowId>;

    /// Kill (close) a window.
    async fn kill_window(&self, window_id: &WindowId) -> Result<()>;

    /// Rename a window.
    async fn rename_window(&self, window_id: &WindowId, name: &str) -> Result<()>;

    // ── Pane I/O ──

    /// Capture the visible text content of a pane (ANSI preserved).
    async fn capture_pane(&self, window_id: &WindowId) -> Result<String>;

    /// Send literal text to a pane (no Enter).
    async fn send_text(&self, window_id: &WindowId, text: &str) -> Result<()>;

    /// Send a key press (e.g. "Enter", "Escape", "C-c").
    async fn send_key(&self, window_id: &WindowId, key: &str) -> Result<()>;

    /// Send text followed by Enter.
    async fn send_line(&self, window_id: &WindowId, text: &str) -> Result<()>;

    /// Send text character-by-character followed by Enter (for TUIs).
    async fn send_line_chars(
        &self,
        window_id: &WindowId,
        text: &str,
        char_delay_ms: u64,
    ) -> Result<()>;

    /// Interrupt whatever the agent is doing (Ctrl-C).
    async fn interrupt(&self, window_id: &WindowId) -> Result<()> {
        self.send_key(window_id, "C-c").await
    }

    // ── Session lifecycle ──

    /// Check whether the session exists.
    async fn session_exists(&self) -> bool;

    /// Create a new detached session.
    async fn create_session(&self) -> Result<()>;

    /// Ensure the session exists — creates it if missing.
    async fn ensure_session(&self) -> Result<()>;

    // ── Utility ──

    /// Get the current working directory of a pane.
    async fn pane_cwd(&self, window_id: &WindowId) -> Result<String>;

    /// Move a window from another session into the atim session.
    async fn move_window_into_session(&self, src_session: &str, window_id: &WindowId)
    -> Result<()>;

    /// Capture the pane content and render it as a PNG screenshot.
    /// Returns the PNG bytes.
    async fn screenshot(&self, window_id: &WindowId) -> Result<Vec<u8>>;

    /// Wait for the pane content to stabilize after agent startup.
    ///
    /// Polls the pane and returns once the visible content has been
    /// unchanged for consecutive checks (TUI finished rendering).
    /// Gives up after `timeout`.
    async fn wait_for_agent_ready(
        &self,
        window_id: &WindowId,
        timeout: std::time::Duration,
    ) -> Result<()>;
}
