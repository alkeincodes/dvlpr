//! End-to-end integration tests for the control CLI:
//! boots a real daemon on a temp socket, sends control commands via
//! `dvlpr::client::send_command`, and asserts effects.
//!
//! Harness pattern mirrors tests/multi_session.rs exactly.

use std::time::Duration;

use dvlpr::protocol::{
    read_msg, write_msg, ClientHello, ControlCommand, Intent, SplitDir, StatusInfo,
    PROTOCOL_VERSION,
};
use dvlpr::server::{run, socket, ServerConfig};

type Reader = tokio::net::unix::OwnedReadHalf;
type Writer = tokio::net::unix::OwnedWriteHalf;

/// Spawn a daemon on `path` in a dedicated OS thread (each test gets its own thread
/// with a fresh single-threaded runtime, same as multi_session.rs).
fn spawn_session(dir: std::path::PathBuf, name: &str) -> std::path::PathBuf {
    let path = socket::socket_path_in(&dir, name);
    let config = ServerConfig {
        socket_path: path.clone(),
        // Use a long-lived pane command so the daemon doesn't exit mid-test.
        command: vec!["sh".into(), "-c".into(), "sleep 30".into()],
        cwd: ".".into(),
        keymap: Some(dvlpr::config::Config::default()),
        session: name.to_string(),
    };
    std::thread::spawn(move || {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let _ = rt.block_on(run(config));
    });
    path
}

/// Poll until the socket file appears (mirrors multi_session.rs).
async fn wait_for_socket(path: &std::path::Path) {
    for _ in 0..100 {
        if path.exists() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("daemon socket never appeared at {:?}", path);
}

async fn connect(path: &std::path::Path) -> (Reader, Writer) {
    let stream = tokio::net::UnixStream::connect(path).await.unwrap();
    let (r, w) = stream.into_split();
    (r, w)
}

/// Query the daemon for its current status (window/client counts) without attaching.
async fn status(path: &std::path::Path) -> StatusInfo {
    let (mut r, mut w) = connect(path).await;
    write_msg(
        &mut w,
        &ClientHello {
            protocol_version: PROTOCOL_VERSION,
            intent: Intent::Status,
        },
    )
    .await
    .unwrap();
    read_msg::<_, StatusInfo>(&mut r).await.unwrap().unwrap()
}

// ---------------------------------------------------------------------------
// Test 1: WindowNew increases the window count by exactly 1.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn window_new_increases_window_count() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().to_path_buf();
    let path = spawn_session(dir, "wnew");
    wait_for_socket(&path).await;

    let before = status(&path).await.windows;

    let reply = dvlpr::client::send_command(
        &path,
        ControlCommand::WindowNew {
            name: Some("api".into()),
        },
    )
    .await
    .unwrap();
    assert!(
        reply.ok,
        "WindowNew should reply ok=true; message={:?}",
        reply.message
    );

    // Poll until window count grows (the command is applied inside the main loop's
    // next iteration; the Status round-trip is a separate connection).
    let mut after = before;
    for _ in 0..50 {
        after = status(&path).await.windows;
        if after == before + 1 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert_eq!(
        after,
        before + 1,
        "window count should have grown by 1 (before={before}, after={after})"
    );
}

// ---------------------------------------------------------------------------
// Test 2: PaneSplit then PaneClose are both accepted.
// Note: StatusInfo carries only window + client counts, not pane counts, so we
// can only assert the round-trip ok replies — not a state delta from Status.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn pane_split_then_close_is_accepted() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().to_path_buf();
    let path = spawn_session(dir, "splitclose");
    wait_for_socket(&path).await;

    // A fresh daemon starts with 1 pane. PaneSplit → 2 panes; PaneClose → 1 pane.
    // We never drop to 0, so the daemon stays alive throughout.
    let split_reply =
        dvlpr::client::send_command(&path, ControlCommand::PaneSplit(SplitDir::Right))
            .await
            .unwrap();
    assert!(
        split_reply.ok,
        "PaneSplit(Right) should reply ok=true; message={:?}",
        split_reply.message
    );

    let close_reply = dvlpr::client::send_command(&path, ControlCommand::PaneClose)
        .await
        .unwrap();
    assert!(
        close_reply.ok,
        "PaneClose should reply ok=true; message={:?}",
        close_reply.message
    );
}

