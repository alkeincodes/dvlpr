//! CLI-boundary tests pinning the `dvlpr update` exit code contract:
//!   0 = success OR already on latest
//!   1 = real failure (network, sha mismatch, no asset, etc.)
//!   2 = install dir not writable (rerun with sudo)

use std::fs;
use std::io::{self, Write};
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::Command;

use dvlpr::update::{error_to_exit, run_with, Fetch};

struct PermDeniedFetcher;
impl Fetch for PermDeniedFetcher {
    fn fetch_to(&self, _: &str, _: &Path) -> io::Result<()> {
        unreachable!("writability check must short-circuit before fetch_to")
    }
    fn fetch_string(&self, url: &str) -> io::Result<String> {
        // run_with calls fetch_string FIRST to get the release JSON, then
        // does the writability check after asset matching. Returning a
        // minimal valid release with the right host asset means the flow
        // reaches the writability check.
        if !url.starts_with("https://api.github.com/repos/") {
            return Err(io::Error::other(format!("unexpected url {url}")));
        }
        let basename = dvlpr::update::host_asset_basename(dvlpr::update::host_target())
            .unwrap_or("dvlpr-x86_64-linux.tar.gz");
        Ok(format!(
            r#"{{
                "tag_name": "v99.99.99",
                "assets": [
                    {{"name": "{basename}",
                      "browser_download_url": "stub://bin"}},
                    {{"name": "{basename}.sha256",
                      "browser_download_url": "stub://sha"}}
                ]
            }}"#
        ))
    }
}

/// Returns a `Release` JSON whose tag_name equals the current binary's
/// version, so `run_with` should short-circuit on the "already latest"
/// fast-path and never call `fetch_to`.
struct AlreadyLatestFetcher;
impl Fetch for AlreadyLatestFetcher {
    fn fetch_to(&self, _: &str, _: &Path) -> io::Result<()> {
        unreachable!("'already latest' path must short-circuit before fetch_to")
    }
    fn fetch_string(&self, _: &str) -> io::Result<String> {
        let v = dvlpr::update::current_version();
        Ok(format!(
            r#"{{
                "tag_name": "v{}.{}.{}",
                "assets": []
            }}"#,
            v.0, v.1, v.2
        ))
    }
}

#[test]
fn run_with_returns_permission_denied_when_install_dir_not_writable() {
    let result = run_with(&PermDeniedFetcher, |_| false, "fake/repo");
    let err = result.expect_err("non-writable install dir must error");
    assert_eq!(
        err.kind(),
        io::ErrorKind::PermissionDenied,
        "expected PermissionDenied, got {:?}: {err}",
        err.kind()
    );
    assert_eq!(error_to_exit(Err(err)), 2);
}

#[test]
fn run_with_returns_ok_without_download_when_already_latest() {
    let result = run_with(&AlreadyLatestFetcher, |_| true, "fake/repo");
    assert!(
        result.is_ok(),
        "already-latest path must return Ok, got: {result:?}"
    );
    assert_eq!(error_to_exit(result), 0);
}

#[test]
fn error_to_exit_maps_kinds_to_documented_codes() {
    assert_eq!(error_to_exit(Ok(())), 0);
    assert_eq!(
        error_to_exit(Err(io::Error::new(io::ErrorKind::PermissionDenied, "x"))),
        2
    );
    assert_eq!(
        error_to_exit(Err(io::Error::new(io::ErrorKind::ConnectionRefused, "x"))),
        1
    );
    assert_eq!(error_to_exit(Err(io::Error::other("any other"))), 1);
}

/// chmod 0o755 helper.
fn make_executable(path: &Path) {
    let mut perms = fs::metadata(path).unwrap().permissions();
    perms.set_mode(0o755);
    fs::set_permissions(path, perms).unwrap();
}

#[test]
fn dvlpr_update_subprocess_exits_1_when_curl_unavailable() {
    let tmp = tempfile::TempDir::new().unwrap();
    let fake_curl = tmp.path().join("curl");
    fs::write(&fake_curl, "#!/bin/sh\nexit 7\n").unwrap();
    make_executable(&fake_curl);

    let output = Command::new(env!("CARGO_BIN_EXE_dvlpr"))
        .arg("update")
        .env("PATH", tmp.path())
        .output()
        .expect("spawn dvlpr update");

    let code = output.status.code();
    assert_eq!(
        code,
        Some(1),
        "expected exit 1 on curl failure, got {code:?}\nstderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("dvlpr update:"),
        "stderr must start with 'dvlpr update:' prefix, got: {stderr}"
    );
    let _ = io::stdout().write_all(&output.stdout);
}

#[test]
fn dvlpr_update_subprocess_exits_2_when_install_dir_not_writable() {
    let is_root = unsafe { libc::geteuid() } == 0;
    if is_root {
        eprintln!("skipping: running as root (no permission boundary applies)");
        return;
    }

    let tmp = tempfile::TempDir::new().unwrap();

    let install_dir = tmp.path().join("ro-install");
    fs::create_dir(&install_dir).unwrap();
    let target_bin = install_dir.join("dvlpr");
    fs::copy(env!("CARGO_BIN_EXE_dvlpr"), &target_bin).unwrap();
    make_executable(&target_bin);

    let path_dir = tmp.path().join("path");
    fs::create_dir(&path_dir).unwrap();
    let response_file = tmp.path().join("response.json");
    let basename = dvlpr::update::host_asset_basename(dvlpr::update::host_target())
        .expect("test runs on a supported host triple");
    let response_json = format!(
        r#"{{
  "tag_name": "v99.99.99",
  "assets": [
    {{"name": "{basename}", "browser_download_url": "stub://bin"}},
    {{"name": "{basename}.sha256", "browser_download_url": "stub://sha"}}
  ]
}}"#
    );
    fs::write(&response_file, response_json).unwrap();
    let fake_curl = path_dir.join("curl");
    let curl_script = format!(
        "#!/bin/sh\ncase \"$*\" in *api.github.com*) cat '{response}' ;; *) exit 1 ;; esac\n",
        response = response_file.display()
    );
    fs::write(&fake_curl, curl_script).unwrap();
    make_executable(&fake_curl);

    let mut dir_perms = fs::metadata(&install_dir).unwrap().permissions();
    dir_perms.set_mode(0o555);
    fs::set_permissions(&install_dir, dir_perms).unwrap();

    struct RestoreDir(std::path::PathBuf);
    impl Drop for RestoreDir {
        fn drop(&mut self) {
            if let Ok(meta) = fs::metadata(&self.0) {
                let mut perms = meta.permissions();
                perms.set_mode(0o755);
                let _ = fs::set_permissions(&self.0, perms);
            }
        }
    }
    let _restore = RestoreDir(install_dir.clone());

    let path_env = format!("{}:/bin:/usr/bin", path_dir.display());

    let output = Command::new(&target_bin)
        .arg("update")
        .env("PATH", &path_env)
        .output()
        .expect("spawn dvlpr update");

    let code = output.status.code();
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(
        code,
        Some(2),
        "expected exit 2 from non-writable install dir, got {code:?}\nstderr: {stderr}"
    );
    assert!(
        stderr.contains("sudo"),
        "stderr must mention 'sudo dvlpr update', got: {stderr}"
    );
    let _ = io::stdout().write_all(&output.stdout);
}
