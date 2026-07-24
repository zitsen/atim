# Slash Commands

在 IM 聊天中发送以下命令控制 agent 会话。

## 会话管理

| 命令 | 说明 |
|------|------|
| `/rebind` | 重新绑定当前窗口。用于 agent 重启后修复路由，或修复过期的 session 映射 |
| `/rebind agent` | 重新绑定 agent 类型（如 `claude`、`copilot`、`codex`） |
| `/unbind` | 解绑 session，向 agent 发送 `/quit`，并关闭 tmux 窗口 |
| `/clear` | 清除 Claude Code 对话并开启新 session UUID。自动重新绑定 |

## 状态查询

| 命令 | 说明 |
|------|------|
| `/status` | 显示 Claude Code 会话状态 |
| `/usage` | 显示 Claude Code 用量/配额信息 |
| `/check` | 系统健康检查报告 |

## 终端控制

| 命令 | 说明 |
|------|------|
| `/ss` 或 `/screenshot` | 截取 tmux 终端的屏幕截图 |
| `/esc` 或 `/dismiss` | 发送 Escape 键（关闭模态框/帮助屏幕） |
| `/enter` | 发送 Enter 键（确认模态框/选择） |
| `!<command>` | 在 agent 的 tmux 窗口中运行 shell 命令并流式输出 |

## Agent 切换

| 命令 | 说明 |
|------|------|
| `/switch <agent>` | 切换到不同的 agent（`claude`、`copilot`、`codex`） |

## 目录浏览

当进入目录浏览模式时，输入文字作为 `zoxide` 查询，快速跳转到匹配的目录。

## Session 恢复

如果 tmux 窗口失效，Atim 会自动提示：

- **Recover Session** — 创建新窗口并恢复之前的 session
- **New Session** — 创建全新的 session

## 会话生命周期

```
消息 → 找到绑定 → 转发到 tmux
     ↘ 没有绑定 → 显示目录浏览器 → 创建新窗口
        ↘ 窗口存在但 agent 没运行 → 提示恢复或新建
           ↘ /rebind — 重新绑定现有窗口
              ↘ /unbind — 清理并删除绑定
```
