use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use atim_core::agent::OutputSource;
use atim_core::agent::types::AgentHandle;
use atim_core::config::Config;
use atim_core::error::Result;
use atim_core::im::ImAdapter;
use atim_core::message::{
    Button, ChatId, ImEvent, ImEventKind, MessageId, MessageTarget, ThreadId, WindowId,
};
use atim_core::message::{InteractiveUi, UiKind};
use atim_core::session::{ThreadBinding, WindowState};
use atim_monitor::monitor::{MonitorEvent, resolve_jsonl};
use atim_queue::message_queue::MessageQueue;
use atim_state::persistence::StateManager;
use atim_tmux::manager::TmuxManager;
use regex::Regex;
use tokio::sync::Mutex;

use crate::browser;
use crate::browser::{BrowserMode, DirectoryBrowser};

/// Key type for per-user pending state: (user_id, chat_id, thread_id).
type UserTriple = (i64, i64, i64);
/// Key type for tool_use message tracking: (chat_id, thread_id, tool_use_id).
type ToolUseMsgKey = (i64, i64, String);

/// The main application server — routes IM events to tmux and monitor
/// events back to IM.
pub struct Server {
    pub config: Config,
    pub state_mgr: StateManager,
    pub tmux_mgr: TmuxManager,
    #[allow(dead_code)]
    pub queue: Arc<Mutex<MessageQueue>>,
    #[allow(dead_code)]
    pub byte_offsets: Arc<Mutex<HashMap<String, u64>>>,
    pub im_adapter: Arc<dyn ImAdapter>,
    /// Track topic names by (chat_id, thread_id) from forum_topic_created/edited.
    pub topic_names: Arc<Mutex<HashMap<(i64, i64), String>>>,
    /// Pending user messages awaiting callback selection: (user_id, chat_id, thread_id) -> text.
    pub pending_messages: Arc<Mutex<HashMap<UserTriple, String>>>,
    /// Callback context validation: token -> (user_id, chat_id, thread_id).
    pub callback_contexts: Arc<Mutex<HashMap<String, UserTriple>>>,
    /// Directory browser for session creation with project navigation.
    pub browser: DirectoryBrowser,
    /// Tool_use message tracking for in-place editing:
    /// key = (chat_id, thread_id, tool_use_id) -> message_id of the sent tool_use summary.
    pub tool_use_msg_ids: Arc<Mutex<HashMap<ToolUseMsgKey, MessageId>>>,
    /// Status message tracking for status→content conversion:
    /// key = (chat_id, thread_id) -> whether status has been consumed by first content.
    pub status_consumed: Arc<Mutex<HashSet<(i64, i64)>>>,
    /// Pending agent selection per (user_id, chat_id, thread_id) during setup workflow.
    pub pending_agents: Arc<Mutex<HashMap<UserTriple, String>>>,
    /// Interactive UI detection cache: window_id -> hash of last detected UI content.
    pub last_ui_states: Arc<Mutex<HashMap<String, String>>>,
    /// Last pane output per window (for non-Claude agents without JSONL logs).
    pub last_pane_output: Arc<Mutex<HashMap<String, String>>>,
    /// Pending rename names: (user_id, chat_id, thread_id) -> new chat_name.
    /// Set when the rename prompt is shown; consumed by the rename callback.
    pub pending_rename_names: Arc<Mutex<HashMap<UserTriple, String>>>,
}

/// Maximum Telegram message length for merged content.
const MAX_MSG_LEN: usize = 3800;

impl Server {
    /// Generate a short callback context token and store the validation context.
    ///
    /// Returns a hex token that can be embedded in callback_data.
    fn make_callback_token(
        contexts: &mut HashMap<String, (i64, i64, i64)>,
        user_id: i64,
        chat_id: i64,
        thread_id: i64,
    ) -> String {
        use std::hash::{Hash, Hasher};
        let counter = contexts.len() as u64;
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        user_id.hash(&mut hasher);
        chat_id.hash(&mut hasher);
        thread_id.hash(&mut hasher);
        counter.hash(&mut hasher);
        let token = format!("{:x}", hasher.finish());
        contexts.insert(token.clone(), (user_id, chat_id, thread_id));
        token
    }

    /// Validate a callback context token and extract the stored context.
    fn validate_callback_token(
        contexts: &mut HashMap<String, (i64, i64, i64)>,
        token: &str,
    ) -> Option<(i64, i64, i64)> {
        let ctx = contexts.remove(token)?;
        Some(ctx)
    }

    /// Run the main event loop, processing IM and monitor events.
    pub async fn run(
        &self,
        mut im_rx: tokio::sync::mpsc::UnboundedReceiver<ImEvent>,
        monitor_rx: &mut tokio::sync::mpsc::UnboundedReceiver<MonitorEvent>,
    ) -> Result<()> {
        // Periodically probe for deleted topics (every 60s)
        let mut probe_interval = tokio::time::interval(std::time::Duration::from_secs(60));
        probe_interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        // Periodically check for interactive UIs (every 5s)
        let mut ui_interval = tokio::time::interval(std::time::Duration::from_secs(5));
        ui_interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

        loop {
            tokio::select! {
                Some(event) = im_rx.recv() => {
                    if let Err(e) = self.handle_im_event(event).await {
                        tracing::error!("handle_im_event error: {e}");
                    }
                }
                Some(event) = monitor_rx.recv() => {
                    if let Err(e) = self.handle_monitor_event(event).await {
                        tracing::error!("handle_monitor_event error: {e}");
                    }
                }
                _ = probe_interval.tick() => {
                    if let Err(e) = self.probe_topic_deletions().await {
                        tracing::error!("probe_topic_deletions error: {e}");
                    }
                }
                _ = ui_interval.tick() => {
                    if let Err(e) = self.probe_interactive_uis().await {
                        tracing::error!("probe_interactive_uis error: {e}");
                    }
                }
                else => break,
            }
        }
        Ok(())
    }

    async fn handle_im_event(&self, event: ImEvent) -> Result<()> {
        tracing::debug!(
            "[Feishu] handle_im_event: user_id={:?} kind={}",
            event.user_id,
            event.kind.variant_name()
        );

        // Log full text for Text events to debug multi-line message routing
        if let ImEventKind::Text {
            ref text,
            is_mention,
            is_group,
        } = event.kind
        {
            tracing::debug!(
                "[Feishu] Text event: user_id={:?} chat_id={} thread_id={:?} is_mention={is_mention} is_group={is_group} text_len={} text_preview={:?}",
                event.user_id,
                event.target.chat_id.0,
                event.target.thread_id.map(|t| t.0),
                text.len(),
                &text.chars().take(120).collect::<String>(),
            );
        }

        // Check user authorization
        if !self.config.is_user_allowed(event.user_id.0) {
            tracing::warn!("Unauthorized user: {:?}", event.user_id);
            return Ok(());
        }

        match event.kind {
            ImEventKind::Text {
                text,
                is_mention,
                is_group,
            } => {
                self.handle_text_message(
                    event.target,
                    event.user_id.0,
                    &text,
                    is_mention,
                    is_group,
                )
                .await?;
            }
            ImEventKind::CallbackQuery {
                data,
                msg_id,
                callback_query_id,
            } => {
                self.handle_callback(
                    event.target,
                    event.user_id.0,
                    &data,
                    msg_id,
                    callback_query_id.as_deref(),
                )
                .await?;
            }
            ImEventKind::Photo { .. } => {
                tracing::info!("Photo message (not yet implemented)");
            }
            ImEventKind::Voice(data) => {
                if data.is_empty() {
                    tracing::warn!("Empty voice data, skipping");
                    return Ok(());
                }
                let status_msg = self
                    .im_adapter
                    .send_message(&event.target, "🎤 Transcribing voice message...")
                    .await;
                match transcribe_voice(
                    &self.config.openai_api_key,
                    &self.config.openai_base_url,
                    &data,
                )
                .await
                {
                    Ok(text) => {
                        if text.is_empty() {
                            if let Ok(ref mid) = status_msg {
                                let _ = self
                                    .im_adapter
                                    .edit_message(
                                        &event.target,
                                        mid,
                                        "🎤 Transcription returned empty text.",
                                    )
                                    .await;
                            }
                        } else {
                            if let Ok(ref mid) = status_msg {
                                let _ = self
                                    .im_adapter
                                    .edit_message(
                                        &event.target,
                                        mid,
                                        &format!("🎤 *Transcribed:*\n{text}"),
                                    )
                                    .await;
                            }
                        }
                    }
                    Err(e) => {
                        tracing::error!("Voice transcription failed: {e}");
                        if let Ok(ref mid) = status_msg {
                            let _ = self
                                .im_adapter
                                .edit_message(
                                    &event.target,
                                    mid,
                                    &format!("🎤 Transcription failed: {e}"),
                                )
                                .await;
                        }
                    }
                }
            }
            ImEventKind::TopicCreated { name } => {
                let mut names = self.topic_names.lock().await;
                names.insert(
                    (
                        event.target.chat_id.0,
                        event.target.thread_id.map(|t| t.0).unwrap_or(0),
                    ),
                    name,
                );
            }
            ImEventKind::TopicClosed => {
                self.handle_topic_closed(&event.target).await?;
            }
            ImEventKind::TopicEdited { new_name } => {
                self.handle_topic_edited(&event.target, &new_name).await?;
            }
        }

        Ok(())
    }

    async fn handle_monitor_event(&self, event: MonitorEvent) -> Result<()> {
        match event {
            MonitorEvent::NewMessages(messages) => {
                let state = self.state_mgr.load_state().await?;

                // Filter and group messages by session_id
                let mut by_session: HashMap<String, Vec<&atim_core::message::NewMessage>> =
                    HashMap::new();
                for msg in &messages {
                    tracing::debug!(
                        "[pipe] NewMessage: session_id={}, role={}, content_type={:?}, complete={}, text_len={}",
                        msg.session_id.0,
                        msg.role,
                        msg.content_type,
                        msg.is_complete,
                        msg.text.len(),
                    );

                    if msg.role != "assistant"
                        && msg.content_type != atim_core::message::ContentType::ToolResult
                    {
                        continue;
                    }
                    if !msg.is_complete {
                        continue;
                    }
                    if msg.text.trim().is_empty()
                        && msg.content_type != atim_core::message::ContentType::ToolResult
                    {
                        continue;
                    }
                    if msg.content_type == atim_core::message::ContentType::Thinking {
                        continue;
                    }
                    by_session
                        .entry(msg.session_id.0.clone())
                        .or_default()
                        .push(msg);
                }

                for (_sid, group) in &by_session {
                    // Resolve to window + binding
                    let window_id = match state
                        .window_states
                        .iter()
                        .find(|(_, ws)| ws.session_id == *_sid)
                    {
                        Some((wid, _)) => wid.clone(),
                        None => {
                            // Fallback: session_id not in window_states. This happens when
                            // Claude Code rotates sessions internally without triggering the
                            // SessionStart hook. Try to match by checking if the unknown
                            // session's JSONL shares a project directory with a known session.
                            let matched = self.match_unknown_session(_sid, &state).await;
                            match matched {
                                Some(wid) => {
                                    tracing::info!(
                                        "[pipe] Fallback: matched unknown session {_sid} to window {wid}",
                                    );
                                    wid
                                }
                                None => {
                                    tracing::warn!(
                                        "[pipe] No window_state for session_id={} (have {} window_states) and fallback failed",
                                        _sid,
                                        state.window_states.len(),
                                    );
                                    continue;
                                }
                            }
                        }
                    };

                    let binding = match state
                        .thread_bindings
                        .iter()
                        .rfind(|b| b.window_id == window_id)
                    {
                        Some(b) => b.clone(),
                        None => {
                            tracing::warn!(
                                "[pipe] No thread_binding for window_id={} (have {} bindings)",
                                window_id,
                                state.thread_bindings.len(),
                            );
                            continue;
                        }
                    };

                    let chat_id = binding.group_chat_id.unwrap_or(binding.chat_id);
                    let thread_id_val = binding.thread_id;
                    let target = MessageTarget {
                        chat_id: ChatId(chat_id),
                        thread_id: if thread_id_val != 0 {
                            Some(ThreadId(thread_id_val))
                        } else {
                            None
                        },
                        chat_name: None,
                    };

                    // ── P1.3 Status→Content + P1.4 Message Merging ──
                    //
                    // 1. If this batch has text entries and no status was sent yet
                    //    for this response cycle, send "Claude is working..."
                    // 2. Merge consecutive Text entries into a single message.
                    // 3. The first text flush edits the status message in-place.
                    // 4. ToolUse / ToolResult break the merge chain.

                    let has_text = group
                        .iter()
                        .any(|m| m.content_type == atim_core::message::ContentType::Text);
                    let status_key = (chat_id, thread_id_val);

                    let mut status_msg_id = if has_text {
                        let consumed = self.status_consumed.lock().await;
                        if !consumed.contains(&status_key) {
                            drop(consumed);
                            self.im_adapter
                                .send_message(&target, "🤖 Claude is working...")
                                .await
                                .ok()
                        } else {
                            drop(consumed);
                            None
                        }
                    } else {
                        None
                    };

                    let mut merged = String::new();

                    macro_rules! flush {
                        () => {
                            if !merged.is_empty() {
                                if let Some(mid) = status_msg_id.take() {
                                    // First content batch → edit status in-place
                                    if let Err(e) =
                                        self.im_adapter.edit_message(&target, &mid, &merged).await
                                    {
                                        tracing::error!("[pipe] Failed to edit status: {e}");
                                    }
                                    self.status_consumed.lock().await.insert(status_key);
                                } else {
                                    if let Err(e) =
                                        self.im_adapter.send_message(&target, &merged).await
                                    {
                                        tracing::error!("[pipe] Failed to send: {e}");
                                    }
                                }
                                merged.clear();
                            }
                        };
                    }

                    for msg in group {
                        tracing::info!(
                            "[pipe] chat={} thread={:?} content_type={:?} tool_use_id={:?}: {}",
                            target.chat_id.0,
                            target.thread_id,
                            msg.content_type,
                            msg.tool_use_id,
                            msg.text.chars().take(60).collect::<String>(),
                        );

                        use atim_core::message::ContentType;
                        match msg.content_type {
                            ContentType::ToolUse => {
                                flush!();
                                if let Some(tuid) = &msg.tool_use_id
                                    && let Ok(mid) =
                                        self.im_adapter.send_message(&target, &msg.text).await
                                {
                                    self.tool_use_msg_ids
                                        .lock()
                                        .await
                                        .insert((chat_id, thread_id_val, tuid.clone()), mid);
                                }
                            }
                            ContentType::ToolResult => {
                                flush!();
                                if let Some(tuid) = &msg.tool_use_id {
                                    let mut map = self.tool_use_msg_ids.lock().await;
                                    if let Some(mid) =
                                        map.remove(&(chat_id, thread_id_val, tuid.clone()))
                                    {
                                        let _ = self
                                            .im_adapter
                                            .edit_message(&target, &mid, &msg.text)
                                            .await;
                                    } else {
                                        let _ =
                                            self.im_adapter.send_message(&target, &msg.text).await;
                                    }
                                } else {
                                    let _ = self.im_adapter.send_message(&target, &msg.text).await;
                                }
                            }
                            _ => {
                                // Text — convert tables, then accumulate for merging
                                let converted = atim_parser::table::convert_tables(&msg.text);
                                if !merged.is_empty() {
                                    merged.push('\n');
                                }
                                merged.push_str(&converted);
                                if merged.len() >= MAX_MSG_LEN {
                                    flush!();
                                }
                            }
                        }
                    }

                    flush!();
                }
            }
            MonitorEvent::SessionMapChanged => {
                tracing::info!("[pipe] SessionMapChanged — syncing session IDs to window states");
                let session_map = self.state_mgr.load_session_map().await?;
                let mut state = self.state_mgr.load_state().await?;
                let mut synced = 0;
                for (window_id, session_id) in &session_map {
                    if let Some(ws) = state.window_states.get_mut(window_id) {
                        // Only assign session_ids for agents that support
                        // tracked sessions — skip agents with no JSONL logs.
                        if let Some(agent) = self.config.agent_registry.get(&ws.agent_type)
                            && !agent.supports_sessions()
                        {
                            continue;
                        }
                        if ws.session_id.is_empty() {
                            ws.session_id = session_id.clone();
                            synced += 1;
                            tracing::info!(
                                "[pipe] Assigned session {session_id} to window {window_id}"
                            );
                        } else if ws.session_id != *session_id {
                            tracing::debug!(
                                "[pipe] Window {window_id} has session {} but map says {session_id} — updating",
                                ws.session_id,
                            );
                            ws.session_id = session_id.clone();
                            synced += 1;
                        }
                    } else {
                        tracing::warn!(
                            "[pipe] Session map has window {window_id} but no WindowState exists for it",
                        );
                    }
                }
                tracing::info!("[pipe] SessionMapChanged: synced {synced} entries");
                self.state_mgr.save_state(&state).await?;
            }
        }
        Ok(())
    }

