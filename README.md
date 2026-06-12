# dvlpr

> A lightweight, agent-aware terminal multiplexer written in Rust — tmux-style
> sessions/windows/panes with a built-in sidebar that watches what your AI
> agents (`claude`, `codex`) are doing in each pane.

[![Release](https://img.shields.io/github/v/release/alkeincodes/dvlpr)](https://github.com/alkeincodes/dvlpr/releases)
[![License: MIT](https://img.shields.io/badge/License-MIT-yellow.svg)](#license)

dvlpr runs a background daemon that owns PTY-backed panes and renders them
server-side. Thin clients attach over a per-session Unix socket and detach
without killing the panes. The vendored [`libghostty-vt`](https://github.com/ghostty-org/ghostty)
terminal engine (the same VT parser that powers [Ghostty](https://ghostty.org))
handles the cell-grid state inside each pane, so what you see in dvlpr renders
identically to a top-tier native terminal.

The headline feature is the **agent-awareness sidebar**: a per-pane status
column that classifies each pane's foreground process (`claude`, `codex`,
`zsh`, …) and tracks AI agent state (Idle / Working / Blocked / Done) by
reading terminal-tail markers. When a long-running agent finishes work in a
window you're not looking at, the sidebar lights up with a blue check.

---

## Features

- **Sessions, windows, panes** — tmux-style nesting with a configurable prefix
  key (default `Ctrl-B`)
- **Automatic window naming** — windows are named after their foreground
  process (`claude`, `codex`, `zsh`, `node`, …); pin a custom name via the
  rename dialog
- **Agent-awareness sidebar** — per-pane status column with state colors
  (working = yellow, blocked = red, done = blue ✓, idle = dim)
- **Detach / reattach** — panes keep running while no client is connected;
  reattach from anywhere
- **`dvlpr ssh`** — thin wrapper that runs the remote `dvlpr` over an SSH
  TTY (the remote has its own daemon and socket; no protocol tunneling)
- **Mouse support** — click to focus, drag a divider to resize, right-click
  a pane for split/zoom/exit, right-click a tab for rename/close
- **Pane zoom** — `prefix 0` fullscreens a pane within its window; auto-unzooms
  on split/close/window-switch
- **Truecolor + UTF-8** — full SGR colors and text attributes flow from PTY
  through the compositor to the client
- **Built-in updater** — `dvlpr update` fetches the latest release for your
  host's target, verifies SHA-256, atomically replaces the running binary
- **Readline-friendly defaults** — `Ctrl-A`/`E`/`U`/`W`/`K` and `Option+←`/`→`
  pass through to the foreground CLI; only `prefix` is intercepted
- **Always-on status bar** — bottom row shows session name + window list with
  active marker (`*`) and zoom marker (`Z`)

---

## Quick start

```sh
# Install
curl -fsSL https://raw.githubusercontent.com/alkeincodes/dvlpr/main/scripts/install.sh | bash

# Start your first session (creates 'default' if no name given)
dvlpr

# Inside dvlpr — split horizontally with prefix + ↓, then start an agent
# Ctrl-B ↓
# then: claude
```

Press `Ctrl-B ?` any time to open the in-app help overlay with the full
keybinding reference.

---

## Installing

For Linux x86_64, Linux aarch64, macOS x86_64, or macOS aarch64:

```sh
curl -fsSL https://raw.githubusercontent.com/alkeincodes/dvlpr/main/scripts/install.sh | bash
```

The script:

1. Detects your OS and architecture from `uname -sm`.
2. Downloads the matching `dvlpr-<arch>-<os>.tar.gz` from the latest
   [GitHub release](https://github.com/alkeincodes/dvlpr/releases/latest).
3. Verifies the SHA-256 checksum.
4. Installs to `/usr/local/bin/dvlpr` (with `sudo`) or `~/.local/bin/dvlpr`
   (no-sudo fallback; appends `~/.local/bin` to `PATH` in `~/.bashrc` if
   needed so non-interactive SSH picks it up).
5. On macOS, strips the Gatekeeper `com.apple.quarantine` attribute so the
   binary runs without a "developer not verified" prompt.
6. Confirms with `dvlpr --version`.

Override the source repo (for forks or self-hosted mirrors):

```sh
DVLPR_REPO=your-org/dvlpr curl -fsSL https://raw.githubusercontent.com/your-org/dvlpr/main/scripts/install.sh | bash
```

### Updating

Once installed:

```sh
dvlpr update
```

Fetches the latest release for your host's target, verifies SHA-256, and
atomically replaces the binary in place. Exit codes:

| Exit | Meaning |
|------|---------|
| `0`  | Updated successfully, or already on the latest release |
| `1`  | Failure — network error, SHA-256 mismatch, no asset for this host |
| `2`  | Install directory not writable — `rerun as: sudo dvlpr update` |

After a successful update, `dvlpr update` offers to **restart your running
sessions** so they pick up the new binary, restoring each session's full layout
(windows, panes, working directories, and claude/codex conversations):

| Flag | Behavior |
|------|----------|
| *(none)* | Prompt `Restart N running session(s) (…) to apply the update? [Y/n]` (default Yes; auto-No when non-interactive) |
| `--restart` | Restart without prompting (also works non-interactively) |
| `--no-restart` | Swap the binary only — leave running sessions on the old binary |

Sessions attached in another terminal, and the session you're running `dvlpr
update` from, are skipped (listed with a reason) so an active terminal is never
yanked out from under you. Stop those manually (`pkill -f 'dvlpr server'`) and
start them fresh to update them.

**Caveats.** Restart-and-restore takes effect from the *next* update onward (the
update that introduces it is driven by the prior binary). A session left running
from an older dvlpr is skipped as incompatible. Only the layout + cwd + agent
conversations are restored — other long-running processes (dev servers, editors)
come back as a shell in their directory.

### Symlinks

`dvlpr update` operates on the resolved binary path via
`std::env::current_exe().canonicalize()`. If you've placed dvlpr behind a
symlink — e.g. `/usr/local/bin/dvlpr` → `/opt/dvlpr-0.2.0/bin/dvlpr` — the
update replaces the resolved file, not the symlink. The supported install
script writes a real file at the install path; custom symlink layouts
aren't officially tested.

---

## Usage

### Session management

| Command | What it does |
|---------|--------------|
| `dvlpr` | Create or attach to the `default` session |
| `dvlpr <name>` | Create or attach to session `<name>` |
| `dvlpr new -s <name>` | Explicit "create or attach" form |
| `dvlpr attach -t <name>` (or `dvlpr a -t <name>`) | Attach to an existing session; error if missing |
| `dvlpr ls` | List live sessions with window counts |
| `dvlpr stop -t <name>` | Stop a session's daemon; error if missing |
| `dvlpr ssh <dest> [name]` | SSH to `<dest>` and run dvlpr there (see below) |
| `dvlpr update` | Fetch the latest release and replace this binary |
| `dvlpr --version` / `-V` | Print the version and build target triple |
| `dvlpr -h` / `--help` / `dvlpr help` | Print the command list |

### Control CLI

The control CLI drives a running session's daemon over its socket — scriptably,
from inside a pane, a plain shell (even while detached), or any process that can
run `dvlpr`, as long as the target session's daemon is alive.

| Command | What it does |
|---------|--------------|
| `dvlpr window new [--name NAME]` | Create a new window tab |
| `dvlpr window rename <NAME>` | Rename the active window |
| `dvlpr window close` | Close the active window |
| `dvlpr window next` | Switch to the next window |
| `dvlpr window prev` | Switch to the previous window |
| `dvlpr window select <N>` | Jump to window N (1-based) |
| `dvlpr pane split right\|down` | Split the focused pane right or down |
| `dvlpr pane close` | Close the focused pane |
| `dvlpr pane zoom` | Toggle zoom on the focused pane |
| `dvlpr sidebar toggle` | Show/hide the agent sidebar |

> **Note:** Closing the last pane or the last window is refused (it would end the session) — use `dvlpr stop -t <name>` to stop a session.

**Examples**

```sh
# From inside a pane — no flag needed, acts on the session you're in ($DVLPR):
dvlpr pane split right
dvlpr window new --name build
dvlpr sidebar toggle

# From any shell — target a session by name:
dvlpr @work window select 2          # @name prefix
dvlpr pane zoom --session api        # --session flag
dvlpr @work window rename deploy

# Scripting: spin up a layout, then drive it
dvlpr server-name pane split down
dvlpr server-name pane split right
dvlpr server-name window new --name logs
```

**Agent scripting** — because `$DVLPR` is set in every pane, an agent running
inside a dvlpr session (Claude, Codex, …) can reshape its own workspace with
zero configuration: `dvlpr window new --name tests` or `dvlpr pane split down`
just work, targeting the agent's session automatically. The non-zero exit codes
(below) make these safe to branch on in a script.

**Session targeting** — a leading `@name` or a trailing `--session NAME` selects
the target session. Without either, dvlpr uses `$DVLPR` (the session you're
currently inside), falling back to `default`.

Precedence: `@name` / `--session` &gt; `$DVLPR` &gt; `default`

**Exit codes**

| Exit | Meaning |
|------|---------|
| `0` | Command applied successfully |
| `1` | Runtime failure — no running session for the target, or the command failed (e.g. `window select` out of range) |
| `2` | Usage error — unrecognised command or bad arguments |

**Reserved words** — `window`, `pane`, and `sidebar` are reserved first words.
`dvlpr window` is now a control command, not a request to create/attach to a
session named `window`. To target a session that happens to share one of those
names, use `dvlpr @window` or `dvlpr --session window`.

**v1 limitation** — `--session NAME` is consumed anywhere in the arg tail, so
you cannot name a window literally `--session`.

### `dvlpr ssh`

```sh
dvlpr ssh user@remote          # attach to a 'default' session on remote
dvlpr ssh user@remote work     # attach to session 'work' on remote
```

`dvlpr ssh` runs `ssh -t <dest> dvlpr <session>` under the hood. The remote
needs its own dvlpr install — install it the same way as on your local
machine (the curl-pipe install script works on the remote too).

**Why not a protocol tunnel?** The remote's daemon listens on the remote's
own socket. Tunneling the dvlpr wire protocol over SSH would require an
extra transport layer and complicate teardown semantics; the exec-wrapper
approach reuses SSH's TTY handling and stays simple.

### Remote bridge (`dvlpr bridge`)

`dvlpr bridge` exposes the host's agent sessions as newline-delimited JSON on
stdin/stdout — list agents and their states, stream transcripts, type into
panes, and manage windows, all scriptable. It is designed to be driven over
SSH (`ssh host dvlpr bridge`) by external tools, but works identically when
run locally:

    printf '' | dvlpr bridge        # emits a hello line and exits on EOF

Protocol details: one JSON object per line; commands carry an `id` echoed in
the matching `reply`. Targeted commands carry the `epoch` from the latest
roster snapshot; a daemon restarted since then answers `stale_target`.

### Nested-session guard

dvlpr sets `DVLPR=<session_name>` in each pane's environment. A nested
`dvlpr` invocation refuses to run with:

```
dvlpr: refusing to run inside dvlpr (DVLPR=<name>); unset DVLPR to force
```

`dvlpr ssh` strips `DVLPR` from the SSH child so the remote isn't tripped
by a stale env var.

---

## Keybindings

The default **prefix** is `Ctrl-B` (matching tmux's convention). All
commands below are pressed after the prefix unless noted otherwise.

### Panes

| Keys | Action |
|------|--------|
| `prefix ↓` | Split focused pane horizontally (stacked) |
| `prefix →` | Split focused pane vertically (side-by-side) |
| `prefix x` | Close the focused pane |
| `prefix 0` | Toggle zoom (fullscreen the focused pane within its window) |
| Click | Focus the pane under the cursor |
| Right-click | Open pane context menu (Split V / Split H / Zoom / Exit) |
| Drag a divider | Resize adjacent panes |

### Windows (tabs)

| Keys | Action |
|------|--------|
| `prefix c` | Open the **New Window** dialog (type a name, press Enter) |
| `prefix n` | Next window |
| `prefix p` | Previous window |
| `prefix 1`…`9` | Jump to window 1–9 directly |
| Click a tab | Switch to that window |
| Right-click a tab | Rename / Close |
| Click `[+]` after the last tab | Same as `prefix c` |

### Session

| Keys | Action |
|------|--------|
| `prefix d` | Detach (panes keep running; reattach with `dvlpr <name>`) |
| `prefix s` | Toggle the agent-awareness sidebar |
| `prefix ?` | Open the help overlay (keybindings + commands) |
| `prefix prefix` | Send a literal `Ctrl-B` to the focused pane |

### Inside the in-app help / dialog / menu overlays

- `Esc` — dismiss
- `↑` / `↓` — navigate
- `Enter` — confirm / submit
- `Tab` — switch tabs in the help overlay (Commands / Keybindings)

---

## Configuration

dvlpr loads the first existing file in this order at daemon start:

1. `$XDG_CONFIG_HOME/dvlpr/config.toml` (or `~/.config/dvlpr/config.toml`
   when `XDG_CONFIG_HOME` is unset)
2. `~/.dvlpr/config.toml` (dotfile fallback)

The file is optional — missing or malformed entries fall back to their
compiled-in defaults, and any failures are logged without crashing the
daemon.

### Sample `config.toml`

```toml
# Prefix key. Must be C-<lowercase letter>, e.g. "C-a", "C-b", "C-q".
prefix = "C-b"

# Scrollback history lines per pane. Default: 2000. Set to 0 to disable
# copy mode scrolling entirely.
scrollback = 2000

# Color theme. One of: "one-dark" (default), "macchiato",
# "frappe", "latte", "mocha".
[theme]
flavor = "one-dark"

# Agent-awareness sidebar width in columns (default 33; clamped 18–36).
[sidebar]
width = 33

# Optional sound played when an agent transitions into Blocked state
# (waiting for input). Set enabled = false to silence; or point
# `blocked` at a custom .wav/.aiff file.
[sound]
enabled = true
# blocked = "/path/to/custom.wav"

# Pane-command keymap (all values default — shown for reference).
[keys]
split-horizontal = "Down"
split-vertical = "Right"
close-pane = "x"
new-window = "c"
next-window = "n"
prev-window = "p"
detach = "d"
toggle-sidebar = "s"
copy-mode = "["            # prefix + this key enters copy mode
```

### Restoring the pre-v0.2.0 `Ctrl-A` prefix

If you have tmux muscle memory or want `Ctrl-B` for something else:

```toml
prefix = "C-a"
```

**A running daemon caches its config at startup.** After editing the file,
shut existing sessions down (`dvlpr stop -t <name>` or close the last pane)
so the next `dvlpr <name>` spawns a daemon that re-reads the file.

---

## Line editing inside agent CLIs

The default prefix `Ctrl-B` leaves the standard readline line-editing keys
free to reach the foreground CLI. Inside `claude`, `codex`, `zsh`, or any
readline-style prompt running in a dvlpr pane, these work as plain pane
bytes:

| Key | Action |
|-----|--------|
| `Ctrl-A` | Beginning of line |
| `Ctrl-E` | End of line |
| `Ctrl-U` | Kill to beginning of line |
| `Ctrl-W` | Kill word backward |
| `Ctrl-K` | Kill to end of line |
| `Option-←` / `Option-→` | Move by word (already emitted by Warp/iTerm2/Ghostty) |
| `Option-Backspace` | Kill word backward |

### Note for zsh users

`Option-←` / `Option-→` arrive at the foreground CLI as `^[[1;3D` /
`^[[1;3C` (the `modifyOtherKeys` encoding). Claude code's input handler
understands this natively, but zsh's default keymaps don't bind it — the
prompt will insert `;3D` / `;3C` literally instead of jumping by word.
Bind them in `~/.zshrc`:

```zsh
bindkey "^[[1;3D" backward-word           # Option+Left
bindkey "^[[1;3C" forward-word            # Option+Right
bindkey "^[[1;3A" up-line-or-history      # Option+Up
bindkey "^[[1;3B" down-line-or-history    # Option+Down
```

### Warp note

Warp's keybinding system has no action that sends raw bytes to the
foreground process, so the macOS `Cmd+Backspace` / `Cmd+←` / `Cmd+→`
shortcuts can't be made to reach `claude`/`codex` from inside dvlpr by
configuring Warp alone — use the `Ctrl-`-prefixed equivalents above.

To get Cmd-key support while staying on Warp, install
[Karabiner-Elements](https://karabiner-elements.pqrs.org/) and add an
app-scoped complex modification that rewrites `Cmd+Backspace` → `Ctrl+U`,
`Cmd+←` → `Ctrl+A`, `Cmd+→` → `Ctrl+E`, and (optionally)
`Option+Backspace` → `Ctrl+W` while Warp (bundle ID
`dev.warp.Warp-Stable`) is the frontmost app. Karabiner rewrites the
keystroke before Warp sees it, so the synthesized control byte reaches
dvlpr through Warp's normal forwarding path.

For a pure host-terminal fix without Karabiner, switch to
[Ghostty](https://ghostty.org/) and add three lines to
`~/.config/ghostty/config`:

```
keybind = cmd+backspace=text:\x15
keybind = cmd+left=text:\x01
keybind = cmd+right=text:\x05
```

Ghostty also implements `modifyOtherKeys` natively (Warp does not), so
the `bindkey` lines above are not strictly needed in a Ghostty session.

---

## Mouse

- **Left-click** a pane to focus it. Drag a divider between panes to resize.
- **Right-click** a pane to open the context menu: **Split Vertically**,
  **Split Horizontally**, **Zoom** (toggle), **Exit** (close this pane).
  Navigate with `↑` / `↓` (or hover); confirm with `Enter` or a left-click;
  dismiss with `Esc` or by clicking outside the menu.
- **Right-click a tab** to open the tab context menu: **Rename** (opens a
  dialog), **Close**.
- Click the `[+]` button just after the last tab to open the New Window
  dialog (same as `prefix c`).

---

## Copy mode

Copy mode lets you scroll into a pane's scrollback history, select text, and
yank it to the clipboard.

### Entering and exiting

| Key | Action |
|-----|--------|
| `prefix [` | Enter copy mode (freezes the focused pane, cursor at bottom) |
| `q` or `Esc` | Exit copy mode (returns to the live pane bottom) |

### Navigation

| Key | Action |
|-----|--------|
| `h` / `←` | Move cursor left |
| `l` / `→` | Move cursor right |
| `k` / `↑` | Move cursor up one row |
| `j` / `↓` | Move cursor down one row |
| `0` / `Home` | Move to beginning of line |
| `$` / `End` | Move to end of line |
| `w` | Move forward one word |
| `b` | Move backward one word |
| `g` | Jump to the top of scrollback |
| `G` | Jump to the live bottom |
| `C-u` | Scroll up half a page |
| `C-d` | Scroll down half a page |
| `C-b` / `PageUp` | Scroll up one full page |
| `C-f` / `PageDown` | Scroll down one full page |

### Mouse-wheel scrolling

Rolling the mouse wheel over a pane scrolls that pane through its scrollback in
place — no `prefix [` and no foreground promotion, so you can skim a background
pane's history while another pane stays focused. The view holds its position
while new output keeps streaming in; scroll back down to the bottom to return
to live. In copy mode the wheel moves the copy offset instead, and entering
copy mode adopts whatever offset you had wheeled to. When the foreground app
has mouse tracking enabled (e.g. a full-screen TUI), the wheel is forwarded to
the app as SGR mouse events rather than scrolling scrollback.

### Selecting and yanking

| Key | Action |
|-----|--------|
| `v` | Toggle character-wise selection (anchors at the current cursor) |
| `y` or `Enter` | Yank the selection to the clipboard via OSC 52; exits copy mode |
| Mouse drag | Draw a selection; releasing the button copies it and exits copy mode |

The bottom row of the pane shows `[copy] <offset>/<scrollback>` while copy
mode is active.

Yanking leaves the pane where you copied from — copying a line out of the
scrollback does **not** jump the view back to the live bottom, so you can keep
reading or copy more. Scroll back down (or `G`) to return to live. Cancelling
instead (`q` / `Esc`) does return to the live bottom.

### Drag to copy

A left-click-drag inside a pane auto-enters copy mode and begins the
selection — no `prefix [` needed (tmux-style). **Releasing the button copies
the highlighted text (OSC 52) and exits copy mode**, so a single drag-and-release
gesture selects and copies with no `y` keypress. (Cmd+C can't be used here: it's
your host terminal's copy of its *own* native selection, but dvlpr owns the mouse
drag, so the host has no selection to copy — the highlight lives inside dvlpr.)
Drag-to-enter is suppressed when the pane's foreground app is itself using the
mouse (e.g. vim, htop), so those apps keep their mouse. A plain click (press +
release without dragging) does not enter copy mode or copy anything.

A mouse-driven copy session lives only while the button is held: releasing it
always ends the session. If you dragged out a selection it is copied; if the
drag collapsed back to nothing, copy mode simply exits. (A keyboard `prefix [`
session is unaffected — a stray mouse release won't tear it down.)

The copy lands in your system clipboard only if your terminal honors OSC 52
clipboard writes (see *OSC 52 clipboard support* below) — that's the same
requirement as the `y` yank.

Disable drag-to-enter entirely with:

```toml
mouse-copy = false
```

### What you copy is what you see

Yank copies exactly the highlighted cells. If a selection extends past the
visible viewport, only the visible (highlighted) portion is copied;
soft-wrapped lines are rejoined within that visible range.

### Configuration

```toml
# Number of scrollback rows retained per pane. Default: 2000.
# Set to 0 to disable copy mode scrolling entirely.
scrollback = 2000

# Set to false to disable drag-to-enter copy mode. Default: true.
mouse-copy = true

[keys]
# Key to enter copy mode after the prefix. Default: "[".
copy-mode = "["
```

### OSC 52 clipboard support

dvlpr delivers the yanked text to your **host terminal** as an
[OSC 52](https://invisible-island.net/xterm/ctlseqs/ctlseqs.html#h3-Operating-System-Commands)
clipboard-write sequence. Whether the paste actually reaches your system
clipboard depends on your host terminal's settings:

- **iTerm2** — enable *Edit > Preferences > General > Applications in terminal
  may access clipboard*.
- **Ghostty** — OSC 52 writes are permitted by default.
- **Warp** — OSC 52 clipboard writes are not reliably supported in v1; use a
  different terminal or copy text manually.
- **tmux / dvlpr nesting** — the outer multiplexer must pass through OSC 52;
  dvlpr does not add a local-clipboard fallback in v1.

### Multi-client and known limitations

- **Ownership** — copy mode is owned by the client that entered it. If several
  clients are attached to the same session, another client typing while you are
  in copy mode does not drive or hijack your session, and only you receive the
  yanked clipboard text.
- **Scrollback eviction during copy mode (v1)** — copy mode freezes the *view*,
  but the pane keeps producing output. If that output overflows the scrollback
  limit while you hold a selection, the oldest rows are evicted and a selection
  anchored in the evicted region is clamped (best-effort) rather than tracked
  exactly — a yank in that rare case may capture slightly shifted text. Increase
  `scrollback` or yank before heavy output to avoid it.

---

## Architecture

dvlpr is a daemon + thin client over a per-session Unix socket.

```
┌──────────────────────────────────────────────┐
│ client (your terminal)                       │
│ • raw mode + SGR mouse + alt-screen          │
│ • sends keystrokes / mouse events            │
│ • renders the diffed frames it receives      │
└──────────────────────────────────────────────┘
                      │  protocol v7 over
                      │  /tmp/dvlpr-<user>/<name>.sock
                      ▼
┌──────────────────────────────────────────────┐
│ daemon (background process)                  │
│ • Session: windows + panes + layout          │
│ • Compositor: composes panes → one frame     │
│ • Per-client diff + backpressure             │
│ • Pane PTYs (portable-pty)                   │
│ • libghostty-vt VT parser per pane           │
│ • Agent state classifier (procinfo+tail)     │
└──────────────────────────────────────────────┘
```

### Key crates and modules

- `src/session/` — windows, panes, layout, input dispatch, mouse handling
- `src/compositor/` — composes per-pane grids into one viewport frame
- `src/client/` — thin client (raw mode, alt-screen, mouse, focus reporting)
- `src/server/` — Unix socket accept loop, per-client writer, central event loop
- `src/pane/` — PTY spawning, libghostty-vt FFI bindings, screen state
- `src/input/` — prefix-key state machine, CSI parser, mouse tracking
- `src/config/` — `~/.dvlpr/config.toml` loader with strict defaults
- `src/update/` — `dvlpr update` orchestration (fetch + verify + atomic rename)
- `src/detect/` — agent name detection (procinfo) + state classification
  (Claude / Codex tail markers)
- `src/procinfo/` — pid → friendly process name (macOS `KERN_PROCARGS2` /
  Linux `/proc/<pid>/cmdline`)
- `src/help/` — in-app help overlay (Commands + Keybindings tabs)
- `src/menu/`, `src/dialog/` — right-click context menus and rename dialog
- `src/layout/` — split-tree layout with sidebar and status bar
- `src/theme/` — theme parsing (One Dark, Catppuccin variants)
- `vendor/libghostty-vt/` — vendored Zig VT engine (MIT, third-party)

### Wire protocol

The protocol is bincode over a Unix socket. The first message is
`ClientHello { protocol_version, intent }`, where `intent` is one of
`Attach { cols, rows }`, `Status` (used by `dvlpr ls`), `Kill` (used by
`dvlpr stop`), `Command` (one-shot control commands), or `Subscribe`
(the long-lived agent-roster push channel used by `dvlpr bridge`).
`Status` and `Kill` are answered without spawning a session (no geometry /
foreground side effects). `PROTOCOL_VERSION` is currently `7`.

### libghostty-vt FFI

The vendored Zig source compiles to a static library at `cargo build`
time. `build.rs` enforces Zig 0.15.2 exactly and links the resulting
`libghostty_vt.a` into the Rust binary via `bindgen`. macOS builds
additionally link against CoreFoundation / CoreGraphics / CoreText /
CoreVideo / QuartzCore / IOSurface / Carbon. See **Building from source**
below for the toolchain notes.

---

## Building from source (contributors)

dvlpr requires the following on the build machine:

- **Rust** (stable — pinned via `rust-toolchain.toml`)
- **Zig 0.15.2 — exactly this version.** The vendored `libghostty-vt`
  terminal engine pins `requireZig(0.15.2)`. Newer Zig (including
  Homebrew's current `zig`, 0.16.x) will **not** build it.
  - Download: <https://ziglang.org/download/0.15.2/>
  - Example (macOS, Apple Silicon):
    ```sh
    curl -fsSL -o /tmp/zig.tar.xz https://ziglang.org/download/0.15.2/zig-aarch64-macos-0.15.2.tar.xz
    tar -xf /tmp/zig.tar.xz -C "$HOME/.local/"
    ```
- **libclang** — required by `bindgen` to generate the FFI bindings.
  - macOS: `xcode-select --install`
  - Linux: install your distribution's `libclang` / `clang-dev` package

The vendored `libghostty-vt` source (under `vendor/`) is self-contained,
so the build does not need network access once the prerequisites are
installed.

### Build + test

```sh
# build.rs reads ZIG; cargo-zigbuild reads CARGO_ZIGBUILD_ZIG_PATH.
# Setting only one risks a silent fallback to a system 'zig' (often 0.16.x
# via Homebrew), which our build.rs preflight rejects.
export ZIG="$HOME/.local/zig-aarch64-macos-0.15.2/zig"
export CARGO_ZIGBUILD_ZIG_PATH="$ZIG"

cargo build
cargo test
```

The full quality gate (formatting, lints, tests) is wrapped by `just`:

```sh
just check   # cargo fmt --check + cargo clippy --all-targets -- -D warnings + cargo test
```

### Local cross-builds (for testing release artifacts)

Install `cargo-zigbuild` once:

```sh
cargo install cargo-zigbuild
```

Then build any of the supported Linux targets via cargo-zigbuild:

```sh
cargo zigbuild --release --target x86_64-unknown-linux-gnu
cargo zigbuild --release --target aarch64-unknown-linux-gnu
```

For macOS targets, use native cargo on an Apple host (cargo-zigbuild
cannot link libghostty-vt's seven Apple frameworks — CoreFoundation,
CoreGraphics, CoreText, CoreVideo, QuartzCore, IOSurface, Carbon —
because Zig's bundled headers are libc-only):

```sh
# On an Apple Silicon Mac (aarch64-apple-darwin is native):
cargo build --release --target aarch64-apple-darwin

# To produce an x86_64 darwin binary from Apple Silicon, add the target:
rustup target add x86_64-apple-darwin
cargo build --release --target x86_64-apple-darwin
```

Production CI mirrors this split: `ubuntu-latest` + `cargo-zigbuild` for
the two Linux targets, `macos-14` + native cargo for the two Darwin
targets. Cross-building to macOS from a non-Mac host is not supported.

### Cutting a release

`scripts/release.sh` is the operator helper:

```sh
./scripts/release.sh 0.2.1
# tags v0.2.1 locally; prints the push command
git push origin main v0.2.1
```

The push triggers `.github/workflows/release.yml`, which:

1. Verifies the tag matches `Cargo.toml`'s `[package].version` (refuses
   to build otherwise — prevents "wrong tag publishes wrong-version
   binary").
2. Builds all 4 prebuilt tarballs in parallel.
3. Generates a `.sha256` for each.
4. Creates a GitHub Release with auto-generated notes from commit
   messages.

The first release (`v0.2.0`) used a manual `git tag` because the slice
itself bumped `Cargo.toml` from `0.1.0` to `0.2.0`; from `v0.2.1` onward,
`scripts/release.sh` is the canonical path.

---

## Project status

**v0.2.0** is the first release with prebuilt binary distribution and the
self-update path.

### Shipped

Sessions / windows / panes (tmux-style, with named multi-session and
detach-reattach), agent-awareness sidebar with state classification
(Claude + Codex), automatic window naming with manual rename via dialog,
pane zoom, truecolor, mouse support including right-click context menus,
focus reporting, `dvlpr ssh` exec-wrapper, nested-session guard, binary
release pipeline (GH Actions matrix), built-in `dvlpr update`.

### Deferred (open issues, contributions welcome)

- **Per-client re-render / chrome reflow** — when multiple clients are
  attached with different sizes, dvlpr today renders one composed grid and
  letterboxes for smaller observers. A heavier per-client rendering would
  let each client get a layout fitted to its own size.
- **Native SSH tunnel transport** — `dvlpr attach user@host:name` over a
  tunneled wire protocol (the current `dvlpr ssh` is the lightweight
  exec wrapper).
- **Copy mode + OSC 52 clipboard yank** — tmux-style `prefix [` navigation
  of the back buffer with selection and clipboard integration.
- **Session rename** — sessions can be created/destroyed but not renamed
  in place.
- **macOS code signing / Developer ID notarization** — current macOS
  binaries are unsigned; install.sh clears the Gatekeeper quarantine bit
  as a workaround.

---

## Contributing

Contributions are welcome — issues, PRs, and discussions all land on
GitHub.

- **File issues** at <https://github.com/alkeincodes/dvlpr/issues>.
  Include `dvlpr --version` output and a reproducer if possible.
- **Open PRs** against `main`. The CI pipeline runs `cargo fmt --check`,
  `cargo clippy --all-targets -- -D warnings`, and `cargo test` against
  each push. `just check` runs the same gate locally.
- **Pinned Zig version** — every PR that touches `build.rs` or the
  vendored Zig must keep the 0.15.2 pin. See **Building from source** for
  the toolchain notes.
- **Test discipline** — new behavior gets a test. Integration tests live
  under `tests/` (one per feature area); unit tests live in
  `#[cfg(test)] mod tests` inside the relevant module.

### Architecture for new contributors

The single biggest mental model to load is the **daemon-client split**:

- The **daemon** owns all state: windows, panes, PTYs, the composed
  grid. It runs on the first `dvlpr <name>` call and exits when the last
  pane in the last window closes.
- A **client** is stateless. It opens the per-session socket, sends a
  `ClientHello` with its viewport size and intent, and from there is a
  pipe: keystrokes → daemon; diff frames → terminal.

Everything else (the sidebar, the right-click menus, focus reporting,
even `dvlpr ssh`'s remote dance) is layered on top of that contract.

---

## License

dvlpr is licensed under the [MIT License](LICENSE).

The vendored `libghostty-vt` terminal engine under `vendor/libghostty-vt/`
is third-party MIT-licensed source authored by the Ghostty project; see
`vendor/libghostty-vt/LICENSE` for the upstream notice.
