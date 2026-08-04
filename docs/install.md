# Installation

## Prerequisites

| Requirement | Notes |
| ----------- | ----- |
| **tmux**    | Atim manages agent windows through tmux |
| **zoxide**  | Optional. For fast directory search     |

## Quick Install

=== "curl"

    ```bash
    curl -fsSL https://raw.githubusercontent.com/zitsen/atim/main/install.sh | bash
    ```

=== "wget"

    ```bash
    wget -qO- https://raw.githubusercontent.com/zitsen/atim/main/install.sh | bash
    ```

=== "Custom install path"

    ```bash
    curl -fsSL https://raw.githubusercontent.com/zitsen/atim/main/install.sh | bash -s -- -b /usr/local/bin
    ```

The installer:

1. Downloads a statically-linked musl binary from the latest GitHub release
2. Verifies the SHA256 checksum
3. Installs `zoxide` (optional, for directory browsing)
4. Creates `~/.atim/config.toml` with interactive setup
5. Installs and enables the systemd user service

## Windows

Atim runs natively on Windows via [psmux](https://github.com/psmux/psmux) (a native Windows tmux replacement) — or inside WSL2. See the [Windows guide](windows.md).

## Build from Source

```bash
git clone https://github.com/zitsen/atim.git
cd atim
cargo build --release
# Binary is at target/release/atim
```

Requires Rust stable (edition 2024). Install via [rustup.rs](https://rustup.rs/).

## Install the Session Hook

For reliable session tracking (recommended):

```bash
atim hook --install
```

This registers each Claude Code session UUID so Atim knows which JSONL logs to watch.

## Service Management

```bash
# Install the systemd service unit
atim service --install

# Start/stop/restart/status (user-level by default)
atim service --start
atim service --stop
atim service --restart
atim service --status

# System-level service (requires root)
atim service --system --install
atim service --system --start
```

## Verify Installation

```bash
atim --help
atim service --status
```