    async fn handle_text_message(
        &self,
        target: MessageTarget,
        user_id: i64,
        text: &str,
        is_mention: bool,
        is_group: bool,
    ) -> Result<()> {
        // Load current state to find thread binding
        let state = self.state_mgr.load_state().await?;

        // Check for screenshot command
        if text.trim() == "/ss" || text.trim() == "/screenshot" || text.trim() == "!ss" {
            if let Some(binding) = state.thread_bindings.iter().find(|b| {
                b.user_id == user_id && b.thread_id == target.thread_id.map(|t| t.0).unwrap_or(0)
            }) {
                let window_id = atim_core::message::WindowId(binding.window_id.clone());
                if self.tmux_mgr.window_exists(&window_id).await {
                    let _ = self.im_adapter.send_chat_action(&target).await;
                    match self.tmux_mgr.screenshot(&window_id).await {
                        Ok(png_data) => {
                            tracing::info!("Screenshot generated: {} bytes", png_data.len());
                            if let Err(e) = self
                                .im_adapter
                                .send_photo(&target, "terminal.png", &png_data)
                                .await
                            {
                                tracing::error!("Failed to send screenshot: {e}");
                            }
                        }
                        Err(e) => {
                            let msg = format!("Screenshot failed: {e}");
                            let _ = self.im_adapter.send_message(&target, &msg).await;
                        }
                    }
                } else {
                    let _ = self
                        .im_adapter
                        .send_message(&target, "Window no longer exists.")
                        .await;
                }
            } else {
                let _ = self
                    .im_adapter
                    .send_message(&target, "No active session to capture.")
                    .await;
            }
            return Ok(());
        }

        // Check for /usage command
        if text.trim() == "/usage" {
            if let Some(binding) = state.thread_bindings.iter().find(|b| {
                b.user_id == user_id && b.thread_id == target.thread_id.map(|t| t.0).unwrap_or(0)
            }) {
                let window_id = atim_core::message::WindowId(binding.window_id.clone());
                if !self.tmux_mgr.window_exists(&window_id).await {
                    let _ = self
                        .im_adapter
                        .send_message(&target, "Window no longer exists.")
                        .await;
                    return Ok(());
                }

                let _ = self.im_adapter.send_chat_action(&target).await;
                let _ = self
                    .im_adapter
                    .send_message(&target, "Fetching usage info...")
                    .await;

                // Send /usage to the agent
                self.tmux_mgr.send_line(&window_id, "/usage").await?;
                // Wait for modal to render
                tokio::time::sleep(std::time::Duration::from_millis(2000)).await;

                // Capture pane content
                match self.tmux_mgr.capture_pane(&window_id).await {
                    Ok(raw) => {
                        let parsed = parse_usage_output(&raw);
                        let _ = self.im_adapter.send_message(&target, &parsed).await;
                    }
                    Err(e) => {
                        let msg = format!("Failed to capture usage: {e}");
                        let _ = self.im_adapter.send_message(&target, &msg).await;
                    }
                }

                // Dismiss the modal
                let _ = self.tmux_mgr.send_key(&window_id, "q").await;
            } else {
                let _ = self
                    .im_adapter
                    .send_message(&target, "No active session.")
                    .await;
            }
            return Ok(());
        }

        // Handle /switch <agent> — switch the active agent at runtime
        if text.trim() == "/switch" || text.trim().starts_with("/switch ") {
            let agent_name: Option<String> = text
                .trim()
                .strip_prefix("/switch ")
                .map(|s| s.trim().to_lowercase());
            let agent_name = match agent_name {
                Some(ref n) if !n.is_empty() => n.clone(),
                _ => {
                    let available: Vec<&str> = self
                        .config
                        .agent_registry
                        .iter()
                        .map(|a| a.name())
                        .collect();
                    let _ = self
                        .im_adapter
                        .send_message(
                            &target,
                            &format!("Available agents: {}", available.join(", ")),
                        )
                        .await;
                    return Ok(());
                }
            };

            let agent = match self.config.agent_registry.get(&agent_name) {
                Some(a) => a.clone(),
                None => {
                    let available: Vec<&str> = self
                        .config
                        .agent_registry
                        .iter()
                        .map(|a| a.name())
                        .collect();
                    let _ = self
                        .im_adapter
                        .send_message(
                            &target,
                            &format!(
                                "Unknown agent '{agent_name}'. Available: {}",
                                available.join(", ")
                            ),
                        )
                        .await;
                    return Ok(());
                }
            };

            if let Some(binding) = state.thread_bindings.iter().find(|b| {
                b.user_id == user_id && b.thread_id == target.thread_id.map(|t| t.0).unwrap_or(0)
            }) {
                let window_id = WindowId(binding.window_id.clone());
                if !self.tmux_mgr.window_exists(&window_id).await {
                    let _ = self
                        .im_adapter
                        .send_message(&target, "Window no longer exists.")
                        .await;
                    return Ok(());
                }

                // Graceful shutdown of current agent
                self.tmux_mgr.send_key(&window_id, "C-c").await?;
                tokio::time::sleep(Duration::from_millis(500)).await;

                // Wait for agent to stop (shell prompt appears)
                let mut stopped = false;
                for _ in 0..10 {
                    tokio::time::sleep(Duration::from_millis(500)).await;
                    if let Ok(info) = self.tmux_mgr.find_window(&window_id).await
                        && is_shell_process(&info.current_command)
                    {
                        stopped = true;
                        break;
                    }
                }
                if !stopped {
                    tracing::warn!(
                        "/switch: agent in window {} did not stop within 5s, proceeding anyway",
                        window_id.0
                    );
                }

                // Launch new agent
                let launch_cmd = agent_launch_cmd(&agent);
                self.tmux_mgr.send_line(&window_id, &launch_cmd).await?;

                // Wait for agent to start
                let mut started = false;
                for _ in 0..10 {
                    tokio::time::sleep(Duration::from_millis(500)).await;
                    if let Ok(info) = self.tmux_mgr.find_window(&window_id).await
                        && !is_shell_process(&info.current_command)
                    {
                        started = true;
                        break;
                    }
                }
                if !started {
                    tracing::warn!(
                        "/switch: new agent in window {} did not start within 5s",
                        window_id.0
                    );
                }

                // Update window state
                let mut new_state = self.state_mgr.load_state().await?;
                if let Some(ws) = new_state.window_states.get_mut(&binding.window_id) {
                    ws.agent_type = agent.name().to_string();
                    ws.session_id = String::new();
                }
                self.state_mgr.save_state(&new_state).await?;

                // Also remove the old session_id from session_map so that
                // the next SessionMapChanged event won't re-fill it.
                if let Ok(mut map) = self.state_mgr.load_session_map().await
                    && map.remove(&binding.window_id).is_some()
                {
                    let _ = self.state_mgr.save_session_map(&map).await;
                }

                let _ = self
                    .im_adapter
                    .send_message(&target, &format!("Switched to **{}**.", agent.name()))
                    .await;
            } else {
                let _ = self
                    .im_adapter
                    .send_message(&target, "No active session to switch.")
                    .await;
            }
            return Ok(());
        }

        // Check for /esc (send Escape key to dismiss modals/help screens)
        if text.trim() == "/esc" || text.trim() == "/dismiss" {
            if let Some(binding) = state.thread_bindings.iter().find(|b| {
                b.user_id == user_id && b.thread_id == target.thread_id.map(|t| t.0).unwrap_or(0)
            }) {
                let window_id = atim_core::message::WindowId(binding.window_id.clone());
                if !self.tmux_mgr.window_exists(&window_id).await {
                    let _ = self
                        .im_adapter
                        .send_message(&target, "Window no longer exists.")
                        .await;
                    return Ok(());
                }
                let _ = self.im_adapter.send_chat_action(&target).await;
                self.tmux_mgr.send_key(&window_id, "Escape").await?;
                let _ = self
                    .im_adapter
                    .send_message(&target, "Sent Escape key.")
                    .await;
            } else {
                let _ = self
                    .im_adapter
                    .send_message(&target, "No active session.")
                    .await;
            }
            return Ok(());
        }

        // Handle /rebind — detect running agent and session, then (re)bind exclusively
        if text.trim() == "/rebind" {
            if let Some(binding) = state.thread_bindings.iter().find(|b| {
                b.user_id == user_id && b.thread_id == target.thread_id.map(|t| t.0).unwrap_or(0)
            }) {
                let window_id = atim_core::message::WindowId(binding.window_id.clone());
                let win_info = match self.tmux_mgr.find_window(&window_id).await {
                    Ok(info) => info,
                    Err(e) => {
                        let _ = self
                            .im_adapter
                            .send_message(&target, &format!("Window dead: {e}"))
                            .await;
                        return Ok(());
                    }
                };

                let mut changes: Vec<String> = Vec::new();

                // Detect running agent
                let detected_agent = self.detect_running_agent(&win_info.current_command);
                let stored_agent = state
                    .window_states
                    .get(&binding.window_id)
                    .map(|ws| ws.agent_type.as_str());

                let agent_type = if let Some(da) = detected_agent {
                    if stored_agent != Some(da) {
                        changes.push(format!("agent: {} → {}", stored_agent.unwrap_or("?"), da));
                    }
                    da
                } else {
                    stored_agent.unwrap_or("claude")
                };

                let agent = self
                    .config
                    .agent_registry
                    .get(agent_type)
                    .cloned()
                    .unwrap_or_else(|| self.config.agent_registry.default().clone());

                // Non-disruptive session discovery (no commands sent to the agent):
                // 1. PID discovery via lsof (most reliable — traces actual open file handles)
                // 2. Read existing pane output for a UUID (may contain stale text)
                // 3. Existing session_map entry as last resort
                let discovered_sid: Option<String> = if agent.supports_sessions() {
                    if let Ok(Some(sid)) = agent.discover_session_by_pid(&binding.window_id) {
                        Some(sid)
                    } else if let Some(from_pane) = self
                        .tmux_mgr
                        .capture_pane(&window_id)
                        .await
                        .ok()
                        .and_then(|t| {
                            let re = Regex::new(
                                r"[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}",
                            )
                            .ok()?;
                            re.find_iter(&t).next().map(|m| m.as_str().to_string())
                        })
                    {
                        Some(from_pane)
                    } else if let Ok(map) = self.state_mgr.load_session_map().await {
                        map.get(&binding.window_id).cloned()
                    } else {
                        None
                    }
                } else {
                    None
                };

                let stored_sid = state
                    .window_states
                    .get(&binding.window_id)
                    .map(|ws| ws.session_id.as_str())
                    .unwrap_or("");

                // Exclusive session check: if the discovered session is already
                // bound to a DIFFERENT window, warn and steal the binding.
                if let Some(ref sid) = discovered_sid {
                    for other_binding in &state.thread_bindings {
                        if other_binding.window_id == binding.window_id {
                            continue;
                        }
                        if let Some(other_ws) = state.window_states.get(&other_binding.window_id)
                            && other_ws.session_id == *sid
                        {
                            let _ = self
                                    .im_adapter
                                    .send_message(
                                        &target,
                                        &format!(
                                            "⚠️ Session {sid} was bound to {} (window {}) — rebinding to this window.",
                                            other_binding.display_name,
                                            other_binding.window_id,
                                        ),
                                    )
                                    .await;
                            // Clear the old binding's session so monitor doesn't track it
                            let mut state = self.state_mgr.load_state().await?;
                            if let Some(old_ws) =
                                state.window_states.get_mut(&other_binding.window_id)
                            {
                                old_ws.session_id = String::new();
                            }
                            self.state_mgr.save_state(&state).await?;
                            break;
                        }
                    }
                }

                // Update window_state
                let mut state = self.state_mgr.load_state().await?;
                if let Some(ws) = state.window_states.get_mut(&binding.window_id) {
                    if ws.agent_type != agent_type {
                        ws.agent_type = agent_type.to_string();
                    }
                    if let Some(ref sid) = discovered_sid
                        && ws.session_id != *sid
                    {
                        changes.push(format!(
                            "session: {} → {}",
                            if stored_sid.is_empty() {
                                "none"
                            } else {
                                stored_sid
                            },
                            sid
                        ));
                        ws.session_id = sid.clone();
                    }
                    self.state_mgr.save_state(&state).await?;
                }

                // Sync session_map if we found a session_id
                if let Some(ref sid) = discovered_sid {
                    if let Ok(mut map) = self.state_mgr.load_session_map().await
                        && map.get(&binding.window_id).map(|s| s.as_str()) != Some(sid)
                    {
                        map.insert(binding.window_id.clone(), sid.clone());
                        let _ = self.state_mgr.save_session_map(&map).await;
                    }
                    // Reset byte offset so monitor re-reads the full log
                    {
                        let mut offsets = self.byte_offsets.lock().await;
                        if offsets.remove(sid).is_some() {
                            changes.push("monitor offset reset".into());
                        }
                    }
                }

                if changes.is_empty() {
                    let sid_info = if stored_sid.is_empty() {
                        "no session".to_string()
                    } else {
                        stored_sid.to_string()
                    };
                    let _ = self
                        .im_adapter
                        .send_message(
                            &target,
                            &format!(
                                "✅ Binding is current: agent={agent_type} session={sid_info}"
                            ),
                        )
                        .await;
                } else {
                    let _ = self
                        .im_adapter
                        .send_message(&target, &format!("🔄 Rebound: {}", changes.join(", ")))
                        .await;
                }
            } else {
                let _ = self
                    .im_adapter
                    .send_message(&target, "No active session.")
                    .await;
            }
            return Ok(());
        }

        // Check for ! command capture
        if let Some(cmd) = text.strip_prefix('!') {
            let cmd = cmd.trim();
            if !cmd.is_empty() && cmd != "ss" {
                if let Some(binding) = state.thread_bindings.iter().find(|b| {
                    b.user_id == user_id
                        && b.thread_id == target.thread_id.map(|t| t.0).unwrap_or(0)
                }) {
                    let window_id = atim_core::message::WindowId(binding.window_id.clone());
                    if !self.tmux_mgr.window_exists(&window_id).await {
                        let _ = self
                            .im_adapter
                            .send_message(&target, "Window no longer exists.")
                            .await;
                        return Ok(());
                    }
                    let _ = self.im_adapter.send_chat_action(&target).await;

                    // Send the command (without ! prefix)
                    self.tmux_mgr.send_line(&window_id, cmd).await?;

                    // Spawn background capture loop
                    let tmux = self.tmux_mgr.clone();
                    let im = self.im_adapter.clone();
                    let wid = binding.window_id.clone();
                    let tgt = target.clone();

                    tokio::spawn(async move {
                        if let Err(e) = run_capture_loop(tmux, im, wid, tgt).await {
                            tracing::error!("! command capture failed: {e}");
                        }
                    });
                } else {
                    let _ = self
                        .im_adapter
                        .send_message(&target, "No active session.")
                        .await;
                }
            }
            return Ok(());
        }

        // If user has an active directory browsing session, use `z` for text-based navigation.
        // If zoxide doesn't match, return early with feedback rather than falling through
        // to the no-binding flow (which would show the agent picker again).
        if let Some(browser_state) = self.browser.get_state(user_id).await
            && browser_state.mode == crate::browser::BrowserMode::Browsing
            && !text.trim().is_empty()
        {
            let trimmed = text.trim();
            match zoxide_query(trimmed).await {
                Ok(Some(matched_path)) if matched_path.is_dir() => {
                    self.browser.navigate_to(user_id, &matched_path).await;
                    let thread_id = target.thread_id.map(|t| t.0).unwrap_or(0);
                    let _ = self
                        .send_browser_keyboard(&target, user_id, thread_id)
                        .await;
                    return Ok(());
                }
                Ok(Some(path)) => {
                    // zoxide matched but path isn't a directory
                    let _ = self
                        .im_adapter
                        .send_message(
                            &target,
                            &format!(
                                "zoxide matched '{}' but it is not a directory.",
                                path.display()
                            ),
                        )
                        .await;
                    return Ok(());
                }
                Ok(None) => {
                    // Try resolving as a filesystem path
                    let path = Path::new(trimmed);
                    if path.is_absolute() && path.is_dir() {
                        self.browser.navigate_to(user_id, path).await;
                        let thread_id = target.thread_id.map(|t| t.0).unwrap_or(0);
                        let _ = self
                            .send_browser_keyboard(&target, user_id, thread_id)
                            .await;
                    } else {
                        let _ = self
                            .im_adapter
                            .send_message(
                                &target,
                                &format!(
                                    "No zoxide match for '{}'. Use the directory buttons to navigate, or try a different name.",
                                    trimmed,
                                ),
                            )
                            .await;
                    }
                    return Ok(());
                }
                Err(e) => {
                    tracing::warn!("zoxide query failed: {e}");
                    let _ = self
                        .im_adapter
                        .send_message(
                            &target,
                            &format!(
                                "zoxide lookup failed: {e}. Use the directory buttons to navigate instead."
                            ),
                        )
                        .await;
                    return Ok(());
                }
            }
        }

        // Find binding for this user+thread
        if let Some(binding) = state.thread_bindings.iter().find(|b| {
            b.user_id == user_id && b.thread_id == target.thread_id.map(|t| t.0).unwrap_or(0)
        }) {
            let agent_type = state
                .window_states
                .get(&binding.window_id)
                .map(|ws| {
                    format!(
                        "agent_type={} session_id={}",
                        ws.agent_type,
                        if ws.session_id.is_empty() {
                            "none"
                        } else {
                            &ws.session_id
                        }
                    )
                })
                .unwrap_or_else(|| "no_window_state".into());
            tracing::debug!(
                "[handle_text_message] user={user_id} group_chat_id={} chat_name={:?} window={} display_name={} {} text={text:?}",
                binding.group_chat_id.unwrap_or(binding.chat_id),
                target.chat_name,
                binding.window_id,
                binding.display_name,
                agent_type,
            );
            // Forward to existing window
            let window_id = atim_core::message::WindowId(binding.window_id.clone());

            // Verify window is alive and agent is actually running
            match self.tmux_mgr.find_window(&window_id).await {
                Err(e) => {
                    // Window died — prompt user to recover or create new session
                    tracing::warn!(
                        "[handle_text_message] Window {} died ({e}), prompting user {}",
                        binding.window_id,
                        user_id
                    );
                    let thread_id = target.thread_id.map(|t| t.0).unwrap_or(0);
                    let key = (user_id, target.chat_id.0, thread_id);
                    {
                        let mut pending = self.pending_messages.lock().await;
                        pending.insert(key, text.to_string());
                    }
                    let mut ctx_lock = self.callback_contexts.lock().await;
                    let recover_token = Self::make_callback_token(
                        &mut ctx_lock,
                        user_id,
                        target.chat_id.0,
                        thread_id,
                    );
                    let cancel_token = Self::make_callback_token(
                        &mut ctx_lock,
                        user_id,
                        target.chat_id.0,
                        thread_id,
                    );
                    let buttons = vec![
                        vec![Button {
                            text: "🔄 Recover Session".into(),
                            callback_data: format!("cb:{recover_token}:recover"),
                        }],
                        vec![
                            Button {
                                text: "🆕 New Session".into(),
                                callback_data: format!("cb:{recover_token}:new"),
                            },
                            Button {
                                text: "❌ Cancel".into(),
                                callback_data: format!("cb:{cancel_token}:lifecycle_cancel"),
                            },
                        ],
                    ];
                    drop(ctx_lock);
                    let _ = self
                        .im_adapter
                        .send_keyboard(
                            &target,
                            "Session no longer available. What would you like to do?",
                            &buttons,
                        )
                        .await;
                    return Ok(());
                }
                Ok(info) if info.name != binding.display_name => {
                    // tmux renumbered or window repurposed — treat as dead window
                    tracing::warn!(
                        "[handle_text_message] Window {} name mismatch: expected='{}' actual='{}' (tmux renumbering?), prompting user {}",
                        binding.window_id,
                        binding.display_name,
                        info.name,
                        user_id
                    );
                    let thread_id = target.thread_id.map(|t| t.0).unwrap_or(0);
                    let key = (user_id, target.chat_id.0, thread_id);
                    {
                        let mut pending = self.pending_messages.lock().await;
                        pending.insert(key, text.to_string());
                    }
                    let mut ctx_lock = self.callback_contexts.lock().await;
                    let recover_token = Self::make_callback_token(
                        &mut ctx_lock,
                        user_id,
                        target.chat_id.0,
                        thread_id,
                    );
                    let cancel_token = Self::make_callback_token(
                        &mut ctx_lock,
                        user_id,
                        target.chat_id.0,
                        thread_id,
                    );
                    let buttons = vec![
                        vec![Button {
                            text: "🔄 Recover Session".into(),
                            callback_data: format!("cb:{recover_token}:recover"),
                        }],
                        vec![
                            Button {
                                text: "🆕 New Session".into(),
                                callback_data: format!("cb:{recover_token}:new"),
                            },
                            Button {
                                text: "❌ Cancel".into(),
                                callback_data: format!("cb:{cancel_token}:lifecycle_cancel"),
                            },
                        ],
                    ];
                    drop(ctx_lock);
                    let _ = self
                        .im_adapter
                        .send_keyboard(
                            &target,
                            "Session no longer available. What would you like to do?",
                            &buttons,
                        )
                        .await;
                    return Ok(());
                }
                Ok(info) if is_shell_process(&info.current_command) => {
                    tracing::info!(
                        "[handle_text_message] window={} shell process '{}'",
                        binding.window_id,
                        info.current_command,
                    );
                    // Re-launch the agent if it exited to shell, then send the text.
                    let agent_type_name = state
                        .window_states
                        .get(&binding.window_id)
                        .map(|ws| ws.agent_type.as_str())
                        .unwrap_or("claude");
                    let agent = self
                        .config
                        .agent_registry
                        .get(agent_type_name)
                        .cloned()
                        .unwrap_or_else(|| self.config.agent_registry.default().clone());
                    tracing::info!(
                        "[handle_text_message] re-launching {} for window {}",
                        agent.name(),
                        binding.window_id,
                    );
                    let launch_cmd = agent_launch_cmd(&agent);
                    self.tmux_mgr.send_line(&window_id, &launch_cmd).await?;
                    // Poll until agent starts (up to 5s)
                    let mut started = false;
                    for _ in 0..10 {
                        tokio::time::sleep(Duration::from_millis(500)).await;
                        if let Ok(info2) = self.tmux_mgr.find_window(&window_id).await
                            && !is_shell_process(&info2.current_command)
                        {
                            started = true;
                            break;
                        }
                    }
                    if !started {
                        tracing::warn!(
                            "[handle_text_message] {} did not start within 5s for window {}, \
                             sending text anyway",
                            agent.name(),
                            binding.window_id,
                        );
                    }
                    // Extra delay for TUI agents (Copilot/Codex) so bubbletea can set up
                    if !agent.supports_sessions() {
                        tokio::time::sleep(Duration::from_millis(1500)).await;
                    }
                    // For session-based agents (Claude Code), wait for the SessionStart hook
                    // to register the new session_id, then sync to state.json so the monitor
                    // can track and route responses.
                    if agent.supports_sessions() {
                        for _ in 0..10 {
                            tokio::time::sleep(Duration::from_millis(500)).await;
                            if let Ok(map) = self.state_mgr.load_session_map().await
                                && map.get(&binding.window_id).map(|s| s.as_str())
                                    != state
                                        .window_states
                                        .get(&binding.window_id)
                                        .map(|ws| ws.session_id.as_str())
                            {
                                let mut new_state = self.state_mgr.load_state().await?;
                                if let Some(sid) = map.get(&binding.window_id)
                                    && let Some(ws) =
                                        new_state.window_states.get_mut(&binding.window_id)
                                {
                                    ws.session_id = sid.clone();
                                }
                                let _ = self.state_mgr.save_state(&new_state).await;
                                tracing::info!(
                                    "[handle_text_message] updated session_id for window {} after re-launch",
                                    binding.window_id,
                                );
                                break;
                            }
                        }
                    }
                    let is_copilot = state
                        .window_states
                        .get(&binding.window_id)
                        .map(|ws| ws.agent_type == "copilot")
                        .unwrap_or(false);
                    if is_copilot {
                        self.tmux_mgr.send_line_chars(&window_id, text, 10).await?;
                    } else {
                        self.tmux_mgr.send_line(&window_id, text).await?;
                    }
                    let sc_chat_id = binding.group_chat_id.unwrap_or(binding.chat_id);
                    self.status_consumed
                        .lock()
                        .await
                        .remove(&(sc_chat_id, binding.thread_id));
                    return Ok(());
                }
                Ok(info) => {
                    // Agent is running — check for mismatches before forwarding
                    tracing::info!(
                        "[handle_text_message] window={} process='{}' sending text len={}",
                        binding.window_id,
                        info.current_command,
                        text.len(),
                    );

                    // 3.2 Chat name mismatch — IM chat_name differs from binding display_name
                    let thread_id = target.thread_id.map(|t| t.0).unwrap_or(0);
                    if let Some(ref chat_name) = target.chat_name
                        && chat_name != &binding.display_name
                    {
                        tracing::info!(
                            "[handle_text_message] Chat name mismatch: display_name='{}' chat_name='{}', prompting user {}",
                            binding.display_name,
                            chat_name,
                            user_id,
                        );
                        let key = (user_id, target.chat_id.0, thread_id);
                        {
                            let mut pending = self.pending_messages.lock().await;
                            pending.insert(key, text.to_string());
                        }
                        // Store the new name so the rename callback can use it
                        self.pending_rename_names
                            .lock()
                            .await
                            .insert(key, chat_name.clone());
                        let mut ctx_lock = self.callback_contexts.lock().await;
                        let rename_token = Self::make_callback_token(
                            &mut ctx_lock,
                            user_id,
                            target.chat_id.0,
                            thread_id,
                        );
                        let cancel_token = Self::make_callback_token(
                            &mut ctx_lock,
                            user_id,
                            target.chat_id.0,
                            thread_id,
                        );
                        let buttons = vec![
                            vec![Button {
                                text: "📝 Rename".into(),
                                callback_data: format!("cb:{rename_token}:rename"),
                            }],
                            vec![
                                Button {
                                    text: "🆕 New Session".into(),
                                    callback_data: format!("cb:{rename_token}:new"),
                                },
                                Button {
                                    text: "❌ Cancel".into(),
                                    callback_data: format!("cb:{cancel_token}:lifecycle_cancel"),
                                },
                            ],
                        ];
                        drop(ctx_lock);
                        let _ = self
                                .im_adapter
                                .send_keyboard(
                                    &target,
                                    &format!(
                                        "Chat name has changed from '{}' to '{}'. What would you like to do?",
                                        binding.display_name, chat_name,
                                    ),
                                    &buttons,
                                )
                                .await;
                        return Ok(());
                    }

                    // 3.3 Agent type mismatch — running process differs from stored agent_type
                    let stored_agent_type = state
                        .window_states
                        .get(&binding.window_id)
                        .map(|ws| ws.agent_type.as_str());
                    if let Some(running_agent) = self.detect_running_agent(&info.current_command)
                        && let Some(stored_type) = stored_agent_type
                        && running_agent != stored_type
                    {
                        tracing::info!(
                            "[handle_text_message] Agent type mismatch: stored='{stored_type}' running='{running_agent}', prompting user {}",
                            user_id,
                        );
                        let key = (user_id, target.chat_id.0, thread_id);
                        {
                            let mut pending = self.pending_messages.lock().await;
                            pending.insert(key, text.to_string());
                        }
                        let mut ctx_lock = self.callback_contexts.lock().await;
                        let rebind_token = Self::make_callback_token(
                            &mut ctx_lock,
                            user_id,
                            target.chat_id.0,
                            thread_id,
                        );
                        let cancel_token = Self::make_callback_token(
                            &mut ctx_lock,
                            user_id,
                            target.chat_id.0,
                            thread_id,
                        );
                        let buttons = vec![
                            vec![Button {
                                text: "🔄 Update Binding".into(),
                                callback_data: format!("cb:{rebind_token}:rebind"),
                            }],
                            vec![
                                Button {
                                    text: "🆕 New Session".into(),
                                    callback_data: format!("cb:{rebind_token}:new"),
                                },
                                Button {
                                    text: "❌ Cancel".into(),
                                    callback_data: format!("cb:{cancel_token}:lifecycle_cancel"),
                                },
                            ],
                        ];
                        drop(ctx_lock);
                        let _ = self
                            .im_adapter
                            .send_keyboard(
                                &target,
                                &format!(
                                    "Agent type has changed from '{}' to '{}'. What would you like to do?",
                                    stored_type, running_agent,
                                ),
                                &buttons,
                            )
                            .await;
                        return Ok(());
                    }

                    // Try to resolve empty session_id (e.g. copilot sessions)
                    if let Some(ws) = state.window_states.get(&binding.window_id)
                        && ws.session_id.is_empty()
                        && ws.agent_type == "copilot"
                        && let Some(agent) = self.config.agent_registry.get("copilot")
                        && let Ok(Some(sid)) = agent.discover_session_by_pid(&window_id.0)
                        && let Ok(mut new_state) = self.state_mgr.load_state().await
                    {
                        if let Some(ws) = new_state.window_states.get_mut(&binding.window_id) {
                            ws.session_id = sid.clone();
                            tracing::info!(
                                "[handle_text_message] Resolved copilot session_id={sid} for window {}",
                                binding.window_id,
                            );
                        }
                        let _ = self.state_mgr.save_state(&new_state).await;
                    }

                    let is_copilot = state
                        .window_states
                        .get(&binding.window_id)
                        .map(|ws| ws.agent_type == "copilot")
                        .unwrap_or(false);
                    let result = if is_copilot {
                        self.tmux_mgr.send_line_chars(&window_id, text, 10).await
                    } else {
                        self.tmux_mgr.send_line(&window_id, text).await
                    };
                    match &result {
                        Ok(()) => tracing::info!(
                            "[handle_text_message] window={} send_line OK",
                            binding.window_id,
                        ),
                        Err(e) => tracing::error!(
                            "[handle_text_message] window={} send_line failed: {e}",
                            binding.window_id,
                        ),
                    }
                    result?;
                    let sc_chat_id = binding.group_chat_id.unwrap_or(binding.chat_id);
                    self.status_consumed
                        .lock()
                        .await
                        .remove(&(sc_chat_id, binding.thread_id));
                    return Ok(());
                }
            }
        } else {
            // In group chat without @-mention and no active binding, silently ignore
            if is_group && !is_mention {
                tracing::debug!(
                    "Ignoring group message from user {user_id} (no binding, no @-mention)"
                );
                return Ok(());
            }

            tracing::debug!(
                "[Feishu] No binding for user {user_id}, showing picker (is_mention={is_mention}, is_group={is_group})"
            );

            // No binding — save pending text, then show agent picker
            let key = (
                user_id,
                target.chat_id.0,
                target.thread_id.map(|t| t.0).unwrap_or(0),
            );
            {
                let mut pending = self.pending_messages.lock().await;
                pending.insert(key, text.to_string());
            }

            self.send_agent_picker(&target, user_id, target.thread_id.map(|t| t.0).unwrap_or(0))
                .await?;
        }

        Ok(())
    }

