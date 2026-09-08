# JSONL 解析器优化计划

> 日期：2026-09-04
>
> 基于对 Codex（`openai/codex` codex-rs）和 Claude Code（atim 现有实现）JSONL 格式的调研，形成以下优化计划。

## 当前状态

`atim-parser` 包含两个独立解析器：

| 解析器 | 路径 | 覆盖格式 |
|--------|------|---------|
| `CodexJsonlParser` | `codex_jsonl.rs` | `~/.codex/sessions/.../rollout-*.jsonl` |
| `JsonlParser` | `jsonl.rs` | `~/.claude/projects/.../*.jsonl` |

分发逻辑在 `lib.rs::read_jsonl()`：路径含 `.codex` → CodexJsonlParser，含 `.copilot` → CopilotJsonlParser，否则 JsonlParser。

---

## 优先级 P0 —— 正确性修复（已完成）

### Codex：`CommandExecutionItem` 字段对齐

**已修复**（`codex_jsonl.rs`）：

| 问题 | 根因 | 修复 |
|------|------|------|
| 命令为空 | `command: Vec<String>`，旧代码 `.as_str()` 取空 | `extract_codex_command()`：优先 `parsed_cmd[0].cmd`，回退数组最后一个元素 |
| 输出为空 | 读不存在的 `output` 字段 | `extract_codex_output()`：读 `aggregated_output` → `formatted_output` → `stdout` → `stderr` |
| 成功时省略输出 | 旧设计（匹配 Claude 的 `summarize_tool_result` 行为） | 现在展示截断（3000 字符）+ exit code 标识 |

---

## 优先级 P1 —— 结构性改进

### 1. Codex：支持 `DynamicToolCallItem`

Codex 较新版本引入 `DynamicToolCallItem` 作为通用工具执行（含 MCP 工具），
与 `CommandExecutionItem` 并行。当前 `parse_item_completed` 只处理 `AgentMessage` 和 `CommandExecution`。

```rust
// 新增分支建议（codex_jsonl.rs）
"DynamicToolCallItem" => parse_dynamic_tool_call(item, timestamp),
```

**输出**：工具名 + arguments 截断 + content_items/output 展示。

**影响**：MCP 工具调用现在被静默忽略（`_ => None`），用户看不到 MCP 工具内容。

### 2. Codex：处理 `LocalShellCall`（Responses API 格式）

`ResponseItem::LocalShellCall` 是 Codex 的新 Responses API 格式，结构体不同于 `CommandExecutionItem`：
```rust
LocalShellCall {
    id: Option<ResponseItemId>,
    call_id: Option<String>,
    status: LocalShellStatus,   // InProgress | Completed | ...
    action: LocalShellAction,   // 含 command 字段（字符串？待确认）
}
```

当 Codex 通过 Responses API 运行时，shell 调用走这个变体而非 `CommandExecutionItem`。
当前 `response_item` 行在 `parse_line` 里被忽略（只处理 `event_msg`），需扩展。

### 3. Claude：支持更多工具名的 `summarize_tool_result`

当前 `summarize_tool_result`（`jsonl.rs`）只对 `Bash`/`Read`/`Edit` 三个工具做了特殊处理，
其他工具用 fallback `({N} lines)`。建议扩展：

- `Write`/`WriteTool` → 展示写入的文件路径
- `WebFetch` → 展示 URL + 响应大小
- `Grep`/`GrepTool` → 展示匹配数

### 4. 统一 tool_use/tool_result 缓存策略

Claude 路径：`tool_use_id → tool_name` 在 `tool_names: HashMap` 里缓存（跨批次）。
Codex 路径：`tool_use_id` 直接内联在 `CommandExecutionItem` 上，无缓存需求。
两个策略没有问题，但需确保：

- Claude `ToolResult` 如果 `tool_use_id` 不在缓存里，`tool_name` 为 `None`，fallback 行为要正确。
- Codex 路径的 `tool_name` 是 `"Bash"`（硬编码），正确但未来需注意其他工具类型。

---

## 优先级 P2 —— 健壮性与可观测性

### 5. Codex：多变体覆盖更完整

`parse_item_completed` 当前只匹配 `AgentMessage` 和 `CommandExecution`，其余返回 `None`。
建议对未匹配的 item_type 输出 `tracing::debug` 而非静默忽略，方便排查格式变化：

```rust
other => {
    tracing::debug!("Unknown Codex item type: {other}");
    None
}
```

### 6. Claude：`is_error` 字段利用

Claude 的 `tool_result` 有 `is_error: bool`。当前 `summarize_tool_result` 通过文本匹配 `exit N`，
未利用 `is_error` 字段。建议在 `ParsedEntry` 中增加 `is_error` 信息，或在 text 里反映出来。

### 7. 统一 ParsedEntry 输出截断

`CodexJsonlParser` 自定义了 `MAX_BASH_OUTPUT_CHARS = 3000`，`JsonlParser` 通过 `summarize_tool_result`
生成摘要（不截断原始输出），实际输出的截断在 `recovery.rs` 的 `MAX_MSG_LEN = 3800` 统一处理。
建议将截断逻辑收敛到单一位置（recovery.rs），避免两层截断语义混淆。

---

## 优先级 P3 —— 维护性

### 8. 提取共享工具枚举

`jsonl.rs` 和 `codex_jsonl.rs` 各自用字符串匹配工具名（`"Bash"`, `"Read"` 等）。
建议在 `atim-core/src/message.rs` 或 `lib.rs` 中定义：

```rust
pub const TOOL_BASH: &str = "Bash";
pub const TOOL_READ: &str = "Read";
pub const TOOL_EDIT: &str = "Edit";
// ...
```

统一引用，避免拼写遗漏。

### 9. 解析器错误上报

当前两个解析器对畸形 JSON 行都是 `None`（静默跳过）。建议在 `parse_str` 里记录
跳过计数，上报到 tracing，方便排查数据问题：

```rust
let skipped = total_lines - entries.len();
if skipped > 0 {
    tracing::debug!("Skipped {skipped}/{total_lines} lines in JSONL parse");
}
```

---

## 测试计划

| 场景 | 测试类型 | 来源 |
|------|---------|------|
| Codex `CommandExecutionItem` 所有字段 | 单元测试（已覆盖） | `codex_jsonl.rs` tests |
| Codex `DynamicToolCallItem` | 单元测试（待加） | 需真实 fixture |
| Codex `LocalShellCall`（Responses API） | 单元测试（待加） | 需确认 `action.command` 结构 |
| Claude `tool_use`/`tool_result` 完整链路 | 单元测试（已有基础） | `jsonl.rs` tests |
| Claude `is_error` 处理 | 单元测试（待加） | — |
| 真实 rollout 文件端到端 | 集成测试（按需） | `#[ignore]` + 本地文件 |

---

## 依赖项

| 事项 | 说明 |
|------|------|
| `codex-rs/protocol/src/items.rs` | Codex 格式的权威源码，关注 `CommandExecutionItem`、`DynamicToolCallItem`、`LocalShellCall` |
| `codex-rs/protocol/src/models.rs` | `ResponseItem` enum 定义 |
| `codex-rs/history/src/rollout_payload.rs` | `RolloutItemWire` 顶层分发 |
| Anthropic Messages API 规范 | Claude `tool_use`/`tool_result` 结构（无独立 schema 文档，依赖 Messages API docs） |
