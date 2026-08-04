# Atim PowerShell installer — installs atim on Windows
#
# Usage:
#   powershell -ExecutionPolicy Bypass -Command "iwr https://raw.githubusercontent.com/zitsen/atim/main/install.ps1 -useb | iex"
#
# Environment variables:
#   ATIM_VERSION        — atim version to install (default: latest)
#   ATIM_BIN_DIR        — install directory (default: $HOME\.local\bin)
#   ATIM_REPO           — atim GitHub repo (default: zitsen/atim)
#   ATIM_IM_BACKEND     — telegram|feishu
#   ATIM_TELEGRAM_TOKEN — Telegram bot token
#   ATIM_ALLOWED_USERS  — Telegram allowed user IDs
#   ATIM_FEISHU_APP_ID  — Feishu App ID
#   ATIM_FEISHU_APP_SECRET — Feishu App Secret

$ErrorActionPreference = "Stop"

$ATIM_REPO = if ($env:ATIM_REPO) { $env:ATIM_REPO } else { "zitsen/atim" }
$ATIM_VERSION = if ($env:ATIM_VERSION) { $env:ATIM_VERSION } else { "latest" }
$INSTALL_DIR = if ($env:ATIM_BIN_DIR) { $env:ATIM_BIN_DIR } else { Join-Path $HOME ".local\bin" }
$ATIM_HOME = if ($env:ATIM_DIR) { $env:ATIM_DIR } else { Join-Path $HOME ".atim" }

function Write-Info($msg) { Write-Host "[*] $msg" -ForegroundColor Cyan }
function Write-Err($msg) { Write-Host "[!] $msg" -ForegroundColor Red; exit 1 }

# ── Install psmux (Windows tmux replacement) ──

function Install-Psmux {
    if (Get-Command psmux -ErrorAction SilentlyContinue) {
        Write-Info "psmux already installed."
        return
    }
    if (Get-Command winget -ErrorAction SilentlyContinue) {
        Write-Info "Installing psmux via winget..."
        winget install psmux --accept-package-agreements --accept-source-agreements
        return
    }
    if (Get-Command scoop -ErrorAction SilentlyContinue) {
        Write-Info "Installing psmux via scoop..."
        scoop bucket add psmux https://github.com/psmux/scoop-psmux
        scoop install psmux
        return
    }
    if (Get-Command choco -ErrorAction SilentlyContinue) {
        Write-Info "Installing psmux via choco..."
        choco install psmux -y
        return
    }
    Write-Err "psmux is required (tmux replacement). Install with: winget install psmux"
}

# ── Platform detection ──

function Get-Target {
    $arch = $env:PROCESSOR_ARCHITECTURE
    if ($arch -eq "AMD64") { return "x86_64" }
    if ($arch -eq "ARM64") { return "aarch64" }
    Write-Err "Unsupported architecture: $arch"
}

function Get-LatestVersion($repo) {
    $release = Invoke-RestMethod -Uri "https://api.github.com/repos/$repo/releases/latest" -Headers @{ "User-Agent" = "atim-installer" }
    return $release.tag_name.TrimStart("v")
}

# ── Download and install ──

function Install-Atim {
    $target = Get-Target
    $version = $ATIM_VERSION
    if ($version -eq "latest") {
        Write-Info "Fetching latest atim release..."
        $version = Get-LatestVersion $ATIM_REPO
    }

    # Windows binaries are packaged as tar.gz with the .exe inside.
    $url = "https://github.com/$ATIM_REPO/releases/download/v$version/atim-$target-pc-windows-gnu.tar.gz"
    Write-Info "Downloading atim $version from $url"
    $archivePath = Join-Path $env:TEMP "atim-$version.tar.gz"

    Invoke-WebRequest -Uri $url -OutFile $archivePath

    # Verify checksum
    $shaUrl = "$url.sha256"
    $expected = (Invoke-WebRequest -Uri $shaUrl).Content.Trim().Split(" ")[0]
    $actual = (Get-FileHash -Path $archivePath -Algorithm SHA256).Hash.ToLower()
    if ($actual -ne $expected) {
        Write-Err "Checksum verification failed: expected $expected, got $actual"
    }
    Write-Info "Checksum verified."

    # Extract
    New-Item -ItemType Directory -Force -Path $INSTALL_DIR | Out-Null
    $extractDir = Join-Path $env:TEMP "atim-extract"
    if (Test-Path $extractDir) { Remove-Item -Recurse -Force $extractDir }
    tar xzf $archivePath -C $env:TEMP
    Move-Item -Force (Join-Path $env:TEMP "atim.exe") (Join-Path $INSTALL_DIR "atim.exe")
    Remove-Item -Force $archivePath

    Write-Info "Installed atim $version to $INSTALL_DIR\atim.exe"
    $env:PATH = "$INSTALL_DIR;$env:PATH"
    Write-Info "Add $INSTALL_DIR to your PATH if not already there."
}

# ── Config ──

function Write-Config {
    New-Item -ItemType Directory -Force -Path $ATIM_HOME | Out-Null
    $backend = if ($env:ATIM_IM_BACKEND) { $env:ATIM_IM_BACKEND } else { "telegram" }
    $token = if ($env:ATIM_TELEGRAM_TOKEN) { $env:ATIM_TELEGRAM_TOKEN } else { "" }
    $users = if ($env:ATIM_ALLOWED_USERS) { $env:ATIM_ALLOWED_USERS } else { "" }
    $appId = if ($env:ATIM_FEISHU_APP_ID) { $env:ATIM_FEISHU_APP_ID } else { "" }
    $appSecret = if ($env:ATIM_FEISHU_APP_SECRET) { $env:ATIM_FEISHU_APP_SECRET } else { "" }

    $config = @"
[im]
backend = "$backend"

[im.feishu]
app_id = "$appId"
app_secret = "$appSecret"

[im.telegram]
token = "$token"
allowed_users = "$users"

[agent]
command = "claude"

[tmux]
session = "atim"

[monitor]
poll_interval = "2.0"

[display]
show_user_messages = "true"
show_tool_calls = "true"
show_hidden_dirs = false
"@
    $configPath = Join-Path $ATIM_HOME "config.toml"
    Set-Content -Path $configPath -Value $config -Encoding UTF8
    Write-Info "Wrote $configPath"
}

# ── Service ──

function Install-Service {
    $atimBin = Join-Path $INSTALL_DIR "atim.exe"
    if (-not (Test-Path $atimBin)) { return }
    Write-Info "Installing atim as a Windows service..."
    & $atimBin service --install
    Write-Info "Run `atim service --start` to start."
}

# ── Main ──

Write-Info "Atim installer for Windows"
Install-Psmux
Install-Atim
Write-Config
Install-Service
Write-Info "Done."
