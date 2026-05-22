use async_trait::async_trait;
use std::time::Duration;
use tokio::sync::mpsc;

use atim_core::error::{Error, Result};
use atim_core::im::ImAdapter;
use atim_core::message::{
    Button, ChatId, CheckItem, ImEvent, ImEventKind, MessageId, MessageTarget, UserId,
};

/// Convert Markdown text to Telegram-compatible HTML.
///
/// Uses pulldown_cmark for parsing and emits only the HTML subset that
/// Telegram supports: <b>, <i>, <code>, <pre>, <s>, <a href>, <blockquote>.
fn markdown_to_html(text: &str) -> String {
    use pulldown_cmark::{Event, Options, Parser, Tag, TagEnd};

    let mut options = Options::empty();
    options.insert(Options::ENABLE_STRIKETHROUGH);
    options.insert(Options::ENABLE_TABLES);

    let parser = Parser::new_ext(text, options);
    let mut out = String::new();
    // Stack to track whether we're inside a list item
    let mut in_list_item = false;

    for event in parser {
        match event {
            Event::Start(tag) => match tag {
                Tag::Paragraph => {}
                Tag::Heading { level: _, .. } => out.push_str("<b>"),
                Tag::BlockQuote(..) => out.push_str("<blockquote>"),
                Tag::CodeBlock(_) => out.push_str("<pre>"),
                Tag::List(_) => {}
                Tag::Item => {
                    in_list_item = true;
                    out.push_str("• ");
                }
                Tag::Emphasis => out.push_str("<i>"),
                Tag::Strong => out.push_str("<b>"),
                Tag::Strikethrough => out.push_str("<s>"),
                Tag::Link { dest_url, .. } => {
                    let escaped = html_escape(dest_url.as_ref());
                    out.push_str(&format!("<a href=\"{escaped}\">"));
                }
                _ => {}
            },
            Event::End(tag) => match tag {
                TagEnd::Paragraph if !in_list_item => {
                    out.push('\n');
                }
                TagEnd::Heading(_) => out.push_str("</b>\n"),
                TagEnd::BlockQuote(..) => out.push_str("</blockquote>\n"),
                TagEnd::CodeBlock => out.push_str("</pre>\n"),
                TagEnd::Item => {
                    in_list_item = false;
                    out.push('\n');
                }
                TagEnd::List(_) => out.push('\n'),
                TagEnd::Emphasis => out.push_str("</i>"),
                TagEnd::Strong => out.push_str("</b>"),
                TagEnd::Strikethrough => out.push_str("</s>"),
                TagEnd::Link => out.push_str("</a>"),
                _ => {}
            },
            Event::Text(t) => out.push_str(&html_escape(&t)),
            Event::Code(t) => {
                out.push_str("<code>");
                out.push_str(&html_escape(&t));
                out.push_str("</code>");
            }
            Event::SoftBreak => out.push('\n'),
            Event::HardBreak => out.push('\n'),
            Event::Rule => out.push_str("──────────\n"),
            _ => {}
        }
    }

    out.trim().to_string()
}

fn html_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

/// Telegram bot adapter using the Bot API directly via HTTP.
pub struct TelegramAdapter {
    bot_token: String,
    api_url: String,
    client: reqwest::Client,
    /// Polling interval in seconds for getUpdates.
    #[allow(dead_code)]
    poll_interval_secs: u64,
}

impl TelegramAdapter {
    pub fn new(bot_token: String) -> Self {
        let api_url = format!("https://api.telegram.org/bot{bot_token}");

        // No client-level timeout — each method applies its own
        // via per-request `.timeout()`. This avoids races between the
        // client timeout and the getUpdates long-poll timeout (both 30s).
        let mut client_builder = reqwest::Client::builder();

        // Apply proxy from TELEGRAM_PROXY env var if set
        if let Ok(proxy_url) = std::env::var("TELEGRAM_PROXY") {
            let proxy_url = proxy_url.trim().to_string();
            if !proxy_url.is_empty() {
                let proxy = reqwest::Proxy::all(&proxy_url).expect("invalid proxy URL");
                client_builder = client_builder.proxy(proxy);
                tracing::info!("Telegram proxy configured: {proxy_url}");
            }
        }

        Self {
            bot_token,
            api_url,
            client: client_builder.build().expect("valid reqwest client"),
            poll_interval_secs: 1,
        }
    }

