use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use aim_core::config::Config;
use aim_core::error::Result;
use aim_core::im::ImAdapter;
use aim_core::message::{Button, ChatId, ImEvent, ImEventKind, MessageId, MessageTarget, ThreadId, WindowId};
use aim_core::session::{ThreadBinding, WindowState};
use aim_monitor::monitor::MonitorEvent;
use aim_queue::message_queue::MessageQueue;
use aim_state::persistence::StateManager;
use aim_tmux::manager::TmuxManager;
use tokio::sync::Mutex;

/// The main application server — routes IM events to tmux and monitor
/// events back to IM.
pub struct Server {
    pub config: Config,
    pub state_mgr: StateManager,
    pub tmux_mgr: TmuxManager,
    pub queue: Arc<Mutex<MessageQueue>>,
    pub byte_offsets: Arc<Mutex<HashMap<String, u64>>>,
    pub im_adapter: Arc<dyn ImAdapter>,
    /// Track topic names by (chat_id, thread_id) from forum_topic_created/edited.
    pub topic_names: Arc<Mutex<HashMap<(i64, i64), String>>>,
    /// Pending user messages awaiting callback selection: (user_id, chat_id, thread_id) -> text.
    pub pending_messages: Arc<Mutex<HashMap<(i64, i64, i64), String>>>,
}

impl Server {
    /// Run the main event loop, processing IM and monitor events.
    pub async fn run(
        &self,
        mut im_rx: tokio::sync::mpsc::UnboundedReceiver<ImEvent>,
        monitor_rx: &mut tokio::sync::mpsc::UnboundedReceiver<MonitorEvent>,
    ) -> Result<()> {
        loop {
            tokio::select! {
                Some(event) = im_rx.recv() => {
                    self.handle_im_event(event).await?;
                }
                Some(event) = monitor_rx.recv() => {
                    self.handle_monitor_event(event).await?;
                }
                else => break,
            }
        }
        Ok(())
    }

