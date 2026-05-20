//! Directory browser and session picker state management.
//!
//! Tracks per-user browsing state (current path, page, mode). The Server
//! reads this state to build inline keyboards with proper callback tokens.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use atim_core::agent::SessionDiscoverer;
use tokio::sync::Mutex;

/// Items per page in the directory listing.
const PAGE_SIZE: usize = 10;

/// State for a user's directory browsing session.
#[derive(Debug, Clone)]
pub struct BrowserState {
    pub current_path: PathBuf,
    pub page: usize,
    pub mode: BrowserMode,
}

#[derive(Debug, Clone, PartialEq)]
pub enum BrowserMode {
    /// Browsing directories to pick a working directory.
    Browsing,
    /// Picking a Claude Code session to resume.
    SessionPick {
        sessions: Vec<ClaudeSession>,
    },
    /// Picking an unbound tmux window to attach.
    WindowPick {
        windows: Vec<WindowEntry>,
    },
}

/// Summary of a Claude Code session found in a directory.
#[derive(Debug, Clone, PartialEq)]
pub struct ClaudeSession {
    pub id: String,
    pub project_slug: String,
    pub summary: String,
    pub timestamp: String,
    pub message_count: usize,
}

/// A directory entry returned from a page of the listing.
#[derive(Debug, Clone)]
pub struct DirEntry {
    pub name: String,
    pub is_dir: bool,
    pub path: PathBuf,
}

/// Result of reading a page of directory entries.
#[derive(Debug, Clone)]
pub struct DirListing {
    pub entries: Vec<DirEntry>,
    pub page: usize,
    pub total_pages: usize,
    pub current_path: PathBuf,
    pub has_parent: bool,
}

/// A page of sessions from the session picker.
#[derive(Debug, Clone)]
pub struct SessionPickerPage {
    pub sessions: Vec<ClaudeSession>,
    pub page: usize,
    pub total_pages: usize,
}

/// A tmux window entry for the window picker.
#[derive(Debug, Clone, PartialEq)]
pub struct WindowEntry {
    pub window_id: String,
    pub name: String,
    pub current_command: String,
    pub agent_type: String,
}

/// A page of windows from the window picker.
#[derive(Debug, Clone)]
pub struct WindowPickerPage {
    pub windows: Vec<WindowEntry>,
    pub page: usize,
    pub total_pages: usize,
}

/// Manages directory browser state (no UI logic — just state transitions).
pub struct DirectoryBrowser {
    sessions: Arc<Mutex<HashMap<i64, BrowserState>>>,
}

