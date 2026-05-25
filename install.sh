#!/bin/bash
#
# Atim installer — installs atim + zoxide from GitHub releases,
# then creates ~/.atim/config.toml and configures user-level service.
#
# Usage:
#   curl -fsSL https://raw.githubusercontent.com/zitsen/atim/main/install.sh | bash
#   curl -fsSL https://raw.githubusercontent.com/zitsen/atim/main/install.sh | bash -s -- -b /usr/local/bin
#
# Environment variables:
#   ATIM_VERSION        — atim version to install     (default: latest)
#   ZOXIDE_VERSION      — zoxide version to install   (default: latest)
#   ATIM_BIN_DIR        — install directory           (default: see -b flag)
#   ATIM_REPO           — atim GitHub repo            (default: zitsen/atim)
#   ZOXIDE_REPO         — zoxide GitHub repo          (default: ajeetdsouza/zoxide)
#   ATIM_ENABLE_SERVICE — enable user service         (default: true)
#   ATIM_START_SERVICE  — ask|true|false              (default: ask)
#   ATIM_DIR            — config directory             (default: ~/.atim)
#
# Optional non-interactive credential env vars:
#   ATIM_IM_BACKEND=telegram|feishu
#   ATIM_TELEGRAM_TOKEN=...
#   ATIM_ALLOWED_USERS=...
#   ATIM_FEISHU_APP_ID=...
#   ATIM_FEISHU_APP_SECRET=...

set -euo pipefail

INSTALL_DIR="${ATIM_BIN_DIR:-${HOME}/.local/bin}"
ATIM_REPO="${ATIM_REPO:-zitsen/atim}"
ATIM_VERSION="${ATIM_VERSION:-latest}"
ZOXIDE_REPO="${ZOXIDE_REPO:-ajeetdsouza/zoxide}"
ZOXIDE_VERSION="${ZOXIDE_VERSION:-latest}"
ATIM_ENABLE_SERVICE="${ATIM_ENABLE_SERVICE:-true}"
ATIM_START_SERVICE="${ATIM_START_SERVICE:-ask}"
ATIM_HOME="${ATIM_DIR:-${HOME}/.atim}"
ATIM_CONFIG_FILE="${ATIM_HOME}/config.toml"

WORK_DIR=""
ATIM_RESOLVED_VERSION=""
ZOXIDE_RESOLVED_VERSION=""
ATIM_DOWNLOAD_URL=""
ZOXIDE_DOWNLOAD_URL=""
IM_BACKEND=""
TELEGRAM_TOKEN=""
ALLOWED_USERS=""
FEISHU_APP_ID=""
FEISHU_APP_SECRET=""

# ── Utils ──

err()   { echo "[!] $*" >&2; }
info()  { echo "[*] $*"; }
has()   { command -v "$1" &>/dev/null; }

cleanup() {
  [[ -n "${WORK_DIR:-}" && -d "$WORK_DIR" ]] && rm -rf "$WORK_DIR"
}
trap cleanup EXIT

ensure_work_dir() {
  [[ -n "$WORK_DIR" ]] || WORK_DIR="$(mktemp -d)"
}

require_cmds() {
  local cmds=(curl tar sha256sum systemctl mktemp)
  local missing=0
  for cmd in "${cmds[@]}"; do
    if ! has "$cmd"; then
      err "missing required command: $cmd"
      missing=1
    fi
  done
  [[ "$missing" -eq 0 ]] || exit 1
}

is_truthy() {
  case "${1,,}" in
    1|true|yes|y|on) return 0 ;;
    *) return 1 ;;
  esac
}

is_falsy() {
  case "${1,,}" in
    0|false|no|n|off) return 0 ;;
    *) return 1 ;;
  esac
}

# ── Platform detection ──

detect_arch() {
  local arch
  arch="$(uname -m)"
  case "$arch" in
    x86_64|amd64) echo "x86_64" ;;
    aarch64|arm64) echo "aarch64" ;;
    *) err "unsupported architecture: $arch"; exit 1 ;;
  esac
}

