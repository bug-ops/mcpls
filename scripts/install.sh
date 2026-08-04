#!/bin/sh
# mcpls installer for Linux and macOS.
#
# Usage:
#   curl -fsSL https://raw.githubusercontent.com/bug-ops/mcpls/main/scripts/install.sh | sh
#
# Environment variables:
#   MCPLS_INSTALL_DIR   Directory to install the binary into (default: "$HOME/.local/bin").
#   MCPLS_VERSION        Release tag to install, e.g. "v0.3.8" (default: latest release).
#
# This script is POSIX sh (no bashisms) so it runs under `sh`, `dash`, and `bash`.

set -eu

REPO="bug-ops/mcpls"
BIN_NAME="mcpls"
INSTALL_DIR="${MCPLS_INSTALL_DIR:-$HOME/.local/bin}"
VERSION="${MCPLS_VERSION:-latest}"

err() {
    printf 'error: %s\n' "$1" >&2
    exit 1
}

info() {
    printf '%s\n' "$1"
}

need_cmd() {
    if ! command -v "$1" >/dev/null 2>&1; then
        err "required command '$1' not found in PATH"
    fi
}

detect_os() {
    uname_s=$(uname -s)
    case "$uname_s" in
        Linux) echo "linux" ;;
        Darwin) echo "macos" ;;
        MINGW* | MSYS* | CYGWIN*)
            err "Windows detected. Use the PowerShell installer instead: https://raw.githubusercontent.com/${REPO}/main/scripts/install.ps1"
            ;;
        *) err "unsupported operating system: $uname_s" ;;
    esac
}

detect_arch() {
    uname_m=$(uname -m)
    case "$uname_m" in
        x86_64 | amd64) echo "x86_64" ;;
        arm64 | aarch64) echo "aarch64" ;;
        *) err "unsupported architecture: $uname_m" ;;
    esac
}

target_triple() {
    os="$1"
    arch="$2"
    case "$os" in
        linux) echo "${arch}-unknown-linux-gnu" ;;
        macos) echo "${arch}-apple-darwin" ;;
    esac
}

main() {
    need_cmd curl
    need_cmd tar
    need_cmd mktemp
    need_cmd awk
    need_cmd install

    if command -v sha256sum >/dev/null 2>&1; then
        sha256_cmd="sha256sum"
    elif command -v shasum >/dev/null 2>&1; then
        sha256_cmd="shasum -a 256"
    else
        err "need either 'sha256sum' or 'shasum' to verify the downloaded archive"
    fi

    os=$(detect_os)
    arch=$(detect_arch)
    target=$(target_triple "$os" "$arch")
    archive="${BIN_NAME}-${target}.tar.gz"

    if [ "$VERSION" = "latest" ]; then
        base_url="https://github.com/${REPO}/releases/latest/download"
        version_label="latest"
    else
        base_url="https://github.com/${REPO}/releases/download/${VERSION}"
        version_label="$VERSION"
    fi

    info "Installing mcpls (${version_label}) for ${target}..."

    tmp_dir=$(mktemp -d)
    trap 'rm -rf "$tmp_dir"' EXIT INT TERM

    archive_path="${tmp_dir}/${archive}"
    checksum_path="${archive_path}.sha256"

    info "Downloading ${archive}..."
    curl -fsSL "${base_url}/${archive}" -o "$archive_path" \
        || err "failed to download ${base_url}/${archive}"
    curl -fsSL "${base_url}/${archive}.sha256" -o "$checksum_path" \
        || err "failed to download ${base_url}/${archive}.sha256"

    info "Verifying checksum..."
    (
        cd "$tmp_dir"
        expected=$(awk '{print $1}' "$(basename "$checksum_path")")
        actual=$($sha256_cmd "$(basename "$archive_path")" | awk '{print $1}')
        if [ "$expected" != "$actual" ]; then
            err "checksum mismatch for ${archive}: expected ${expected}, got ${actual}"
        fi
    )

    info "Extracting..."
    tar xzf "$archive_path" -C "$tmp_dir" "$BIN_NAME"

    mkdir -p "$INSTALL_DIR"
    install -m 755 "${tmp_dir}/${BIN_NAME}" "${INSTALL_DIR}/${BIN_NAME}"

    info "Installed mcpls to ${INSTALL_DIR}/${BIN_NAME}"

    case ":$PATH:" in
        *":$INSTALL_DIR:"*) ;;
        *)
            info ""
            info "Warning: ${INSTALL_DIR} is not on your PATH."
            info "Add this to your shell profile (e.g. ~/.bashrc, ~/.zshrc):"
            info "  export PATH=\"${INSTALL_DIR}:\$PATH\""
            ;;
    esac

    info ""
    "${INSTALL_DIR}/${BIN_NAME}" --version
}

main "$@"
