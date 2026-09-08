# Codex CLI Rollout JSONL Schema

> Source: `openai/codex` (`codex-rs`), files `protocol/src/items.rs`, `protocol/src/models.rs`, `history/src/rollout_payload.rs`
>
> Canonical URL: https://github.com/openai/codex

## 文件位置

```
~/.codex/sessions/YYYY/MM/DD/rollout-<ISO8601>-<SESSION_ID>.jsonl
```

## 顶层格式

每行一个 JSON，`type` 字段判别类型（`#[serde(tag = "type", rename_all = "snake_case")]`）：

| `type` | 说明 | payload 类型 |
|--------|------|------------|
| `session_meta` | 会话元数据（首行） | `SessionMetaLine` |
| `response_item` | 模型输出（消息、工具调用等） | `ResponseItem` |
| `event_msg` | 事件封装（`item_completed` 等） | `EventMsg` |
| `compacted` | 压缩事件 | `CompactedItem` |
| `turn_context` | 轮次上下文 | `TurnContextItem` |
| `token_usage_record` | token 用量 | `TokenUsageRecord` |
| `world_state` | 环境状态 | `WorldStateItem` |
| `inter_agent_communication` | 多 agent 通信 | — |

## `CommandExecutionItem` — Shell 命令执行

`type: "event_msg"` → `payload.type: "item_completed"` → `item.type: "CommandExecution"`

```rust
pub struct CommandExecutionItem {
    pub id: String,
    pub plugin_id: Option<String>,          // 插件来源
    pub script_path: Option<String>,
    pub process_id: Option<String>,         // OS 进程 PID
    pub command: Vec<String>,               // ⚠️ argv 数组，不是字符串！
    pub cwd: PathUri,                       // "file:///path/to/dir"
    pub parsed_cmd: Vec<ParsedCommand>,     // [{ type, cmd }] 解析后命令
    pub source: ExecCommandSource,          // "unified_exec_startup" 等
    pub interaction_input: Option<String>,  // 交互输入（stdin）
    pub status: CommandExecutionStatus,     // InProgress | Completed | Failed | Declined
    pub stdout: Option<String>,
    pub stderr: Option<String>,
    pub aggregated_output: Option<String>,  // stdout + stderr 合并（首选）
    pub formatted_output: Option<String>,   // 格式化输出（含 ANSI，回退）
    pub exit_code: Option<i32>,
    pub duration: Option<Duration>,         // { secs, nanos }
}
```

**字段获取优先级（输出）**：`aggregated_output` > `formatted_output` > `stdout`。
`formatted_output` 可能含 ANSI 转义；`aggregated_output` 通常是纯文本。

**命令提取**：`parsed_cmd[0].cmd` 是人类可读的命令（如 `pwd && rg --files`）；
`command` 是完整 argv（如 `["/usr/bin/zsh", "-lc", "pwd && rg ..."]`）。

## `ResponseItem` — 模型响应（`response_item` 行）

```rust
pub enum ResponseItem {
    AgentMessage { id, author, recipient, content: Vec<AgentMessageInputContent> },
    Message { id, role, content: Vec<ContentItem>, phase },
    Reasoning { id, summary, content, encrypted_content },
    LocalShellCall { id, call_id, status, action },   // Responses API 的 shell 调用
    DynamicToolCallItem { id, tool, arguments, status, content_items, success, error, duration },
    // ... 其他变体
}
```

`AgentMessage.content` 是 `Vec<AgentMessageInputContent>`，通常每个元素有 `type` + `text`。

## `LocalShellCall` — Responses API 格式的 Shell 调用

较新格式，结构体字段不同于 `CommandExecutionItem`；用于 Responses API 端点。
目前 Codex JSONL 中主要出现 `CommandExecutionItem`，`LocalShellCall` 出现在 `response_item` 行里。

## JSONL 示例（精简）

```json
{"timestamp":"2026-09-04T07:53:42.779Z","ordinal":46,"type":"event_msg","payload":{"type":"item_completed","item":{"type":"CommandExecution","id":"exec-abc","command":["/usr/bin/zsh","-lc","pwd && ls"],"cwd":"file:///home/user/code","parsed_cmd":[{"type":"unknown","cmd":"pwd && ls"}],"status":"completed","stdout":"/home/user/code\nfile1\nfile2\n","stderr":"","aggregated_output":"/home/user/code\nfile1\nfile2\n","exit_code":0,"duration":{"secs":0,"nanos":12345}},"completed_at_ms":1788508422779}}
```

## atim 解析器已知问题与修复记录

| 问题 | 原因 | 修复（`codex_jsonl.rs`） |
|------|------|----------------------|
| 命令显示为空 | `command` 是 `Vec<String>`，旧代码用 `.as_str()` 取 | 优先 `parsed_cmd[0].cmd`，回退数组最后一个元素 |
| 输出为空 | 读不存在的 `output` 字段；成功时本就省略 | 改读 `aggregated_output` → `formatted_output` → `stdout`，成功也展示（截断 3000 字符） |
