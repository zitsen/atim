# Telegram

Telegram 是 Atim 支持的 IM 平台之一。

## 创建 Telegram Bot

1. 在 Telegram 中找到 [@BotFather](https://t.me/botfather)
2. 发送 `/newbot` 创建新 bot
3. 按提示设置 bot 名称和用户名
4. 获取 Bot Token
5. 获取你的 User ID：发送消息给 [@userinfobot](https://t.me/userinfobot)

## 配置

```toml
[im]
backend = "telegram"

[im.telegram]
token = "123456:ABC-DEF1234ghIkl-zyx57W2v1u123ew11"
allowed_users = "123456789"   # 留空 = 允许所有用户
```

或者使用环境变量：

```bash
ATIM_IM_BACKEND=telegram
ATIM_TELEGRAM_TOKEN=123456:ABC-DEF1234ghIkl-zyx57W2v1u123ew11
ATIM_ALLOWED_USERS=123456789
```

## 启动

```bash
atim service --start
atim service --status   # 验证运行状态
```

## 使用方式

### 使用 Topics（推荐）

1. 创建 Telegram 群组
2. 启用 **Topics**（群组设置 → Topics / 论坛模式）
3. 将 bot 添加为管理员
4. 打开新 topic 并发送消息
5. Atim 自动创建 tmux 窗口并启动 agent

每个 topic 独立映射到一个 agent 窗口，互不干扰。

### 直接对话

不使用 Topics 时，可以在与 bot 的私聊中直接发送消息。

## 命令参考

与飞书版本相同的命令，参见 [Slash Commands](commands.md)。

## 故障排除

### Bot 没有响应

1. 检查服务状态：`atim service --status`
2. 检查日志：`journalctl --user -u atim -f`
3. 确认 Token 正确
4. 确认 `allowed_users` 包含你的 User ID（或留空允许所有用户）

### Topic 消息不回复

确认 bot 是群组的**管理员**，普通成员无法接收所有消息。

### Session 路由丢失

发送 `/rebind` 重新发现 session UUID 并更新绑定。
