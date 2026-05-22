//! Feishu (Lark) IM adapter.
//!
//! Architecture:
//!   - WebSocket-based (Feishu pushes events via WSS persistent connection)
//!   - openlark crate for WS connection + token management
//!   - Interactive cards for inline keyboard UI
//!   - String IDs (open_id, chat_id) hashed to i64 for Atim's core types

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use tokio::sync::RwLock;
use tokio::sync::mpsc;

use atim_core::error::{Error, Result};
use atim_core::im::ImAdapter;
use atim_core::message::{
    Button, ChatId, ImEvent, ImEventKind, MessageId, MessageTarget, ThreadId, UserId,
};

use open_lark::Config;
use open_lark::ws_client::{EventDispatcherHandler, LarkWsClient};

const FEISHU_BASE_URL: &str = "https://open.feishu.cn";

// ── FeishuAdapter ──

pub struct FeishuAdapter {
    app_id: String,
    app_secret: String,
    client: reqwest::Client,
    token_cache: Arc<RwLock<TokenCache>>,
    /// Stable hash → original Feishu string ID (for outbound API calls).
    id_map: Arc<RwLock<IdMap>>,
    /// Persistence file for id_map (survives restarts).
    id_map_file: PathBuf,
    event_tx: Arc<RwLock<Option<mpsc::UnboundedSender<ImEvent>>>>,
    /// Bot's own open_id, used to detect @-mentions in group chats.
    bot_open_id: Arc<RwLock<Option<String>>>,
    /// Bot's display name, also used for @-mention detection.
    bot_name: Arc<RwLock<Option<String>>>,
    /// Cached group chat names: Feishu chat_id → display name.
    chat_names: Arc<RwLock<HashMap<String, String>>>,
}

/// Cached tenant access token with auto-refresh via AuthService.
struct TokenCache {
    token: String,
    expires_at: Instant,
    app_id: String,
    app_secret: String,
}

/// Bi-directional mapping between Atim i64 IDs and Feishu string IDs.
struct IdMap {
    user_ids: HashMap<i64, String>, // hash → open_id
    chat_ids: HashMap<i64, String>, // hash → chat_id
    thread_ids: HashMap<i64, String>, // hash → root_id (for topic-based groups)
}

impl FeishuAdapter {
    pub fn new(app_id: String, app_secret: String, atim_dir: PathBuf) -> Self {
        let id_map_file = atim_dir.join("feishu_id_map.json");
        let adapter = Self {
            app_id: app_id.clone(),
            app_secret: app_secret.clone(),
            client: reqwest::Client::builder()
                .timeout(Duration::from_secs(30))
                .build()
                .expect("valid reqwest client"),
            token_cache: Arc::new(RwLock::new(TokenCache {
                token: String::new(),
                expires_at: Instant::now(),
                app_id,
                app_secret,
            })),
            id_map: Arc::new(RwLock::new(IdMap {
                user_ids: HashMap::new(),
                chat_ids: HashMap::new(),
                thread_ids: HashMap::new(),
            })),
            id_map_file,
            event_tx: Arc::new(RwLock::new(None)),
            bot_open_id: Arc::new(RwLock::new(None)),
            bot_name: Arc::new(RwLock::new(None)),
            chat_names: Arc::new(RwLock::new(HashMap::new())),
        };
        // Load persisted id_map so previously-registered chat_ids resolve on restart
        adapter.load_id_map_sync();
        adapter
    }

    // ── Token management (via openlark AuthService) ──

    /// Get a valid tenant_access_token, refreshing if needed.
    async fn get_token(&self) -> Result<String> {
        let mut cache = self.token_cache.write().await;
        if cache.token.is_empty() || Instant::now() >= cache.expires_at {
            cache.refresh().await?;
        }
        Ok(cache.token.clone())
    }

    // ── API helpers (keep reqwest for REST calls) ──

    /// Send an authenticated POST request to the Feishu API.
    async fn api_post(&self, path: &str, body: &serde_json::Value) -> Result<serde_json::Value> {
        let token = self.get_token().await?;
        let url = format!("{FEISHU_BASE_URL}/open-apis{path}");
        let resp = self
            .client
            .post(&url)
            .header("Authorization", format!("Bearer {token}"))
            .json(body)
            .send()
            .await
            .map_err(|e| Error::Feishu(format!("HTTP error: {e}")))?;

        let json: serde_json::Value = resp
            .json()
            .await
            .map_err(|e| Error::Feishu(format!("JSON decode: {e}")))?;

        if json["code"].as_i64().unwrap_or(-1) != 0 {
            let msg = json["msg"].as_str().unwrap_or("unknown");
            return Err(Error::Feishu(format!("API error: {msg}")));
        }
        Ok(json["data"].clone())
    }

    /// Send an authenticated GET request to the Feishu API.
    async fn api_get(&self, path: &str) -> Result<serde_json::Value> {
        let token = self.get_token().await?;
        let url = format!("{FEISHU_BASE_URL}/open-apis{path}");
        let resp = self
            .client
            .get(&url)
            .header("Authorization", format!("Bearer {token}"))
            .send()
            .await
            .map_err(|e| Error::Feishu(format!("HTTP error: {e}")))?;

        let json: serde_json::Value = resp
            .json()
            .await
            .map_err(|e| Error::Feishu(format!("JSON decode: {e}")))?;

        if json["code"].as_i64().unwrap_or(-1) != 0 {
            let msg = json["msg"].as_str().unwrap_or("unknown");
            return Err(Error::Feishu(format!("API error: {msg}")));
        }
        Ok(json["data"].clone())
    }

