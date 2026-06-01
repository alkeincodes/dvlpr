//! Integration tests for session resurrection. Each test runs the REAL daemon as a
//! child process with its own DVLPR_RUNTIME_DIR + XDG_STATE_HOME, so SIGKILL is a true
//! crash and DVLPR_RESTORE drives the real restore path.
//!
//! Hermeticity: each child daemon also gets an isolated, empty XDG_CONFIG_HOME and HOME
//! so `Config::load` finds no user config and uses compiled-in defaults — crucially the
//! default prefix Ctrl-b, which the `\x02c\r` New-Window keystroke depends on. Without
//! this, a developer's real `~/.config/dvlpr/config.toml` (or `~/.dvlpr/config.toml`)
//! could rebind the prefix and make the structural-change keystroke a no-op.

use std::process::{Child, Command};
use std::time::Duration;

use dvlpr::client::query_status;
use dvlpr::protocol::{
    read_msg, write_msg, ClientHello, ClientMsg, Intent, ServerHello, PROTOCOL_VERSION,
};

fn snapshot_path(state: &std::path::Path, name: &str) -> std::path::PathBuf {
    state.join("dvlpr").join(format!("{name}.json"))
}
fn socket_path(run: &std::path::Path, name: &str) -> std::path::PathBuf {
    run.join(format!("{name}.sock"))
}

