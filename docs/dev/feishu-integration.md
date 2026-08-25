# Feishu Integration Design

## Overview

Feishu (Lark) is one of Atim's IM backends. Unlike Telegram's long-polling model, Feishu uses **webhooks** — events are pushed to a public HTTP endpoint.

```
Feishu Open Platform
  │  POST /webhook/feishu (events)
  ▼
axum HTTP server (embedded in Atim)
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
| Message ID | Integer (`i64`) | String (`message_id`) |
| User ID | Integer (`i64`) | String (`open_id`) |
| Chat ID | Integer (`i64`) | String (`chat_id`) |
| Interactive UI | InlineKeyboardMarkup | Interactive Cards (JSON template) |
| Text format | HTML | Rich text / Markdown (partial) |
| Event verification | None | Challenge-response handshake |
| Image upload | multipart/form-data | formData with file_token |

## ID Mapping

Feishu IDs are strings (`ou_xxx`, `oc_xxx`). To fit Atim's internal ID types:

- **Stable hash**: `hash_string_to_i64(feishu_id) -> i64` using a deterministic hash
- **Reverse lookup**: store `HashMap<i64, String>` in the adapter for outbound API calls
- The mapping is deterministic so it survives restarts without persistence

## Webhook Server

Started inside `FeishuAdapter::run()` on a configurable port (default 9090).

Routes:
- `POST /webhook/feishu` — receive events + card actions
- `GET /health` — health check

## Token Management

Token is fetched lazily on first API call, then refreshed automatically 5 minutes before expiry.

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

## Thread/Topic Mapping

Feishu does not have Telegram-style forum topics. Strategy:

- **P2P chat**: one chat per user → direct 1:1 with a tmux window
- **Group chat**: multi-user → each user gets their own window within the group
- **Topic group chat**: each topic maps to its own tmux window (recommended)

## Group Chat Behavior

In group chats, the bot only responds when @-mentioned, or when the message starts with `/` or `!`.

## Config

| Env Var | Default | Description |
|---------|---------|-------------|
| `ATIM_FEISHU_APP_ID` | — | Feishu app ID |
| `ATIM_FEISHU_APP_SECRET` | — | Feishu app secret |
| `ATIM_FEISHU_WEBHOOK_PORT` | `9090` | Webhook HTTP server port |
| `ATIM_IM_BACKEND` | `telegram` | Set to `feishu` to enable |

## Testing

- Unit tests: token refresh, card building, event parsing
- Integration: `cargo run` with `ATIM_IM_BACKEND=feishu`, send a message
- Webhook debug: `ngrok http 9090` for local development