    /// Show the directory browser inline keyboard for session creation.
    async fn show_directory_browser(
        &self,
        target: &MessageTarget,
        user_id: i64,
        thread_id: i64,
    ) -> Result<()> {
        let start_path = std::env::current_dir().unwrap_or_else(|_| Path::new("/").to_path_buf());

        self.browser.start_browsing(user_id, &start_path).await;
        let _ = self.send_browser_keyboard(target, user_id, thread_id).await;
        Ok(())
    }

    /// Show the agent picker inline keyboard with "Choose Agent" title.
    async fn send_agent_picker(
        &self,
        target: &MessageTarget,
        user_id: i64,
        thread_id: i64,
    ) -> Result<()> {
        let mut ctx_lock = self.callback_contexts.lock().await;
        let mut buttons: Vec<Vec<Button>> = Vec::new();

        for agent in self.config.agent_registry.iter() {
            let token =
                Self::make_callback_token(&mut ctx_lock, user_id, target.chat_id.0, thread_id);
            buttons.push(vec![Button {
                text: format!("🚀 {}", agent.name()),
                callback_data: format!("cb:{token}:agent:{}", agent.name()),
            }]);
        }

        let cancel_token =
            Self::make_callback_token(&mut ctx_lock, user_id, target.chat_id.0, thread_id);
        buttons.push(vec![Button {
            text: "❌ Cancel".into(),
            callback_data: format!("cb:{cancel_token}:cancel"),
        }]);

        drop(ctx_lock);

        let _ = self
            .im_adapter
            .send_keyboard(target, "Choose Agent", &buttons)
            .await;
        Ok(())
    }

