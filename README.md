# dvlpr

A lightweight, agent-aware terminal multiplexer written in Rust. A background
daemon owns PTY-backed panes and renders them server-side; thin clients attach and
detach over a Unix socket while the panes keep running.

> Status: in development. Implemented so far: a single-pane walking skeleton
> (daemon + client + detach/reattach) and the `libghostty-vt` build plumbing.

## Installing

For Linux x86_64, Linux aarch64, macOS x86_64, or macOS aarch64:

```sh
curl -fsSL https://raw.githubusercontent.com/alkeincodes/dvlpr/main/scripts/install.sh | bash
```

The script downloads the prebuilt binary for your host from the latest
GitHub Release, verifies its SHA-256, and installs to `/usr/local/bin/dvlpr`
(via `sudo`) or `~/.local/bin/dvlpr` (no sudo). On macOS it strips the
Gatekeeper quarantine attribute. Confirms with `dvlpr --version`.

### Updating

Once installed:

```sh
dvlpr update
```

Fetches the latest release for your host's target, verifies SHA-256, and
atomically replaces the binary in place. If the install directory isn't
writable, it exits with code 2 and prints `rerun as: sudo dvlpr update`
— rerun with `sudo` and try again.

Running sessions keep the old binary loaded until you `dvlpr kill -t <name>`
and reattach.

### Pre-v0.2.0 → v0.2.0 transition

Source-built `0.1.0` installs do not have the `dvlpr update` subcommand
(it was introduced in v0.2.0 along with the binary release pipeline). On
those hosts, re-run the install script once to land v0.2.0; from there,
`dvlpr update` handles every subsequent release.

### Symlinks

`dvlpr update` operates on the resolved binary path
(`std::env::current_exe().canonicalize()`). If you've placed dvlpr behind
a symlink — e.g. `/usr/local/bin/dvlpr` → `/opt/dvlpr-0.2.0/bin/dvlpr` —
the update replaces the resolved file, not the symlink. The supported
install script writes a real file at the install path; custom symlink
layouts aren't officially tested.

## Building from source (contributors)

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

Production CI mirrors this split: ubuntu-latest + cargo-zigbuild for the
two Linux targets, macos-14 + native cargo for the two Darwin targets.
Cross-building to macOS from a non-Mac host is not supported by this slice.

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
