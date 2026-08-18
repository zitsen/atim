use std::collections::HashMap;
use std::time::Duration;

use atim_core::error::{Error, Result};
use atim_core::message::WindowId;
use atim_core::terminal::WindowInfo;
use tokio::process::Command;

/// Alias for the shared WindowInfo type (defined in atim-core).
pub use atim_core::terminal::WindowInfo as TmuxWindowInfo;

/// Manages tmux windows for agent sessions.
///
/// All operations shell out to the `tmux` CLI via `tokio::process::Command`.
/// The binary name is configurable so Windows users can point Atim at
/// psmux (a native Windows tmux-compatible multiplexer that ships a
/// `tmux` alias — `winget install psmux`).
#[derive(Clone)]
pub struct TmuxManager {
    pub session_name: String,
    /// The tmux-compatible binary to invoke ("tmux" on Linux/macOS,
    /// "psmux" or its "tmux" alias on Windows).
    pub binary: String,
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
            binary: "tmux".to_string(),
            send_delay: Duration::from_millis(100),
        }
    }

    /// Use a custom binary instead of `tmux` (e.g. `psmux` on Windows).
    pub fn with_binary(mut self, binary: &str) -> Self {
        self.binary = binary.to_string();
        self
    }

    /// Set the delay between typing text and pressing Enter.
    pub fn with_send_delay(mut self, delay: Duration) -> Self {
        self.send_delay = delay;
        self
    }

    /// Run `tmux` with the given args and return stdout as a String.
    ///
    /// Automatically recovers from a dead tmux server: if the server socket
    /// is stale ("no server running"), creates a new session and retries.
    /// Also retries transient EIO errors (e.g. an underlying disk hiccup on
    /// a mounted filesystem) so a single glitch doesn't fail the operation.
    async fn tmux(&self, args: &[&str]) -> Result<String> {
        const MAX_EIO_RETRIES: u32 = 3;
        let mut eio_attempt = 0u32;

        loop {
            match self.tmux_once(args).await {
                Ok(out) => return Ok(out),
                Err(Error::Tmux(msg)) if is_eio_error(&msg) && eio_attempt < MAX_EIO_RETRIES => {
                    eio_attempt += 1;
                    tracing::warn!(
                        "tmux {} transient I/O error (attempt {}/{}): {}",
                        args.join(" "),
                        eio_attempt,
                        MAX_EIO_RETRIES,
                        msg
                    );
                    tokio::time::sleep(Duration::from_millis(500 * eio_attempt as u64)).await;
                    // Skip the dead-server recovery path for EIO — just retry.
                    continue;
                }
                Err(e) => return Err(e),
            }
        }
    }

    /// Execute one tmux command, including the dead-server recovery path.
    async fn tmux_once(&self, args: &[&str]) -> Result<String> {
        let output = Command::new(&self.binary)
            .args(args)
            .output()
            .await
            .map_err(|e| Error::Tmux(format!("failed to execute {}: {e}", self.binary)))?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            let err_msg = format!("tmux {} failed: {stderr}", args.join(" "));

            // Recover from a dead tmux server
            if stderr.contains("no server running") {
                tracing::warn!(
                    "tmux server not running, re-creating session \"{}\"",
                    self.session_name
                );
                let _ = Command::new(&self.binary)
                    .args(["new-session", "-d", "-s", &self.session_name])
                    .output()
                    .await;
                // Retry the original command
                let retry = Command::new(&self.binary)
                    .args(args)
                    .output()
                    .await
                    .map_err(|e| Error::Tmux(format!("failed to execute {}: {e}", self.binary)))?;
                if retry.status.success() {
                    return Ok(String::from_utf8_lossy(&retry.stdout).to_string());
                }
                let retry_stderr = String::from_utf8_lossy(&retry.stderr);
                return Err(Error::Tmux(format!(
                    "tmux {} failed (after server restart): {retry_stderr}",
                    args.join(" ")
                )));
            }

            return Err(Error::Tmux(err_msg));
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
        self.tmux(&["kill-window", "-t", &window_id.0]).await?;
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
            "-p", // print to stdout
            "-e", // preserve ANSI
        ])
        .await
    }

    /// Send literal text to a pane.
    ///
    /// Uses `-l` (literal) to avoid interpreting special characters.
    /// Uses `--` to prevent tmux from parsing text starting with `--` as flags.
    pub async fn send_text(&self, window_id: &WindowId, text: &str) -> Result<()> {
        self.tmux(&["send-keys", "-t", &window_id.0, "-l", "--", text])
            .await?;
        Ok(())
    }

    /// Send a key press (e.g. "Escape", "C-c").
    ///
    /// "Enter" is special-cased to send literal `\r` via `-l` so that
    /// psmux on Windows always interprets it as carriage-return (submit),
    /// not newline (next-line-in-input).  Other key names are sent via
    /// tmux's named-key protocol (`send-keys <key>`).
    pub async fn send_key(&self, window_id: &WindowId, key: &str) -> Result<()> {
        if key == "Enter" {
            // Literal CR (\r) — always "submit" regardless of terminal.
            self.tmux(&["send-keys", "-t", &window_id.0, "-l", "--", "\r"])
                .await?;
        } else {
            self.tmux(&["send-keys", "-t", &window_id.0, key]).await?;
        }
        Ok(())
    }

    /// Send text followed by Enter (carriage return `\r`).
    ///
    /// Uses literal `-l \r` instead of the named `Enter` key so the behavior
    /// is identical on tmux (Linux/macOS) and psmux (Windows) — psmux may
    /// interpret named keys differently than tmux, but `\r` is always a
    /// carriage return.
    pub async fn send_line(&self, window_id: &WindowId, text: &str) -> Result<()> {
        self.send_text(window_id, text).await?;
        if !self.send_delay.is_zero() {
            tokio::time::sleep(self.send_delay).await;
        }
        // Send literal CR (\r) — always "submit" regardless of terminal.
        // Named key "Enter" on psmux may be interpreted as \n ("newline")
        // rather than \r ("submit"), which moves the cursor down in Claude
        // Code's TUI instead of submitting the message.
        self.tmux(&["send-keys", "-t", &window_id.0, "-l", "--", "\r"])
            .await?;
        Ok(())
    }

    /// Send text character-by-character followed by Enter.
    ///
    /// This is more reliable for bubbletea TUIs (e.g. Copilot CLI) that
    /// process individual key events. Each character is sent as a separate
    /// `send-keys -l` call with a `char_delay_ms` pause between them.
    pub async fn send_line_chars(
        &self,
        window_id: &WindowId,
        text: &str,
        char_delay_ms: u64,
    ) -> Result<()> {
        for c in text.chars() {
            let mut buf = [0u8; 4];
            let s: &str = c.encode_utf8(&mut buf);
            self.tmux(&["send-keys", "-t", &window_id.0, "-l", s])
                .await?;
            if char_delay_ms > 0 {
                tokio::time::sleep(Duration::from_millis(char_delay_ms)).await;
            }
        }
        if !self.send_delay.is_zero() {
            tokio::time::sleep(self.send_delay).await;
        }
        // Same literal \r fix as send_line (see comment there).
        self.tmux(&["send-keys", "-t", &window_id.0, "-l", "--", "\r"])
            .await?;
        Ok(())
    }

    /// Interrupt whatever the agent is doing (Ctrl-C).
    pub async fn interrupt(&self, window_id: &WindowId) -> Result<()> {
        self.send_key(window_id, "C-c").await
    }

    /// Wait for the pane content to stabilize after agent startup.
    ///
    /// Polls the pane every 500ms and returns once the visible content has
    /// been unchanged for two consecutive checks (suggesting the TUI has
    /// finished rendering and is ready for input). Gives up after `timeout`.
    pub async fn wait_for_agent_ready(
        &self,
        window_id: &WindowId,
        timeout: Duration,
    ) -> Result<()> {
        let deadline = tokio::time::Instant::now() + timeout;
        let mut last = String::new();
        let mut stable_count = 0u32;

        while stable_count < 3 {
            if tokio::time::Instant::now() >= deadline {
                return Ok(());
            }
            tokio::time::sleep(Duration::from_millis(500)).await;
            let current = self.capture_pane(window_id).await.unwrap_or_default();
            if current == last {
                stable_count += 1;
            } else {
                stable_count = 0;
            }
            last = current;
        }
        Ok(())
    }

    /// `window_id|window_name|pane_command|session_name`.
    pub async fn list_all_windows(&self) -> Result<Vec<(WindowInfo, String)>> {
        let out = self
            .tmux(&[
                "list-windows",
                "-a",
                "-F",
                "#{window_id}|#{window_name}|#{pane_current_command}|#{session_name}",
            ])
            .await?;

        Ok(out
            .lines()
            .filter(|l| !l.trim().is_empty())
            .filter_map(|l| {
                let mut parts = l.splitn(4, '|');
                Some((
                    WindowInfo {
                        window_id: WindowId(parts.next()?.to_string()),
                        name: parts.next()?.to_string(),
                        current_command: parts.next().unwrap_or("").to_string(),
                    },
                    parts.next().unwrap_or("").to_string(),
                ))
            })
            .collect())
    }

    /// Move a window from another session into the atim session.
    pub async fn move_window_into_session(
        &self,
        src_session: &str,
        window_id: &WindowId,
    ) -> Result<()> {
        self.tmux(&[
            "move-window",
            "-s",
            &format!("{src_session}:{}", window_id.0),
            "-t",
            &self.session_name,
        ])
        .await?;
        tracing::info!(
            "Moved window {} from session '{}' to '{}'",
            window_id.0,
            src_session,
            self.session_name,
        );
        Ok(())
    }

    /// Check whether the atim tmux session exists.
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

