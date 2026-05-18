//! Feishu (Lark) IM adapter.
//!
//! Architecture:
//!   - Webhook-based (Feishu pushes events to an axum HTTP server)
//!   - Token auth via app_id + app_secret (auto-refresh every 2 hours)
//!   - Interactive cards for inline keyboard UI
//!   - String IDs (open_id, chat_id) hashed to i64 for Aim's core types

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use axum::extract::State;
use axum::routing::post;
use axum::{Json, Router};
use tokio::sync::mpsc;
use tokio::sync::RwLock;

use aim_core::error::{Error, Result};
use aim_core::im::ImAdapter;
use aim_core::message::{
    Button, ChatId, ImEvent, ImEventKind, MessageId, MessageTarget, ThreadId, UserId,
};

// ── FeishuAdapter ──

pub struct FeishuAdapter {
    app_id: String,
    app_secret: String,
    port: u16,
    client: reqwest::Client,
    token_mgr: Arc<RwLock<TokenManager>>,
    /// Stable hash → original Feishu string ID (for outbound API calls).
    id_map: Arc<RwLock<IdMap>>,
    event_tx: Arc<RwLock<Option<mpsc::UnboundedSender<ImEvent>>>>,
}

/// Cached tenant access token with expiry.
struct TokenManager {
    app_id: String,
    app_secret: String,
    token: String,
    expires_at: Instant,
}

/// Bi-directional mapping between Aim i64 IDs and Feishu string IDs.
struct IdMap {
    user_ids: HashMap<i64, String>,  // hash → open_id
    chat_ids: HashMap<i64, String>,  // hash → chat_id
}

impl FeishuAdapter {
    pub fn new(app_id: String, app_secret: String, port: u16) -> Self {
        Self {
            app_id: app_id.clone(),
            app_secret: app_secret.clone(),
            port,
            client: reqwest::Client::builder()
                .timeout(Duration::from_secs(30))
                .build()
                .expect("valid reqwest client"),
            token_mgr: Arc::new(RwLock::new(TokenManager {
                app_id,
                app_secret,
                token: String::new(),
                expires_at: Instant::now(),
            })),
            id_map: Arc::new(RwLock::new(IdMap {
                user_ids: HashMap::new(),
                chat_ids: HashMap::new(),
            })),
            event_tx: Arc::new(RwLock::new(None)),
        }
    }

    // ── Token management ──

    /// Get a valid tenant_access_token, refreshing if needed.
    async fn get_token(&self) -> Result<String> {
        let mut mgr = self.token_mgr.write().await;
        if mgr.token.is_empty() || Instant::now() >= mgr.expires_at {
            mgr.refresh(&self.client).await?;
        }
        Ok(mgr.token.clone())
    }

    // ── API helpers ──

    /// Send an authenticated GET request to the Feishu API.
    async fn api_get(&self, path: &str) -> Result<serde_json::Value> {
        let token = self.get_token().await?;
        let url = format!("https://open.feishu.cn/open-apis{path}");
        let resp = self
            .client
            .get(&url)
            .header("Authorization", format!("Bearer {token}"))
            .send()
            .await
            .map_err(|e| Error::Feishu(format!("HTTP error: {e}")))?;

        let json: serde_json::Value = resp.json().await
            .map_err(|e| Error::Feishu(format!("JSON decode: {e}")))?;

        if json["code"].as_i64().unwrap_or(-1) != 0 {
            let msg = json["msg"].as_str().unwrap_or("unknown");
            return Err(Error::Feishu(format!("API error: {msg}")));
        }
        Ok(json["data"].clone())
    }

    /// Send an authenticated POST request to the Feishu API.
    async fn api_post(&self, path: &str, body: &serde_json::Value) -> Result<serde_json::Value> {
        let token = self.get_token().await?;
        let url = format!("https://open.feishu.cn/open-apis{path}");
        let resp = self
            .client
            .post(&url)
            .header("Authorization", format!("Bearer {token}"))
            .json(body)
            .send()
            .await
            .map_err(|e| Error::Feishu(format!("HTTP error: {e}")))?;

        let json: serde_json::Value = resp.json().await
            .map_err(|e| Error::Feishu(format!("JSON decode: {e}")))?;

        if json["code"].as_i64().unwrap_or(-1) != 0 {
            let msg = json["msg"].as_str().unwrap_or("unknown");
            return Err(Error::Feishu(format!("API error: {msg}")));
        }
        Ok(json["data"].clone())
    }

