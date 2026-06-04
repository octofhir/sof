#!/bin/sh
# octofhir-sof installer.
#
#   curl -fsSL https://raw.githubusercontent.com/octofhir/sof/main/install.sh | sh
#
# Downloads the latest release binary for this platform, verifies its SHA-256,
# and installs it. Override the destination with OCTOFHIR_SOF_INSTALL_DIR.
set -eu

REPO="octofhir/sof"
BIN="octofhir-sof"
INSTALL_DIR="${OCTOFHIR_SOF_INSTALL_DIR:-$HOME/.local/bin}"

err() { echo "octofhir-sof-install: $*" >&2; exit 1; }
have() { command -v "$1" >/dev/null 2>&1; }

os=$(uname -s)
arch=$(uname -m)
case "$os" in
  Linux)
    case "$arch" in
      x86_64|amd64)  target="x86_64-unknown-linux-gnu" ;;
      aarch64|arm64) target="aarch64-unknown-linux-gnu" ;;
      *) err "unsupported Linux arch: $arch" ;;
    esac ;;
  Darwin)
    case "$arch" in
      x86_64|amd64)  target="x86_64-apple-darwin" ;;
      arm64|aarch64) target="aarch64-apple-darwin" ;;
      *) err "unsupported macOS arch: $arch" ;;
    esac ;;
  *) err "unsupported OS: $os (use the Windows .zip from the releases page)" ;;
esac

archive="${BIN}-${target}.tar.gz"
base="https://github.com/${REPO}/releases/latest/download"
tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT

dl() {
  if have curl; then curl -fsSL "$1" -o "$2"
  elif have wget; then wget -qO "$2" "$1"
  else err "need curl or wget"; fi
}

echo "octofhir-sof-install: downloading $archive"
dl "$base/$archive" "$tmp/$archive"
dl "$base/$archive.sha256" "$tmp/$archive.sha256" || err "could not fetch checksum"

# Verify SHA-256 (the .sha256 file holds the bare hash or '<hash>  <file>').
expected=$(awk '{print $1}' "$tmp/$archive.sha256")
if have sha256sum; then actual=$(sha256sum "$tmp/$archive" | awk '{print $1}')
elif have shasum; then actual=$(shasum -a 256 "$tmp/$archive" | awk '{print $1}')
else err "need sha256sum or shasum to verify the download"; fi
[ "$expected" = "$actual" ] || err "checksum mismatch (expected $expected, got $actual)"

tar -xzf "$tmp/$archive" -C "$tmp"
mkdir -p "$INSTALL_DIR"
install -m 0755 "$tmp/$BIN" "$INSTALL_DIR/$BIN" 2>/dev/null \
  || { cp "$tmp/$BIN" "$INSTALL_DIR/$BIN" && chmod 0755 "$INSTALL_DIR/$BIN"; }

echo "octofhir-sof-install: installed $("$INSTALL_DIR/$BIN" --version 2>/dev/null || echo "$BIN") to $INSTALL_DIR"
case ":$PATH:" in
  *":$INSTALL_DIR:"*) ;;
  *) echo "octofhir-sof-install: add $INSTALL_DIR to your PATH:  export PATH=\"$INSTALL_DIR:\$PATH\"" ;;
esac