    /// Send an authenticated DELETE request (message recall).
    /// Only used when `ATIM_FEISHU_ENABLE_DELETE` is set — otherwise recall is skipped.
    async fn api_delete(&self, path: &str) -> Result<serde_json::Value> {
        let token = self.get_token().await?;
        let url = format!("{FEISHU_BASE_URL}/open-apis{path}");
        let resp = self
            .client
            .delete(&url)
            .header("Authorization", format!("Bearer {token}"))
            .send()
            .await
            .map_err(|e| Error::Feishu(format!("HTTP error: {e}")))?;

        let json: serde_json::Value = resp
            .json()
            .await
            .map_err(|e| Error::Feishu(format!("JSON decode: {e}")))?;

        let code = json["code"].as_i64().unwrap_or(-1);
        if code != 0 {
            let msg = json["msg"].as_str().unwrap_or("unknown");
            // 1000001 = message too old to recall, 99991663 = token expired
            if code != 1000001 && code != 99991663 {
                return Err(Error::Feishu(format!("API error: {msg}")));
            }
            tracing::warn!("Feishu message recall failed (code {code}): {msg} — continuing");
        }
        Ok(json["data"].clone())
    }

    /// Check whether message deletion/recall is enabled via env var.
    fn delete_enabled() -> bool {
        std::env::var("ATIM_FEISHU_ENABLE_DELETE").is_ok()
    }

    /// Upload image data and return the image_key.
    async fn upload_image(&self, data: &[u8], filename: &str) -> Result<String> {
        let token = self.get_token().await?;
        let url = "https://open.feishu.cn/open-apis/im/v1/images";
        let mime = mime_guess::from_path(filename).first_or_octet_stream();

        let form = reqwest::multipart::Form::new()
            .part(
                "image",
                reqwest::multipart::Part::bytes(data.to_vec())
                    .file_name(filename.to_string())
                    .mime_str(mime.as_ref())
                    .map_err(|e| Error::Feishu(format!("mime error: {e}")))?,
            )
            .text("image_type", "message");

        let resp = self
            .client
            .post(url)
            .header("Authorization", format!("Bearer {token}"))
            .multipart(form)
            .send()
            .await
            .map_err(|e| Error::Feishu(format!("upload error: {e}")))?;

        let json: serde_json::Value = resp
            .json()
            .await
            .map_err(|e| Error::Feishu(format!("JSON decode: {e}")))?;

        if json["code"].as_i64().unwrap_or(-1) != 0 {
            let msg = json["msg"].as_str().unwrap_or("unknown");
            return Err(Error::Feishu(format!("image upload error: {msg}")));
        }
        Ok(json["data"]["image_key"].as_str().unwrap_or("").to_string())
    }

    // ── ID mapping ──

    /// Deterministically hash a Feishu string ID to i64.
    ///
    /// Uses FNV-1a so the output is stable across process restarts.
    /// `DefaultHasher` (SipHash with random seed) would produce different
    /// values on every process start, breaking persisted state.
    fn hash_id(id: &str) -> i64 {
        let mut hash: u64 = 0xcbf29ce484222325; // FNV offset basis
        for byte in id.bytes() {
            hash ^= byte as u64;
            hash = hash.wrapping_mul(0x100000001b3); // FNV prime
        }
        // Clear sign bit so the result is always non-negative
        (hash & i64::MAX as u64) as i64
    }

    /// Register a Feishu open_id and return its stable i64 UserId.
    async fn register_user(&self, open_id: &str) -> UserId {
        let uid = Self::hash_id(open_id);
        {
            let mut map = self.id_map.write().await;
            map.user_ids
                .entry(uid)
                .or_insert_with(|| open_id.to_string());
        }
        self.save_id_map().await;
        UserId(uid)
    }

    /// Register a Feishu chat_id and return its stable i64 ChatId.
    async fn register_chat(&self, chat_id: &str) -> ChatId {
        let cid = Self::hash_id(chat_id);
        {
            let mut map = self.id_map.write().await;
            map.chat_ids
                .entry(cid)
                .or_insert_with(|| chat_id.to_string());
        }
        self.save_id_map().await;
        ChatId(cid)
    }

    /// Register a Feishu topic root_id and return its stable i64 thread_id.
    async fn register_thread(&self, root_id: &str) -> ThreadId {
        let tid = Self::hash_id(root_id);
        {
            let mut map = self.id_map.write().await;
            map.thread_ids
                .entry(tid)
                .or_insert_with(|| root_id.to_string());
        }
        self.save_id_map().await;
        ThreadId(tid)
    }

    /// Register a Feishu group chat as a thread-like entity, producing a
    /// thread_id that is distinct from the chat_id.
    async fn register_chat_thread(&self, chat_id: &str) -> ThreadId {
        let threaded_id = format!("\0thread\0{chat_id}");
        let tid = Self::hash_id(&threaded_id);
        {
            let mut map = self.id_map.write().await;
            map.thread_ids
                .entry(tid)
                .or_insert_with(|| chat_id.to_string());
        }
        self.save_id_map().await;
        ThreadId(tid)
    }

    /// Look up the original Feishu root_id for a ThreadId.
    async fn get_thread_root_id(&self, tid: &ThreadId) -> Option<String> {
        let map = self.id_map.read().await;
        map.thread_ids.get(&tid.0).cloned()
    }

    /// Look up the original Feishu chat_id for a ChatId.
    async fn resolve_chat(&self, cid: &ChatId) -> Option<String> {
        let map = self.id_map.read().await;
        map.chat_ids.get(&cid.0).cloned()
    }

