# Telegram

Telegram 是 Atim 的另一个支持的 IM 平台。

## 创建 Telegram Bot

1. 在 Telegram 中找到 [@BotFather](https://t.me/botfather)
2. 发送 `/newbot` 创建新 bot
3. 获取 Bot Token
4. 获取你的 User ID：发送消息给 [@userinfobot](https://t.me/userinfobot)

## 配置

```toml
[im]
backend = "telegram"

[im.telegram]
token = "123456:ABC-DEF1234ghIkl-zyx57W2v1u123ew11"
allowed_users = "123456789"   # 留空 = 允许所有用户
```

## 使用方式

1. 创建 Telegram 群组并启用 **Topics**（论坛模式）
2. 将 bot 添加为管理员
3. 打开新 topic 并发送消息
4. Atim 自动创建 tmux 窗口并启动 agent

每个 topic 独立映射到一个 agent 窗口，互不干扰。

## Slash Commands

与飞书版本相同的命令，参见 [Slash Commands](commands.md)。
