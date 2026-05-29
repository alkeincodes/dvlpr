#!/usr/bin/env bash
# Mac-side helper: package the dvlpr source, template install.sh with the
# right source URL, and serve both over HTTP so a remote host can install
# via `curl -fsSL http://<mac>/install.sh | bash`.
#
# Usage:
#   ./scripts/serve.sh                  # auto-detect Mac LAN IP on en0, port 8080
#   ./scripts/serve.sh 9000             # custom port
#   MAC_URL=https://my.tunnel ./scripts/serve.sh   # override base URL (cloudflared, tailscale, ...)
#
# Stop with Ctrl-C.

set -euo pipefail

PORT="${1:-8080}"
DIST="/tmp/dvlpr-installer"
REPO_ROOT="$(cd "$(dirname "$0")/.." && pwd)"
TEMPLATE="${REPO_ROOT}/scripts/dev-install.sh"

[ -f "${TEMPLATE}" ] || { echo "Missing ${TEMPLATE}" >&2; exit 1; }

# === Resolve the URL the remote will hit ===
if [ -n "${MAC_URL:-}" ]; then
  BASE_URL="${MAC_URL%/}"
else
  LAN_IP="$(ipconfig getifaddr en0 2>/dev/null || true)"
  if [ -z "${LAN_IP}" ]; then
    LAN_IP="$(ipconfig getifaddr en1 2>/dev/null || true)"
  fi
  if [ -z "${LAN_IP}" ]; then
    echo "Couldn't auto-detect Mac LAN IP on en0/en1." >&2
    echo "Set MAC_URL=http://<host>:${PORT} (or use a tunnel like cloudflared/tailscale)." >&2
    exit 1
  fi
  BASE_URL="http://${LAN_IP}:${PORT}"
fi

# === Build the tarball (source only, no target/, no .git/, no .claude/) ===
echo "Packaging source from ${REPO_ROOT}..."
rm -rf "${DIST}"
mkdir -p "${DIST}"
tar -czf "${DIST}/dvlpr-src.tar.gz" \
  --exclude='target' \
  --exclude='.git' \
  --exclude='.claude' \
  --exclude='.DS_Store' \
  -C "$(dirname "${REPO_ROOT}")" "$(basename "${REPO_ROOT}")"
SRC_SIZE=$(du -h "${DIST}/dvlpr-src.tar.gz" | cut -f1)
echo "  dvlpr-src.tar.gz  (${SRC_SIZE})"

# === Template install.sh with the source URL ===
sed "s|@DVLPR_SRC_URL@|${BASE_URL}/dvlpr-src.tar.gz|g" \
  "${TEMPLATE}" > "${DIST}/install.sh"
chmod +x "${DIST}/install.sh"
echo "  install.sh        (templated)"

# === Optionally include the user's prefix config ===
if [ -f "$HOME/.dvlpr/config.toml" ]; then
  cp "$HOME/.dvlpr/config.toml" "${DIST}/config.toml"
  echo "  config.toml       (from ~/.dvlpr/)"
fi

# === Serve ===
cat <<EOF

────────────────────────────────────────────────────────────
Serving ${DIST} on ${BASE_URL}

On the remote host, run:

    curl -fsSL ${BASE_URL}/install.sh | bash

Ctrl-C here to stop the server.
────────────────────────────────────────────────────────────

EOF

cd "${DIST}"
exec python3 -m http.server "${PORT}" --bind 0.0.0.0