    /// Fetch the group chat name from Feishu API, caching the result.
    ///
    /// Returns `None` if the API call fails or the name is empty.
    async fn fetch_chat_name(&self, chat_id: &str) -> Option<String> {
        // Check cache first
        {
            let cache = self.chat_names.read().await;
            if let Some(name) = cache.get(chat_id) {
                return Some(name.clone());
            }
        }

        // Fetch from API
        let path = format!("/im/v1/chats/{chat_id}");
        match self.api_get(&path).await {
            Ok(data) => {
                let name = data["name"].as_str().unwrap_or("").to_string();
                if !name.is_empty() {
                    let mut cache = self.chat_names.write().await;
                    cache.insert(chat_id.to_string(), name.clone());
                    Some(name)
                } else {
                    None
                }
            }
            Err(e) => {
                tracing::warn!("Failed to fetch chat name for {chat_id}: {e}");
                None
            }
        }
    }

    /// Add thread_id to a request body if the target has one.
    async fn add_thread_id(&self, body: &mut serde_json::Value, target: &MessageTarget) {
        if let Some(ref thread_id) = target.thread_id
            && let Some(root_id) = self.get_thread_root_id(thread_id).await {
                body["thread_id"] = serde_json::json!(root_id);
            }
    }

    /// Persist the current id_map to disk.
    async fn save_id_map(&self) {
        let map = self.id_map.read().await;
        let data = serde_json::json!({
            "user_ids": map.user_ids,
            "chat_ids": map.chat_ids,
            "thread_ids": map.thread_ids,
        });
        let json = serde_json::to_string_pretty(&data).unwrap_or_default();
        // Use std::fs for simplicity (called infrequently)
        let _ = std::fs::create_dir_all(self.id_map_file.parent().unwrap_or(&self.id_map_file));
        let _ = std::fs::write(&self.id_map_file, &json);
    }

    /// Load id_map from disk synchronously (called once at construction).
    fn load_id_map_sync(&self) {
        let data = match std::fs::read_to_string(&self.id_map_file) {
            Ok(d) => d,
            Err(_) => return,
        };
        let parsed: serde_json::Value = match serde_json::from_str(&data) {
            Ok(v) => v,
            Err(_) => return,
        };
        let mut map = match self.id_map.try_write().ok() {
            Some(m) => m,
            None => return,
        };
        if let Some(users) = parsed["user_ids"].as_object() {
            for (k, v) in users {
                if let (Some(k), Some(v)) = (k.parse::<i64>().ok(), v.as_str()) {
                    map.user_ids.entry(k).or_insert_with(|| v.to_string());
                }
            }
        }
        if let Some(chats) = parsed["chat_ids"].as_object() {
            for (k, v) in chats {
                if let (Some(k), Some(v)) = (k.parse::<i64>().ok(), v.as_str()) {
                    map.chat_ids.entry(k).or_insert_with(|| v.to_string());
                }
            }
        }
        if let Some(threads) = parsed["thread_ids"].as_object() {
            for (k, v) in threads {
                if let (Some(k), Some(v)) = (k.parse::<i64>().ok(), v.as_str()) {
                    map.thread_ids.entry(k).or_insert_with(|| v.to_string());
                }
            }
        }
    }

    /// Fetch bot info (app name, open_id) from Feishu API.
    async fn get_bot_info(&self) -> Result<serde_json::Value> {
        self.api_get("/bot/v3/info").await
    }

    /// Process a raw WS event payload (schema 2.0).
    async fn handle_raw_payload(&self, payload: &[u8]) -> Result<()> {
        let value: serde_json::Value = serde_json::from_slice(payload)
            .map_err(|e| Error::Feishu(format!("payload JSON: {e}")))?;

        let event_type = value["header"]["event_type"].as_str().unwrap_or("");
        if event_type == "card.action.trigger" {
            tracing::debug!(
                "Card payload top keys: {:?}",
                value.as_object().map(|o| o.keys().collect::<Vec<_>>())
            );
            tracing::debug!(
                "Card payload event: {}",
                &value["event"]
                    .to_string()
                    .chars()
                    .take(500)
                    .collect::<String>()
            );
        }
        match event_type {
            "im.message.receive_v1" => handle_message_event(self, &value).await,
            "card.action.trigger" => handle_card_action(self, &value).await,
            _ => {
                tracing::debug!("Unhandled Feishu event type: {event_type}");
                Ok(())
            }
        }
    }
}

// ── Token refresh via direct API call ──

impl TokenCache {
    async fn refresh(&mut self) -> Result<()> {
        let url = format!(
            "{}/open-apis/auth/v3/app_access_token/internal",
            FEISHU_BASE_URL
        );
        let body = serde_json::json!({
            "app_id": self.app_id,
            "app_secret": self.app_secret,
        });

        let resp = reqwest::Client::new()
            .post(&url)
            .json(&body)
            .send()
            .await
            .map_err(|e| Error::Feishu(format!("token request failed: {e}")))?;

        let value: serde_json::Value = resp
            .json()
            .await
            .map_err(|e| Error::Feishu(format!("token response parse: {e}")))?;

        let code = value["code"].as_i64().unwrap_or(-1);
        if code != 0 {
            let msg = value["msg"].as_str().unwrap_or("unknown");
            return Err(Error::Feishu(format!(
                "token API error: code={code} msg={msg}"
            )));
        }

        // Feishu returns app_access_token at top level (not nested under "data")
        let token = value["app_access_token"]
            .as_str()
            .ok_or_else(|| Error::Feishu("token response missing app_access_token".into()))?
            .to_string();
        let expire = value["expire"].as_i64().unwrap_or(7200) as u64;

        self.token = token;
        self.expires_at = Instant::now() + Duration::from_secs(expire.saturating_sub(300));

        tracing::info!("Feishu token refreshed, expires in {expire}s");
        Ok(())
    }
}

