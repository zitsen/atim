//! Windows terminal manager — ConPTY-based implementation.
//!
//! Uses `portable-pty` (the same library WezTerm uses) which provides
//! ConPTY support on Windows. Each "window" maps to a running child
//! process (the AI coding agent) attached to a ConPTY.
//!
//! This module is only compiled on Windows (`#[cfg(windows)]`).

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::Mutex;

use atim_core::error::{Error, Result};
use atim_core::message::WindowId;
use atim_core::terminal::WindowInfo;
use portable_pty::{CommandBuilder, MasterPty, NativePtySystem, PtySize, PtySystem};

/// A running agent session in a ConPTY.
struct PtyWindow {
    window_id: WindowId,
    name: String,
    master: Box<dyn MasterPty + Send>,
    child: Box<dyn portable_pty::Child + Send + Sync>,
    /// Latest captured pane content (ANSI preserved).
    buffer: Arc<Mutex<String>>,
    cwd: String,
}

impl PtyWindow {
    fn spawn(window_id: WindowId, name: String, cmd: &str, cwd: &str) -> Result<Self> {
        let pty_system = NativePtySystem::default();
        let mut cmd_builder = CommandBuilder::new(cmd);
        cmd_builder.cwd(cwd);
        cmd_builder.env("TERM", "xterm-256color");

        let pair = pty_system
            .openpty(PtySize {
                rows: 40,
                cols: 120,
                pixel_width: 0,
                pixel_height: 0,
            })
            .map_err(|e| Error::Tmux(format!("failed to open ConPTY: {e}")))?;

        let child = pair
            .slave
            .spawn_command(cmd_builder)
            .map_err(|e| Error::Tmux(format!("failed to spawn agent: {e}")))?;

        // Drop the slave after spawning — the master keeps the pty alive.
        drop(pair.slave);

        // Continuously read pty output into the buffer.
        let reader = pair.master.try_clone_reader().ok();
        let buffer = Arc::new(Mutex::new(String::new()));
        let buf = buffer.clone();
        std::thread::spawn(move || {
            use std::io::Read;
            if let Some(mut r) = reader {
                let mut buf_inner = [0u8; 4096];
                loop {
                    match r.read(&mut buf_inner) {
                        Ok(0) | Err(_) => break,
                        Ok(n) => {
                            if let Ok(mut b) = buf.lock() {
                                let text = String::from_utf8_lossy(&buf_inner[..n]).to_string();
                                b.push_str(&text);
                                if b.len() > 64 * 1024 {
                                    *b = b[b.len() - 32 * 1024..].to_string();
                                }
                            }
                        }
                    }
                }
            }
        });

        Ok(Self {
            window_id,
            name,
            master: pair.master,
            child,
            buffer,
            cwd: cwd.to_string(),
        })
    }
}

/// Windows terminal manager.
///
/// Each window is a child process (the agent) attached to a ConPTY.
/// There is no persistent session — the manager owns all windows in-process.
#[derive(Clone, Default)]
pub struct WindowsTerminalManager {
    /// window_id string (e.g. "w0", "w1") → PtyWindow.
    windows: Arc<Mutex<HashMap<String, PtyWindow>>>,
    next_id: Arc<Mutex<u32>>,
}

impl WindowsTerminalManager {
    pub fn new() -> Self {
        Self::default()
    }
}

#[async_trait::async_trait]
impl atim_core::terminal::TerminalManager for WindowsTerminalManager {
    fn session_name(&self) -> &str {
        "atim"
    }

    async fn list_windows(&self) -> Result<Vec<WindowInfo>> {
        let windows = self
            .windows
            .lock()
            .map_err(|_| Error::Tmux("lock poisoned".into()))?;
        Ok(windows
            .values()
            .map(|w| WindowInfo {
                window_id: w.window_id.clone(),
                name: w.name.clone(),
                current_command: "agent".to_string(),
            })
            .collect())
    }

    async fn window_map(&self) -> Result<HashMap<String, WindowInfo>> {
        let windows = self
            .windows
            .lock()
            .map_err(|_| Error::Tmux("lock poisoned".into()))?;
        Ok(windows
            .values()
            .map(|w| {
                (
                    w.window_id.0.clone(),
                    WindowInfo {
                        window_id: w.window_id.clone(),
                        name: w.name.clone(),
                        current_command: "agent".to_string(),
                    },
                )
            })
            .collect())
    }

    async fn find_window(&self, window_id: &WindowId) -> Result<WindowInfo> {
        let windows = self
            .windows
            .lock()
            .map_err(|_| Error::Tmux("lock poisoned".into()))?;
        windows
            .get(&window_id.0)
            .map(|w| WindowInfo {
                window_id: w.window_id.clone(),
                name: w.name.clone(),
                current_command: "agent".to_string(),
            })
            .ok_or_else(|| Error::WindowNotFound(window_id.0.clone()))
    }

    async fn list_all_windows(&self) -> Result<Vec<(WindowInfo, String)>> {
        let windows = self.list_windows().await?;
        Ok(windows
            .into_iter()
            .map(|w| (w, self.session_name().to_string()))
            .collect())
    }

    async fn new_window(&self, name: &str, cwd: &str) -> Result<WindowId> {
        let mut next = self
            .next_id
            .lock()
            .map_err(|_| Error::Tmux("lock poisoned".into()))?;
        let id = format!("w{}", *next);
        *next += 1;
        drop(next);

        let window_id = WindowId(id.clone());
        let window = PtyWindow::spawn(window_id.clone(), name.to_string(), "cmd.exe", cwd)?;

        let mut windows = self
            .windows
            .lock()
            .map_err(|_| Error::Tmux("lock poisoned".into()))?;
        windows.insert(id, window);
        Ok(window_id)
    }