    /// Upload image data and return the image_key.
    async fn upload_image(&self, data: &[u8], filename: &str) -> Result<String> {
        let token = self.get_token().await?;
        let url = "https://open.feishu.cn/open-apis/im/v1/images";
        let mime = mime_guess::from_path(filename).first_or_octet_stream();

        let form = reqwest::multipart::Form::new()
            .part("image", reqwest::multipart::Part::bytes(data.to_vec())
                .file_name(filename.to_string())
                .mime_str(mime.as_ref())
                .map_err(|e| Error::Feishu(format!("mime error: {e}")))?)
            .text("image_type", "message");

        let resp = self.client.post(url)
            .header("Authorization", format!("Bearer {token}"))
            .multipart(form)
            .send().await
            .map_err(|e| Error::Feishu(format!("upload error: {e}")))?;

        let json: serde_json::Value = resp.json().await
            .map_err(|e| Error::Feishu(format!("JSON decode: {e}")))?;

        if json["code"].as_i64().unwrap_or(-1) != 0 {
            let msg = json["msg"].as_str().unwrap_or("unknown");
            return Err(Error::Feishu(format!("image upload error: {msg}")));
        }
        Ok(json["data"]["image_key"].as_str().unwrap_or("").to_string())
    }

    // ── ID mapping ──

    /// Deterministically hash a Feishu string ID to i64.
    fn hash_id(id: &str) -> i64 {
        use std::hash::{Hash, Hasher};
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        id.hash(&mut hasher);
        hasher.finish() as i64
    }

    /// Register a Feishu open_id and return its stable i64 UserId.
    async fn register_user(&self, open_id: &str) -> UserId {
        let uid = Self::hash_id(open_id);
        let mut map = self.id_map.write().await;
        map.user_ids.entry(uid).or_insert_with(|| open_id.to_string());
        UserId(uid)
    }

    /// Register a Feishu chat_id and return its stable i64 ChatId.
    async fn register_chat(&self, chat_id: &str) -> ChatId {
        let cid = Self::hash_id(chat_id);
        let mut map = self.id_map.write().await;
        map.chat_ids.entry(cid).or_insert_with(|| chat_id.to_string());
        ChatId(cid)
    }

    /// Look up the original Feishu open_id for a UserId.
    #[allow(dead_code)]
    async fn resolve_user(&self, uid: &UserId) -> Option<String> {
        let map = self.id_map.read().await;
        map.user_ids.get(&uid.0).cloned()
    }

    /// Look up the original Feishu chat_id for a ChatId.
    async fn resolve_chat(&self, cid: &ChatId) -> Option<String> {
        let map = self.id_map.read().await;
        map.chat_ids.get(&cid.0).cloned()
    }
}

// ── Token refresh ──

impl TokenManager {
    async fn refresh(&mut self, client: &reqwest::Client) -> Result<()> {
        let body = serde_json::json!({
            "app_id": self.app_id,
            "app_secret": self.app_secret,
        });

        let resp = client
            .post("https://open.feishu.cn/open-apis/auth/v3/tenant_access_token/internal")
            .json(&body)
            .send()
            .await
            .map_err(|e| Error::Feishu(format!("token refresh HTTP error: {e}")))?;

        let json: serde_json::Value = resp
            .json()
            .await
            .map_err(|e| Error::Feishu(format!("token refresh JSON error: {e}")))?;

        if json["code"].as_i64().unwrap_or(-1) != 0 {
            let msg = json["msg"].as_str().unwrap_or("unknown");
            return Err(Error::Feishu(format!("token refresh failed: {msg}")));
        }

        self.token = json["tenant_access_token"]
            .as_str()
            .unwrap_or("")
            .to_string();
        let expire = json["expire"].as_i64().unwrap_or(7200);

        // Refresh 5 minutes early
        self.expires_at = Instant::now() + Duration::from_secs(expire as u64 - 300);

        tracing::info!("Feishu token refreshed, expires in {expire}s");
        Ok(())
    }
}