/// A child daemon that is SIGKILLed + reaped on drop, so a panicking assertion never
/// leaks a real PTY daemon into the test host.
struct DaemonGuard(Child);
impl Drop for DaemonGuard {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// Spawn `dvlpr server <name>` as a child with isolated runtime + state dirs and an
/// isolated, empty config/home so the daemon loads default config (prefix Ctrl-b).
/// `restore` sets DVLPR_RESTORE=1. `config_home` and `home` are empty temp dirs.
fn spawn_daemon(
    run: &std::path::Path,
    state: &std::path::Path,
    config_home: &std::path::Path,
    home: &std::path::Path,
    name: &str,
    restore: bool,
) -> DaemonGuard {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_dvlpr"));
    cmd.arg("server")
        .arg(name)
        .env("DVLPR_RUNTIME_DIR", run)
        .env("XDG_STATE_HOME", state)
        // Hermetic config: empty XDG_CONFIG_HOME + HOME → no config file found →
        // compiled-in defaults (prefix Ctrl-b), independent of the dev's real config.
        .env("XDG_CONFIG_HOME", config_home)
        .env("HOME", home)
        // Don't let an inherited $DVLPR or $DVLPR_SESSION_CWD perturb the child.
        .env_remove("DVLPR")
        .env_remove("DVLPR_SESSION_CWD");
    if restore {
        cmd.env("DVLPR_RESTORE", "1");
    }
    DaemonGuard(cmd.spawn().expect("spawn dvlpr server"))
}

/// Wait until the daemon is actually LIVE (accepts a connection), not merely until the
/// socket file exists. After a SIGKILL the stale socket file lingers, so a path-exists
/// check would return before the new daemon rebinds. A connect to a stale socket fails
/// with ECONNREFUSED, so this correctly waits for the fresh daemon to bind.
async fn wait_live(path: &std::path::Path) {
    for _ in 0..200 {
        if tokio::net::UnixStream::connect(path).await.is_ok() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("daemon never became live at {path:?}");
}

/// Poll until the snapshot at `path` loads AND satisfies `pred` (e.g. window count),
/// up to ~6s. Returns the loaded snapshot. Panics on timeout.
async fn wait_snapshot(
    path: &std::path::Path,
    pred: impl Fn(&dvlpr::persist::SessionSnapshot) -> bool,
) -> dvlpr::persist::SessionSnapshot {
    for _ in 0..300 {
        if let Some(s) = dvlpr::persist::load(path) {
            if pred(&s) {
                return s;
            }
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    panic!("snapshot at {path:?} never reached expected state");
}

/// Attach and return the (read, write) halves so the caller can send input.
async fn attach(
    path: &std::path::Path,
) -> (
    tokio::net::unix::OwnedReadHalf,
    tokio::net::unix::OwnedWriteHalf,
) {
    let stream = tokio::net::UnixStream::connect(path).await.unwrap();
    let (mut r, mut w) = stream.into_split();
    write_msg(
        &mut w,
        &ClientHello {
            protocol_version: PROTOCOL_VERSION,
            intent: Intent::Attach { cols: 80, rows: 24 },
        },
    )
    .await
    .unwrap();
    let hello: ServerHello = read_msg(&mut r).await.unwrap().unwrap();
    assert!(matches!(hello, ServerHello::Ok { .. }));
    (r, w)
}

/// Send a graceful Intent::Kill to the daemon at `path`.
async fn graceful_kill(path: &std::path::Path) {
    let stream = tokio::net::UnixStream::connect(path).await.unwrap();
    let (_kr, mut kw) = stream.into_split();
    write_msg(
        &mut kw,
        &ClientHello {
            protocol_version: PROTOCOL_VERSION,
            intent: Intent::Kill {
                keep_snapshot: false,
            },
        },
    )
    .await
    .unwrap();
}

#[tokio::test]
async fn abrupt_kill_leaves_snapshot_and_restore_rebuilds_layout() {
    let run = tempfile::tempdir().unwrap();
    let state = tempfile::tempdir().unwrap();
    let cfg = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();
    let snap = snapshot_path(state.path(), "crash");
    let sock = socket_path(run.path(), "crash");

    // 1) Start daemon, attach, create a SECOND window via `prefix c` + Enter, so the
    //    snapshot has a non-trivial 2-window layout to rebuild.
    let mut child = spawn_daemon(
        run.path(),
        state.path(),
        cfg.path(),
        home.path(),
        "crash",
        false,
    );
    wait_live(&sock).await;
    let (_r, mut w) = attach(&sock).await;
    // prefix = Ctrl-b (0x02); `c` opens the New Window dialog; Enter submits (empty
    // name → auto-named new window).
    write_msg(&mut w, &ClientMsg::Input(b"\x02c\r".to_vec()))
        .await
        .unwrap();
    // Let the structural change + 1s snapshot cadence persist it; poll until the
    // snapshot captures both windows instead of racing a fixed sleep under load.
    let loaded = wait_snapshot(&snap, |s| s.windows.len() == 2).await;
    assert_eq!(loaded.windows.len(), 2, "two windows should be captured");

    // 2) SIGKILL — a true abrupt death; no teardown runs.
    child.0.kill().expect("sigkill daemon");
    let _ = child.0.wait();
    assert!(
        snap.exists(),
        "abrupt SIGKILL must leave the snapshot behind"
    );

    // 3) Restore: start a fresh daemon with DVLPR_RESTORE=1; it must rebuild 2 windows.
    let _restored = spawn_daemon(
        run.path(),
        state.path(),
        cfg.path(),
        home.path(),
        "crash",
        true,
    );
    wait_live(&sock).await;
    let status = query_status(&sock).await.expect("status after restore");
    assert_eq!(
        status.windows, 2,
        "restored daemon must rebuild both windows"
    );

    // Cleanup: graceful kill (also exercises snapshot deletion). The DaemonGuard reaps
    // the child on drop regardless.
    graceful_kill(&sock).await;
}

#[tokio::test]
async fn graceful_kill_deletes_the_snapshot() {
    let run = tempfile::tempdir().unwrap();
    let state = tempfile::tempdir().unwrap();
    let cfg = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();
    let snap = snapshot_path(state.path(), "clean");
    let sock = socket_path(run.path(), "clean");

    let _child = spawn_daemon(
        run.path(),
        state.path(),
        cfg.path(),
        home.path(),
        "clean",
        false,
    );
    wait_live(&sock).await;
    let (_r, _w) = attach(&sock).await;
    // Poll until the snapshot exists rather than racing a fixed sleep under load.
    let _ = wait_snapshot(&snap, |_| true).await;

    // Graceful kill via Intent::Kill → teardown deletes the snapshot.
    graceful_kill(&sock).await;
    // Wait for the daemon to actually tear down and unlink the snapshot.
    let mut deleted = false;
    for _ in 0..200 {
        if !snap.exists() {
            deleted = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(deleted, "graceful kill must delete the snapshot");
}