detect_target() {
  local os
  os="$(uname -s)"
  case "$os" in
    Linux) ;;
    Darwin) err "installer does not support macOS yet."; exit 1 ;;
    *) err "unsupported OS: $os"; exit 1 ;;
  esac
  echo "$(detect_arch)-unknown-linux-musl"
}

fetch_latest_version() {
  local repo="$1"
  local tag
  tag="$(curl -fsSL "https://api.github.com/repos/${repo}/releases/latest" \
    | grep '"tag_name"' | cut -d'"' -f4)"
  [[ -n "$tag" ]] || {
    err "failed to fetch latest release from ${repo}"
    exit 1
  }
  echo "${tag#v}"
}

# ── Resolve download URLs ──

resolve_atim_url() {
  local target version
  target="$(detect_target)"
  version="${ATIM_VERSION#v}"

  if [[ "$ATIM_VERSION" == "latest" ]]; then
    info "Fetching latest atim release info ..." >&2
    version="$(fetch_latest_version "$ATIM_REPO")"
  fi

  ATIM_RESOLVED_VERSION="$version"
  ATIM_DOWNLOAD_URL="https://github.com/${ATIM_REPO}/releases/download/v${version}/atim-${target}.tar.gz"
}

resolve_zoxide_url() {
  local target version
  target="$(detect_target)"
  version="${ZOXIDE_VERSION#v}"

  if [[ "$ZOXIDE_VERSION" == "latest" ]]; then
    info "Fetching latest zoxide release info ..." >&2
    version="$(fetch_latest_version "$ZOXIDE_REPO")"
  fi

  ZOXIDE_RESOLVED_VERSION="$version"
  ZOXIDE_DOWNLOAD_URL="https://github.com/${ZOXIDE_REPO}/releases/download/v${version}/zoxide-${version}-${target}.tar.gz"
}

# ── Install binaries ──

install_atim_binary() {
  local url archive_name archive_path sha256_path extract_dir

  ensure_work_dir
  mkdir -p "$INSTALL_DIR"

  resolve_atim_url
  url="$ATIM_DOWNLOAD_URL"
  archive_name="$(basename "$url")"
  archive_path="${WORK_DIR}/${archive_name}"
  sha256_path="${archive_path}.sha256"
  extract_dir="${WORK_DIR}/atim-extract"

  info "Downloading atim ${ATIM_RESOLVED_VERSION} ..."
  info "  ${url}"
  curl -fsSL "$url" -o "$archive_path"
  curl -fsSL "${url}.sha256" -o "$sha256_path"

  (cd "$WORK_DIR" && sha256sum -c "${archive_name}.sha256" 2>/dev/null) || {
    err "atim checksum verification failed"
    exit 1
  }

  rm -rf "$extract_dir"
  mkdir -p "$extract_dir"
  tar xzf "$archive_path" -C "$extract_dir"
  [[ -f "${extract_dir}/atim" ]] || {
    err "atim binary not found in archive"
    exit 1
  }

  cp "${extract_dir}/atim" "${INSTALL_DIR}/atim"
  chmod +x "${INSTALL_DIR}/atim"
  info "Installed atim ${ATIM_RESOLVED_VERSION} to ${INSTALL_DIR}/atim"
}

install_zoxide_binary() {
  local url archive_name archive_path extract_dir

  ensure_work_dir
  mkdir -p "$INSTALL_DIR"

  resolve_zoxide_url
  url="$ZOXIDE_DOWNLOAD_URL"
  archive_name="$(basename "$url")"
  archive_path="${WORK_DIR}/${archive_name}"
  extract_dir="${WORK_DIR}/zoxide-extract"

  info "Downloading zoxide ${ZOXIDE_RESOLVED_VERSION} ..."
  info "  ${url}"
  curl -fsSL "$url" -o "$archive_path"

  rm -rf "$extract_dir"
  mkdir -p "$extract_dir"
  tar xzf "$archive_path" -C "$extract_dir"
  [[ -f "${extract_dir}/zoxide" ]] || {
    err "zoxide binary not found in archive"
    exit 1
  }

  cp "${extract_dir}/zoxide" "${INSTALL_DIR}/zoxide"
  chmod +x "${INSTALL_DIR}/zoxide"
  info "Installed zoxide ${ZOXIDE_RESOLVED_VERSION} to ${INSTALL_DIR}/zoxide"
}

