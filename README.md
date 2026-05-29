# dvlpr

A lightweight, agent-aware terminal multiplexer written in Rust. A background
daemon owns PTY-backed panes and renders them server-side; thin clients attach and
detach over a Unix socket while the panes keep running.

> Status: in development. Implemented so far: a single-pane walking skeleton
> (daemon + client + detach/reattach) and the `libghostty-vt` build plumbing.

## Prerequisites

Building dvlpr requires the following on the build machine:

- **Rust** (stable — pinned via `rust-toolchain.toml`).
- **Zig 0.15.2 — exactly this version.** The vendored `libghostty-vt` terminal
  engine is compiled from Zig at build time and pins `requireZig(0.15.2)`.
  Newer Zig (including Homebrew's current `zig`, 0.16.x) will **not** build it.
  - Download: <https://ziglang.org/download/0.15.2/> (pick your platform's archive).
  - Example (macOS, Apple Silicon):
    ```sh
    curl -fsSL -o /tmp/zig.tar.xz https://ziglang.org/download/0.15.2/zig-aarch64-macos-0.15.2.tar.xz
    tar -xf /tmp/zig.tar.xz -C "$HOME/.local/"
    ```
  - Make it discoverable, either by putting it on `PATH`, or by pointing the
    `ZIG` environment variable at the binary (the build script honors `ZIG`):
    ```sh
    export ZIG="$HOME/.local/zig-aarch64-macos-0.15.2/zig"
    ```
- **libclang** — required by `bindgen` to generate the FFI bindings.
  - macOS: install the Xcode Command Line Tools (`xcode-select --install`).
  - Linux: install your distribution's `libclang` / `clang` development package.

The vendored `libghostty-vt` source (under `vendor/`) is self-contained, so the
build does not need network access once the prerequisites are installed.

## Building and testing

```sh
export ZIG="$HOME/.local/zig-aarch64-macos-0.15.2/zig"   # or have zig 0.15.2 on PATH
cargo build
cargo test
```

The full quality gate (formatting, lints, tests) is wrapped by `just`:

```sh
just check   # cargo fmt --check + cargo clippy --all-targets -- -D warnings + cargo test
```

## Line editing inside agent CLIs

The default prefix is `Ctrl-B` (matching tmux's convention), which leaves
the standard readline line-editing keys free to reach the foreground CLI.
Inside `claude`, `codex`, `zsh`, or any other readline-style prompt
running in a dvlpr pane, these all work as plain pane bytes:

| Key | Action |
|-----|--------|
| `Ctrl-A` | Beginning of line |
| `Ctrl-E` | End of line |
| `Ctrl-U` | Kill to beginning of line |
| `Ctrl-W` | Kill word backward |
| `Ctrl-K` | Kill to end of line |
| `Option-←` / `Option-→` | Move by word (already emitted by Warp/iTerm2/Ghostty) |
| `Option-Backspace` | Kill word backward |

To change the prefix, add `prefix = "C-<letter>"` to a dvlpr config file.
dvlpr loads the first existing file in this order:

1. `$XDG_CONFIG_HOME/dvlpr/config.toml` (or `~/.config/dvlpr/config.toml`
   when `XDG_CONFIG_HOME` is unset)
2. `~/.dvlpr/config.toml` (dotfile fallback)

For example, to restore the pre-2026-05-28 `Ctrl-A` prefix, write
`prefix = "C-a"` to either path.

**Upgrading from a prior release:** a running daemon caches its config at
startup, so an existing session keeps the old default prefix until that
daemon exits — `Config::load()` is not re-run on attach. After upgrading,
either `dvlpr kill -t <name>` or close the last pane to shut the session
down via the existing `Closed` flow, then start fresh with `dvlpr <name>`
so the new daemon picks up the new default.

### Note for zsh users

`Option-←` / `Option-→` arrive at the foreground CLI as `^[[1;3D` /
`^[[1;3C` (the `modifyOtherKeys` encoding). Claude code's input handler
understands this natively, but zsh's default keymaps don't bind it —
the prompt will insert `;3D` / `;3C` literally instead of jumping by
word. Bind them in `~/.zshrc`:

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
configuring Warp alone — use the `Ctrl-`-prefixed equivalents above. A
future slice will add a copy mode (with `OSC 52` clipboard yank) to
address text selection from the back buffer.

To get Cmd-key support while staying on Warp, install
[Karabiner-Elements](https://karabiner-elements.pqrs.org/) and add an
app-scoped complex modification that rewrites `Cmd+Backspace` →
`Ctrl+U`, `Cmd+←` → `Ctrl+A`, `Cmd+→` → `Ctrl+E`, and (optionally)
`Option+Backspace` → `Ctrl+W` while Warp (bundle ID
`dev.warp.Warp-Stable`) is the frontmost app. Karabiner rewrites the
keystroke before Warp sees it, so the synthesized control byte reaches
dvlpr through Warp's normal forwarding path. Outside Warp the keys
keep their native macOS behavior.

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

## Mouse

- **Left-click** a pane to focus it. Drag a divider between panes to resize.
- **Right-click** a pane to open the context menu: **Split Vertically**,
  **Split Horizontally**, **Zoom** (toggle), **Exit** (close this pane).
  Navigate with `↑` / `↓` (or hover); confirm with `Enter` or a left-click;
  dismiss with `Esc` or by clicking outside the menu.
- Click the `[+]` button just after the last tab to open a new window (same as `prefix c`).
- `C-b ?` — open the help overlay (keybindings + dvlpr CLI commands)

## License

dvlpr's own license is not yet decided. The vendored `libghostty-vt` library under
`vendor/libghostty-vt/` is third-party MIT-licensed source; see
`vendor/libghostty-vt/LICENSE`.
