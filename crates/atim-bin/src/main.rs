use std::path::PathBuf;
use std::sync::Arc;

use atim_core::agent::Agent;
use atim_core::config::Config;
use atim_core::im::ImAdapter;
use clap::{Parser, Subcommand};
use tokio::sync::{Mutex, mpsc};

mod browser;
mod hook;
mod router;
mod server;
mod service;

#[derive(Parser)]
#[command(name = "atim", about = "IM-to-Claude-Code bridge via tmux")]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    /// Claude Code SessionStart hook — registers session_id → window_id mappings
    Hook {
        /// Install the hook script to ~/.config/claude/hooks/SessionStart
        #[arg(long, short)]
        install: bool,
    },
    /// Manage atim as a systemd service (user-level by default)
    Service {
        /// Install the systemd service unit
        #[arg(long)]
        install: bool,
        /// Start the service
        #[arg(long)]
        start: bool,
        /// Stop the service
        #[arg(long)]
        stop: bool,
        /// Restart the service
        #[arg(long)]
        restart: bool,
        /// Show service status
        #[arg(long)]
        status: bool,
        /// Use system-level service instead of user-level (requires root)
        #[arg(long)]
        system: bool,
    },
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    rustls::crypto::ring::default_provider()
        .install_default()
        .expect("Failed to install rustls CryptoProvider");
    let cli = Cli::parse();

    match cli.command {
        Some(Command::Hook { install }) => {
            let cmd = if install {
                hook::HookCommand::Install
            } else {
                hook::HookCommand::Run
            };
            if let Err(e) = hook::run_hook(cmd) {
                eprintln!("atim hook error: {e}");
                std::process::exit(1);
            }
            return Ok(());
        }
        Some(Command::Service {
            install,
            start,
            stop,
            restart,
            status: _,
            system,
        }) => {
            let cmd = if install {
                service::ServiceCommand::Install
            } else if start {
                service::ServiceCommand::Start
            } else if stop {
                service::ServiceCommand::Stop
            } else if restart {
                service::ServiceCommand::Restart
            } else {
                service::ServiceCommand::Status
            };
            if let Err(e) = service::run_service(cmd, system) {
                eprintln!("atim service error: {e}");
                std::process::exit(1);
            }
            return Ok(());
        }
        None => {
            // Run the server (default)
        }
    }

    // 0. Load legacy .env (if present) for backward compatibility.
    if let Ok(home) = std::env::var("HOME") {
        let env_path = PathBuf::from(home).join(".atim").join(".env");
        if env_path.exists() {
            dotenvy::from_filename(&env_path).ok();
        }
    }

    // 1. Load config (now supports config.toml + env fallback)
    let config = Config::from_env()?;
    // Ensure config.toml exists (migrate from legacy .env if needed).
    let _ = atim_state::persistence::StateManager::ensure_config_toml(&config.atim_dir)?;

    // 2. Setup logging
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    tracing::info!(
        "Atim v{} starting — atim_dir={:?}",
        env!("CARGO_PKG_VERSION"),
        config.atim_dir
    );

    // 3. Load persisted state
    let state_mgr = atim_state::persistence::StateManager::open(&config.atim_dir).await?;

    let server_state = state_mgr.load_state().await?;
    let byte_offsets = state_mgr.load_monitor_offsets().await?;
    let byte_offsets = Arc::new(Mutex::new(byte_offsets));

    // 4. Ensure tmux session exists, then re-resolve stale window IDs
    let tmux_mgr = atim_tmux::manager::TmuxManager::new(&config.tmux_session_name);

    tmux_mgr.ensure_session().await?;

    let windows = tmux_mgr.window_map().await?;
    let mut resolved_state = re_resolve_state(server_state, &windows);
    state_mgr.save_state(&resolved_state).await?;
    state_mgr
        .clean_session_map(|window_id| windows.contains_key(window_id))
        .await?;

    // 4b. Sync known session_ids from window_states to session_map.
    //     This covers windows where the SessionStart hook didn't run
    //     (e.g. plugin-managed hooks, sessions started before hook install).
    if let Ok(mut map) = state_mgr.load_session_map().await {
        let mut changed = false;
        for (wid, ws) in &resolved_state.window_states {
            if !ws.session_id.is_empty() && !map.contains_key(wid) {
                map.insert(wid.clone(), ws.session_id.clone());
                changed = true;
                tracing::info!(
                    "Synced session {} to session_map for window {wid}",
                    ws.session_id
                );
            }
        }
        if changed {
            state_mgr.save_session_map(&map).await?;
        }
    }

    // 4c. Discover session_ids for windows with empty session_id
    //     (e.g. windows created before the SessionStart hook was installed)
    let empty_windows: Vec<(String, String)> = resolved_state
        .window_states
        .iter()
        .filter(|(_, ws)| ws.session_id.is_empty())
        .map(|(wid, ws)| (wid.clone(), ws.cwd.clone()))
        .collect();
    let mut known_ids: std::collections::HashSet<String> = resolved_state
        .window_states
        .values()
        .map(|ws| ws.session_id.clone())
        .filter(|s| !s.is_empty())
        .collect();
    for (wid, cwd) in &empty_windows {
        // Only discover session_ids for Claude Code windows — other agents
        // (Copilot, Codex) don't produce JSONL logs.
        let agent_type = resolved_state
            .window_states
            .get(wid)
            .map(|ws| ws.agent_type.as_str())
            .unwrap_or("");
        if agent_type != "claude" {
            tracing::debug!(
                "Window {wid} agent_type is '{agent_type}' — not Claude Code, skipping session discovery"
            );
            continue;
        }

        tracing::info!("No session_id for window {wid}, attempting to discover...");
        if let Some(sid) = discover_session_for_window(wid, cwd, &known_ids) {
            tracing::info!(
                "Discovered session {sid} for window {wid}, syncing to state and session_map"
            );
            if let Some(ws) = resolved_state.window_states.get_mut(wid) {
                ws.session_id = sid.clone();
            }
            known_ids.insert(sid.clone());
            if let Ok(mut map) = state_mgr.load_session_map().await {
                map.insert(wid.clone(), sid);
                let _ = state_mgr.save_session_map(&map).await;
            }
        }
    }
    state_mgr.save_state(&resolved_state).await?;

    // 5. Start the IM adapter
    let (im_tx, im_rx) = mpsc::unbounded_channel();

    let raw_adapter: Arc<dyn ImAdapter> = {
        let backend = config.im_backend.clone();
        match backend.as_str() {
            "feishu" => {
                let adapter = atim_im::feishu::FeishuAdapter::new(
                    config.feishu_app_id.clone(),
                    config.feishu_app_secret.clone(),
                    config.atim_dir.clone(),
                );
                Arc::new(adapter)
            }
            _ => {
                // telegram (default)
                let adapter =
                    atim_im::telegram::TelegramAdapter::new(config.telegram_bot_token.clone());
                Arc::new(adapter)
            }
        }
    };

    // Wrap with flood control for rate limiting
    let im_adapter: Arc<dyn ImAdapter> = Arc::new(
        atim_queue::flood_control::FloodControlledAdapter::new(raw_adapter.clone()),
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
    let monitor = atim_monitor::monitor::SessionMonitor::new(
        config.atim_dir.clone(),
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
    let queue = Arc::new(Mutex::new(atim_queue::message_queue::MessageQueue::new()));

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
        pending_agents: Arc::new(Mutex::new(std::collections::HashMap::new())),
        last_ui_states: Arc::new(Mutex::new(std::collections::HashMap::new())),
        last_pane_output: Arc::new(Mutex::new(std::collections::HashMap::new())),
        pending_rename_names: Arc::new(Mutex::new(std::collections::HashMap::new())),
        welcome_sent: Arc::new(Mutex::new(std::collections::HashSet::new())),
    };

    server.run(im_rx, &mut monitor_rx).await?;

    // 9. Graceful shutdown
    tracing::info!("Shutting down...");
    im_handle.abort();
    monitor_handle.abort();

    Ok(())
}

