# Windows

Atim runs natively on Windows. There are two supported approaches:

## Option 1: Native (psmux) — Recommended

[psmux](https://github.com/psmux/psmux) is a native Windows terminal multiplexer written in Rust. It speaks the tmux command language and ships a `tmux` alias, so Atim's existing tmux manager works against it directly — no WSL, Cygwin, or MSYS2 needed.

### Install psmux

=== "winget"

    ```powershell
    winget install psmux
    ```

=== "scoop"

    ```powershell
    scoop bucket add psmux https://github.com/psmux/scoop-psmux
    scoop install psmux
    ```

=== "chocolatey"

    ```powershell
    choco install psmux
    ```

=== "cargo"

    ```powershell
    cargo install psmux
    ```

This installs `psmux`, `pmux`, and `tmux` binaries. Atim automatically uses psmux on Windows.

### Install Atim

```powershell
powershell -ExecutionPolicy Bypass -Command "iwr https://raw.githubusercontent.com/zitsen/atim/main/install.ps1 -useb | iex"
```

### Install the Session Hook

```powershell
atim hook --install
```

### Manage the Service

```powershell
atim service --install
atim service --start
atim service --status
```

## Option 2: WSL2

Use this if you prefer the classic tmux environment.

### Install WSL2

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

## Troubleshooting
