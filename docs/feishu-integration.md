# Feishu Integration Design

## Overview

Feishu (Lark) is the second IM backend for Aim. Unlike Telegram's long-polling model, Feishu uses **webhooks** — events are pushed to a public HTTP endpoint.

```
Feishu Open Platform
  │  POST /webhook/feishu (events)
  ▼
axum HTTP server (embedded in Aim)
  │  verify → parse → ImEvent
  ▼
tx channel → Server event loop → tmux agent

Outbound:
Server → FeishuAdapter::send_message()
  │  POST https://open.feishu.cn/open-apis/im/v1/messages
  ▼
Feishu Open Platform API
```

## Key Differences from Telegram

| Aspect | Telegram | Feishu |
|--------|----------|--------|
| Event delivery | Long polling (pull) | Webhook (push) |
| Auth | Bot token | app_id + app_secret → tenant_access_token (2hr) |
| Message ID | Integer (`i32`) | String (`message_id`) |
| User ID | Integer (`i64`) | String (`open_id`) |
| Chat ID | Integer (`i64`) | String (`chat_id`) |
| Interactive UI | InlineKeyboardMarkup | Interactive Cards (JSON template) |
| Text format | MarkdownV2 | Rich text / Markdown (partial) |
| Event verification | None | Challenge-response handshake |
| Image upload | multipart/form-data | formData with file_token |

## Architecture

### New Dependencies (`aim-im/Cargo.toml`)

```toml
axum = "0.8"           # HTTP server for webhook
jsonwebtoken = "9"     # JWT generation for Feishu auth
tower-http = { version = "0.6", features = ["cors"] }  # CORS for dev
```

### ID Mapping

Feishu IDs are strings (`ou_xxx`, `oc_xxx`). To fit Aim's `ChatId(i64)` / `UserId(i64)`:

- **Stable hash**: `hash_string_to_i64(feishu_id) -> i64` using a deterministic hash (e.g. CRC-64 or std::collections::hash_map::DefaultHasher)
- **Reverse lookup**: store `HashMap<i64, String>` in the adapter for outbound API calls
- The mapping is deterministic so it survives restarts without persistence

### Webhook Server

Started inside `FeishuAdapter::run()` on a configurable port (`AIM_FEISHU_PORT`, default 9090).

Routes:
- `POST /webhook/feishu` — receive events + card actions
- `GET /health` — health check

### Token Management

```rust
struct FeishuTokenManager {
    app_id: String,
    app_secret: String,
    token: RwLock<Option<TokenState>>,
}

struct TokenState {
    access_token: String,
    expires_at: Instant,  // refresh 5 min before expiry
}
```

Token is fetched lazily on first API call, then refreshed automatically.

## ImAdapter Method Mapping

| ImAdapter Method | Feishu API | Notes |
|---|---|---|
| `run()` | Start axum server | Blocking call (server loops forever) |
| `send_message()` | `POST /im/v1/messages` | `msg_type: "text"` or `"interactive"` |
| `edit_message()` | `PATCH /im/v1/messages/:id` | Only for card messages |
| `send_photo()` | `POST /im/v1/messages` | Upload image first to get `image_key` |
| `send_keyboard()` | `POST /im/v1/messages` | Send as `msg_type: "interactive"` (card) |
| `delete_message()` | `DELETE /im/v1/messages/:id` | |
| `edit_keyboard()` | `PATCH /im/v1/messages/:id` | Update card content |

### Card Template for Interactive UI

Feishu interactive cards use a JSON template. Example for permission prompts:

```json
{
  "config": { "wide_screen_mode": true },
  "header": {
    "title": { "tag": "plain_text", "content": "Claude Code — Permission Required" },
    "template": "blue"
  },
  "elements": [
    {
      "tag": "markdown",
      "content": "Do you want to make this edit?\n\n**File**: `src/main.rs`"
    },
    {
      "tag": "action",
      "actions": [
        {
          "tag": "button",
          "text": { "tag": "plain_text", "content": "✅ Yes" },
          "value": { "action": "approve" },
          "type": "primary"
        },
        {
          "tag": "button",
          "text": { "tag": "plain_text", "content": "❌ No" },
          "value": { "action": "reject" },
          "type": "danger"
        }
      ]
    }
  ]
}
```

### Inbound Event → ImEventKind

| Feishu Event | ImEventKind |
|---|---|
| `im.message.receive_v1` (text) | `Text(String)` |
| `im.message.receive_v1` (image) | `Photo { caption, data, mime_type }` |
| `card.action.trigger` | `CallbackQuery { data, msg_id }` |

## Thread/Topic Mapping

Feishu does not have Telegram-style forum topics. Strategy:

- **P2P chat**: one chat per user → direct 1:1 with a tmux window
  - `chat_id` = hash of Feishu chat_id
  - `thread_id` = None

- **Group chat**: multi-user → each user gets their own window
  - `chat_id` = hash of Feishu chat_id  
  - `thread_id` = hash of sender's `open_id` (unique per user within the group)
  - Messages are routed by (chat_id, thread_id) → window

## Config (in `aim-core`)

Add to `Config`:

```rust
// ── Feishu ──
pub feishu_app_id: String,
pub feishu_app_secret: String,
pub feishu_webhook_port: u16,
```

Env vars:
- `AIM_FEISHU_APP_ID`
- `AIM_FEISHU_APP_SECRET`
- `AIM_FEISHU_WEBHOOK_PORT` (default `9090`)

## Runtime Wiring

In `aim-bin/src/main.rs`, the IM backend is selected at startup:

```rust
let im_adapter: Box<dyn ImAdapter> = match im_backend {
    "telegram" => Box::new(TelegramAdapter::new(config.telegram_bot_token.clone())),
    "feishu" => Box::new(FeishuAdapter::new(
        config.feishu_app_id.clone(),
        config.feishu_app_secret.clone(),
        config.feishu_webhook_port,
    )),
    _ => return Err("unknown IM backend, use AIM_IM_BACKEND=telegram|feishu"),
};
```

## File Structure

```
crates/aim-im/src/
├── lib.rs           # pub mod feishu;
├── telegram.rs      # existing
└── feishu/
    ├── mod.rs       # FeishuAdapter struct + ImAdapter impl
    ├── token.rs     # TokenManager (fetch, cache, refresh)
    ├── webhook.rs   # axum server, event handlers
    ├── card.rs      # Card message builder
    └── api.rs       # Feishu API client (send_message, etc.)
```

## Implementation Priority

1. Token management + API client
2. Webhook server with event parsing
3. `ImAdapter` impl (all 7 methods)
4. Card builder for interactive UI
5. Wiring into `aim-bin`
6. Voice transcription (same OpenAI pipeline)

## Testing

- Unit tests: token refresh, card building, event parsing
- Integration: `cargo run` with `AIM_IM_BACKEND=feishu`, send a message
- Webhook debug: `ngrok http 9090` for local development
