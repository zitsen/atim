use std::time::Duration;

use atim_core::error::Result;
use atim_core::message::{MessageId, MessageTarget, WindowId};
use atim_core::session::{ChatBinding, WindowBinding};
use atim_monitor::monitor::resolve_jsonl;

use super::{
    MAX_MSG_LEN, agent_launch_cmd, extract_kv_rows, extract_modal_text, is_shell_process,
    strip_ansi,
};

/// Pull `cwd` out of the head of a session JSONL.
///
/// The chunk is a fixed-size read, so its last line is usually truncated
/// mid-object — that line simply fails to parse and is skipped.
fn cwd_from_jsonl(chunk: &[u8]) -> Option<String> {
    let text = std::str::from_utf8(chunk).ok()?;
    for line in text.lines() {
        if let Ok(val) = serde_json::from_str::<serde_json::Value>(line)
            && let Some(cwd) = val.get("cwd").and_then(|v| v.as_str())
            && !cwd.is_empty()
        {
            return Some(cwd.to_string());
        }
    }
    None
}

/// Index of the window `query` names, best match first.
///
/// An exact match beats a partial one, and a partial works in either
/// direction: a window rendered as "3:Skills" still answers a query of
/// "Skills". An empty query names nothing, which matters because a partial
/// match would otherwise succeed against every candidate.
fn pick_window<'a>(query: &str, names: impl Iterator<Item = &'a str>) -> Option<usize> {
    if query.is_empty() {
        return None;
    }
    let names: Vec<&str> = names.collect();
    names.iter().position(|n| *n == query).or_else(|| {
        names
            .iter()
            .position(|n| n.contains(query) || query.contains(n))
    })
}

/// The command inside a `!<cmd>` message, if that is what this is.
///
/// A bare `!` is not a command — splitting it would leave the agent reading an
/// empty line as a message.
fn shell_command_payload(text: &str) -> Option<&str> {
    let rest = text.strip_prefix('!')?;
    (!rest.is_empty()).then(|| rest.trim_start())
}

/// Coerce a `/atim config set` boolean, or `None` for anything else.
fn parse_bool_setting(value: &str) -> Option<bool> {
    match value {
        "true" | "1" | "on" => Some(true),
        "false" | "0" | "off" => Some(false),
        _ => None,
    }
}

/// Everything from the last line mentioning `command` to the end of `content`.
///
/// The modal echoes the command it was invoked with, and the captured pane
/// still holds whatever scrolled above it. When the command is absent the
/// whole capture is returned rather than nothing, so the caller has something
/// to show instead of a blank reply.
fn after_last_mention(content: &str, command: &str) -> String {
    let lines: Vec<&str> = content.lines().collect();
    match lines.iter().rposition(|l| l.contains(command)) {
        Some(idx) => lines[idx..].join("\n"),
        None => content.to_string(),
    }
}

impl super::Server {
    /// Extract the `cwd` field from a session's JSONL file.
    ///
    /// Reads the first few lines looking for a message entry with a `cwd` field.
    /// Returns `None` if the file doesn't exist or has no `cwd`.
    pub(super) async fn extract_cwd_from_jsonl(session_id: &str) -> Option<String> {
        let path = resolve_jsonl(session_id).await?;
        let data = tokio::fs::read(&path).await.ok()?;
        // Only read the first 4 KB — cwd is in the first user message.
        cwd_from_jsonl(&data[..data.len().min(4096)])
    }

