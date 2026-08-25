# Features

## Session Recovery

如果 agent 意外中断（机器重启、窗口关闭、进程崩溃），Atim 会在你下次发消息时自动检测并提示选择：

- **Recover Session** — 在原项目目录重新创建 tmux 窗口，使用 `--resume` 恢复之前的会话上下文
- **New Session** — 创建全新的会话

恢复过程自动完成，包括：重建 tmux 窗口、恢复工作目录、更新 session 绑定。会话历史不会丢失。

## Voice Messages (语音消息)

配置 `ATIM_OPENAI_API_KEY` 后，发送飞书语音消息会自动转录为文字。

- 使用 OpenAI Whisper API（默认模型 `gpt-4o-transcribe`）
- 支持任何 OpenAI 兼容的 API 端点（通过 `ATIM_OPENAI_BASE_URL` 配置）
- 在移动场景下，语音输入比打字高效得多

!!! note
    语音转录目前仅将转录文本显示在聊天中，不会自动转发给 agent。需要手动复制转录结果发送。

## Edit Diff Cards

当 agent 使用 Edit 工具修改文件时，Atim 会自动将修改内容以格式化的 diff 卡片发送到聊天中，无需查看终端即可看到代码变更。

## Interactive UI Detection

Atim 持续监测 agent 的终端输出，自动检测交互式 UI 元素：

- **AskUserQuestion** — agent 向用户提问时，显示选项按钮
- **PermissionPrompt** — agent 请求执行权限时，显示批准/拒绝按钮
- **ExitPlanMode** — agent 退出计划模式时的确认提示

这些交互通过 inline keyboard 实现，无需手动操作终端。

## Flood Control

每条聊天通道有独立的限流机制，防止 agent 短时间内大量输出时刷屏：

- 内容感知的消息合并（连续的小输出合并为一条消息）
- 速率限制（超出限制时排队等待）

## Session Picker (会话选择器)

当创建新会话时，如果 agent 支持 session（Claude Code、Copilot、Mimo），Atim 会列出当前项目目录下的已有会话，让你选择恢复或新建。

## Screenshot (终端截图)

使用 `/ss` 命令截取当前 agent 终端画面。截图使用 ANSI 渲染：

- 支持 256 色和 24-bit RGB
- 支持 CJK 字符（使用 WenQuanYi / Noto Sans CJK 回退字体）
- 输出为 PNG 图片，自动适配 IM 平台的尺寸限制

## Shell Command Execution

在聊天中使用 `!<command>` 前缀可以在 agent 的 tmux 窗口中执行 shell 命令：

- 实时流式输出（每 1.5 秒轮询终端）
- 自动检测 shell prompt 以判断命令完成
- 30 秒超时保护
- 输出截断为 3800 字符

## Auto-Confirm Trust Dialog

首次在新目录启动 Claude Code 时会弹出"信任此目录"的确认对话框，Atim 会自动确认，无需手动操作终端。
