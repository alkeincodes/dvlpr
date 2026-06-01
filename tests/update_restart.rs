//! Integration: a keep-snapshot shutdown preserves the session snapshot
//! (so `dvlpr update`'s restart-and-restore can rebuild the layout), while a
//! graceful stop still deletes it.

use std::path::PathBuf;
use std::process::{Child, Command};
use std::time::{Duration, Instant};

use dvlpr::protocol::{ClientHello, Intent, PROTOCOL_VERSION};

struct Env {
    runtime: tempfile::TempDir,
    state: tempfile::TempDir,
    config: tempfile::TempDir,
}

fn env() -> Env {
    Env {
        runtime: tempfile::tempdir().unwrap(),
        state: tempfile::tempdir().unwrap(),
        config: tempfile::tempdir().unwrap(),
    }
}

fn spawn_daemon(e: &Env, name: &str) -> Child {
    Command::new(env!("CARGO_BIN_EXE_dvlpr"))
        .arg("server")
        .arg(name)
        .env("DVLPR_RUNTIME_DIR", e.runtime.path())
        .env("XDG_STATE_HOME", e.state.path())
        .env("XDG_CONFIG_HOME", e.config.path())
        .env_remove("DVLPR_RESTORE")
        .spawn()
        .unwrap()
}

fn sock(e: &Env, name: &str) -> PathBuf {
    e.runtime.path().join(format!("{name}.sock"))
}

fn snapshot(e: &Env, name: &str) -> PathBuf {
    e.state.path().join("dvlpr").join(format!("{name}.json"))
}

fn wait_until(mut f: impl FnMut() -> bool, label: &str) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        if f() {
            return;
        }
        std::thread::sleep(Duration::from_millis(25));
    }
    panic!("timed out waiting for: {label}");
}

/// Send a ClientHello{Kill{keep_snapshot}} over the session socket (blocking).
fn send_kill(path: &std::path::Path, keep_snapshot: bool) {
    use std::io::Write;
    use std::os::unix::net::UnixStream;
    let mut s = UnixStream::connect(path).unwrap();
    let hello = ClientHello {
        protocol_version: PROTOCOL_VERSION,
        intent: Intent::Kill { keep_snapshot },
    };
    let payload = dvlpr::protocol::encode(&hello).unwrap();
    s.write_all(&(payload.len() as u32).to_be_bytes()).unwrap();
    s.write_all(&payload).unwrap();
    s.flush().unwrap();
}

#[test]
fn keep_snapshot_shutdown_preserves_snapshot_graceful_deletes() {
    let e = env();

    // 1. keep_snapshot = true -> snapshot file survives the shutdown.
    let mut d = spawn_daemon(&e, "keep");
    wait_until(|| sock(&e, "keep").exists(), "keep socket up");
    // Let the 1s snapshot tick write at least once.
    wait_until(|| snapshot(&e, "keep").exists(), "keep snapshot written");
    send_kill(&sock(&e, "keep"), true);
    wait_until(|| !sock(&e, "keep").exists(), "keep socket gone");
    let _ = d.wait();
    assert!(
        snapshot(&e, "keep").exists(),
        "keep_snapshot=true must NOT delete the snapshot"
    );

    // 2. keep_snapshot = false -> snapshot file is deleted.
    let mut d2 = spawn_daemon(&e, "drop");
    wait_until(|| sock(&e, "drop").exists(), "drop socket up");
    wait_until(|| snapshot(&e, "drop").exists(), "drop snapshot written");
    send_kill(&sock(&e, "drop"), false);
    wait_until(|| !sock(&e, "drop").exists(), "drop socket gone");
    let _ = d2.wait();
    assert!(
        !snapshot(&e, "drop").exists(),
        "keep_snapshot=false (graceful stop) must delete the snapshot"
    );
}
