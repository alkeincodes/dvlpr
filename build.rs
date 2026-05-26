use std::env;
use std::path::PathBuf;
use std::process::Command;

/// Map a Rust target triple to the corresponding zig target.
fn zig_target(rust_target: &str) -> &'static str {
    match rust_target {
        "x86_64-unknown-linux-gnu" => "x86_64-linux-gnu",
        "aarch64-unknown-linux-gnu" => "aarch64-linux-gnu",
        "x86_64-unknown-linux-musl" => "x86_64-linux-musl",
        "aarch64-unknown-linux-musl" => "aarch64-linux-musl",
        "x86_64-apple-darwin" => "x86_64-macos",
        "aarch64-apple-darwin" => "aarch64-macos",
        other => panic!("unsupported target for libghostty-vt build: {other}"),
    }
}

fn main() {
    let manifest_dir = PathBuf::from(env::var("CARGO_MANIFEST_DIR").unwrap());
    let vendor = manifest_dir.join("vendor/libghostty-vt");
    let target = env::var("TARGET").unwrap();
    let zig_target = zig_target(&target);
    let zig = env::var("ZIG").unwrap_or_else(|_| "zig".to_string());
    let optimize = env::var("LIBGHOSTTY_VT_OPTIMIZE").unwrap_or_else(|_| "ReleaseFast".into());
    let simd = env::var("LIBGHOSTTY_VT_SIMD").unwrap_or_else(|_| "true".into());
    let version = std::fs::read_to_string(vendor.join("VERSION"))
        .expect("read vendor/libghostty-vt/VERSION")
        .trim()
        .to_string();

    // Preflight: require zig 0.15.2 with an actionable error (0.16.x cannot build this).
    let ver_out = Command::new(&zig)
        .arg("version")
        .output()
        .unwrap_or_else(|e| panic!("failed to run `{zig} version` — set ZIG to a zig 0.15.2 binary: {e}"));
    let ver = String::from_utf8_lossy(&ver_out.stdout).trim().to_string();
    if ver != "0.15.2" {
        panic!(
            "libghostty-vt requires zig 0.15.2, found `{ver}`. \
             brew's zig (0.16.x) will NOT build it. Download 0.15.2 from \
             https://ziglang.org/download/0.15.2/ and set ZIG=/path/to/zig."
        );
    }

    // Compile the static library (also emits C headers under zig-out/include).
    let status = Command::new(&zig)
        .current_dir(&vendor)
        .args([
            "build",
            "-Demit-lib-vt",
            &format!("-Doptimize={optimize}"),
            &format!("-Dsimd={simd}"),
            &format!("-Dtarget={zig_target}"),
            &format!("-Dversion-string={version}"),
            "-Demit-xcframework=false",
        ])
        .status()
        .expect("failed to spawn zig build");
    assert!(status.success(), "zig build of libghostty-vt failed");

    let lib_dir = vendor.join("zig-out/lib");
    let static_lib = lib_dir.join("libghostty-vt.a");
    assert!(static_lib.exists(), "expected static lib at {static_lib:?}");

    println!("cargo:rustc-link-search=native={}", lib_dir.display());
    if target.contains("apple") {
        // macOS: link the archive by absolute path to avoid linker ambiguity.
        println!("cargo:rustc-link-arg={}", static_lib.display());
    } else {
        println!("cargo:rustc-link-lib=static=ghostty-vt");
    }

    // Rebuild only when the vendored source (not the build outputs) changes.
    println!("cargo:rerun-if-changed=build.rs");
    for sub in ["src", "include", "pkg", "build.zig", "build.zig.zon", "VERSION"] {
        println!(
            "cargo:rerun-if-changed={}",
            vendor.join(sub).display()
        );
    }
    println!("cargo:rerun-if-env-changed=ZIG");
    println!("cargo:rerun-if-env-changed=LIBGHOSTTY_VT_OPTIMIZE");
    println!("cargo:rerun-if-env-changed=LIBGHOSTTY_VT_SIMD");
}