    /// Set a custom base URL (e.g., for testing with a local bot API server).
    pub fn with_base_url(mut self, base_url: &str) -> Self {
        self.api_url = format!("{}/bot{}", base_url.trim_end_matches('/'), self.bot_token);
        self
    }

    fn sanitized_url(&self) -> String {
        // Redact the token portion for safe logging
        let prefix = self.api_url.trim_end_matches(&self.bot_token);
        format!("{}<redacted>", prefix)
    }

    /// Sanitize a string by replacing all occurrences of the bot token.
    fn sanitize_str(&self, s: &str) -> String {
        s.replace(&self.bot_token, "<redacted>")
    }

    /// Call a Telegram API method with a per-request timeout.
    ///
    /// `timeout` is applied to the HTTP request itself (connect + send + receive).
    /// `getUpdates` should pass a value > its long-poll timeout (e.g. 60s);
    /// all other methods should pass a normal deadline (e.g. 15s).
    async fn api_post_timeout<T: serde::Serialize>(
        &self,
        method: &str,
        body: &T,
        timeout: Duration,
    ) -> Result<serde_json::Value> {
        let url = format!("{}/{method}", self.api_url);
        let resp = self
            .client
            .post(&url)
            .json(body)
            .timeout(timeout)
            .send()
            .await
            .map_err(|e| {
                // Determine error category for diagnostics
                let tag = if e.is_timeout() {
                    "timeout"
                } else if e.is_connect() {
                    "connect"
                } else if e.is_body() {
                    "body"
                } else {
                    "request"
                };
                // Include the source chain, sanitizing the token from all text
                let chain =
                    std::iter::successors(Some(&e as &dyn std::error::Error), |e| e.source())
                        .map(|s| self.sanitize_str(&s.to_string()))
                        .collect::<Vec<_>>()
                        .join(": ");
                Error::Telegram(format!(
                    "network error ({tag}) calling {}: {chain}",
                    self.sanitized_url(),
                ))
            })?;

        if !resp.status().is_success() {
            let status = resp.status();
            let json: serde_json::Value = resp.json().await.unwrap_or(serde_json::Value::Null);
            let desc = json
                .get("description")
                .and_then(|v| v.as_str())
                .unwrap_or("unknown error");
            return Err(Error::Telegram(format!(
                "API error (HTTP {status}): {desc}"
            )));
        }

        let json: serde_json::Value = resp
            .json()
            .await
            .map_err(|e| Error::Telegram(format!("JSON parse error: {e}")))?;

        Ok(json["result"].clone())
    }

    /// Convenience wrapper with a 15-second timeout for normal API calls.
    async fn api_post<T: serde::Serialize>(
        &self,
        method: &str,
        body: &T,
    ) -> Result<serde_json::Value> {
        self.api_post_timeout(method, body, Duration::from_secs(15))
            .await
    }

    async fn get_updates(&self, offset: Option<i64>) -> Result<Vec<serde_json::Value>> {
        // Use a short poll timeout (5s) because the proxy at 127.0.0.1:7890
        // kills idle connections after ~3s. A longer timeout would cause
        // constant "connection closed" errors and risk missing updates.
        let mut params = serde_json::json!({
            "timeout": 5,
            "allowed_updates": ["message", "callback_query", "my_chat_member"]
        });
        if let Some(o) = offset {
            params["offset"] = serde_json::json!(o);
        }
        // Client timeout must exceed the poll timeout to avoid racing.
        let result = self
            .api_post_timeout("getUpdates", &params, Duration::from_secs(10))
            .await?;
        Ok(result.as_array().cloned().unwrap_or_default())
    }

    /// Download a file from Telegram by its `file_id`.
    ///
    /// Uses `getFile` to resolve the file path, then downloads from
    /// Telegram's file server.
    async fn download_file(&self, file_id: &str) -> Result<Vec<u8>> {
        let result = self
            .api_post("getFile", &serde_json::json!({ "file_id": file_id }))
            .await?;
        let file_path = result["file_path"]
            .as_str()
            .ok_or_else(|| Error::Telegram("missing file_path in getFile response".into()))?;

        let url = format!(
            "https://api.telegram.org/file/bot{}/{}",
            self.bot_token, file_path
        );
        let resp = self
            .client
            .get(&url)
            .timeout(Duration::from_secs(30))
            .send()
            .await
            .map_err(|e| Error::Telegram(format!("file download failed: {e}")))?;

        if !resp.status().is_success() {
            return Err(Error::Telegram(format!(
                "file download HTTP {}",
                resp.status()
            )));
        }

        let bytes = resp
            .bytes()
            .await
            .map_err(|e| Error::Telegram(format!("file read error: {e}")))?;
        Ok(bytes.to_vec())
    }
}

