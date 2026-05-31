---
description: Audit the working tree for release readiness (version, release-channel integrity, build gates, README drift, hygiene) and emit a READY / NOT READY verdict.
---

# Pre-Release Audit

Read-only audit run before tagging a dvlpr release. It NEVER tags, commits, or
pushes — `scripts/release.sh` owns that. Your job is to produce a **READY** or
**NOT READY** verdict, listing BLOCKING findings first, then ADVISORY, then an
appendix inventory.

Optional argument `$ARGUMENTS`: a base ref to diff from. If empty, use the last
semver tag.

Work through every section. For each finding, tag it **BLOCKING** (must fix
before release) or **INFO/ADVISORY** (report only). Run each command with Bash
and reason about the output — do not assume.

## 1. Base detection

```bash
BASE="${ARGUMENTS:-$(git describe --tags --abbrev=0 2>/dev/null)}"
# First release has no prior tag — fall back to the repo's root commit so the
# range below still covers the whole history instead of an empty `..HEAD` range.
DIFFBASE="${BASE:-$(git rev-list --max-parents=0 HEAD | tail -1)}"
echo "base=${BASE:-<none, using root commit>}  head=$(git rev-parse --short HEAD)  branch=$(git branch --show-current)"
```

If `$BASE` is empty (no semver tag yet — e.g. the very first release) say so;
the range falls back to the root commit via `$DIFFBASE`.

## 2. Commit-range review

```bash
git log "$DIFFBASE"..HEAD --first-parent --oneline
git diff --stat "$DIFFBASE"..HEAD
```

Classify each entry: **user-facing** (new keybinding, prefix/default change,
config key, `dvlpr` subcommand, behavior change) vs **maintenance** (version
bump, fmt-only, internal refactor, test-only). Keep this list — sections 6 and
the appendix use it.

## 3. Version coherence

```bash
# Cargo.toml [package] version — exactly one match expected.
grep -cE '^version = "[0-9]+\.[0-9]+\.[0-9]+"$' Cargo.toml
CARGO_VER=$(grep -E '^version = "[0-9]+\.[0-9]+\.[0-9]+"$' Cargo.toml | head -1 | sed -E 's/^version = "(.*)"$/\1/')
# Cargo.lock dvlpr entry.
LOCK_VER=$(grep -A1 '^name = "dvlpr"$' Cargo.lock | grep '^version' | head -1 | sed -E 's/version = "(.*)"/\1/')
echo "Cargo.toml=$CARGO_VER  Cargo.lock=$LOCK_VER  last_tag=${BASE:-<none>}"
```

- More or fewer than 1 Cargo.toml version line → **BLOCKING**.
- `CARGO_VER` != `LOCK_VER` → **BLOCKING** (run `cargo check` to resync).
- `CARGO_VER` == `${BASE#v}` → **INFO**: bump still pending; release.sh will
  bump it as part of tagging. This is the normal pre-release state, not a
  failure.
- No prior tag (`$BASE` empty) → the version-vs-tag check is N/A; just confirm
  Cargo.toml and Cargo.lock agree.

## 4. Release-channel integrity

The 4 canonical release assets must agree across `scripts/install.sh`,
`.github/workflows/release.yml`, and `src/update/mod.rs`:

```bash
for a in dvlpr-x86_64-linux.tar.gz dvlpr-aarch64-linux.tar.gz \
         dvlpr-x86_64-macos.tar.gz dvlpr-aarch64-macos.tar.gz; do
  for f in scripts/install.sh .github/workflows/release.yml src/update/mod.rs; do
    grep -q -- "$a" "$f" || echo "BLOCKING: $a missing from $f"
  done
done
```

Any printed line → **BLOCKING** (a rename in one place silently breaks installs
or self-update; nothing else tests this).

Repo slug must agree across install.sh / Cargo.toml / build.rs:

```bash
grep -n 'DVLPR_REPO:-' scripts/install.sh
grep -nE '^repository' Cargo.toml
grep -n 'DVLPR_RELEASE_REPO' build.rs
grep -roh 'alkeincodes/dvlpr' scripts/install.sh Cargo.toml build.rs | sort -u
```

The last command should print exactly one slug. More than one distinct slug, or
a file not referencing it → **BLOCKING**.

## 5. Build gates

```bash
"${ZIG:-zig}" version    # must print 0.15.2 — build.rs hard-fails otherwise
just check               # fmt --check + clippy -D warnings + test
```

- `zig version` != `0.15.2` → **BLOCKING** (set `$ZIG` to a 0.15.2 binary).
- `just check` non-zero → **BLOCKING**, quote the failing output.

If the operator asked for a **quick audit**, you may skip `just check` — but the
verdict MUST state that build gates were not run.

## 6. README drift

For each user-facing change from section 2, confirm `README.md` documents it:

```bash
grep -ni '<keyword>' README.md   # repeat per new keybinding/config key/subcommand
```

- An undocumented user-facing change → **ADVISORY**, naming the specific gap.
- Stale hardcoded version strings → **ADVISORY**. Note: `README.md` near line
  124 (`/opt/dvlpr-<ver>/...`) is an *illustrative* symlink path, not an install
  pin — mention only if clearly misleading; do not block.

## 7. Vendor compliance

```bash
cat vendor/libghostty-vt/VERSION
grep '"version"' vendor/libghostty-vt.vendor.json
ls vendor/libghostty-vt/LICENSE
```

- `VERSION` != the `version` field in `vendor.json` → **ADVISORY** (pin drift).
- Missing `LICENSE` → **ADVISORY** (the lib ships statically linked; MIT
  attribution is required for redistribution).

## 8. Hygiene / leak guard

```bash
if [ -f .release-denylist ]; then
  grep -rEnf .release-denylist src README.md Cargo.toml scripts .github \
    && echo "BLOCKING: denylist hit(s) above" || echo "leak guard clean"
else
  echo "INFO: .release-denylist absent — leak guard skipped"
fi
git ls-files docs/        # expect empty
git check-ignore -q docs/ && echo "docs/ ignored OK" || echo "BLOCKING: docs/ not ignored"
```

- Any denylist hit → **BLOCKING**.
- `git ls-files docs/` non-empty → **BLOCKING** (internal docs committed).

## 9. Verdict

Emit, in this order:

1. **`READY`** or **`NOT READY`** (NOT READY if any BLOCKING finding).
2. **Blocking findings** — each with the file/command and the fix.
3. **Advisory findings**.
4. **Appendix** — resolved base/head/version; changes since base split into
   user-facing vs maintenance.
