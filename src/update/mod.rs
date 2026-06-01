//! `dvlpr update` — fetch the latest GH release for this host's target,
//! verify SHA-256, atomically replace the running binary in place.
//!
//! Sync (no tokio): the update flow is a one-shot operator action, not
//! something that participates in the daemon's event loop. Shells out to
//! system `curl` (network I/O) and `sha256sum`/`shasum` (verification);
//! parses GH API JSON via `serde_json` (already a dep) — no new HTTP-
//! client crate. See `docs/superpowers/specs/2026-05-29-binary-release-and-update-design.md`
//! for the design.

use std::io;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;

use serde::Deserialize;

/// The GitHub Releases API response shape we depend on. Only the fields we
/// actually read are listed — `serde_json` ignores the rest.
#[derive(Debug, Deserialize, PartialEq, Eq)]
pub struct Release {
    pub tag_name: String,
    pub assets: Vec<Asset>,
}

#[derive(Debug, Deserialize, PartialEq, Eq)]
pub struct Asset {
    pub name: String,
    pub browser_download_url: String,
}

/// Indirection over network I/O. Production uses `CurlFetcher`; tests pass
/// a stub that returns canned bytes.
pub trait Fetch {
    /// Download URL to `dest` (used for tarballs and `.sha256` files).
    fn fetch_to(&self, url: &str, dest: &Path) -> io::Result<()>;
    /// Fetch URL and return the body as String (used for the GH API JSON).
    fn fetch_string(&self, url: &str) -> io::Result<String>;
}

/// Production [`Fetch`] impl — shells out to system `curl`.
pub struct CurlFetcher;

impl Fetch for CurlFetcher {
    fn fetch_to(&self, url: &str, dest: &Path) -> io::Result<()> {
        let status = Command::new("curl")
            .args(["-fsSL", "-o"])
            .arg(dest)
            .arg(url)
            .status()?;
        if !status.success() {
            return Err(io::Error::other(format!(
                "curl exited {} fetching {url}",
                status.code().unwrap_or(-1)
            )));
        }
        Ok(())
    }

    fn fetch_string(&self, url: &str) -> io::Result<String> {
        let out = Command::new("curl").args(["-fsSL", url]).output()?;
        if !out.status.success() {
            return Err(io::Error::other(format!(
                "curl exited {} fetching {url}",
                out.status.code().unwrap_or(-1)
            )));
        }
        String::from_utf8(out.stdout).map_err(io::Error::other)
    }
}

/// Compile-time release host. Set by `build.rs` via `DVLPR_RELEASE_REPO`
/// (defaults to `alkeincodes/dvlpr`; fork builds override via the same env).
pub fn release_repo() -> &'static str {
    env!("DVLPR_RELEASE_REPO")
}

/// Compile-time host target triple. Set by `build.rs` via `DVLPR_TARGET`.
pub fn host_target() -> &'static str {
    env!("DVLPR_TARGET")
}

/// Fetch `https://api.github.com/repos/{repo}/releases/latest` and parse it.
pub fn fetch_latest_release<F: Fetch>(fetcher: &F, repo: &str) -> io::Result<Release> {
    let url = format!("https://api.github.com/repos/{repo}/releases/latest");
    let body = fetcher.fetch_string(&url)?;
    serde_json::from_str(&body).map_err(io::Error::other)
}

/// Parse `vMAJOR.MINOR.PATCH` (the GH tag convention) into a triple. Rejects
/// missing `v` prefix and non-numeric components so a bogus tag like
/// `v0.2-rc1` fails fast rather than silently sorting wrong.
pub fn parse_remote_version(tag: &str) -> io::Result<(u32, u32, u32)> {
    let Some(rest) = tag.strip_prefix('v') else {
        return Err(io::Error::other(format!(
            "remote tag {tag:?} must start with 'v'"
        )));
    };
    let parts: Vec<&str> = rest.split('.').collect();
    if parts.len() != 3 {
        return Err(io::Error::other(format!(
            "remote tag {tag:?} is not MAJOR.MINOR.PATCH"
        )));
    }
    let parse = |s: &str| -> io::Result<u32> { s.parse().map_err(io::Error::other) };
    Ok((parse(parts[0])?, parse(parts[1])?, parse(parts[2])?))
}