    async fn kill_window(&self, window_id: &WindowId) -> Result<()> {
        let mut windows = self
            .windows
            .lock()
            .map_err(|_| Error::Tmux("lock poisoned".into()))?;
        if let Some(mut w) = windows.remove(&window_id.0) {
            w.child.kill().ok();
        }
        Ok(())
    }

    async fn rename_window(&self, window_id: &WindowId, name: &str) -> Result<()> {
        let mut windows = self
            .windows
            .lock()
            .map_err(|_| Error::Tmux("lock poisoned".into()))?;
        if let Some(w) = windows.get_mut(&window_id.0) {
            w.name = name.to_string();
        }
        Ok(())
    }

    async fn capture_pane(&self, window_id: &WindowId) -> Result<String> {
        let windows = self
            .windows
            .lock()
            .map_err(|_| Error::Tmux("lock poisoned".into()))?;
        let w = windows
            .get(&window_id.0)
            .ok_or_else(|| Error::WindowNotFound(window_id.0.clone()))?;
        let buf = w
            .buffer
            .lock()
            .map_err(|_| Error::Tmux("lock poisoned".into()))?;
        Ok(buf.clone())
    }

    async fn send_text(&self, window_id: &WindowId, text: &str) -> Result<()> {
        use std::io::Write;
        let windows = self
            .windows
            .lock()
            .map_err(|_| Error::Tmux("lock poisoned".into()))?;
        let w = windows
            .get(&window_id.0)
            .ok_or_else(|| Error::WindowNotFound(window_id.0.clone()))?;
        let mut writer = w
            .master
            .take_writer()
            .map_err(|e| Error::Tmux(format!("take writer: {e}")))?;
        writer
            .write_all(text.as_bytes())
            .map_err(|e| Error::Tmux(format!("write: {e}")))?;
        writer
            .flush()
            .map_err(|e| Error::Tmux(format!("flush: {e}")))?;
        Ok(())
    }

    async fn send_key(&self, window_id: &WindowId, key: &str) -> Result<()> {
        use std::io::Write;
        let windows = self
            .windows
            .lock()
            .map_err(|_| Error::Tmux("lock poisoned".into()))?;
        let w = windows
            .get(&window_id.0)
            .ok_or_else(|| Error::WindowNotFound(window_id.0.clone()))?;
        let mut writer = w
            .master
            .take_writer()
            .map_err(|e| Error::Tmux(format!("take writer: {e}")))?;
        let bytes = match key {
            "Enter" => b"\r".to_vec(),
            "Escape" => b"\x1b".to_vec(),
            "C-c" => b"\x03".to_vec(),
            other => other.as_bytes().to_vec(),
        };
        writer
            .write_all(&bytes)
            .map_err(|e| Error::Tmux(format!("write: {e}")))?;
        writer
            .flush()
            .map_err(|e| Error::Tmux(format!("flush: {e}")))?;
        Ok(())
    }

    async fn send_line(&self, window_id: &WindowId, text: &str) -> Result<()> {
        self.send_text(window_id, text).await?;
        self.send_key(window_id, "Enter").await
    }

    async fn send_line_chars(
        &self,
        window_id: &WindowId,
        text: &str,
        char_delay_ms: u64,
    ) -> Result<()> {
        for c in text.chars() {
            let mut buf = [0u8; 4];
            self.send_text(window_id, c.encode_utf8(&mut buf)).await?;
            if char_delay_ms > 0 {
                tokio::time::sleep(std::time::Duration::from_millis(char_delay_ms)).await;
            }
        }
        self.send_key(window_id, "Enter").await
    }

    async fn session_exists(&self) -> bool {
        true
    }

    async fn create_session(&self) -> Result<()> {
        Ok(())
    }

    async fn ensure_session(&self) -> Result<()> {
        Ok(())
    }

    async fn pane_cwd(&self, window_id: &WindowId) -> Result<String> {
        let windows = self
            .windows
            .lock()
            .map_err(|_| Error::Tmux("lock poisoned".into()))?;
        windows
            .get(&window_id.0)
            .map(|w| w.cwd.clone())
            .ok_or_else(|| Error::WindowNotFound(window_id.0.clone()))
    }

    async fn move_window_into_session(
        &self,
        _src_session: &str,
        _window_id: &WindowId,
    ) -> Result<()> {
        // No-op on Windows — all windows live in this manager.
        Ok(())
    }

    async fn screenshot(&self, window_id: &WindowId) -> Result<Vec<u8>> {
        let text = self.capture_pane(window_id).await?;
        crate::screenshot::render_ansi_to_png(&text)
    }

    async fn wait_for_agent_ready(
        &self,
        window_id: &WindowId,
        timeout: std::time::Duration,
    ) -> Result<()> {
        let deadline = tokio::time::Instant::now() + timeout;
        let mut last = String::new();
        let mut stable = 0u32;

        while stable < 3 {
            if tokio::time::Instant::now() >= deadline {
                return Ok(());
            }
            tokio::time::sleep(std::time::Duration::from_millis(500)).await;
            let current = self.capture_pane(window_id).await.unwrap_or_default();
            if current == last {
                stable += 1;
            } else {
                stable = 0;
            }
            last = current;
        }
        Ok(())
    }
}
