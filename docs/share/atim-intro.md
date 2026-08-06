# 把 AI 编程助手装进口袋：Atim 飞书桥接实践指南

## 背景介绍

AI 编程助手（Claude Code、Copilot CLI 等）已经深度融入日常开发，但它们始终被限制在终端里：必须在电脑前操作、必须开一个终端窗口、一个会话绑定一个屏幕、agent 干完活你也不知道。这带来几个实际困扰：

- **离开工位就断联**：在地铁上、在会议室、在食堂排队时，突然想检查一个问题或让 agent 跑个任务，只能干等回到工位。
- **非技术同事无法使用**：测试、产品、运维同事想借用 AI 编程能力，但终端操作门槛太高。
- **通知缺失**：agent 执行一个耗时的任务（跑测试、批量改代码）时，你只能守着终端等，无法抽身做别的事。

Atim 正是为了解决这些问题而生。它将 AI 编程 agent 桥接到即时通讯软件（飞书、Telegram），实现"**在 IM 里发一条消息，电脑上的 agent 就开始干活，把结果发回同一个聊天**"。

Atim 是自部署、MIT 开源的 Rust 项目，代码仓库位于 https://github.com/zitsen/atim。

## Atim 能做什么

### 随时随地使用 AI 编程助手

在飞书群里发消息，就等于在给本地的 Claude Code 下指令。无论你是在工位、在地铁上还是在家里，只要手机能连上飞书，你的 AI 编程助手就随时待命。

- 在飞书里直接对话，agent 的思考过程和最终结果都会实时回到群聊
- 支持语音消息，发送语音会自动转录成文字再发给 agent，移动场景下更高效
- 支持终端截图，随时查看 agent 正在运行的终端画面

### 一个群组对应一个会话

每个飞书群组对应一个独立的 agent 会话。这意味着：

- **上下文隔离**：不同群组的对话互不干扰，各自保留独立的上下文
- **多任务并行**：可以同时开多个群组，每个群组跑一个不同的任务或项目
- **团队共享**：把机器人加入团队群，整个团队都能用自然语言让 AI 干活，不需要任何终端操作经验

### 常用功能一览

| 功能 | 说明 |
|------|------|
| **多 Agent 支持** | Claude Code、Copilot CLI、Codex CLI、Mimo，自动识别切换 |
| **会话管理** | 一键清空对话、重新绑定、查看状态、切换 agent |
| **Shell 命令** | 在聊天里直接执行 `!cargo test`、`!git status` 等命令并看到实时输出 |
| **会话恢复** | 机器重启或窗口关闭后，自动恢复之前的会话上下文 |
| **语音消息** | 飞书语音自动转录为文字后发送给 agent |

### 跨平台使用

Atim 支持 Linux、macOS 和 Windows，团队内不同系统的同事都能统一接入使用。

## 准备环境

### 安装 Atim

=== "Linux / macOS"

    ```bash
    curl -fsSL https://raw.githubusercontent.com/zitsen/atim/main/install.sh | bash
    ```

=== "Windows"

    ```powershell
    winget install psmux
    powershell -ExecutionPolicy Bypass -Command "iwr https://raw.githubusercontent.com/zitsen/atim/main/install.ps1 -useb | iex"
    ```

安装脚本会自动完成：下载二进制、校验完整性、生成配置文件、注册系统服务。

### 前置依赖

| 依赖 | 说明 |
|------|------|
| **tmux**（Linux/macOS）或 **psmux**（Windows） | Atim 的运行基础，必须安装 |
| **Claude Code 等 agent CLI** | 需要预先安装并完成登录认证 |
| **zoxide**（可选） | 提供目录快速跳转能力，提升使用体验 |

### 创建飞书机器人

