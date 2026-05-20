#!/bin/bash
#
# Atim installer — downloads the latest release binary.
#
# Usage:
#   curl -fsSL https://raw.githubusercontent.com/huolinhe/atim/main/install.sh | bash
#   curl -fsSL https://raw.githubusercontent.com/huolinhe/atim/main/install.sh | bash -s -- -b /usr/local/bin
#
# Environment variables:
#   ATIM_VERSION  — version to install (default: latest)
#   ATIM_BIN_DIR  — install directory  (default: see -b flag)

set -euo pipefail

INSTALL_DIR="${ATIM_BIN_DIR:-${HOME}/.local/bin}"
REPO="huolinhe/atim"
VERSION="${ATIM_VERSION:-latest}"

# ── Utils ──

err()   { echo "[!] $*" >&2; }
info()  { echo "[*] $*"; }
has()   { command -v "$1" &>/dev/null; }

cleanup() {
  [[ -n "${TMPDIR:-}" && -d "$TMPDIR" ]] && rm -rf "$TMPDIR"
}
trap cleanup EXIT

# ── Platform detection ──

detect_arch() {
  local arch
  arch="$(uname -m)"
  case "$arch" in
    x86_64|amd64)  echo "x86_64" ;;
    aarch64|arm64)  echo "aarch64" ;;
    *) err "unsupported architecture: $arch"; exit 1 ;;
  esac
}

detect_os() {
  local os
  os="$(uname -s)"
  case "$os" in
    Linux)  echo "unknown-linux-musl" ;;
    Darwin) err "atim does not support macOS yet."; exit 1 ;;
    *)      err "unsupported OS: $os"; exit 1 ;;
  esac
}

# ── Resolve download URL ──

resolve_url() {
  local arch os target tag url
  arch="$(detect_arch)"
  os="$(detect_os)"
  target="${arch}-${os}"

  if [[ "$VERSION" == "latest" ]]; then
    info "Fetching latest release info ..."
    tag="$(curl -fsSL "https://api.github.com/repos/${REPO}/releases/latest" \
            | grep '"tag_name"' | cut -d'"' -f4)"
    [[ -z "$tag" ]] && { err "failed to fetch latest release"; exit 1; }
    VERSION="${tag#v}"
  fi

  url="https://github.com/${REPO}/releases/download/v${VERSION}/atim-${target}.tar.gz"
  echo "$url"
}

# ── Install ──

install_binary() {
  local url archive sha256_url sha256_remote sha256_file

  url=$(resolve_url)
  archive="$(basename "$url")"
  sha256_url="${url}.sha256"

  info "Downloading atim ${VERSION} ..."
  info "  ${url}"

  TMPDIR="$(mktemp -d)"

  curl -fsSL "$url" -o "${TMPDIR}/${archive}"
  curl -fsSL "$sha256_url" -o "${TMPDIR}/${archive}.sha256"

  # Verify checksum
  (cd "$TMPDIR" && sha256sum -c "$archive.sha256" 2>/dev/null) || {
    err "checksum verification failed"
    exit 1
  }

  info "Extracting ..."
  tar xzf "${TMPDIR}/${archive}" -C "$TMPDIR"

  mkdir -p "$INSTALL_DIR"
  mv "${TMPDIR}/atim" "${INSTALL_DIR}/atim"
  chmod +x "${INSTALL_DIR}/atim"

  info "Installed atim ${VERSION} to ${INSTALL_DIR}/atim"
}

# ── Post-install hint ──

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

# ── Flags ──

while [[ $# -gt 0 ]]; do
  case "$1" in
    -b) INSTALL_DIR="$2"; shift 2 ;;
    -h|--help)
      echo "Usage: bash install.sh [-b <install-dir>]"
      echo ""
      echo "  -b <dir>   Install binary to <dir> (default: ~/.local/bin)"
      exit 0
      ;;
    *) err "unknown flag: $1"; exit 1 ;;
  esac
done

# ── Main ──

install_binary
print_path_hint

info "Done. Run 'atim --help' to get started."
