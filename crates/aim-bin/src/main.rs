use std::path::PathBuf;
use std::sync::Arc;

use aim_core::config::Config;
use aim_core::im::ImAdapter;
use tokio::sync::{mpsc, Mutex};

mod browser;
mod router;
mod server;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // 0. Load environment from ~/.aim/.env if it exists
    if let Ok(home) = std::env::var("HOME") {
        let env_path = PathBuf::from(home).join(".aim").join(".env");
        if env_path.exists() {
            dotenvy::from_filename(&env_path).ok();
            tracing::info!("Loaded environment from {:?}", env_path);
        }
    }

    // 1. Load config
    let config = Config::from_env()?;

    // 2. Setup logging
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    tracing::info!(
        "Aim v{} starting — aim_dir={:?}",
        env!("CARGO_PKG_VERSION"),
        config.aim_dir
    );

    // 3. Load persisted state
    let state_mgr = aim_state::persistence::StateManager::new(
        config.state_file.clone(),
        config.session_map_file.clone(),
        config.monitor_state_file.clone(),
    );

    let server_state = state_mgr.load_state().await?;
    let byte_offsets = state_mgr.load_monitor_offsets().await?;
    let byte_offsets = Arc::new(Mutex::new(byte_offsets));

    // 4. Ensure tmux session exists, then re-resolve stale window IDs
    let tmux_mgr = aim_tmux::manager::TmuxManager::new(&config.tmux_session_name);

    tmux_mgr.ensure_session().await?;

    let windows = tmux_mgr.window_map().await?;
    let resolved_state = re_resolve_state(server_state, &windows);
    state_mgr.save_state(&resolved_state).await?;
    state_mgr
        .clean_session_map(|window_id| windows.contains_key(window_id))
        .await?;

    // 5. Start the IM adapter
    let (im_tx, im_rx) = mpsc::unbounded_channel();

    let raw_adapter: Arc<dyn ImAdapter> = {
        let backend = std::env::var("AIM_IM_BACKEND")
            .unwrap_or_else(|_| "telegram".into());
        match backend.as_str() {
            "feishu" => {
                let adapter = aim_im::feishu::FeishuAdapter::new(
                    config.feishu_app_id.clone(),
                    config.feishu_app_secret.clone(),
                    config.feishu_webhook_port,
                );
                Arc::new(adapter)
            }
            _ => {
                // telegram (default)
                let adapter = aim_im::telegram::TelegramAdapter::new(config.telegram_bot_token.clone());
                Arc::new(adapter)
            }
        }
    };

    // Wrap with flood control for rate limiting
    let im_adapter: Arc<dyn ImAdapter> = Arc::new(
        aim_queue::flood_control::FloodControlledAdapter::new(raw_adapter.clone()),
    );

    let im_handle = {
        let adapter = raw_adapter;
        tokio::spawn(async move {
            if let Err(e) = adapter.run(im_tx).await {
                tracing::error!("IM adapter exited: {e}");
            }
        })
    };

    // 6. Start the monitor
    let monitor = aim_monitor::monitor::SessionMonitor::new(
        config.aim_dir.clone(),
        byte_offsets.clone(),
        config.monitor_poll_interval_secs,
    );
    let (monitor_tx, mut monitor_rx) = mpsc::unbounded_channel();
    let monitor_handle = tokio::spawn(async move {
        if let Err(e) = monitor.run(monitor_tx).await {
            tracing::error!("Session monitor exited: {e}");
        }
    });

    // 7. Start message queue worker
    let queue = Arc::new(Mutex::new(aim_queue::message_queue::MessageQueue::new()));

    // 8. Enter main event loop
    let server = server::Server {
        config,
        state_mgr,
        tmux_mgr,
        queue,
        byte_offsets,
        im_adapter,
        topic_names: Arc::new(Mutex::new(std::collections::HashMap::new())),
        pending_messages: Arc::new(Mutex::new(std::collections::HashMap::new())),
        callback_contexts: Arc::new(Mutex::new(std::collections::HashMap::new())),
        browser: browser::DirectoryBrowser::new(),
        tool_use_msg_ids: Arc::new(Mutex::new(std::collections::HashMap::new())),
        status_consumed: Arc::new(Mutex::new(std::collections::HashSet::new())),
        last_ui_states: Arc::new(Mutex::new(std::collections::HashMap::new())),
    };

    server.run(im_rx, &mut monitor_rx).await?;

    // 9. Graceful shutdown
    tracing::info!("Shutting down...");
    im_handle.abort();
    monitor_handle.abort();

    Ok(())
}

