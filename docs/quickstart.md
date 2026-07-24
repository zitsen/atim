# Quick Start

Get Atim running in under 5 minutes.

## Step 1: Install

```bash
curl -fsSL https://raw.githubusercontent.com/zitsen/atim/main/install.sh | bash
```

Follow the interactive prompts to configure your IM backend (Feishu or Telegram).

## Step 2: Start the Service

```bash
atim service --start
atim service --status   # verify it's running
```

## Step 3: Chat with the Agent

=== "Feishu"

    1. Add your bot to a Feishu group chat
    2. Send any message in the chat
    3. Atim creates a tmux window and starts Claude Code
    4. Chat naturally — every message goes to the agent, every response comes back

=== "Telegram"

    1. Create a Telegram group and enable **Topics** (Forum mode)
    2. Add your bot as an admin
    3. Open a new topic and send any message
    4. Atim creates a tmux window and starts Claude Code
    5. Chat naturally

## What Happens Under the Hood

```
Your message  →  Atim receives it via IM webhook
             →  Finds or creates a tmux window binding
             →  Sends keystrokes to the agent's terminal
             →  Agent processes and generates output
             →  Monitor detects new output in JSONL log
             →  Atim sends the response back to your IM chat
```

## Next Steps

- [Configure your setup](configuration.md) — customize polling interval, display options, etc.
- [Learn slash commands](commands.md) — control sessions with `/rebind`, `/clear`, etc.
- [Set up Feishu](feishu.md) or [Telegram](telegram.md) — detailed setup guides
