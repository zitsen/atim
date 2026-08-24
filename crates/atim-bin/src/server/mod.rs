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
    Button, ChatId, CheckItem, CheckStatus, ImEvent, ImEventKind, MessageId, MessageTarget,
    ThreadId, WindowId,
};
use atim_core::message::{InteractiveUi, UiKind};
use atim_core::session::{ChatBinding, RuntimeState, SessionInfo, WindowBinding};
use atim_core::terminal::TerminalManager;
use atim_monitor::monitor::{MonitorEvent, resolve_jsonl};
use atim_queue::message_queue::MessageQueue;
use atim_state::persistence::StateManager;
use tokio::sync::Mutex;

use crate::browser;
use crate::browser::{BrowserMode, DirectoryBrowser};

mod recovery;

/// Key type for per-user pending state: (user_id, thread_id).
/// thread_id serves as the sole chat identifier — for Feishu, thread_id == chat_id.
type UserTriple = (i64, i64);
/// Key type for tool_use message tracking: (chat_id, thread_id, tool_use_id).
type ToolUseMsgKey = (i64, i64, String);

/// Terminal manager type used by the server.
///
/// On Linux/macOS this is `TmuxManager` (tmux CLI). On Windows it is
/// the ConPTY-based `WindowsTerminalManager`.
pub type TerminalMgr = Arc<dyn TerminalManager>;

/// Callback context stored for each inline-keyboard token:
/// (user_id, thread_id, created_at). Created_at enables stale-token cleanup.
type CallbackCtx = (i64, i64, std::time::Instant);

/// The main application server — routes IM events to tmux and monitor
/// events back to IM.
pub struct Server {
    pub config: Config,
    pub state_mgr: StateManager,
    pub tmux_mgr: TerminalMgr,
    /// Message queue for IM message ordering (reserved for future use).
    #[allow(dead_code)]
    pub queue: Arc<Mutex<MessageQueue>>,
    /// Shared byte offsets for monitor (used in pipe handler directly).
    pub byte_offsets: Arc<Mutex<HashMap<String, u64>>>,
    pub im_adapter: Arc<dyn ImAdapter>,
    /// Track topic names by (chat_id, thread_id) from forum_topic_created/edited.
    pub topic_names: Arc<Mutex<HashMap<(i64, i64), String>>>,
    /// Pending user messages awaiting callback selection: (user_id, thread_id) -> text.
    pub pending_messages: Arc<Mutex<HashMap<UserTriple, String>>>,
    /// Directory browser for session creation with project navigation.
    pub browser: DirectoryBrowser,
    /// Tool_use message tracking for in-place editing:
    /// key = (chat_id, thread_id, tool_use_id) -> message_id of the sent tool_use summary.
    pub tool_use_msg_ids: Arc<Mutex<HashMap<ToolUseMsgKey, MessageId>>>,
    /// Status message tracking for status→content conversion:
    /// key = (chat_id, thread_id) -> whether status has been consumed by first content.
    pub status_consumed: Arc<Mutex<HashSet<(i64, i64)>>>,
    /// Pending agent selection per (user_id, thread_id) during setup workflow.
    pub pending_agents: Arc<Mutex<HashMap<UserTriple, String>>>,
    /// Interactive UI detection cache: window_id -> hash of last detected UI content.
    pub last_ui_states: Arc<Mutex<HashMap<String, String>>>,
    /// Last pane output per window (for non-Claude agents without JSONL logs).
    pub last_pane_output: Arc<Mutex<HashMap<String, String>>>,
    /// Pending chat names: (user_id, thread_id) -> IM chat/group name.
    /// Set from the first text message's target.chat_name before any callback
    /// resolves. Used as the window name / binding display_name so the window
    /// reflects the actual chat name instead of a generic "atim-{user_id}".
    pub pending_chat_names: Arc<Mutex<HashMap<UserTriple, String>>>,
    /// Pending rename names: (user_id, thread_id) -> new chat_name.
    /// Set when the rename prompt is shown; consumed by the rename callback.
    pub pending_rename_names: Arc<Mutex<HashMap<UserTriple, String>>>,
    /// Callback context tokens for validating inline keyboard callbacks.
    /// Stores (user_id, thread_id, created_at) for each callback token.
    pub callback_contexts: Arc<Mutex<HashMap<String, CallbackCtx>>>,
    /// Track which chat_ids have received the welcome message (in-memory, resets on restart).
    pub welcome_sent: Arc<Mutex<HashSet<i64>>>,
}

/// Maximum Telegram message length for merged content.
const MAX_MSG_LEN: usize = 3800;

/// Pre-compiled regex for extracting the Session ID from `/status` output.
static SESSION_ID_RE: std::sync::LazyLock<regex::Regex> = std::sync::LazyLock::new(|| {
    regex::Regex::new(
        r"Session ID:\s+([0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12})",
    )
    .expect("SESSION_ID_RE is valid")
});

impl Server {
    /// Generate a short callback context token and store the validation context.
    ///
    /// Returns a hex token that can be embedded in callback_data.
    /// thread_id is the sole chat identifier — for Feishu, thread_id == chat_id.
    /// Each token records its creation time so stale tokens can be cleaned up.
    fn make_callback_token(
        contexts: &mut HashMap<String, CallbackCtx>,
        user_id: i64,
        thread_id: i64,
    ) -> String {
        use std::hash::{Hash, Hasher};
        let counter = contexts.len() as u64;
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        user_id.hash(&mut hasher);
        thread_id.hash(&mut hasher);
        counter.hash(&mut hasher);
        let token = format!("{:x}", hasher.finish());
        contexts.insert(
            token.clone(),
            (user_id, thread_id, std::time::Instant::now()),
        );
        token
    }

    /// Validate a callback context token and extract the stored context.
    /// Returns (user_id, thread_id).
    fn validate_callback_token(
        contexts: &mut HashMap<String, CallbackCtx>,
        token: &str,
    ) -> Option<(i64, i64)> {
        let (user_id, thread_id, _created) = contexts.remove(token)?;
        Some((user_id, thread_id))
    }