// ── ImAdapter implementation ──

#[async_trait]
impl ImAdapter for FeishuAdapter {
    async fn run(&self, tx: mpsc::UnboundedSender<ImEvent>) -> Result<()> {
        // Store the event sender so webhook handlers can emit events
        {
            let mut stored = self.event_tx.write().await;
            *stored = Some(tx);
        }

        let addr = SocketAddr::from(([0, 0, 0, 0], self.port));
        tracing::info!("Feishu webhook listening on {addr}");

        let app_state = WebhookState {
            adapter: self.clone(),
        };

        let app = Router::new()
            .route("/webhook/feishu", post(handle_webhook))
            .route("/health", post(|| async { Json(serde_json::json!({"status": "ok"})) }))
            .with_state(app_state);

        let listener = tokio::net::TcpListener::bind(addr)
            .await
            .map_err(|e| Error::Feishu(format!("bind failed: {e}")))?;

        axum::serve(listener, app)
            .await
            .map_err(|e| Error::Feishu(format!("server error: {e}")))?;

        Ok(())
    }

    async fn send_message(&self, target: &MessageTarget, text: &str) -> Result<MessageId> {
        let chat_id = self.resolve_chat(&target.chat_id)
            .await
            .ok_or_else(|| Error::Feishu("unknown chat_id".into()))?;

        let content = serde_json::json!({"text": text});
        let body = serde_json::json!({
            "receive_id": chat_id,
            "msg_type": "text",
            "content": content.to_string(),
        });

        let data = self.api_post("/im/v1/messages?receive_id_type=chat_id", &body).await?;
        let msg_id = data["message_id"].as_str().unwrap_or("").to_string();
        Ok(MessageId(msg_id))
    }

    async fn edit_message(&self, target: &MessageTarget, msg_id: &MessageId, text: &str) -> Result<()> {
        // Feishu only supports editing card (interactive) messages
        // For text messages we'd need to send a new message and delete the old one
        // Patch the message content
        let chat_id = self.resolve_chat(&target.chat_id)
            .await
            .ok_or_else(|| Error::Feishu("unknown chat_id".into()))?;

        // Feishu PATCH /im/v1/messages/:message_id only works for card messages
        // For text, fall back to send + delete pattern
        let _ = chat_id;
        let _ = msg_id;
        let _ = text;

        // Send new message instead
        self.send_message(target, text).await?;
        Ok(())
    }

    async fn send_photo(&self, target: &MessageTarget, filename: &str, data: &[u8]) -> Result<MessageId> {
        let chat_id = self.resolve_chat(&target.chat_id)
            .await
            .ok_or_else(|| Error::Feishu("unknown chat_id".into()))?;

        let image_key = self.upload_image(data, filename).await?;
        if image_key.is_empty() {
            return Err(Error::Feishu("image upload returned empty key".into()));
        }

        let content = serde_json::json!({"image_key": image_key});
        let body = serde_json::json!({
            "receive_id": chat_id,
            "msg_type": "image",
            "content": content.to_string(),
        });

        let data = self.api_post("/im/v1/messages?receive_id_type=chat_id", &body).await?;
        let msg_id = data["message_id"].as_str().unwrap_or("").to_string();
        Ok(MessageId(msg_id))
    }

    async fn send_keyboard(
        &self,
        target: &MessageTarget,
        text: &str,
        buttons: &[Vec<Button>],
    ) -> Result<MessageId> {
        let chat_id = self.resolve_chat(&target.chat_id)
            .await
            .ok_or_else(|| Error::Feishu("unknown chat_id".into()))?;

        let card = build_card(text, buttons);

        let body = serde_json::json!({
            "receive_id": chat_id,
            "msg_type": "interactive",
            "content": serde_json::to_string(&card)
                .map_err(|e| Error::Feishu(format!("card serialization: {e}")))?,
        });

        let data = self.api_post("/im/v1/messages?receive_id_type=chat_id", &body).await?;
        let msg_id = data["message_id"].as_str().unwrap_or("").to_string();
        Ok(MessageId(msg_id))
    }

