# Atim：把 AI 编程助手装进口袋

## 引言

AI 编程助手（Claude Code 等）已经足够强大，但它们被绑在终端里：必须在电脑前、必须开终端、一个会话绑一个屏幕、agent 干完活你也不知道。

Atim（发音像 "Atom"）解决这个问题 —— 一个开源的 IM 桥接工具，把 AI 编程 agent 接入即时通讯软件。在飞书或 Telegram 里发一条消息，电脑上的 agent 就开始干活，把结果发回同一个聊天。

```
飞书 / Telegram  ↔  tmux  ↔  Claude Code / Copilot / Codex / Mimo
```

## Atim 是什么

Atim 是一个自部署、MIT 开源的桥接服务，用 Rust 编写。它通过 tmux 控制 AI 编程 agent，不依赖任何 agent 的 SDK —— 读终端输出、发按键，任何 CLI agent 都能被控制。

### 核心能力

| 能力 | 说明 |
|------|------|
| **多 IM** | 飞书 / Lark + Telegram |
| **多 Agent** | Claude Code、Copilot CLI、Codex CLI、Mimo 自动识别 |
| **话题隔离** | 每个飞书话题 / Telegram topic 独立映射一个 agent 会话 |
| **会话恢复** | tmux 窗口挂掉后一键恢复，绑定关系自动重建 |
| **语音消息** | 飞书语音 → Whisper 转录 → 发给 agent |
| **终端截图** | `/ss` 直接看 agent 的终端画面 |
| **shell 命令** | `!command` 在 agent 窗口执行任意命令并流式输出 |

### 技术特点

- **tmux 作为通用控制层**：任何 CLI agent 都能接入，新 agent 接入零代码
- **JSONL 字节偏移追踪**：只读会话日志的新增内容，不重复解析
- **SQLite 状态持久化**：绑定关系、会话状态跨重启保留
- **跨平台**：Linux、macOS，以及 Windows 原生支持（通过 psmux，一个兼容 tmux 命令的 Windows 终端复用器）

### 安装

```bash
# Linux / macOS
curl -fsSL https://raw.githubusercontent.com/zitsen/atim/main/install.sh | bash

# Windows
winget install psmux
powershell -ExecutionPolicy Bypass -Command "iwr https://raw.githubusercontent.com/zitsen/atim/main/install.ps1 -useb | iex"
```

## 架构与消息流转

### 架构图

```
  ┌──────────────┐     ┌──────────────────────────┐     ┌──────────────┐
  │  飞书         │     │     Atim Server          │     │  Telegram    │
  │  Bot API      │ <-> │                          │ <-> │  Bot API     │
  └──────────────┘     │  ┌──────┐  ┌───────────┐ │     └──────────────┘
                       │  │ Queue│  │  Monitor  │ │
                       │  └──┬───┘  └─────┬─────┘ │
                       │     │            │       │
                       │  ┌──┴────────────┴───┐   │
                       │  │  State + Tmux     │   │
                       │  └────────┬──────────┘   │
                       └───────────┼──────────────┘
                                   │
                          ┌────────┴────────┐
                          │  tmux session   │
                          │  ┌────────────┐ │
                          │  │ Claude Code│ │
                          │  │ / Copilot  │ │
                          │  │ / Codex    │ │
                          │  └────────────┘ │
                          └─────────────────┘
```

### 消息流转

```
在飞书发消息
  → Atim 收到 webhook
  → 查找/创建 tmux 窗口绑定
  → 向 agent 终端发送按键
  → agent 处理并产生输出
  → Monitor 检测到 JSONL 日志新增
  → Atim 把响应发回飞书
```

### 三个设计决策

1. **为什么用 tmux 而不是 SDK？** 任何 CLI agent 都能被控制，不需要 agent 提供 API。新 agent 接入 = 零代码。
2. **为什么话题映射窗口？** 一个话题 = 一个 tmux 窗口 = 一个独立 agent 会话。多任务并行互不干扰。
3. **为什么字节偏移？** 每个会话只从上次的位置读 JSONL，不重复解析，监控效率高。

## 使用方式

### 基本对话

将机器人加入飞书群聊（或 Telegram 群组并开启话题模式），发送任意消息。Atim 自动创建 tmux 窗口、启动 agent，之后每条消息都会转发给 agent，每个响应都会回到群聊。

### 会话管理命令

| 命令 | 说明 |
|------|------|
| `/rebind` | 重新绑定当前窗口（agent 重启后修复路由） |
| `/unbind` | 解绑 session，关闭 tmux 窗口 |
| `/clear` | 清空对话开启新会话 |
| `/status` | 查看 agent 会话状态 |
| `/ss` | 截图当前终端 |
| `/usage` | 查看 Claude Code 用量 |
| `/switch <agent>` | 切换 agent（claude/copilot/codex） |
| `!<command>` | 在 agent 窗口执行 shell 命令 |

### 会话恢复

如果 tmux 窗口失效，Atim 会提示恢复或新建。选择恢复后，新窗口会在正确的项目目录启动，并续接之前的会话上下文 —— 绑定关系、会话 UUID、工作目录都会自动重建。

## 为什么需要这样的工具

### AI 编程助手的使用场景限制