    /// Remove callback tokens older than `max_age`, preventing unbounded growth
    /// when users never tap the inline keyboard buttons.
    fn cleanup_stale_callback_tokens(
        contexts: &mut HashMap<String, CallbackCtx>,
        max_age: std::time::Duration,
    ) -> usize {
        let cutoff = std::time::Instant::now() - max_age;
        let before = contexts.len();
        contexts.retain(|_, (_, _, created)| *created >= cutoff);
        before - contexts.len()
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
        // Periodically clean up stale callback tokens (every 10 min) to prevent
        // unbounded growth when users never tap the inline keyboard buttons.
        let mut cleanup_interval = tokio::time::interval(std::time::Duration::from_secs(600));
        cleanup_interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

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
                _ = cleanup_interval.tick() => {
                    let mut guard = self.callback_contexts.lock().await;
                    let removed = Self::cleanup_stale_callback_tokens(
                        &mut guard,
                        std::time::Duration::from_secs(600),
                    );
                    if removed > 0 {
                        tracing::info!("Cleaned up {removed} stale callback tokens");
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
            message_id: _,
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
                message_id,
            } => {
                self.handle_text_message(
                    event.target,
                    event.user_id.0,
                    &text,
                    is_mention,
                    is_group,
                    message_id,
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
            ImEventKind::BotAdded { .. } => {
                self.handle_bot_added(&event.target).await?;
            }
        }

        Ok(())
    }

    async fn handle_monitor_event(&self, event: MonitorEvent) -> Result<()> {
        match event {
            MonitorEvent::NewMessages(messages) => {
                let rt = self.state_mgr.load_runtime().await?;

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
                    // Resolve to chat binding by session_id directly (V2)
                    let binding = match rt.chat_bindings.iter().find(|cb| cb.session_id == *_sid) {
                        Some(cb) => cb.clone(),
                        None => {
                            tracing::warn!(
                                "[pipe] No chat_binding for session_id={} (have {} bindings)",
                                _sid,
                                rt.chat_bindings.len(),
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
                                // AskUserQuestion: send interactive card with option buttons
                                if msg.tool_name.as_deref() == Some("AskUserQuestion")
                                    && let Some(ref raw) = msg.raw_input
                                    && let Ok(mid) =
                                        self.send_ask_user_card(&target, raw, &msg.text).await
                                {
                                    if let Some(tuid) = &msg.tool_use_id {
                                        self.tool_use_msg_ids
                                            .lock()
                                            .await
                                            .insert((chat_id, thread_id_val, tuid.clone()), mid);
                                    }
                                    continue;
                                }
                                // Edit: include diff content in the card
                                if matches!(
                                    msg.tool_name.as_deref(),
                                    Some("Edit" | "EditTool" | "TextEditTool")
                                ) && let Some(ref raw) = msg.raw_input
                                {
                                    let diff_text = build_edit_diff_card(raw, &msg.text);
                                    if let Ok(mid) =
                                        self.im_adapter.send_message(&target, &diff_text).await
                                        && let Some(tuid) = &msg.tool_use_id
                                    {
                                        self.tool_use_msg_ids
                                            .lock()
                                            .await
                                            .insert((chat_id, thread_id_val, tuid.clone()), mid);
                                    }
                                    continue;
                                }
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
                                // Take the tracked message id out of the map BEFORE awaiting,
                                // so we don't hold the lock across network I/O.
                                let tracked_mid = if let Some(tuid) = &msg.tool_use_id {
                                    self.tool_use_msg_ids.lock().await.remove(&(
                                        chat_id,
                                        thread_id_val,
                                        tuid.clone(),
                                    ))
                                } else {
                                    None
                                };
                                if let Some(mid) = tracked_mid {
                                    // Edit tool: diff card already sent, just append result
                                    let is_edit = matches!(
                                        msg.tool_name.as_deref(),
                                        Some("Edit" | "EditTool" | "TextEditTool")
                                    );
                                    if !is_edit {
                                        let _ = self
                                            .im_adapter
                                            .edit_message(&target, &mid, &msg.text)
                                            .await;
                                    }
                                } else {
                                    let _ = self.im_adapter.send_message(&target, &msg.text).await;
                                }
                            }
                            _ => {
                                // Text — accumulate for merging.
                                // Tables are handled by each adapter separately:
                                // Telegram converts to card-style, Feishu renders natively.
                                if !merged.is_empty() {
                                    merged.push('\n');
                                }
                                merged.push_str(&msg.text);
                                if merged.len() >= MAX_MSG_LEN {
                                    flush!();
                                }
                            }
                        }
                    }

                    flush!();
                }

                // Persist the updated byte offsets to SQLite so they survive restarts.
                // The monitor updates `byte_offsets` in memory after reading each JSONL
                // batch; this syncs those updates to the DB on every processed event so
                // the server never re-reads already-delivered messages after a restart.
                {
                    let offsets = self.byte_offsets.lock().await;
                    for sid in by_session.keys() {
                        if let Some(&offset) = offsets.get(sid) {
                            let _ = self.state_mgr.upsert_offset(sid, offset).await;
                        }
                    }
                }
            }
            MonitorEvent::SessionMapChanged => {
                tracing::info!("[pipe] SessionMapChanged — syncing session IDs to window bindings");
                let session_map = self.state_mgr.consume_hook_session_map().await?;
                let mut rt = self.state_mgr.load_runtime().await?;
                let mut synced = 0;
                for (window_id, session_id) in &session_map {
                    // Find window_binding by window_id
                    if let Some(wb) = rt.window_bindings.get_mut(window_id) {
                        // Only assign session_ids for agents that support
                        // tracked sessions — skip agents with no JSONL logs.
                        if let Some(agent) = self.config.agent_registry.get(&wb.agent_type)
                            && !agent.supports_sessions()
                        {
                            continue;
                        }
                        if wb.session_id.is_empty() {
                            wb.session_id = session_id.clone();
                            synced += 1;
                            tracing::info!(
                                "[pipe] Assigned session {session_id} to window {window_id}"
                            );
                        } else if wb.session_id != *session_id {
                            tracing::debug!(
                                "[pipe] Window {window_id} has session {} but map says {session_id} — updating",
                                wb.session_id,
                            );
                            wb.session_id = session_id.clone();
                            synced += 1;
                        }
                    } else {
                        tracing::warn!(
                            "[pipe] Session map has window {window_id} but no WindowBinding exists for it",
                        );
                    }
                    // Also sync the chat binding's session_id so find_cb() works.
                    // Match by display_name == window_name (stable link across session changes).
                    // Update when empty (first assignment) or stale (session UUID changed).
                    if let Some(wb) = rt.window_bindings.get(window_id) {
                        let window_name = wb.window_name.clone();
                        if let Some(cb) = rt.chat_bindings.iter_mut().find(|cb| {
                            cb.display_name == window_name
                                && (cb.session_id.is_empty() || cb.session_id != *session_id)
                        }) {
                            if cb.session_id.is_empty() {
                                tracing::info!(
                                    "[pipe] Assigned session {session_id} to chat binding '{}' (user={} thread={})",
                                    cb.display_name,
                                    cb.user_id,
                                    cb.thread_id,
                                );
                            } else {
                                tracing::info!(
                                    "[pipe] Updated stale session {} → {session_id} for chat binding '{}' (user={} thread={})",
                                    cb.session_id,
                                    cb.display_name,
                                    cb.user_id,
                                    cb.thread_id,
                                );
                            }
                            cb.session_id = session_id.clone();
                        }
                    }
                }
                tracing::info!("[pipe] SessionMapChanged: synced {synced} entries");
                self.state_mgr.save_runtime(&rt).await?;
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
        message_id: Option<String>,
    ) -> Result<()> {
        // Load runtime state to find chat binding (V2)
        let rt = self.state_mgr.load_runtime().await?;

        // ── /atim command (meta-commands, no binding needed) ──
        if let Some(atim_cmd) = text.trim().strip_prefix("/atim ") {
            let subcommand = atim_cmd.trim();
            return self.handle_atim_command(&target, user_id, subcommand).await;
        }
        if text.trim() == "/atim" {
            let _ = self
                .im_adapter
                .send_message(&target, "Available subcommands: `/atim help`")
                .await;
            return Ok(());
        }

        // ── replyAtOnly filter: skip non-@mention messages in group chats ──
        if is_group && !is_mention {
            let thread_id_val = target.thread_id.map(|t| t.0).unwrap_or(0);
            let should_skip = rt
                .chat_bindings
                .iter()
                .find(|b| b.user_id == user_id && b.thread_id == thread_id_val)
                .is_some_and(|b| b.reply_at_only);
            if should_skip {
                tracing::debug!(
                    "[handle_text_message] replyAtOnly=true, skipping non-mention message from user {user_id} in group"
                );
                return Ok(());
            }
        }

        // Helper: find chat_binding by (user_id, thread_id)
        let thread_id_val = target.thread_id.map(|t| t.0).unwrap_or(0);
        let find_cb = || -> Option<(ChatBinding, Option<&WindowBinding>)> {
            let cb = rt
                .chat_bindings
                .iter()
                .find(|b| b.user_id == user_id && b.thread_id == thread_id_val)?
                .clone();
            let wb = rt.resolve_window_binding(user_id, thread_id_val);
            Some((cb, wb))
        };

        // Check for screenshot command
        if text.trim() == "/ss" || text.trim() == "/screenshot" || text.trim() == "!ss" {
            if let Some((_binding, wb)) = find_cb() {
                let wid_str = wb.map(|w| &w.window_id).map(|s| s.as_str()).unwrap_or("");
                if wid_str.is_empty() {
                    let _ = self
                        .im_adapter
                        .send_message(&target, "No active session to capture.")
                        .await;
                    return Ok(());
                }
                let window_id = atim_core::message::WindowId(wid_str.to_string());
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

        // Check for /usage command — uses the same dismiss-then-capture flow
        // as other Claude Code built-in slash commands.
        if text.trim() == "/usage" {
            if let Some((_binding, wb)) = find_cb() {
                let wid_str = wb.map(|w| &w.window_id).map(|s| s.as_str()).unwrap_or("");
                if wid_str.is_empty() {
                    let _ = self
                        .im_adapter
                        .send_message(&target, "No active session.")
                        .await;
                    return Ok(());
                }
                let wid = WindowId(wid_str.to_string());
                if !self.tmux_mgr.window_exists(&wid).await {
                    let _ = self
                        .im_adapter
                        .send_message(&target, "Session window no longer exists.")
                        .await;
                    return Ok(());
                }
                let _ = self.im_adapter.send_chat_action(&target).await;
                self.send_slash_and_capture(&target, &wid, "/usage").await;
            } else {
                let _ = self
                    .im_adapter
                    .send_message(&target, "No active session.")
                    .await;
            }
            return Ok(());
        }

        // Claude Code built-in slash commands — capture output and return to chat
        let trimmed = text.trim();
        if matches!(
            trimmed,
            "/status" | "/doctor" | "/help" | "/compact" | "/clear"
        ) {
            if let Some((_binding, wb)) = find_cb() {
                let wid_str = wb.map(|w| &w.window_id).map(|s| s.as_str()).unwrap_or("");
                if wid_str.is_empty() {
                    let _ = self
                        .im_adapter
                        .send_message(&target, "No active session.")
                        .await;
                    return Ok(());
                }
                let wid = WindowId(wid_str.to_string());
                if !self.tmux_mgr.window_exists(&wid).await {
                    let _ = self
                        .im_adapter
                        .send_message(&target, "Session window no longer exists.")
                        .await;
                    return Ok(());
                }

                let _ = self.im_adapter.send_chat_action(&target).await;

                if matches!(trimmed, "/compact" | "/clear") {
                    // /clear resets the conversation and creates a new session UUID.
                    // We need to clear old bindings, send /clear, then discover the
                    // new UUID via /status and update bindings — like /rebind does.
                    let thread_id_val = target.thread_id.map(|t| t.0).unwrap_or(0);
                    let wid_str_owned = wb.map(|w| w.window_id.clone()).unwrap_or_default();

                    // Phase 1: clear stale session bindings (before /clear creates new session)
                    if trimmed == "/clear"
                        && !wid_str_owned.is_empty()
                        && let Some((_binding, wb)) = find_cb()
                        && !wb.map(|w| w.session_id.is_empty()).unwrap_or(true)
                    {
                        let old_sid = wb.map(|w| w.session_id.clone()).unwrap_or_default();
                        let mut rt = self.state_mgr.load_runtime().await?;
                        if let Some(wb2) = rt.window_bindings.get_mut(&wid_str_owned) {
                            wb2.session_id.clear();
                        }
                        for cb in rt.chat_bindings.iter_mut() {
                            if cb.session_id == old_sid {
                                cb.session_id.clear();
                            }
                        }
                        self.state_mgr.save_runtime(&rt).await?;
                        if let Ok(mut map) = self.state_mgr.load_session_map().await {
                            map.remove(&wid_str_owned);
                            if let Err(e) = self.state_mgr.save_session_map(&map).await {
                                tracing::warn!("[clear] Failed to save session_map: {e}");
                            }
                        }
                        if let Err(e) = self.state_mgr.remove_offset(&old_sid).await {
                            tracing::warn!("[clear] Failed to remove offset: {e}");
                        }
                        self.byte_offsets.lock().await.remove(&old_sid);
                    }

                    // Phase 2: send /clear
                    self.tmux_mgr.send_line(&wid, trimmed).await?;

                    // Phase 3: wait for new session to initialize, then discover UUID
                    let new_sid: Option<String> =
                        if trimmed == "/clear" && !wid_str_owned.is_empty() {
                            tokio::time::sleep(Duration::from_secs(2)).await;
                            self.discover_session_via_status(&wid_str_owned).await
                        } else {
                            None
                        };

                    // Phase 4: update bindings with new UUID
                    if let Some(ref sid) = new_sid {
                        let mut rt = self.state_mgr.load_runtime().await?;
                        if let Some(wb2) = rt.window_bindings.get_mut(&wid_str_owned) {
                            wb2.session_id = sid.clone();
                        }
                        for cb in rt.chat_bindings.iter_mut() {
                            if cb.user_id == user_id && cb.thread_id == thread_id_val {
                                cb.session_id = sid.clone();
                            }
                        }
                        self.state_mgr.save_runtime(&rt).await?;
                        if let Ok(mut map) = self.state_mgr.load_session_map().await {
                            map.insert(wid_str_owned.clone(), sid.clone());
                            if let Err(e) = self.state_mgr.save_session_map(&map).await {
                                tracing::warn!("[clear/rebind] Failed to save session_map: {e}");
                            }
                        }
                    }

                    let msg = if let Some(ref sid) = new_sid {
                        format!("Sent `{trimmed}` to agent (new session: {sid}).")
                    } else {
                        format!("Sent `{trimmed}` to agent.")
                    };
                    let _ = self.im_adapter.send_message(&target, &msg).await;
                } else {
                    // Commands that open a modal — capture output and dismiss
                    self.send_slash_and_capture(&target, &wid, trimmed).await;
                }
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

            if let Some((_binding, wb_opt)) = find_cb() {
                let wid_str = wb_opt
                    .map(|w| &w.window_id)
                    .map(|s| s.as_str())
                    .unwrap_or("");
                if wid_str.is_empty() {
                    let _ = self
                        .im_adapter
                        .send_message(&target, "No active session to switch.")
                        .await;
                    return Ok(());
                }
                let window_id = WindowId(wid_str.to_string());
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

                // Update window binding via V2
                let mut rt = self.state_mgr.load_runtime().await?;
                if let Some(wb) = rt.window_bindings.get_mut(wid_str) {
                    wb.agent_type = agent.name().to_string();
                    wb.session_id = String::new();
                }
                self.state_mgr.save_runtime(&rt).await?;
                // Also clear session_id from session_map so SessionMapChanged won't re-fill
                if let Ok(mut map) = self.state_mgr.load_session_map().await
                    && map.remove(wid_str).is_some()
                    && let Err(e) = self.state_mgr.save_session_map(&map).await
                {
                    tracing::warn!("[switch] Failed to save session_map: {e}");
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
            if let Some((_binding, wb_opt)) = find_cb() {
                let wid_str = wb_opt
                    .map(|w| &w.window_id)
                    .map(|s| s.as_str())
                    .unwrap_or("");
                if wid_str.is_empty() {
                    let _ = self
                        .im_adapter
                        .send_message(&target, "No active session.")
                        .await;
                    return Ok(());
                }
                let window_id = atim_core::message::WindowId(wid_str.to_string());
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

        // Check for /enter (send Enter key to confirm modals/selections)
        if text.trim() == "/enter" {
            if let Some((_binding, wb_opt)) = find_cb() {
                let wid_str = wb_opt
                    .map(|w| &w.window_id)
                    .map(|s| s.as_str())
                    .unwrap_or("");
                if wid_str.is_empty() {
                    let _ = self
                        .im_adapter
                        .send_message(&target, "No active session.")
                        .await;
                    return Ok(());
                }
                let window_id = atim_core::message::WindowId(wid_str.to_string());
                if !self.tmux_mgr.window_exists(&window_id).await {
                    let _ = self
                        .im_adapter
                        .send_message(&target, "Window no longer exists.")
                        .await;
                    return Ok(());
                }
                let _ = self.im_adapter.send_chat_action(&target).await;
                self.tmux_mgr.send_key(&window_id, "Enter").await?;
                let _ = self
                    .im_adapter
                    .send_message(&target, "Sent Enter key.")
                    .await;
            } else {
                let _ = self
                    .im_adapter
                    .send_message(&target, "No active session.")
                    .await;
            }
            return Ok(());
        }

        // Handle /unbind — send /quit to agent, kill tmux window, remove bindings
        if text.trim() == "/unbind" {
            if let Some((binding, wb)) = find_cb() {
                let sid = if wb.map(|w| w.session_id.as_str()).unwrap_or("").is_empty() {
                    // If window binding has no session_id, try the chat binding
                    if binding.session_id.is_empty() {
                        None
                    } else {
                        Some(binding.session_id.clone())
                    }
                } else {
                    Some(
                        wb.expect("wb is Some in this branch (checked by outer if)")
                            .session_id
                            .clone(),
                    )
                };

                let wid_str = wb.map(|w| &w.window_id).map(|s| s.as_str()).unwrap_or("");

                // 1. Send /quit to the agent (best-effort)
                if !wid_str.is_empty() {
                    let window_id = atim_core::message::WindowId(wid_str.to_string());
                    if self.tmux_mgr.window_exists(&window_id).await {
                        self.tmux_mgr.send_line(&window_id, "/quit").await.ok();
                        // Brief wait for Claude to exit gracefully
                        tokio::time::sleep(std::time::Duration::from_millis(1500)).await;
                        // 2. Kill the tmux window
                        self.tmux_mgr.kill_window(&window_id).await.ok();
                    }
                }

                // 3. Load mutable runtime and remove bindings
                let mut rt = self.state_mgr.load_runtime().await?;

                // Remove chat binding by (user_id, thread_id)
                rt.chat_bindings
                    .retain(|cb| !(cb.user_id == user_id && cb.thread_id == thread_id_val));
                // Remove window binding
                if !wid_str.is_empty() {
                    rt.window_bindings.remove(wid_str);
                }

                // Remove session from sessions map
                if let Some(ref session_id) = sid {
                    rt.sessions.remove(session_id);
                    // 4. Remove monitor offset
                    if let Err(e) = self.state_mgr.remove_offset(session_id).await {
                        tracing::warn!("[unbind] Failed to remove offset: {e}");
                    }
                }

                self.state_mgr.save_runtime(&rt).await?;

                let _ = self
                    .im_adapter
                    .send_message(
                        &target,
                        "✅ Session unbound. You can now start a new session.",
                    )
                    .await;
            } else {
                let _ = self
                    .im_adapter
                    .send_message(&target, "No active session to unbind.")
                    .await;
            }
            return Ok(());
        }

        // Handle /reload — quit the agent process, then resume the session
        // in the same window (keeps binding intact).
        if text.trim() == "/reload" {
            if let Some((binding, wb)) = find_cb() {
                let wid_str = wb.map(|w| &w.window_id).map(|s| s.as_str()).unwrap_or("");
                let session_id = wb
                    .map(|w| w.session_id.clone())
                    .filter(|s| !s.is_empty())
                    .or_else(|| {
                        if binding.session_id.is_empty() {
                            None
                        } else {
                            Some(binding.session_id.clone())
                        }
                    });
                let agent_type = wb.map(|w| w.agent_type.clone()).unwrap_or_default();

                let window_id = atim_core::message::WindowId(wid_str.to_string());
                if wid_str.is_empty() || !self.tmux_mgr.window_exists(&window_id).await {
                    let _ = self
                        .im_adapter
                        .send_message(&target, "No active session window found.")
                        .await;
                    return Ok(());
                }

                // 1. Send /quit to exit the agent gracefully
                self.tmux_mgr.send_line(&window_id, "/quit").await.ok();
                // Wait for the agent to exit and the pane to return to a shell
                let mut exited = false;
                for _ in 0..10 {
                    if let Ok(info) = self.tmux_mgr.find_window(&window_id).await
                        && is_shell_process(&info.current_command)
                    {
                        exited = true;
                        break;
                    }
                    tokio::time::sleep(Duration::from_millis(500)).await;
                }
                if !exited {
                    // Force kill the window if agent didn't exit cleanly
                    self.tmux_mgr.kill_window(&window_id).await.ok();
                    tokio::time::sleep(Duration::from_millis(500)).await;
                    // Re-create window in same cwd
                    let cwd = wb.map(|w| w.cwd.clone()).unwrap_or_default();
                    let _ = self.tmux_mgr.new_window(&binding.display_name, &cwd).await;
                }

                // 2. Relaunch agent (resume if we have a session_id)
                let agent = self
                    .config
                    .agent_registry
                    .get(&agent_type)
                    .cloned()
                    .unwrap_or_else(|| self.config.agent_registry.default().clone());

                let resume_or_launch = if let Some(ref sid) = session_id
                    && agent.supports_sessions()
                    && let Some(resume_cmd) = agent.resume_command(sid)
                {
                    resume_cmd
                } else {
                    agent_launch_cmd(&agent)
                };

                let result = self.tmux_mgr.send_line(&window_id, &resume_or_launch).await;
                match result {
                    Ok(()) => {
                        // Wait for agent process to start
                        let mut started = false;
                        for _ in 0..10 {
                            if let Ok(info) = self.tmux_mgr.find_window(&window_id).await
                                && !is_shell_process(&info.current_command)
                            {
                                started = true;
                                break;
                            }
                            tokio::time::sleep(Duration::from_millis(500)).await;
                        }
                        if started {
                            let _ = self
                                .im_adapter
                                .send_message(
                                    &target,
                                    if session_id.is_some() {
                                        "✅ Session reloaded (resumed)."
                                    } else {
                                        "✅ Agent reloaded."
                                    },
                                )
                                .await;
                        } else {
                            let _ = self
                                .im_adapter
                                .send_message(&target, "⚠️ Agent did not start after reload.")
                                .await;
                        }
                    }
                    Err(e) => {
                        let _ = self
                            .im_adapter
                            .send_message(&target, &format!("❌ Reload failed: {e}"))
                            .await;
                    }
                }
            } else {
                let _ = self
                    .im_adapter
                    .send_message(&target, "No active session.")
                    .await;
            }
            return Ok(());
        }

        // Handle /new — quit the current agent, then start a fresh agent
        // session in the same window (same cwd, same agent type, no resume).
        if text.trim() == "/new" {
            if let Some((binding, wb)) = find_cb() {
                let wid_str = wb.map(|w| &w.window_id).map(|s| s.as_str()).unwrap_or("");
                let window_id = atim_core::message::WindowId(wid_str.to_string());
                if wid_str.is_empty() || !self.tmux_mgr.window_exists(&window_id).await {
                    let _ = self
                        .im_adapter
                        .send_message(&target, "No active session window found.")
                        .await;
                    return Ok(());
                }

                // 1. Send /quit to exit the agent gracefully
                self.tmux_mgr.send_line(&window_id, "/quit").await.ok();
                let mut exited = false;
                for _ in 0..10 {
                    if let Ok(info) = self.tmux_mgr.find_window(&window_id).await
                        && is_shell_process(&info.current_command)
                    {
                        exited = true;
                        break;
                    }
                    tokio::time::sleep(Duration::from_millis(500)).await;
                }
                if !exited {
                    // Force kill + recreate window in same cwd
                    self.tmux_mgr.kill_window(&window_id).await.ok();
                    tokio::time::sleep(Duration::from_millis(500)).await;
                    let cwd = wb.map(|w| w.cwd.clone()).unwrap_or_default();
                    let _ = self.tmux_mgr.new_window(&binding.display_name, &cwd).await;
                }

                // 2. Launch a FRESH agent (no resume — new session)
                let agent_type = wb.map(|w| w.agent_type.clone()).unwrap_or_default();
                let agent = self
                    .config
                    .agent_registry
                    .get(&agent_type)
                    .cloned()
                    .unwrap_or_else(|| self.config.agent_registry.default().clone());
                let launch_cmd = agent_launch_cmd(&agent);

                let result = self.tmux_mgr.send_line(&window_id, &launch_cmd).await;
                match result {
                    Ok(()) => {
                        // Wait for agent process to start
                        let mut started = false;
                        for _ in 0..10 {
                            if let Ok(info) = self.tmux_mgr.find_window(&window_id).await
                                && !is_shell_process(&info.current_command)
                            {
                                started = true;
                                break;
                            }
                            tokio::time::sleep(Duration::from_millis(500)).await;
                        }
                        if started {
                            let _ = self
                                .im_adapter
                                .send_message(&target, "✅ New session started.")
                                .await;
                        } else {
                            let _ = self
                                .im_adapter
                                .send_message(&target, "⚠️ Agent did not start.")
                                .await;
                        }
                    }
                    Err(e) => {
                        let _ = self
                            .im_adapter
                            .send_message(&target, &format!("❌ Failed: {e}"))
                            .await;
                    }
                }
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
            if let Some((binding, _wb_opt)) = find_cb() {
                // Find the tmux window by matching the display_name (window name)
                // rather than relying on stale session-based bindings.
                // Fall back to target.chat_name if display_name is stale.
                let wid_str = match self.find_window_by_name(&binding.display_name).await {
                    Some(wid) => wid,
                    None => {
                        // Try the IM chat_name as fallback — display_name may be stale
                        if let Some(ref cn) = target.chat_name {
                            match self.find_window_by_name(cn).await {
                                Some(wid) => {
                                    tracing::info!(
                                        "[rebind] Found window via chat_name '{}' (display_name '{}' did not match)",
                                        cn,
                                        binding.display_name,
                                    );
                                    wid
                                }
                                None => {
                                    let _ = self
                                        .im_adapter
                                        .send_message(&target, "No active session window found.")
                                        .await;
                                    return Ok(());
                                }
                            }
                        } else {
                            let _ = self
                                .im_adapter
                                .send_message(&target, "No active session window found.")
                                .await;
                            return Ok(());
                        }
                    }
                };
                let window_id_str = wid_str.to_string();

                let window_id = atim_core::message::WindowId(window_id_str.clone());
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

                // Load a fresh mutable runtime snapshot for all reads and mutations in this handler.
                // Using a single rt avoids lost-update bugs from multiple load/save pairs.
                let mut rebind_rt = self.state_mgr.load_runtime().await?;

                // Detect running agent (owned Strings to avoid borrow issues during later mutations)
                let detected_agent = self.detect_running_agent(&win_info.current_command);
                let stored_agent_str = rebind_rt
                    .window_bindings
                    .get(&window_id_str)
                    .map(|wb| wb.agent_type.clone())
                    .unwrap_or_default();

                let agent_type: String = if let Some(da) = detected_agent {
                    if stored_agent_str.as_str() != da {
                        changes.push(format!(
                            "agent: {} → {}",
                            if stored_agent_str.is_empty() {
                                "?"
                            } else {
                                &stored_agent_str
                            },
                            da,
                        ));
                    }
                    da.to_string()
                } else {
                    if stored_agent_str.is_empty() {
                        "claude".to_string()
                    } else {
                        stored_agent_str.clone()
                    }
                };

                let agent = self
                    .config
                    .agent_registry
                    .get(&agent_type)
                    .cloned()
                    .unwrap_or_else(|| self.config.agent_registry.default().clone());

                // Session discovery via /status command — most reliable source.
                let discovered_sid: Option<String> = if agent.supports_sessions() {
                    // Capture current pane content first (for baseline).
                    // strip_ansi is required: capture_pane uses -e (preserve ANSI), so the raw
                    // output contains escape codes that break UUID regex matching.
                    let baseline_raw = self
                        .tmux_mgr
                        .capture_pane(&window_id)
                        .await
                        .ok()
                        .unwrap_or_default();
                    let baseline = strip_ansi(&baseline_raw);
                    let baseline_len = baseline.lines().count();

                    // Send /status to Claude Code
                    self.tmux_mgr.send_line(&window_id, "/status").await.ok();
                    tokio::time::sleep(Duration::from_millis(2000)).await;

                    // Capture updated pane, strip ANSI, then extract Session ID.
                    let captured_raw = self
                        .tmux_mgr
                        .capture_pane(&window_id)
                        .await
                        .ok()
                        .unwrap_or_default();
                    let captured = strip_ansi(&captured_raw);
                    let lines: Vec<&str> = captured.lines().collect();
                    let new_text = lines
                        .iter()
                        .copied()
                        .skip(baseline_len)
                        .collect::<Vec<_>>()
                        .join("\n");

                    let mut sid = SESSION_ID_RE
                        .captures(&new_text)
                        .and_then(|c| c.get(1))
                        .map(|m| m.as_str().to_string());

                    if sid.is_none() {
                        // Try full pane as fallback (status modal may replace rather than append)
                        sid = SESSION_ID_RE
                            .captures(&captured)
                            .and_then(|c| c.get(1))
                            .map(|m| m.as_str().to_string());
                    }

                    // Dismiss /status modal — send Escape, verify, retry once
                    self.tmux_mgr.send_key(&window_id, "Escape").await.ok();
                    tokio::time::sleep(Duration::from_millis(500)).await;
                    {
                        let pane = self
                            .tmux_mgr
                            .capture_pane(&window_id)
                            .await
                            .unwrap_or_default();
                        let lower = strip_ansi(&pane).to_lowercase();
                        if lower.contains("session id") || lower.contains("/status") {
                            self.tmux_mgr.send_key(&window_id, "Escape").await.ok();
                            tokio::time::sleep(Duration::from_millis(500)).await;
                        }
                    }

                    sid
                } else {
                    None
                };

                // Capture stored_sid before any mutations (owned String)
                let stored_sid = rebind_rt
                    .window_bindings
                    .get(&window_id_str)
                    .map(|wb| wb.session_id.clone())
                    .unwrap_or_default();

                // Exclusive session check: collect conflict info first (immutable), then mutate.
                if let Some(ref sid) = discovered_sid {
                    let conflict = rebind_rt
                        .window_bindings
                        .iter()
                        .find(|(wid, wb)| wid.as_str() != window_id_str && wb.session_id == *sid)
                        .map(|(wid, _)| wid.clone());

                    if let Some(conflict_wid) = conflict {
                        let chat_name = rebind_rt
                            .chat_bindings
                            .iter()
                            .find(|cb| cb.session_id == *sid)
                            .map(|cb| cb.display_name.clone())
                            .unwrap_or_else(|| "unknown".to_string());
                        let _ = self
                    .im_adapter
                    .send_message(
                        &target,
                        &format!(
                            "⚠️ Session {sid} was bound to {chat_name} (window {conflict_wid}) — rebinding to this window.",
                        ),
                    )
                    .await;
                        // Clear old binding's session in-place (no extra load needed)
                        if let Some(old_wb) = rebind_rt.window_bindings.get_mut(&conflict_wid) {
                            old_wb.session_id = String::new();
                        }
                    }
                }

                // Update window binding
                if let Some(wb) = rebind_rt.window_bindings.get_mut(&window_id_str) {
                    if wb.agent_type != agent_type {
                        wb.agent_type = agent_type.clone();
                    }
                    if let Some(ref sid) = discovered_sid
                        && wb.session_id != *sid
                    {
                        changes.push(format!(
                            "session: {} → {}",
                            if stored_sid.is_empty() {
                                "none"
                            } else {
                                &stored_sid
                            },
                            sid
                        ));
                        wb.session_id = sid.clone();
                    }
                    // Re-bind the current working directory from the live pane.
                    if let Ok(pane_path) = self.tmux_mgr.pane_cwd(&window_id).await
                        && !pane_path.is_empty()
                        && wb.cwd != pane_path
                    {
                        changes.push(format!(
                            "cwd: {} → {}",
                            if wb.cwd.is_empty() { "none" } else { &wb.cwd },
                            pane_path,
                        ));
                        wb.cwd = pane_path;
                    }
                }

                // Sync display_name and window_name when chat_name differs.
                if let Some(ref cn) = target.chat_name
                    && cn != &binding.display_name
                {
                    if let Some(cb) = rebind_rt
                        .chat_bindings
                        .iter_mut()
                        .find(|cb| cb.user_id == user_id && cb.thread_id == thread_id_val)
                    {
                        changes.push(format!("display_name: {} → {}", cb.display_name, cn,));
                        cb.display_name = cn.clone();
                    }
                    if let Some(wb) = rebind_rt.window_bindings.get_mut(&window_id_str) {
                        wb.window_name = cn.clone();
                    }
                    let _ = self.tmux_mgr.rename_window(&window_id, cn).await;
                }

                // Sync the chat binding's session_id so find_cb() works.
                // Prefer the newly discovered session; fall back to the window binding's current value.
                let sync_sid = discovered_sid.clone().or_else(|| {
                    rebind_rt
                        .window_bindings
                        .get(&window_id_str)
                        .filter(|wb| !wb.session_id.is_empty())
                        .map(|wb| wb.session_id.clone())
                });
                if let Some(ref sid) = sync_sid
                    && let Some(cb) = rebind_rt
                        .chat_bindings
                        .iter_mut()
                        .find(|cb| cb.user_id == user_id && cb.thread_id == thread_id_val)
                    && cb.session_id != *sid
                {
                    cb.session_id = sid.clone();
                }

                // Single save — all mutations above are on the same rebind_rt snapshot.
                self.state_mgr.save_runtime(&rebind_rt).await?;

                // Sync session_map
                if let Some(ref sid) = discovered_sid {
                    if let Ok(mut map) = self.state_mgr.load_session_map().await
                        && map.get(&window_id_str).map(|s| s.as_str()) != Some(sid)
                    {
                        map.insert(window_id_str.clone(), sid.clone());
                        if let Err(e) = self.state_mgr.save_session_map(&map).await {
                            tracing::warn!("[rebind] Failed to save session_map: {e}");
                        }
                    }
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

        // Handle /check — health check report card
        if text.trim() == "/check" {
            if let Some((binding, wb_opt)) = find_cb() {
                let mut items: Vec<CheckItem> = Vec::new();
                let wid_str = wb_opt
                    .map(|w| &w.window_id)
                    .map(|s| s.as_str())
                    .unwrap_or("");
                let window_id = WindowId(wid_str.to_string());

                // 1. Tmux window check
                let window_alive = self.tmux_mgr.find_window(&window_id).await;
                match &window_alive {
                    Ok(info) => items.push(CheckItem {
                        label: "Tmux Window".into(),
                        status: CheckStatus::Ok,
                        detail: format!("{} — `{}`", info.name, info.current_command),
                    }),
                    Err(e) => items.push(CheckItem {
                        label: "Tmux Window".into(),
                        status: CheckStatus::Fail,
                        detail: format!("{}: {e}", wid_str),
                    }),
                }

                // 2. Chat binding
                let chat_name = target.chat_name.as_deref().unwrap_or(&binding.display_name);
                items.push(CheckItem {
                    label: "Chat Binding".into(),
                    status: CheckStatus::Info,
                    detail: format!(
                        "chat={} display={} window={}",
                        chat_name, binding.display_name, wid_str,
                    ),
                });

                // 3. Session & agent
                let (agent_type, sid) = match wb_opt {
                    Some(wb) => (wb.agent_type.as_str(), wb.session_id.as_str()),
                    None => ("?", ""),
                };

                items.push(CheckItem {
                    label: "Agent".into(),
                    status: if agent_type != "?" {
                        CheckStatus::Ok
                    } else {
                        CheckStatus::Warn
                    },
                    detail: format!("{} session={}", agent_type, sid),
                });

                // 4. Session file status
                if !sid.is_empty() {
                    let jsonl_path = resolve_jsonl(sid).await;
                    match jsonl_path {
                        Some(path) if path.exists() => {
                            let meta = match std::fs::metadata(&path) {
                                Ok(m) => m,
                                Err(_) => {
                                    items.push(CheckItem {
                                        label: "Session File".into(),
                                        status: CheckStatus::Fail,
                                        detail: "cannot read metadata".into(),
                                    });
                                    return Ok(());
                                }
                            };
                            let file_size = meta.len();
                            let mtime = meta.modified().ok();
                            let offset = { self.byte_offsets.lock().await.get(sid).copied() };

                            let offset_str = offset
                                .map(|o| format!("{:.1}KB", o as f64 / 1024.0))
                                .unwrap_or_else(|| "none".into());
                            let behind = offset
                                .map(|o| {
                                    if file_size > o {
                                        format!(" ({}KB behind)", (file_size - o) as f64 / 1024.0)
                                    } else {
                                        String::new()
                                    }
                                })
                                .unwrap_or_default();

                            let time_str = mtime
                                .and_then(|t| {
                                    std::time::SystemTime::now()
                                        .duration_since(t)
                                        .ok()
                                        .map(|d| {
                                            let mins = d.as_secs() / 60;
                                            if mins < 1 {
                                                "just now".into()
                                            } else {
                                                format!("{mins}min ago")
                                            }
                                        })
                                })
                                .unwrap_or_else(|| "unknown".into());

                            items.push(CheckItem {
                                label: "Session File".into(),
                                status: CheckStatus::Ok,
                                detail: format!(
                                    "{} ({:.1}KB) offset={}{} last_activity={}",
                                    path.file_name()
                                        .map(|n| n.to_string_lossy())
                                        .unwrap_or_default(),
                                    file_size as f64 / 1024.0,
                                    offset_str,
                                    behind,
                                    time_str,
                                ),
                            });
                        }
                        _ => {
                            items.push(CheckItem {
                                label: "Session File".into(),
                                status: CheckStatus::Warn,
                                detail: "not found".into(),
                            });
                        }
                    }
                } else {
                    items.push(CheckItem {
                        label: "Session".into(),
                        status: CheckStatus::Warn,
                        detail: "no session bound".into(),
                    });
                }

                let _ = self
                    .im_adapter
                    .send_check_card(&target, "🔍 系统巡检", &items)
                    .await;
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
                if let Some((_binding, wb)) = find_cb() {
                    let wid_str = wb.map(|w| &w.window_id).map(|s| s.as_str()).unwrap_or("");
                    if wid_str.is_empty() {
                        let _ = self
                            .im_adapter
                            .send_message(&target, "No active session.")
                            .await;
                        return Ok(());
                    }
                    let window_id = atim_core::message::WindowId(wid_str.to_string());
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
                    let wid = wid_str.to_string();
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
                        .send_browser_keyboard(&target, user_id, thread_id, None)
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
                            .send_browser_keyboard(&target, user_id, thread_id, None)
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
        if let Some((binding, wb_opt)) = find_cb() {
            let mut window_id_str = wb_opt
                .map(|w| &w.window_id)
                .map(|s| s.as_str())
                .unwrap_or("")
                .to_string();

            // Never trust a stored window_id blindly — always resolve by
            // binding display_name (tmux window name).  This handles:
            // - tmux renumbering on restart (stale @id)
            // - window repurposed (exists but has a different name)
            // - no window_binding found (window_id_str is empty)
            // Clear any stale browser state — user is chatting with a bound agent.
            self.browser.end_session(user_id).await;
            if let Some(real_wid) = self.find_window_by_name(&binding.display_name).await {
                tracing::info!(
                    "[handle_text_message] Resolved binding '{}' to live window {} (was {})",
                    binding.display_name,
                    real_wid,
                    if window_id_str.is_empty() {
                        "(none)"
                    } else {
                        &window_id_str
                    },
                );
                // Update V2 runtime so future lookups work without retrying name search
                if !binding.session_id.is_empty() {
                    let mut rt = self.state_mgr.load_runtime().await?;
                    // Remove stale window_binding if we had one
                    if !window_id_str.is_empty() {
                        rt.window_bindings.remove(&window_id_str);
                    }
                    // Insert or update window_binding for the live window.
                    // When there's no existing WindowBinding (e.g. after restart),
                    // resolve agent_type from the session info map.
                    let agent_type = wb_opt
                        .map(|w| w.agent_type.clone())
                        .or_else(|| {
                            rt.sessions
                                .get(&binding.session_id)
                                .map(|s| s.agent_type.clone())
                        })
                        .unwrap_or_default();
                    rt.window_bindings.insert(
                        real_wid.clone(),
                        WindowBinding {
                            window_id: real_wid.clone(),
                            session_id: binding.session_id.clone(),
                            cwd: wb_opt.map(|w| w.cwd.clone()).unwrap_or_default(),
                            agent_type,
                            window_name: binding.display_name.clone(),
                        },
                    );
                    self.state_mgr.save_runtime(&rt).await?;
                }
                window_id_str = real_wid;
            }

            let agent_type_str = wb_opt
                .map(|wb| {
                    format!(
                        "agent_type={} session_id={}",
                        wb.agent_type,
                        if wb.session_id.is_empty() {
                            "none"
                        } else {
                            &wb.session_id
                        }
                    )
                })
                .unwrap_or_else(|| "no_window_state".into());
            tracing::debug!(
                "[handle_text_message] user={user_id} group_chat_id={} chat_name={:?} window={} display_name={} {} text={text:?}",
                binding.group_chat_id.unwrap_or(binding.chat_id),
                target.chat_name,
                window_id_str,
                binding.display_name,
                agent_type_str,
            );
            // Forward to existing window
            let window_id = atim_core::message::WindowId(window_id_str.to_string());

            // Verify window is alive and agent is actually running
            match self.tmux_mgr.find_window(&window_id).await {
                Err(e) => {
                    // Window died — prompt user to recover or create new session
                    tracing::warn!(
                        "[handle_text_message] Window {} died ({e}), prompting user {}",
                        window_id_str,
                        user_id
                    );
                    let tid = binding.thread_id;
                    let key = (user_id, tid);
                    {
                        let mut pending = self.pending_messages.lock().await;
                        pending.insert(key, text.to_string());
                    }
                    let mut ctx_lock = self.callback_contexts.lock().await;
                    let recover_token = Self::make_callback_token(&mut ctx_lock, user_id, tid);
                    let cancel_token = Self::make_callback_token(&mut ctx_lock, user_id, tid);
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
                        window_id_str,
                        binding.display_name,
                        info.name,
                        user_id
                    );
                    let tid = binding.thread_id;
                    let key = (user_id, tid);
                    {
                        let mut pending = self.pending_messages.lock().await;
                        pending.insert(key, text.to_string());
                    }
                    let mut ctx_lock = self.callback_contexts.lock().await;
                    let recover_token = Self::make_callback_token(&mut ctx_lock, user_id, tid);
                    let cancel_token = Self::make_callback_token(&mut ctx_lock, user_id, tid);
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
                        window_id_str,
                        info.current_command,
                    );
                    // Re-launch the agent if it exited to shell, then send the text.
                    let agent_type_name =
                        wb_opt.map(|wb| wb.agent_type.as_str()).unwrap_or("claude");
                    let agent = self
                        .config
                        .agent_registry
                        .get(agent_type_name)
                        .cloned()
                        .unwrap_or_else(|| self.config.agent_registry.default().clone());
                    tracing::info!(
                        "[handle_text_message] re-launching {} for window {}",
                        agent.name(),
                        window_id_str,
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
                            window_id_str,
                        );
                    }
                    // Extra delay for TUI agents (Copilot/Codex) so bubbletea can set up
                    if !agent.supports_sessions() {
                        tokio::time::sleep(Duration::from_millis(1500)).await;
                    }
                    // For session-based agents (Claude Code), wait for the SessionStart hook
                    // to register the new session_id, then sync via V2 runtime.
                    if agent.supports_sessions() {
                        for _ in 0..10 {
                            tokio::time::sleep(Duration::from_millis(500)).await;
                            if let Ok(map) = self.state_mgr.load_session_map().await
                                && map.get(&window_id_str).map(|s| s.as_str())
                                    != wb_opt.map(|wb| wb.session_id.as_str())
                            {
                                if let Some(sid) = map.get(&window_id_str) {
                                    let mut rt = self.state_mgr.load_runtime().await?;
                                    if let Some(wb) = rt.window_bindings.get_mut(&window_id_str) {
                                        wb.session_id = sid.clone();
                                        tracing::info!(
                                            "[handle_text_message] updated session_id for window {} after re-launch",
                                            window_id_str,
                                        );
                                    }
                                    self.state_mgr.save_runtime(&rt).await?;
                                }
                                break;
                            }
                        }
                    }
                    let is_copilot = wb_opt.map(|wb| wb.agent_type == "copilot").unwrap_or(false);
                    self.send_text_to_agent(&window_id, text, is_copilot)
                        .await?;
                    if let Some(ref mid) = message_id {
                        let _ = self.im_adapter.add_reaction(&target, mid, "DONE").await;
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
                        window_id_str,
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
                        let key = (user_id, thread_id);
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
                        let rename_token =
                            Self::make_callback_token(&mut ctx_lock, user_id, thread_id);
                        let cancel_token =
                            Self::make_callback_token(&mut ctx_lock, user_id, thread_id);
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
                    let stored_agent_type = wb_opt.map(|wb| wb.agent_type.as_str());
                    if let Some(running_agent) = self.detect_running_agent(&info.current_command)
                        && let Some(stored_type) = stored_agent_type
                        && running_agent != stored_type
                    {
                        if stored_type.is_empty() {
                            // Empty agent_type means the window_binding was created
                            // by a previous bug or missing session data. Fix it silently
                            // instead of prompting the user.
                            tracing::info!(
                                "[handle_text_message] Fixing empty agent_type → '{running_agent}' for window {}",
                                window_id_str,
                            );
                            if let Ok(mut rt) = self.state_mgr.load_runtime().await
                                && let Some(wb) = rt.window_bindings.get_mut(&window_id_str)
                            {
                                wb.agent_type = running_agent.to_string();
                                if let Err(e) = self.state_mgr.save_runtime(&rt).await {
                                    tracing::warn!(
                                        "[handle_text_message] Failed to save runtime after agent_type fix: {e}"
                                    );
                                }
                            }
                        } else {
                            tracing::info!(
                                "[handle_text_message] Agent type mismatch: stored='{stored_type}' running='{running_agent}', prompting user {}",
                                user_id,
                            );
                            let key = (user_id, thread_id);
                            {
                                let mut pending = self.pending_messages.lock().await;
                                pending.insert(key, text.to_string());
                            }
                            let mut ctx_lock = self.callback_contexts.lock().await;
                            let rebind_token =
                                Self::make_callback_token(&mut ctx_lock, user_id, thread_id);
                            let cancel_token =
                                Self::make_callback_token(&mut ctx_lock, user_id, thread_id);
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
                                        callback_data: format!(
                                            "cb:{cancel_token}:lifecycle_cancel"
                                        ),
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
                    }

                    // Try to resolve empty session_id (e.g. copilot sessions)
                    if wb_opt.map(|wb| wb.agent_type.as_str()) == Some("copilot")
                        && wb_opt.map(|wb| wb.session_id.is_empty()).unwrap_or(false)
                        && let Some(agent) = self.config.agent_registry.get("copilot")
                        && let Ok(Some(sid)) = agent.discover_session_by_pid(&window_id.0)
                    {
                        let mut rt = self.state_mgr.load_runtime().await?;
                        if let Some(wb) = rt.window_bindings.get_mut(&window_id_str) {
                            wb.session_id = sid.clone();
                            tracing::info!(
                                "[handle_text_message] Resolved copilot session_id={sid} for window {}",
                                window_id_str,
                            );
                        }
                        self.state_mgr.save_runtime(&rt).await?;
                    }

                    let is_copilot = wb_opt.map(|wb| wb.agent_type == "copilot").unwrap_or(false);
                    let result = self.send_text_to_agent(&window_id, text, is_copilot).await;
                    match &result {
                        Ok(()) => {
                            tracing::info!(
                                "[handle_text_message] window={} send_line OK",
                                window_id_str,
                            );
                            if let Some(ref mid) = message_id {
                                let _ = self.im_adapter.add_reaction(&target, mid, "DONE").await;
                            }
                        }
                        Err(e) => {
                            tracing::error!(
                                "[handle_text_message] window={} send_line failed: {e}",
                                window_id_str,
                            );
                            let _ = self
                                .im_adapter
                                .send_message(&target, &format!("❌ 发送失败: {e}"))
                                .await;
                        }
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
            tracing::debug!(
                "[Feishu] No binding for user {user_id}, showing picker (is_mention={is_mention}, is_group={is_group})"
            );

            // Send welcome message on first interaction in this chat
            {
                let mut sent = self.welcome_sent.lock().await;
                if sent.insert(target.chat_id.0) {
                    let welcome = concat!(
                        "👋 Thanks for adding me!\n\n",
                        "I'm **atim** — a bridge between IM and Claude Code agents.\n",
                        "Send `/atim help` to see available commands."
                    );
                    let _ = self.im_adapter.send_message(&target, welcome).await;
                }
            }

            // No binding — save pending text + chat name, then show agent picker
            let thread_id = target.thread_id.map(|t| t.0).unwrap_or(0);
            let key = (user_id, thread_id);
            {
                let mut pending = self.pending_messages.lock().await;
                pending.insert(key, text.to_string());
            }
            // Preserve the chat/group name so the window gets a meaningful
            // name (e.g. "Copilot Chat") instead of the generic "atim-{user_id}".
            if let Some(ref cn) = target.chat_name {
                self.pending_chat_names.lock().await.insert(key, cn.clone());
            }

            // Check if a default agent is configured for this chat
            // Priority: per-chat setting > config.toml [agent] default_agent
            let default_agent = self
                .state_mgr
                .load_chat_setting(user_id, thread_id, "default_agent")
                .await
                .unwrap_or(None)
                .filter(|s| !s.is_empty())
                .or_else(|| {
                    let cfg = self.config.default_agent.clone();
                    if cfg.is_empty() { None } else { Some(cfg) }
                });
            if let Some(agent_name) = default_agent
                && !agent_name.is_empty()
                && self.config.agent_registry.get(&agent_name).is_some()
            {
                tracing::info!(
                    "[handle_text_message] Using default agent '{agent_name}' for user {user_id}"
                );
                self.pending_agents.lock().await.insert(key, agent_name);
                self.show_setup_flow(&target, user_id, thread_id).await?;
                return Ok(());
            }

            self.send_agent_picker(&target, user_id, thread_id).await?;
        }

        Ok(())
    }

    /// Send a welcome message when the bot is added to a group chat.
    async fn handle_bot_added(&self, target: &MessageTarget) -> Result<()> {
        let mut sent = self.welcome_sent.lock().await;
        if sent.insert(target.chat_id.0) {
            let welcome = concat!(
                "👋 Thanks for adding me!\n\n",
                "I'm **atim** — a bridge between IM and Claude Code agents.\n",
                "Send `/atim help` to see available commands."
            );
            let _ = self.im_adapter.send_message(target, welcome).await;
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
        let _ = self
            .send_browser_keyboard(target, user_id, thread_id, None)
            .await;
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
            let token = Self::make_callback_token(&mut ctx_lock, user_id, thread_id);
            buttons.push(vec![Button {
                text: format!("🚀 {}", agent.name()),
                callback_data: format!("cb:{token}:agent:{}", agent.name()),
            }]);
        }

        let cancel_token = Self::make_callback_token(&mut ctx_lock, user_id, thread_id);
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
        let rt = self.state_mgr.load_runtime().await?;
        let unbound = self.unbound_windows(&rt).await;
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
    async fn unbound_windows(&self, rt: &RuntimeState) -> Vec<browser::WindowEntry> {
        let bound_ids: HashSet<String> = rt
            .window_bindings
            .values()
            .map(|b| b.window_id.clone())
            .collect();

        match self.tmux_mgr.list_windows().await {
            Ok(windows) => windows
                .into_iter()
                .filter(|w| !bound_ids.contains(&w.window_id.0))
                .map(|w| {
                    let agent_type = rt
                        .window_bindings
                        .get(&w.window_id.0)
                        .map(|wb| wb.agent_type.as_str())
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
        let _ = self
            .send_browser_keyboard(target, user_id, thread_id, None)
            .await;
        Ok(())
    }

    /// Send an AskUserQuestion as a Feishu interactive card with clickable option buttons.
    ///
    /// Parses the tool_use input JSON to extract questions/options, builds
    /// a card with one button per option, and returns the message_id for
    /// tracking (so the ToolResult can edit it later).
    async fn send_ask_user_card(
        &self,
        target: &MessageTarget,
        raw_input: &str,
        fallback_text: &str,
    ) -> Result<MessageId> {
        let parsed: serde_json::Value = serde_json::from_str(raw_input)
            .map_err(|e| atim_core::error::Error::Config(format!("AskUserQuestion JSON: {e}")))?;

        let questions = parsed["questions"].as_array();
        let mut all_buttons: Vec<Vec<Button>> = Vec::new();
        let mut card_text = String::new();

        if let Some(qs) = questions {
            for (qi, q) in qs.iter().enumerate() {
                let question = q["question"].as_str().unwrap_or("Choose:");
                let header = q["header"].as_str().unwrap_or("");
                if !header.is_empty() {
                    card_text.push_str(&format!("**{}**: {}\n", header, question));
                } else if qs.len() > 1 {
                    card_text.push_str(&format!("**Q{}**: {}\n", qi + 1, question));
                } else {
                    card_text.push_str(&format!("**{}**\n", question));
                }

                if let Some(options) = q["options"].as_array() {
                    for (i, opt) in options.iter().enumerate() {
                        let label = opt["label"].as_str().unwrap_or("");
                        let desc = opt["description"].as_str().unwrap_or("");
                        let btn_text = if !desc.is_empty() && desc != label {
                            format!("{}. {} — {}", i + 1, label, desc)
                        } else {
                            format!("{}. {}", i + 1, label)
                        };
                        let btn_label = if btn_text.len() > 45 {
                            format!(
                                "{}…",
                                &btn_text[..btn_text
                                    .char_indices()
                                    .nth(42)
                                    .map(|(j, _)| j)
                                    .unwrap_or(btn_text.len())]
                            )
                        } else {
                            btn_text
                        };
                        all_buttons.push(vec![Button {
                            text: btn_label,
                            callback_data: format!("ui:select:{i}"),
                        }]);
                    }
                }
            }
        }

        if card_text.is_empty() {
            card_text = fallback_text.to_string();
        }

        // Always add a cancel button
        all_buttons.push(vec![Button {
            text: "✖ Cancel".into(),
            callback_data: "ui:esc".into(),
        }]);

        self.im_adapter
            .send_keyboard(target, &card_text, &all_buttons)
            .await
    }

    /// Build and send the current browser keyboard to the user.
    async fn send_browser_keyboard(
        &self,
        target: &MessageTarget,
        user_id: i64,
        thread_id: i64,
        msg_id: Option<MessageId>,
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
                    let token = Self::make_callback_token(&mut ctx_lock, user_id, thread_id);
                    buttons.push(vec![Button {
                        text: display,
                        callback_data: format!("cb:{token}:browse:dir:{i}"),
                    }]);
                }

                // Navigation row
                let mut nav_row = Vec::new();
                let cancel_token = Self::make_callback_token(&mut ctx_lock, user_id, thread_id);
                nav_row.push(Button {
                    text: "❌ Cancel".into(),
                    callback_data: format!("cb:{cancel_token}:browse:cancel"),
                });

                if listing.total_pages > 1 {
                    let page_token = Self::make_callback_token(&mut ctx_lock, user_id, thread_id);
                    nav_row.push(Button {
                        text: format!("◀ {}/{} ▶", listing.page + 1, listing.total_pages),
                        callback_data: format!("cb:{page_token}:browse:page"),
                    });
                }
                if listing.has_parent {
                    let up_token = Self::make_callback_token(&mut ctx_lock, user_id, thread_id);
                    nav_row.push(Button {
                        text: "⬆ Up".into(),
                        callback_data: format!("cb:{up_token}:browse:up"),
                    });
                }
                let sel_token = Self::make_callback_token(&mut ctx_lock, user_id, thread_id);
                nav_row.push(Button {
                    text: "✅ Select".into(),
                    callback_data: format!("cb:{sel_token}:browse:confirm"),
                });
                buttons.push(nav_row);

                // "Switch Agent" button to go back to agent picker
                let agent_token = Self::make_callback_token(&mut ctx_lock, user_id, thread_id);
                buttons.push(vec![Button {
                    text: "🤖 Switch Agent".into(),
                    callback_data: format!("cb:{agent_token}:browse:switch_agent"),
                }]);

                let text = format!(
                    "📁 Select a project directory:\n{}",
                    listing.current_path.display()
                );
                (text, buttons)
            }
            BrowserMode::SessionPick { sessions: _ } => {
                let page = browser::get_session_picker_page(&state)
                    .expect("state.mode is SessionPick (checked by match arm)");
                let mut buttons: Vec<Vec<Button>> = Vec::new();

                for (i, session) in page.sessions.iter().enumerate() {
                    let relative = relative_time(&session.timestamp);
                    let summary = if session.summary.len() > 45 {
                        let end = session
                            .summary
                            .char_indices()
                            .nth(42)
                            .map(|(i, _)| i)
                            .unwrap_or(session.summary.len());
                        format!("{}…", &session.summary[..end])
                    } else if session.summary.is_empty() {
                        "(empty)".into()
                    } else {
                        session.summary.clone()
                    };
                    let token = Self::make_callback_token(&mut ctx_lock, user_id, thread_id);
                    buttons.push(vec![Button {
                        text: format!("🔄 {} | {}", relative, summary),
                        callback_data: format!("cb:{token}:browse:sel:{i}"),
                    }]);
                }

                // Navigation row
                let mut nav_row = Vec::new();
                let cancel_token = Self::make_callback_token(&mut ctx_lock, user_id, thread_id);
                nav_row.push(Button {
                    text: "❌ Cancel".into(),
                    callback_data: format!("cb:{cancel_token}:browse:cancel"),
                });

                let back_token = Self::make_callback_token(&mut ctx_lock, user_id, thread_id);
                nav_row.push(Button {
                    text: "📁 Browse".into(),
                    callback_data: format!("cb:{back_token}:browse:back"),
                });

                if page.total_pages > 1 {
                    let page_token = Self::make_callback_token(&mut ctx_lock, user_id, thread_id);
                    nav_row.push(Button {
                        text: format!("◀ {}/{} ▶", page.page + 1, page.total_pages),
                        callback_data: format!("cb:{page_token}:browse:page"),
                    });
                }
                let new_token = Self::make_callback_token(&mut ctx_lock, user_id, thread_id);
                nav_row.push(Button {
                    text: "🆕 New".into(),
                    callback_data: format!("cb:{new_token}:browse:new"),
                });
                buttons.push(nav_row);

                let text = format!("Session Picker\n{}", state.current_path.display());
                (text, buttons)
            }
            BrowserMode::WindowPick { windows: _ } => {
                let page = browser::get_window_picker_page(&state)
                    .expect("state.mode is WindowPick (checked by match arm)");
                let mut buttons: Vec<Vec<Button>> = Vec::new();

                for (i, win) in page.windows.iter().enumerate() {
                    let agent = if win.agent_type.is_empty() {
                        &win.current_command
                    } else {
                        &win.agent_type
                    };
                    let label = format!("💬 {} [{}]", win.name, agent);
                    let token = Self::make_callback_token(&mut ctx_lock, user_id, thread_id);
                    buttons.push(vec![Button {
                        text: label,
                        callback_data: format!("cb:{token}:browse:win:{i}"),
                    }]);
                }

                // Navigation row
                let mut nav_row = Vec::new();
                let cancel_token = Self::make_callback_token(&mut ctx_lock, user_id, thread_id);
                nav_row.push(Button {
                    text: "❌ Cancel".into(),
                    callback_data: format!("cb:{cancel_token}:browse:cancel"),
                });

                let new_token = Self::make_callback_token(&mut ctx_lock, user_id, thread_id);
                nav_row.push(Button {
                    text: "🆕 New Session".into(),
                    callback_data: format!("cb:{new_token}:browse:new_win"),
                });

                if page.total_pages > 1 {
                    let page_token = Self::make_callback_token(&mut ctx_lock, user_id, thread_id);
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

        if let Some(edit_id) = msg_id {
            // Edit existing card in-place
            let _ = self
                .im_adapter
                .edit_keyboard(target, &edit_id, &buttons)
                .await;
        } else {
            let _ = self.im_adapter.send_keyboard(target, &text, &buttons).await;
        }
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
                let _ = self
                    .send_browser_keyboard(target, user_id, thread_id, Some(msg_id.clone()))
                    .await;
            }
            "page" => {
                // Toggle to next page
                let new_page = state.page + 1;
                self.browser.set_page(user_id, new_page).await;
                let _ = self
                    .send_browser_keyboard(target, user_id, thread_id, Some(msg_id.clone()))
                    .await;
            }
            "confirm" => {
                // Update the cwd picker card in-place to show the selected dir
                let _ = self
                    .im_adapter
                    .edit_message(
                        target,
                        msg_id,
                        &format!("📁 Selected: {}", state.current_path.display()),
                    )
                    .await;

                // Scan the current directory for sessions and show picker
                let sessions = browser::scan_claude_sessions(&state.current_path);
                if sessions.is_empty() {
                    // No existing sessions — create new directly
                    let key = (target.chat_id.0, thread_id);
                    let mut topic_name = self.topic_names.lock().await.remove(&key);
                    if topic_name.is_none() {
                        topic_name = self
                            .pending_chat_names
                            .lock()
                            .await
                            .remove(&(user_id, thread_id));
                    }
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
                    let _ = self
                        .send_browser_keyboard(target, user_id, thread_id, None)
                        .await;
                }
            }
            "back" => {
                self.browser.show_browsing(user_id).await;
                let _ = self
                    .send_browser_keyboard(target, user_id, thread_id, Some(msg_id.clone()))
                    .await;
            }
            "new" => {
                // Create a new session in the selected directory
                let key = (target.chat_id.0, thread_id);
                let mut topic_name = self.topic_names.lock().await.remove(&key);
                if topic_name.is_none() {
                    topic_name = self
                        .pending_chat_names
                        .lock()
                        .await
                        .remove(&(user_id, thread_id));
                }
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
                    // Patch old card to text-only, send new card for fresh listing
                    let _ = self
                        .im_adapter
                        .edit_message(target, msg_id, &format!("📁 {}", entry.path.display()))
                        .await;
                    let _ = self
                        .send_browser_keyboard(target, user_id, thread_id, None)
                        .await;
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
                    let key = (target.chat_id.0, thread_id);
                    let mut topic_name = self.topic_names.lock().await.remove(&key);
                    if topic_name.is_none() {
                        topic_name = self
                            .pending_chat_names
                            .lock()
                            .await
                            .remove(&(user_id, thread_id));
                    }
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
                let _ = self
                    .send_browser_keyboard(target, user_id, thread_id, Some(msg_id.clone()))
                    .await;
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
                    let key = (target.chat_id.0, thread_id);
                    let mut topic_name = self.topic_names.lock().await.remove(&key);
                    if topic_name.is_none() {
                        topic_name = self
                            .pending_chat_names
                            .lock()
                            .await
                            .remove(&(user_id, thread_id));
                    }
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
            "switch_agent" => {
                // Clear browser session and go back to agent picker
                self.browser.end_session(user_id).await;
                let _ = self
                    .im_adapter
                    .edit_message(target, msg_id, "Switching agent...")
                    .await;
                self.send_agent_picker(target, user_id, thread_id).await?;
            }
            _ => {
                tracing::warn!("Unknown browser action: {action}");
            }
        }
        Ok(())
    }

    /// If the pane shows a "trust this folder" confirmation dialog (first
    /// run of Claude Code in a new directory), auto-confirm it by sending
    /// Enter.  Waits for the dialog to disappear before returning.
    /// Returns `true` if a trust dialog was detected and (attempted) dismissed.
    async fn auto_confirm_trust_dialog(&self, window_id: &WindowId) -> bool {
        let pane = self
            .tmux_mgr
            .capture_pane(window_id)
            .await
            .unwrap_or_default();
        let clean = strip_ansi(&pane);
        let lower = clean.to_lowercase();
        if !lower.contains("trust") && !lower.contains("safety") {
            return false;
        }
        tracing::info!(
            "Detected trust folder dialog in window {}, auto-confirming",
            window_id.0
        );
        // Send Enter to confirm "Yes, I trust this folder"
        self.tmux_mgr.send_key(window_id, "Enter").await.ok();
        // Wait for the dialog to disappear
        tokio::time::sleep(Duration::from_millis(1500)).await;
        // Verify the dialog is gone; retry once if still present
        let pane = self
            .tmux_mgr
            .capture_pane(window_id)
            .await
            .unwrap_or_default();
        let clean = strip_ansi(&pane).to_lowercase();
        if clean.contains("trust") || clean.contains("safety") {
            tracing::warn!("Trust dialog still present after Enter, retrying");
            self.tmux_mgr.send_key(window_id, "Enter").await.ok();
            tokio::time::sleep(Duration::from_millis(1500)).await;
        }
        let _ = self
            .tmux_mgr
            .wait_for_agent_ready(window_id, Duration::from_secs(4))
            .await;
        true
    }

    /// Actively discover the session_id by sending `/status` to the agent
    /// in the given window and parsing the Session ID from the response.
    ///
    /// This is more reliable than polling session_map.json (SessionStart hook),
    /// because it queries the running agent directly on demand.
    async fn discover_session_via_status(&self, window_id: &str) -> Option<String> {
        let wid = WindowId(window_id.to_string());

        // Capture baseline pane content before sending /status.
        let baseline_raw = self
            .tmux_mgr
            .capture_pane(&wid)
            .await
            .ok()
            .unwrap_or_default();
        let baseline = strip_ansi(&baseline_raw);
        let baseline_len = baseline.lines().count();

        // Send /status to the agent
        self.tmux_mgr.send_line(&wid, "/status").await.ok();
        tokio::time::sleep(Duration::from_millis(2000)).await;

        // Capture updated pane content and extract Session ID from new lines.
        let captured_raw = self
            .tmux_mgr
            .capture_pane(&wid)
            .await
            .ok()
            .unwrap_or_default();
        let captured = strip_ansi(&captured_raw);
        let lines: Vec<&str> = captured.lines().collect();
        let new_text = lines
            .iter()
            .copied()
            .skip(baseline_len)
            .collect::<Vec<_>>()
            .join("\n");

        let mut sid = SESSION_ID_RE
            .captures(&new_text)
            .and_then(|c| c.get(1))
            .map(|m| m.as_str().to_string());

        if sid.is_none() {
            // Full pane fallback — /status modal may replace rather than append
            sid = SESSION_ID_RE
                .captures(&captured)
                .and_then(|c| c.get(1))
                .map(|m| m.as_str().to_string());
        }

        // Dismiss /status modal — send Escape, then verify it closed.
        // Retry once if the modal is still visible (but don't send more
        // than 2 total — extra Escapes trigger Claude Code's "rewind" prompt).
        self.tmux_mgr.send_key(&wid, "Escape").await.ok();
        tokio::time::sleep(Duration::from_millis(500)).await;
        {
            let pane = self.tmux_mgr.capture_pane(&wid).await.unwrap_or_default();
            let lower = strip_ansi(&pane).to_lowercase();
            if lower.contains("session id") || lower.contains("/status") {
                self.tmux_mgr.send_key(&wid, "Escape").await.ok();
                tokio::time::sleep(Duration::from_millis(500)).await;
            }
        }

        if sid.is_some() {
            tracing::info!("Found session {:?} for window {window_id} via /status", sid);
        } else {
            tracing::debug!("/status session discovery returned no match for window {window_id}");
        }

        sid
    }

    /// After starting an agent in a new window, wait for the session_id to be
    /// registered by actively polling all available discovery methods until
    /// timeout — /status for Claude, PID/lsof for Copilot/Codex, and
    /// session_map.json for the SessionStart hook.
    ///
    /// Unlike the old approach (fixed sleep then one-shot discovery), this
    /// continuously retries so the caller can send the user's message first
    /// and let the agent initialize on its own schedule.
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
            .load_runtime()
            .await
            .ok()
            .and_then(|rt| rt.window_bindings.get(window_id).cloned())
            .and_then(|wb| {
                if wb.agent_type == "claude" {
                    Some(self.config.agent_registry.default().clone())
                } else {
                    self.config.agent_registry.get(&wb.agent_type).cloned()
                }
            })
            .unwrap_or_else(|| self.config.agent_registry.default().clone());

        // Only Claude Code supports sessions — skip for others.
        if !agent.supports_sessions() {
            return None;
        }

        let deadline = std::time::Instant::now() + timeout;
        let mut status_retries = 0u32;

        // Phase 1: Active retry loop — tries all discovery methods until
        // we find the session_id or run out of time.
        loop {
            let remaining = deadline.saturating_duration_since(std::time::Instant::now());
            if remaining.is_zero() {
                break;
            }

            // a) PID-based discovery (non-intrusive, works for Copilot/Codex)
            if let Ok(Some(sid)) = agent.discover_session_by_pid(window_id) {
                tracing::info!("Found session {sid} for window {window_id} via PID discovery");
                return Some(sid);
            }

            // b) session_map.json (SessionStart hook, Claude only)
            if agent.has_session_start_hook()
                && let Ok(map) = self.state_mgr.load_session_map().await
                && let Some(sid) = map.get(window_id)
                && !sid.is_empty()
            {
                tracing::info!("Found session {sid} for window {window_id} via session_map");
                return Some(sid.clone());
            }

            // c) /status command (Claude only — sends command, needs 2s
            //    wait for response). Do this every other iteration to
            //    avoid spamming the agent.
            status_retries += 1;
            if status_retries.is_multiple_of(2) {
                if let Some(sid) = self.discover_session_via_status(window_id).await {
                    return Some(sid);
                }
                continue; // discover_session_via_status already waited ~2s
            }

            tokio::time::sleep(Duration::from_millis(1000)).await;
        }

        // Phase 2: Fallback — working-directory based discovery (one-shot)
        if let Some(cwd) = cwd_hint {
            tracing::warn!(
                "Active discovery timed out for window {window_id}, trying path-based discovery with cwd={cwd}"
            );
            let state = self.state_mgr.load_runtime().await.ok()?;
            let mut known_ids: std::collections::HashSet<String> = state
                .window_bindings
                .values()
                .map(|wb| wb.session_id.clone())
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
    async fn resolve_agent(&self, user_id: i64, thread_id: i64) -> AgentHandle {
        let key = (user_id, thread_id);
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
        let thread_id = target.thread_id.map(|t| t.0).unwrap_or(0);
        let name = format!("atim-{user_id}");
        let window_name = match topic_name {
            Some(tn) => tn.to_string(),
            None => self
                .pending_chat_names
                .lock()
                .await
                .remove(&(user_id, thread_id))
                .unwrap_or(name),
        };
        let window_id = self
            .tmux_mgr
            .new_window(&window_name, &cwd.to_string_lossy())
            .await?;

        let agent = self
            .resolve_agent(user_id, target.thread_id.map(|t| t.0).unwrap_or(0))
            .await;
        let launch_cmd = agent_launch_cmd(&agent);
        self.tmux_mgr.send_line(&window_id, &launch_cmd).await?;

        // Wait for agent process to actually start (non-shell process appears)
        let mut agent_started = false;
        for _ in 0..10 {
            if let Ok(info) = self.tmux_mgr.find_window(&window_id).await
                && !is_shell_process(&info.current_command)
            {
                agent_started = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(500)).await;
        }

        if !agent_started {
            let pane = self
                .tmux_mgr
                .capture_pane(&window_id)
                .await
                .unwrap_or_default();
            let clean = atim_parser::terminal::TerminalParser::strip_ansi(&pane);
            let err_msg = if clean.trim().is_empty() {
                "❌ Agent failed to start. Check agent command and configuration.".into()
            } else {
                format!("❌ Agent failed to start:\n```\n{}```", clean.trim())
            };
            let _ = self.im_adapter.send_message(target, &err_msg).await;
            return Ok(());
        }

        // Wait for the agent's TUI to finish rendering before forwarding
        // the first message, otherwise the text lands in the input box but
        // Enter gets consumed by an animation/transition rather than submitting.
        let _ = self
            .tmux_mgr
            .wait_for_agent_ready(&window_id, Duration::from_secs(4))
            .await;

        // Auto-confirm trust folder dialog if present (first run in a new dir)
        if self.auto_confirm_trust_dialog(&window_id).await {
            // Verify the agent is still running after trust confirmation.
            // If the agent exited (back to shell), report failure.
            if let Ok(info) = self.tmux_mgr.find_window(&window_id).await
                && is_shell_process(&info.current_command)
            {
                let pane = self
                    .tmux_mgr
                    .capture_pane(&window_id)
                    .await
                    .unwrap_or_default();
                let clean = atim_parser::terminal::TerminalParser::strip_ansi(&pane);
                let err_msg = format!(
                    "❌ Agent exited after trust dialog:\n```\n{}```",
                    clean.trim()
                );
                let _ = self.im_adapter.send_message(target, &err_msg).await;
                return Ok(());
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

        let thread_id = target.thread_id.map(|t| t.0).unwrap_or(0);
        let cwd_str = cwd.to_string_lossy().to_string();
        let wid = window_id.0.clone();

        // Persist via V2 API
        self.state_mgr
            .upsert_session(&SessionInfo {
                session_id: String::new(),
                cwd: cwd_str.clone(),
                agent_type: agent.name().to_string(),
            })
            .await?;
        self.state_mgr
            .upsert_window_binding(&WindowBinding {
                window_id: wid.clone(),
                session_id: String::new(),
                cwd: cwd_str.clone(),
                agent_type: agent.name().to_string(),
                window_name: window_name.to_string(),
            })
            .await?;
        self.state_mgr
            .upsert_chat_binding(&ChatBinding {
                user_id,
                thread_id,
                chat_id: target.chat_id.0,
                display_name: window_name.to_string(),
                group_chat_id: None,
                topic_name: topic_name.map(String::from),
                session_id: String::new(),
                reply_at_only: false,
            })
            .await?;

        // Resolve session_id FIRST via /status so the monitor can track
        // responses. Sending /status before the user's message avoids the
        // case where the message lands in the input box while the /status
        // modal is still open (which would consume Enter as "dismiss" rather
        // than "submit").
        if let Some(sid) = self
            .resolve_session_id(
                &wid,
                Duration::from_secs(15),
                Some(cwd.to_str().unwrap_or_default()),
            )
            .await
        {
            self.state_mgr
                .upsert_window_binding(&WindowBinding {
                    window_id: wid.clone(),
                    session_id: sid.clone(),
                    cwd: cwd_str.clone(),
                    agent_type: agent.name().to_string(),
                    window_name: window_name.to_string(),
                })
                .await?;
            // Also update the chat binding's session_id so find_cb() works
            self.state_mgr
                .upsert_chat_binding(&ChatBinding {
                    user_id,
                    thread_id,
                    chat_id: target.chat_id.0,
                    display_name: window_name.to_string(),
                    group_chat_id: None,
                    topic_name: topic_name.map(String::from),
                    session_id: sid,
                    reply_at_only: false,
                })
                .await?;
        }

        // Forward the user's pending message to the agent.
        // Some agents (Copilot) need the message to create a session.
        if !_initial_text.is_empty() {
            let is_copilot = agent.name() == "copilot";
            self.send_text_to_agent(&window_id, _initial_text, is_copilot)
                .await?;
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
            .resolve_agent(user_id, target.thread_id.map(|t| t.0).unwrap_or(0))
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

        let thread_id = target.thread_id.map(|t| t.0).unwrap_or(0);
        let cwd_str = cwd.to_string_lossy().to_string();
        let wid = window_id.0.clone();

        // Persist via V2 API
        self.state_mgr
            .upsert_session(&SessionInfo {
                session_id: session_id.to_string(),
                cwd: cwd_str.clone(),
                agent_type: agent.name().to_string(),
            })
            .await?;
        self.state_mgr
            .upsert_window_binding(&WindowBinding {
                window_id: wid.clone(),
                session_id: session_id.to_string(),
                cwd: cwd_str.clone(),
                agent_type: agent.name().to_string(),
                window_name: window_name.to_string(),
            })
            .await?;
        self.state_mgr
            .upsert_chat_binding(&ChatBinding {
                user_id,
                thread_id,
                chat_id: target.chat_id.0,
                display_name: window_name.to_string(),
                group_chat_id: None,
                topic_name: topic_name.map(String::from),
                session_id: session_id.to_string(),
                reply_at_only: false,
            })
            .await?;

        // Forward the user's pending message to the resumed session
        if !_initial_text.is_empty() {
            let is_copilot = agent.name() == "copilot";
            self.send_text_to_agent(&window_id, _initial_text, is_copilot)
                .await?;
        }

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
                .resolve_agent(user_id, target.thread_id.map(|t| t.0).unwrap_or(0))
                .await;
            let launch_cmd = agent_launch_cmd(&agent);
            self.tmux_mgr.send_line(&wid, &launch_cmd).await?;
            // Wait for agent process to start
            let mut agent_started = false;
            for _ in 0..10 {
                if let Ok(info) = self.tmux_mgr.find_window(&wid).await
                    && !is_shell_process(&info.current_command)
                {
                    agent_started = true;
                    break;
                }
                tokio::time::sleep(Duration::from_millis(500)).await;
            }

            if !agent_started {
                let pane = self.tmux_mgr.capture_pane(&wid).await.unwrap_or_default();
                let clean = atim_parser::terminal::TerminalParser::strip_ansi(&pane);
                let err_msg = if clean.trim().is_empty() {
                    "❌ Agent failed to start. Check agent command and configuration.".into()
                } else {
                    format!("❌ Agent failed to start:\n```\n{}```", clean.trim())
                };
                let _ = self.im_adapter.send_message(target, &err_msg).await;
                return Ok(());
            }
        }

        // Auto-confirm trust folder dialog if present (first run in a new dir)
        if self.auto_confirm_trust_dialog(&wid).await {
            // Verify the agent is still running after trust confirmation
            if let Ok(info) = self.tmux_mgr.find_window(&wid).await
                && is_shell_process(&info.current_command)
            {
                let pane = self.tmux_mgr.capture_pane(&wid).await.unwrap_or_default();
                let clean = atim_parser::terminal::TerminalParser::strip_ansi(&pane);
                let err_msg = format!(
                    "❌ Agent exited after trust dialog:\n```\n{}```",
                    clean.trim()
                );
                let _ = self.im_adapter.send_message(target, &err_msg).await;
                return Ok(());
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

        // Persist via V2 API
        let thread_id = target.thread_id.map(|t| t.0).unwrap_or(0);
        let cwd = self.tmux_mgr.pane_cwd(&wid).await.unwrap_or_default();
        let agent = self.resolve_agent(user_id, thread_id).await;

        self.state_mgr
            .upsert_window_binding(&WindowBinding {
                window_id: window_id.to_string(),
                session_id: String::new(),
                cwd: cwd.clone(),
                agent_type: agent.name().to_string(),
                window_name: window_name.clone(),
            })
            .await?;
        self.state_mgr
            .upsert_chat_binding(&ChatBinding {
                user_id,
                thread_id,
                chat_id: target.chat_id.0,
                display_name: window_name.to_string(),
                group_chat_id: None,
                topic_name: topic_name.map(String::from),
                session_id: String::new(),
                reply_at_only: false,
            })
            .await?;

        // Resolve session_id FIRST via /status so the monitor can track
        // responses. Sending /status before the user's message avoids
        // the modal being open when the message's Enter is sent.
        if let Some(sid) = self
            .resolve_session_id(window_id, Duration::from_secs(15), Some(&cwd))
            .await
        {
            self.state_mgr
                .upsert_window_binding(&WindowBinding {
                    window_id: window_id.to_string(),
                    session_id: sid.clone(),
                    cwd: cwd.clone(),
                    agent_type: agent.name().to_string(),
                    window_name: window_name.clone(),
                })
                .await?;
            self.state_mgr
                .upsert_chat_binding(&ChatBinding {
                    user_id,
                    thread_id,
                    chat_id: target.chat_id.0,
                    display_name: window_name.to_string(),
                    group_chat_id: None,
                    topic_name: topic_name.map(String::from),
                    session_id: sid,
                    reply_at_only: false,
                })
                .await?;
        }

        // Forward the user's pending message to the agent.
        // Some agents need the message to create a session file.
        if !_text.is_empty() {
            let is_copilot = agent.name() == "copilot";
            self.send_text_to_agent(&wid, _text, is_copilot).await?;
        }

        Ok(())
    }

    #[allow(dead_code)]
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
            .resolve_agent(user_id, target.thread_id.map(|t| t.0).unwrap_or(0))
            .await;
        let launch_cmd = agent_launch_cmd(&agent);
        self.tmux_mgr.send_line(&window_id, &launch_cmd).await?;

        // Wait for agent process to actually start (non-shell process appears)
        let mut agent_started = false;
        for _ in 0..10 {
            if let Ok(info) = self.tmux_mgr.find_window(&window_id).await
                && !is_shell_process(&info.current_command)
            {
                agent_started = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(500)).await;
        }

        if !agent_started {
            let pane = self
                .tmux_mgr
                .capture_pane(&window_id)
                .await
                .unwrap_or_default();
            let clean = atim_parser::terminal::TerminalParser::strip_ansi(&pane);
            let err_msg = if clean.trim().is_empty() {
                "❌ Agent failed to start. Check agent command and configuration.".into()
            } else {
                format!("❌ Agent failed to start:\n```\n{}```", clean.trim())
            };
            let _ = self.im_adapter.send_message(target, &err_msg).await;
            return Ok(());
        }

        // Wait for the agent's TUI to finish rendering before forwarding
        // the first message, otherwise the text lands in the input box but
        // Enter gets consumed by an animation/transition rather than submitting.
        let _ = self
            .tmux_mgr
            .wait_for_agent_ready(&window_id, Duration::from_secs(4))
            .await;

        // Notify user the session is ready
        let _ = self
            .im_adapter
            .send_message(
                target,
                "✅ Session ready! Send your message to start chatting.",
            )
            .await;

        // Persist via V2 API
        let thread_id = target.thread_id.map(|t| t.0).unwrap_or(0);
        let wid = window_id.0.clone();

        self.state_mgr
            .upsert_window_binding(&WindowBinding {
                window_id: wid.clone(),
                session_id: String::new(),
                cwd: cwd.clone(),
                agent_type: agent.name().to_string(),
                window_name: window_name.to_string(),
            })
            .await?;
        self.state_mgr
            .upsert_chat_binding(&ChatBinding {
                user_id,
                thread_id,
                chat_id: target.chat_id.0,
                display_name: window_name.to_string(),
                group_chat_id: None,
                topic_name: topic_name.map(String::from),
                session_id: String::new(),
                reply_at_only: false,
            })
            .await?;

        // Try to resolve session_id so the monitor can track responses.
        if let Some(sid) = self
            .resolve_session_id(&wid, Duration::from_secs(15), Some(&cwd))
            .await
        {
            self.state_mgr
                .upsert_window_binding(&WindowBinding {
                    window_id: wid.clone(),
                    session_id: sid.clone(),
                    cwd: cwd.clone(),
                    agent_type: agent.name().to_string(),
                    window_name: window_name.to_string(),
                })
                .await?;
            // Also update the chat binding's session_id so find_cb() works
            self.state_mgr
                .upsert_chat_binding(&ChatBinding {
                    user_id,
                    thread_id,
                    chat_id: target.chat_id.0,
                    display_name: window_name.to_string(),
                    group_chat_id: None,
                    topic_name: topic_name.map(String::from),
                    session_id: sid,
                    reply_at_only: false,
                })
                .await?;
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
            let rt = self.state_mgr.load_runtime().await?;
            let chat_id = target.chat_id.0;
            let thread_id = target.thread_id.map(|t| t.0).unwrap_or(0);
            // Try exact (user_id, thread_id) match first, then fall back to
            // (user_id, chat_id) for group chats where card actions don't
            // include a thread_id.
            let cb = rt.chat_bindings.iter().find(|b| {
                b.user_id == user_id && (b.thread_id == thread_id || b.chat_id == chat_id)
            });
            let window_id = cb
                .filter(|cb| !cb.session_id.is_empty())
                .and_then(|cb| {
                    rt.window_bindings
                        .values()
                        .find(|wb| wb.session_id == cb.session_id)
                })
                .map(|wb| &wb.window_id);
            if let Some(wid) = window_id {
                let _ = handle_ui_callback(&self.tmux_mgr, wid, data).await;
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
            // Feishu group card actions always carry thread_id=0. To look up
            // the correct pending message and thread context, peek at the
            // callback token context (without consuming — browse is multi-step).
            let browse_thread_id = if thread_id == 0 {
                self.callback_contexts
                    .lock()
                    .await
                    .get(token)
                    .filter(|(uid, _, _)| *uid == user_id)
                    .map(|(_, tid, _)| *tid)
                    .unwrap_or(0)
            } else {
                thread_id
            };

            let key = (user_id, browse_thread_id);
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
            // Use fixed target so downstream (create_and_bind_in_dir etc.)
            // receives the correct thread_id instead of 0.
            let fixed_target = MessageTarget {
                chat_id: target.chat_id,
                thread_id: Some(ThreadId(browse_thread_id)),
                chat_name: target.chat_name.clone(),
            };
            self.handle_browser_action(
                &fixed_target,
                user_id,
                browse_thread_id,
                &msg_id,
                browse_action,
                &text,
            )
            .await?;
            return Ok(());
        }

        // Non-browse callbacks: validate callback context
        tracing::debug!(
            "Validating callback token: action={action} contexts_count={}",
            self.callback_contexts.lock().await.len(),
        );
        let mut ctx_lock = self.callback_contexts.lock().await;
        let ctx = Self::validate_callback_token(&mut ctx_lock, token);
        drop(ctx_lock);

        let ctx = match ctx {
            Some(c) => c,
            None => {
                tracing::warn!(
                    "Stale or invalid callback token: {data} contexts_count_before={}",
                    self.callback_contexts.lock().await.len()
                );
                if let Some(qid) = callback_query_id {
                    let _ = self
                        .im_adapter
                        .answer_callback(qid, "This selection has expired. Please start over.")
                        .await;
                }
                return Ok(());
            }
        };

        // Verify the context matches — the callback was created for this user+thread.
        // Special case: if the received thread_id is 0 (Feishu card actions from groups
        // don't include chat_type, so thread_id defaults to 0), accept if the user has
        // a chat binding matching the token's expected thread_id.
        if ctx.0 != user_id {
            tracing::warn!(
                "Callback context mismatch: expected ({},{}) got ({},{})",
                ctx.0,
                ctx.1,
                user_id,
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
        let effective_thread_id = if ctx.1 != thread_id && thread_id == 0 && ctx.1 != 0 {
            // Card action from a group — Feishu can't provide thread context.
            // Accept if the user has a binding OR a pending message for the
            // token's thread_id (new session flow before binding exists).
            let rt = self.state_mgr.load_runtime().await?;
            let has_binding = rt
                .chat_bindings
                .iter()
                .any(|b| b.user_id == user_id && b.thread_id == ctx.1);
            let has_pending = self
                .pending_messages
                .lock()
                .await
                .contains_key(&(user_id, ctx.1));
            if has_binding || has_pending {
                ctx.1
            } else {
                tracing::warn!(
                    "Callback context mismatch (no group binding or pending msg): expected ({},{}) got ({},{})",
                    ctx.0,
                    ctx.1,
                    user_id,
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
        } else if ctx.1 != thread_id {
            tracing::warn!(
                "Callback context mismatch: expected ({},{}) got ({},{})",
                ctx.0,
                ctx.1,
                user_id,
                thread_id,
            );
            if let Some(qid) = callback_query_id {
                let _ = self
                    .im_adapter
                    .answer_callback(qid, "This selection is from a different chat.")
                    .await;
            }
            return Ok(());
        } else {
            thread_id
        };

        let key = (user_id, effective_thread_id);
        // Shadow with effective_thread_id for the rest of the handler, so
        // group card actions (with thread_id=0 in the callback event) use
        // the correct thread context.
        let thread_id = effective_thread_id;

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
                // Re-insert pending message so the setup workflow can consume it.
                self.pending_messages.lock().await.insert(key, text);
                // Restore topic_name (e.g. forum thread title) so it's available
                // when create_and_bind_in_dir is eventually called.
                if let Some(ref tn) = topic_name {
                    self.topic_names
                        .lock()
                        .await
                        .insert((target.chat_id.0, thread_id), tn.clone());
                }
                let _ = self
                    .im_adapter
                    .edit_message(&target, &msg_id, "Starting new session setup...")
                    .await;
                self.send_agent_picker(&target, user_id, thread_id).await?;
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
                tracing::debug!(
                    "Agent callback: user={user_id} chat={} thread={thread_id} agent={agent_name} pending_count={}",
                    target.chat_id.0,
                    self.pending_messages.lock().await.len(),
                );
                if !agent_name.is_empty() {
                    self.pending_agents
                        .lock()
                        .await
                        .insert(key, agent_name.clone());
                    // Save as default agent for this chat
                    let _ = self
                        .state_mgr
                        .save_chat_setting(user_id, thread_id, "default_agent", &agent_name)
                        .await;
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
                self.handle_recover_session(&target, user_id, thread_id, &text, &msg_id)
                    .await?;
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
                let rt = self.state_mgr.load_runtime().await.ok();
                if let Some((cb, wid)) = rt
                    .as_ref()
                    .and_then(|rt| rt.chat_binding_with_window(user_id, thread_id))
                {
                    let wid = WindowId(wid.to_string());
                    let _ = self.tmux_mgr.send_line(&wid, &text).await;
                    let sc_chat_id = cb.group_chat_id.unwrap_or(cb.chat_id);
                    self.status_consumed
                        .lock()
                        .await
                        .remove(&(sc_chat_id, cb.thread_id));
                }
                let _ = self
                    .im_adapter
                    .edit_message(&target, &msg_id, "Session renamed. Message forwarded.")
                    .await;
            }
            "rebind" => {
                let rt = self.state_mgr.load_runtime().await.ok();
                let info = rt
                    .as_ref()
                    .and_then(|rt| rt.chat_binding_with_window(user_id, thread_id));
                if let Some((binding, wid_str)) = info {
                    let wid = WindowId(wid_str.to_string());
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

        // Clean up pending messages for this thread — remove ALL entries for this thread
        // regardless of user, since the topic is closed for everyone.
        let mut pending = self.pending_messages.lock().await;
        pending.retain(|k, _| k.1 != thread_id);
        drop(pending);

        // Find and kill the associated tmux window, then remove binding
        let mut rt = self.state_mgr.load_runtime().await?;
        // Collect window_ids matching this (chat_id, thread_id) across all users
        let window_ids: Vec<String> = rt
            .chat_bindings
            .iter()
            .filter(|b| b.chat_id == chat_id && b.thread_id == thread_id)
            .filter_map(|cb| {
                rt.resolve_window_id(cb.user_id, cb.thread_id)
                    .map(|w| w.to_string())
            })
            .collect();
        for wid_str in &window_ids {
            let wid = WindowId(wid_str.clone());
            tracing::info!("Killing tmux window {} for closed topic", wid.0);
            if let Err(e) = self.tmux_mgr.kill_window(&wid).await {
                tracing::debug!("Error killing window {} (may already be gone): {e}", wid.0);
            }
        }
        rt.chat_bindings
            .retain(|b| b.chat_id != chat_id || b.thread_id != thread_id);
        // Also clean up window_bindings that no longer have a chat_binding
        let active_sessions: std::collections::HashSet<String> = rt
            .chat_bindings
            .iter()
            .map(|b| b.session_id.clone())
            .filter(|s| !s.is_empty())
            .collect();
        rt.window_bindings
            .retain(|_, wb| active_sessions.contains(&wb.session_id));
        self.state_mgr.save_runtime(&rt).await?;
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
        let mut rt = self.state_mgr.load_runtime().await?;

        // Collect session_ids before mutating to avoid borrow conflict
        let session_ids: Vec<String> = rt
            .chat_bindings
            .iter()
            .filter(|b| b.chat_id == target.chat_id.0 && b.thread_id == thread_id)
            .map(|b| b.session_id.clone())
            .collect();

        for binding in rt
            .chat_bindings
            .iter_mut()
            .filter(|b| b.chat_id == target.chat_id.0 && b.thread_id == thread_id)
        {
            binding.topic_name = Some(new_name.to_string());
        }

        for sid in &session_ids {
            if !sid.is_empty()
                && let Some(wb) = rt.window_bindings.values().find(|wb| wb.session_id == *sid)
            {
                let wid = WindowId(wb.window_id.clone());
                if let Err(e) = self.tmux_mgr.rename_window(&wid, new_name).await {
                    tracing::warn!("Failed to rename tmux window {}: {e}", wid.0);
                }
            }
        }
        self.state_mgr.save_runtime(&rt).await?;
        Ok(())
    }

    /// Periodically probe whether forum topics still exist.
    ///
    /// Uses `send_chat_action` as a lightweight probe. If the topic was
    /// deleted, Telegram returns an error and we treat it as a topic close.
    async fn probe_topic_deletions(&self) -> Result<()> {
        let rt = self.state_mgr.load_runtime().await?;
        let mut deleted: Vec<(i64, i64, String)> = Vec::new();

        for binding in &rt.chat_bindings {
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
                        binding.session_id.clone(),
                    ));
                }
            }
        }

        // Clean up deleted topics
        if !deleted.is_empty() {
            let mut rt = self.state_mgr.load_runtime().await?;
            for (chat_id, thread_id, session_id) in &deleted {
                // Kill the window if we can find it
                if !session_id.is_empty() {
                    let dead_windows: Vec<String> = rt
                        .window_bindings
                        .values()
                        .filter(|wb| &wb.session_id == session_id)
                        .map(|wb| wb.window_id.clone())
                        .collect();
                    for wid_str in &dead_windows {
                        let wid = WindowId(wid_str.clone());
                        if let Err(e) = self.tmux_mgr.kill_window(&wid).await {
                            tracing::debug!("Error killing stale window {}: {e}", wid.0);
                        }
                    }
                }
                rt.chat_bindings
                    .retain(|b| !(b.chat_id == *chat_id && b.thread_id == *thread_id));
                tracing::info!("Cleaned up deleted topic: chat={chat_id} thread={thread_id}");
            }
            // Clean up orphaned window_bindings
            let active_sessions: std::collections::HashSet<String> = rt
                .chat_bindings
                .iter()
                .map(|b| b.session_id.clone())
                .filter(|s| !s.is_empty())
                .collect();
            rt.window_bindings
                .retain(|_, wb| active_sessions.contains(&wb.session_id));
            self.state_mgr.save_runtime(&rt).await?;
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
        let rt = self.state_mgr.load_runtime().await?;

        // Clean up UI/pane state for windows that no longer exist, preventing
        // unbounded growth across window kills/rebinds. Lock is short-lived.
        let live_windows: HashSet<String> = rt
            .window_bindings
            .values()
            .map(|wb| wb.window_id.clone())
            .collect();
        self.last_ui_states
            .lock()
            .await
            .retain(|wid, _| live_windows.contains(wid));
        self.last_pane_output
            .lock()
            .await
            .retain(|wid, _| live_windows.contains(wid));

        // Clean up tool_use message tracking for bindings that are gone
        // (e.g. session unbound), preventing unbounded growth when an agent
        // is interrupted before a ToolResult arrives.
        let live_chats: HashSet<(i64, i64)> = rt
            .chat_bindings
            .iter()
            .map(|cb| (cb.group_chat_id.unwrap_or(cb.chat_id), cb.thread_id))
            .collect();
        self.tool_use_msg_ids
            .lock()
            .await
            .retain(|(chat_id, tid, _), _| live_chats.contains(&(*chat_id, *tid)));

        // Deduplicate by window_id: when multiple chat bindings share the same
        // session, only probe the last binding per window to avoid sending
        // interactive UI (e.g. permission requests) to all groups.
        let mut seen_windows = HashSet::new();
        let bindings: Vec<_> = rt
            .resolved_bindings()
            .into_iter()
            .rev()
            .filter(|(_cb, wb)| seen_windows.insert(wb.window_id.clone()))
            .collect();

        for (cb, wb) in bindings.into_iter().rev() {
            let wid = WindowId(wb.window_id.clone());
            let pane_text = match self.tmux_mgr.capture_pane(&wid).await {
                Ok(t) => t,
                Err(_) => continue, // window gone
            };

            // Strip ANSI
            let clean = atim_parser::terminal::TerminalParser::strip_ansi(&pane_text);

            // Agent type comes directly from the window binding
            let agent_type = wb.agent_type.as_str();
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

            // Check whether UI hash changed — short lock around the read/update.
            let hash_changed = {
                let mut ui_states = self.last_ui_states.lock().await;
                let prev = ui_states.get(&wb.window_id);
                if prev == Some(&content_hash) {
                    false
                } else {
                    ui_states.insert(wb.window_id.clone(), content_hash);
                    true
                }
            };

            if !hash_changed {
                // UI unchanged — still forward pane output if agent uses PaneCapture
                if agent.output_source() == OutputSource::PaneCapture {
                    self.forward_new_pane_output(wb, cb, &clean).await;
                }
                continue;
            }

            // New or changed UI — send keyboard
            if let Some(interactive) = ui {
                // Send all interactive UI cards including AskUserQuestion
                // (options are extracted and shown as clickable buttons)
                {
                    let target = MessageTarget {
                        chat_id: ChatId(cb.group_chat_id.unwrap_or(cb.chat_id)),
                        thread_id: Some(ThreadId(cb.thread_id)),
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
                self.forward_new_pane_output(wb, cb, &clean).await;
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
    async fn forward_new_pane_output(&self, wb: &WindowBinding, cb: &ChatBinding, clean: &str) {
        // Only hold the lock for the map read + baseline update. The diff and
        // the send_message await happen outside the lock to avoid blocking
        // other pane-output updates on network I/O.
        let prev = {
            let mut pane_outputs = self.last_pane_output.lock().await;
            let last = pane_outputs.get(&wb.window_id).cloned();
            pane_outputs.insert(wb.window_id.clone(), clean.to_string());
            last
        };

        // First call: establish baseline only
        let Some(prev) = prev else {
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
                let c = t
                    .chars()
                    .next()
                    .expect("t is non-empty (checked by is_empty filter above)");
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
            chat_id: ChatId(cb.group_chat_id.unwrap_or(cb.chat_id)),
            thread_id: Some(ThreadId(cb.thread_id)),
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
    tmux: TerminalMgr,
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
/// Convert an ISO 8601 timestamp to a human-readable relative time.
///
/// Returns strings like "13s", "2m", "4h", "2d", "1w", "4w".
fn relative_time(ts: &str) -> String {
    let Ok(dt) = chrono::DateTime::parse_from_rfc3339(ts) else {
        // Try without timezone
        if let Ok(naive) = chrono::NaiveDateTime::parse_from_str(ts, "%Y-%m-%dT%H:%M:%S") {
            let dt = naive.and_utc();
            let now = chrono::Utc::now();
            let delta = now.signed_duration_since(dt);
            return format_delta(delta);
        }
        return "(unknown)".into();
    };
    let now = chrono::Utc::now();
    let delta = now.signed_duration_since(dt);
    format_delta(delta)
}

fn format_delta(delta: chrono::TimeDelta) -> String {
    let secs = delta.num_seconds();
    if secs < 60 {
        format!("{}s", secs.max(0))
    } else if secs < 3600 {
        format!("{}m", secs / 60)
    } else if secs < 86400 {
        format!("{}h", secs / 3600)
    } else if secs < 604800 {
        format!("{}d", secs / 86400)
    } else {
        format!("{}w", secs / 604800)
    }
}

fn is_shell_process(process: &str) -> bool {
    matches!(
        process,
        "zsh" | "bash" | "sh" | "fish" | "dash" | "ksh" | "powershell" | "pwsh" | "cmd" // Windows shells
    )
}

/// Build a markdown card showing Edit tool diff content.
fn build_edit_diff_card(raw_input: &str, fallback: &str) -> String {
    let parsed: serde_json::Value = match serde_json::from_str(raw_input) {
        Ok(v) => v,
        Err(_) => return fallback.to_string(),
    };
    let file_path = parsed["file_path"].as_str().unwrap_or("?");
    let old = parsed["old_string"].as_str().unwrap_or("");
    let new = parsed["new_string"].as_str().unwrap_or("");
    let replace_all = parsed["replace_all"].as_bool().unwrap_or(false);

    let mut out = format!("✏️ Edit: {file_path}");
    if replace_all {
        out.push_str(" (replace_all)");
    }

    // Truncate long strings for display
    let old_display = if old.len() > 800 {
        format!(
            "{}…",
            &old[..old
                .char_indices()
                .nth(797)
                .map(|(i, _)| i)
                .unwrap_or(old.len())]
        )
    } else {
        old.to_string()
    };
    let new_display = if new.len() > 800 {
        format!(
            "{}…",
            &new[..new
                .char_indices()
                .nth(797)
                .map(|(i, _)| i)
                .unwrap_or(new.len())]
        )
    } else {
        new.to_string()
    };

    out.push_str(&format!(
        "\n```diff\n- {}\n+ {}\n```",
        old_display.replace('\n', "\n- "),
        new_display.replace('\n', "\n+ ")
    ));
    out
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

/// Extract meaningful text from a Claude Code modal capture.
///
/// Strips box-drawing characters, control characters, and lines that are
/// purely decorative (separators, blank lines with only box-drawing glyphs).
fn extract_modal_text(s: &str) -> String {
    let mut lines: Vec<String> = Vec::new();
    for line in s.lines() {
        let trimmed = line.trim();

        // Skip lines that contain only box-drawing + whitespace
        if trimmed
            .chars()
            .all(|c| c.is_whitespace() || BOX_DRAWING.contains(&c))
        {
            continue;
        }

        // Skip the "Press q to close" / "q to close" instruction line
        if trimmed.to_lowercase().contains("q to close")
            || trimmed.to_lowercase().contains("press ")
        {
            continue;
        }

        // Strip remaining box-drawing glyphs from content lines
        let cleaned: String = trimmed
            .chars()
            .filter(|c| !BOX_DRAWING.contains(c) && !c.is_control())
            .collect::<String>()
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ");

        if !cleaned.is_empty() {
            lines.push(cleaned);
        }
    }
    lines.join("\n")
}

/// Extract key-value pairs from a Claude Code modal capture.
fn extract_kv_rows(s: &str, exclude_keys: &[&str]) -> Vec<(String, String)> {
    let clean = strip_ansi(s);
    let mut rows = Vec::new();

    for line in clean.lines() {
        let trimmed = line.trim();

        if trimmed.is_empty()
            || trimmed
                .chars()
                .all(|c| c.is_whitespace() || BOX_DRAWING.contains(&c))
        {
            continue;
        }

        let cleaned: String = trimmed
            .chars()
            .filter(|c| !BOX_DRAWING.contains(c) && !c.is_control())
            .collect();
        let cleaned = cleaned.trim();

        if cleaned
            .split_whitespace()
            .all(|w| matches!(w, "Settings" | "Status" | "Config" | "Usage" | "Stats"))
            || cleaned.eq_ignore_ascii_case("status")
            || cleaned.eq_ignore_ascii_case("usage")
        {
            continue;
        }

        if cleaned.to_lowercase().contains("q to close")
            || cleaned.to_lowercase().contains("press ")
            || cleaned.to_lowercase().contains("esc to")
            || cleaned.to_lowercase().contains("esc ")
        {
            continue;
        }

        if let Some((key, val)) = cleaned.split_once(':') {
            let k = key.trim();
            let v = val.trim();
            if !k.is_empty() && !exclude_keys.contains(&k) {
                rows.push((k.to_string(), v.to_string()));
            }
        }
    }

    rows
}

const BOX_DRAWING: &[char] = &[
    '─', '━', '│', '┃', '┄', '┅', '┆', '┇', '┈', '┉', '┊', '┋', '┌', '┍', '┎', '┏', '┐', '┑', '┒',
    '┓', '└', '┕', '┖', '┗', '┘', '┙', '┚', '┛', '├', '┝', '┞', '┟', '┠', '┡', '┢', '┣', '┤', '┥',
    '┦', '┧', '┨', '┩', '┪', '┫', '┬', '┭', '┮', '┯', '┰', '┱', '┲', '┳', '┴', '┵', '┶', '┷', '┸',
    '┹', '┺', '┻', '┼', '┽', '┾', '┿', '╀', '╁', '╂', '╃', '╄', '╅', '╆', '╇', '╈', '╉', '╊', '╋',
    '╌', '╍', '╎', '╏', '═', '║', '╒', '╓', '╔', '╕', '╖', '╗', '╘', '╙', '╚', '╛', '╜', '╝', '╞',
    '╟', '╠', '╡', '╢', '╣', '╤', '╥', '╦', '╧', '╨', '╩', '╪', '╫', '╬', '╭', '╮', '╯', '╰', '╱',
    '╲', '╳', '╴', '╵', '╶', '╷', '╸', '╹', '╺', '╻', '╼', '╽', '╾', '╿',
];

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

/// Extract option lines from AskUserQuestion content.
///
/// Options typically appear as:
///   ❯ 1. Option A
///     2. Option B
///     3. Option C
/// or:
///   ☐ Option A
///   ☐ Option B
fn extract_options(content: &str) -> Vec<String> {
    let mut options = Vec::new();
    for line in content.lines() {
        let trimmed = line.trim();
        // Match numbered options: "❯ 1. Foo", "  2. Bar", "☐ Foo"
        if let Some(rest) = trimmed.strip_prefix('❯').map(|s| s.trim())
            && let Some(opt) = rest.strip_prefix(|c: char| c.is_ascii_digit())
            && let Some(opt) = opt.strip_prefix('.').or_else(|| opt.strip_prefix(')'))
        {
            options.push(opt.trim().to_string());
            continue;
        }
        if let Some(rest) = trimmed.strip_prefix(|c: char| c.is_ascii_digit())
            && let Some(opt) = rest.strip_prefix('.').or_else(|| rest.strip_prefix(')'))
        {
            let opt = opt.trim();
            if !opt.is_empty() && !opt.starts_with('·') {
                options.push(opt.to_string());
                continue;
            }
        }
        // Match checkbox options: "☐ Foo", "✔ Foo"
        if trimmed.starts_with('☐') || trimmed.starts_with('✔') || trimmed.starts_with('☒') {
            options.push(trimmed[4..].trim().to_string());
            continue;
        }
    }
    options
}

/// Build inline keyboard buttons for an interactive UI.
fn ui_to_buttons(ui: &InteractiveUi) -> Vec<Vec<Button>> {
    match ui.kind {
        UiKind::AskUserQuestion => {
            let options = extract_options(&ui.content);
            if options.is_empty() {
                // Fallback: generic Up/Down/Select buttons
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
            } else {
                // Each option as a clickable button — send the option text via
                // ui:select:<index>, which the handler translates to Down×N + Enter.
                let mut buttons: Vec<Vec<Button>> = options
                    .into_iter()
                    .enumerate()
                    .map(|(i, opt)| {
                        let label = if opt.len() > 45 {
                            format!(
                                "{}…",
                                &opt[..opt
                                    .char_indices()
                                    .nth(42)
                                    .map(|(j, _)| j)
                                    .unwrap_or(opt.len())]
                            )
                        } else {
                            opt
                        };
                        vec![Button {
                            text: label,
                            callback_data: format!("ui:select:{i}"),
                        }]
                    })
                    .collect();
                buttons.push(vec![Button {
                    text: "✖ Cancel".into(),
                    callback_data: "ui:esc".into(),
                }]);
                buttons
            }
        }
        UiKind::ExitPlanMode => {
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
    tmux: &TerminalMgr,
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
        s if s.starts_with("ui:select:") => {
            // Navigate to option index then press Enter
            let idx: usize = s[10..].parse().unwrap_or(0);
            // First option is already selected (❯), so press Down for each index > 0
            for _ in 0..idx {
                tmux.send_key(&wid, "Down").await?;
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
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
        let token = Server::make_callback_token(&mut contexts, 100, -456);
        assert_eq!(contexts.len(), 1);

        let ctx = Server::validate_callback_token(&mut contexts, &token);
        assert_eq!(ctx, Some((100, -456)));
        assert!(contexts.is_empty());
    }

    #[test]
    fn test_callback_token_rejects_stale() {
        let mut contexts = HashMap::new();
        let token = Server::make_callback_token(&mut contexts, 100, -456);
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
        let t1 = Server::make_callback_token(&mut contexts, 100, -456);
        let t2 = Server::make_callback_token(&mut contexts, 200, -456);
        assert_ne!(t1, t2);
        assert_eq!(contexts.len(), 2);
    }

    #[test]
    fn test_callback_token_embedded_format() {
        let mut contexts = HashMap::new();
        let token = Server::make_callback_token(&mut contexts, 100, -456);

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
        assert_eq!(ctx, Some((100, -456)));
    }

    #[test]
    fn test_strip_ansi_basic() {
        assert_eq!(strip_ansi("hello"), "hello");
        assert_eq!(strip_ansi("\x1b[31mred\x1b[0m"), "red");
        assert_eq!(strip_ansi("\x1b[1mbold\x1b[22m"), "bold");
    }

    #[test]
    fn test_extract_modal_text_usage() {
        let input = "╭──────────────────────╮\n│ Usage                │\n├──────────────────────┤\n│ Input tokens    1234 │\n│ Output tokens   5678 │\n│ Total tokens   6912  │\n│ Cost           $0.12 │\n├──────────────────────┤\n│ Press q to close     │\n╰──────────────────────╯\n";
        let result = extract_modal_text(input);
        assert!(result.contains("Usage"));
        assert!(result.contains("Input tokens 1234"));
        assert!(result.contains("Cost"));
        assert!(!result.to_lowercase().contains("q to close"));
        assert!(!result.contains("╭"));
    }

    #[test]
    fn test_extract_modal_text_with_ansi() {
        let input = "\x1b[32m╭──────────────────────╮\n\x1b[0m│ \x1b[1mStatus\x1b[0m                │\n├──────────────────────┤\n│ Mode: Normal         │\n│ Messages: 42         │\n╰──────────────────────╯\n";
        // strip_ansi first, then extract
        let cleaned = strip_ansi(input);
        let result = extract_modal_text(&cleaned);
        assert!(result.contains("Status"));
        assert!(result.contains("Mode: Normal"));
        assert!(result.contains("Messages: 42"));
    }

    #[test]
    fn test_extract_modal_text_empty() {
        assert_eq!(extract_modal_text(""), "");
        assert_eq!(extract_modal_text("╭─╮\n│ │\n╰─╯"), "");
    }

    #[test]
    fn test_extract_kv_rows_basic() {
        let input = "Settings Status Config Usage Stats\nStatus\nVersion: 2.1.152\nSession name: add-nerd-font-support\nModel: deepseek-chat\nEsc to cancel";
        let rows = extract_kv_rows(input, &["Auth token", "Setting sources", "API key"]);
        assert!(rows.iter().any(|(k, _)| k == "Version"));
        assert!(rows.iter().any(|(_, v)| v == "2.1.152"));
        assert!(rows.iter().any(|(k, _)| k == "Model"));
        assert!(!rows.iter().any(|(k, _)| k == "Auth token"));
    }

    #[test]
    fn test_extract_kv_rows_filters_diagnostics() {
        let input = "Status\nVersion: 2.1.152\nSession name: test\n⚠ installMethod is native\nbut claude not found";
        let rows = extract_kv_rows(input, &["Auth token", "Setting sources", "API key"]);
        assert!(rows.iter().any(|(k, _)| k == "Version"));
        assert_eq!(rows.len(), 2); // only Version and Session name
    }

    #[test]
    fn test_extract_kv_rows_usage() {
        let input = "Usage\nInput tokens: 1234\nOutput tokens: 5678\nTotal tokens: 6912\nCost: $0.12\nPress q to close";
        let rows = extract_kv_rows(input, &[]);
        assert!(rows.iter().any(|(k, _)| k == "Input tokens"));
        assert!(rows.iter().any(|(_, v)| v == "1234"));
        assert!(rows.iter().any(|(k, _)| k == "Cost"));
        assert_eq!(rows.len(), 4);
    }

    #[test]
    fn test_extract_kv_rows_empty() {
        assert_eq!(extract_kv_rows("", &[]).len(), 0);
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

    // ─── Core path integration tests ───────────────────────────
    //
    // These tests exercise the key routing/binding paths without tmux or
    // IM backends. They construct RuntimeState directly and verify that
    // the correct window/chat_binding is resolved.

    fn make_test_runtime() -> RuntimeState {
        let mut rt = RuntimeState::default();
        rt.window_bindings.insert(
            "@1".into(),
            WindowBinding {
                window_id: "@1".into(),
                session_id: "ses_test".into(),
                cwd: "/home/user/project".into(),
                agent_type: "claude".into(),
                window_name: "test-window".into(),
            },
        );
        rt.window_bindings.insert(
            "@2".into(),
            WindowBinding {
                window_id: "@2".into(),
                session_id: "ses_other".into(),
                cwd: "/home/user/other".into(),
                agent_type: "claude".into(),
                window_name: "other-window".into(),
            },
        );
        rt.chat_bindings = vec![
            ChatBinding {
                user_id: 100,
                thread_id: 200,
                chat_id: 200,
                display_name: "test-window".into(),
                group_chat_id: None,
                topic_name: None,
                session_id: "ses_test".into(),
                reply_at_only: false,
            },
            ChatBinding {
                user_id: 100,
                thread_id: 300,
                chat_id: 300,
                display_name: "other-window".into(),
                group_chat_id: None,
                topic_name: None,
                session_id: "ses_other".into(),
                reply_at_only: false,
            },
        ];
        rt.sessions.insert(
            "ses_test".into(),
            SessionInfo {
                session_id: "ses_test".into(),
                cwd: "/home/user/project".into(),
                agent_type: "claude".into(),
            },
        );
        rt.sessions.insert(
            "ses_other".into(),
            SessionInfo {
                session_id: "ses_other".into(),
                cwd: "/home/user/other".into(),
                agent_type: "claude".into(),
            },
        );
        rt
    }

    #[test]
    fn test_resolve_window_binding_by_user_thread() {
        let rt = make_test_runtime();
        let wb = rt.resolve_window_binding(100, 200).unwrap();
        assert_eq!(wb.window_id, "@1");
        assert_eq!(wb.cwd, "/home/user/project");
    }

    #[test]
    fn test_resolve_window_binding_nonexistent_user() {
        let rt = make_test_runtime();
        assert!(rt.resolve_window_binding(999, 200).is_none());
    }

    #[test]
    fn test_resolve_window_binding_empty_session_id() {
        let mut rt = make_test_runtime();
        rt.chat_bindings.push(ChatBinding {
            user_id: 200,
            thread_id: 400,
            chat_id: 400,
            display_name: "unbound".into(),
            group_chat_id: None,
            topic_name: None,
            session_id: String::new(),
            reply_at_only: false,
        });
        assert!(rt.resolve_window_binding(200, 400).is_none());
    }

    #[test]
    fn test_resolve_window_binding_multiple_chats_same_window() {
        let mut rt = make_test_runtime();
        rt.chat_bindings.push(ChatBinding {
            user_id: 100,
            thread_id: 201,
            chat_id: 201,
            display_name: "test-window-2".into(),
            group_chat_id: None,
            topic_name: None,
            session_id: "ses_test".into(),
            reply_at_only: false,
        });
        // Both chat bindings should resolve to the same window
        let wb1 = rt.resolve_window_binding(100, 200).unwrap();
        let wb2 = rt.resolve_window_binding(100, 201).unwrap();
        assert_eq!(wb1.window_id, wb2.window_id);
    }

    #[test]
    fn test_session_map_changed_updates_stale_chat_binding() {
        let mut rt = make_test_runtime();
        // Simulate a stale chat_binding with an old session_id
        // (same display_name as the window, but old session UUID)
        rt.chat_bindings.push(ChatBinding {
            user_id: 300,
            thread_id: 500,
            chat_id: 500,
            display_name: "fresh-window".into(),
            group_chat_id: None,
            topic_name: None,
            session_id: "ses_old".into(),
            reply_at_only: false,
        });
        rt.window_bindings.insert(
            "@3".into(),
            WindowBinding {
                window_id: "@3".into(),
                session_id: "ses_new".into(),
                cwd: "/home/user/fresh".into(),
                agent_type: "claude".into(),
                window_name: "fresh-window".into(),
            },
        );

        // Simulate SessionMapChanged logic: update chat_binding when
        // window_binding has new session_id and display_name matches
        for (window_id, session_id) in &[("@3", "ses_new")] {
            if let Some(wb) = rt.window_bindings.get(*window_id) {
                let window_name = wb.window_name.clone();
                if let Some(cb) = rt.chat_bindings.iter_mut().find(|cb| {
                    cb.display_name == window_name
                        && (cb.session_id.is_empty() || cb.session_id != *session_id)
                }) {
                    cb.session_id = (*session_id).to_string();
                }
            }
        }

        let cb = rt
            .chat_bindings
            .iter()
            .find(|cb| cb.user_id == 300 && cb.thread_id == 500)
            .unwrap();
        assert_eq!(
            cb.session_id, "ses_new",
            "stale session_id should be updated"
        );
    }

    #[test]
    fn test_session_map_changed_skips_matching_session() {
        let mut rt = make_test_runtime();

        // Should NOT update because session_id already matches
        for (window_id, session_id) in &[("@1", "ses_test")] {
            if let Some(wb) = rt.window_bindings.get(*window_id) {
                let window_name = wb.window_name.clone();
                if let Some(cb) = rt.chat_bindings.iter_mut().find(|cb| {
                    cb.display_name == window_name
                        && (cb.session_id.is_empty() || cb.session_id != *session_id)
                }) {
                    cb.session_id = "should_not_change".into();
                }
            }
        }

        let cb = rt
            .chat_bindings
            .iter()
            .find(|cb| cb.user_id == 100 && cb.thread_id == 200)
            .unwrap();
        assert_eq!(cb.session_id, "ses_test");
    }

    #[test]
    fn test_find_cb_by_user_and_thread() {
        let rt = make_test_runtime();
        // find_cb searches chat_bindings by (user_id, thread_id)
        let cb = rt
            .chat_bindings
            .iter()
            .find(|b| b.user_id == 100 && b.thread_id == 200);
        assert!(cb.is_some());
        assert_eq!(cb.unwrap().display_name, "test-window");
    }

    #[test]
    fn test_window_binding_updated_after_resolve() {
        let mut rt = make_test_runtime();

        // Simulate handle_text_message: find real window by display_name,
        // then update window_binding
        let display_name = "test-window".to_string();
        let real_wid = "@5".to_string();
        let binding = rt
            .chat_bindings
            .iter()
            .find(|b| b.display_name == display_name)
            .cloned()
            .unwrap();

        // Remove stale, insert new
        rt.window_bindings.remove("@1");
        rt.window_bindings.insert(
            real_wid.clone(),
            WindowBinding {
                window_id: real_wid.clone(),
                session_id: binding.session_id.clone(),
                cwd: "/new/cwd".into(),
                agent_type: "claude".into(),
                window_name: binding.display_name.clone(),
            },
        );

        let wb = rt.resolve_window_binding(100, 200).unwrap();
        assert_eq!(wb.window_id, "@5");
        assert_eq!(wb.cwd, "/new/cwd");
    }

    #[test]
    fn test_recover_cwd_fallback_to_jsonl_stale_home() {
        let mut rt = make_test_runtime();
        // Simulate a dead window where cwd equals HOME (stale)
        let home = home::home_dir()
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_default();
        rt.window_bindings.insert(
            "@dead".into(),
            WindowBinding {
                window_id: "@dead".into(),
                session_id: "ses_test".into(),
                cwd: home.clone(),
                agent_type: "claude".into(),
                window_name: "test-window".into(),
            },
        );

        let session_id = "ses_test".to_string();
        let cwd = home.clone();

        // The stale check: if cwd == HOME, use the session store's cwd
        let resolved = if !session_id.is_empty() && (cwd.is_empty() || cwd == home) {
            rt.sessions
                .get(&session_id)
                .map(|si| si.cwd.clone())
                .unwrap_or_else(|| home.clone())
        } else {
            cwd
        };

        assert_eq!(resolved, "/home/user/project");
    }

    #[test]
    fn test_clear_removes_session_id_from_bindings() {
        let mut rt = make_test_runtime();
        let wid = "@1".to_string();

        // Simulate /clear Phase 1: clear session bindings
        let old_sid = {
            let wb = rt.window_bindings.get(&wid).unwrap();
            wb.session_id.clone()
        };
        assert_eq!(old_sid, "ses_test");

        // Clear window_binding session_id
        if let Some(wb) = rt.window_bindings.get_mut(&wid) {
            wb.session_id.clear();
        }
        // Clear all chat_bindings referencing this session_id
        for cb in rt.chat_bindings.iter_mut() {
            if cb.session_id == old_sid {
                cb.session_id.clear();
            }
        }

        assert!(rt.window_bindings.get(&wid).unwrap().session_id.is_empty());
        assert!(
            rt.chat_bindings
                .iter()
                .all(|cb| cb.session_id != "ses_test")
        );
    }

    #[test]
    fn test_rebind_updates_window_binding_session() {
        let mut rt = make_test_runtime();

        // Simulate /rebind: discover new session, update window binding
        let wid = "@1".to_string();
        let new_sid = "ses_new_uuid".to_string();

        if let Some(wb) = rt.window_bindings.get_mut(&wid) {
            wb.session_id = new_sid.clone();
        }

        assert_eq!(
            rt.window_bindings.get(&wid).unwrap().session_id,
            "ses_new_uuid"
        );
    }

    #[test]
    fn test_rebind_updates_chat_binding_session() {
        let mut rt = make_test_runtime();

        // Simulate /rebind: update chat_binding's session_id to match
        let new_sid = "ses_new_uuid".to_string();
        if let Some(cb) = rt
            .chat_bindings
            .iter_mut()
            .find(|cb| cb.user_id == 100 && cb.thread_id == 200)
        {
            cb.session_id = new_sid.clone();
        }

        assert_eq!(
            rt.chat_bindings
                .iter()
                .find(|cb| cb.user_id == 100 && cb.thread_id == 200)
                .unwrap()
                .session_id,
            "ses_new_uuid"
        );
    }

    #[test]
    fn test_rebind_with_display_name_fallback() {
        let mut rt = make_test_runtime();

        // Simulate rebind with a stale display_name that doesn't match
        // any tmux window. The fallback to chat_name should be used.
        let display_name = "atim-12345".to_string(); // stale
        let chat_name = "real-window".to_string(); // actual tmux window name

        // Window exists by chat_name, not display_name
        rt.window_bindings.insert(
            "@10".into(),
            WindowBinding {
                window_id: "@10".into(),
                session_id: "ses_real".into(),
                cwd: "/real/cwd".into(),
                agent_type: "claude".into(),
                window_name: chat_name,
            },
        );

        // Fallback logic: if display_name doesn't find window, try chat_name
        let found_by_display = rt
            .window_bindings
            .values()
            .find(|wb| wb.window_name == display_name);
        assert!(
            found_by_display.is_none(),
            "stale display_name should not match"
        );

        let found_by_chat_name = rt
            .window_bindings
            .values()
            .find(|wb| wb.window_name == "real-window");
        assert!(
            found_by_chat_name.is_some(),
            "chat_name fallback should find window"
        );
    }

    #[test]
    fn test_resolved_bindings_iterates_joined() {
        let rt = make_test_runtime();
        let pairs: Vec<_> = rt.resolved_bindings();
        assert_eq!(pairs.len(), 2);
        for (cb, wb) in &pairs {
            assert_eq!(cb.session_id, wb.session_id);
        }
    }

    #[test]
    fn test_chat_binding_with_window() {
        let rt = make_test_runtime();
        let result = rt.chat_binding_with_window(100, 200);
        assert!(result.is_some());
        let (cb, wid) = result.unwrap();
        assert_eq!(cb.display_name, "test-window");
        assert_eq!(wid, "@1");
    }

    #[test]
    fn test_chat_binding_with_window_unbound() {
        let rt = make_test_runtime();
        let result = rt.chat_binding_with_window(999, 999);
        assert!(result.is_none());
    }

    #[test]
    fn test_session_map_changed_stale_to_new() {
        let mut rt = make_test_runtime();
        // Chat binding has old session, window binding has new session
        rt.window_bindings.insert(
            "@10".into(),
            WindowBinding {
                window_id: "@10".into(),
                session_id: "ses_new".into(),
                cwd: "/path".into(),
                agent_type: "claude".into(),
                window_name: "migrated-window".into(),
            },
        );
        rt.chat_bindings.push(ChatBinding {
            user_id: 400,
            thread_id: 600,
            chat_id: 600,
            display_name: "migrated-window".into(),
            group_chat_id: None,
            topic_name: None,
            session_id: "ses_old".into(),
            reply_at_only: false,
        });

        // Apply SessionMapChanged logic
        let window_id = "@10";
        let session_id = "ses_new";
        if let Some(wb) = rt.window_bindings.get(window_id) {
            let window_name = wb.window_name.clone();
            if let Some(cb) = rt.chat_bindings.iter_mut().find(|cb| {
                cb.display_name == window_name
                    && (cb.session_id.is_empty() || cb.session_id != *session_id)
            }) {
                cb.session_id = session_id.to_string();
            }
        }

        let cb = rt
            .chat_bindings
            .iter()
            .find(|cb| cb.user_id == 400)
            .unwrap();
        assert_eq!(cb.session_id, "ses_new", "stale session should be replaced");
    }

    #[test]
    fn test_clear_offset_scoped_to_old_session() {
        // /clear should only remove offsets for the old session, not all
        let old_sid = "ses_test".to_string();
        let other_sid = "ses_other".to_string();
        let mut offsets = HashMap::from([(old_sid.clone(), 100u64), (other_sid.clone(), 200u64)]);

        offsets.remove(&old_sid);
        assert!(!offsets.contains_key(&old_sid));
        assert!(offsets.contains_key(&other_sid));
    }
}