#[async_trait]
impl ImAdapter for TelegramAdapter {
    async fn run(&self, tx: mpsc::UnboundedSender<ImEvent>) -> Result<()> {
        let mut offset: Option<i64> = None;

        loop {
            match self.get_updates(offset).await {
                Ok(updates) => {
                    for update in updates {
                        // Advance offset past this update
                        if let Some(update_id) = update["update_id"].as_i64() {
                            offset = Some(update_id + 1);
                        }

                        // Process message
                        if let Some(msg) = update.get("message")
                            && let Some(event) = parse_message(msg)
                        {
                            let event = if matches!(event.kind, ImEventKind::Voice(..)) {
                                if let Some(file_id) = msg["voice"]["file_id"].as_str() {
                                    match self.download_file(file_id).await {
                                        Ok(data) => ImEvent {
                                            kind: ImEventKind::Voice(data),
                                            ..event
                                        },
                                        Err(e) => {
                                            tracing::error!("Failed to download voice: {e}");
                                            event
                                        }
                                    }
                                } else {
                                    event
                                }
                            } else {
                                event
                            };
                            let _ = tx.send(event);
                        }

                        // Process callback query
                        if let Some(cq) = update.get("callback_query")
                            && let Some(event) = parse_callback_query(cq)
                        {
                            let _ = tx.send(event);
                        }

                        // Process bot added to group
                        if let Some(mcm) = update.get("my_chat_member")
                            && let Some(event) = parse_bot_added(mcm)
                        {
                            let _ = tx.send(event);
                        }
                    }
                }
                Err(e) => {
                    tracing::error!("Telegram poll error: {e}");
                    tokio::time::sleep(std::time::Duration::from_secs(5)).await;
                }
            }
        }
    }

    async fn send_message(&self, target: &MessageTarget, text: &str) -> Result<MessageId> {
        let html = markdown_to_html(text);
        let mut params = serde_json::json!({
            "chat_id": target.chat_id.0,
            "text": html,
            "parse_mode": "HTML",
        });
        if let Some(thread) = target.thread_id {
            params["message_thread_id"] = serde_json::json!(thread.0);
        }
        let result = self.api_post("sendMessage", &params).await?;
        Ok(MessageId(
            result["message_id"].as_i64().unwrap_or(0).to_string(),
        ))
    }

    async fn send_check_card(
        &self,
        target: &MessageTarget,
        title: &str,
        items: &[CheckItem],
    ) -> Result<MessageId> {
        let mut html_parts = vec![format!("🔍 <b>{}</b>", title)];
        for item in items {
            let emoji = item.status.emoji();
            html_parts.push(format!("{} <b>{}</b> — {}", emoji, item.label, item.detail));
        }
        let html = html_parts.join("\n");
        let mut params = serde_json::json!({
            "chat_id": target.chat_id.0,
            "text": html,
            "parse_mode": "HTML",
        });
        if let Some(thread) = target.thread_id {
            params["message_thread_id"] = serde_json::json!(thread.0);
        }
        let result = self.api_post("sendMessage", &params).await?;
        Ok(MessageId(
            result["message_id"].as_i64().unwrap_or(0).to_string(),
        ))
    }

    async fn edit_message(
        &self,
        target: &MessageTarget,
        msg_id: &MessageId,
        text: &str,
    ) -> Result<()> {
        let html = markdown_to_html(text);
        let mut params = serde_json::json!({
            "chat_id": target.chat_id.0,
            "message_id": msg_id.0,
            "text": html,
            "parse_mode": "HTML",
        });
        if let Some(thread) = target.thread_id {
            params["message_thread_id"] = serde_json::json!(thread.0);
        }
        self.api_post("editMessageText", &params).await?;
        Ok(())
    }