// ── ImAdapter implementation ──

#[async_trait]
impl ImAdapter for FeishuAdapter {
    async fn run(&self, tx: mpsc::UnboundedSender<ImEvent>) -> Result<()> {
        // Store the event sender so WS event handlers can emit events
        {
            let mut stored = self.event_tx.write().await;
            *stored = Some(tx);
        }

        tracing::info!("Feishu adapter starting (openlark WebSocket)");

        // Fetch bot's own info for @-mention detection
        match self.get_bot_info().await {
            Ok(info) => {
                tracing::debug!("Feishu bot info response: {info}");
                if let Some(open_id) = info["app_open_id"].as_str() {
                    *self.bot_open_id.write().await = Some(open_id.to_string());
                    tracing::info!("Feishu bot open_id: {open_id}");
                } else {
                    tracing::warn!("Feishu bot info response missing app_open_id");
                }
                if let Some(name) = info["app_name"].as_str() {
                    *self.bot_name.write().await = Some(name.to_string());
                    tracing::info!("Feishu bot name: {name}");
                } else {
                    tracing::warn!("Feishu bot info response missing app_name");
                }
            }
            Err(e) => {
                tracing::error!("Failed to fetch Feishu bot info: {e}");
            }
        }

        // Reconnection loop: keep the adapter alive across disconnects
        let mut backoff = 1u64;
        loop {
            let (payload_tx, payload_rx) = mpsc::unbounded_channel::<Vec<u8>>();

            let event_handler = EventDispatcherHandler::builder()
                .payload_sender(payload_tx)
                .build();

            // Spawn payload processing in background task
            let this = self.clone();
            tokio::spawn(async move {
                Self::payload_loop(this, payload_rx).await;
            });

            // Build openlark config for WS connection
            let cfg = Config::builder()
                .app_id(&self.app_id)
                .app_secret(&self.app_secret)
                .base_url(FEISHU_BASE_URL)
                .timeout(Duration::from_secs(30))
                .build()
                .map_err(|e| Error::Feishu(format!("config build: {e}")))?;

            tracing::info!("Connecting to Feishu WS via openlark");
            let result = LarkWsClient::open(Arc::new(cfg), event_handler).await;

            match result {
                Ok(()) => {
                    backoff = 1;
                    tracing::info!("Feishu WS disconnected gracefully, reconnecting");
                }
                Err(e) => {
                    tracing::error!("Feishu WS error: {e}, reconnecting in {backoff}s");
                    backoff = backoff.saturating_mul(2).min(60);
                }
            }

            // If the event channel is closed, the server is shutting down
            if self
                .event_tx
                .read()
                .await
                .as_ref()
                .map(|tx| tx.is_closed())
                .unwrap_or(true)
            {
                tracing::info!("Feishu adapter shutting down");
                return Ok(());
            }

            tokio::time::sleep(Duration::from_secs(backoff)).await;
        }
    }

    async fn send_message(&self, target: &MessageTarget, text: &str) -> Result<MessageId> {
        let chat_id = self
            .resolve_chat(&target.chat_id)
            .await
            .ok_or_else(|| Error::Feishu("unknown chat_id".into()))?;

        // Send as interactive card so Feishu renders markdown and future edits
        // can use PATCH (instead of recall+resend).
        let card = serde_json::json!({
            "config": { "wide_screen_mode": true },
            "elements": [
                { "tag": "markdown", "content": text },
            ],
        });

        let mut body = serde_json::json!({
            "receive_id": chat_id,
            "msg_type": "interactive",
            "content": serde_json::to_string(&card)
                .map_err(|e| Error::Feishu(format!("card serialization: {e}")))?,
        });

        self.add_thread_id(&mut body, target).await;

        let data = self
            .api_post("/im/v1/messages?receive_id_type=chat_id", &body)
            .await?;
        let msg_id = data["message_id"].as_str().unwrap_or("").to_string();
        Ok(MessageId(msg_id))
    }

    async fn edit_message(
        &self,
        target: &MessageTarget,
        msg_id: &MessageId,
        text: &str,
    ) -> Result<()> {
        let chat_id = self
            .resolve_chat(&target.chat_id)
            .await
            .ok_or_else(|| Error::Feishu("unknown chat_id".into()))?;

        let card = serde_json::json!({
            "config": { "wide_screen_mode": true },
            "elements": [
                { "tag": "markdown", "content": text },
            ],
        });
        let card_str = serde_json::to_string(&card)
            .map_err(|e| Error::Feishu(format!("card serialization: {e}")))?;

        // Try PATCH in-place first (works on card messages we sent earlier).
        // Only sends `content` per Feishu PATCH API spec.
        let token = self.get_token().await?;
        let url = format!("{FEISHU_BASE_URL}/open-apis/im/v1/messages/{}", msg_id.0);
        let patch_body = serde_json::json!({ "content": &card_str });
        let resp = self
            .client
            .patch(&url)
            .header("Authorization", format!("Bearer {token}"))
            .json(&patch_body)
            .send()
            .await
            .map_err(|e| Error::Feishu(format!("HTTP error: {e}")))?;

        let json: serde_json::Value = resp
            .json()
            .await
            .map_err(|e| Error::Feishu(format!("JSON decode: {e}")))?;

        if json["code"].as_i64().unwrap_or(-1) == 0 {
            return Ok(());
        }

        // PATCH failed — try delete+resend as fallback, but only if delete is enabled
        let msg = json["msg"].as_str().unwrap_or("unknown");
        tracing::warn!("edit PATCH failed ({msg}), trying delete+resend");
        if Self::delete_enabled() {
            self.api_delete(&format!("/im/v1/messages/{}", msg_id.0))
                .await
                .ok();
            let mut post_body = serde_json::json!({
                "receive_id": chat_id,
                "msg_type": "interactive",
                "content": &card_str,
            });
            self.add_thread_id(&mut post_body, target).await;
            self.api_post("/im/v1/messages?receive_id_type=chat_id", &post_body)
                .await?;
            Ok(())
        } else {
            Err(Error::Feishu(format!("edit PATCH failed ({msg})")))
        }
    }