/// Detect a transient I/O error (os error 5 / EIO) in a tmux error message.
/// These can come from an underlying mounted filesystem hiccup and are worth
/// retrying rather than failing the whole operation.
fn is_eio_error(msg: &str) -> bool {
    msg.contains("os error 5") || msg.contains("Input/output error") || msg.contains("EIO")
}

// ── TerminalManager trait implementation ──

#[async_trait::async_trait]
impl atim_core::terminal::TerminalManager for TmuxManager {
    fn session_name(&self) -> &str {
        &self.session_name
    }

    async fn list_windows(&self) -> Result<Vec<WindowInfo>> {
        TmuxManager::list_windows(self).await
    }

    async fn window_map(&self) -> Result<HashMap<String, WindowInfo>> {
        TmuxManager::window_map(self).await
    }

    async fn find_window(&self, window_id: &WindowId) -> Result<WindowInfo> {
        TmuxManager::find_window(self, window_id).await
    }

    async fn window_exists(&self, window_id: &WindowId) -> bool {
        TmuxManager::window_exists(self, window_id).await
    }

    async fn list_all_windows(&self) -> Result<Vec<(WindowInfo, String)>> {
        TmuxManager::list_all_windows(self).await
    }

    async fn new_window(&self, name: &str, cwd: &str) -> Result<WindowId> {
        TmuxManager::new_window(self, name, cwd).await
    }

