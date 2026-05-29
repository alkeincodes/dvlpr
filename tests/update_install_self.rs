//! Integration tests for the `install_self` staging→verify→extract→rename
//! pipeline. No network: a stub `Fetch` impl returns canned bytes.

use std::fs;
use std::io::{self, Write};
use std::os::unix::fs::PermissionsExt;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::path::{Path, PathBuf};
use std::process::Command;

use dvlpr::update::{install_self, Fetch};

const FAKE_BIN_URL: &str = "stub://binary";
const FAKE_SHA_URL: &str = "stub://sha";

/// Build a real tarball containing a single executable `dvlpr` with the
/// given content, and return (tarball bytes, sha256 file content).
fn make_release_artifact(content: &[u8]) -> (Vec<u8>, String) {
    let tmp = tempfile::TempDir::new().unwrap();
    let payload = tmp.path().join("dvlpr");
    fs::write(&payload, content).unwrap();
    let mut perms = fs::metadata(&payload).unwrap().permissions();
    perms.set_mode(0o755);
    fs::set_permissions(&payload, perms).unwrap();

    let tarball = tmp.path().join("dvlpr-x86_64-linux.tar.gz");
    let status = Command::new("tar")
        .args(["-czf"])
        .arg(&tarball)
        .args(["-C"])
        .arg(tmp.path())
        .arg("dvlpr")
        .status()
        .unwrap();
    assert!(status.success(), "tar failed");

    let tar_bytes = fs::read(&tarball).unwrap();

    // sha256sum's output format is "<hex>  <filename>\n" on Linux.
    // shasum -a 256 on macOS is the same format. Both `sha256sum -c` and
    // `shasum -a 256 -c` accept this. We assemble it manually so the test
    // works regardless of which tool is present.
    let sha = sha256_hex(&tar_bytes);
    let sha_file = format!("{sha}  dvlpr-x86_64-linux.tar.gz\n");

    (tar_bytes, sha_file)
}

/// Compute the SHA-256 of `bytes` and return it as lowercase hex.
fn sha256_hex(bytes: &[u8]) -> String {
    // Shell out to whichever sha tool is on this host; tests run on dev
    // machines that have at least one of these.
    let mut child = if Command::new("sha256sum").arg("--version").output().is_ok() {
        Command::new("sha256sum")
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .spawn()
            .unwrap()
    } else {
        Command::new("shasum")
            .args(["-a", "256"])
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .spawn()
            .unwrap()
    };
    child.stdin.as_mut().unwrap().write_all(bytes).unwrap();
    let out = child.wait_with_output().unwrap();
    let line = String::from_utf8(out.stdout).unwrap();
    // Output is "<hex>  -" (stdin → '-' filename). Take the first whitespace-delim word.
    line.split_whitespace().next().unwrap().to_string()
}

struct StubFetcher {
    tar_bytes: Vec<u8>,
    sha_text: String,
}

impl Fetch for StubFetcher {
    fn fetch_to(&self, url: &str, dest: &Path) -> io::Result<()> {
        match url {
            FAKE_BIN_URL => fs::write(dest, &self.tar_bytes),
            FAKE_SHA_URL => fs::write(dest, &self.sha_text),
            other => Err(io::Error::other(format!("unexpected stub url {other}"))),
        }
    }
    fn fetch_string(&self, _: &str) -> io::Result<String> {
        unreachable!("install_self never calls fetch_string")
    }
}

fn setup_install_dir(initial_content: &[u8]) -> (tempfile::TempDir, PathBuf) {
    let tmp = tempfile::TempDir::new().unwrap();
    let install_dir = tmp.path().to_path_buf();
    let current_exe = install_dir.join("dvlpr");
    fs::write(&current_exe, initial_content).unwrap();
    let mut perms = fs::metadata(&current_exe).unwrap().permissions();
    perms.set_mode(0o755);
    fs::set_permissions(&current_exe, perms).unwrap();
    (tmp, current_exe)
}

fn count_staging_dirs(install_dir: &Path) -> usize {
    fs::read_dir(install_dir)
        .unwrap()
        .filter_map(|e| e.ok())
        .filter(|e| {
            e.file_name()
                .to_string_lossy()
                .starts_with(".dvlpr-update-")
        })
        .count()
}

#[test]
fn install_self_replaces_binary_atomically() {
    let (tarball, sha_text) = make_release_artifact(b"new content v2");
    let stub = StubFetcher {
        tar_bytes: tarball,
        sha_text,
    };
    let (_tmp, current_exe) = setup_install_dir(b"old content v1");
    let install_dir = current_exe.parent().unwrap().to_path_buf();

    install_self(
        &stub,
        FAKE_BIN_URL,
        FAKE_SHA_URL,
        &install_dir,
        "dvlpr-x86_64-linux.tar.gz",
        &current_exe,
    )
    .expect("install_self should succeed on valid inputs");

    assert_eq!(
        fs::read(&current_exe).unwrap(),
        b"new content v2",
        "current_exe should hold the new binary content"
    );
    assert_eq!(
        count_staging_dirs(&install_dir),
        0,
        "staging dir should be cleaned up on success"
    );
    let mode = fs::metadata(&current_exe).unwrap().permissions().mode();
    assert!(
        mode & 0o111 != 0,
        "installed binary must have execute bit set (got mode {mode:o})"
    );
}

#[test]
fn install_self_rejects_corrupt_tarball() {
    let (tarball, _real_sha) = make_release_artifact(b"new content v2");
    let bogus_sha = format!("{}  dvlpr-x86_64-linux.tar.gz\n", "0".repeat(64));
    let stub = StubFetcher {
        tar_bytes: tarball,
        sha_text: bogus_sha,
    };
    let (_tmp, current_exe) = setup_install_dir(b"old content v1");
    let install_dir = current_exe.parent().unwrap().to_path_buf();

    let err = install_self(
        &stub,
        FAKE_BIN_URL,
        FAKE_SHA_URL,
        &install_dir,
        "dvlpr-x86_64-linux.tar.gz",
        &current_exe,
    )
    .expect_err("sha mismatch must fail");
    assert!(
        err.to_string().to_lowercase().contains("sha"),
        "error should mention sha: {err}"
    );
    assert_eq!(
        fs::read(&current_exe).unwrap(),
        b"old content v1",
        "current_exe must remain unchanged on sha failure"
    );
    assert_eq!(
        count_staging_dirs(&install_dir),
        0,
        "staging dir must be cleaned up even on failure"
    );
}

#[test]
fn install_self_cleans_staging_on_panic() {
    struct PanicFetcher;
    impl Fetch for PanicFetcher {
        fn fetch_to(&self, _: &str, _: &Path) -> io::Result<()> {
            panic!("simulated network panic mid-pipeline");
        }
        fn fetch_string(&self, _: &str) -> io::Result<String> {
            unreachable!()
        }
    }
    let (_tmp, current_exe) = setup_install_dir(b"old content v1");
    let install_dir = current_exe.parent().unwrap().to_path_buf();

    let result = catch_unwind(AssertUnwindSafe(|| {
        install_self(
            &PanicFetcher,
            FAKE_BIN_URL,
            FAKE_SHA_URL,
            &install_dir,
            "dvlpr-x86_64-linux.tar.gz",
            &current_exe,
        )
    }));
    assert!(result.is_err(), "panic should have unwound");
    assert_eq!(
        count_staging_dirs(&install_dir),
        0,
        "Drop guard must remove staging dir during panic unwind"
    );
}
