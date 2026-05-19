use std::collections::HashMap;
use std::time::Duration;

use atim_core::error::{Error, Result};
use atim_core::message::WindowId;
use tokio::process::Command;

/// Info about a single tmux window.
#[derive(Debug, Clone)]
pub struct WindowInfo {
    pub window_id: WindowId,
    pub name: String,
    pub current_command: String,
}

/// Manages tmux windows for agent sessions.
///
/// All operations shell out to the `tmux` CLI via `tokio::process::Command`.
#[derive(Clone)]
pub struct TmuxManager {
    pub session_name: String,
    send_delay: Duration,
}

impl TmuxManager {
    /// Create a new manager for the given tmux session.
    ///
    /// `send_delay` is the pause between a `send-keys -l` and the trailing Enter
    /// keystroke — some agents need a brief delay to register the input field.
    pub fn new(session_name: &str) -> Self {
        Self {
            session_name: session_name.to_string(),
            send_delay: Duration::from_millis(100),
        }
    }

    /// Set the delay between typing text and pressing Enter.
    pub fn with_send_delay(mut self, delay: Duration) -> Self {
        self.send_delay = delay;
        self
    }

    /// Run `tmux` with the given args and return stdout as a String.
    async fn tmux(&self, args: &[&str]) -> Result<String> {
        let output = Command::new("tmux")
            .args(args)
            .output()
            .await
            .map_err(|e| Error::Tmux(format!("failed to execute tmux: {e}")))?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(Error::Tmux(format!(
                "tmux {} failed: {stderr}",
                args.join(" ")
            )));
        }