    async fn handle_im_event(&self, event: ImEvent) -> Result<()> {
        // Check user authorization
        if !self.config.is_user_allowed(event.user_id.0) {
            tracing::warn!("Unauthorized user: {:?}", event.user_id);
            return Ok(());
        }

        match event.kind {
            ImEventKind::Text(text) => {
                self.handle_text_message(event.target, event.user_id.0, &text)
                    .await?;
            }
            ImEventKind::CallbackQuery { data, msg_id } => {
                self.handle_callback(event.target, event.user_id.0, &data, msg_id).await?;
            }
            ImEventKind::Photo { .. } => {
                tracing::info!("Photo message (not yet implemented)");
            }
            ImEventKind::Voice(_) => {
                tracing::info!("Voice message (not yet implemented)");
            }
            ImEventKind::TopicCreated { name } => {
                let mut names = self.topic_names.lock().await;
                names.insert((event.target.chat_id.0, event.target.thread_id.map(|t| t.0).unwrap_or(0)), name);
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

                for msg in &messages {
                    tracing::debug!(
                        "[pipe] NewMessage: session_id={}, role={}, content_type={:?}, complete={}, text_len={}",
                        msg.session_id.0, msg.role, msg.content_type, msg.is_complete, msg.text.len(),
                    );

                    // Only forward complete assistant text/tool_result messages
                    if msg.role != "assistant" {
                        continue;
                    }
                    if !msg.is_complete {
                        continue;
                    }
                    if msg.text.trim().is_empty() {
                        continue;
                    }
                    // Skip thinking content and tool_use metadata
                    if matches!(
                        msg.content_type,
                        aim_core::message::ContentType::Thinking
                            | aim_core::message::ContentType::ToolUse
                    ) {
                        continue;
                    }

                    // Find window_id for this session
                    let window_id = match state.window_states.iter().find(|(_, ws)| {
                        ws.session_id == msg.session_id.0
                    }) {
                        Some((wid, _)) => wid.clone(),
                        None => {
                            tracing::warn!(
                                "[pipe] No window_state for session_id={} (have {} window_states)",
                                msg.session_id.0,
                                state.window_states.len(),
                            );
                            for (wid, ws) in &state.window_states {
                                tracing::warn!("  window={} session_id={:?}", wid, ws.session_id);
                            }
                            continue;
                        }
                    };

                    // Find thread binding for this window
                    let binding = match state.thread_bindings.iter().find(|b| {
                        b.window_id == window_id
                    }) {
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

                    // Use group_chat_id for supergroup topics, chat_id for private chats
                    let chat_id = binding.group_chat_id.unwrap_or(binding.chat_id);

                    let target = MessageTarget {
                        chat_id: ChatId(chat_id),
                        thread_id: if binding.thread_id != 0 {
                            Some(ThreadId(binding.thread_id))
                        } else {
                            None
                        },
                    };

                    tracing::info!(
                        "[pipe] Forwarding to chat={} thread={:?}: {} chars",
                        target.chat_id.0,
                        target.thread_id,
                        msg.text.len(),
                    );

                    if let Err(e) = self.im_adapter.send_message(&target, &msg.text).await {
                        tracing::error!("[pipe] Failed to send response: {e}");
                    } else {
                        tracing::info!("[pipe] Send successful to chat={}", target.chat_id.0);
                    }
                }
            }
            MonitorEvent::SessionMapChanged => {
                tracing::info!("[pipe] SessionMapChanged — syncing session IDs to window states");
                let session_map = self.state_mgr.load_session_map().await?;
                let mut state = self.state_mgr.load_state().await?;
                let mut synced = 0;
                for (window_id, session_id) in &session_map {
                    if let Some(ws) = state.window_states.get_mut(window_id) {
                        if ws.session_id.is_empty() {
                            ws.session_id = session_id.clone();
                            synced += 1;
                            tracing::info!("[pipe] Assigned session {session_id} to window {window_id}");
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
    ) -> Result<()> {
        // Load current state to find thread binding
        let state = self.state_mgr.load_state().await?;

        // Find binding for this user+thread
        if let Some(binding) = state.thread_bindings.iter().find(|b| {
            b.user_id == user_id
                && b.thread_id == target.thread_id.map(|t| t.0).unwrap_or(0)
        }) {
            // Forward to existing window
            let window_id = aim_core::message::WindowId(binding.window_id.clone());

            // Verify window is still alive
            if !self.tmux_mgr.window_exists(&window_id).await {
                // Window died — clear binding
                let mut new_state = state;
                new_state.thread_bindings.retain(|b| b.window_id != window_id.0);
                self.state_mgr.save_state(&new_state).await?;

                // Notify user
                let _ = self.tmux_mgr; // TODO: send notification message
                return Ok(());
            }

            // Send text to the agent via tmux
            self.tmux_mgr.send_line(&window_id, text).await?;
        } else {
            // No binding — list unbound windows with inline keyboard
            let windows = self.tmux_mgr.window_map().await?;
            let bound_ids: HashSet<String> = state.thread_bindings.iter()
                .map(|b| b.window_id.clone())
                .collect();

            let unbound: Vec<&String> = windows.keys()
                .filter(|id| !bound_ids.contains(*id))
                .collect();

            if !unbound.is_empty() {
                // Store pending message for callback handling
                let mut pending = self.pending_messages.lock().await;
                pending.insert((user_id, target.chat_id.0, target.thread_id.map(|t| t.0).unwrap_or(0)), text.to_string());
                drop(pending);

                // Build inline keyboard with unbound windows + create new option
                let mut buttons: Vec<Vec<Button>> = unbound.iter().filter_map(|wid| {
                    windows.get(*wid).map(|info| {
                        vec![Button {
                            text: format!("{} — {}", info.name, info.current_command),
                            callback_data: format!("bind_window:{wid}"),
                        }]
                    })
                }).collect();
                buttons.push(vec![Button {
                    text: "＋ New session".into(),
                    callback_data: "create_new".into(),
                }]);

                let _ = self.im_adapter.send_keyboard(&target, "Select a session to bind:", &buttons).await;
            } else {
                // No unbound windows — create a new one
                let topic_name = self.topic_names.lock().await.remove(
                    &(target.chat_id.0, target.thread_id.map(|t| t.0).unwrap_or(0))
                );
                self.create_and_bind(&target, user_id, text, topic_name.as_deref()).await?;
            }
        }

        Ok(())
    }

    /// Bind an existing tmux window to a user/thread and send the initial text.
    async fn bind_window(
        &self,
        target: &MessageTarget,
        user_id: i64,
        text: &str,
        window_id: &str,
        topic_name: Option<&str>,
    ) -> Result<()> {
        let wid = WindowId(window_id.to_string());
        let window_name = match topic_name {
            Some(name) => name.to_string(),
            None => format!("aim-{user_id}"),
        };

        // Rename to reflect the new binding
        let _ = self.tmux_mgr.rename_window(&wid, &window_name).await;

        // Send text to the agent
        self.tmux_mgr.send_line(&wid, text).await?;

        // Persist the binding
        let mut state = self.state_mgr.load_state().await?;
        // Ensure WindowState entry exists (needed for session_id tracking)
        let cwd = self.tmux_mgr.pane_cwd(&wid).await.unwrap_or_default();
        state.window_states.entry(window_id.to_string()).or_insert(WindowState {
            session_id: String::new(),
            cwd,
            window_name: window_name.clone(),
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
        initial_text: &str,
        topic_name: Option<&str>,
    ) -> Result<()> {
        let window_name = match topic_name {
            Some(name) => name.to_string(),
            None => format!("aim-{user_id}"),
        };
        let cwd = std::env::current_dir()
            .unwrap_or_else(|_| std::path::PathBuf::from("/"))
            .to_string_lossy()
            .to_string();

        let window_id = self
            .tmux_mgr
            .new_window(&window_name, &cwd)
            .await?;

        // Start the agent
        let agent_cmd = &self.config.agent_command;
        self.tmux_mgr.send_line(&window_id, agent_cmd).await?;

        // Wait briefly for agent to start, then send initial text
        tokio::time::sleep(std::time::Duration::from_millis(1500)).await;
        self.tmux_mgr.send_line(&window_id, initial_text).await?;

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
                cwd,
                window_name: window_name.to_string(),
            },
        );
        self.state_mgr.save_state(&state).await?;

        Ok(())
    }

    async fn handle_callback(
        &self,
        target: MessageTarget,
        user_id: i64,
        data: &str,
        msg_id: MessageId,
    ) -> Result<()> {
        tracing::debug!("Callback from user {user_id}: {data}");

        let key = (user_id, target.chat_id.0, target.thread_id.map(|t| t.0).unwrap_or(0));

        let pending = self.pending_messages.lock().await.remove(&key);
        let text = match pending {
            Some(t) => t,
            None => {
                tracing::warn!("No pending message for callback (key={key:?})");
                return Ok(());
            }
        };

        let topic_name = self.topic_names.lock().await.remove(
            &(target.chat_id.0, target.thread_id.map(|t| t.0).unwrap_or(0))
        );

        if data == "create_new" {
            self.create_and_bind(&target, user_id, &text, topic_name.as_deref()).await?;
            let _ = self.im_adapter.edit_message(&target, &msg_id, "New session created.").await;
        } else if let Some(window_id) = data.strip_prefix("bind_window:") {
            let wid = WindowId(window_id.to_string());
            if !self.tmux_mgr.window_exists(&wid).await {
                let _ = self.im_adapter.edit_message(&target, &msg_id, "Session no longer available.").await;
                return Ok(());
            }
            self.bind_window(&target, user_id, &text, window_id, topic_name.as_deref()).await?;
            let _ = self.im_adapter.edit_message(&target, &msg_id, "Session bound.").await;
        } else {
            tracing::warn!("Unknown callback data: {data}");
        }

        Ok(())
    }

    async fn handle_topic_closed(&self, target: &MessageTarget) -> Result<()> {
        let thread_id = target.thread_id.map(|t| t.0).unwrap_or(0);
        let chat_id = target.chat_id.0;

        // Clean up topic name
        self.topic_names.lock().await.remove(&(chat_id, thread_id));

        // Clean up pending messages for this chat+thread
        let mut pending = self.pending_messages.lock().await;
        pending.retain(|k, _| k.1 != chat_id || k.2 != thread_id);
        drop(pending);

        // Remove thread binding
        let mut state = self.state_mgr.load_state().await?;
        state.thread_bindings.retain(|b| b.thread_id != thread_id);
        self.state_mgr.save_state(&state).await?;
        Ok(())
    }

    async fn handle_topic_edited(&self, target: &MessageTarget, new_name: &str) -> Result<()> {
        // Update topic name in memory
        let mut names = self.topic_names.lock().await;
        names.insert((target.chat_id.0, target.thread_id.map(|t| t.0).unwrap_or(0)), new_name.to_string());
        drop(names);

        // Also update in persisted binding if one exists
        let thread_id = target.thread_id.map(|t| t.0).unwrap_or(0);
        let mut state = self.state_mgr.load_state().await?;
        if let Some(binding) = state.thread_bindings.iter_mut().find(|b| {
            b.chat_id == target.chat_id.0 && b.thread_id == thread_id
        }) {
            binding.topic_name = Some(new_name.to_string());
            self.state_mgr.save_state(&state).await?;
        }
        Ok(())
    }
}