    async fn send_photo(
        &self,
        target: &MessageTarget,
        filename: &str,
        data: &[u8],
    ) -> Result<MessageId> {
        let chat_id = self
            .resolve_chat(&target.chat_id)
            .await
            .ok_or_else(|| Error::Feishu("unknown chat_id".into()))?;

        let image_key = self.upload_image(data, filename).await?;
        if image_key.is_empty() {
            return Err(Error::Feishu("image upload returned empty key".into()));
        }

        let content = serde_json::json!({"image_key": image_key});
        let mut body = serde_json::json!({
            "receive_id": chat_id,
            "msg_type": "image",
            "content": content.to_string(),
        });
        self.add_thread_id(&mut body, target).await;

        let data = self
            .api_post("/im/v1/messages?receive_id_type=chat_id", &body)
            .await?;
        let msg_id = data["message_id"].as_str().unwrap_or("").to_string();
        Ok(MessageId(msg_id))
    }

    async fn send_keyboard(
        &self,
        target: &MessageTarget,
        text: &str,
        buttons: &[Vec<Button>],
    ) -> Result<MessageId> {
        let chat_id = self
            .resolve_chat(&target.chat_id)
            .await
            .ok_or_else(|| Error::Feishu("unknown chat_id".into()))?;

        tracing::debug!(
            "send_keyboard to chat_id={chat_id} target_chat={:?} target_thread={:?} text={}",
            target.chat_id.0,
            target.thread_id,
            &text[..text.floor_char_boundary(text.len().min(50))]
        );

        let card = build_card(text, buttons);

        let mut body = serde_json::json!({
            "receive_id": chat_id,
            "msg_type": "interactive",
            "content": serde_json::to_string(&card)
                .map_err(|e| Error::Feishu(format!("card serialization: {e}")))?,
        });
        self.add_thread_id(&mut body, target).await;

        let data = self
            .api_post("/im/v1/messages?receive_id_type=chat_id", &body)
            .await?;
        let msg_id = data["message_id"].as_str().unwrap_or("").to_string();
        Ok(MessageId(msg_id))
    }

    async fn delete_message(&self, _target: &MessageTarget, msg_id: &MessageId) -> Result<()> {
        if Self::delete_enabled() {
            match self
                .api_delete(&format!("/im/v1/messages/{}", msg_id.0))
                .await
            {
                Ok(_) => tracing::debug!("Feishu message {} recalled", msg_id.0),
                Err(e) => tracing::warn!("Feishu message recall failed: {e}"),
            }
        } else {
            tracing::debug!(
                "Feishu delete_message skipped (ATIM_FEISHU_ENABLE_DELETE not set, msg_id={})",
                msg_id.0
            );
        }
        Ok(())
    }

    async fn edit_keyboard(
        &self,
        target: &MessageTarget,
        msg_id: &MessageId,
        buttons: &[Vec<Button>],
    ) -> Result<()> {
        let _chat_id = self
            .resolve_chat(&target.chat_id)
            .await
            .ok_or_else(|| Error::Feishu("unknown chat_id".into()))?;

        let card = build_card("(updated)", buttons);
        let body = serde_json::json!({
            "content": serde_json::to_string(&card)
                .map_err(|e| Error::Feishu(format!("card serialization: {e}")))?,
        });

        let token = self.get_token().await?;
        let url = format!("{FEISHU_BASE_URL}/open-apis/im/v1/messages/{}", msg_id.0);
        let resp = self
            .client
            .patch(&url)
            .header("Authorization", format!("Bearer {token}"))
            .json(&body)
            .send()
            .await
            .map_err(|e| Error::Feishu(format!("HTTP error: {e}")))?;

        let json: serde_json::Value = resp
            .json()
            .await
            .map_err(|e| Error::Feishu(format!("JSON decode: {e}")))?;

        if json["code"].as_i64().unwrap_or(-1) != 0 {
            let msg = json["msg"].as_str().unwrap_or("unknown");
            tracing::warn!(
                "Feishu edit_keyboard PATCH failed ({msg}), falling back to new message"
            );
            let _ = self.send_keyboard(target, "(updated)", buttons).await?;
        }
        Ok(())
    }

    async fn answer_callback(&self, _callback_query_id: &str, _text: &str) -> Result<()> {
        // Feishu card actions don't have a direct "answer" mechanism.
        Ok(())
    }

