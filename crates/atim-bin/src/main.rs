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
mod update;

#[derive(Parser)]
#[command(name = "atim", about = "IM-to-Claude-Code bridge via tmux", version)]
struct Cli {
    /// Set the log level (trace, debug, info, warn, error).
    /// RUST_LOG takes precedence if set; otherwise this controls the level.
    /// Also reads ATIM_LOG_LEVEL env var.
    #[arg(long = "log-level", short = 'l', default_value = "info")]
    log_level: String,
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
        /// Uninstall/remove the service
        #[arg(long)]
        uninstall: bool,
        /// Enable the service (daemon-reload + enable)
        #[arg(long)]
        enable: bool,
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
    /// Check for updates and install the latest version from GitHub
    Update,
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
            uninstall,
            enable,
            start,
            stop,
            restart,
            status,
            system,
        }) => {
            let mut cmds = Vec::new();
            if install {
                cmds.push(service::ServiceCommand::Install);
            }
            if uninstall {
                cmds.push(service::ServiceCommand::Uninstall);
            }
            if enable {
                cmds.push(service::ServiceCommand::Enable);
            }
            if start {
                cmds.push(service::ServiceCommand::Start);
            }
            if stop {
                cmds.push(service::ServiceCommand::Stop);
            }
            if restart {
                cmds.push(service::ServiceCommand::Restart);
            }
            if status || cmds.is_empty() {
                cmds.push(service::ServiceCommand::Status);
            }
            if let Err(e) = service::run_service(&cmds, system) {
                eprintln!("atim service error: {e}");
                std::process::exit(1);
            }
            return Ok(());
        }
        Some(Command::Update) => {
            if let Err(e) = update::run_update().await {
                eprintln!("atim update error: {e}");
                std::process::exit(1);
            }
            return Ok(());
        }
        None => {
            // Run the server (default)
        }
    }

    // 0. Load legacy .env (if present) for backward compatibility.
    if let Some(home) = home::home_dir() {
        let env_path = home.join(".atim").join(".env");
        if env_path.exists() {
            dotenvy::from_filename(&env_path).ok();
        }
    }

    // 1. Load config (now supports config.toml + env fallback)
    let config = Config::from_env()?;
    // Ensure config.toml exists (migrate from legacy .env if needed).
    let _ = atim_state::persistence::StateManager::ensure_config_toml(&config.atim_dir)?;

    // 2. Setup logging
    // RUST_LOG takes precedence when set (per-module granularity via EnvFilter).
    // Otherwise --log-level / ATIM_LOG_LEVEL env controls the base level.
    // hyper_util is always suppressed to WARN unless explicitly overridden.
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| {
                let level =
                    std::env::var("ATIM_LOG_LEVEL").unwrap_or_else(|_| cli.log_level.clone());
                tracing_subscriber::EnvFilter::new(format!("{},hyper_util=warn", level))
            }),
        )
        .init();

    tracing::info!(
        "Atim v{} starting — atim_dir={:?}",
        env!("CARGO_PKG_VERSION"),
        config.atim_dir
    );

    // 3. Load persisted V2 runtime state
    let state_mgr = atim_state::persistence::StateManager::open(&config.atim_dir).await?;

    let mut runtime = state_mgr.load_runtime().await?;
    let byte_offsets = state_mgr.load_monitor_offsets().await?;
    let byte_offsets = Arc::new(Mutex::new(byte_offsets));

    // 4. Ensure terminal session exists, then rebuild window bindings.
    //    Linux/macOS: tmux CLI. Windows: psmux (native Windows tmux
    //    replacement, tmux-command-compatible — `winget install psmux`).
    use atim_core::terminal::TerminalManager;
    #[cfg(windows)]
    let tmux_mgr: std::sync::Arc<dyn TerminalManager> = std::sync::Arc::new(
        atim_tmux::manager::TmuxManager::new(&config.tmux_session_name).with_binary("psmux"),
    );
    #[cfg(not(windows))]
    let tmux_mgr: std::sync::Arc<dyn TerminalManager> = std::sync::Arc::new(
        atim_tmux::manager::TmuxManager::new(&config.tmux_session_name),
    );

    tmux_mgr.ensure_session().await?;

    let windows = tmux_mgr.window_map().await?;

    // 4a. Rebuild window_bindings — match by window_name since window_id (@id) is ephemeral.
    //     For each live tmux window, create or update its WindowBinding.
    //     Stale bindings (window gone from tmux) are kept for later resurrection.
    let mut new_bindings = std::collections::HashMap::new();
    for (wid, info) in &windows {
        let existing = runtime
            .window_bindings
            .values()
            .find(|wb| wb.window_name == info.name);
        match existing {
            Some(wb) => {
                // Window name matched — re-key to the current window_id.
                let updated = atim_core::session::WindowBinding {
                    window_id: wid.clone(),
                    session_id: wb.session_id.clone(),
                    cwd: wb.cwd.clone(),
                    agent_type: wb.agent_type.clone(),
                    window_name: wb.window_name.clone(),
                };
                new_bindings.insert(wid.clone(), updated);
            }
            None => {
                // New window — create a fresh binding.
                tracing::info!(
                    "Discovered new tmux window {wid} ('{}', cmd={}), creating binding",
                    info.name,
                    info.current_command
                );
                let agent_type = classify_agent_type(&info.current_command);
                new_bindings.insert(
                    wid.clone(),
                    atim_core::session::WindowBinding {
                        window_id: wid.clone(),
                        session_id: String::new(),
                        cwd: String::new(),
                        agent_type,
                        window_name: info.name.clone(),
                    },
                );
            }
        }
    }
    // Preserve stale bindings (windows no longer in tmux) for later resurrection.
    for (wid, wb) in &runtime.window_bindings {
        if !windows.contains_key(wid) && !new_bindings.contains_key(wid) {
            tracing::info!(
                "Keeping stale binding for window {wid} ('{}') for later resurrection",
                wb.window_name
            );
            new_bindings.insert(wid.clone(), wb.clone());
        }
    }
    runtime.window_bindings = new_bindings;

    // 4b. Discover session_ids for windows with empty session_id
    //     (e.g. windows created before the SessionStart hook was installed)
    let empty_windows: Vec<(String, String)> = runtime
        .window_bindings
        .iter()
        .filter(|(_, wb)| wb.session_id.is_empty())
        .map(|(wid, wb)| (wid.clone(), wb.cwd.clone()))
        .collect();
    let mut known_ids: std::collections::HashSet<String> = runtime
        .window_bindings
        .values()
        .map(|wb| wb.session_id.clone())
        .filter(|s| !s.is_empty())
        .collect();
    for (wid, cwd) in &empty_windows {
        let agent_type = runtime
            .window_bindings
            .get(wid)
            .map(|wb| wb.agent_type.as_str())
            .unwrap_or("");
        if agent_type != "claude" {
            tracing::debug!(
                "Window {wid} agent_type is '{agent_type}' — not Claude Code, skipping session discovery"
            );
            continue;
        }

        tracing::info!("No session_id for window {wid}, attempting to discover...");
        if let Some(sid) = discover_session_for_window(wid, cwd, &known_ids) {
            tracing::info!("Discovered session {sid} for window {wid}, syncing to runtime");
            if let Some(wb) = runtime.window_bindings.get_mut(wid) {
                wb.session_id = sid.clone();
            }
            known_ids.insert(sid.clone());
            // Also ensure a session entry exists
            runtime.sessions.entry(sid.clone()).or_insert_with(|| {
                atim_core::session::SessionInfo {
                    session_id: sid.clone(),
                    cwd: cwd.clone(),
                    agent_type: "claude".to_string(),
                }
            });
        }
    }

    // 4c. Sync hook output — consume session_map.json
    let hook_map = state_mgr.consume_hook_session_map().await?;
    for (window_id, session_id) in &hook_map {
        // Update or create window binding for this window_id
        runtime
            .window_bindings
            .entry(window_id.clone())
            .and_modify(|wb| wb.session_id = session_id.clone())
            .or_insert_with(|| atim_core::session::WindowBinding {
                window_id: window_id.clone(),
                session_id: session_id.clone(),
                cwd: String::new(),
                agent_type: "claude".to_string(),
                window_name: String::new(),
            });
        // Ensure a session entry exists
        runtime
            .sessions
            .entry(session_id.clone())
            .or_insert_with(|| atim_core::session::SessionInfo {
                session_id: session_id.clone(),
                cwd: String::new(),
                agent_type: "claude".to_string(),
            });
    }

    // 4d. Sync chat_bindings' session_ids from window_bindings.
    //     Chat bindings may have an empty session_id even when the
    //     corresponding window binding already has one (e.g. after a
    //     restart where the SessionStart hook already fired, or if the
    //     session was discovered at startup).
    let mut synced_cb = 0;
    for cb in &mut runtime.chat_bindings {
        if !cb.session_id.is_empty() {
            continue;
        }
        // Try to find a window binding with a matching window_name
        if let Some(sid) = runtime
            .window_bindings
            .values()
            .find(|wb| !wb.session_id.is_empty() && wb.window_name == cb.display_name)
            .map(|wb| wb.session_id.clone())
        {
            tracing::info!(
                "Startup: syncing session_id {sid} to chat binding '{}' (user={} thread={})",
                cb.display_name,
                cb.user_id,
                cb.thread_id,
            );
            cb.session_id = sid;
            synced_cb += 1;
        }
    }
    if synced_cb > 0 {
        tracing::info!("Startup: synced {synced_cb} chat binding(s) from window bindings");
    }

    state_mgr.save_runtime(&runtime).await?;

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
        pending_chat_names: Arc::new(Mutex::new(std::collections::HashMap::new())),
        pending_rename_names: Arc::new(Mutex::new(std::collections::HashMap::new())),
        welcome_sent: Arc::new(Mutex::new(std::collections::HashSet::new())),
    };

    // 8. Enter main event loop — wait for Ctrl+C (SIGINT/CTRL_C_EVENT)
    // to gracefully shut down, regardless of platform.
    tokio::select! {
        result = server.run(im_rx, &mut monitor_rx) => {
            if let Err(e) = result {
                tracing::error!("Server exited with error: {e}");
            }
        }
        _ = tokio::signal::ctrl_c() => {
            tracing::info!("Received interrupt signal, shutting down...");
        }
    }

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

/// Classify the agent type based on the tmux pane's current command.
fn classify_agent_type(current_command: &str) -> String {
    match current_command {
        "claude" => "claude".to_string(),
        "code" | "copilot" => "copilot".to_string(),
        "codex" => "codex".to_string(),
        _ => current_command.to_string(),
    }
}