# ── Interactive setup ──

ensure_tty() {
  [[ -r /dev/tty ]] || {
    err "interactive setup requires /dev/tty (or pre-set ATIM_IM_BACKEND and credentials)"
    exit 1
  }
}

prompt_input() {
  local __var="$1"
  local prompt="$2"
  local default="${3:-}"
  local secret="${4:-false}"
  local value=""

  ensure_tty

  if [[ -n "$default" ]]; then
    prompt="${prompt} [${default}]"
  fi

  if [[ "$secret" == "true" ]]; then
    read -r -s -p "${prompt}: " value < /dev/tty
    printf "\n" > /dev/tty
  else
    read -r -p "${prompt}: " value < /dev/tty
  fi

  [[ -n "$value" ]] || value="$default"
  printf -v "$__var" '%s' "$value"
}

prompt_yes_no() {
  local prompt="$1"
  local default_yes="$2"
  local hint default_value answer

  if [[ "$default_yes" == "true" ]]; then
    hint="Y/n"
    default_value="yes"
  else
    hint="y/N"
    default_value="no"
  fi

  ensure_tty
  while true; do
    read -r -p "${prompt} [${hint}]: " answer < /dev/tty
    answer="${answer,,}"
    [[ -n "$answer" ]] || answer="$default_value"
    case "$answer" in
      y|yes) return 0 ;;
      n|no) return 1 ;;
      *) info "Please answer yes or no." ;;
    esac
  done
}

collect_im_config() {
  local backend="${ATIM_IM_BACKEND:-}"
  backend="${backend,,}"

  if [[ -z "$backend" ]]; then
    while true; do
      prompt_input backend "Choose IM backend (telegram/feishu)" "telegram"
      backend="${backend,,}"
      case "$backend" in
        telegram|feishu) break ;;
        *) err "invalid backend: ${backend} (expected telegram or feishu)" ;;
      esac
    done
  fi

  case "$backend" in
    telegram)
      IM_BACKEND="telegram"
      TELEGRAM_TOKEN="${ATIM_TELEGRAM_TOKEN:-}"
      [[ -n "$TELEGRAM_TOKEN" ]] || prompt_input TELEGRAM_TOKEN "Telegram bot token" "" "true"
      [[ -n "$TELEGRAM_TOKEN" ]] || {
        err "telegram bot token cannot be empty"
        exit 1
      }

      if [[ -n "${ATIM_ALLOWED_USERS+x}" ]]; then
        ALLOWED_USERS="${ATIM_ALLOWED_USERS}"
      else
        prompt_input ALLOWED_USERS "Telegram allowed user IDs (comma-separated, empty = allow all)"
      fi
      FEISHU_APP_ID=""
      FEISHU_APP_SECRET=""
      ;;
    feishu)
      IM_BACKEND="feishu"
      FEISHU_APP_ID="${ATIM_FEISHU_APP_ID:-}"
      FEISHU_APP_SECRET="${ATIM_FEISHU_APP_SECRET:-}"
      [[ -n "$FEISHU_APP_ID" ]] || prompt_input FEISHU_APP_ID "Feishu App ID"
      [[ -n "$FEISHU_APP_SECRET" ]] || prompt_input FEISHU_APP_SECRET "Feishu App Secret" "" "true"
      [[ -n "$FEISHU_APP_ID" ]] || {
        err "feishu app id cannot be empty"
        exit 1
      }
      [[ -n "$FEISHU_APP_SECRET" ]] || {
        err "feishu app secret cannot be empty"
        exit 1
      }
      TELEGRAM_TOKEN=""
      ALLOWED_USERS=""
      ;;
    *)
      err "invalid ATIM_IM_BACKEND: ${backend} (expected telegram or feishu)"
      exit 1
      ;;
  esac
}

