# Windows (WSL2)

Atim depends on [tmux](https://github.com/tmux/tmux), which does not run natively on Windows. The recommended approach is to use **WSL2** (Windows Subsystem for Linux).

## Install WSL2

Open PowerShell as Administrator and run:

```powershell
wsl --install
```

Restart your computer when prompted. After restart, WSL2 will complete setup with Ubuntu.

!!! tip
    You can also install a specific distribution: `wsl --install -d Ubuntu-24.04`

## Set up WSL2

Open the Ubuntu terminal (from Start menu or `wsl` command):

```bash
# Update packages
sudo apt update && sudo apt upgrade -y

# Install tmux
sudo apt install -y tmux

# Install zoxide (optional, for directory browsing)
curl -sSfL https://raw.githubusercontent.com/ajeetdsouza/zoxide/main/install.sh | sh
```

## Install Atim

```bash
curl -fsSL https://raw.githubusercontent.com/zitsen/atim/main/install.sh | bash
```

The installer will:
1. Download the Linux musl binary
2. Install zoxide (optional)
3. Run interactive setup for your IM backend (Feishu or Telegram)
4. Install and enable the systemd user service

## Configure

Edit `~/.atim/config.toml` (created by the installer):

```toml
[im]
backend = "feishu"   # or "telegram"

[im.feishu]
app_id = "cli_xxxxxxxxxxxxxx"
app_secret = "xxxxxxxxxxxxxxxxxxxxxxxxxx"

[agent]
command = "claude"

[tmux]
session = "atim"
```

## Start the Service

```bash
atim service --start
atim service --status   # verify it's running
```

## Access from Windows Apps

WSL2 runs its own Linux environment. Your Feishu/Telegram messages will work exactly the same way — the agent sessions run in tmux inside WSL2.

To access WSL2 files from Windows Explorer:

```
\\wsl$\Ubuntu\home\<username>
```

## Troubleshooting

### systemd not available in WSL2

Older WSL2 versions don't support systemd. Enable it by editing `/etc/wsl.conf`:

```ini
[boot]
systemd=true
```

Then restart WSL2: `wsl --shutdown` (from PowerShell).

### tmux session not persisting after closing terminal

Use `atim service --install && atim service --start` to run as a background service. The service persists across terminal sessions.

### Network issues inside WSL2

If you have trouble connecting to Feishu/Telegram APIs, check your DNS:

```bash
cat /etc/resolv.conf
```

If empty, add a nameserver: `echo "nameserver 8.8.8.8" | sudo tee /etc/resolv.conf`

## Future: Native Windows Support

Native Windows support (without WSL2) is tracked as a planned feature. This would replace tmux with Windows Terminal + ConPTY sessions. See the [GitHub issues](https://github.com/zitsen/atim/issues) for progress.