    /// Recover a session when the tmux window has died.
    ///
    /// Creates a new tmux window, (re-)launches the agent, optionally
    /// resumes the stored session, re-keys state bindings (both V1 and
    /// V2 runtime), then forwards the user's pending text.
    ///
    /// Updates the original card (`msg_id`) in-place instead of sending
    /// a separate status message.
    pub(super) async fn handle_recover_session(
        &self,
        target: &MessageTarget,
        user_id: i64,
        thread_id: i64,
        text: &str,
        msg_id: &MessageId,
    ) -> Result<()> {
        let _rt_guard = self.state_mgr.lock_runtime().await;
        let mut rt = self.state_mgr.load_runtime().await?;

        // Match by chat_id — the only stable Feishu session identifier.
        let chat_id = target.chat_id.0;
        tracing::info!(
            "[recover] Looking up binding for user={user_id} thread={thread_id} chat={chat_id}"
        );
        let cb = match rt
            .chat_bindings
            .iter()
            .find(|b| b.user_id == user_id && b.chat_id == chat_id)
        {
            Some(cb) => cb.clone(),
            None => {
                tracing::warn!(
                    "[recover] No chat binding for user={user_id} thread={thread_id} chat={chat_id}"
                );
                // Dump all bindings for this user to help diagnose
                for b in rt.chat_bindings.iter().filter(|b| b.user_id == user_id) {
                    tracing::warn!(
                        "[recover]   candidate: thread={} chat={} display={}",
                        b.thread_id,
                        b.chat_id,
                        b.display_name
                    );
                }
                let _ = self
                    .im_adapter
                    .send_message(target, "Session not found.")
                    .await;
                return Ok(());
            }
        };

        tracing::info!(
            "[recover] Found binding: display={} thread={} chat={} session={}",
            cb.display_name,
            cb.thread_id,
            cb.chat_id,
            cb.session_id
        );

        // Re-resolve thread_id from the found binding (card actions use
        // chat-derived thread_id which may differ from the binding's thread_id).
        let thread_id = cb.thread_id;

        // Resolve session info from window_binding or session store
        let old_window_id: String;
        let mut cwd: String;
        let session_id: String;
        let agent_type_name: String;

        if let Some(wb) = rt.resolve_window_binding(user_id, thread_id) {
            old_window_id = wb.window_id.clone();
            cwd = wb.cwd.clone();
            session_id = wb.session_id.clone();
            agent_type_name = wb.agent_type.clone();

            // If cwd is HOME or empty and the window still exists, try tmux's pane_current_path.
            let home = home::home_dir()
                .map(|p| p.to_string_lossy().into_owned())
                .unwrap_or_default();
            let workdir = self.config.workdir.clone();
            if cwd == home || cwd == workdir || cwd.is_empty() {
                let wid = WindowId(old_window_id.clone());
                if let Ok(path) = self.tmux_mgr.pane_cwd(&wid).await {
                    tracing::info!(
                        "[recover] Overriding stale cwd {:?} with tmux pane_cwd {:?}",
                        cwd,
                        path,
                    );
                    cwd = path;
                }
            }
        } else {
            // Window is dead — reconstruct from chat_binding + sessions
            old_window_id = String::new();
            session_id = cb.session_id.clone();
            let si = if !session_id.is_empty() {
                rt.sessions.get(&session_id)
            } else {
                None
            };
            if let Some(si) = si {
                cwd = si.cwd.clone();
                agent_type_name = si.agent_type.clone();
            } else {
                // Last resort: try any window_binding with matching session_id
                let wb_cwd = rt
                    .window_bindings
                    .values()
                    .find(|wb| wb.session_id == session_id)
                    .map(|wb| wb.cwd.as_str())
                    .filter(|c| *c != "~" && !c.is_empty());
                cwd = wb_cwd.unwrap_or("~").to_string();
                agent_type_name = "claude".to_string();
            }
        }

        // If cwd is stale (HOME, workdir, or empty), try to extract from the session's JSONL file.
        let home = home::home_dir()
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_default();
        let workdir = self.config.workdir.clone();
        if !session_id.is_empty()
            && (cwd == "~" || cwd.is_empty() || cwd == home || cwd == workdir)
            && let Some(jsonl_cwd) = Self::extract_cwd_from_jsonl(&session_id).await
        {
            tracing::info!(
                "[recover] Overriding stale cwd {:?} with JSONL cwd {:?}",
                cwd,
                jsonl_cwd,
            );
            cwd = jsonl_cwd;
        }

        // Normalize ~ to actual path (use workdir if configured, otherwise HOME)
        if cwd == "~" || cwd.is_empty() {
            cwd = if !self.config.workdir.is_empty() {
                self.config.workdir.clone()
            } else {
                home::home_dir()
                    .map(|p| p.to_string_lossy().into_owned())
                    .unwrap_or_default()
            };
            if !session_id.is_empty() {
                let _ = self
                    .im_adapter
                    .edit_message(
                        target,
                        msg_id,
                        &format!(
                            "⚠️ Could not resolve project directory for session.\n\
                             Starting in {cwd} — agent context may be wrong."
                        ),
                    )
                    .await;
            }
        }

        let agent = self
            .config
            .agent_registry
            .get(&agent_type_name)
            .cloned()
            .unwrap_or_else(|| self.config.agent_registry.default().clone());

        let _ = self
            .im_adapter
            .edit_message(target, msg_id, "🔄 Recovering session...")
            .await;

        // Create new tmux window
        let window_name = &cb.display_name;
        let new_window_id = self.tmux_mgr.new_window(window_name, &cwd).await?;

        // Launch agent (resume if we have a session_id)
        if !session_id.is_empty() && agent.supports_sessions() {
            if let Some(resume_cmd) = agent.resume_command(&session_id) {
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
        let mut agent_started = false;
        for _ in 0..10 {
            if let Ok(info) = self.tmux_mgr.find_window(&new_window_id).await
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
                .capture_pane(&new_window_id)
                .await
                .unwrap_or_default();
            let clean = atim_parser::terminal::TerminalParser::strip_ansi(&pane);
            let err_msg = if clean.trim().is_empty() {
                "❌ Agent failed to start. Check agent command and configuration.".into()
            } else {
                format!("❌ Agent failed to start:\n```\n{}```", clean.trim())
            };
            let _ = self.im_adapter.edit_message(target, msg_id, &err_msg).await;
            return Ok(());
        }

        // Give the agent TUI time to initialize its input handler.
        // The loop above only confirms the shell process exited; the agent
        // still needs to render its UI and become ready for keystrokes.
        let startup_wait = if agent.supports_sessions() {
            2000
        } else {
            1500
        };
        tokio::time::sleep(Duration::from_millis(startup_wait)).await;

        // Recovering into a directory Claude has not seen opens the workspace
        // trust dialog, and the pending message below would answer it: its
        // Enter confirms the default, "No, exit", killing the recovered agent.
        if !self.confirm_agent_launch(&new_window_id).await {
            let pane = self
                .tmux_mgr
                .capture_pane(&new_window_id)
                .await
                .unwrap_or_default();
            let clean = atim_parser::terminal::TerminalParser::strip_ansi(&pane);
            let err_msg = format!(
                "❌ Agent exited after trust dialog:\n```\n{}```",
                clean.trim()
            );
            let _ = self.im_adapter.edit_message(target, msg_id, &err_msg).await;
            return Ok(());
        }

        // ── Update V2 runtime: re-key window_binding and chat_binding ──
        // Single load-save cycle to avoid race conditions with concurrent state writers.
        if !old_window_id.is_empty() {
            rt.window_bindings.remove(&old_window_id);
        }

        let new_session_id: String = if !session_id.is_empty() && agent.supports_sessions() {
            session_id.clone()
        } else {
            String::new()
        };

        rt.window_bindings.insert(
            new_window_id.0.clone(),
            WindowBinding {
                window_id: new_window_id.0.clone(),
                session_id: new_session_id.clone(),
                cwd: cwd.clone(),
                agent_type: agent.name().to_string(),
                window_name: window_name.clone(),
            },
        );

        if !new_session_id.is_empty()
            && let Some(cb) = rt
                .chat_bindings
                .iter_mut()
                .find(|cb| cb.user_id == user_id && cb.thread_id == thread_id)
        {
            cb.session_id = new_session_id.clone();
        }

        // Clean up stale session_map entries for the old window_id
        if !old_window_id.is_empty()
            && let Ok(mut map) = self.state_mgr.load_session_map().await
            && map.remove(&old_window_id).is_some()
            && let Err(e) = self.state_mgr.save_session_map(&map).await
        {
            tracing::warn!("[recover] Failed to save session_map (old): {e}");
        }

        // For resumed sessions, update session_map + clear monitor offset
        if !new_session_id.is_empty() {
            let mut map = self.state_mgr.load_session_map().await.unwrap_or_default();
            map.insert(new_window_id.0.clone(), new_session_id.clone());
            if let Err(e) = self.state_mgr.save_session_map(&map).await {
                tracing::warn!("[recover] Failed to save session_map (new): {e}");
            }
            if let Err(e) = self.state_mgr.remove_offset(&session_id).await {
                tracing::warn!("[recover] Failed to remove offset: {e}");
            }
            self.byte_offsets.lock().await.remove(&session_id);
        }

        self.state_mgr.save_runtime_locked(_rt_guard, &rt).await?;

        // For fresh sessions, resolve session_id FIRST (like create_and_bind_in_dir).
        // This both establishes routing AND gives the agent time to initialize
        // its TUI input handler before we forward the pending message.
        if new_session_id.is_empty() && agent.supports_sessions() {
            let wid = new_window_id.0.clone();
            if let Some(sid) = self.resolve_session_id(&wid, Duration::from_secs(15)).await {
                self.state_mgr
                    .upsert_window_binding(&WindowBinding {
                        window_id: wid.clone(),
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
                        chat_id: cb.chat_id,
                        display_name: cb.display_name.clone(),
                        group_chat_id: cb.group_chat_id,
                        topic_name: cb.topic_name.clone(),
                        session_id: sid.clone(),
                        reply_at_only: false,
                    })
                    .await?;
                let mut map = self.state_mgr.load_session_map().await.unwrap_or_default();
                map.insert(wid, sid);
                if let Err(e) = self.state_mgr.save_session_map(&map).await {
                    tracing::warn!("[recover/fresh] Failed to save session_map: {e}");
                }
            }
        }

        // Forward the user's original text to the agent.
        // resolve_session_id above gives the agent enough time to initialize.
        tracing::info!(
            "[recover] Forwarding pending message (len={}) to window {}",
            text.len(),
            new_window_id.0,
        );
        let is_copilot = agent.name() == "copilot";
        if is_copilot {
            self.tmux_mgr
                .send_line_chars(&new_window_id, text, 10)
                .await?;
        } else {
            self.tmux_mgr.send_line(&new_window_id, text).await?;
        }

        let sc_chat_id = cb.group_chat_id.unwrap_or(cb.chat_id);
        self.status_consumed
            .lock()
            .await
            .remove(&(sc_chat_id, cb.thread_id));

        // Update the original card in-place
        let _ = self
            .im_adapter
            .edit_message(target, msg_id, "✅ Session recovered.")
            .await;

        Ok(())
    }

    /// Rename a bound window and its tmux window to a new name.
    pub(super) async fn handle_rename_window(
        &self,
        new_name: &str,
        user_id: i64,
        thread_id: i64,
    ) -> Result<()> {
        if new_name.is_empty() {
            return Ok(());
        }

        let _rt_guard = self.state_mgr.lock_runtime().await;
        let mut rt = self.state_mgr.load_runtime().await?;
        if let Some(cb) = rt
            .chat_bindings
            .iter_mut()
            .find(|b| b.user_id == user_id && b.thread_id == thread_id)
        {
            cb.display_name = new_name.to_string();
            if let Some(topic) = &mut cb.topic_name {
                *topic = new_name.to_string();
            }
            if !cb.session_id.is_empty()
                && let Some(wb) = rt
                    .window_bindings
                    .values_mut()
                    .find(|wb| wb.session_id == cb.session_id)
            {
                wb.window_name = new_name.to_string();
                let wid = WindowId(wb.window_id.clone());
                let _ = self.tmux_mgr.rename_window(&wid, new_name).await;
            }
            self.state_mgr.save_runtime_locked(_rt_guard, &rt).await?;
        }
        Ok(())
    }

    /// Update the binding's agent_type to match the actual running process.
    pub(super) async fn handle_rebind_agent(
        &self,
        _target: &MessageTarget,
        user_id: i64,
        thread_id: i64,
        agent_name: &str,
    ) -> Result<()> {
        let _rt_guard = self.state_mgr.lock_runtime().await;
        let mut rt = self.state_mgr.load_runtime().await?;
        if let Some(cb) = rt
            .chat_bindings
            .iter()
            .find(|b| b.user_id == user_id && b.thread_id == thread_id)
            && !cb.session_id.is_empty()
            && let Some(wb) = rt
                .window_bindings
                .values_mut()
                .find(|wb| wb.session_id == cb.session_id)
        {
            wb.agent_type = agent_name.to_string();
            self.state_mgr.save_runtime_locked(_rt_guard, &rt).await?;
        }
        Ok(())
    }

    /// Find a live tmux window by matching the display/name field.
    ///
    /// First searches the atim session (exact then contains match).
    /// Falls back to all sessions — if a matching window is found in a
    /// different session, it is moved into the atim session so all
    /// subsequent operations (send-keys, capture-pane, etc.) work through
    /// the normal session-prefixed path.
    pub(super) async fn find_window_by_name(&self, name: &str) -> Option<String> {
        let windows = self.tmux_mgr.list_windows().await.ok()?;
        if let Some(i) = pick_window(name, windows.iter().map(|w| w.name.as_str())) {
            return Some(windows[i].window_id.0.clone());
        }

        // Fallback: the window may be in a user's session rather than the atim
        // one — common after a restart. Pull it in so later lookups hit the
        // first branch directly.
        let all_windows = self.tmux_mgr.list_all_windows().await.ok()?;
        let i = pick_window(name, all_windows.iter().map(|(w, _)| w.name.as_str()))?;
        let (w, session) = &all_windows[i];
        if session != self.tmux_mgr.session_name() {
            self.tmux_mgr
                .move_window_into_session(session, &w.window_id)
                .await
                .ok()?;
        }
        Some(w.window_id.0.clone())
    }

    /// Send text to the agent, optimizing `!` shell commands by sending
    /// `!` first and the command separately to avoid the agent treating
    /// the entire input as a conversational message.
    pub(super) async fn send_text_to_agent(
        &self,
        window_id: &WindowId,
        text: &str,
        is_copilot: bool,
    ) -> Result<()> {
        if let Some(cmd) = shell_command_payload(text) {
            if is_copilot {
                self.tmux_mgr.send_line_chars(window_id, "!", 10).await?;
                tokio::time::sleep(Duration::from_millis(200)).await;
                self.tmux_mgr.send_line_chars(window_id, cmd, 10).await?;
            } else {
                self.tmux_mgr.send_line(window_id, "!").await?;
                tokio::time::sleep(Duration::from_millis(200)).await;
                self.tmux_mgr.send_line(window_id, cmd).await?;
            }
        } else {
            if is_copilot {
                self.tmux_mgr.send_line_chars(window_id, text, 10).await?;
            } else {
                self.tmux_mgr.send_line(window_id, text).await?;
            }
        }
        Ok(())
    }

    /// Send a slash command to the agent, capture the modal content,
    /// return it to the chat, then dismiss the modal.
    pub(super) async fn send_slash_and_capture(
        &self,
        target: &MessageTarget,
        window_id: &WindowId,
        command: &str,
    ) {
        // 1. Flush stale pane content
        let _ = self.tmux_mgr.capture_pane(window_id).await;
        // 2. Send the slash command — opens a modal
        self.tmux_mgr.send_line(window_id, command).await.ok();

        // 3. Wait for modal to fully render
        //    /usage needs more time since it fetches API data
        let wait_ms = if command == "/usage" { 10_000 } else { 5_000 };
        tokio::time::sleep(Duration::from_millis(wait_ms)).await;

        // 4. Capture pane content
        let raw = self
            .tmux_mgr
            .capture_pane(window_id)
            .await
            .ok()
            .unwrap_or_default();
        let content = strip_ansi(&raw);

        tracing::debug!(
            "send_slash_and_capture({command}): captured {} chars after {wait_ms}ms",
            content.len()
        );

        // 5. Dismiss the modal
        self.tmux_mgr.send_key(window_id, "Escape").await.ok();

        // 6. The modal echoes the command it was invoked with and the capture
        //    still holds whatever scrolled above it, so anchor on the last
        //    mention of the command.
        let content = after_last_mention(&content, command);
        let trimmed = content.trim();

        if trimmed.is_empty() {
            tracing::warn!("send_slash_and_capture({command}): captured empty content");
            let _ = self
                .im_adapter
                .send_message(target, &format!("`{command}` returned no output."))
                .await;
            return;
        }

        match command {
            "/status" => {
                let rows = extract_kv_rows(trimmed, &["Auth token", "Setting sources", "API key"]);
                if rows.is_empty() {
                    let _ = self
                        .im_adapter
                        .send_message(target, &format!("`{command}` returned no output."))
                        .await;
                } else {
                    let _ = self.im_adapter.send_kv_table(target, "Status", &rows).await;
                }
            }
            "/usage" => {
                let rows = extract_kv_rows(trimmed, &[]);
                if rows.is_empty() {
                    // Fallback: send the raw captured text so user can see what happened
                    let extracted = extract_modal_text(trimmed);
                    if extracted.is_empty() {
                        tracing::warn!(
                            "send_slash_and_capture(/usage): empty rows, raw content: {trimmed:?}"
                        );
                        let _ = self
                            .im_adapter
                            .send_message(target, "`/usage` returned no output.")
                            .await;
                    } else {
                        let _ = self
                            .im_adapter
                            .send_message(target, &format!("`/usage`:\n```\n{extracted}\n```"))
                            .await;
                    }
                } else {
                    let _ = self.im_adapter.send_kv_table(target, "Usage", &rows).await;
                }
            }
            _ => {
                let extracted = extract_modal_text(trimmed);
                if extracted.is_empty() {
                    let _ = self
                        .im_adapter
                        .send_message(target, &format!("`{command}` returned no output."))
                        .await;
                } else if extracted.len() > MAX_MSG_LEN {
                    let truncated: String = extracted.chars().take(MAX_MSG_LEN).collect();
                    let _ = self
                        .im_adapter
                        .send_message(
                            target,
                            &format!("`{command}` _(truncated):_\n```\n{}…\n```", truncated),
                        )
                        .await;
                } else {
                    let _ = self
                        .im_adapter
                        .send_message(target, &format!("`{command}`:\n```\n{}\n```", extracted))
                        .await;
                }
            }
        }
    }

    /// Handle `/atim` subcommands.
    pub(super) async fn handle_atim_command(
        &self,
        target: &MessageTarget,
        user_id: i64,
        cmd: &str,
    ) -> Result<()> {
        match cmd {
            // ── Session commands ──
            "status" | "st" => {
                let _ = self
                    .im_adapter
                    .send_message(target, "Gathering system info...")
                    .await;

                let mut lines = vec!["**atim Status**".to_string()];

                // CPU cores + load
                match tokio::process::Command::new("sh")
                    .args(["-c", "nproc && cat /proc/loadavg | cut -d' ' -f1-3"])
                    .output()
                    .await
                {
                    Ok(out) => {
                        let out = String::from_utf8_lossy(&out.stdout);
                        let parts: Vec<&str> = out.trim().splitn(2, '\n').collect();
                        let cores = parts.first().unwrap_or(&"?").trim();
                        let load = parts.get(1).unwrap_or(&"?").trim();
                        lines.push(format!("**CPU**: {} cores (load: {})", cores, load));
                    }
                    Err(_) => lines.push("**CPU**: N/A".into()),
                }

                // Memory
                match tokio::process::Command::new("free")
                    .args(["-h"])
                    .output()
                    .await
                {
                    Ok(out) => {
                        let out = String::from_utf8_lossy(&out.stdout);
                        for line in out.lines().skip(1).take(1) {
                            let fields: Vec<&str> = line.split_whitespace().collect();
                            if fields.len() >= 3 {
                                lines.push(format!(
                                    "**Memory**: total={} used={} free={}",
                                    fields[1], fields[2], fields[3]
                                ));
                            }
                        }
                    }
                    Err(_) => lines.push("**Memory**: N/A".into()),
                }

                // Disk
                match tokio::process::Command::new("df")
                    .args(["-h", "/"])
                    .output()
                    .await
                {
                    Ok(out) => {
                        let out = String::from_utf8_lossy(&out.stdout);
                        for line in out.lines().skip(1).take(1) {
                            let fields: Vec<&str> = line.split_whitespace().collect();
                            if fields.len() >= 4 {
                                lines.push(format!(
                                    "**Disk** (/): size={} used={} avail={}",
                                    fields[1], fields[2], fields[3]
                                ));
                            }
                        }
                    }
                    Err(_) => lines.push("**Disk**: N/A".into()),
                }

                // Uptime
                if let Ok(out) = tokio::process::Command::new("uptime")
                    .args(["-p"])
                    .output()
                    .await
                {
                    let uptime = String::from_utf8_lossy(&out.stdout).trim().to_string();
                    lines.push(format!("**Uptime**: {}", uptime));
                }

                let _ = self
                    .im_adapter
                    .send_message(target, &lines.join("\n"))
                    .await;
            }
            "ls" => {
                let rt = self.state_mgr.load_runtime().await?;
                let mut lines = vec!["| Name | Window | CWD | Agent | Session ID |".to_string()];
                lines.push("|------|--------|-----|-------|------------|".to_string());
                // Sessions with a tmux window
                for (wb_id, wb) in &rt.window_bindings {
                    let name = if wb.window_name.is_empty() {
                        "-"
                    } else {
                        &wb.window_name
                    };
                    let agent = if wb.agent_type.is_empty() {
                        "-"
                    } else {
                        &wb.agent_type
                    };
                    let sid = if wb.session_id.is_empty() {
                        "-"
                    } else {
                        &wb.session_id
                    };
                    let sid_short = if sid.len() > 8 { &sid[..8] } else { sid };
                    lines.push(format!(
                        "| {} | {} | {} | {} | {} |",
                        name, wb_id, wb.cwd, agent, sid_short
                    ));
                }
                // Sessions with a chat binding but no window (dead/lost)
                let window_sids: std::collections::HashSet<&str> = rt
                    .window_bindings
                    .values()
                    .map(|wb| wb.session_id.as_str())
                    .collect();
                for cb in &rt.chat_bindings {
                    if cb.session_id.is_empty() || window_sids.contains(cb.session_id.as_str()) {
                        continue;
                    }
                    let name = if cb.display_name.is_empty() {
                        "-"
                    } else {
                        &cb.display_name
                    };
                    let sid = &cb.session_id;
                    let sid_short = if sid.len() > 8 {
                        &sid[..8]
                    } else {
                        sid.as_str()
                    };
                    let cwd = rt.sessions.get(sid).map(|s| s.cwd.as_str()).unwrap_or("-");
                    let agent = rt
                        .sessions
                        .get(sid)
                        .map(|s| s.agent_type.as_str())
                        .unwrap_or("-");
                    lines.push(format!(
                        "| {} | - | {} | {} | {} |",
                        name, cwd, agent, sid_short
                    ));
                }
                if lines.len() <= 2 {
                    lines.push("_No sessions_".to_string());
                }
                let _ = self
                    .im_adapter
                    .send_message(target, &lines.join("\n"))
                    .await;
            }
            cmd if cmd.starts_with("chdir ") => {
                let args = cmd
                    .strip_prefix("chdir")
                    .expect("guarded by starts_with above")
                    .trim();
                let mut parts = args.splitn(2, ' ');
                let name = parts.next().unwrap_or("").trim();
                let dir = parts.next().unwrap_or("").trim();
                if name.is_empty() || dir.is_empty() {
                    let _ = self
                        .im_adapter
                        .send_message(target, "Usage: `/atim chdir <name> <dir>`")
                        .await;
                } else {
                    let _rt_guard = self.state_mgr.lock_runtime().await;
                    let mut rt = self.state_mgr.load_runtime().await?;
                    let found = rt
                        .window_bindings
                        .values_mut()
                        .find(|wb| wb.window_name == name);
                    match found {
                        Some(wb) => {
                            let old = wb.cwd.clone();
                            wb.cwd = dir.to_string();
                            self.state_mgr.save_runtime_locked(_rt_guard, &rt).await?;
                            let _ = self
                                .im_adapter
                                .send_message(
                                    target,
                                    &format!("Updated `{}` cwd: `{}` → `{}`", name, old, dir),
                                )
                                .await;
                        }
                        None => {
                            let _ = self
                                .im_adapter
                                .send_message(target, &format!("No session with name `{}`", name))
                                .await;
                        }
                    }
                }
            }
            cmd if cmd.starts_with("rm ") => {
                let name = cmd
                    .strip_prefix("rm")
                    .expect("guarded by starts_with above")
                    .trim();
                if name.is_empty() {
                    let _ = self
                        .im_adapter
                        .send_message(target, "Usage: `/atim rm <name>`")
                        .await;
                } else {
                    let _rt_guard = self.state_mgr.lock_runtime().await;
                    let mut rt = self.state_mgr.load_runtime().await?;
                    // Find window binding by name
                    let wid = rt
                        .window_bindings
                        .values()
                        .find(|wb| wb.window_name == name)
                        .map(|wb| wb.window_id.clone());
                    match wid {
                        Some(wid) => {
                            // Remove from window_bindings
                            if let Some(wb) = rt.window_bindings.remove(&wid) {
                                // Remove associated chat bindings
                                rt.chat_bindings.retain(|cb| cb.session_id != wb.session_id);
                            }
                            // Remove from session_map
                            let mut map =
                                self.state_mgr.load_session_map().await.unwrap_or_default();
                            map.remove(&wid);
                            if let Err(e) = self.state_mgr.save_session_map(&map).await {
                                tracing::warn!("[rm] Failed to save session_map: {e}");
                            }
                            self.state_mgr.save_runtime_locked(_rt_guard, &rt).await?;
                            // Close tmux window if it still exists
                            let window_id = atim_core::message::WindowId(wid.clone());
                            if self.tmux_mgr.window_exists(&window_id).await {
                                let _ = self.tmux_mgr.kill_window(&window_id).await;
                            }
                            let _ = self
                                .im_adapter
                                .send_message(
                                    target,
                                    &format!("🗑 Removed session `{}` (window {})", name, wid),
                                )
                                .await;
                        }
                        None => {
                            let _ = self
                                .im_adapter
                                .send_message(target, &format!("No session with name `{}`", name))
                                .await;
                        }
                    }
                }
            }
            // ── Shell command (run outside session window) ──
            cmd if cmd.starts_with("cmd ") => {
                let shell_cmd = cmd
                    .strip_prefix("cmd ")
                    .expect("guarded by starts_with")
                    .trim();
                if shell_cmd.is_empty() {
                    let _ = self
                        .im_adapter
                        .send_message(target, "Usage: `/atim cmd <shell command>`")
                        .await;
                    return Ok(());
                }
                let _ = self.im_adapter.send_chat_action(target).await;
                match tokio::process::Command::new("sh")
                    .args(["-c", shell_cmd])
                    .output()
                    .await
                {
                    Ok(out) => {
                        let stdout = String::from_utf8_lossy(&out.stdout);
                        let stderr = String::from_utf8_lossy(&out.stderr);
                        let mut output = String::new();
                        if !stdout.is_empty() {
                            output.push_str(&stdout);
                        }
                        if !stderr.is_empty() {
                            if !output.is_empty() {
                                output.push_str("\n--- stderr ---\n");
                            }
                            output.push_str(&stderr);
                        }
                        if !out.status.success() {
                            output.push_str(&format!(
                                "\n(exit code: {})",
                                out.status.code().unwrap_or(-1)
                            ));
                        }
                        if output.is_empty() {
                            output = "(no output)".into();
                        }
                        if output.len() > super::MAX_MSG_LEN {
                            output.truncate(super::MAX_MSG_LEN - 20);
                            output.push_str("\n...(truncated)");
                        }
                        let _ = self
                            .im_adapter
                            .send_message(target, &format!("```\n{output}\n```"))
                            .await;
                    }
                    Err(e) => {
                        let _ = self
                            .im_adapter
                            .send_message(target, &format!("Failed to run command: {e}"))
                            .await;
                    }
                }
            }
            // ── Config commands (also accessible via /atim config) ──
            "show" | "sh" => {
                self.handle_config_show(target, user_id).await;
            }
            cmd if cmd.starts_with("set ") || cmd.starts_with("s ") => {
                let args = if cmd.starts_with("set ") {
                    cmd.strip_prefix("set ")
                } else {
                    cmd.strip_prefix("s ")
                }
                .expect("guarded by starts_with")
                .trim();
                self.handle_config_set(target, user_id, args).await;
            }
            // /atim config [show|set ...] — canonical config subcommand
            cmd if cmd == "config" || cmd == "c" => {
                self.handle_config_show(target, user_id).await;
            }
            cmd if cmd.starts_with("config ") || cmd.starts_with("c ") => {
                let rest = if cmd.starts_with("config ") {
                    cmd.strip_prefix("config ")
                } else {
                    cmd.strip_prefix("c ")
                }
                .expect("guarded by starts_with")
                .trim();
                match rest {
                    "show" | "sh" | "" => {
                        self.handle_config_show(target, user_id).await;
                    }
                    _ if rest.starts_with("set ") || rest.starts_with("s ") => {
                        let args = if rest.starts_with("set ") {
                            rest.strip_prefix("set ")
                        } else {
                            rest.strip_prefix("s ")
                        }
                        .expect("guarded by starts_with")
                        .trim();
                        self.handle_config_set(target, user_id, args).await;
                    }
                    _ => {
                        let _ = self
                            .im_adapter
                            .send_message(target, "Usage: `/atim config [show|set <key> <value>]`")
                            .await;
                    }
                }
            }
            "help" | "h" | "-h" | "--help" => {
                let help = concat!(
                    "**atim Commands**\n\n",
                    "**Session**\n",
                    "`/atim status` (`st`) — System CPU/mem/disk\n",
                    "`/atim ls` — List sessions\n",
                    "`/atim chdir <name> <dir>` — Update session cwd\n",
                    "`/atim rm <name>` — Remove session\n",
                    "`/atim cmd <shell>` — Run shell command\n\n",
                    "**Config**\n",
                    "`/atim config` (`c`) — Show chat config\n",
                    "`/atim config set <key> <value>` — Set config\n\n",
                    "**Session control**\n",
                    "`/switch <agent>` — Switch running agent\n",
                    "`/rebind` — Re-detect agent & session\n",
                    "`/reload` — Restart agent in same window\n",
                    "`/new` — New agent session\n",
                    "`/unbind` — Remove binding & close window\n",
                    "`/check` — Health check\n\n",
                    "**Claude Code**\n",
                    "`/status` `/doctor` `/help` `/compact` `/clear`\n\n",
                    "**Tools**\n",
                    "`/ss` `/screenshot` — Terminal screenshot\n",
                    "`/usage` — API usage\n",
                    "`/esc` `/dismiss` — Send Escape\n",
                    "`/enter` — Send Enter\n",
                    "`!cmd` — Run shell command\n",
                );
                let _ = self.im_adapter.send_message(target, help).await;
            }
            _ => {
                let _ = self
                    .im_adapter
                    .send_message(
                        target,
                        &format!("Unknown subcommand: `{}`. Try `/atim help`.", cmd),
                    )
                    .await;
            }
        }
        Ok(())
    }

    /// Show config for the current chat.
    async fn handle_config_show(&self, target: &MessageTarget, user_id: i64) {
        let thread_id = target.thread_id.map(|t| t.0).unwrap_or(0);

        // replyAtOnly from ChatBinding
        let reply_at_only = match self.state_mgr.load_runtime().await {
            Ok(rt) => rt
                .chat_bindings
                .iter()
                .find(|b| b.user_id == user_id && b.thread_id == thread_id)
                .map(|b| b.reply_at_only)
                .unwrap_or(false),
            Err(_) => false,
        };

        // defaultAgent from chat_settings
        let default_agent = self
            .state_mgr
            .load_chat_setting(user_id, thread_id, "default_agent")
            .await
            .unwrap_or(None)
            .unwrap_or_default();

        let agent_display = if default_agent.is_empty() {
            "(not set)".to_string()
        } else {
            format!("`{default_agent}`")
        };

        let msg = format!(
            "**Chat Config**\n\n`replyAtOnly`: {}\n`defaultAgent`: {}",
            if reply_at_only { "true" } else { "false" },
            agent_display,
        );
        let _ = self.im_adapter.send_message(target, &msg).await;
    }

    /// Set a config value for the current chat.
    async fn handle_config_set(&self, target: &MessageTarget, user_id: i64, args: &str) {
        let mut parts = args.splitn(2, ' ');
        let key = parts.next().unwrap_or("").trim();
        let value = parts.next().unwrap_or("").trim();
        match key {
            "replyAtOnly" => {
                let Some(val) = parse_bool_setting(value) else {
                    let _ = self
                        .im_adapter
                        .send_message(target, "Usage: `/atim config set replyAtOnly [true|false]`")
                        .await;
                    return;
                };
                let thread_id = target.thread_id.map(|t| t.0).unwrap_or(0);
                let _rt_guard = self.state_mgr.lock_runtime().await;
                let mut rt = match self.state_mgr.load_runtime().await {
                    Ok(rt) => rt,
                    Err(e) => {
                        let _ = self
                            .im_adapter
                            .send_message(target, &format!("Failed to load config: {e}"))
                            .await;
                        return;
                    }
                };
                if let Some(cb) = rt
                    .chat_bindings
                    .iter_mut()
                    .find(|b| b.user_id == user_id && b.thread_id == thread_id)
                {
                    cb.reply_at_only = val;
                    if let Err(e) = self.state_mgr.save_runtime_locked(_rt_guard, &rt).await {
                        let _ = self
                            .im_adapter
                            .send_message(target, &format!("Failed to save config: {e}"))
                            .await;
                        return;
                    }
                    let _ = self
                        .im_adapter
                        .send_message(
                            target,
                            &format!(
                                "`replyAtOnly` set to **{}**",
                                if val { "true" } else { "false" },
                            ),
                        )
                        .await;
                } else {
                    let _ = self
                        .im_adapter
                        .send_message(target, "No chat binding found. Start a session first.")
                        .await;
                }
            }
            "defaultAgent" => {
                let agent_name = value.trim().to_lowercase();
                if agent_name.is_empty() {
                    // Clear the default
                    let thread_id = target.thread_id.map(|t| t.0).unwrap_or(0);
                    let _ = self
                        .state_mgr
                        .save_chat_setting(user_id, thread_id, "default_agent", "")
                        .await;
                    let _ = self
                        .im_adapter
                        .send_message(target, "`defaultAgent` cleared.")
                        .await;
                } else if self.config.agent_registry.get(&agent_name).is_some() {
                    let thread_id = target.thread_id.map(|t| t.0).unwrap_or(0);
                    if let Err(e) = self
                        .state_mgr
                        .save_chat_setting(user_id, thread_id, "default_agent", &agent_name)
                        .await
                    {
                        let _ = self
                            .im_adapter
                            .send_message(target, &format!("Failed to save config: {e}"))
                            .await;
                        return;
                    }
                    let _ = self
                        .im_adapter
                        .send_message(target, &format!("`defaultAgent` set to **{agent_name}**"))
                        .await;
                } else {
                    let available: Vec<&str> = self
                        .config
                        .agent_registry
                        .iter()
                        .map(|a| a.name())
                        .collect();
                    let _ = self
                        .im_adapter
                        .send_message(
                            target,
                            &format!(
                                "Unknown agent '{agent_name}'. Available: {}",
                                available.join(", ")
                            ),
                        )
                        .await;
                }
            }
            "" => {
                let _ = self
                    .im_adapter
                    .send_message(
                        target,
                        "Usage: `/atim config set <key> <value>`\nAvailable keys: `replyAtOnly`, `defaultAgent`",
                    )
                    .await;
            }
            _ => {
                let _ = self
                    .im_adapter
                    .send_message(
                        target,
                        &format!(
                            "Unknown config key: `{}`. Available: `replyAtOnly`, `defaultAgent`",
                            key
                        ),
                    )
                    .await;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── cwd_from_jsonl ──

    #[test]
    fn test_cwd_is_read_from_the_first_line_that_has_one() {
        let chunk = br#"{"type":"summary","summary":"x"}
{"type":"user","cwd":"/home/me/project","message":{}}
{"type":"assistant","cwd":"/elsewhere"}
"#;
        assert_eq!(cwd_from_jsonl(chunk).as_deref(), Some("/home/me/project"));
    }

    #[test]
    fn test_a_truncated_tail_is_skipped_not_fatal() {
        // The caller reads a fixed-size chunk, so the last line is routinely
        // cut mid-object. It must not stop the scan.
        let chunk = br#"{"type":"user","cwd":"/wanted"}
{"type":"assistant","cwd":"/half"#;
        assert_eq!(cwd_from_jsonl(chunk).as_deref(), Some("/wanted"));
    }

    #[test]
    fn test_an_empty_cwd_does_not_count() {
        let chunk = br#"{"type":"user","cwd":""}
{"type":"user","cwd":"/real"}
"#;
        assert_eq!(cwd_from_jsonl(chunk).as_deref(), Some("/real"));
    }

    #[test]
    fn test_no_cwd_anywhere_is_none() {
        let chunk = br#"{"type":"summary","summary":"x"}
garbage that is not json
"#;
        assert_eq!(cwd_from_jsonl(chunk), None);
    }

    #[test]
    fn test_invalid_utf8_is_none_rather_than_a_panic() {
        assert_eq!(cwd_from_jsonl(&[0xff, 0xfe, 0x00]), None);
    }

    // ── pick_window ──

    #[test]
    fn test_an_exact_name_beats_an_earlier_partial_one() {
        // "3:Skills" comes first, but the query names the other window exactly.
        let names = ["3:Skills", "Skills"];
        assert_eq!(pick_window("Skills", names.iter().copied()), Some(1));
    }

    #[test]
    fn test_a_partial_match_works_in_either_direction() {
        // tmux renders a window as "<index>:<name>".
        let names = ["3:Skills"];
        assert_eq!(pick_window("Skills", names.iter().copied()), Some(0));
        assert_eq!(pick_window("3:Skills", names.iter().copied()), Some(0));
    }

    #[test]
    fn test_the_first_of_several_equal_matches_wins() {
        let names = ["atim", "atim"];
        assert_eq!(pick_window("atim", names.iter().copied()), Some(0));
    }

    #[test]
    fn test_an_empty_query_names_nothing() {
        // Every name contains "", so a partial match would otherwise win and
        // silently target whatever window happens to be first.
        let names = ["atim", "zsh"];
        assert_eq!(pick_window("", names.iter().copied()), None);
    }

    #[test]
    fn test_no_candidates_is_none() {
        assert_eq!(pick_window("Skills", std::iter::empty()), None);
    }

    // ── shell_command_payload ──

    #[test]
    fn test_a_bang_prefix_yields_the_command() {
        assert_eq!(shell_command_payload("!ls -la"), Some("ls -la"));
        assert_eq!(shell_command_payload("! ls"), Some("ls"));
        assert_eq!(shell_command_payload("!   ls"), Some("ls"));
    }

    #[test]
    fn test_a_bare_bang_is_not_a_command() {
        // Sending "!" alone would leave the agent reading an empty line.
        assert_eq!(shell_command_payload("!"), None);
    }

    #[test]
    fn test_a_double_bang_keeps_the_second() {
        assert_eq!(shell_command_payload("!!"), Some("!"));
    }

    #[test]
    fn test_ordinary_text_is_not_a_command() {
        assert_eq!(shell_command_payload("hello"), None);
        assert_eq!(shell_command_payload(""), None);
        assert_eq!(shell_command_payload("what does ! do?"), None);
    }

    // ── parse_bool_setting ──

    #[test]
    fn test_booleans_accept_the_spellings_the_help_lists() {
        for truthy in ["true", "1", "on"] {
            assert_eq!(parse_bool_setting(truthy), Some(true), "{truthy}");
        }
        for falsy in ["false", "0", "off"] {
            assert_eq!(parse_bool_setting(falsy), Some(false), "{falsy}");
        }
    }

    #[test]
    fn test_anything_else_is_rejected_so_the_caller_can_show_usage() {
        // Case-sensitive as written: the help says [true|false], and a typo
        // should get the usage line rather than silently setting something.
        for bad in ["", "yes", "no", "2", "TRUE", "True", "on "] {
            assert_eq!(parse_bool_setting(bad), None, "{bad:?}");
        }
    }

    // ── after_last_mention ──

    /// A `/status` capture: the pane still holds the shell line that launched
    /// the agent, then the modal echoing the command, then the payload.
    const STATUS_CAPTURE: &str = "\
user@host ~ % claude
/status
 Version: 2.1.284
 Session ID: 01729ab9-4b3e-49ac-a077-614fb93a0298
 Model: opus
";

    #[test]
    fn test_the_modal_payload_starts_at_the_echoed_command() {
        let out = after_last_mention(STATUS_CAPTURE, "/status");
        assert!(out.starts_with("/status\n"));
        assert!(out.contains("Session ID: 01729ab9"));
        // The shell line that scrolled above the modal is dropped.
        assert!(!out.contains("user@host"));
    }

    #[test]
    fn test_the_last_mention_wins_when_the_command_repeats() {
        // The command appears in the scrollback and again as the modal echo;
        // anchoring on the first would drag the scrollback along.
        let capture = "/status\nold output that was already shown\n/status\nVersion: 2.1.284\n";
        let out = after_last_mention(capture, "/status");
        assert!(out.starts_with("/status\nVersion"));
        assert!(!out.contains("old output"));
    }

    #[test]
    fn test_a_command_that_is_absent_returns_the_whole_capture() {
        // Better to show the raw pane than to reply with nothing.
        let out = after_last_mention(STATUS_CAPTURE, "/usage");
        assert_eq!(out, STATUS_CAPTURE);
    }

    #[test]
    fn test_a_command_with_no_line_of_its_own_still_anchors() {
        // The echo is matched as a substring, so a padded or decorated echo
        // still anchors.
        let capture = "noise\n  /status  \nVersion: 2.1.284\n";
        let out = after_last_mention(capture, "/status");
        assert_eq!(out, "  /status  \nVersion: 2.1.284");
    }
}