    async fn delete_message(&self, _target: &MessageTarget, msg_id: &MessageId) -> Result<()> {
        self.api_get(&format!("/im/v1/messages/{}", msg_id.0)).await?;
        // Feishu doesn't have a direct "delete" for user-visible messages,
        // but recalling is not supported for bot messages.
        // We mark it as handled.
        tracing::debug!("Feishu message {} deletion not supported by platform", msg_id.0);
        Ok(())
    }

    async fn edit_keyboard(
        &self,
        target: &MessageTarget,
        msg_id: &MessageId,
        buttons: &[Vec<Button>],
    ) -> Result<()> {
        let chat_id = self.resolve_chat(&target.chat_id)
            .await
            .ok_or_else(|| Error::Feishu("unknown chat_id".into()))?;

        // For card messages, PATCH updates the card content
        // We need to send a new card and delete the old one
        let _ = chat_id;
        let _ = msg_id;
        let _ = buttons;

        // Build a placeholder card with updated buttons
        let card = build_card("(updated)", buttons);
        let body = serde_json::json!({
            "content": serde_json::to_string(&card)
                .map_err(|e| Error::Feishu(format!("card serialization: {e}")))?,
        });

        // PATCH /im/v1/messages/:message_id
        let token = self.get_token().await?;
        let url = format!("https://open.feishu.cn/open-apis/im/v1/messages/{}", msg_id.0);
        let resp = self.client.patch(&url)
            .header("Authorization", format!("Bearer {token}"))
            .json(&body)
            .send().await
            .map_err(|e| Error::Feishu(format!("HTTP error: {e}")))?;

        let json: serde_json::Value = resp.json().await
            .map_err(|e| Error::Feishu(format!("JSON decode: {e}")))?;

        if json["code"].as_i64().unwrap_or(-1) != 0 {
            let msg = json["msg"].as_str().unwrap_or("unknown");
            return Err(Error::Feishu(format!("edit keyboard error: {msg}")));
        }
        Ok(())
    }

    async fn answer_callback(&self, _callback_query_id: &str, _text: &str) -> Result<()> {
        // Feishu card actions don't have a direct "answer" mechanism.
        // The card can be updated via PATCH if needed.
        Ok(())
    }

    async fn send_chat_action(&self, target: &MessageTarget) -> Result<()> {
        let chat_id = self.resolve_chat(&target.chat_id)
            .await
            .ok_or_else(|| Error::Feishu("unknown chat_id".into()))?;

        // Feishu doesn't have a direct "typing" indicator API for open messages.
        // Use get_message to check if the chat still exists.
        let token = self.get_token().await?;
        let url = format!("https://open.feishu.cn/open-apis/im/v1/messages?container_id_type=chat&container_id={chat_id}");
        let resp = self.client.get(&url)
            .header("Authorization", format!("Bearer {token}"))
            .send().await
            .map_err(|e| Error::Feishu(format!("chat action probe error: {e}")))?;

        let json: serde_json::Value = resp.json().await
            .map_err(|e| Error::Feishu(format!("JSON decode: {e}")))?;

        let code = json["code"].as_i64().unwrap_or(-1);
        if code != 0 && code != 10000 && code != 99991663 {
            let msg = json["msg"].as_str().unwrap_or("unknown");
            return Err(Error::Feishu(format!("chat/probe error ({code}): {msg}")));
        }
        Ok(())
    }
}

impl Clone for FeishuAdapter {
    fn clone(&self) -> Self {
        Self {
            app_id: self.app_id.clone(),
            app_secret: self.app_secret.clone(),
            port: self.port,
            client: self.client.clone(),
            token_mgr: self.token_mgr.clone(),
            id_map: self.id_map.clone(),
            event_tx: self.event_tx.clone(),
        }
    }
}

// ── Webhook ──