    async fn send_photo(
        &self,
        target: &MessageTarget,
        filename: &str,
        data: &[u8],
    ) -> Result<MessageId> {
        let url = format!("{}/sendPhoto", self.api_url);
        let mut form = reqwest::multipart::Form::new()
            .text("chat_id", target.chat_id.0.to_string())
            .part(
                "photo",
                reqwest::multipart::Part::bytes(data.to_vec()).file_name(filename.to_string()),
            );
        if let Some(thread) = target.thread_id {
            form = form.text("message_thread_id", thread.0.to_string());
        }

        let resp = self
            .client
            .post(&url)
            .multipart(form)
            .send()
            .await
            .map_err(|e| Error::Telegram(format!("HTTP error: {e}")))?;
        let json: serde_json::Value = resp
            .json()
            .await
            .map_err(|e| Error::Telegram(format!("JSON parse error: {e}")))?;

        if json.get("ok").and_then(|v| v.as_bool()).unwrap_or(false) {
            Ok(MessageId(
                json["result"]["message_id"]
                    .as_i64()
                    .unwrap_or(0)
                    .to_string(),
            ))
        } else {
            let desc = json
                .get("description")
                .and_then(|v| v.as_str())
                .unwrap_or("unknown error");
            Err(Error::Telegram(format!("sendPhoto failed: {desc}")))
        }
    }

    async fn send_keyboard(
        &self,
        target: &MessageTarget,
        text: &str,
        buttons: &[Vec<Button>],
    ) -> Result<MessageId> {
        let inline_keyboard: Vec<Vec<serde_json::Value>> = buttons
            .iter()
            .map(|row| {
                row.iter()
                    .map(|btn| {
                        serde_json::json!({
                            "text": btn.text,
                            "callback_data": btn.callback_data,
                        })
                    })
                    .collect()
            })
            .collect();

        let mut params = serde_json::json!({
            "chat_id": target.chat_id.0,
            "text": text,
            "reply_markup": {
                "inline_keyboard": inline_keyboard,
            },
        });
        if let Some(thread) = target.thread_id {
            params["message_thread_id"] = serde_json::json!(thread.0);
        }
        let result = self.api_post("sendMessage", &params).await?;
        Ok(MessageId(
            result["message_id"].as_i64().unwrap_or(0).to_string(),
        ))
    }

    async fn delete_message(&self, target: &MessageTarget, msg_id: &MessageId) -> Result<()> {
        let mut params = serde_json::json!({
            "chat_id": target.chat_id.0,
            "message_id": msg_id.0,
        });
        if let Some(thread) = target.thread_id {
            params["message_thread_id"] = serde_json::json!(thread.0);
        }
        self.api_post("deleteMessage", &params).await?;
        Ok(())
    }

    async fn edit_keyboard(
        &self,
        target: &MessageTarget,
        msg_id: &MessageId,
        buttons: &[Vec<Button>],
    ) -> Result<()> {
        let inline_keyboard: Vec<Vec<serde_json::Value>> = buttons
            .iter()
            .map(|row| {
                row.iter()
                    .map(|btn| {
                        serde_json::json!({
                            "text": btn.text,
                            "callback_data": btn.callback_data,
                        })
                    })
                    .collect()
            })
            .collect();

        let mut params = serde_json::json!({
            "chat_id": target.chat_id.0,
            "message_id": msg_id.0,
            "reply_markup": {
                "inline_keyboard": inline_keyboard,
            },
        });
        if let Some(thread) = target.thread_id {
            params["message_thread_id"] = serde_json::json!(thread.0);
        }
        self.api_post("editMessageReplyMarkup", &params).await?;
        Ok(())
    }

    async fn send_chat_action(&self, target: &MessageTarget) -> Result<()> {
        let mut params = serde_json::json!({
            "chat_id": target.chat_id.0,
            "action": "typing",
        });
        if let Some(thread) = target.thread_id {
            params["message_thread_id"] = serde_json::json!(thread.0);
        }
        self.api_post("sendChatAction", &params).await?;
        Ok(())
    }

    async fn answer_callback(&self, callback_query_id: &str, text: &str) -> Result<()> {
        let params = serde_json::json!({
            "callback_query_id": callback_query_id,
            "text": text,
            "show_alert": false,
        });
        self.api_post("answerCallbackQuery", &params).await?;
        Ok(())
    }