        Ok(String::from_utf8_lossy(&output.stdout).to_string())
    }

    // ── Window listing ──

    /// List all windows in the session.
    ///
    /// Returns `Ok(vec![])` if the session does not exist (no error).
    pub async fn list_windows(&self) -> Result<Vec<WindowInfo>> {
        let out = self
            .tmux(&[
                "list-windows",
                "-t",
                &self.session_name,
                "-F",
                "#{window_id}|#{window_name}|#{pane_current_command}",
            ])
            .await?;

        Ok(out
            .lines()
            .filter(|l| !l.trim().is_empty())
            .filter_map(|l| {
                let mut parts = l.splitn(3, '|');
                Some(WindowInfo {
                    window_id: WindowId(parts.next()?.to_string()),
                    name: parts.next()?.to_string(),
                    current_command: parts.next().unwrap_or("").to_string(),
                })
            })
            .collect())
    }

    /// Get a map of window_id → WindowInfo for quick lookup.
    pub async fn window_map(&self) -> Result<HashMap<String, WindowInfo>> {
        let windows = self.list_windows().await?;
        Ok(windows
            .into_iter()
            .map(|w| (w.window_id.0.clone(), w))
            .collect())
    }

    /// Find a window by its tmux window ID (e.g. "@0", "@12").
    pub async fn find_window(&self, window_id: &WindowId) -> Result<WindowInfo> {
        let out = self
            .tmux(&[
                "display-message",
                "-t",
                &format!("{}:{}", self.session_name, window_id.0),
                "-p",
                "#{window_id}|#{window_name}|#{pane_current_command}",
            ])
            .await
            .map_err(|_| Error::WindowNotFound(window_id.0.clone()))?;

        let trimmed = out.trim();
        let mut parts = trimmed.splitn(3, '|');
        Ok(WindowInfo {
            window_id: WindowId(parts.next().unwrap_or("").to_string()),
            name: parts.next().unwrap_or("").to_string(),
            current_command: parts.next().unwrap_or("").to_string(),
        })
    }

    /// Check if a window exists.
    pub async fn window_exists(&self, window_id: &WindowId) -> bool {
        self.find_window(window_id).await.is_ok()
    }

    // ── Window management ──

    /// Create a new window in the session.
    ///
    /// Returns the `WindowId` of the newly created window.
    /// Falls back to `-a` (append after last window) if the default
    /// placement hits an "index in use" conflict (e.g. from leftover
    /// windows of a previous run).
    pub async fn new_window(&self, name: &str, cwd: &str) -> Result<WindowId> {
        let result = self
            .tmux(&[
                "new-window",
                "-t",
                &self.session_name,
                "-n",
                name,
                "-c",
                cwd,
                "-P",
                "-F",
                "#{window_id}",
            ])
            .await;

        match result {
            Ok(out) => Ok(WindowId(out.trim().to_string())),
            Err(e) if e.to_string().contains("index") && e.to_string().contains("in use") => {
                tracing::warn!("Window index conflict, retrying with -a: {e}");
                let out = self
                    .tmux(&[
                        "new-window",
                        "-a",
                        "-t",
                        &self.session_name,
                        "-n",
                        name,
                        "-c",
                        cwd,
                        "-P",
                        "-F",
                        "#{window_id}",
                    ])
                    .await?;
                Ok(WindowId(out.trim().to_string()))
            }
            Err(e) => Err(e),
        }
    }

    /// Kill (close) a window by its ID.
    pub async fn kill_window(&self, window_id: &WindowId) -> Result<()> {
        self.tmux(&["kill-window", "-t", &window_id.0])
            .await?;
        Ok(())
    }

    /// Rename a window.
    pub async fn rename_window(&self, window_id: &WindowId, name: &str) -> Result<()> {
        self.tmux(&["rename-window", "-t", &window_id.0, name])
            .await?;
        Ok(())
    }

    // ── Pane I/O ──

    /// Capture the visible text content of a pane.
    ///
    /// Uses `-e` to preserve ANSI escape sequences (colours, bold, etc).
    pub async fn capture_pane(&self, window_id: &WindowId) -> Result<String> {
        self.tmux(&[
            "capture-pane",
            "-t",
            &window_id.0,
            "-p",  // print to stdout
            "-e",  // preserve ANSI
        ])
        .await
    }

    /// Send literal text to a pane.
    ///
    /// Uses `-l` (literal) to avoid interpreting special characters.
    pub async fn send_text(&self, window_id: &WindowId, text: &str) -> Result<()> {
        self.tmux(&["send-keys", "-t", &window_id.0, "-l", text])
            .await?;
        Ok(())
    }

    /// Send a key press (e.g. "Enter", "Escape", "C-c").
    pub async fn send_key(&self, window_id: &WindowId, key: &str) -> Result<()> {
        self.tmux(&["send-keys", "-t", &window_id.0, key])
            .await?;
        Ok(())
    }

    /// Send text followed by Enter.
    ///
    /// The optional `send_delay` (set via `with_send_delay`) pauses between
    /// typing the text and pressing Enter, giving the agent time to show
    /// its input prompt before receiving the newline.
    pub async fn send_line(&self, window_id: &WindowId, text: &str) -> Result<()> {
        self.send_text(window_id, text).await?;
        if !self.send_delay.is_zero() {
            tokio::time::sleep(self.send_delay).await;
        }
        self.send_key(window_id, "Enter").await?;
        Ok(())
    }

    /// Interrupt whatever the agent is doing (Ctrl-C).
    pub async fn interrupt(&self, window_id: &WindowId) -> Result<()> {
        self.send_key(window_id, "C-c").await
    }

    // ── Utility ──

    /// Check whether the aim tmux session exists.
    pub async fn session_exists(&self) -> bool {
        self.tmux(&["has-session", "-t", &self.session_name])
            .await
            .is_ok()
    }

    /// Create a new detached tmux session.
    pub async fn create_session(&self) -> Result<()> {
        self.tmux(&["new-session", "-d", "-s", &self.session_name])
            .await?;
        Ok(())
    }

    /// Ensure the session exists — creates it if missing.
    pub async fn ensure_session(&self) -> Result<()> {
        if !self.session_exists().await {
            tracing::info!("Creating tmux session \"{}\"", self.session_name);
            self.create_session().await?;
        }
        Ok(())
    }

    /// Capture the pane content and render it as a PNG screenshot.
    ///
    /// Returns the PNG bytes.
    pub async fn screenshot(&self, window_id: &WindowId) -> Result<Vec<u8>> {
        let ansi_text = self.capture_pane(window_id).await?;
        // Get actual pane width from tmux for correct layout
        crate::screenshot::render_ansi_to_png(&ansi_text)
    }

    /// Get the current working directory of a pane.
    pub async fn pane_cwd(&self, window_id: &WindowId) -> Result<String> {
        let out = self
            .tmux(&[
                "display-message",
                "-t",
                &window_id.0,
                "-p",
                "#{pane_current_path}",
            ])
            .await?;
        Ok(out.trim().to_string())
    }
}