    async fn send_chat_action(&self, target: &MessageTarget) -> Result<()> {
        let chat_id = self
            .resolve_chat(&target.chat_id)
            .await
            .ok_or_else(|| Error::Feishu("unknown chat_id".into()))?;

        let token = self.get_token().await?;
        let url = format!(
            "{FEISHU_BASE_URL}/open-apis/im/v1/messages?container_id_type=chat&container_id={chat_id}&page_size=1"
        );
        let resp = self
            .client
            .get(&url)
            .header("Authorization", format!("Bearer {token}"))
            .send()
            .await
            .map_err(|e| Error::Feishu(format!("chat probe error: {e}")))?;

        let json: serde_json::Value = resp
            .json()
            .await
            .map_err(|e| Error::Feishu(format!("JSON decode: {e}")))?;

        let code = json["code"].as_i64().unwrap_or(-1);
        if code != 0 {
            let msg = json["msg"].as_str().unwrap_or("unknown");
            return Err(Error::Feishu(format!("chat probe error ({code}): {msg}")));
        }
        Ok(())
    }
}

// ── Payload processing loop ──

impl FeishuAdapter {
    /// Background task: drain raw WS payloads and dispatch to event handlers.
    async fn payload_loop(
        adapter: FeishuAdapter,
        mut payload_rx: mpsc::UnboundedReceiver<Vec<u8>>,
    ) {
        while let Some(payload) = payload_rx.recv().await {
            if let Err(e) = adapter.handle_raw_payload(&payload).await {
                tracing::error!("Feishu payload handler: {e}");
            }
        }
    }
}

// ── Image download ──

impl FeishuAdapter {
    /// Download an image by its image_key from Feishu.
    async fn download_image(&self, image_key: &str) -> Result<Vec<u8>> {
        let token = self.get_token().await?;
        let url = format!("{FEISHU_BASE_URL}/open-apis/im/v1/images/{image_key}/download");
        let resp = self
            .client
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

        let bytes = resp
            .bytes()
            .await
            .map_err(|e| Error::Feishu(format!("image read error: {e}")))?;
        Ok(bytes.to_vec())
    }
}

impl Clone for FeishuAdapter {
    fn clone(&self) -> Self {
        Self {
            app_id: self.app_id.clone(),
            app_secret: self.app_secret.clone(),
            client: self.client.clone(),
            token_cache: self.token_cache.clone(),
            id_map: self.id_map.clone(),
            id_map_file: self.id_map_file.clone(),
            event_tx: self.event_tx.clone(),
            bot_open_id: self.bot_open_id.clone(),
            bot_name: self.bot_name.clone(),
            chat_names: self.chat_names.clone(),
        }
    }
}