/// Current binary's compile-time version, parsed from `CARGO_PKG_VERSION`.
/// Panics in tests if the Cargo.toml version is malformed — that's a build
/// invariant, not a runtime error.
pub fn current_version() -> (u32, u32, u32) {
    parse_remote_version(&format!("v{}", env!("CARGO_PKG_VERSION")))
        .expect("CARGO_PKG_VERSION is not MAJOR.MINOR.PATCH — fix Cargo.toml")
}

/// Map a Rust target triple to its release-asset basename.
pub fn host_asset_basename(target_triple: &str) -> io::Result<&'static str> {
    Ok(match target_triple {
        "x86_64-unknown-linux-gnu" => "dvlpr-x86_64-linux.tar.gz",
        "aarch64-unknown-linux-gnu" => "dvlpr-aarch64-linux.tar.gz",
        "x86_64-apple-darwin" => "dvlpr-x86_64-macos.tar.gz",
        "aarch64-apple-darwin" => "dvlpr-aarch64-macos.tar.gz",
        other => {
            return Err(io::Error::other(format!(
                "no prebuilt asset for target {other:?} — build from source"
            )))
        }
    })
}

/// Find the (binary, sha256) asset pair for a given basename. Both must be
/// present; either-missing is a release-pipeline bug worth surfacing.
pub fn asset_pair_for<'a>(
    release: &'a Release,
    basename: &str,
) -> io::Result<(&'a Asset, &'a Asset)> {
    let sha_name = format!("{basename}.sha256");
    let binary = release
        .assets
        .iter()
        .find(|a| a.name == basename)
        .ok_or_else(|| {
            io::Error::other(format!(
                "release {} missing binary asset {basename}",
                release.tag_name
            ))
        })?;
    let sha = release
        .assets
        .iter()
        .find(|a| a.name == sha_name)
        .ok_or_else(|| {
            io::Error::other(format!(
                "release {} missing sha256 asset {sha_name}",
                release.tag_name
            ))
        })?;
    Ok((binary, sha))
}

/// RAII guard: removes the staging directory on Drop (success, error, or
/// panic). Drop-during-unwind is guaranteed by Rust's default panic=unwind
/// strategy, which dvlpr uses (verify via `panic = "unwind"` absence in
/// Cargo.toml's [profile.release] — absence means default = unwind).
struct StagingGuard(PathBuf);

