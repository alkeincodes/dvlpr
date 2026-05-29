#!/usr/bin/env bash
# Operator helper: bump Cargo.toml + Cargo.lock to a new version, commit,
# tag. Does NOT push — the operator does that explicitly so they can do
# one final review before triggering the release workflow.
#
# Usage:
#   ./scripts/release.sh 0.2.1
#
# After this returns, run:
#   git push origin main v0.2.1
#
# That triggers .github/workflows/release.yml which builds 4 target
# tarballs and creates the GH release.

set -euo pipefail

NEW_VERSION="${1:-}"
[ -n "${NEW_VERSION}" ] || { echo "usage: ./scripts/release.sh <version>" >&2; exit 1; }
echo "${NEW_VERSION}" | grep -qE '^[0-9]+\.[0-9]+\.[0-9]+$' \
  || { echo "version must be MAJOR.MINOR.PATCH" >&2; exit 1; }

# Sanity checks.
[ -z "$(git status --porcelain)" ] || { echo "working tree is dirty" >&2; exit 1; }
[ "$(git branch --show-current)" = main ] || { echo "must be on main" >&2; exit 1; }
git fetch --tags --quiet
if git rev-parse "v${NEW_VERSION}" >/dev/null 2>&1; then
  echo "tag v${NEW_VERSION} already exists" >&2
  exit 1
fi

# Bump Cargo.toml [package] version. Match the exact line shape to avoid
# rewriting any embedded version strings in dependencies.
MATCHES=$(grep -cE '^version = "[0-9]+\.[0-9]+\.[0-9]+"$' Cargo.toml)
[ "$MATCHES" = "1" ] || { echo "Cargo.toml has $MATCHES version lines; expected exactly 1" >&2; exit 1; }
CURRENT=$(grep -E '^version = "[0-9]+\.[0-9]+\.[0-9]+"$' Cargo.toml | sed -E 's/^version = "(.*)"$/\1/')
[ "$CURRENT" != "$NEW_VERSION" ] || { echo "Cargo.toml already at ${NEW_VERSION}; nothing to bump" >&2; exit 1; }
sed -i.bak -E 's/^version = "[0-9]+\.[0-9]+\.[0-9]+"$/version = "'"${NEW_VERSION}"'"/' Cargo.toml
# Rollback guard for the cargo-check step ONLY. If cargo check fails after
# the sed bump, the EXIT trap restores Cargo.toml from .bak. Once cargo
# check succeeds, the bump is internally consistent — the .bak and the
# trap are both removed. A failure during git add/commit/tag (the steps
# after cargo check) leaves Cargo.toml at the new version with no commit
# made; the operator recovers manually with `git checkout Cargo.toml
# Cargo.lock`.
rollback() {
  if [ -f Cargo.toml.bak ]; then
    mv Cargo.toml.bak Cargo.toml
    echo "release.sh: cargo check failed — rolled back Cargo.toml (no commit was made)" >&2
  fi
}
trap rollback EXIT

# Refresh Cargo.lock so the package's own version line updates.
export ZIG="${ZIG:-$HOME/.local/zig-aarch64-macos-0.15.2/zig}"
cargo check --quiet >/dev/null 2>&1

# cargo check passed → the bump is internally consistent. Discard the
# backup and clear the trap; from here on, git failures are reported but
# not automatically rolled back (operator runs `git checkout` manually).
rm -f Cargo.toml.bak
trap - EXIT

git add Cargo.toml Cargo.lock
git commit -m "release: v${NEW_VERSION}"
git tag "v${NEW_VERSION}"

cat <<MSG

Tagged v${NEW_VERSION} (commit $(git rev-parse --short HEAD)).

To publish, run:

    git push origin main v${NEW_VERSION}

The release workflow will then build all 4 target tarballs and create
the GitHub release.
MSG