    /// After agent selection, show window picker or directory browser.
    async fn show_setup_flow(
        &self,
        target: &MessageTarget,
        user_id: i64,
        thread_id: i64,
    ) -> Result<()> {
        let state = self.state_mgr.load_state().await?;
        let unbound = self.unbound_windows(&state).await;
        if !unbound.is_empty() {
            self.show_window_picker(target, user_id, &unbound, thread_id)
                .await?;
        } else {
            // topic_name is NOT consumed here — the browser action handler
            // or callback handler will consume it when creating the binding.
            self.show_directory_browser(target, user_id, thread_id)
                .await?;
        }
        Ok(())
    }

    /// Compute unbound tmux windows (exist in tmux but not in any binding).
    async fn unbound_windows(
        &self,
        state: &atim_core::session::ServerState,
    ) -> Vec<browser::WindowEntry> {
        let bound_ids: HashSet<String> = state
            .thread_bindings
            .iter()
            .map(|b| b.window_id.clone())
            .collect();

        match self.tmux_mgr.list_windows().await {
            Ok(windows) => windows
                .into_iter()
                .filter(|w| !bound_ids.contains(&w.window_id.0))
                .map(|w| {
                    let agent_type = state
                        .window_states
                        .get(&w.window_id.0)
                        .map(|ws| ws.agent_type.as_str())
                        .unwrap_or("");
                    browser::WindowEntry {
                        window_id: w.window_id.0,
                        name: w.name,
                        current_command: w.current_command,
                        agent_type: agent_type.to_string(),
                    }
                })
                .collect(),
            Err(_) => vec![],
        }
    }

    /// Show the window picker inline keyboard.
    async fn show_window_picker(
        &self,
        target: &MessageTarget,
        user_id: i64,
        unbound: &[browser::WindowEntry],
        thread_id: i64,
    ) -> Result<()> {
        let start_path = std::env::current_dir().unwrap_or_else(|_| Path::new("/").to_path_buf());

        self.browser.start_browsing(user_id, &start_path).await;
        self.browser
            .show_window_picker(user_id, unbound.to_vec())
            .await;
        let _ = self.send_browser_keyboard(target, user_id, thread_id).await;
        Ok(())
    }

    /// Build and send the current browser keyboard to the user.
    async fn send_browser_keyboard(
        &self,
        target: &MessageTarget,
        user_id: i64,
        thread_id: i64,
    ) -> Result<()> {
        let state = match self.browser.get_state(user_id).await {
            Some(s) => s,
            None => {
                tracing::warn!("No browser state for user {user_id}");
                return Ok(());
            }
        };

        let mut ctx_lock = self.callback_contexts.lock().await;

        let (text, buttons) = match &state.mode {
            BrowserMode::Browsing => {
                let listing = browser::get_dir_listing(&state);
                let mut buttons: Vec<Vec<Button>> = Vec::new();

                // Entry rows
                for (i, entry) in listing.entries.iter().enumerate() {
                    let display = format!("📁 {}", entry.name);
                    let token = Self::make_callback_token(
                        &mut ctx_lock,
                        user_id,
                        target.chat_id.0,
                        thread_id,
                    );
                    buttons.push(vec![Button {
                        text: display,
                        callback_data: format!("cb:{token}:browse:dir:{i}"),
                    }]);
                }

                // Navigation row
                let mut nav_row = Vec::new();
                let cancel_token =
                    Self::make_callback_token(&mut ctx_lock, user_id, target.chat_id.0, thread_id);
                nav_row.push(Button {
                    text: "❌ Cancel".into(),
                    callback_data: format!("cb:{cancel_token}:browse:cancel"),
                });

                if listing.total_pages > 1 {
                    let page_token = Self::make_callback_token(
                        &mut ctx_lock,
                        user_id,
                        target.chat_id.0,
                        thread_id,
                    );
                    nav_row.push(Button {
                        text: format!("◀ {}/{} ▶", listing.page + 1, listing.total_pages),
                        callback_data: format!("cb:{page_token}:browse:page"),
                    });
                }
                if listing.has_parent {
                    let up_token = Self::make_callback_token(
                        &mut ctx_lock,
                        user_id,
                        target.chat_id.0,
                        thread_id,
                    );
                    nav_row.push(Button {
                        text: "⬆ Up".into(),
                        callback_data: format!("cb:{up_token}:browse:up"),
                    });
                }
                let sel_token =
                    Self::make_callback_token(&mut ctx_lock, user_id, target.chat_id.0, thread_id);
                nav_row.push(Button {
                    text: "✅ Select".into(),
                    callback_data: format!("cb:{sel_token}:browse:confirm"),
                });
                buttons.push(nav_row);

                let text = format!(
                    "📁 Select a project directory:\n{}",
                    listing.current_path.display()
                );
                (text, buttons)
            }
            BrowserMode::SessionPick { sessions: _ } => {
                let page = browser::get_session_picker_page(&state).unwrap();
                let mut buttons: Vec<Vec<Button>> = Vec::new();

                for (i, session) in page.sessions.iter().enumerate() {
                    let timestamp_short = if session.timestamp.len() > 16 {
                        &session.timestamp[..16]
                    } else {
                        &session.timestamp
                    };
                    let summary = if session.summary.len() > 40 {
                        let end = session
                            .summary
                            .char_indices()
                            .nth(37)
                            .map(|(i, _)| i)
                            .unwrap_or(session.summary.len());
                        format!("{}…", &session.summary[..end])
                    } else if session.summary.is_empty() {
                        "(empty)".into()
                    } else {
                        session.summary.clone()
                    };
                    let token = Self::make_callback_token(
                        &mut ctx_lock,
                        user_id,
                        target.chat_id.0,
                        thread_id,
                    );
                    buttons.push(vec![Button {
                        text: format!(
                            "🔄 {} {} | {}",
                            timestamp_short, session.project_slug, summary
                        ),
                        callback_data: format!("cb:{token}:browse:sel:{i}"),
                    }]);
                }

                // Navigation row
                let mut nav_row = Vec::new();
                let cancel_token =
                    Self::make_callback_token(&mut ctx_lock, user_id, target.chat_id.0, thread_id);
                nav_row.push(Button {
                    text: "❌ Cancel".into(),
                    callback_data: format!("cb:{cancel_token}:browse:cancel"),
                });

                let back_token =
                    Self::make_callback_token(&mut ctx_lock, user_id, target.chat_id.0, thread_id);
                nav_row.push(Button {
                    text: "📁 Browse".into(),
                    callback_data: format!("cb:{back_token}:browse:back"),
                });

                if page.total_pages > 1 {
                    let page_token = Self::make_callback_token(
                        &mut ctx_lock,
                        user_id,
                        target.chat_id.0,
                        thread_id,
                    );
                    nav_row.push(Button {
                        text: format!("◀ {}/{} ▶", page.page + 1, page.total_pages),
                        callback_data: format!("cb:{page_token}:browse:page"),
                    });
                }
                let new_token =
                    Self::make_callback_token(&mut ctx_lock, user_id, target.chat_id.0, thread_id);
                nav_row.push(Button {
                    text: "🆕 New".into(),
                    callback_data: format!("cb:{new_token}:browse:new"),
                });
                buttons.push(nav_row);

                let text = format!(
                    "🔄 Select a session to resume (or choose New):\n{}",
                    state.current_path.display()
                );
                (text, buttons)
            }
            BrowserMode::WindowPick { windows: _ } => {
                let page = browser::get_window_picker_page(&state).unwrap();
                let mut buttons: Vec<Vec<Button>> = Vec::new();

                for (i, win) in page.windows.iter().enumerate() {
                    let agent = if win.agent_type.is_empty() {
                        &win.current_command
                    } else {
                        &win.agent_type
                    };
                    let label = format!("💬 {} [{}]", win.name, agent);
                    let token = Self::make_callback_token(
                        &mut ctx_lock,
                        user_id,
                        target.chat_id.0,
                        thread_id,
                    );
                    buttons.push(vec![Button {
                        text: label,
                        callback_data: format!("cb:{token}:browse:win:{i}"),
                    }]);
                }

                // Navigation row
                let mut nav_row = Vec::new();
                let cancel_token =
                    Self::make_callback_token(&mut ctx_lock, user_id, target.chat_id.0, thread_id);
                nav_row.push(Button {
                    text: "❌ Cancel".into(),
                    callback_data: format!("cb:{cancel_token}:browse:cancel"),
                });

                let new_token =
                    Self::make_callback_token(&mut ctx_lock, user_id, target.chat_id.0, thread_id);
                nav_row.push(Button {
                    text: "🆕 New Session".into(),
                    callback_data: format!("cb:{new_token}:browse:new_win"),
                });

                if page.total_pages > 1 {
                    let page_token = Self::make_callback_token(
                        &mut ctx_lock,
                        user_id,
                        target.chat_id.0,
                        thread_id,
                    );
                    nav_row.push(Button {
                        text: format!("◀ {}/{} ▶", page.page + 1, page.total_pages),
                        callback_data: format!("cb:{page_token}:browse:page"),
                    });
                }
                buttons.push(nav_row);

                let text = "💬 An unbound tmux window was found. Select one to attach, or create a new session:".to_string();
                (text, buttons)
            }
        };

        drop(ctx_lock);
        let _ = self.im_adapter.send_keyboard(target, &text, &buttons).await;
        Ok(())
    }

    /// Handle a browser callback action.
    async fn handle_browser_action(
        &self,
        target: &MessageTarget,
        user_id: i64,
        thread_id: i64,
        msg_id: &MessageId,
        action: &str,
        text: &str,
    ) -> Result<()> {
        let state = match self.browser.get_state(user_id).await {
            Some(s) => s,
            None => {
                tracing::warn!("Browser action {action} for user {user_id} but no active session");
                return Ok(());
            }
        };

        match action {
            "cancel" => {
                self.browser.end_session(user_id).await;
                let _ = self
                    .im_adapter
                    .edit_message(target, msg_id, "Cancelled.")
                    .await;
            }
            "up" => {
                self.browser.go_up(user_id).await;
                let _ = self.send_browser_keyboard(target, user_id, thread_id).await;
                let _ = self.im_adapter.delete_message(target, msg_id).await;
            }
            "page" => {
                // Toggle to next page
                let new_page = state.page + 1;
                self.browser.set_page(user_id, new_page).await;
                let _ = self.send_browser_keyboard(target, user_id, thread_id).await;
                let _ = self.im_adapter.delete_message(target, msg_id).await;
            }
            "confirm" => {
                // Scan the current directory for sessions and show picker
                let sessions = browser::scan_claude_sessions(&state.current_path);
                if sessions.is_empty() {
                    // No existing sessions — create new directly
                    let topic_name = self
                        .topic_names
                        .lock()
                        .await
                        .remove(&(target.chat_id.0, thread_id));
                    self.browser.end_session(user_id).await;
                    // Override cwd for the new window
                    let _ = self
                        .im_adapter
                        .edit_message(target, msg_id, "Creating new session...")
                        .await;
                    self.create_and_bind_in_dir(
                        target,
                        user_id,
                        text,
                        &state.current_path,
                        topic_name.as_deref(),
                    )
                    .await?;
                } else {
                    self.browser.show_session_picker(user_id, sessions).await;
                    let _ = self.send_browser_keyboard(target, user_id, thread_id).await;
                    let _ = self.im_adapter.delete_message(target, msg_id).await;
                }
            }
            "back" => {
                self.browser.show_browsing(user_id).await;
                let _ = self.send_browser_keyboard(target, user_id, thread_id).await;
                let _ = self.im_adapter.delete_message(target, msg_id).await;
            }
            "new" => {
                // Create a new session in the selected directory
                let topic_name = self
                    .topic_names
                    .lock()
                    .await
                    .remove(&(target.chat_id.0, thread_id));
                self.browser.end_session(user_id).await;
                let _ = self
                    .im_adapter
                    .edit_message(target, msg_id, "Creating new session...")
                    .await;
                self.create_and_bind_in_dir(
                    target,
                    user_id,
                    text,
                    &state.current_path,
                    topic_name.as_deref(),
                )
                .await?;
            }
            d if d.starts_with("dir:") => {
                // Navigate to a directory by index
                let idx: usize = d[4..].parse().unwrap_or(0);
                let listing = browser::get_dir_listing(&state);
                if let Some(entry) = listing.entries.get(idx)
                    && entry.is_dir
                {
                    self.browser.navigate_to(user_id, &entry.path).await;
                    let _ = self.send_browser_keyboard(target, user_id, thread_id).await;
                    let _ = self.im_adapter.delete_message(target, msg_id).await;
                }
            }
            s if s.starts_with("sel:") => {
                // Select a session from the picker by index
                let idx: usize = s[4..].parse().unwrap_or(0);
                let state_now = self.browser.get_state(user_id).await;
                if let Some(BrowserMode::SessionPick { sessions }) =
                    state_now.as_ref().map(|s| &s.mode)
                    && let Some(session) = sessions.get(idx)
                {
                    let topic_name = self
                        .topic_names
                        .lock()
                        .await
                        .remove(&(target.chat_id.0, thread_id));
                    self.browser.end_session(user_id).await;
                    let _ = self
                        .im_adapter
                        .edit_message(target, msg_id, "Resuming session...")
                        .await;
                    self.create_and_bind_with_resume(
                        target,
                        user_id,
                        text,
                        &state.current_path,
                        &session.id,
                        topic_name.as_deref(),
                    )
                    .await?;
                }
            }
            "new_win" => {
                // User chose "New Session" from window picker — go to directory browser
                self.browser.show_browsing(user_id).await;
                let _ = self.send_browser_keyboard(target, user_id, thread_id).await;
                let _ = self.im_adapter.delete_message(target, msg_id).await;
            }
            w if w.starts_with("win:") => {
                // User selected an unbound tmux window to attach
                let idx: usize = w[4..].parse().unwrap_or(0);
                let state_now = self.browser.get_state(user_id).await;
                if let Some(BrowserMode::WindowPick { windows }) =
                    state_now.as_ref().map(|s| &s.mode)
                    && let Some(entry) = windows.get(idx)
                {
                    self.browser.end_session(user_id).await;
                    let _ = self
                        .im_adapter
                        .edit_message(target, msg_id, "Attaching to existing window...")
                        .await;
                    let topic_name = self
                        .topic_names
                        .lock()
                        .await
                        .remove(&(target.chat_id.0, thread_id));
                    self.bind_window(
                        target,
                        user_id,
                        text,
                        &entry.window_id,
                        topic_name.as_deref(),
                    )
                    .await?;
                }
            }
            _ => {
                tracing::warn!("Unknown browser action: {action}");
            }
        }
        Ok(())
    }