impl Drop for StagingGuard {
    fn drop(&mut self) {
        // Best-effort: a failed cleanup is reported via tracing in
        // production (no tracing here to keep the test surface tight),
        // never re-raised — Drop must not panic.
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// Best-effort write-permission check on `dir`. Opens a probe file for
/// O_RDWR; success means the user has at least write access. Cleans up
/// the probe file on success.
///
/// Caveat: a `false` return is "we couldn't write" without distinguishing
/// EACCES (the "needs sudo" intent), EROFS (read-only mount), or ENOSPC
/// (disk full). Callers should treat false as "can't write here" rather
/// than strictly "permission denied" — the exit-2 messaging surfaced to
/// users says "rerun as: sudo dvlpr update", which is still the right
/// first-line action even on EROFS/ENOSPC.
pub fn parent_writable(dir: &Path) -> bool {
    let probe = dir.join(format!(".dvlpr-write-probe-{}", std::process::id()));
    match std::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .open(&probe)
    {
        Ok(_) => {
            let _ = std::fs::remove_file(&probe);
            true
        }
        Err(_) => false,
    }
}

/// Staging→download→verify→extract→atomic-replace pipeline.
///
/// - `install_dir` MUST be the canonicalized parent of `current_exe_canonical`
///   (the caller — `run_with` in the next task — does the canonicalize).
/// - `basename` is the exact tarball filename the sha256 file references,
///   e.g. `dvlpr-x86_64-linux.tar.gz`. Mismatched basename → sha verify fails.
/// - On success: `current_exe_canonical` contains the new binary; staging
///   directory has been removed.
/// - On error: `current_exe_canonical` is unchanged; staging directory has
///   been removed (RAII guard).
pub fn install_self<F: Fetch>(
    fetcher: &F,
    binary_url: &str,
    sha_url: &str,
    install_dir: &Path,
    basename: &str,
    current_exe_canonical: &Path,
) -> io::Result<()> {
    // 1. Create same-FS staging dir. Bind the RAII guard IMMEDIATELY so a
    //    later metadata/set_permissions failure still cleans the dir.
    let staging = install_dir.join(format!(".dvlpr-update-{}", std::process::id()));
    std::fs::create_dir_all(&staging)?;
    let _guard = StagingGuard(staging.clone());
    let mut perms = std::fs::metadata(&staging)?.permissions();
    perms.set_mode(0o700);
    std::fs::set_permissions(&staging, perms)?;

    // 2. Download tarball + sha256 file UNDER THE EXACT BASENAME (the sha
    //    file references the tarball by basename, so verify needs the same
    //    name in cwd).
    let tarball = staging.join(basename);
    let sha_path = staging.join(format!("{basename}.sha256"));
    fetcher.fetch_to(binary_url, &tarball)?;
    fetcher.fetch_to(sha_url, &sha_path)?;

    // 3. Verify SHA-256. sha256sum (Linux) and shasum -a 256 (macOS) accept
    //    the same "<hex>  <filename>\n" file format with `-c`. Try
    //    sha256sum first, fall back to shasum.
    let verify_status = if Command::new("sha256sum").arg("--version").output().is_ok() {
        Command::new("sha256sum")
            .arg("-c")
            .arg(sha_path.file_name().unwrap())
            .current_dir(&staging)
            .status()?
    } else {
        Command::new("shasum")
            .args(["-a", "256", "-c"])
            .arg(sha_path.file_name().unwrap())
            .current_dir(&staging)
            .status()?
    };
    if !verify_status.success() {
        return Err(io::Error::other(format!(
            "sha256 verify failed for {basename}"
        )));
    }

    // 4. Extract tarball. -C <staging> writes the binary as staging/dvlpr.
    let extract_status = Command::new("tar")
        .args(["-xzf"])
        .arg(&tarball)
        .arg("-C")
        .arg(&staging)
        .status()?;
    if !extract_status.success() {
        return Err(io::Error::other(format!("tar -xzf failed for {basename}")));
    }
    // Tarball convention: each release ships a single top-level binary
    // named `dvlpr`. The release-workflow tar invocation is `tar -czf
    // <basename> dvlpr`, so this name is the entry name.
    const EXTRACTED_BINARY_NAME: &str = "dvlpr";
    let extracted = staging.join(EXTRACTED_BINARY_NAME);
    if !extracted.exists() {
        return Err(io::Error::other(
            "extracted tarball did not contain a 'dvlpr' binary",
        ));
    }

    // 5. Atomic rename onto the canonical current_exe path. Same FS
    //    (staging is under install_dir) → rename(2) is atomic on Linux
    //    and macOS.
    std::fs::rename(&extracted, current_exe_canonical)?;
    Ok(())
}

/// Map `run_with`'s result to a documented exit code.
///   0 — success or already on latest
///   2 — install dir not writable (rerun with sudo)
///   1 — any other failure (network, JSON parse, sha mismatch, no asset, …)
///
/// Side-effect: prints the error message to stderr prefixed with
/// `dvlpr update: ` for non-zero exits. Stays out of stdout so a caller
/// scripting around `dvlpr update` can pipe stdout cleanly.
pub fn error_to_exit(result: io::Result<()>) -> i32 {
    match result {
        Ok(()) => 0,
        Err(e) => {
            eprintln!("dvlpr update: {e}");
            if e.kind() == io::ErrorKind::PermissionDenied {
                2
            } else {
                1
            }
        }
    }
}

/// Lower-seam orchestration — takes injected dependencies so tests can
/// exercise the writability/permission-denied path without a real exe
/// to write to. `run()` (below) wires this up with production defaults.
pub fn run_with<F: Fetch>(
    fetcher: &F,
    writable: impl Fn(&Path) -> bool,
    repo: &str,
) -> io::Result<()> {
    // 1. Fetch + parse latest release JSON.
    let release = fetch_latest_release(fetcher, repo)?;

    // 2. Compare versions.
    let remote = parse_remote_version(&release.tag_name)?;
    let current = current_version();
    if remote <= current {
        println!(
            "dvlpr {}.{}.{} — already on the latest release ({}).",
            current.0, current.1, current.2, release.tag_name
        );
        return Ok(());
    }

    // 3. Pick the asset pair for this host triple.
    let basename = host_asset_basename(host_target())?;
    let (binary, sha) = asset_pair_for(&release, basename)?;

    // 4. Resolve install location.
    let current_exe = std::env::current_exe()?.canonicalize()?;
    let install_dir = current_exe
        .parent()
        .ok_or_else(|| io::Error::other("current_exe has no parent directory"))?
        .to_path_buf();

    // 5. Writability check — bail with PermissionDenied (maps to exit 2)
    //    BEFORE any download work. The `Fn(&Path) -> bool` indirection lets
    //    tests force the bail without needing a real read-only directory.
    if !writable(&install_dir) {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!(
                "install dir {} is not writable — rerun as: sudo dvlpr update",
                install_dir.display()
            ),
        ));
    }