// ── Event handlers ──

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
    let msg_type = message["message_type"]
        .as_str()
        .or_else(|| message["msg_type"].as_str())
        .unwrap_or("");
    let _message_id = message["message_id"].as_str().unwrap_or("");
    let chat_type = message["chat_type"].as_str().unwrap_or("p2p");
    let root_id = message["root_id"].as_str(); // thread root (if any)

    tracing::debug!("Feishu message: type={msg_type} chat={chat_type} from={open_id}");

    let user_id = adapter.register_user(open_id).await;
    let chat_id_atim = adapter.register_chat(chat_id).await;

    // For topic/thread messages, use root_id as thread_id (consistent per-topic routing).
    // For non-topic group chats, use a thread_id derived from chat_id but
    // distinct from it, so the binding model has a unique (chat_id, thread_id)
    // pair per group without conflating the two dimensions.
    let thread_id = if let Some(root_id) = root_id {
        Some(adapter.register_thread(root_id).await)
    } else if chat_type == "group" {
        Some(adapter.register_chat_thread(chat_id).await)
    } else {
        None
    };

    let chat_name = if chat_type == "group" {
        adapter.fetch_chat_name(chat_id).await
    } else {
        None
    };
    let target = MessageTarget {
        chat_id: chat_id_atim,
        thread_id,
        chat_name: chat_name.clone(),
    };

    // For group chats, fetch the chat name and emit TopicCreated so the
    // server can use it as the window/topic name (e.g. "atim" instead of
    // "atim-<user_id>").
    if let Some(ref cn) = chat_name {
            let topic_target = MessageTarget {
                chat_id: chat_id_atim,
                thread_id,
                chat_name: Some(cn.clone()),
            };
            if let Some(tx) = adapter.event_tx.read().await.as_ref() {
                let _ = tx.send(ImEvent {
                    user_id,
                    target: topic_target,
                    kind: ImEventKind::TopicCreated { name: cn.clone() },
                });
            }
        }

    let content_str = message["content"].as_str().unwrap_or("{}");
    tracing::debug!(
        "Feishu received: root_id={:?} chat_type={chat_type} msg_type={msg_type} chat_id={chat_id} from={open_id} raw_content={}",
        root_id.unwrap_or("(none)"),
        content_str,
    );

    let parsed_content: serde_json::Value =
        serde_json::from_str(content_str).unwrap_or_default();

    let mut text = String::new();
    let mut has_mention = false;

    // Check for @-mention of the bot in group chats (shared by text/post types)
    if chat_type == "group" {
        let bot_id = adapter.bot_open_id.read().await;
        let bot_name = adapter.bot_name.read().await;
        let mentions = message["mentions"].as_array();
        if let Some(mentions) = mentions {
            for mention in mentions {
                // Check by bot open_id (preferred)
                let is_bot = match bot_id.as_ref() {
                    Some(bid) => mention["id"]["open_id"].as_str() == Some(bid),
                    None => false,
                };
                // Fallback: check by name
                let is_bot = is_bot
                    || bot_name.as_ref().is_some_and(|name| {
                        mention["name"]
                            .as_str()
                            .is_some_and(|n| n.contains(name) || name.contains(n))
                    });
                // Broader fallback: any mention named "bot" or the app_id prefix
                let is_bot = is_bot
                    || mention["name"]
                        .as_str()
                        .is_some_and(|n| n.contains("bot") || adapter.app_id.contains(n));
                if is_bot {
                    has_mention = true;
                }
            }
        }
        // Permissive fallback: if mentions array exists and is non-empty but we
        // couldn't match the bot's identity, treat any mention as a bot mention
        if !has_mention && message["mentions"].as_array().map_or(0, |m| m.len()) > 0 {
            tracing::debug!(
                "Mentions present but no bot match — permissive fallback, treating as @mention"
            );
            has_mention = true;
        }
    }

    let kind = match msg_type {
        "text" => {
            let raw = parsed_content["text"].as_str().unwrap_or("").to_string();
            text = raw.clone();
            // Strip mention placeholder from text
            if let Some(mentions) = message["mentions"].as_array() {
                for mention in mentions {
                    if let Some(key) = mention["key"].as_str() {
                        text = text.replace(key, "").trim().to_string();
                    }
                }
            }
            ImEventKind::Text {
                text: text.clone(),
                is_mention: has_mention,
                is_group: chat_type == "group",
            }
        }
        "post" => {
            // Rich text: extract from post content.
            // Feishu post content has multiple possible formats:
            //   Format 1 (locale-wrapped):
            //     {"post": {"zh_cn": {"title": "...", "content": [[...]]}}}
            //   Format 2 (direct, no locale wrapper):
            //     {"title": "", "content": [[...]]}
            //
            // Different Feishu clients send different formats, so we try
            // both.
            let post_obj = &parsed_content["post"];
            let locale = if post_obj["zh_cn"].is_object() {
                "zh_cn"
            } else if post_obj["en_us"].is_object() {
                "en_us"
            } else if post_obj["ja_jp"].is_object() {
                "ja_jp"
            } else {
                ""
            };

            if !locale.is_empty() {
                text = extract_post_text(&post_obj[locale]);
            } else if parsed_content["content"].is_array() {
                // Format 2: direct content without locale wrapper
                text = extract_post_text(&parsed_content);
            }
            ImEventKind::Text {
                text: text.clone(),
                is_mention: has_mention,
                is_group: chat_type == "group",
            }
        }
        "image" => {
            let image_key = parsed_content["image_key"].as_str().unwrap_or("");
            let data = adapter.download_image(image_key).await.unwrap_or_default();
            ImEventKind::Photo {
                caption: None,
                data,
                mime_type: "image/png".into(),
            }
        }
        _ => {
            tracing::debug!("Skipping unknown msg_type: {msg_type}");
            return Ok(()); // skip audio, file, sticker, etc.
        }
    };

    tracing::debug!(
        "Feishu emit: user_id={user_id:?} msg_type={msg_type} chat_type={chat_type} has_mention={has_mention} text_len={} text_preview={:?}",
        text.len(),
        &text.chars().take(100).collect::<String>(),
    );
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

    // WS mode: fields are nested under operator/context, not at event top level.
    // Webhook mode: fields are at event top level.
    let operator = &event["operator"];
    let context = &event["context"];

    let open_id = operator["open_id"]
        .as_str()
        .filter(|s| !s.is_empty())
        .or_else(|| event["open_id"].as_str())
        .unwrap_or("");

    let chat_id = context["open_chat_id"]
        .as_str()
        .filter(|s| !s.is_empty())
        .or_else(|| event["chat_id"].as_str())
        .unwrap_or(open_id); // fallback to open_id for p2p

    let message_id = context["open_message_id"]
        .as_str()
        .filter(|s| !s.is_empty())
        .or_else(|| event["message_id"].as_str())
        .unwrap_or("");

    // Determine chat_type from explicit field or infer from chat_id prefix
    let explicit_chat_type = event["chat_type"].as_str().unwrap_or("p2p");
    let is_group = explicit_chat_type == "group" || chat_id.starts_with("oc_");
    let chat_type = if is_group { "group" } else { "p2p" };

    tracing::debug!(
        "Card action event: open_id={open_id} chat_id={chat_id} chat_type={chat_type} msg_id={message_id}"
    );

    // Feishu card action values are JSON objects — extract the "action" field
    let data = match action_value["action"].as_str() {
        Some(a) => a.to_string(),
        None => action_value["data"]
            .as_str()
            .map(String::from)
            .unwrap_or_else(|| serde_json::to_string(action_value).unwrap_or_default()),
    };
    tracing::debug!("Card action: data={data} chat_type={chat_type}");

    let user_id = adapter.register_user(open_id).await;

    // Check for thread_id in the card action context (Feishu may include it
    // for messages sent to topic threads), or root_id in the event envelope.
    let root_id = event["root_id"].as_str().filter(|s| !s.is_empty());
    let ctx_thread_id = context["thread_id"].as_str().filter(|s| !s.is_empty());

    let thread_id = if let Some(tid) = ctx_thread_id.or(root_id) {
        Some(adapter.register_thread(tid).await)
    } else if is_group {
        // Fallback: derive a thread_id from chat_id that is distinct from
        // the chat_id itself, so the binding model has a unique (chat_id,
        // thread_id) pair per group without conflating the two dimensions.
        Some(adapter.register_chat_thread(chat_id).await)
    } else {
        None
    };

    let target = MessageTarget {
        chat_id: adapter.register_chat(chat_id).await,
        thread_id,
        chat_name: None,
    };

    let action_token = event["action"]["token"]
        .as_str()
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

// ── Card builder ──

