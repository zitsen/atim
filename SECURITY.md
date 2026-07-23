# Security Policy

## Supported Versions

| Version | Supported          |
| ------- | ------------------ |
| 0.3.x   | :white_check_mark: |
| < 0.3   | :x:                |

## Reporting a Vulnerability

If you discover a security vulnerability in Atim, please report it responsibly.

**Do not open a public GitHub issue for security vulnerabilities.**

Instead, please email: **security@zitsen.io**

Include:
- A description of the vulnerability
- Steps to reproduce (if applicable)
- The version of Atim affected

We will acknowledge receipt within 48 hours and aim to provide a fix or mitigation plan within 7 days.

## Scope

Atim runs as a user-level systemd service and communicates with IM backends (Telegram, Feishu) and local tmux sessions. The main security considerations are:

- **IM webhook authentication**: Verify that incoming messages are from authenticated sources
- **Shell command injection**: The `!` prefix feature runs commands in tmux — this is intentional but should be used with awareness
- **Credential storage**: Tokens and secrets in `~/.atim/config.toml` (chmod 600)
- **File access**: Atim reads/writes to `~/.atim/` and the tmux session directory

## Disclosure

We follow coordinated disclosure. After a fix is released, we will publish a security advisory on GitHub.