1. 打开[飞书开放平台](https://open.feishu.cn/page/launcher)，创建企业自建应用。
2. 在「凭证与基础信息」中获取 App ID 和 App Secret。
3. 在「权限管理」中开通 `im:message`（收发消息）、`im:message.group_at_msg`（群聊 @ 消息）、`im:resource`（发送图片/文件）权限。
4. 发布应用版本，在群聊中添加机器人。

### 配置并启动

编辑 `~/.atim/config.toml`：

```toml
[im]
backend = "feishu"

[im.feishu]
app_id = "cli_xxxxxxxxxxxxxx"
app_secret = "xxxxxxxxxxxxxxxxxxxxxxxxxx"

[agent]
command = "claude"

[tmux]
session = "atim"

[monitor]
poll_interval = "2.0"
```

启动服务：

```bash
atim service --start
atim service --status   # 验证运行状态
```

可选：安装 Session Hook，让 Claude Code 启动时自动上报会话信息，提升会话追踪的可靠性：

```bash
atim hook --install
```

## 使用方式

### 基本对话

将机器人添加到飞书群聊，发送任意消息。Atim 会自动完成一系列动作：创建会话 → 启动 Claude Code → 将你的消息发送给 agent → 把 agent 的响应发回群聊。整个过程无需任何手动干预。

后续每条消息都会直接转发给 agent，agent 的每个响应（包括思考过程、工具调用、最终结果）都会实时回到群聊。你可以完全脱离终端，用手机就能完成整个开发交互。

### 多群组并行

创建多个飞书群组，每个群组独立对应一个 agent 会话：

- 群组「项目A」→ agent 会话 1
- 群组「项目B」→ agent 会话 2

不同群组的对话互不干扰，上下文各自独立。适合同时推进多个任务，或者给不同项目分配不同的 agent 环境。

### 常用命令

| 命令 | 说明 |
|------|------|
| `/rebind` | 重新绑定当前会话（agent 重启后修复） |
| `/unbind` | 解绑会话，关闭当前 agent |
| `/clear` | 清空对话开启新会话 |
| `/status` | 查看 agent 会话状态 |
| `/ss` | 截图当前终端 |
| `/usage` | 查看 Claude Code 用量 |
| `/switch <agent>` | 切换 agent（claude/copilot/codex） |
| `!<command>` | 在 agent 窗口执行 shell 命令并流式输出 |

其中 `!<command>` 非常实用：比如 `!cargo test`、`!git status`，直接在聊天里执行终端命令并看到实时输出，不需要切到终端。

### 会话恢复

如果 agent 意外中断（比如机器重启、窗口被误关），Atim 会在你下次发消息时提示"恢复会话"或"新建会话"。选择恢复后：

1. Atim 自动重建会话环境；
2. 恢复之前的会话上下文和工作目录；
3. 在正确的项目目录重新启动 agent。

整个恢复过程自动完成，会话历史不会丢失。

### 语音消息

配置 `ATIM_OPENAI_API_KEY` 后，发送飞书语音消息会自动转录为文字再发送给 agent。在移动场景下，语音输入比打字高效得多。

## 实际使用场景

### 场景一：随时随地处理问题

出门在外时，通过手机飞书发送"检查一下测试环境 192.168.3.53 的服务是否正常"，本地的 Claude Code 会立即执行检查并返回结果。代码和数据始终在本地机器上，不存在数据上传到云端的风险。

### 场景二：团队共享 AI 能力

将机器人加入团队群聊后，测试、运维同事可以直接用自然语言让 AI 干活：

- 测试同事："帮我写一组登录接口的测试用例"
- 运维同事："检查一下这台机器上 TDengine 的磁盘占用情况"
- 产品同事："帮我总结这个接口文档的变更点"

不需要任何终端操作经验，agent 成为团队里随时在线的一员。

### 场景三：长任务异步执行

让 agent 跑一个耗时的批量任务（如全量代码扫描、批量重构），发完消息就可以关掉手机去忙别的。agent 执行过程中有进展或完成时，结果会实时推送到群聊，不用守着终端干等。

## 另一个选择：cc-connect

除了 Atim，社区还有一个非常优秀的同类方案：[cc-connect](https://github.com/chenhg5/cc-connect)。它同样是开源的 IM ↔ AI Agent 桥接工具，已登上 GitHub Trendshift 榜单，社区非常活跃。

### cc-connect 的特点

**通用 Agent 支持（10+）**：除了 Claude Code、Codex、Copilot，还支持 Cursor Agent、Kimi CLI、Gemini CLI、OpenCode 等，以及任何实现 Agent Client Protocol（ACP）协议的 agent。其中 Kimi CLI 是 Moonshot AI 的 Kimi K3 模型（3T 参数开源模型），国内可直接使用。

**13 大聊天平台**：飞书、钉钉、Slack、Discord、Telegram、企业微信、QQ、LINE、微博、Matrix 等，甚至支持微信个人号接入。大部分平台无需公网 IP，部署门槛低。

**多 Agent 编排**：在群聊中绑定多个机器人，让它们相互协作 —— 问 Claude 一个需求怎么做，再听 Gemini 的补充意见，一个对话里完成多模型交叉验证。

**聊天即控制**：通过斜杠命令完成全部控制：

```text
/model claude-sonnet   → 切换模型
/reasoning high        → 切换推理强度
/mode bypass           → 切换权限模式
/dir ~/projects/foo    → 切换下次会话工作目录
/memory                → 读写 agent 指令文件（持久记忆）
```

**智能定时任务**：用自然语言创建定时任务，如"每天早上 6 点总结 GitHub trending"，到点自动执行并推送结果。

**多模态与多语言**：语音消息自动转录、截图直接转发给 agent；界面支持中/英/日/西/繁中 5 种语言。

## 对比与选型建议

| 维度 | Atim | cc-connect |
|------|------|-----------|
| **Agent 支持** | 4+（终端类 CLI） | 10+（含 Cursor、Kimi CLI） |
| **IM 平台** | 飞书、Telegram | 13 个（含钉钉、微信、QQ） |
| **多 Agent 协作** | 单 agent | 群聊多 bot 互聊 |
| **定时任务** | 不支持 | 自然语言定时任务 |
| **Windows** | 支持（psmux） | 支持 |
| **特色能力** | 终端截图、shell 命令、会话恢复 | 平台覆盖面广、多模型交叉验证、个人微信 |

**选型建议**：

- **主要使用飞书/Telegram**，希望稳定易用的桥接方案 → 选择 **Atim**
- **需要覆盖钉钉/微信/QQ 等更多平台**，或者需要多 agent 编排、定时任务等高级能力 → 选择 **cc-connect**
- 两者都是 MIT 开源，可以都部署试用，根据团队实际使用体验做最终选择

## 使用小结

经过实际使用，Atim 在以下场景带来了明显收益：

1. **移动办公效率显著提升**：地铁、会议室、食堂等碎片时间都能让 agent 干活，之前只能干等的问题不复存在。
2. **团队协作门槛大幅降低**：非技术同事通过自然语言就能使用 AI 编程能力，agent 从"个人工具"变成了"团队资源"。
3. **会话管理自动化**：多群组隔离、会话恢复这些能力全部自动完成，使用者完全无感。

同时也需要注意几点：

- **agent 运行在本地机器**：机器关机或休眠时 agent 无法工作，需要保证机器常开（建议部署在服务器或常开的开发机上）。
- **能力边界**：Atim 目前主要面向飞书和 Telegram，如果团队需要钉钉、微信等更多平台，可以考虑 cc-connect。
- **安全与权限**：机器人加入群聊后，任何群成员都能触发 agent 执行命令，建议在受控的团队群中使用，并关注 agent 的权限模式配置。

## 参考链接

- **Atim 仓库**: https://github.com/zitsen/atim
- **Atim 文档**: https://zitsen.github.io/atim/
- **cc-connect 仓库**: https://github.com/chenhg5/cc-connect
- **psmux**（Windows tmux 替代）: https://github.com/psmux/psmux
