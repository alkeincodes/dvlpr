#!/usr/bin/env bash
# dvlpr remote installer for Linux (x86_64, aarch64).
#
# Templated by scripts/serve.sh — DVLPR_SRC_URL below is replaced with the
# URL of your Mac's HTTP server. Designed for `curl ... | bash` invocation.
#
# What it does:
#   1. Installs build prereqs via apt (build-essential, libclang-dev, ...).
#   2. Installs the Rust toolchain via rustup (skipped if already present).
#   3. Downloads Zig 0.15.2 — exact version required by libghostty-vt.
#   4. Fetches the dvlpr source tarball from the Mac.
#   5. Builds the release binary.
#   6. Installs to /usr/local/bin/dvlpr (with sudo) or ~/.local/bin/dvlpr.
#   7. Optionally fetches ~/.dvlpr/config.toml from the same Mac, if served.
#   8. Smoke-tests `dvlpr ls`.

set -euo pipefail

# === Templated by serve.sh ===
DVLPR_SRC_URL="@DVLPR_SRC_URL@"

# === Detect arch ===
KERNEL=$(uname -s)
ARCH=$(uname -m)
case "${KERNEL}-${ARCH}" in
  Linux-x86_64)
    ZIG_DIR="zig-x86_64-linux-0.15.2"
    ;;
  Linux-aarch64)
    ZIG_DIR="zig-aarch64-linux-0.15.2"
    ;;
  *)
    echo "Unsupported platform: ${KERNEL}-${ARCH}. Only Linux x86_64 and aarch64 are supported." >&2
    exit 1
    ;;
esac
ZIG_URL="https://ziglang.org/download/0.15.2/${ZIG_DIR}.tar.xz"

say() { printf '\033[1;36m==>\033[0m %s\n' "$*"; }

# === 1. Build deps ===
say "Installing build prereqs (apt)..."
sudo apt-get update -qq
sudo DEBIAN_FRONTEND=noninteractive apt-get install -y --no-install-recommends \
  build-essential libclang-dev pkg-config curl xz-utils ca-certificates

# === 2. Rust ===
if ! command -v cargo >/dev/null 2>&1; then
  say "Installing Rust (rustup)..."
  curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs \
    | sh -s -- -y --default-toolchain stable --profile minimal
else
  say "Rust already present: $(cargo --version)"
fi
# shellcheck disable=SC1091
. "$HOME/.cargo/env"

# === 3. Zig 0.15.2 (exact) ===
ZIG_HOME="$HOME/.local/${ZIG_DIR}"
if [ ! -x "${ZIG_HOME}/zig" ]; then
  say "Installing Zig 0.15.2..."
  mkdir -p "$HOME/.local"
  curl -fsSL -o /tmp/zig.tar.xz "${ZIG_URL}"
  tar -xf /tmp/zig.tar.xz -C "$HOME/.local/"
  rm -f /tmp/zig.tar.xz
else
  say "Zig 0.15.2 already at ${ZIG_HOME}"
fi
export ZIG="${ZIG_HOME}/zig"
"${ZIG}" version | grep -q '^0\.15\.2$' \
  || { echo "Zig version mismatch — expected 0.15.2, got $("${ZIG}" version)" >&2; exit 1; }

# === 4. Source ===
SRC_DIR="$HOME/dvlpr-src"
say "Fetching source from ${DVLPR_SRC_URL}..."
rm -rf "${SRC_DIR}"
mkdir -p "${SRC_DIR}"
curl -fsSL "${DVLPR_SRC_URL}" | tar -xz -C "${SRC_DIR}" --strip-components=1

# === 5. Build ===
say "Building (cargo build --release)..."
cd "${SRC_DIR}"
cargo build --release

BIN="target/release/dvlpr"
if [ ! -x "${BIN}" ]; then
  echo "Build did not produce ${BIN}" >&2
  exit 1
fi

# === 6. Install ===
if sudo -n true 2>/dev/null; then
  say "Installing to /usr/local/bin/dvlpr..."
  sudo install -m 0755 "${BIN}" /usr/local/bin/dvlpr
else
  # No passwordless sudo — try a TTY prompt first; fall back to ~/.local/bin.
  if sudo -v 2>/dev/null; then
    say "Installing to /usr/local/bin/dvlpr (sudo)..."
    sudo install -m 0755 "${BIN}" /usr/local/bin/dvlpr
  else
    say "Installing to ~/.local/bin/dvlpr (no sudo)..."
    mkdir -p "$HOME/.local/bin"
    install -m 0755 "${BIN}" "$HOME/.local/bin/dvlpr"
    if ! grep -qs '\.local/bin' "$HOME/.bashrc"; then
      echo 'export PATH="$HOME/.local/bin:$PATH"' >> "$HOME/.bashrc"
      say "Added ~/.local/bin to PATH in ~/.bashrc — re-login for non-interactive SSH to pick it up."
    fi
  fi
fi

# === 7. Optional config ===
CONFIG_URL="${DVLPR_SRC_URL%/*}/config.toml"
if curl -fsSL --output /dev/null --silent --head --fail "${CONFIG_URL}" 2>/dev/null; then
  say "Fetching config.toml..."
  mkdir -p "$HOME/.dvlpr"
  curl -fsSL "${CONFIG_URL}" -o "$HOME/.dvlpr/config.toml"
fi

# === 8. Smoke test ===
say "Smoke test..."
hash -r 2>/dev/null || true
if command -v dvlpr >/dev/null 2>&1; then
  dvlpr ls
  say "Installed at: $(command -v dvlpr)"
else
  echo "WARNING: 'dvlpr' not on PATH in this shell — open a fresh login to pick it up." >&2
fi

say "Done."