// ---------------------------------------------------------------------------
// Test 3: Closing the last pane via control is REFUSED (ok=false) and the
// daemon stays alive. Use `dvlpr kill` to stop a session.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn closing_last_pane_via_control_is_refused() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().to_path_buf();
    let path = spawn_session(dir, "solo");
    wait_for_socket(&path).await;

    // The daemon starts with exactly 1 pane. Attempting to close it should be
    // refused (ok=false) — the session must never be emptied via a control command.
    let reply = dvlpr::client::send_command(&path, ControlCommand::PaneClose)
        .await
        .expect("reply must arrive");
    assert!(
        !reply.ok,
        "closing the last pane via control should be refused (ok=false), got {reply:?}"
    );

    // Daemon must still be alive: a subsequent status query should succeed.
    let s = status(&path).await;
    assert!(
        s.windows >= 1,
        "daemon should still be alive after refusing last-pane close (windows={})",
        s.windows
    );
}

// ---------------------------------------------------------------------------
// Test 4: Closing the last window via control is REFUSED (ok=false) and the
// daemon stays alive.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn closing_last_window_via_control_is_refused() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().to_path_buf();
    let path = spawn_session(dir, "solo-win");
    wait_for_socket(&path).await;

    let reply = dvlpr::client::send_command(&path, ControlCommand::WindowClose)
        .await
        .expect("reply must arrive");
    assert!(
        !reply.ok,
        "closing the last window via control should be refused (ok=false), got {reply:?}"
    );

    // Daemon must still be alive.
    let s = status(&path).await;
    assert!(
        s.windows >= 1,
        "daemon should still be alive after refusing last-window close (windows={})",
        s.windows
    );
}

// ---------------------------------------------------------------------------
// Test 5: Sending a command to a socket with no daemon bound returns a clean error.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn command_to_missing_session_errors_cleanly() {
    let tmp = tempfile::tempdir().unwrap();
    let bogus_path = socket::socket_path_in(tmp.path(), "nope");
    // No daemon is listening on bogus_path.

    let err = dvlpr::client::send_command(&bogus_path, ControlCommand::PaneZoom)
        .await
        .unwrap_err();

    assert!(
        matches!(
            err.kind(),
            std::io::ErrorKind::NotFound | std::io::ErrorKind::ConnectionRefused
        ),
        "expected NotFound or ConnectionRefused, got {:?}",
        err.kind()
    );
}

// ---------------------------------------------------------------------------
// Test 6: Closing a NON-last pane via control succeeds (ok=true) and the
// daemon stays alive.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn closing_non_last_pane_via_control_succeeds() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().to_path_buf();
    let path = spawn_session(dir, "non-last-pane");
    wait_for_socket(&path).await;

    // Split to get 2 panes, then close one — should be accepted.
    let split_reply =
        dvlpr::client::send_command(&path, ControlCommand::PaneSplit(SplitDir::Right))
            .await
            .unwrap();
    assert!(
        split_reply.ok,
        "PaneSplit(Right) should reply ok=true; message={:?}",
        split_reply.message
    );

    let close_reply = dvlpr::client::send_command(&path, ControlCommand::PaneClose)
        .await
        .unwrap();
    assert!(
        close_reply.ok,
        "PaneClose (non-last) should reply ok=true; message={:?}",
        close_reply.message
    );

    // Daemon still alive.
    let s = status(&path).await;
    assert!(
        s.windows >= 1,
        "daemon should still be alive (windows={})",
        s.windows
    );
}

// ---------------------------------------------------------------------------
// Test 7: Closing a NON-last window via control succeeds (ok=true) and the
// daemon stays alive.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn closing_non_last_window_via_control_succeeds() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().to_path_buf();
    let path = spawn_session(dir, "non-last-win");
    wait_for_socket(&path).await;

    // Create a second window, then close one — should be accepted.
    let new_reply = dvlpr::client::send_command(
        &path,
        ControlCommand::WindowNew {
            name: Some("extra".into()),
        },
    )
    .await
    .unwrap();
    assert!(
        new_reply.ok,
        "WindowNew should reply ok=true; message={:?}",
        new_reply.message
    );

    // Poll until window count reflects the new window.
    for _ in 0..50 {
        if status(&path).await.windows >= 2 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }

    let close_reply = dvlpr::client::send_command(&path, ControlCommand::WindowClose)
        .await
        .unwrap();
    assert!(
        close_reply.ok,
        "WindowClose (non-last) should reply ok=true; message={:?}",
        close_reply.message
    );

    // Daemon still alive with at least 1 window.
    let s = status(&path).await;
    assert!(
        s.windows >= 1,
        "daemon should still be alive (windows={})",
        s.windows
    );
}