/// Discover a session ID for a tmux window using the Agent trait.
///
/// Tries lsof-based PID tracing first, then falls back to project-slug
/// matching against `~/.claude/projects/<slug>/`.
fn discover_session_for_window(
    window_id: &str,
    cwd: &str,
    known_ids: &std::collections::HashSet<String>,
) -> Option<String> {
    let agent = atim_core::agent::claude::ClaudeAgent;

    // Phase 1: trace the pane PID with lsof
    if let Ok(Some(sid)) = agent.discover_session_by_pid(window_id) {
        return Some(sid);
    }

    // Phase 2: fallback to project-slug matching
    if !cwd.is_empty() {
        tracing::info!(
            "lsof failed for window {window_id}, trying project-slug matching (cwd={cwd})"
        );
        if let Ok(Some(sid)) = agent.discover_session(cwd, known_ids) {
            return Some(sid);
        }
    }

    None
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
    mut state: atim_core::session::ServerState,
    windows: &std::collections::HashMap<String, atim_tmux::manager::WindowInfo>,
) -> atim_core::session::ServerState {
    // 1. Resolve window_states — re-key by matching window_name.
    //    Stale entries (window not in tmux) are KEPT so they can be
    //    resurrected later when a message arrives.
    let mut resolved_windows = std::collections::HashMap::new();
    for (wid, ws) in &state.window_states {
        if windows.contains_key(wid) {
            resolved_windows.insert(wid.clone(), ws.clone());
        } else if let Some((new_id, _)) =
            windows.iter().find(|(_, info)| info.name == ws.window_name)
        {
            resolved_windows.insert(new_id.clone(), ws.clone());
            tracing::info!(
                "Re-resolved window_state {wid} → {new_id} by name '{}'",
                ws.window_name
            );
        } else {
            tracing::info!(
                "Keeping stale window_state {wid} ('{}') for later resurrection",
                ws.window_name
            );
            resolved_windows.insert(wid.clone(), ws.clone());
        }
    }
    state.window_states = resolved_windows;

    // 2. Resolve thread_bindings — update window_id by matching display_name.
    //    Stale entries are KEPT for later resurrection.
    let mut resolved_bindings = Vec::new();
    for tb in &state.thread_bindings {
        if windows.contains_key(&tb.window_id) {
            resolved_bindings.push(tb.clone());
        } else if let Some((new_id, _)) = windows
            .iter()
            .find(|(_, info)| info.name == tb.display_name)
        {
            let mut updated = tb.clone();
            updated.window_id = new_id.clone();
            resolved_bindings.push(updated);
            tracing::info!(
                "Re-resolved ThreadBinding window_id {} → {} by display_name '{}'",
                tb.window_id,
                new_id,
                tb.display_name
            );
        } else {
            tracing::info!(
                "Keeping stale ThreadBinding for window {} ('{}') for later resurrection",
                tb.window_id,
                tb.display_name
            );
            resolved_bindings.push(tb.clone());
        }
    }
    state.thread_bindings = resolved_bindings;

    // 3. Keep all display_names entries — stale ones may be needed for resurrection
    state
        .window_display_names
        .retain(|wid, _| windows.contains_key(wid) || state.window_states.contains_key(wid));

    state
}

