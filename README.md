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

## License

dvlpr's own license is not yet decided. The vendored `libghostty-vt` library under
`vendor/libghostty-vt/` is third-party MIT-licensed source; see
`vendor/libghostty-vt/LICENSE`.
