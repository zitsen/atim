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

**已完成**（`codex_jsonl.rs`，commit `5477799`）

```rust
"DynaractionItem" => ...  // 含 namespace::tool 显示 + content_items 提取
```

### 2. Codex：处理 `LocalShellCall`（Responses API 格式）

**已完成**（`codex_jsonl.rs`，commit `5477799`）

`parse_line` 新增 `response_item` 分支，`parse_response_item` 处理：
- `local_shell_call`：`action.type == "exec"` → 提取命令，状态标识 ✅/❌
- `agent_message`：复用 `extract_text_from_content`
- 其他类型 `tracing::debug`

### 3. Claude：`tool_icon` 新增 WebFetch/WebSearch 图标

**已完成**（`jsonl.rs`，commit `5477799`）。

`Write`/`Grep`/`WebFetch` 的 `summarize_tool_result` 行为沿用通用 fallback（显示行数），足以满足当前需求。

### 4. 统一 tool_use/tool_result 缓存策略

当前缓存策略（Claude 路径跨批次 HashMap，Codex 路径内联）工作正常，暂不需要统一。

Claude 路径：`tool_use_id → tool_name` 在 `tool_names: HashMap` 里缓存（跨批次）。
Codex 路径：`tool_use_id` 直接内联在 `CommandExecutionItem` 上，无缓存需求。
两个策略没有问题，但需确保：

- Claude `ToolResult` 如果 `tool_use_id` 不在缓存里，`tool_name` 为 `None`，fallback 行为要正确。
- Codex 路径的 `tool_name` 是 `"Bash"`（硬编码），正确但未来需注意其他工具类型。

---

## 优先级 P2 —— 健壮性与可观测性

### 5. Codex：多变体覆盖更完整

**已完成**（commit `5477799`）—— `parse_item_completed` 和 `parse_response_item` 末尾的 `_` / `other` 分支现在输出 `tracing::debug!("Unknown ... type: {other}")`，不再静默忽略。

### 6. Claude：`is_error` 字段利用

待做——当前 `summarize_tool_result` 通过文本匹配 `exit N`，未利用 `tool_result.is_error` 字段。

### 7. 统一 ParsedEntry 输出截断

`CodexJsonlParser` 自定义了 `MAX_BASH_OUTPUT_CHARS = 3000`，`JsonlParser` 通过 `summarize_tool_result`
生成摘要（不截断原始输出），实际输出的截断在 `recovery.rs` 的 `MAX_MSG_LEN = 3800` 统一处理。
建议将截断逻辑收敛到单一位置（recovery.rs），避免两层截断语义混淆。

---

## 优先级 P3 —— 维护性

### 8. 提取共享工具枚举

**已完成**（`lib.rs`，commit `5477799`）

```rust
pub const TOOL_BASH: &str = "Bash";
pub const TOOL_READ: &str = "Read";
pub const TOOL_EDIT: &str = "Edit";
pub const TOOL_WRITE: &str = "Write";
pub const TOOL_GREP: &str = "Grep";
pub const TOOL_GLOB: &str = "Glob";
pub const TOOL_WEBFETCH: &str = "WebFetch";
```

`codex_jsonl.rs` 现用 `crate::TOOL_BASH` 替换硬编码字符串。

### 9. 解析器错误上报

待做——`parse_str` 里可记录跳过行数到 tracing。

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