    async fn kill_window(&self, window_id: &WindowId) -> Result<()> {
        TmuxManager::kill_window(self, window_id).await
    }

    async fn rename_window(&self, window_id: &WindowId, name: &str) -> Result<()> {
        TmuxManager::rename_window(self, window_id, name).await
    }

    async fn capture_pane(&self, window_id: &WindowId) -> Result<String> {
        TmuxManager::capture_pane(self, window_id).await
    }

    async fn send_text(&self, window_id: &WindowId, text: &str) -> Result<()> {
        TmuxManager::send_text(self, window_id, text).await
    }

    async fn send_key(&self, window_id: &WindowId, key: &str) -> Result<()> {
        TmuxManager::send_key(self, window_id, key).await
    }

    async fn send_line(&self, window_id: &WindowId, text: &str) -> Result<()> {
        TmuxManager::send_line(self, window_id, text).await
    }

    async fn send_line_chars(
        &self,
        window_id: &WindowId,
        text: &str,
        char_delay_ms: u64,
    ) -> Result<()> {
        TmuxManager::send_line_chars(self, window_id, text, char_delay_ms).await
    }

    async fn interrupt(&self, window_id: &WindowId) -> Result<()> {
        TmuxManager::interrupt(self, window_id).await
    }

    async fn session_exists(&self) -> bool {
        TmuxManager::session_exists(self).await
    }

    async fn create_session(&self) -> Result<()> {
        TmuxManager::create_session(self).await
    }

    async fn ensure_session(&self) -> Result<()> {
        TmuxManager::ensure_session(self).await
    }

    async fn pane_cwd(&self, window_id: &WindowId) -> Result<String> {
        TmuxManager::pane_cwd(self, window_id).await
    }

    async fn move_window_into_session(
        &self,
        src_session: &str,
        window_id: &WindowId,
    ) -> Result<()> {
        TmuxManager::move_window_into_session(self, src_session, window_id).await
    }

    async fn screenshot(&self, window_id: &WindowId) -> Result<Vec<u8>> {
        TmuxManager::screenshot(self, window_id).await
    }

    async fn wait_for_agent_ready(&self, window_id: &WindowId, timeout: Duration) -> Result<()> {
        TmuxManager::wait_for_agent_ready(self, window_id, timeout).await
    }
}