    /// After starting an agent in a new window, wait for the session_id to be
    /// registered — first by polling session_map.json (the SessionStart hook),
    /// then falling back to Agent::discover_session_by_pid / discover_session.
    ///
    /// `cwd_hint` is the working directory of the pane — used for the
    /// project-slug fallback when lsof fails (Claude only).
    async fn resolve_session_id(
        &self,
        window_id: &str,
        timeout: Duration,
        cwd_hint: Option<&str>,
    ) -> Option<String> {
        // Determine the agent for this window to dispatch session discovery.
        let agent = self
            .state_mgr
            .load_state()
            .await
            .ok()
            .and_then(|s| s.window_states.get(window_id).cloned())
            .and_then(|ws| {
                if ws.agent_type == "claude" {
                    Some(self.config.agent_registry.default().clone())
                } else {
                    self.config.agent_registry.get(&ws.agent_type).cloned()
                }
            })
            .unwrap_or_else(|| self.config.agent_registry.default().clone());

        // Only Claude Code supports sessions — skip for others.
        if !agent.supports_sessions() {
            return None;
        }

        let start = std::time::Instant::now();

        // Phase 1: Poll session_map.json for the hook's entry (every 300ms)
        while start.elapsed() < timeout {
            if let Ok(map) = self.state_mgr.load_session_map().await
                && let Some(sid) = map.get(window_id)
                && !sid.is_empty()
            {
                tracing::info!("Found session {sid} for window {window_id} via session_map");
                return Some(sid.clone());
            }
            tokio::time::sleep(Duration::from_millis(300)).await;
        }

        // Phase 2: Fallback — agent session discovery (lsof → project-slug)
        tracing::warn!(
            "SessionStart hook didn't register session_id for window {window_id} within {timeout:?}, trying agent discovery"
        );

        // Phase 2a: PID tracing
        if let Ok(Some(sid)) = agent.discover_session_by_pid(window_id) {
            return Some(sid);
        }

        // Phase 2b: working-directory based discovery
        if let Some(cwd) = cwd_hint {
            tracing::warn!(
                "PID discovery failed for window {window_id}, trying path-based discovery with cwd={cwd}"
            );
            let state = self.state_mgr.load_state().await.ok()?;
            let mut known_ids: std::collections::HashSet<String> = state
                .window_states
                .values()
                .map(|ws| ws.session_id.clone())
                .filter(|sid| !sid.is_empty())
                .collect();
            if let Ok(map) = self.state_mgr.load_session_map().await {
                for sid in map.values() {
                    if !sid.is_empty() {
                        known_ids.insert(sid.clone());
                    }
                }
            }
            if let Ok(Some(sid)) = agent.discover_session(cwd, &known_ids) {
                return Some(sid);
            }
        }

        None
    }

    /// Get the agent selected for this context, or the default agent.
    /// Consumes the pending agent selection (one-time use).
    async fn resolve_agent(&self, user_id: i64, chat_id: i64, thread_id: i64) -> AgentHandle {
        let key = (user_id, chat_id, thread_id);
        let name = self.pending_agents.lock().await.remove(&key);
        match name {
            Some(n) => self
                .config
                .agent_registry
                .get(&n)
                .cloned()
                .unwrap_or_else(|| self.config.agent_registry.default().clone()),
            None => self.config.agent_registry.default().clone(),
        }
    }

    /// Create a new tmux window and agent session in a specific directory.
    async fn create_and_bind_in_dir(
        &self,
        target: &MessageTarget,
        user_id: i64,
        _initial_text: &str,
        cwd: &Path,
        topic_name: Option<&str>,
    ) -> Result<()> {
        let name = format!("atim-{user_id}");
        let window_name = topic_name.unwrap_or(&name);
        let window_id = self
            .tmux_mgr
            .new_window(window_name, &cwd.to_string_lossy())
            .await?;

        let agent = self
            .resolve_agent(
                user_id,
                target.chat_id.0,
                target.thread_id.map(|t| t.0).unwrap_or(0),
            )
            .await;
        let launch_cmd = agent_launch_cmd(&agent);
        self.tmux_mgr.send_line(&window_id, &launch_cmd).await?;

        // Wait for agent process to actually start (non-shell process appears)
        for _ in 0..10 {
            if let Ok(info) = self.tmux_mgr.find_window(&window_id).await
                && !is_shell_process(&info.current_command)
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(500)).await;
        }

        // Notify user the session is ready
        let _ = self
            .im_adapter
            .send_message(
                target,
                "✅ Session ready! Send your message to start chatting.",
            )
            .await;

        let mut state = self.state_mgr.load_state().await?;
        state.thread_bindings.push(ThreadBinding {
            user_id,
            thread_id: target.thread_id.map(|t| t.0).unwrap_or(0),
            chat_id: target.chat_id.0,
            window_id: window_id.0.clone(),
            display_name: window_name.to_string(),
            group_chat_id: None,
            topic_name: topic_name.map(String::from),
        });
        state.window_states.insert(
            window_id.0.clone(),
            WindowState {
                session_id: String::new(),
                cwd: cwd.to_string_lossy().to_string(),
                window_name: window_name.to_string(),
                agent_type: agent.name().to_string(),
            },
        );
        self.state_mgr.save_state(&state).await?;

        // Try to resolve session_id so the monitor can track responses.
        // This is best-effort: if it fails, the SessionMapChanged handler
        // in the monitor loop will discover it later (after a restart or hook re-run).
        let wid = window_id.0.clone();
        if let Some(sid) = self
            .resolve_session_id(
                &wid,
                Duration::from_secs(15),
                Some(cwd.to_str().unwrap_or_default()),
            )
            .await
        {
            let mut state = self.state_mgr.load_state().await?;
            if let Some(ws) = state.window_states.get_mut(&wid) {
                ws.session_id = sid;
            }
            self.state_mgr.save_state(&state).await?;
        }