/// Build a Feishu interactive card JSON for permission prompts / choices.
fn build_card(text: &str, buttons: &[Vec<Button>]) -> serde_json::Value {
    let mut elements: Vec<serde_json::Value> = vec![serde_json::json!({
        "tag": "markdown",
        "content": text,
    })];

    for row in buttons {
        let actions: Vec<serde_json::Value> = row
            .iter()
            .map(|btn| {
                let btn_type = if btn.callback_data.contains("approve")
                    || btn.callback_data.contains("yes")
                    || btn.callback_data.contains("confirm")
                {
                    "primary"
                } else if btn.callback_data.contains("reject") || btn.callback_data.contains("no") {
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
                "content": "Atim — Agent Response",
            },
            "template": "blue",
        },
        "elements": elements,
    })
}

/// Extract plain text from a Feishu `post` rich-text block.
fn extract_post_content_text(content: &serde_json::Value) -> String {
    let mut result = String::new();

    if let Some(title) = content["title"].as_str()
        && !title.is_empty() {
            result.push_str(title);
            result.push('\n');
        }

    if let Some(paragraphs) = content["content"].as_array() {
        for (i, para) in paragraphs.iter().enumerate() {
            if i > 0 {
                result.push('\n');
            }
            if let Some(elements) = para.as_array() {
                for el in elements {
                    let tag = el["tag"].as_str().unwrap_or("");
                    match tag {
                        "text" => {
                            if let Some(t) = el["text"].as_str() {
                                result.push_str(t);
                            }
                        }
                        "a" => {
                            if let Some(t) = el["text"].as_str() {
                                result.push_str(t);
                            }
                            if let Some(href) = el["href"].as_str() {
                                result.push_str(&format!(" ({href})"));
                            }
                        }
                        "at" => {
                            if let Some(name) = el["user_name"].as_str() {
                                result.push('@');
                                result.push_str(name);
                            } else if let Some(open_id) = el["user_id"].as_str() {
                                result.push_str(&format!("@user:{open_id}"));
                            }
                        }
                        "md" => {
                            if let Some(t) = el["text"].as_str() {
                                result.push_str(t);
                            }
                        }
                        "img" => {
                            result.push_str(" [image] ");
                        }
                        _ => {}
                    }
                }
            }
        }
    }

    result
}

/// Shorthand wrapper for the helper above.
fn extract_post_text(post_content: &serde_json::Value) -> String {
    extract_post_content_text(post_content)
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
    fn test_hash_id_unique_per_input() {
        let h1 = FeishuAdapter::hash_id("ou_a");
        let h2 = FeishuAdapter::hash_id("ou_b");
        assert_ne!(h1, h2);
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
        assert_eq!(card["header"]["title"]["content"], "Atim — Agent Response");
        assert_eq!(card["elements"].as_array().unwrap().len(), 3);

        let actions_row0 = card["elements"][1]["actions"].as_array().unwrap();
        assert_eq!(actions_row0[0]["text"]["content"], "Yes");
        assert_eq!(actions_row0[0]["type"], "primary");
        let actions_row1 = card["elements"][2]["actions"].as_array().unwrap();
        assert_eq!(actions_row1[0]["text"]["content"], "No");
        assert_eq!(actions_row1[0]["type"], "danger");
    }

    #[test]
    fn test_extract_post_content_text_simple() {
        let content = serde_json::json!({
            "title": "Note",
            "content": [[
                { "tag": "text", "text": "Hello, world!" }
            ]]
        });
        let result = extract_post_content_text(&content);
        assert!(result.contains("Hello, world!"));
        assert!(result.contains("Note"));
    }

    #[test]
    fn test_extract_post_content_text_with_link() {
        let content = serde_json::json!({
            "title": "",
            "content": [[
                { "tag": "text", "text": "Check out " },
                { "tag": "a", "text": "this link", "href": "https://example.com" }
            ]]
        });
        let result = extract_post_content_text(&content);
        assert!(result.contains("Check out"));
        assert!(result.contains("this link"));
        assert!(result.contains("example.com"));
    }

    #[test]
    fn test_extract_post_content_text_with_mention() {
        let content = serde_json::json!({
            "title": "",
            "content": [[
                { "tag": "text", "text": "Hey " },
                { "tag": "at", "user_name": "Alice", "user_id": "ou_xxx" }
            ]]
        });
        let result = extract_post_content_text(&content);
        assert!(result.contains("Hey"));
        assert!(result.contains("Alice"));
    }

    #[test]
    fn test_extract_post_content_text_multi_paragraph() {
        let content = serde_json::json!({
            "title": "",
            "content": [
                [{ "tag": "text", "text": "First paragraph" }],
                [{ "tag": "text", "text": "Second paragraph" }],
            ]
        });
        let result = extract_post_content_text(&content);
        assert!(result.contains("First paragraph"));
        assert!(result.contains("Second paragraph"));
        let lines: Vec<&str> = result.lines().collect();
        assert_eq!(lines.len(), 2);
    }

    #[test]
    fn test_extract_post_content_text_empty() {
        let content = serde_json::json!({});
        let result = extract_post_content_text(&content);
        assert_eq!(result, "");
    }

    #[test]
    fn test_extract_post_content_text_markdown_tag() {
        let content = serde_json::json!({
            "title": "",
            "content": [[
                { "tag": "md", "text": "**bold** and `code`" }
            ]]
        });
        let result = extract_post_content_text(&content);
        assert!(result.contains("bold"));
        assert!(result.contains("code"));
    }

    #[test]
    fn test_extract_post_content_text_image_placeholder() {
        let content = serde_json::json!({
            "title": "",
            "content": [[
                { "tag": "text", "text": "See this: " },
                { "tag": "img", "image_key": "img_xxx" }
            ]]
        });
        let result = extract_post_content_text(&content);
        assert!(result.contains("See this"));
        assert!(result.contains("[image]"));
    }
}