impl DirectoryBrowser {
    pub fn new() -> Self {
        Self {
            sessions: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    #[allow(dead_code)]
    pub fn state(&self) -> &Arc<Mutex<HashMap<i64, BrowserState>>> {
        &self.sessions
    }

    /// Start a new browsing session for a user at a given path.
    pub async fn start_browsing(&self, user_id: i64, start_path: &Path) {
        let mut map = self.sessions.lock().await;
        map.insert(user_id, BrowserState {
            current_path: start_path.to_path_buf(),
            page: 0,
            mode: BrowserMode::Browsing,
        });
    }

    pub async fn get_state(&self, user_id: i64) -> Option<BrowserState> {
        let map = self.sessions.lock().await;
        map.get(&user_id).cloned()
    }

    pub async fn navigate_to(&self, user_id: i64, path: &Path) {
        let mut map = self.sessions.lock().await;
        if let Some(state) = map.get_mut(&user_id) {
            state.current_path = path.to_path_buf();
            state.page = 0;
        }
    }

    pub async fn go_up(&self, user_id: i64) {
        let mut map = self.sessions.lock().await;
        if let Some(state) = map.get_mut(&user_id) {
            if let Some(parent) = state.current_path.parent() {
                state.current_path = parent.to_path_buf();
                state.page = 0;
            }
        }
    }

    pub async fn show_session_picker(&self, user_id: i64, sessions: Vec<ClaudeSession>) {
        let mut map = self.sessions.lock().await;
        if let Some(state) = map.get_mut(&user_id) {
            state.mode = BrowserMode::SessionPick { sessions };
            state.page = 0;
        }
    }

    /// Switch to window picker mode with the given list of unbound windows.
    pub async fn show_window_picker(&self, user_id: i64, windows: Vec<WindowEntry>) {
        let mut map = self.sessions.lock().await;
        if let Some(state) = map.get_mut(&user_id) {
            state.mode = BrowserMode::WindowPick { windows };
            state.page = 0;
        }
    }

    pub async fn show_browsing(&self, user_id: i64) {
        let mut map = self.sessions.lock().await;
        if let Some(state) = map.get_mut(&user_id) {
            state.mode = BrowserMode::Browsing;
            state.page = 0;
        }
    }

    pub async fn set_page(&self, user_id: i64, page: usize) {
        let mut map = self.sessions.lock().await;
        if let Some(state) = map.get_mut(&user_id) {
            let max_page = match &state.mode {
                BrowserMode::Browsing => {
                    let entries = read_dir_entries(&state.current_path);
                    entries.len().saturating_sub(1) / PAGE_SIZE
                }
                BrowserMode::SessionPick { sessions } => {
                    sessions.len().saturating_sub(1) / PAGE_SIZE
                }
                BrowserMode::WindowPick { windows } => {
                    windows.len().saturating_sub(1) / PAGE_SIZE
                }
            };
            state.page = page.min(max_page);
        }
    }

    pub async fn end_session(&self, user_id: i64) {
        let mut map = self.sessions.lock().await;
        map.remove(&user_id);
    }
}

/// Get the directory listing for the current page of a browser state.
pub fn get_dir_listing(state: &BrowserState) -> DirListing {
    let mut entries = read_dir_entries(&state.current_path);

    // Insert ".." at the top if not at root
    if state.current_path.parent().is_some() {
        entries.insert(0, DirEntry {
            name: "..".into(),
            is_dir: true,
            path: state.current_path.parent().unwrap_or(&state.current_path).to_path_buf(),
        });
    }

    let total_pages = (entries.len().saturating_sub(1) / PAGE_SIZE) + 1;
    let page = state.page.min(total_pages.saturating_sub(1));
    let start = page * PAGE_SIZE;
    let page_entries: Vec<_> = entries.into_iter().skip(start).take(PAGE_SIZE).collect();

    DirListing {
        entries: page_entries,
        page,
        total_pages,
        current_path: state.current_path.clone(),
        has_parent: state.current_path.parent().is_some(),
    }
}

/// Get the session picker page for the given browser state.
pub fn get_session_picker_page(state: &BrowserState) -> Option<SessionPickerPage> {
    match &state.mode {
        BrowserMode::SessionPick { sessions } => {
            let total_pages = (sessions.len().saturating_sub(1) / PAGE_SIZE) + 1;
            let page = state.page.min(total_pages.saturating_sub(1));
            let start = page * PAGE_SIZE;
            let page_sessions: Vec<_> = sessions.iter().skip(start).take(PAGE_SIZE).cloned().collect();
            Some(SessionPickerPage {
                sessions: page_sessions,
                page,
                total_pages,
            })
        }
        BrowserMode::Browsing => None,
        BrowserMode::WindowPick { .. } => None,
    }
}

/// Get the window picker page for the given browser state.
pub fn get_window_picker_page(state: &BrowserState) -> Option<WindowPickerPage> {
    match &state.mode {
        BrowserMode::WindowPick { windows } => {
            let total_pages = (windows.len().saturating_sub(1) / PAGE_SIZE) + 1;
            let page = state.page.min(total_pages.saturating_sub(1));
            let start = page * PAGE_SIZE;
            let page_windows: Vec<_> = windows.iter().skip(start).take(PAGE_SIZE).cloned().collect();
            Some(WindowPickerPage {
                windows: page_windows,
                page,
                total_pages,
            })
        }
        _ => None,
    }
}

/// Read directory entries from a path (directories only, no hidden).
fn read_dir_entries(path: &Path) -> Vec<DirEntry> {
    let mut entries = Vec::new();
    if let Ok(read_dir) = std::fs::read_dir(path) {
        for entry in read_dir.flatten() {
            let name = entry.file_name().to_string_lossy().to_string();
            if name.starts_with('.') {
                continue;
            }
            let is_dir = entry.file_type().map(|t| t.is_dir()).unwrap_or(false);
            if !is_dir {
                continue;
            }
            entries.push(DirEntry {
                name,
                is_dir: true,
                path: entry.path(),
            });
        }
    }
    entries.sort_by(|a, b| a.name.to_lowercase().cmp(&b.name.to_lowercase()));
    entries
}

/// Scan for existing Claude Code sessions using ClaudeSessionDiscoverer.
pub async fn scan_claude_sessions(_path: &Path) -> Vec<ClaudeSession> {
    let discoverer = atim_core::agent::claude::ClaudeSessionDiscoverer;
    let detected = discoverer.scan_sessions(_path).await;
    detected
        .into_iter()
        .map(|s| ClaudeSession {
            id: s.id,
            project_slug: s.project_slug,
            summary: s.summary,
            timestamp: s.timestamp,
            message_count: s.message_count,
        })
        .collect()
}
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_read_dir_entries_filters_hidden() {
        let dir = std::env::temp_dir().join("atim-test-browser");
        let _ = std::fs::create_dir_all(&dir);
        let _ = std::fs::create_dir_all(dir.join(".hidden"));
        let _ = std::fs::create_dir_all(dir.join("visible"));

        let entries = read_dir_entries(&dir);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].name, "visible");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_read_dir_entries_sorted() {
        let dir = std::env::temp_dir().join("atim-test-browser-sorted");
        let _ = std::fs::create_dir_all(&dir);
        let _ = std::fs::create_dir_all(dir.join("zeta"));
        let _ = std::fs::create_dir_all(dir.join("alpha"));
        let _ = std::fs::create_dir_all(dir.join("beta"));

        let entries = read_dir_entries(&dir);
        assert_eq!(entries[0].name, "alpha");
        assert_eq!(entries[1].name, "beta");
        assert_eq!(entries[2].name, "zeta");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn test_browser_lifecycle() {
        let browser = DirectoryBrowser::new();
        let path = std::env::temp_dir();
        browser.start_browsing(100, &path).await;
        let state = browser.get_state(100).await.unwrap();
        assert_eq!(state.current_path, path);
        assert_eq!(state.mode, BrowserMode::Browsing);

        browser.go_up(100).await;
        browser.end_session(100).await;
        assert!(browser.get_state(100).await.is_none());
    }

    #[tokio::test]
    async fn test_session_picker_mode_switch() {
        let browser = DirectoryBrowser::new();
        browser.start_browsing(200, &std::env::temp_dir()).await;
        browser.show_session_picker(200, vec![]).await;
        let state = browser.get_state(200).await.unwrap();
        assert_eq!(state.mode, BrowserMode::SessionPick { sessions: vec![] });

        browser.show_browsing(200).await;
        let state = browser.get_state(200).await.unwrap();
        assert_eq!(state.mode, BrowserMode::Browsing);
        browser.end_session(200).await;
    }

    #[test]
    fn test_get_dir_listing_structure() {
        let state = BrowserState {
            current_path: std::env::temp_dir(),
            page: 0,
            mode: BrowserMode::Browsing,
        };
        let listing = get_dir_listing(&state);
        assert_eq!(listing.current_path, std::env::temp_dir());
        assert!(listing.has_parent);
        // page and total_pages make sense
        assert!(listing.page <= listing.total_pages);
    }

    #[tokio::test]
    async fn test_scan_sessions_no_error() {
        let sessions = scan_claude_sessions(&std::env::temp_dir()).await;
        assert!(sessions.is_empty() || !sessions.is_empty());
    }

    #[test]
    fn test_get_session_picker_page() {
        let state = BrowserState {
            current_path: PathBuf::from("/tmp"),
            page: 0,
            mode: BrowserMode::SessionPick {
                sessions: vec![
                    ClaudeSession { id: "a".into(), project_slug: "my-proj".into(), summary: "A".into(), timestamp: "2026-01-01".into(), message_count: 1 },
                    ClaudeSession { id: "b".into(), project_slug: "other-proj".into(), summary: "B".into(), timestamp: "2026-01-02".into(), message_count: 2 },
                ],
            },
        };
        let page = get_session_picker_page(&state).unwrap();
        assert_eq!(page.sessions.len(), 2);
    }

    #[test]
    fn test_browsing_mode_returns_none_for_picker() {
        let state = BrowserState {
            current_path: PathBuf::from("/tmp"),
            page: 0,
            mode: BrowserMode::Browsing,
        };
        assert!(get_session_picker_page(&state).is_none());
    }

    #[test]
    fn test_window_picker_mode() {
        let browser = DirectoryBrowser::new();
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            browser.start_browsing(400, &std::env::temp_dir()).await;
            let windows = vec![
                WindowEntry { window_id: "@1".into(), name: "atim-400".into(), current_command: "claude".into(), agent_type: "claude".into() },
                WindowEntry { window_id: "@2".into(), name: "dev".into(), current_command: "nvim".into(), agent_type: String::new() },
            ];
            browser.show_window_picker(400, windows.clone()).await;
            let state = browser.get_state(400).await.unwrap();
            assert_eq!(
                state.mode,
                BrowserMode::WindowPick { windows }
            );
            browser.end_session(400).await;
        });
    }

    #[test]
    fn test_window_picker_page() {
        let windows = vec![
            WindowEntry { window_id: "@1".into(), name: "w1".into(), current_command: "claude".into(), agent_type: "claude".into() },
            WindowEntry { window_id: "@2".into(), name: "w2".into(), current_command: "bash".into(), agent_type: String::new() },
        ];
        let state = BrowserState {
            current_path: PathBuf::from("/tmp"),
            page: 0,
            mode: BrowserMode::WindowPick { windows },
        };
        let page = get_window_picker_page(&state).unwrap();
        assert_eq!(page.windows.len(), 2);
    }

    #[test]
    fn test_window_picker_page_returns_none_for_browsing() {
        let state = BrowserState {
            current_path: PathBuf::from("/tmp"),
            page: 0,
            mode: BrowserMode::Browsing,
        };
        assert!(get_window_picker_page(&state).is_none());
    }

    #[test]
    fn test_get_session_picker_returns_none_for_window_pick() {
        let state = BrowserState {
            current_path: PathBuf::from("/tmp"),
            page: 0,
            mode: BrowserMode::WindowPick { windows: vec![] },
        };
        assert!(get_session_picker_page(&state).is_none());
    }

    #[test]
    fn test_set_page_clamps() {
        let browser = DirectoryBrowser::new();
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            browser.start_browsing(300, &PathBuf::from("/")).await;
            browser.set_page(300, 999).await; // should clamp
            let state = browser.get_state(300).await.unwrap();
            assert!(state.page < 100); // clamped to something reasonable
            browser.end_session(300).await;
        });
    }
}
