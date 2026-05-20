#!/usr/bin/env sh
# Install Giant - downloads the appropriate prebuilt binary from GitHub
# releases for your OS/arch and drops it on your PATH.
#
# Usage:
#   curl -fsSL https://giant.build/install.sh | sh
#
# Override the version:
#   curl -fsSL https://giant.build/install.sh | GIANT_VERSION=0.1.0 sh
#
# Override the install dir:
#   curl -fsSL https://giant.build/install.sh | GIANT_INSTALL_DIR=$HOME/bin sh

set -eu

REPO="johnae/giant"
VERSION="${GIANT_VERSION:-latest}"

err() { printf 'install: %s\n' "$*" >&2; exit 1; }
info() { printf 'install: %s\n' "$*" >&2; }

# --- detect platform ---
OS="$(uname -s | tr '[:upper:]' '[:lower:]')"
ARCH="$(uname -m)"

case "$OS" in
  linux)
    case "$ARCH" in
      x86_64|amd64) TRIPLE="x86_64-unknown-linux-musl" ;;
      aarch64|arm64) TRIPLE="aarch64-unknown-linux-gnu" ;;
      *) err "unsupported linux arch: $ARCH" ;;
    esac
    ;;
  darwin)
    case "$ARCH" in
      x86_64) TRIPLE="x86_64-apple-darwin" ;;
      arm64) TRIPLE="aarch64-apple-darwin" ;;
      *) err "unsupported macOS arch: $ARCH" ;;
    esac
    ;;
  *) err "unsupported OS: $OS (Windows: download the .exe from GitHub releases)" ;;
esac

# --- resolve install dir ---
if [ -n "${GIANT_INSTALL_DIR:-}" ]; then
  DEST="$GIANT_INSTALL_DIR"
elif [ -w "/usr/local/bin" ] 2>/dev/null; then
  DEST="/usr/local/bin"
else
  DEST="$HOME/.local/bin"
  mkdir -p "$DEST"
fi

# --- resolve version ---
if [ "$VERSION" = "latest" ]; then
  RELEASE_URL="https://api.github.com/repos/$REPO/releases/latest"
  if command -v jq >/dev/null 2>&1; then
    VERSION="$(curl -fsSL "$RELEASE_URL" | jq -r .tag_name)"
  else
    VERSION="$(curl -fsSL "$RELEASE_URL" | grep -o '"tag_name": *"[^"]*"' | head -1 | sed 's/.*"\([^"]*\)"$/\1/')"
  fi
  [ -n "$VERSION" ] || err "could not resolve latest version"
  # Strip leading "v" if present.
  VERSION="${VERSION#v}"
fi

# --- download + verify ---
TARBALL="giant-${VERSION}-${TRIPLE}.tar.gz"
URL="https://github.com/$REPO/releases/download/v${VERSION}/$TARBALL"
SUMS_URL="https://github.com/$REPO/releases/download/v${VERSION}/SHA256SUMS"

TMPDIR="$(mktemp -d)"
trap 'rm -rf "$TMPDIR"' EXIT

info "downloading giant $VERSION for $TRIPLE"
curl -fsSL --output "$TMPDIR/$TARBALL" "$URL"
curl -fsSL --output "$TMPDIR/SHA256SUMS" "$SUMS_URL" || info "warning: no SHA256SUMS file; skipping checksum verification"

if [ -f "$TMPDIR/SHA256SUMS" ]; then
  (cd "$TMPDIR" && grep " $TARBALL\$" SHA256SUMS | sha256sum -c -)
fi

# --- extract + install ---
tar -xzf "$TMPDIR/$TARBALL" -C "$TMPDIR"
install -m 0755 "$TMPDIR/giant" "$DEST/giant"

info "installed giant $VERSION to $DEST/giant"

# Hint about PATH if needed.
case ":$PATH:" in
  *":$DEST:"*) ;;
  *) info "note: $DEST is not on your PATH; add it to your shell profile" ;;
esac

"$DEST/giant" --version