    // 6. Stage, download, verify, extract, atomic rename.
    install_self(
        fetcher,
        &binary.browser_download_url,
        &sha.browser_download_url,
        &install_dir,
        basename,
        &current_exe,
    )?;

    println!(
        "Updated dvlpr {}.{}.{} → {}.{}.{}.",
        current.0, current.1, current.2, remote.0, remote.1, remote.2
    );
    println!("Running sessions keep the old binary until you `dvlpr stop -t <name>` and reattach.");
    Ok(())
}

/// Production entry point: wires `run_with` to `CurlFetcher`,
/// `parent_writable`, and the compile-time `release_repo()`. Returns
/// an exit code suitable for `std::process::exit`.
pub fn run() -> i32 {
    let result = run_with(&CurlFetcher, parent_writable, release_repo());
    error_to_exit(result)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_remote_version_round_trips_common_tags() {
        assert_eq!(parse_remote_version("v0.2.0").unwrap(), (0, 2, 0));
        assert_eq!(parse_remote_version("v1.10.42").unwrap(), (1, 10, 42));
    }

    #[test]
    fn parse_remote_version_rejects_malformed_tags() {
        assert!(parse_remote_version("0.2.0").is_err()); // no v prefix
        assert!(parse_remote_version("v0.2").is_err()); // missing patch
        assert!(parse_remote_version("v0.2.0-rc1").is_err()); // non-numeric patch
        assert!(parse_remote_version("vX.Y.Z").is_err()); // all non-numeric
        assert!(parse_remote_version("").is_err()); // empty
    }

    #[test]
    fn current_version_matches_cargo_pkg_version() {
        // current_version() and parse_remote_version() must agree on the same
        // string. This is structural — it doesn't lock a specific version
        // floor, so it stays correct across future Cargo.toml bumps (incl. v1+).
        let parsed = parse_remote_version(&format!("v{}", env!("CARGO_PKG_VERSION")))
            .expect("CARGO_PKG_VERSION must be MAJOR.MINOR.PATCH");
        assert_eq!(current_version(), parsed);
    }

    #[test]
    fn host_asset_basename_covers_all_four_supported_targets() {
        assert_eq!(
            host_asset_basename("x86_64-unknown-linux-gnu").unwrap(),
            "dvlpr-x86_64-linux.tar.gz"
        );
        assert_eq!(
            host_asset_basename("aarch64-unknown-linux-gnu").unwrap(),
            "dvlpr-aarch64-linux.tar.gz"
        );
        assert_eq!(
            host_asset_basename("x86_64-apple-darwin").unwrap(),
            "dvlpr-x86_64-macos.tar.gz"
        );
        assert_eq!(
            host_asset_basename("aarch64-apple-darwin").unwrap(),
            "dvlpr-aarch64-macos.tar.gz"
        );
    }

    #[test]
    fn host_asset_basename_rejects_unsupported_target() {
        let err = host_asset_basename("x86_64-unknown-linux-musl").unwrap_err();
        assert!(err.to_string().contains("no prebuilt asset"));
    }

    #[test]
    fn fetch_latest_release_parses_canonical_github_json() {
        // Minimal GH API shape — fixture is a stripped real response.
        let body = r#"{
            "tag_name": "v0.2.1",
            "assets": [
                {"name": "dvlpr-x86_64-linux.tar.gz",
                 "browser_download_url": "https://example.test/bin"},
                {"name": "dvlpr-x86_64-linux.tar.gz.sha256",
                 "browser_download_url": "https://example.test/sha"}
            ]
        }"#;
        struct StubBody(&'static str);
        impl Fetch for StubBody {
            fn fetch_to(&self, _: &str, _: &Path) -> io::Result<()> {
                unreachable!()
            }
            fn fetch_string(&self, _: &str) -> io::Result<String> {
                Ok(self.0.into())
            }
        }
        let release = fetch_latest_release(&StubBody(body), "fake/repo").unwrap();
        assert_eq!(release.tag_name, "v0.2.1");
        assert_eq!(release.assets.len(), 2);
        assert_eq!(release.assets[0].name, "dvlpr-x86_64-linux.tar.gz");
    }

    #[test]
    fn asset_pair_for_finds_matching_pair() {
        let release = Release {
            tag_name: "v0.2.1".into(),
            assets: vec![
                Asset {
                    name: "dvlpr-x86_64-linux.tar.gz".into(),
                    browser_download_url: "https://x/bin".into(),
                },
                Asset {
                    name: "dvlpr-x86_64-linux.tar.gz.sha256".into(),
                    browser_download_url: "https://x/sha".into(),
                },
                Asset {
                    name: "dvlpr-aarch64-macos.tar.gz".into(),
                    browser_download_url: "https://x/mac-bin".into(),
                },
            ],
        };
        let (bin, sha) = asset_pair_for(&release, "dvlpr-x86_64-linux.tar.gz").unwrap();
        assert_eq!(bin.name, "dvlpr-x86_64-linux.tar.gz");
        assert_eq!(sha.name, "dvlpr-x86_64-linux.tar.gz.sha256");
    }

    #[test]
    fn asset_pair_for_errors_when_sha_is_missing() {
        let release = Release {
            tag_name: "v0.2.1".into(),
            assets: vec![Asset {
                name: "dvlpr-x86_64-linux.tar.gz".into(),
                browser_download_url: "https://x/bin".into(),
            }],
        };
        let err = asset_pair_for(&release, "dvlpr-x86_64-linux.tar.gz").unwrap_err();
        assert!(err.to_string().contains("missing sha256"));
    }

    #[test]
    fn asset_pair_for_errors_when_binary_is_missing() {
        let release = Release {
            tag_name: "v0.2.1".into(),
            assets: vec![Asset {
                name: "dvlpr-x86_64-linux.tar.gz.sha256".into(),
                browser_download_url: "https://x/sha".into(),
            }],
        };
        let err = asset_pair_for(&release, "dvlpr-x86_64-linux.tar.gz").unwrap_err();
        assert!(err.to_string().contains("missing binary asset"));
    }
}
