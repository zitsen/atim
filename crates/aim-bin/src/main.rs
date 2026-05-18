use std::sync::Arc;

use aim_core::config::Config;
use aim_core::im::ImAdapter;
use tokio::sync::{mpsc, Mutex};

mod router;
mod server;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
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
    let mut resolved_state = server_state;
    resolved_state
        .window_states
        .retain(|id, _| windows.contains_key(id));
    state_mgr.save_state(&resolved_state).await?;
    state_mgr
        .clean_session_map(|window_id| windows.contains_key(window_id))
        .await?;

    // 5. Start the IM adapter
    let (im_tx, im_rx) = mpsc::unbounded_channel();

    let im_adapter: Arc<dyn ImAdapter> = {
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

    let im_handle = {
        let adapter = im_adapter.clone();
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
    };

    server.run(im_rx, &mut monitor_rx).await?;

    // 9. Graceful shutdown
    tracing::info!("Shutting down...");
    im_handle.abort();
    monitor_handle.abort();

    Ok(())
}
