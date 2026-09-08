# Claude Code Session JSONL Schema

> 来源：Anthropic Claude Code CLI，JSONL 格式版本 v2.1.143
>
> 注意：该格式官方文档有限，以下基于 atim 逆向分析（`atim-parser/src/jsonl.rs`）与 Anthropic Messages API 规范推断。

## 文件位置

```
~/.claude/projects/<project-hash>/<SESSION_UUID>.jsonl
```

## 顶层格式

每行一个 JSON，`type` 字段判别：

| `type` | 说明 |
|--------|------|
| `message` | 会话消息（用户/助手），核心类型 |
| `summary` | 会话摘要 |
| `session_meta` | 会话元数据 |

## `message` 行结构

```json
{
  "type": "message",
  "message": {
    "role": "assistant",       // "user" | "assistant"
    "content": [               // ContentBlock[]
      { "type": "text", "text": "..." },
      { "type": "tool_use", "id": "call_xxx", "name": "Bash", "input": {...} },
      { "type": "tool_result", "tool_use_id": "call_xxx", "content": [...], "is_error": false }
    ]
  },
  "uuid": "...",
  "timestamp": "ISO8601"
}
```

## ContentBlock 类型

### `text`

```json
{ "type": "text", "text": "..." }
```

### `tool_use`（助手发出的工具调用）

```json
{
  "type": "tool_use",
  "id": "toolu_01ABC...",
  "name": "Bash",            // 工具名，Bash/Read/Write/Edit/Grep/Glob 等
  "input": {
    "command": "ls -la",     // Bash 时为字符串（⚠️ 不是数组）
    "description": "List files"  // 可选
  }
}
```

**关键**：`input.command` 是**纯字符串**，不是 Codex 的 `Vec<String>`。

### `tool_result`（用户返回的工具结果）

```json
{
  "type": "tool_result",
  "tool_use_id": "toolu_01ABC...",   // 关联 tool_use.id
  "content": [
    { "type": "text", "text": "file1.txt\nfile2.txt\n" }
  ],
  "is_error": false
}
```

**输出**：`content` 是 `ContentBlock[]`（与 tool_use 同结构），非裸字符串。

## 工具名映射（Claude Code 工具名）

| 工具名 | 对应功能 |
|--------|---------|
| `Bash` | Shell 命令执行 |
| `Read` / `ReadTool` / `FileReadTool` | 文件读取 |
| `Write` / `WriteTool` / `FileWriteTool` | 文件写入 |
| `Edit` / `EditTool` / `TextEditTool` | 文件编辑 |
| `Grep` / `GrepTool` | 文本搜索 |
| `Glob` / `GlobTool` | 文件通配符搜索 |
| `WebFetch` / `WebSearch` | 网络工具 |
| `Agent` / `Subagent` | 子 agent 调用 |

## JSONL 示例（精简）

```json
{"type":"message","message":{"role":"assistant","content":[{"type":"text","text":"I'll check the directory."},{"type":"tool_use","id":"toolu_01XYZ","name":"Bash","input":{"command":"ls -la","description":"List files in current directory"}}]},"uuid":"abc-123","timestamp":"2026-09-04T08:00:00Z"}
{"type":"message","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"toolu_01XYZ","content":[{"type":"text","text":"total 48\ndrwxr-xr-x 5 user user 4096 Sep  4 08:00 ."}],"is_error":false}]},"uuid":"def-456","timestamp":"2026-09-04T08:00:01Z"}
```

## Claude vs Codex 核心差异

| 维度 | Claude JSONL | Codex Rollout JSONL |
|------|-------------|---------------------|
| 顶层结构 | `{type:"message", message:{role,content:[...]}}` | `{type:"event_msg", payload:{type:"item_completed", item:{...}}}` |
| Bash 命令 | `input.command: String`（字符串） | `command: Vec<String>`（argv 数组）+ `parsed_cmd[].cmd` |
| Bash 输出 | `tool_result.content[].text`（ContentBlock 数组） | `aggregated_output` / `formatted_output`（裸字符串） |
| 工具标识 | `tool_use.name`（直接在内容块里） | `item.type`（顶层判别）+ `tool_name`（通过 cache 关联） |
| tool_use↔result 关联 | `tool_use_id` 字段 | `id` 字段 + `tool_use_id` 传递 |
| 成功 Bash 输出 | 完整输出（不主动省略） | `aggregated_output` 等（过去省略，现在也展示） |