        Ok(())
    }

    /// Create a new tmux window and resume an existing Claude session.
    async fn create_and_bind_with_resume(
        &self,
        target: &MessageTarget,
        user_id: i64,
        _initial_text: &str,
        cwd: &Path,
        session_id: &str,
        topic_name: Option<&str>,
    ) -> Result<()> {
        let name = format!("atim-{user_id}");
        let window_name = topic_name.unwrap_or(&name);
        let window_id = self
            .tmux_mgr
            .new_window(window_name, &cwd.to_string_lossy())
            .await?;

        // Launch agent with resume command
        let agent = self
            .resolve_agent(
                user_id,
                target.chat_id.0,
                target.thread_id.map(|t| t.0).unwrap_or(0),
            )
            .await;
        let resume_cmd = match agent.resume_command(session_id) {
            Some(cmd) => cmd,
            None => {
                tracing::error!("Agent '{}' does not support session resume", agent.name());
                let _ = self
                    .im_adapter
                    .send_message(target, "This agent does not support resuming sessions.")
                    .await;
                return Ok(());
            }
        };
        self.tmux_mgr.send_line(&window_id, &resume_cmd).await?;

        // Notify user the session is ready instead of sending the first line to Claude
        let _ = self
            .im_adapter
            .send_message(
                target,
                "✅ Session ready! Send your message to start chatting.",
            )
            .await;

        let mut state = self.state_mgr.load_state().await?;
        state.thread_bindings.push(ThreadBinding {
            user_id,
            thread_id: target.thread_id.map(|t| t.0).unwrap_or(0),
            chat_id: target.chat_id.0,
            window_id: window_id.0.clone(),
            display_name: window_name.to_string(),
            group_chat_id: None,
            topic_name: topic_name.map(String::from),
        });
        state.window_states.insert(
            window_id.0.clone(),
            WindowState {
                session_id: session_id.to_string(),
                cwd: cwd.to_string_lossy().to_string(),
                window_name: window_name.to_string(),
                agent_type: agent.name().to_string(),
            },
        );
        self.state_mgr.save_state(&state).await?;
        Ok(())
    }

    /// Bind an existing tmux window to a user/thread and notify the user.
    ///
    /// If the window is running a shell (agent not started), sends the agent
    /// command and waits for it to launch before notifying the user.
    async fn bind_window(
        &self,
        target: &MessageTarget,
        user_id: i64,
        _text: &str,
        window_id: &str,
        topic_name: Option<&str>,
    ) -> Result<()> {
        let wid = WindowId(window_id.to_string());
        let window_name = match topic_name {
            Some(name) => name.to_string(),
            None => format!("atim-{user_id}"),
        };

        // Rename to reflect the new binding
        let _ = self.tmux_mgr.rename_window(&wid, &window_name).await;

        // If the pane is running a shell (agent hasn't started), launch it now
        if let Ok(info) = self.tmux_mgr.find_window(&wid).await
            && is_shell_process(&info.current_command)
        {
            let agent = self
                .resolve_agent(
                    user_id,
                    target.chat_id.0,
                    target.thread_id.map(|t| t.0).unwrap_or(0),
                )
                .await;
            let launch_cmd = agent_launch_cmd(&agent);
            self.tmux_mgr.send_line(&wid, &launch_cmd).await?;
            // Wait for agent process to start
            for _ in 0..10 {
                if let Ok(info) = self.tmux_mgr.find_window(&wid).await
                    && !is_shell_process(&info.current_command)
                {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(500)).await;
            }
        }

        // Notify user the session is ready
        let _ = self
            .im_adapter
            .send_message(
                target,
                "✅ Session ready! Send your message to start chatting.",
            )
            .await;

        // Persist the binding
        let mut state = self.state_mgr.load_state().await?;
        // Ensure WindowState entry exists (needed for session_id tracking)
        let cwd = self.tmux_mgr.pane_cwd(&wid).await.unwrap_or_default();
        let agent = self
            .resolve_agent(
                user_id,
                target.chat_id.0,
                target.thread_id.map(|t| t.0).unwrap_or(0),
            )
            .await;
        state
            .window_states
            .entry(window_id.to_string())
            .or_insert(WindowState {
                session_id: String::new(),
                cwd,
                window_name: window_name.clone(),
                agent_type: agent.name().to_string(),
            });
        state.thread_bindings.push(ThreadBinding {
            user_id,
            thread_id: target.thread_id.map(|t| t.0).unwrap_or(0),
            chat_id: target.chat_id.0,
            window_id: window_id.to_string(),
            display_name: window_name.to_string(),
            group_chat_id: None,
            topic_name: topic_name.map(String::from),
        });
        self.state_mgr.save_state(&state).await?;

        Ok(())
    }

    async fn create_and_bind(
        &self,
        target: &MessageTarget,
        user_id: i64,
        _initial_text: &str,
        topic_name: Option<&str>,
    ) -> Result<()> {
        let window_name = match topic_name {
            Some(name) => name.to_string(),
            None => format!("atim-{user_id}"),
        };
        let cwd = std::env::current_dir()
            .unwrap_or_else(|_| std::path::PathBuf::from("/"))
            .to_string_lossy()
            .to_string();

        let window_id = self.tmux_mgr.new_window(&window_name, &cwd).await?;

        // Start the agent
        let agent = self
            .resolve_agent(
                user_id,
                target.chat_id.0,
                target.thread_id.map(|t| t.0).unwrap_or(0),
            )
            .await;
        let launch_cmd = agent_launch_cmd(&agent);
        self.tmux_mgr.send_line(&window_id, &launch_cmd).await?;

        // Wait for agent process to actually start (non-shell process appears)
        for _ in 0..10 {
            if let Ok(info) = self.tmux_mgr.find_window(&window_id).await
                && !is_shell_process(&info.current_command)
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(500)).await;
        }

        // Notify user the session is ready
        let _ = self
            .im_adapter
            .send_message(
                target,
                "✅ Session ready! Send your message to start chatting.",
            )
            .await;

        // Persist the binding
        let mut state = self.state_mgr.load_state().await?;
        state.thread_bindings.push(ThreadBinding {
            user_id,
            thread_id: target.thread_id.map(|t| t.0).unwrap_or(0),
            chat_id: target.chat_id.0,
            window_id: window_id.0.clone(),
            display_name: window_name.to_string(),
            group_chat_id: None,
            topic_name: topic_name.map(String::from),
        });
        state.window_states.insert(
            window_id.0.clone(),
            WindowState {
                session_id: String::new(),
                cwd: cwd.clone(),
                window_name: window_name.to_string(),
                agent_type: agent.name().to_string(),
            },
        );
        self.state_mgr.save_state(&state).await?;

        // Try to resolve session_id so the monitor can track responses.
        let wid = window_id.0.clone();
        if let Some(sid) = self
            .resolve_session_id(&wid, Duration::from_secs(15), Some(&cwd))
            .await
        {
            let mut state = self.state_mgr.load_state().await?;
            if let Some(ws) = state.window_states.get_mut(&wid) {
                ws.session_id = sid;
            }
            self.state_mgr.save_state(&state).await?;
        }

        Ok(())
    }

    async fn handle_callback(
        &self,
        target: MessageTarget,
        user_id: i64,
        data: &str,
        msg_id: MessageId,
        callback_query_id: Option<&str>,
    ) -> Result<()> {
        tracing::debug!("Callback from user {user_id}: {data}");

        // Handle UI navigation callbacks (no token validation needed)
        if data.starts_with("ui:") {
            let state = self.state_mgr.load_state().await?;
            if let Some(binding) = state.thread_bindings.iter().find(|b| {
                b.user_id == user_id && b.thread_id == target.thread_id.map(|t| t.0).unwrap_or(0)
            }) {
                let _ = handle_ui_callback(&self.tmux_mgr, &binding.window_id, data).await;
                if let Some(qid) = callback_query_id {
                    let _ = self.im_adapter.answer_callback(qid, "").await;
                }
            }
            return Ok(());
        }

        // Parse callback data: cb:<token>:<action>[:<arg>]
        let parsed = data.strip_prefix("cb:").and_then(|s| {
            let mut parts = s.splitn(3, ':');
            let token = parts.next()?;
            let action = parts.next()?;
            let arg = parts.next();
            Some((token, action, arg))
        });

        let (token, action, arg) = match parsed {
            Some(p) => p,
            None => {
                tracing::warn!("Callback missing cb: prefix: {data}");
                return Ok(());
            }
        };

        // Browse callbacks: skip token validation — route directly using the current
        // browser state and pending message. Browse is inherently multi-step (directory
        // navigation, session picker, etc.) so single-use tokens break the flow. The
        // browser already maintains per-user state for safety.
        let thread_id = target.thread_id.map(|t| t.0).unwrap_or(0);
        if action == "browse" {
            let key = (user_id, target.chat_id.0, thread_id);
            let pending = self.pending_messages.lock().await.get(&key).cloned();
            let text = match pending {
                Some(t) => t,
                None => {
                    tracing::warn!("No pending message for browse callback (key={key:?})");
                    if let Some(qid) = callback_query_id {
                        let _ = self.im_adapter.answer_callback(qid, "").await;
                    }
                    return Ok(());
                }
            };
            let browse_action = arg.unwrap_or("");
            self.handle_browser_action(&target, user_id, thread_id, &msg_id, browse_action, &text)
                .await?;
            return Ok(());
        }

        // Non-browse callbacks: validate callback context
        let mut ctx_lock = self.callback_contexts.lock().await;
        let ctx = Self::validate_callback_token(&mut ctx_lock, token);
        drop(ctx_lock);

        let ctx = match ctx {
            Some(c) => c,
            None => {
                tracing::warn!("Stale or invalid callback token: {data}");
                if let Some(qid) = callback_query_id {
                    let _ = self
                        .im_adapter
                        .answer_callback(qid, "This selection has expired. Please start over.")
                        .await;
                }
                return Ok(());
            }
        };

        // Verify the context matches — the callback was created for this user+chat+thread
        if ctx.0 != user_id || ctx.1 != target.chat_id.0 || ctx.2 != thread_id {
            tracing::warn!(
                "Callback context mismatch: expected ({},{},{}) got ({},{},{})",
                ctx.0,
                ctx.1,
                ctx.2,
                user_id,
                target.chat_id.0,
                thread_id,
            );
            if let Some(qid) = callback_query_id {
                let _ = self
                    .im_adapter
                    .answer_callback(qid, "This selection is from a different chat.")
                    .await;
            }
            return Ok(());
        }

        let key = (user_id, target.chat_id.0, thread_id);

        let pending = self.pending_messages.lock().await.remove(&key);
        let text = match pending {
            Some(t) => t,
            None => {
                tracing::warn!("No pending message for callback (key={key:?})");
                return Ok(());
            }
        };

        let topic_name = self
            .topic_names
            .lock()
            .await
            .remove(&(target.chat_id.0, thread_id));

        match action {
            "new" => {
                self.create_and_bind(&target, user_id, &text, topic_name.as_deref())
                    .await?;
                let _ = self
                    .im_adapter
                    .edit_message(&target, &msg_id, "New session created.")
                    .await;
            }
            "bind" => {
                let window_id = arg.unwrap_or("");
                let wid = WindowId(window_id.to_string());
                if !self.tmux_mgr.window_exists(&wid).await {
                    let _ = self
                        .im_adapter
                        .edit_message(&target, &msg_id, "Session no longer available.")
                        .await;
                    return Ok(());
                }
                self.bind_window(&target, user_id, &text, window_id, topic_name.as_deref())
                    .await?;
                let _ = self
                    .im_adapter
                    .edit_message(&target, &msg_id, "Session bound.")
                    .await;
            }
            "browse" => {
                // Directory browser action — sub-actions: dir:<path>, up, cancel, confirm, sel:<sid>, back, page
                let browse_action = arg.unwrap_or("");
                self.handle_browser_action(
                    &target,
                    user_id,
                    thread_id,
                    &msg_id,
                    browse_action,
                    &text,
                )
                .await?;
            }
            "cancel" => {
                let _ = self
                    .im_adapter
                    .edit_message(&target, &msg_id, "Cancelled.")
                    .await;
            }
            "agent" => {
                let agent_name = arg.unwrap_or("").to_string();
                if !agent_name.is_empty() {
                    self.pending_agents
                        .lock()
                        .await
                        .insert(key, agent_name.clone());
                }
                // Re-insert pending message for the subsequent setup flow
                self.pending_messages.lock().await.insert(key, text);
                let _ = self
                    .im_adapter
                    .edit_message(&target, &msg_id, &format!("🤖 Agent: {}", agent_name))
                    .await;
                self.show_setup_flow(&target, user_id, thread_id).await?;
            }
            "recover" => {
                self.handle_recover_session(&target, user_id, thread_id, &text)
                    .await?;
                let _ = self
                    .im_adapter
                    .edit_message(&target, &msg_id, "Session recovered.")
                    .await;
            }
            "rename" => {
                let new_name = self
                    .pending_rename_names
                    .lock()
                    .await
                    .remove(&key)
                    .unwrap_or_default();
                if new_name.is_empty() {
                    let _ = self
                        .im_adapter
                        .edit_message(&target, &msg_id, "Rename name not found.")
                        .await;
                    return Ok(());
                }
                self.handle_rename_window(&new_name, user_id, thread_id)
                    .await?;
                let state = self.state_mgr.load_state().await?;
                if let Some(binding) = state
                    .thread_bindings
                    .iter()
                    .find(|b| b.user_id == user_id && b.thread_id == thread_id)
                {
                    let wid = WindowId(binding.window_id.clone());
                    let _ = self.tmux_mgr.send_line(&wid, &text).await;
                    let sc_chat_id = binding.group_chat_id.unwrap_or(binding.chat_id);
                    self.status_consumed
                        .lock()
                        .await
                        .remove(&(sc_chat_id, binding.thread_id));
                }
                let _ = self
                    .im_adapter
                    .edit_message(&target, &msg_id, "Session renamed. Message forwarded.")
                    .await;
            }
            "rebind" => {
                let state = self.state_mgr.load_state().await?;
                let binding_opt = state
                    .thread_bindings
                    .iter()
                    .find(|b| b.user_id == user_id && b.thread_id == thread_id)
                    .cloned();
                drop(state);
                if let Some(binding) = binding_opt {
                    let wid = WindowId(binding.window_id.clone());
                    let running_agent = match self.tmux_mgr.find_window(&wid).await {
                        Ok(info) => self.detect_running_agent(&info.current_command),
                        Err(_) => None,
                    };
                    if let Some(agent_name) = running_agent {
                        self.handle_rebind_agent(&target, user_id, thread_id, agent_name)
                            .await?;
                        let is_copilot = agent_name == "copilot";
                        if is_copilot {
                            let _ = self.tmux_mgr.send_line_chars(&wid, &text, 10).await;
                        } else {
                            let _ = self.tmux_mgr.send_line(&wid, &text).await;
                        }
                        let sc_chat_id = binding.group_chat_id.unwrap_or(binding.chat_id);
                        self.status_consumed
                            .lock()
                            .await
                            .remove(&(sc_chat_id, binding.thread_id));
                        let _ = self
                            .im_adapter
                            .edit_message(
                                &target,
                                &msg_id,
                                &format!("Binding updated to {agent_name}. Message forwarded."),
                            )
                            .await;
                    } else {
                        let _ = self
                            .im_adapter
                            .edit_message(&target, &msg_id, "Could not detect running agent type.")
                            .await;
                    }
                } else {
                    let _ = self
                        .im_adapter
                        .edit_message(&target, &msg_id, "Session not found.")
                        .await;
                }
            }
            "lifecycle_cancel" => {
                let _ = self
                    .im_adapter
                    .edit_message(&target, &msg_id, "Cancelled.")
                    .await;
            }
            _ => {
                tracing::warn!("Unknown callback action: {action}");
            }
        }

        Ok(())
    }

    async fn handle_topic_closed(&self, target: &MessageTarget) -> Result<()> {
        let thread_id = target.thread_id.map(|t| t.0).unwrap_or(0);
        let chat_id = target.chat_id.0;

        tracing::info!("Topic closed: chat={chat_id} thread={thread_id}");

        // Clean up topic name
        self.topic_names.lock().await.remove(&(chat_id, thread_id));

        // Clean up pending messages for this chat+thread
        let mut pending = self.pending_messages.lock().await;
        pending.retain(|k, _| k.1 != chat_id || k.2 != thread_id);
        drop(pending);

        // Find and kill the associated tmux window, then remove binding
        let mut state = self.state_mgr.load_state().await?;
        if let Some(binding) = state
            .thread_bindings
            .iter()
            .find(|b| b.chat_id == chat_id && b.thread_id == thread_id)
        {
            let wid = WindowId(binding.window_id.clone());
            tracing::info!("Killing tmux window {} for closed topic", wid.0);
            if let Err(e) = self.tmux_mgr.kill_window(&wid).await {
                // Window may already be gone — that's fine
                tracing::debug!("Error killing window {} (may already be gone): {e}", wid.0);
            }
        }
        state
            .thread_bindings
            .retain(|b| b.chat_id != chat_id || b.thread_id != thread_id);
        self.state_mgr.save_state(&state).await?;
        Ok(())
    }

    async fn handle_topic_edited(&self, target: &MessageTarget, new_name: &str) -> Result<()> {
        let thread_id = target.thread_id.map(|t| t.0).unwrap_or(0);
        tracing::info!(
            "Topic renamed: chat={} thread={thread_id} -> \"{new_name}\"",
            target.chat_id.0
        );

        // Update topic name in memory
        let mut names = self.topic_names.lock().await;
        names.insert((target.chat_id.0, thread_id), new_name.to_string());
        drop(names);

        // Update in persisted binding and rename tmux window
        let mut state = self.state_mgr.load_state().await?;
        if let Some(binding) = state
            .thread_bindings
            .iter_mut()
            .find(|b| b.chat_id == target.chat_id.0 && b.thread_id == thread_id)
        {
            binding.topic_name = Some(new_name.to_string());
            let wid = WindowId(binding.window_id.clone());
            if let Err(e) = self.tmux_mgr.rename_window(&wid, new_name).await {
                tracing::warn!("Failed to rename tmux window {}: {e}", wid.0);
            }
            self.state_mgr.save_state(&state).await?;
        }
        Ok(())
    }

    /// Periodically probe whether forum topics still exist.
    ///
    /// Uses `send_chat_action` as a lightweight probe. If the topic was
    /// deleted, Telegram returns an error and we treat it as a topic close.
    async fn probe_topic_deletions(&self) -> Result<()> {
        let state = self.state_mgr.load_state().await?;
        let mut deleted = Vec::new();

        for binding in &state.thread_bindings {
            if binding.thread_id == 0 {
                continue; // not a forum topic
            }
            let chat_id = binding.group_chat_id.unwrap_or(binding.chat_id);
            let target = MessageTarget {
                chat_id: ChatId(chat_id),
                thread_id: Some(ThreadId(binding.thread_id)),
                chat_name: None,
            };
            if let Err(e) = self.im_adapter.send_chat_action(&target).await {
                let err_str = e.to_string();
                // Detect topic-deleted errors from Telegram
                if err_str.contains("topic")
                    || err_str.contains("chat not found")
                    || err_str.contains("Forbidden")
                {
                    tracing::warn!(
                        "Topic probe failed for chat={} thread={}: {e} — cleaning up",
                        chat_id,
                        binding.thread_id,
                    );
                    deleted.push((
                        binding.chat_id,
                        binding.thread_id,
                        binding.window_id.clone(),
                    ));
                }
            }
        }

        // Clean up deleted topics
        if !deleted.is_empty() {
            let mut state = self.state_mgr.load_state().await?;
            for (chat_id, thread_id, window_id) in &deleted {
                let wid = WindowId(window_id.clone());
                if let Err(e) = self.tmux_mgr.kill_window(&wid).await {
                    tracing::debug!("Error killing stale window {}: {e}", wid.0);
                }
                state
                    .thread_bindings
                    .retain(|b| b.chat_id != *chat_id || b.thread_id != *thread_id);
                tracing::info!("Cleaned up deleted topic: chat={chat_id} thread={thread_id}");
            }
            self.state_mgr.save_state(&state).await?;
        }

        Ok(())
    }

    /// Periodically check each bound window for interactive Claude Code UIs.
    ///
    /// When a new interactive UI is detected, sends an inline keyboard
    /// so the user can respond without typing.
    ///
    /// For non-Claude agents (Copilot CLI, Codex CLI), captures terminal
    /// output and forwards new lines back to the IM since they don't
    /// write JSONL session logs.
    async fn probe_interactive_uis(&self) -> Result<()> {
        let state = self.state_mgr.load_state().await?;
        let mut ui_states = self.last_ui_states.lock().await;
        let mut pane_outputs = self.last_pane_output.lock().await;

        for binding in &state.thread_bindings {
            let wid = WindowId(binding.window_id.clone());
            let pane_text = match self.tmux_mgr.capture_pane(&wid).await {
                Ok(t) => t,
                Err(_) => continue, // window gone
            };

            // Strip ANSI
            let clean = atim_parser::terminal::TerminalParser::strip_ansi(&pane_text);

            // Look up the per-window agent from WindowState
            let agent_type = state
                .window_states
                .get(&binding.window_id)
                .map(|ws| ws.agent_type.as_str())
                .unwrap_or("");
            let agent = self
                .config
                .agent_registry
                .get(agent_type)
                .unwrap_or_else(|| self.config.agent_registry.default());

            // Use the agent's own parser for interactive UI detection
            let parser = agent.parser();
            let ui = parser.detect_interactive(&clean);

            // Compute content hash
            let content_hash = ui
                .as_ref()
                .map(|u| format!("{:?}:{}", u.kind, u.content.len()))
                .unwrap_or_default();

            let prev = ui_states.get(&binding.window_id);
            if prev == Some(&content_hash) {
                // UI unchanged — still forward pane output if agent uses PaneCapture
                if agent.output_source() == OutputSource::PaneCapture {
                    self.forward_new_pane_output(binding, &clean, &mut pane_outputs)
                        .await;
                }
                continue;
            }
            ui_states.insert(binding.window_id.clone(), content_hash);

            // New or changed UI — send keyboard
            if let Some(interactive) = ui {
                // Skip AskUser cards — the TUI prompt (❯) from any agent is
                // wrongly detected as a question via stale scrollback content.
                let should_send_card = interactive.kind != UiKind::AskUserQuestion;
                if should_send_card {
                    let target = MessageTarget {
                        chat_id: ChatId(binding.group_chat_id.unwrap_or(binding.chat_id)),
                        thread_id: Some(ThreadId(binding.thread_id)),
                        chat_name: None,
                    };
                    let buttons = ui_to_buttons(&interactive);
                    let header = format!("🧭 {}:", ui_display_name(interactive.kind));
                    let text = format!(
                        "{header}\n{content}",
                        content = truncate_ui_content(&interactive.content, 200)
                    );
                    let _ = self
                        .im_adapter
                        .send_keyboard(&target, &text, &buttons)
                        .await;
                }
            }

            // Forward terminal output for PaneCapture agents
            if agent.output_source() == OutputSource::PaneCapture {
                self.forward_new_pane_output(binding, &clean, &mut pane_outputs)
                    .await;
            }
        }

        Ok(())
    }

    /// Forward new terminal output for a non-Claude agent window.
    ///
    /// TUI-based agents (Copilot CLI, Codex CLI) redraw the screen in-place
    /// rather than appending lines, so line-count-based diffing doesn't work.
    /// Instead, compares the full pane content hash and forwards any
    /// newly-appeared lines that aren't TUI chrome (borders, prompts, etc.).
    async fn forward_new_pane_output(
        &self,
        binding: &ThreadBinding,
        clean: &str,
        pane_outputs: &mut HashMap<String, String>,
    ) {
        let last = pane_outputs.get(&binding.window_id);

        // First call: establish baseline
        let Some(prev) = last else {
            pane_outputs.insert(binding.window_id.clone(), clean.to_string());
            return;
        };

        if prev == clean {
            return; // No change
        }

        // Content changed — find lines in new pane text that weren't in old
        // Trim trailing whitespace from each line to avoid terminal padding noise
        fn trim_trailing(s: &str) -> &str {
            s.trim_end()
        }
        let old_lines: Vec<&str> = prev.lines().map(trim_trailing).collect();
        let new_lines: Vec<&str> = clean.lines().map(trim_trailing).collect();
        let old_set: std::collections::HashSet<&str> = old_lines.iter().copied().collect();
        let added: Vec<&str> = new_lines
            .iter()
            .copied()
            .filter(|l| !old_set.contains(l))
            .collect();

        // Update baseline even if we don't forward (so future diffs are accurate)
        pane_outputs.insert(binding.window_id.clone(), clean.to_string());

        if added.is_empty() {
            return;
        }

        // Filter out TUI chrome (borders, empty lines, status bars)
        let significant: Vec<&str> = added
            .iter()
            .filter(|l| {
                let t = l.trim();
                if t.is_empty() {
                    return false;
                }
                let c = t.chars().next().unwrap();
                // Skip box-drawing, block, and shade characters
                if matches!(
                    c,
                    '│' | '╭'
                        | '╮'
                        | '╰'
                        | '╯'
                        | '─'
                        | '▔'
                        | '▁'
                        | '░'
                        | '█'
                        | '▝'
                        | '▘'
                        | '▖'
                        | '▗'
                ) {
                    return false;
                }
                // Skip lines that are purely decorative
                if t.chars()
                    .all(|c| c.is_ascii_punctuation() || c.is_whitespace())
                {
                    return false;
                }
                true
            })
            .copied()
            .collect();

        if significant.is_empty() {
            return;
        }

        let joined = significant.join("\n");
        let target = MessageTarget {
            chat_id: ChatId(binding.group_chat_id.unwrap_or(binding.chat_id)),
            thread_id: Some(ThreadId(binding.thread_id)),
            chat_name: None,
        };
        let display = if joined.len() > MAX_MSG_LEN {
            let truncated: String = joined.chars().take(MAX_MSG_LEN).collect();
            format!("```\n{}…\n```", truncated)
        } else if joined.len() > 400 {
            format!("```\n{}\n```", joined)
        } else {
            joined
        };

        let _ = self.im_adapter.send_message(&target, &display).await;
    }

    /// Try to associate an unknown session_id with a window by matching the
    /// JSONL project directory against the window's tracked session project dir.
    ///
    /// This handles the case where Claude Code rotates session IDs internally
    /// without triggering the SessionStart hook. The monitor discovers the
    /// new JSONL via filesystem scan, but the server needs to figure out
    /// which window the new session belongs to.
    async fn match_unknown_session(
        &self,
        session_id: &str,
        state: &atim_core::session::ServerState,
    ) -> Option<String> {
        let new_path = resolve_jsonl(session_id).await?;
        let new_parent = new_path.parent()?;

        // Determine agent type from the session path
        let is_copilot = new_path
            .to_str()
            .map(|p| p.contains(".copilot") && p.contains("session-state"))
            .unwrap_or(false);

        if is_copilot {
            // Copilot sessions: match by PID using inuse lock files.
            for (wid, ws) in &state.window_states {
                if ws.agent_type != "copilot" {
                    continue;
                }
                let discovered = self
                    .config
                    .agent_registry
                    .get("copilot")
                    .and_then(|a| a.discover_session_by_pid(wid).ok()?);
                if let Some(sid) = discovered
                    && sid == session_id
                {
                    if let Ok(mut new_state) = self.state_mgr.load_state().await {
                        if let Some(ws) = new_state.window_states.get_mut(wid) {
                            ws.session_id = session_id.to_string();
                            tracing::info!(
                                "[pipe] Updated copilot window {wid} session: {session_id}"
                            );
                        }
                        let _ = self.state_mgr.save_state(&new_state).await;
                    }
                    return Some(wid.clone());
                }
            }
            return None;
        }

        // Claude Code sessions: match by project directory.
        for (wid, ws) in &state.window_states {
            if ws.session_id.is_empty() || ws.agent_type != "claude" {
                continue;
            }
            // Compare project directories: if the old session's JSONL is in
            // the same project directory as the new session's JSONL, assume
            // they belong to the same window.
            if let Some(old_path) = resolve_jsonl(&ws.session_id).await
                && old_path.parent() == Some(new_parent)
            {
                // Persist the new mapping so future lookups are direct
                if let Ok(mut new_state) = self.state_mgr.load_state().await {
                    if let Some(ws) = new_state.window_states.get_mut(wid) {
                        ws.session_id = session_id.to_string();
                        tracing::info!(
                            "[pipe] Updated window {wid} session: {} → {session_id}",
                            ws.session_id,
                        );
                    }
                    let _ = self.state_mgr.save_state(&new_state).await;
                }
                return Some(wid.clone());
            }
        }
        None
    }

    /// Recover a session when the tmux window has died.
    ///
    /// Creates a new tmux window, (re-)launches the agent, optionally
    /// resumes the stored session, re-keys state bindings, then forwards
    /// the user's pending text.
    async fn handle_recover_session(
        &self,
        target: &MessageTarget,
        user_id: i64,
        thread_id: i64,
        text: &str,
    ) -> Result<()> {
        let state = self.state_mgr.load_state().await?;
        let binding = match state
            .thread_bindings
            .iter()
            .find(|b| b.user_id == user_id && b.thread_id == thread_id)
        {
            Some(b) => b.clone(),
            None => {
                tracing::warn!("[recover] No binding found for user={user_id} thread={thread_id}");
                let _ = self
                    .im_adapter
                    .send_message(target, "Session not found.")
                    .await;
                return Ok(());
            }
        };
        let ws = state.window_states.get(&binding.window_id).cloned();
        let cwd = ws.as_ref().map(|w| w.cwd.as_str()).unwrap_or("~");
        let agent_type_name = ws
            .as_ref()
            .map(|w| w.agent_type.as_str())
            .unwrap_or("claude");
        let session_id = ws.as_ref().map(|w| w.session_id.as_str()).unwrap_or("");

        let agent = self
            .config
            .agent_registry
            .get(agent_type_name)
            .cloned()
            .unwrap_or_else(|| self.config.agent_registry.default().clone());

        let _ = self
            .im_adapter
            .send_message(target, "🔄 Recovering session...")
            .await;

        // Create new tmux window
        let window_name = &binding.display_name;
        let new_window_id = self.tmux_mgr.new_window(window_name, cwd).await?;

        // Launch agent (resume if we have a session_id)
        if !session_id.is_empty() && agent.supports_sessions() {
            if let Some(resume_cmd) = agent.resume_command(session_id) {
                self.tmux_mgr.send_line(&new_window_id, &resume_cmd).await?;
            } else {
                let launch_cmd = agent_launch_cmd(&agent);
                self.tmux_mgr.send_line(&new_window_id, &launch_cmd).await?;
            }
        } else {
            let launch_cmd = agent_launch_cmd(&agent);
            self.tmux_mgr.send_line(&new_window_id, &launch_cmd).await?;
        }

        // Wait for agent process to start
        for _ in 0..10 {
            if let Ok(info) = self.tmux_mgr.find_window(&new_window_id).await
                && !is_shell_process(&info.current_command)
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(500)).await;
        }

        if !agent.supports_sessions() {
            tokio::time::sleep(Duration::from_millis(1500)).await;
        }

        // Update state: re-key window_states and binding to the new window_id
        let mut new_state = self.state_mgr.load_state().await?;
        if let Some(old_ws) = new_state.window_states.remove(&binding.window_id) {
            new_state
                .window_states
                .insert(new_window_id.0.clone(), old_ws);
        }
        if let Some(b) = new_state
            .thread_bindings
            .iter_mut()
            .find(|b| b.user_id == user_id && b.thread_id == thread_id)
        {
            b.window_id = new_window_id.0.clone();
        }
        // Clean up stale session_map entries for the old window_id
        if let Ok(mut map) = self.state_mgr.load_session_map().await
            && map.remove(&binding.window_id).is_some()
        {
            let _ = self.state_mgr.save_session_map(&map).await;
        }
        self.state_mgr.save_state(&new_state).await?;

        // For session-based agents, resolve the new session_id if not resuming
        if !session_id.is_empty() && agent.supports_sessions() {
            // Already resumed — update window_state with the resolved session_id
            if let Ok(mut final_state) = self.state_mgr.load_state().await {
                if let Some(ws) = final_state.window_states.get_mut(&new_window_id.0) {
                    ws.session_id = session_id.to_string();
                }
                let _ = self.state_mgr.save_state(&final_state).await;
            }
        } else if agent.supports_sessions() {
            let wid = new_window_id.0.clone();
            if let Some(sid) = self
                .resolve_session_id(&wid, Duration::from_secs(15), Some(cwd))
                .await
                && let Ok(mut final_state) = self.state_mgr.load_state().await
            {
                if let Some(ws) = final_state.window_states.get_mut(&wid) {
                    ws.session_id = sid;
                }
                let _ = self.state_mgr.save_state(&final_state).await;
            }
        }

        // Forward the user's original text to the new window
        let is_copilot = agent.name() == "copilot";
        if is_copilot {
            self.tmux_mgr
                .send_line_chars(&new_window_id, text, 10)
                .await?;
        } else {
            self.tmux_mgr.send_line(&new_window_id, text).await?;
        }

        let sc_chat_id = binding.group_chat_id.unwrap_or(binding.chat_id);
        self.status_consumed
            .lock()
            .await
            .remove(&(sc_chat_id, binding.thread_id));

        Ok(())
    }

    /// Rename a bound window and its tmux window to a new name.
    async fn handle_rename_window(
        &self,
        new_name: &str,
        user_id: i64,
        thread_id: i64,
    ) -> Result<()> {
        if new_name.is_empty() {
            return Ok(());
        }

        let mut state = self.state_mgr.load_state().await?;
        if let Some(binding) = state
            .thread_bindings
            .iter_mut()
            .find(|b| b.user_id == user_id && b.thread_id == thread_id)
        {
            binding.display_name = new_name.to_string();
            if let Some(topic) = &mut binding.topic_name {
                *topic = new_name.to_string();
            }
            let wid = WindowId(binding.window_id.clone());
            let _ = self.tmux_mgr.rename_window(&wid, new_name).await;
            if let Some(ws) = state.window_states.get_mut(&binding.window_id) {
                ws.window_name = new_name.to_string();
            }
            self.state_mgr.save_state(&state).await?;
        }
        Ok(())
    }

    /// Update the binding's agent_type to match the actual running process.
    async fn handle_rebind_agent(
        &self,
        _target: &MessageTarget,
        user_id: i64,
        thread_id: i64,
        agent_name: &str,
    ) -> Result<()> {
        let mut state = self.state_mgr.load_state().await?;
        if let Some(binding) = state
            .thread_bindings
            .iter()
            .find(|b| b.user_id == user_id && b.thread_id == thread_id)
            && let Some(ws) = state.window_states.get_mut(&binding.window_id)
        {
            ws.agent_type = agent_name.to_string();
            self.state_mgr.save_state(&state).await?;
        }
        Ok(())
    }

    /// Detect the actual agent type from a running process name.
    fn detect_running_agent(&self, command: &str) -> Option<&'static str> {
        self.config
            .agent_registry
            .iter()
            .find(|a| command.contains(a.name()))
            .map(|a| a.name())
    }
}