    async fn add_reaction(
        &self,
        target: &MessageTarget,
        message_id: &str,
        _emoji: &str,
    ) -> Result<()> {
        let params = serde_json::json!({
            "chat_id": target.chat_id.0,
            "message_id": message_id.parse::<i64>().unwrap_or(0),
            "reaction": [{"type": "emoji", "emoji": "👍"}],
        });
        self.api_post("setMessageReaction", &params).await?;
        Ok(())
    }
}

// ── Parsing helpers ──

fn parse_message(msg: &serde_json::Value) -> Option<ImEvent> {
    let chat_id = msg["chat"]["id"].as_i64()?;
    // Service messages (topic close, etc.) may not have a `from` field
    let user_id = msg["from"]["id"].as_i64().unwrap_or(0);
    let thread_id = msg["is_topic_message"]
        .as_bool()
        .filter(|&b| b)
        .and_then(|_| msg["message_thread_id"].as_i64());

    let chat_type = msg["chat"]["type"].as_str().unwrap_or("private");
    let is_group = chat_type == "group" || chat_type == "supergroup";
    let chat_name = msg["chat"]["title"].as_str().map(String::from);

    let target = MessageTarget {
        chat_id: atim_core::message::ChatId(chat_id),
        thread_id: thread_id.map(atim_core::message::ThreadId),
        chat_name,
    };

    let kind = if let Some(tc) = msg.get("forum_topic_created") {
        let name = tc.get("name").and_then(|v| v.as_str()).unwrap_or("");
        ImEventKind::TopicCreated {
            name: name.to_string(),
        }
    } else if let Some(te) = msg.get("forum_topic_edited") {
        let new_name = te.get("name").and_then(|v| v.as_str()).unwrap_or("");
        ImEventKind::TopicEdited {
            new_name: new_name.to_string(),
        }
    } else if msg.get("forum_topic_closed").is_some() {
        ImEventKind::TopicClosed
    } else if let Some(text) = msg.get("text").and_then(|v| v.as_str()) {
        ImEventKind::Text {
            text: text.to_string(),
            is_mention: false,
            is_group,
            message_id: msg["message_id"].as_i64().map(|id| id.to_string()),
        }
    } else if msg.get("photo").is_some() {
        ImEventKind::Photo {
            caption: msg
                .get("caption")
                .and_then(|v| v.as_str())
                .map(String::from),
            data: Vec::new(), // Would need to download the photo file
            mime_type: "image/jpeg".into(),
        }
    } else if msg.get("voice").is_some() {
        ImEventKind::Voice(Vec::new()) // Would need to download voice file
    } else {
        return None;
    };

    Some(ImEvent {
        user_id: atim_core::message::UserId(user_id),
        target,
        kind,
    })
}

fn parse_callback_query(cq: &serde_json::Value) -> Option<ImEvent> {
    let user_id = cq["from"]["id"].as_i64()?;
    let msg = cq.get("message")?;
    let chat_id = msg["chat"]["id"].as_i64()?;
    let thread_id = msg["is_topic_message"]
        .as_bool()
        .filter(|&b| b)
        .and_then(|_| msg["message_thread_id"].as_i64());
    let msg_id = msg["message_id"].as_i64()?;
    let data = cq["data"].as_str()?;
    let query_id = cq["id"].as_str().map(String::from);

    Some(ImEvent {
        user_id: atim_core::message::UserId(user_id),
        target: MessageTarget {
            chat_id: atim_core::message::ChatId(chat_id),
            thread_id: thread_id.map(atim_core::message::ThreadId),
            chat_name: None,
        },
        kind: ImEventKind::CallbackQuery {
            data: data.to_string(),
            msg_id: MessageId(msg_id.to_string()),
            callback_query_id: query_id,
        },
    })
}

fn parse_bot_added(mcm: &serde_json::Value) -> Option<ImEvent> {
    let chat_id = mcm["chat"]["id"].as_i64()?;
    let new_status = mcm["new_chat_member"]["status"].as_str()?;
    if new_status != "member" && new_status != "administrator" {
        return None;
    }
    let chat_name = mcm["chat"]
        .get("title")
        .and_then(|v| v.as_str().map(|s| s.to_string()));
    Some(ImEvent {
        user_id: UserId(0),
        target: MessageTarget {
            chat_id: ChatId(chat_id),
            thread_id: None,
            chat_name: chat_name.clone(),
        },
        kind: ImEventKind::BotAdded { chat_name },
    })
}