#[derive(Clone)]
struct WebhookState {
    adapter: FeishuAdapter,
}

/// Handle incoming webhook events from Feishu.
async fn handle_webhook(
    State(state): State<WebhookState>,
    Json(payload): Json<serde_json::Value>,
) -> Json<serde_json::Value> {
    // Feishu webhook URL verification challenge
    if let Some(challenge) = payload.get("challenge").and_then(|v| v.as_str()) {
        tracing::debug!("Feishu webhook challenge");
        return Json(serde_json::json!({"challenge": challenge}));
    }

    let event_type = payload["header"]["event_type"].as_str().unwrap_or("");
    tracing::debug!("Feishu event: {event_type}");

    match event_type {
        "im.message.receive_v1" => {
            if let Err(e) = handle_message_event(&state.adapter, &payload).await {
                tracing::error!("Feishu message handler: {e}");
            }
        }
        "card.action.trigger" => {
            if let Err(e) = handle_card_action(&state.adapter, &payload).await {
                tracing::error!("Feishu card action handler: {e}");
            }
        }
        _ => {
            tracing::debug!("Unhandled Feishu event type: {event_type}");
        }
    }

    // Always return OK to Feishu
    Json(serde_json::json!({}))
}

async fn handle_message_event(adapter: &FeishuAdapter, payload: &serde_json::Value) -> Result<()> {
    let event = &payload["event"];
    let sender = &event["sender"];
    let message = &event["message"];

    let open_id = sender["sender_id"]["open_id"]
        .as_str()
        .ok_or_else(|| Error::Feishu("missing open_id".into()))?;
    let chat_id = message["chat_id"]
        .as_str()
        .ok_or_else(|| Error::Feishu("missing chat_id".into()))?;
    let msg_type = message["msg_type"].as_str().unwrap_or("");
    let _message_id = message["message_id"].as_str().unwrap_or("");
    let chat_type = message["chat_type"].as_str().unwrap_or("p2p");
    let root_id = message["root_id"].as_str();  // thread root (if any)

    let user_id = adapter.register_user(open_id).await;
    let chat_id_aim = adapter.register_chat(chat_id).await;

    // For group chats, use sender's user hash as thread_id
    let thread_id = if chat_type == "group" {
        Some(ThreadId(adapter.register_user(open_id).await.0))
    } else {
        root_id.map(|id| ThreadId(FeishuAdapter::hash_id(id)))
    };

    let target = MessageTarget {
        chat_id: chat_id_aim,
        thread_id,
    };

    let parsed_content: serde_json::Value = serde_json::from_str(
        message["content"].as_str().unwrap_or("{}"),
    ).unwrap_or_default();

    let kind = match msg_type {
        "text" => {
            let text = parsed_content["text"].as_str().unwrap_or("");
            ImEventKind::Text(text.to_string())
        }
        "image" => {
            let image_key = parsed_content["image_key"].as_str().unwrap_or("");
            // Download image data
            let data = adapter.download_image(image_key).await.unwrap_or_default();
            ImEventKind::Photo {
                caption: None,
                data,
                mime_type: "image/png".into(),
            }
        }
        _ => return Ok(()),  // skip audio, file, sticker, etc.
    };

    if let Some(tx) = adapter.event_tx.read().await.as_ref() {
        let _ = tx.send(ImEvent {
            user_id,
            target,
            kind,
        });
    }

    Ok(())
}

async fn handle_card_action(adapter: &FeishuAdapter, payload: &serde_json::Value) -> Result<()> {
    let event = &payload["event"];
    let action_value = &event["action"]["value"];
    let message_id = event["message_id"]
        .as_str()
        .unwrap_or("");
    let open_id = event["open_id"]
        .as_str()
        .unwrap_or("");
    let chat_id = event["chat_id"]
        .as_str()
        .unwrap_or("");

    let data = serde_json::to_string(action_value)
        .map_err(|e| Error::Feishu(format!("action value serialization: {e}")))?;

    let user_id = adapter.register_user(open_id).await;

    let target = MessageTarget {
        chat_id: adapter.register_chat(chat_id).await,
        thread_id: None,
    };

    // Feishu action_token is the equivalent of Telegram's callback_query_id
    let action_token = event["action"]["token"].as_str()
        .or_else(|| event["action_token"].as_str())
        .map(String::from);

    if let Some(tx) = adapter.event_tx.read().await.as_ref() {
        let _ = tx.send(ImEvent {
            user_id,
            target,
            kind: ImEventKind::CallbackQuery {
                data,
                msg_id: MessageId(message_id.to_string()),
                callback_query_id: action_token,
            },
        });
    }

    Ok(())
}

