#!/usr/bin/env bash
# dvlpr remote installer — fetches a prebuilt binary from GitHub Releases
# and places it on PATH. Designed for `curl -fsSL <url> | bash`.
#
# What it does:
#   1. Detect OS/arch via `uname -sm`.
#   2. curl the matching dvlpr-<arch>-<os>.tar.gz from the latest release.
#   3. curl the matching .sha256, verify (sha256sum on Linux, shasum on macOS).
#   4. tar -xzf, then install -m 0755 to /usr/local/bin (with sudo) or
#      ~/.local/bin (fallback when sudo isn't available).
#   5. On macOS only: xattr -d com.apple.quarantine (Gatekeeper bypass).
#   6. Print `dvlpr --version` as confirmation.

set -euo pipefail

REPO="${DVLPR_REPO:-alkeincodes/dvlpr}"
BASE_URL="https://github.com/${REPO}/releases/latest/download"

case "$(uname -s)-$(uname -m)" in
  Linux-x86_64)   ASSET="dvlpr-x86_64-linux.tar.gz" ;;
  Linux-aarch64)  ASSET="dvlpr-aarch64-linux.tar.gz" ;;
  Darwin-arm64)   ASSET="dvlpr-aarch64-macos.tar.gz" ;;
  Darwin-x86_64)  ASSET="dvlpr-x86_64-macos.tar.gz" ;;
  *) echo "Unsupported: $(uname -sm). dvlpr ships binaries for Linux x86_64/aarch64 and macOS x86_64/aarch64." >&2; exit 1 ;;
esac

say() { printf '\033[1;36m==>\033[0m %s\n' "$*"; }

TMP=$(mktemp -d); trap 'rm -rf "${TMP}"' EXIT
cd "${TMP}"

say "Fetching ${ASSET} from ${REPO}"
curl -fsSL -o "${ASSET}"        "${BASE_URL}/${ASSET}"
curl -fsSL -o "${ASSET}.sha256" "${BASE_URL}/${ASSET}.sha256"

say "Verifying SHA-256"
if command -v sha256sum >/dev/null 2>&1; then
  sha256sum -c "${ASSET}.sha256"
else
  shasum -a 256 -c "${ASSET}.sha256"
fi

tar -xzf "${ASSET}"

if sudo -n true 2>/dev/null || sudo -v 2>/dev/null; then
  say "Installing to /usr/local/bin/dvlpr"
  sudo install -m 0755 dvlpr /usr/local/bin/dvlpr
  DEST=/usr/local/bin/dvlpr
else
  say "Installing to ~/.local/bin/dvlpr (no sudo)"
  mkdir -p "$HOME/.local/bin"
  install -m 0755 dvlpr "$HOME/.local/bin/dvlpr"
  DEST="$HOME/.local/bin/dvlpr"
  if ! grep -qs '\.local/bin' "$HOME/.bashrc" 2>/dev/null; then
    echo 'export PATH="$HOME/.local/bin:$PATH"' >> "$HOME/.bashrc"
    say "Added ~/.local/bin to PATH in ~/.bashrc — re-login for non-interactive SSH to pick it up."
  fi
fi

# macOS Gatekeeper: best-effort quarantine strip.
[ "$(uname -s)" = Darwin ] && xattr -d com.apple.quarantine "${DEST}" 2>/dev/null || true

say "Installed"
"${DEST}" --version