escape_toml_string() {
  local value="$1"
  value="${value//\\/\\\\}"
  value="${value//\"/\\\"}"
  printf '%s' "$value"
}

write_config_toml() {
  local backend token users app_id app_secret

  backend="$(escape_toml_string "$IM_BACKEND")"
  token="$(escape_toml_string "$TELEGRAM_TOKEN")"
  users="$(escape_toml_string "$ALLOWED_USERS")"
  app_id="$(escape_toml_string "$FEISHU_APP_ID")"
  app_secret="$(escape_toml_string "$FEISHU_APP_SECRET")"

  mkdir -p "$ATIM_HOME"
  cat > "$ATIM_CONFIG_FILE" <<EOF
[im]
backend = "${backend}"

[im.feishu]
app_id = "${app_id}"
app_secret = "${app_secret}"

[im.telegram]
token = "${token}"
allowed_users = "${users}"

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
EOF
  chmod 600 "$ATIM_CONFIG_FILE"
  info "Wrote ${ATIM_CONFIG_FILE}"
}

# ── Service setup ──

install_user_service() {
  local atim_bin="${INSTALL_DIR}/atim"
  info "Installing user-level systemd service ..."
  "$atim_bin" service --install
}

enable_user_service() {
  if is_falsy "$ATIM_ENABLE_SERVICE"; then
    info "Skipping service enable (ATIM_ENABLE_SERVICE=${ATIM_ENABLE_SERVICE})"
    return
  fi

  info "Enabling user-level atim.service ..."
  systemctl --user enable atim.service
}

start_user_service_if_requested() {
  local start_mode="${ATIM_START_SERVICE,,}"

  if [[ "$start_mode" == "ask" ]]; then
    if prompt_yes_no "Start atim user service now?" "true"; then
      start_mode="true"
    else
      start_mode="false"
    fi
  fi

  if is_truthy "$start_mode"; then
    info "Starting user-level atim.service ..."
    systemctl --user start atim.service
  elif is_falsy "$start_mode"; then
    info "Skipped starting service. Run: systemctl --user start atim.service"
  else
    err "invalid ATIM_START_SERVICE value: ${ATIM_START_SERVICE} (expected ask/true/false)"
    exit 1
  fi
}

# ── Post-install hints ──

print_path_hint() {
  local needs_export=0
  case ":${PATH}:" in
    *:"${INSTALL_DIR}":*) ;;
    *) needs_export=1 ;;
  esac

  if [[ $needs_export -eq 1 ]]; then
    echo ""
    info "${INSTALL_DIR} is not in your PATH."
    info "Add the following line to your shell config (~/.bashrc, ~/.zshrc):"
    echo ""
    echo "    export PATH=\"${INSTALL_DIR}:\$PATH\""
    echo ""
  fi
}

print_zoxide_hint() {
  local shell_name
  shell_name="$(basename "${SHELL:-}")"
  echo ""
  info "Enable zoxide in your shell startup file:"
  case "$shell_name" in
    zsh)  echo "    eval \"\$(zoxide init zsh)\"" ;;
    bash) echo "    eval \"\$(zoxide init bash)\"" ;;
    fish) echo "    zoxide init fish | source" ;;
    *)    echo "    eval \"\$(zoxide init ${shell_name})\"" ;;
  esac
  echo ""
}

# ── Flags ──

while [[ $# -gt 0 ]]; do
  case "$1" in
    -b)
      INSTALL_DIR="$2"
      shift 2
      ;;
    -h|--help)
      echo "Usage: bash install.sh [-b <install-dir>]"
      echo ""
      echo "  -b <dir>   Install binaries to <dir> (default: ~/.local/bin)"
      exit 0
      ;;
    *)
      err "unknown flag: $1"
      exit 1
      ;;
  esac
done

# ── Main ──

require_cmds
install_atim_binary
install_zoxide_binary
collect_im_config
write_config_toml
install_user_service
enable_user_service
start_user_service_if_requested
print_path_hint
print_zoxide_hint

info "Done. Run 'atim --help' to get started."