// ── Image download ──

impl FeishuAdapter {
    /// Download an image by its image_key from Feishu.
    async fn download_image(&self, image_key: &str) -> Result<Vec<u8>> {
        let token = self.get_token().await?;
        let url = format!("https://open.feishu.cn/open-apis/im/v1/images/{image_key}/download");
        let resp = self.client
            .get(&url)
            .header("Authorization", format!("Bearer {token}"))
            .send()
            .await
            .map_err(|e| Error::Feishu(format!("image download error: {e}")))?;

        if !resp.status().is_success() {
            return Err(Error::Feishu(format!(
                "image download failed: HTTP {}",
                resp.status()
            )));
        }

        let bytes = resp.bytes()
            .await
            .map_err(|e| Error::Feishu(format!("image read error: {e}")))?;
        Ok(bytes.to_vec())
    }
}

// ── Card builder ──

/// Build a Feishu interactive card JSON for permission prompts / choices.
fn build_card(text: &str, buttons: &[Vec<Button>]) -> serde_json::Value {
    let mut elements: Vec<serde_json::Value> = vec![
        serde_json::json!({
            "tag": "markdown",
            "content": text,
        }),
    ];

    for row in buttons {
        let actions: Vec<serde_json::Value> = row
            .iter()
            .map(|btn| {
                let btn_type = if btn.callback_data.contains("approve")
                    || btn.callback_data.contains("yes")
                    || btn.callback_data.contains("confirm")
                {
                    "primary"
                } else if btn.callback_data.contains("reject")
                    || btn.callback_data.contains("no")
                {
                    "danger"
                } else {
                    "default"
                };

                serde_json::json!({
                    "tag": "button",
                    "text": {
                        "tag": "plain_text",
                        "content": btn.text,
                    },
                    "value": {
                        "action": btn.callback_data,
                    },
                    "type": btn_type,
                })
            })
            .collect();

        if !actions.is_empty() {
            elements.push(serde_json::json!({
                "tag": "action",
                "actions": actions,
            }));
        }
    }

    serde_json::json!({
        "config": {
            "wide_screen_mode": true,
        },
        "header": {
            "title": {
                "tag": "plain_text",
                "content": "Aim — Agent Response",
            },
            "template": "blue",
        },
        "elements": elements,
    })
}

// ── Tests ──

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_hash_id_deterministic() {
        let id = "ou_abc123";
        let h1 = FeishuAdapter::hash_id(id);
        let h2 = FeishuAdapter::hash_id(id);
        assert_eq!(h1, h2);
    }

    #[test]
    fn test_build_card() {
        let buttons = vec![
            vec![Button {
                text: "Yes".into(),
                callback_data: "approve".into(),
            }],
            vec![Button {
                text: "No".into(),
                callback_data: "reject".into(),
            }],
        ];

        let card = build_card("Proceed?", &buttons);
        assert_eq!(card["header"]["title"]["content"], "Aim — Agent Response");
        assert_eq!(card["elements"].as_array().unwrap().len(), 3); // markdown + 2 action rows

        let actions_row0 = card["elements"][1]["actions"].as_array().unwrap();
        assert_eq!(actions_row0[0]["text"]["content"], "Yes");
        assert_eq!(actions_row0[0]["type"], "primary");
        let actions_row1 = card["elements"][2]["actions"].as_array().unwrap();
        assert_eq!(actions_row1[0]["text"]["content"], "No");
        assert_eq!(actions_row1[0]["type"], "danger");
    }
}