#[cfg(test)]
mod tests {
    use atim_core::session::{ServerState, ThreadBinding, WindowState};
    use atim_tmux::manager::WindowInfo;
    use std::collections::HashMap;

    #[test]
    fn test_re_resolve_all_alive() {
        let mut windows = HashMap::new();
        windows.insert(
            "@0".into(),
            WindowInfo {
                window_id: atim_core::message::WindowId("@0".into()),
                name: "atim-100".into(),
                current_command: "claude".into(),
            },
        );
        windows.insert(
            "@1".into(),
            WindowInfo {
                window_id: atim_core::message::WindowId("@1".into()),
                name: "welcome".into(),
                current_command: "zsh".into(),
            },
        );

        let state = ServerState {
            window_states: HashMap::from([(
                "@0".into(),
                WindowState {
                    session_id: "sess_a".into(),
                    cwd: "/home".into(),
                    window_name: "atim-100".into(),
                    agent_type: "claude".into(),
                },
            )]),
            thread_bindings: vec![ThreadBinding {
                user_id: 100,
                thread_id: 1,
                chat_id: -100,
                window_id: "@0".into(),
                display_name: "atim-100".into(),
                group_chat_id: None,
                topic_name: None,
            }],
            window_display_names: HashMap::from([("@0".into(), "atim-100".into())]),
            user_window_offsets: HashMap::new(),
        };

        let resolved = super::re_resolve_state(state, &windows);
        assert_eq!(resolved.window_states.len(), 1);
        assert!(resolved.window_states.contains_key("@0"));
        assert_eq!(resolved.thread_bindings.len(), 1);
        assert_eq!(resolved.thread_bindings[0].window_id, "@0");
    }

    #[test]
    fn test_re_resolve_stale_kept_for_resurrection() {
        let windows = HashMap::new(); // no windows at all
        let state = ServerState {
            window_states: HashMap::from([(
                "@9".into(),
                WindowState {
                    session_id: "sess_x".into(),
                    cwd: "/tmp".into(),
                    window_name: "ghost".into(),
                    agent_type: "claude".into(),
                },
            )]),
            thread_bindings: vec![ThreadBinding {
                user_id: 200,
                thread_id: 5,
                chat_id: -200,
                window_id: "@9".into(),
                display_name: "ghost".into(),
                group_chat_id: None,
                topic_name: None,
            }],
            window_display_names: HashMap::from([("@9".into(), "ghost".into())]),
            user_window_offsets: HashMap::new(),
        };

        let resolved = super::re_resolve_state(state, &windows);
        // Stale entries are KEPT for later resurrection
        assert_eq!(resolved.window_states.len(), 1);
        assert!(resolved.window_states.contains_key("@9"));
        assert_eq!(resolved.thread_bindings.len(), 1);
        assert_eq!(resolved.thread_bindings[0].window_id, "@9");
        assert!(resolved.window_display_names.contains_key("@9"));
    }