///
/// Establishes a baseline pane capture, then polls every 1.5s for new output.
/// Sends/edits a message in the chat with the accumulated output.
/// Stops when a shell prompt is detected or 30s timeout.
async fn run_capture_loop(
    tmux: TmuxManager,
    im: Arc<dyn ImAdapter>,
    window_id: String,
    target: MessageTarget,
) -> Result<()> {
    let wid = WindowId(window_id);

    // Establish baseline (content before the command)
    let baseline = match tmux.capture_pane(&wid).await {
        Ok(c) => strip_ansi(&c),
        Err(_) => return Ok(()),
    };
    let baseline_lines: Vec<&str> = baseline.lines().collect();
    let baseline_count = baseline_lines.len();

    let mut msg_id: Option<MessageId> = None;
    let start = std::time::Instant::now();
    let max_duration = Duration::from_secs(30);
    let poll_interval = Duration::from_millis(1500);

    loop {
        tokio::time::sleep(poll_interval).await;

        if start.elapsed() > max_duration {
            let timeout_msg = "⏱️ Command timed out (30s)";
            if let Some(mid) = &msg_id {
                let _ = im.edit_message(&target, mid, timeout_msg).await;
            } else {
                let _ = im.send_message(&target, timeout_msg).await;
            }
            break;
        }

        let content = match tmux.capture_pane(&wid).await {
            Ok(c) => strip_ansi(&c),
            Err(_) => break,
        };

        let lines: Vec<&str> = content.lines().collect();
        if lines.len() <= baseline_count {
            continue;
        }

        let new_output: Vec<&str> = lines.iter().copied().skip(baseline_count).collect();
        let joined = new_output.join("\n").trim().to_string();
        if joined.is_empty() {
            continue;
        }

        // Truncate if too long
        let display = if joined.len() > MAX_MSG_LEN {
            let truncated: String = joined.chars().take(MAX_MSG_LEN).collect();
            format!(
                "```\n{}…\n```\n_(truncated, {}s elapsed)_",
                truncated,
                start.elapsed().as_secs()
            )
        } else {
            format!(
                "```\n{}\n```\n_({}s elapsed)_",
                joined,
                start.elapsed().as_secs()
            )
        };

        if let Some(mid) = &msg_id {
            let _ = im.edit_message(&target, mid, &display).await;
        } else {
            if let Ok(mid) = im.send_message(&target, &display).await {
                msg_id = Some(mid);
            }
        }

        // Check if command finished
        if is_shell_prompt(&joined) {
            let final_msg = format!("✅ Done:\n```\n{}\n```", joined);
            if let Some(mid) = &msg_id {
                let _ = im.edit_message(&target, mid, &final_msg).await;
            }
            break;
        }
    }

    Ok(())
}

/// Check if a process name is a shell (agent has likely exited).
fn is_shell_process(process: &str) -> bool {
    matches!(process, "zsh" | "bash" | "sh" | "fish" | "dash" | "ksh")
}

/// Build the launch command string for an agent (command + extra args).
fn agent_launch_cmd(agent: &atim_core::agent::AgentHandle) -> String {
    let cmd = agent.new_session_command();
    let args = agent.extra_args();
    if args.is_empty() {
        cmd
    } else {
        format!("{} {}", cmd, args.join(" "))
    }
}

/// Detect whether the tail of command output contains a shell/agent prompt.
fn is_shell_prompt(output: &str) -> bool {
    for line in output.lines().rev() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        // Common shell prompt characters
        if trimmed == "$"
            || trimmed == "❯"
            || trimmed == "%"
            || trimmed == "#"
            || trimmed == ">"
            || trimmed == ">>>"
        {
            return true;
        }
        // Lines ending with $, ❯ but with short length (likely prompt)
        if trimmed.len() <= 5
            && (trimmed.ends_with('$')
                || trimmed.ends_with('❯')
                || trimmed.ends_with('#')
                || trimmed.ends_with('%'))
        {
            return true;
        }
        break; // Only check the last non-empty line
    }
    false
}

/// Strip ANSI escape sequences from a string.
fn strip_ansi(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut in_escape = false;
    for c in s.chars() {
        if in_escape {
            if c.is_ascii_alphabetic() {
                in_escape = false;
            }
            continue;
        }
        if c == '\x1b' {
            in_escape = true;
            continue;
        }
        out.push(c);
    }
    out
}