| 限制 | 影响 |
|------|------|
| 必须在电脑前 | 离开工位就断了 |
| 必须开终端 | 非技术同事无法使用 |
| 一对一 | 一个会话绑一个屏幕 |
| 无通知 | agent 干完活你不知道 |

### Atim 解决的三个本质问题

**随时随地**：手机 = 新的终端。地铁上、旅途中、会议室里，都能让本地 agent 干活。代码和数据始终在你的机器上。

**团队协作**：非技术同事（测试、产品、运营）也能通过飞书触发 AI 工具。把 agent 变成团队的一个成员。

**会话即工作流**：一个话题 = 一个长期运行的 agent 会话。上下文不丢失，任务可追踪，多任务并行。

### 数据安全视角

Atim 是本地优先的：agent 运行在你自己的机器上，代码不出本地，只有对话内容经过 IM。自部署、MIT 开源、无云依赖。

## 另一个选择：cc-connect

[cc-connect](https://github.com/chenhg5/cc-connect) 是另一个优秀的 IM ↔ AI Agent 桥接方案，社区活跃，已登上 GitHub Trendshift 榜单。核心设计理念是"用 IM 做前端，用 ACP 协议做后端"。

### 通用 Agent 支持（10+）

| Agent | 说明 |
|-------|------|
| Claude Code | 最成熟的 agent，完全支持会话管理 |
| Codex | OpenAI 编程 agent |
| Cursor Agent | Cursor 编辑器的 agent 模式 |
| Kimi CLI | Moonshot AI 的 Kimi K3（3T 参数开源模型） |
| Gemini CLI | Google 的编程 agent |
| Qoder CLI / OpenCode / iFlow CLI | 各类开源 CLI agent |
| Pi / Devin / Copilot | 更多第三方 agent |
| **ACP 协议** | 任何实现 Agent Client Protocol 的 agent 都可接入 |

### 13 大聊天平台

| 平台 | 说明 |
|------|------|
| 飞书 / Lark | 企业协作首选 |
| 钉钉 | 阿里系办公 |
| Slack / Discord / Telegram | 海外主流 |
| 企业微信 / 微信个人号（ilink） | 个人微信也能接入 —— 很多方案做不到 |
| QQ / QQ 官方机器人 | 国内社交 |
| WPS 协作 / 微博 / LINE / Matrix | 更多场景 |

大部分平台无需公网 IP，部署门槛低。

### 多 Agent 编排

在群聊里绑定多个机器人，让它们相互协作：

```
你：这个需求怎么做？
Claude：建议用 A 方案，理由如下……
Gemini：补充一点，A 方案在 X 场景有坑，可以……
```

一个对话里完成多模型交叉验证。

### 聊天即控制

cc-connect 支持完整的斜杠命令控制：

```text
/model claude-sonnet   → 切换模型
/reasoning high        → 切换推理强度
/mode bypass           → 切换权限模式
/dir ~/projects/foo    → 切换下次会话工作目录
/dir 3                 → 跳到历史目录 3
/memory                → 读写 agent 指令文件（持久记忆）
```

### 更多能力

- **智能定时任务**：自然语言创建 cron，如"每天早上 6 点总结 GitHub trending"
- **多模态**：语音消息自动 STT 转录，截图直接转发给 agent
- **多项目架构**：一个进程管理多个项目，各自独立的 agent + 平台组合
- **多语言**：中/英/日/西/繁中 5 种语言界面

### 技术特点

- Go 编写，MIT 开源
- 使用 Agent Client Protocol (ACP) 对接 agent，比终端控制更原生、更轻量
- npm 包 + GitHub Release 分发

## 对比与选择

### Atim vs cc-connect

| 维度 | Atim | cc-connect |
|------|------|-----------|
| **语言** | Rust | Go |
| **控制方式** | tmux 终端级（通用） | ACP 协议（原生） |
| **Agent 支持** | 4+（终端类 CLI） | 10+（含 Cursor、Kimi CLI） |
| **IM 平台** | 飞书、Telegram | 13 个（含钉钉、微信、QQ） |
| **多 Agent 协作** | ❌ 单 agent | ✅ 群聊多 bot 互聊 |
| **定时任务** | ❌ | ✅ 自然语言 cron |
| **Windows** | ✅ 原生（psmux） | ✅ |
| **独特优势** | tmux 通用控制、终端截图、字节偏移监控 | 平台覆盖面、ACP 生态、个人微信 |

### 如何选择

- **需要终端级控制**（任何 CLI agent 都能接，包括未来的新 agent）→ **Atim**
- **需要平台覆盖广**（钉钉/微信/QQ + 多 agent 编排 + 定时任务）→ **cc-connect**
- 两个都是 MIT 开源，可以都试试

## 结语

AI 编程助手的价值，不该被限制在终端里。把 agent 接入 IM，等于把它从"工具"变成了"团队里随时在线的成员"。

无论是 Atim 还是 cc-connect —— 选择哪个不重要，重要的是：你的 AI 助手，应该和你一样随时在线。

## 参考链接

- **Atim**: https://github.com/zitsen/atim
- **Atim 文档**: https://zitsen.github.io/atim/
- **cc-connect**: https://github.com/chenhg5/cc-connect
- **psmux**（Windows tmux 替代）: https://github.com/psmux/psmux