    #[test]
    fn test_re_resolve_by_display_name() {
        let mut windows = HashMap::new();
        windows.insert(
            "@42".into(),
            WindowInfo {
                window_id: atim_core::message::WindowId("@42".into()),
                name: "atim-300".into(),
                current_command: "claude".into(),
            },
        );

        // Stale window_id "@old" — should be re-resolved to "@42" by matching display_name / window_name
        let state = ServerState {
            window_states: HashMap::from([(
                "@old".into(),
                WindowState {
                    session_id: "sess_y".into(),
                    cwd: "/projects".into(),
                    window_name: "atim-300".into(),
                    agent_type: "claude".into(),
                },
            )]),
            thread_bindings: vec![ThreadBinding {
                user_id: 300,
                thread_id: 10,
                chat_id: -300,
                window_id: "@old".into(),
                display_name: "atim-300".into(),
                group_chat_id: None,
                topic_name: None,
            }],
            window_display_names: HashMap::from([("@old".into(), "atim-300".into())]),
            user_window_offsets: HashMap::new(),
        };

        let resolved = super::re_resolve_state(state, &windows);
        // window_state re-keyed to @42
        assert_eq!(resolved.window_states.len(), 1);
        assert!(!resolved.window_states.contains_key("@old"));
        assert_eq!(
            resolved.window_states.get("@42").unwrap().session_id,
            "sess_y"
        );

        // thread_binding updated
        assert_eq!(resolved.thread_bindings[0].window_id, "@42");

        // display_names cleaned up (old removed, but nothing added since the entry was old -> new)
        assert!(!resolved.window_display_names.contains_key("@old"));
    }

    #[test]
    fn test_re_resolve_partial_stale() {
        let mut windows = HashMap::new();
        windows.insert(
            "@10".into(),
            WindowInfo {
                window_id: atim_core::message::WindowId("@10".into()),
                name: "alive".into(),
                current_command: "bash".into(),
            },
        );

        let state = ServerState {
            window_states: HashMap::from([
                (
                    "@10".into(),
                    WindowState {
                        session_id: "sess_keep".into(),
                        cwd: "/a".into(),
                        window_name: "alive".into(),
                        agent_type: "claude".into(),
                    },
                ),
                (
                    "@99".into(),
                    WindowState {
                        session_id: "sess_dead".into(),
                        cwd: "/b".into(),
                        window_name: "dead".into(),
                        agent_type: "claude".into(),
                    },
                ),
            ]),
            thread_bindings: vec![
                ThreadBinding {
                    user_id: 1,
                    thread_id: 1,
                    chat_id: -1,
                    window_id: "@10".into(),
                    display_name: "alive".into(),
                    group_chat_id: None,
                    topic_name: None,
                },
                ThreadBinding {
                    user_id: 2,
                    thread_id: 2,
                    chat_id: -2,
                    window_id: "@99".into(),
                    display_name: "dead".into(),
                    group_chat_id: None,
                    topic_name: None,
                },
            ],
            window_display_names: HashMap::new(),
            user_window_offsets: HashMap::new(),
        };

        let resolved = super::re_resolve_state(state, &windows);
        // Live entry kept, stale entry also kept for resurrection
        assert_eq!(resolved.window_states.len(), 2);
        assert!(resolved.window_states.contains_key("@10"));
        assert!(resolved.window_states.contains_key("@99"));
        assert_eq!(resolved.thread_bindings.len(), 2);
    }

    #[test]
    fn test_re_resolve_rekeys_window_states_when_name_matches() {
        let mut windows = HashMap::new();
        windows.insert(
            "@new1".into(),
            WindowInfo {
                window_id: atim_core::message::WindowId("@new1".into()),
                name: "project-alpha".into(),
                current_command: "claude".into(),
            },
        );

        let state = ServerState {
            window_states: HashMap::from([(
                "@old_stale".into(),
                WindowState {
                    session_id: "sess_alpha".into(),
                    cwd: "/alpha".into(),
                    window_name: "project-alpha".into(),
                    agent_type: "claude".into(),
                },
            )]),
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