/// Parse Claude Code usage modal output into a readable summary.
///
/// Strips ANSI, finds the usage section, and extracts key metrics
/// (tokens, cost, sessions) into a clean text format.
fn parse_usage_output(raw: &str) -> String {
    let clean = strip_ansi(raw);

    let mut lines: Vec<String> = Vec::new();
    let mut in_usage = false;
    let mut found_data = false;

    for line in clean.lines() {
        let trimmed = line.trim();

        // Detect start of usage section
        if trimmed.contains("Usage") || trimmed.contains("usage") {
            in_usage = true;
            continue;
        }

        if !in_usage {
            // Also check for common box-drawing chars around "Usage"
            if trimmed
                .replace(['─', '╭', '│'], "")
                .trim()
                .eq_ignore_ascii_case("usage")
            {
                in_usage = true;
            }
            continue;
        }

        // Stop at common modal boundaries
        if trimmed.contains("q to close")
            || trimmed.contains("Press")
            || trimmed.starts_with("══")
            || trimmed.starts_with("╰")
            || trimmed.starts_with("╭")
        {
            break;
        }

        // Skip separator lines (box drawing, dashes, etc.)
        if trimmed.chars().all(|c| {
            c == '─'
                || c == '═'
                || c == '╭'
                || c == '╮'
                || c == '╰'
                || c == '╯'
                || c == '├'
                || c == '┤'
                || c == '│'
                || c == ' '
                || c == '═'
                || c == '━'
                || c == '┃'
        }) {
            continue;
        }
        if trimmed.is_empty() || trimmed == "│" {
            continue;
        }

        // Replace multiple spaces with single space
        let normalized: String = trimmed
            .chars()
            .filter(|c| !c.is_control())
            .collect::<String>()
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ");

        if normalized.is_empty() {
            continue;
        }

        // Clean up any remaining box-drawing chars
        let cleaned = normalized.replace(['│', '┃'], "").trim().to_string();

        if !cleaned.is_empty() {
            found_data = true;
            lines.push(cleaned);
        }
    }

    if !found_data {
        // Fallback: just return a summary with token/cost info found anywhere
        let fallback = extract_usage_fallback(&clean);
        if !fallback.is_empty() {
            return fallback;
        }
        return "No usage data available. The /usage modal may not be supported in this version."
            .into();
    }

    format!("📊 Usage:\n{}", lines.join("\n"))
}

/// Fallback: scan the entire captured text for usage-like patterns.
fn extract_usage_fallback(text: &str) -> String {
    let mut results = Vec::new();
    let patterns = [
        ("Input tokens", "token"),
        ("Output tokens", "token"),
        ("Total tokens", "token"),
        ("Cost", "$"),
        ("Sessions", "session"),
        ("Total input", "input"),
        ("Total output", "output"),
    ];

    for line in text.lines() {
        for (label, _) in &patterns {
            if line.to_lowercase().contains(&label.to_lowercase()) {
                let cleaned: String = line.chars().filter(|c| !c.is_control()).collect();
                let parts: Vec<&str> = cleaned.split_whitespace().collect();
                if parts.len() >= 2 {
                    let val = parts.last().unwrap_or(&"");
                    if val.contains(|c: char| c.is_ascii_digit()) {
                        results.push(format!("  {label}: {val}"));
                    }
                }
                break;
            }
        }
    }

    if results.is_empty() {
        String::new()
    } else {
        let mut out = "📊 Usage:\n".to_string();
        for r in results {
            out.push_str(&r);
            out.push('\n');
        }
        out.trim_end().to_string()
    }
}

/// Transcribe an OGG voice message using OpenAI's gpt-4o-transcribe model.
async fn transcribe_voice(api_key: &str, base_url: &str, audio_data: &[u8]) -> Result<String> {
    let url = format!("{}/audio/transcriptions", base_url.trim_end_matches('/'));

    let part = reqwest::multipart::Part::bytes(audio_data.to_vec())
        .file_name("voice.ogg")
        .mime_str("audio/ogg")
        .map_err(|e| atim_core::error::Error::Config(format!("mime error: {e}")))?;

    let form = reqwest::multipart::Form::new()
        .text("model", "gpt-4o-transcribe")
        .part("file", part);

    let client = reqwest::Client::new();
    let resp = client
        .post(&url)
        .header("Authorization", format!("Bearer {api_key}"))
        .multipart(form)
        .timeout(Duration::from_secs(30))
        .send()
        .await
        .map_err(|e| {
            atim_core::error::Error::Telegram(format!("transcription request failed: {e}"))
        })?;

    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        return Err(atim_core::error::Error::Telegram(format!(
            "transcription API error (HTTP {status}): {body}"
        )));
    }

    let json: serde_json::Value = resp
        .json()
        .await
        .map_err(|e| atim_core::error::Error::Telegram(format!("transcription JSON parse: {e}")))?;

    Ok(json["text"].as_str().unwrap_or("").to_string())
}

/// Map an interactive UI kind to a display-friendly name.
fn ui_display_name(kind: UiKind) -> &'static str {
    match kind {
        UiKind::AskUserQuestion => "Ask user",
        UiKind::ExitPlanMode => "Plan review",
        UiKind::PermissionPrompt => "Permission request",
        UiKind::BashApproval => "Bash approval",
        UiKind::RestoreCheckpoint => "Restore checkpoint",
        UiKind::Settings => "Settings",
        UiKind::Unknown => "Interactive prompt",
    }
}

/// Truncate UI content for the message preview.
fn truncate_ui_content(content: &str, max_len: usize) -> String {
    let stripped = strip_ansi(content);
    if stripped.len() <= max_len {
        stripped
    } else {
        let end = stripped.floor_char_boundary(max_len);
        format!("{}...", &stripped[..end])
    }
}

/// Build inline keyboard buttons for an interactive UI.
fn ui_to_buttons(ui: &InteractiveUi) -> Vec<Vec<Button>> {
    match ui.kind {
        UiKind::AskUserQuestion | UiKind::ExitPlanMode => {
            vec![
                vec![
                    Button {
                        text: "⬆ Up".into(),
                        callback_data: "ui:up".into(),
                    },
                    Button {
                        text: "⬇ Down".into(),
                        callback_data: "ui:down".into(),
                    },
                ],
                vec![
                    Button {
                        text: "✔ Select".into(),
                        callback_data: "ui:enter".into(),
                    },
                    Button {
                        text: "✖ Cancel".into(),
                        callback_data: "ui:esc".into(),
                    },
                ],
            ]
        }
        UiKind::PermissionPrompt | UiKind::BashApproval => {
            vec![
                vec![
                    Button {
                        text: "✔ Yes".into(),
                        callback_data: "ui:yes".into(),
                    },
                    Button {
                        text: "✖ No".into(),
                        callback_data: "ui:no".into(),
                    },
                ],
                vec![Button {
                    text: "✖ Cancel".into(),
                    callback_data: "ui:esc".into(),
                }],
            ]
        }
        UiKind::RestoreCheckpoint => {
            vec![
                vec![
                    Button {
                        text: "⬆ Up".into(),
                        callback_data: "ui:up".into(),
                    },
                    Button {
                        text: "⬇ Down".into(),
                        callback_data: "ui:down".into(),
                    },
                ],
                vec![
                    Button {
                        text: "✔ Restore".into(),
                        callback_data: "ui:enter".into(),
                    },
                    Button {
                        text: "✖ Skip".into(),
                        callback_data: "ui:esc".into(),
                    },
                ],
            ]
        }
        UiKind::Settings => {
            vec![
                vec![
                    Button {
                        text: "⬆ Up".into(),
                        callback_data: "ui:up".into(),
                    },
                    Button {
                        text: "⬇ Down".into(),
                        callback_data: "ui:down".into(),
                    },
                ],
                vec![
                    Button {
                        text: "✔ Select".into(),
                        callback_data: "ui:enter".into(),
                    },
                    Button {
                        text: "Esc".into(),
                        callback_data: "ui:esc".into(),
                    },
                ],
            ]
        }
        UiKind::Unknown => {
            vec![vec![Button {
                text: "Esc".into(),
                callback_data: "ui:esc".into(),
            }]]
        }
    }
}

/// Handle a UI navigation callback by sending the corresponding key to the session.
async fn handle_ui_callback(
    tmux: &TmuxManager,
    window_id: &str,
    callback_data: &str,
) -> Result<()> {
    let wid = WindowId(window_id.to_string());
    match callback_data {
        "ui:up" => tmux.send_key(&wid, "Up").await?,
        "ui:down" => tmux.send_key(&wid, "Down").await?,
        "ui:enter" => tmux.send_key(&wid, "Enter").await?,
        "ui:esc" => tmux.send_key(&wid, "Escape").await?,
        "ui:yes" => {
            tmux.send_key(&wid, "Enter").await?; // Enter to select Yes
        }
        "ui:no" => {
            tmux.send_key(&wid, "Tab").await?; // Tab to No, then Enter
            tmux.send_key(&wid, "Enter").await?;
        }
        _ => {}
    }
    Ok(())
}

/// Query zoxide for the best matching directory.
///
/// Runs `z query <text>` and returns the first matching path, or `None`
/// if zoxide isn't installed or finds no match.
async fn zoxide_query(text: &str) -> anyhow::Result<Option<PathBuf>> {
    let output = tokio::process::Command::new("zoxide")
        .arg("query")
        .arg(text)
        .output()
        .await
        .map_err(|e| anyhow::anyhow!("zoxide query failed: {e}"))?;

    if output.status.success() {
        let path_str = String::from_utf8_lossy(&output.stdout).trim().to_string();
        if !path_str.is_empty() {
            return Ok(Some(PathBuf::from(path_str)));
        }
    }
    Ok(None)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    #[test]
    fn test_make_and_validate_callback_token() {
        let mut contexts = HashMap::new();
        let token = Server::make_callback_token(&mut contexts, 100, -456, 789);
        assert_eq!(contexts.len(), 1);

        let ctx = Server::validate_callback_token(&mut contexts, &token);
        assert_eq!(ctx, Some((100, -456, 789)));
        assert!(contexts.is_empty());
    }

    #[test]
    fn test_callback_token_rejects_stale() {
        let mut contexts = HashMap::new();
        let token = Server::make_callback_token(&mut contexts, 100, -456, 789);
        assert_eq!(contexts.len(), 1);

        // Valid once
        let ctx = Server::validate_callback_token(&mut contexts, &token);
        assert!(ctx.is_some());

        // Stale — already consumed
        let ctx = Server::validate_callback_token(&mut contexts, &token);
        assert!(ctx.is_none());
    }

    #[test]
    fn test_callback_token_rejects_unknown() {
        let mut contexts = HashMap::new();
        let ctx = Server::validate_callback_token(&mut contexts, "deadbeef");
        assert!(ctx.is_none());
    }

    #[test]
    fn test_make_callback_token_different_contexts() {
        let mut contexts = HashMap::new();
        let t1 = Server::make_callback_token(&mut contexts, 100, -456, 789);
        let t2 = Server::make_callback_token(&mut contexts, 200, -456, 789);
        assert_ne!(t1, t2);
        assert_eq!(contexts.len(), 2);
    }

    #[test]
    fn test_callback_token_embedded_format() {
        let mut contexts = HashMap::new();
        let token = Server::make_callback_token(&mut contexts, 100, -456, 789);

        // Simulate building the callback_data string
        let callback_data = format!("cb:{token}:bind:@0");
        assert!(callback_data.starts_with("cb:"));
        assert!(callback_data.ends_with(":bind:@0"));

        // Extract token back
        let extracted = callback_data
            .strip_prefix("cb:")
            .and_then(|s| s.split(':').next())
            .unwrap();
        let ctx = Server::validate_callback_token(&mut contexts, extracted);
        assert_eq!(ctx, Some((100, -456, 789)));
    }

    #[test]
    fn test_strip_ansi_basic() {
        assert_eq!(strip_ansi("hello"), "hello");
        assert_eq!(strip_ansi("\x1b[31mred\x1b[0m"), "red");
        assert_eq!(strip_ansi("\x1b[1mbold\x1b[22m"), "bold");
    }

    #[test]
    fn test_parse_usage_output_clean() {
        let input = "╭──────────────────────╮\n│ Usage                │\n├──────────────────────┤\n│ Input tokens    1234 │\n│ Output tokens   5678 │\n│ Total tokens   6912  │\n│ Cost           $0.12 │\n├──────────────────────┤\n│ Press q to close     │\n╰──────────────────────╯\n";
        let result = parse_usage_output(input);
        assert!(result.contains("Usage"));
        assert!(result.contains("Input tokens"));
        assert!(result.contains("1234"));
        assert!(result.contains("Cost"));
        assert!(result.contains("$0.12"));
    }

    #[test]
    fn test_parse_usage_output_with_ansi() {
        let input = "\x1b[32m╭──────────────────────╮\n\x1b[0m│ \x1b[1mUsage\x1b[0m                │\n├──────────────────────┤\n│ Input tokens    1234 │\n│ Output tokens   5678 │\n│ Total tokens   6912  │\n│ Cost           $0.12 │\n│ Press q to close     │\n╰──────────────────────╯\n";
        let result = parse_usage_output(input);
        assert!(result.contains("Input tokens"));
        assert!(result.contains("1234"));
    }

    #[test]
    fn test_parse_usage_output_no_data() {
        let result = parse_usage_output("just some random text\nwith no usage info\n");
        assert!(result.contains("No usage data"));
    }

    #[test]
    fn test_extract_usage_fallback_simple() {
        let text = "some preamble\nInput tokens: 1,234\nOutput tokens: 5,678\nmore text\n";
        let result = extract_usage_fallback(text);
        assert!(result.contains("Input tokens"));
        assert!(result.contains("1,234"));
    }

    #[test]
    fn test_extract_usage_fallback_empty() {
        let result = extract_usage_fallback("nothing useful here");
        assert_eq!(result, "");
    }

    #[test]
    fn test_ui_display_name_all_kinds() {
        assert_eq!(ui_display_name(UiKind::AskUserQuestion), "Ask user");
        assert_eq!(
            ui_display_name(UiKind::PermissionPrompt),
            "Permission request"
        );
        assert_eq!(ui_display_name(UiKind::BashApproval), "Bash approval");
        assert_eq!(ui_display_name(UiKind::ExitPlanMode), "Plan review");
        assert_eq!(
            ui_display_name(UiKind::RestoreCheckpoint),
            "Restore checkpoint"
        );
        assert_eq!(ui_display_name(UiKind::Settings), "Settings");
        assert_eq!(ui_display_name(UiKind::Unknown), "Interactive prompt");
    }

    #[test]
    fn test_ui_to_buttons_permission() {
        let ui = InteractiveUi {
            kind: UiKind::PermissionPrompt,
            content: "Do you want to proceed?".to_string(),
        };
        let buttons = ui_to_buttons(&ui);
        assert_eq!(buttons.len(), 2);
        assert_eq!(buttons[0][0].text, "✔ Yes");
        assert_eq!(buttons[0][1].text, "✖ No");
        assert_eq!(buttons[0][0].callback_data, "ui:yes");
    }

    #[test]
    fn test_ui_to_buttons_ask_question() {
        let ui = InteractiveUi {
            kind: UiKind::AskUserQuestion,
            content: "Choose an option:".to_string(),
        };
        let buttons = ui_to_buttons(&ui);
        assert_eq!(buttons.len(), 2);
        assert_eq!(buttons[0][0].text, "⬆ Up");
        assert_eq!(buttons[0][1].text, "⬇ Down");
        assert_eq!(buttons[1][0].text, "✔ Select");
        assert_eq!(buttons[1][1].text, "✖ Cancel");
    }

    #[test]
    fn test_ui_to_buttons_unknown() {
        let ui = InteractiveUi {
            kind: UiKind::Unknown,
            content: "something".to_string(),
        };
        let buttons = ui_to_buttons(&ui);
        assert_eq!(buttons.len(), 1);
        assert_eq!(buttons[0][0].text, "Esc");
    }

    #[test]
    fn test_truncate_ui_content_short() {
        let result = truncate_ui_content("short text", 100);
        assert_eq!(result, "short text");
    }

    #[test]
    fn test_truncate_ui_content_long() {
        let long = "a".repeat(500);
        let result = truncate_ui_content(&long, 200);
        assert_eq!(result.len(), 203); // 200 + "..."
        assert!(result.ends_with("..."));
    }

    #[test]
    fn test_truncate_ui_strips_ansi() {
        let input = "\x1B[32mHello\x1B[0m World";
        let result = truncate_ui_content(input, 100);
        assert_eq!(result, "Hello World");
    }
}