/// Re-resolve stale window IDs in persisted state against live tmux windows.
///
/// For each `ThreadBinding` whose `window_id` doesn't appear in the current
/// window map, look for a window whose display name matches. If found, update
/// the binding's `window_id`. If not, remove the binding entirely.
///
/// Similarly, for `window_states`, re-key stale entries by matching
/// `window_name` against current window display names.
fn re_resolve_state(
    mut state: aim_core::session::ServerState,
    windows: &std::collections::HashMap<String, aim_tmux::manager::WindowInfo>,
) -> aim_core::session::ServerState {
    // 1. Resolve window_states — re-key by matching window_name
    let mut resolved_windows = std::collections::HashMap::new();
    for (wid, ws) in &state.window_states {
        if windows.contains_key(wid) {
            resolved_windows.insert(wid.clone(), ws.clone());
        } else if let Some((new_id, _)) = windows.iter().find(|(_, info)| info.name == ws.window_name) {
            resolved_windows.insert(new_id.clone(), ws.clone());
            tracing::info!("Re-resolved window_state {wid} → {new_id} by name '{}'", ws.window_name);
        } else {
            tracing::info!("Removing stale window_state {wid} ('{}') — no matching tmux window", ws.window_name);
        }
    }
    state.window_states = resolved_windows;

    // 2. Resolve thread_bindings — update window_id by matching display_name
    let mut resolved_bindings = Vec::new();
    for tb in &state.thread_bindings {
        if windows.contains_key(&tb.window_id) {
            resolved_bindings.push(tb.clone());
        } else if let Some((new_id, _)) = windows.iter().find(|(_, info)| info.name == tb.display_name) {
            let mut updated = tb.clone();
            updated.window_id = new_id.clone();
            resolved_bindings.push(updated);
            tracing::info!("Re-resolved ThreadBinding window_id {} → {} by display_name '{}'", tb.window_id, new_id, tb.display_name);
        } else {
            tracing::info!("Removing stale ThreadBinding for window {} ('{}')", tb.window_id, tb.display_name);
        }
    }
    state.thread_bindings = resolved_bindings;

    // 3. Clean up display_names HashMap
    state.window_display_names.retain(|wid, _| windows.contains_key(wid) || state.window_states.contains_key(wid));

    state
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use aim_core::session::{ServerState, ThreadBinding, WindowState};
    use aim_tmux::manager::WindowInfo;

    #[test]
    fn test_re_resolve_all_alive() {
        let mut windows = HashMap::new();
        windows.insert("@0".into(), WindowInfo {
            window_id: aim_core::message::WindowId("@0".into()),
            name: "aim-100".into(),
            current_command: "claude".into(),
        });
        windows.insert("@1".into(), WindowInfo {
            window_id: aim_core::message::WindowId("@1".into()),
            name: "welcome".into(),
            current_command: "zsh".into(),
        });

        let state = ServerState {
            window_states: HashMap::from([
                ("@0".into(), WindowState { session_id: "sess_a".into(), cwd: "/home".into(), window_name: "aim-100".into() }),
            ]),
            thread_bindings: vec![
                ThreadBinding {
                    user_id: 100, thread_id: 1, chat_id: -100,
                    window_id: "@0".into(), display_name: "aim-100".into(),
                    group_chat_id: None, topic_name: None,
                },
            ],
            window_display_names: HashMap::from([("@0".into(), "aim-100".into())]),
            user_window_offsets: HashMap::new(),
        };

        let resolved = super::re_resolve_state(state, &windows);
        assert_eq!(resolved.window_states.len(), 1);
        assert!(resolved.window_states.contains_key("@0"));
        assert_eq!(resolved.thread_bindings.len(), 1);
        assert_eq!(resolved.thread_bindings[0].window_id, "@0");
    }

    #[test]
    fn test_re_resolve_stale_removed() {
        let windows = HashMap::new(); // no windows at all
        let state = ServerState {
            window_states: HashMap::from([
                ("@9".into(), WindowState { session_id: "sess_x".into(), cwd: "/tmp".into(), window_name: "ghost".into() }),
            ]),
            thread_bindings: vec![
                ThreadBinding {
                    user_id: 200, thread_id: 5, chat_id: -200,
                    window_id: "@9".into(), display_name: "ghost".into(),
                    group_chat_id: None, topic_name: None,
                },
            ],
            window_display_names: HashMap::from([("@9".into(), "ghost".into())]),
            user_window_offsets: HashMap::new(),
        };

        let resolved = super::re_resolve_state(state, &windows);
        assert_eq!(resolved.window_states.len(), 0);
        assert_eq!(resolved.thread_bindings.len(), 0);
        assert_eq!(resolved.window_display_names.len(), 0);
    }

    #[test]
    fn test_re_resolve_by_display_name() {
        let mut windows = HashMap::new();
        windows.insert("@42".into(), WindowInfo {
            window_id: aim_core::message::WindowId("@42".into()),
            name: "aim-300".into(),
            current_command: "claude".into(),
        });

        // Stale window_id "@old" — should be re-resolved to "@42" by matching display_name / window_name
        let state = ServerState {
            window_states: HashMap::from([
                ("@old".into(), WindowState { session_id: "sess_y".into(), cwd: "/projects".into(), window_name: "aim-300".into() }),
            ]),
            thread_bindings: vec![
                ThreadBinding {
                    user_id: 300, thread_id: 10, chat_id: -300,
                    window_id: "@old".into(), display_name: "aim-300".into(),
                    group_chat_id: None, topic_name: None,
                },
            ],
            window_display_names: HashMap::from([("@old".into(), "aim-300".into())]),
            user_window_offsets: HashMap::new(),
        };

        let resolved = super::re_resolve_state(state, &windows);
        // window_state re-keyed to @42
        assert_eq!(resolved.window_states.len(), 1);
        assert!(!resolved.window_states.contains_key("@old"));
        assert_eq!(resolved.window_states.get("@42").unwrap().session_id, "sess_y");

        // thread_binding updated
        assert_eq!(resolved.thread_bindings[0].window_id, "@42");

        // display_names cleaned up (old removed, but nothing added since the entry was old -> new)
        assert!(!resolved.window_display_names.contains_key("@old"));
    }

    #[test]
    fn test_re_resolve_partial_stale() {
        let mut windows = HashMap::new();
        windows.insert("@10".into(), WindowInfo {
            window_id: aim_core::message::WindowId("@10".into()),
            name: "alive".into(),
            current_command: "bash".into(),
        });

        let state = ServerState {
            window_states: HashMap::from([
                ("@10".into(), WindowState { session_id: "sess_keep".into(), cwd: "/a".into(), window_name: "alive".into() }),
                ("@99".into(), WindowState { session_id: "sess_dead".into(), cwd: "/b".into(), window_name: "dead".into() }),
            ]),
            thread_bindings: vec![
                ThreadBinding {
                    user_id: 1, thread_id: 1, chat_id: -1,
                    window_id: "@10".into(), display_name: "alive".into(),
                    group_chat_id: None, topic_name: None,
                },
                ThreadBinding {
                    user_id: 2, thread_id: 2, chat_id: -2,
                    window_id: "@99".into(), display_name: "dead".into(),
                    group_chat_id: None, topic_name: None,
                },
            ],
            window_display_names: HashMap::new(),
            user_window_offsets: HashMap::new(),
        };

        let resolved = super::re_resolve_state(state, &windows);
        assert_eq!(resolved.window_states.len(), 1);
        assert!(resolved.window_states.contains_key("@10"));
        assert_eq!(resolved.thread_bindings.len(), 1);
        assert_eq!(resolved.thread_bindings[0].window_id, "@10");
    }

    #[test]
    fn test_re_resolve_rekeys_window_states_when_name_matches() {
        let mut windows = HashMap::new();
        windows.insert("@new1".into(), WindowInfo {
            window_id: aim_core::message::WindowId("@new1".into()),
            name: "project-alpha".into(),
            current_command: "claude".into(),
        });

        let state = ServerState {
            window_states: HashMap::from([
                ("@old_stale".into(), WindowState {
                    session_id: "sess_alpha".into(),
                    cwd: "/alpha".into(),
                    window_name: "project-alpha".into(),
                }),
            ]),
            thread_bindings: vec![],
            window_display_names: HashMap::new(),
            user_window_offsets: HashMap::new(),
        };

        let resolved = super::re_resolve_state(state, &windows);
        assert!(!resolved.window_states.contains_key("@old_stale"));
        assert!(resolved.window_states.contains_key("@new1"));
        assert_eq!(resolved.window_states["@new1"].session_id, "sess_alpha");
    }
}
