//! End-to-end integration tests for the control CLI:
//! boots a real daemon on a temp socket, sends control commands via
//! `dvlpr::client::send_command`, and asserts effects.
//!
//! Harness pattern mirrors tests/multi_session.rs exactly.

use std::time::Duration;

use dvlpr::protocol::{
    read_msg, write_msg, ClientHello, ControlCommand, Intent, ServerHello, ServerMsg, SplitDir,
    StatusInfo, PROTOCOL_VERSION,
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
// Test 3: Closing the last pane still delivers an ok reply (Critical regression).
// A daemon starts with exactly one pane; PaneClose closes it, making the session
// empty. The reply must arrive BEFORE the daemon shuts down (deferred shutdown).
// ---------------------------------------------------------------------------

#[tokio::test]
async fn closing_last_pane_still_returns_ok_reply() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().to_path_buf();
    let path = spawn_session(dir, "solo");
    wait_for_socket(&path).await;

    // The daemon starts with exactly 1 pane (sleep 30). Closing it empties the
    // session and should trigger shutdown — but only AFTER the reply is flushed.
    let reply = dvlpr::client::send_command(&path, ControlCommand::PaneClose)
        .await
        .expect("reply must arrive before the daemon exits");
    assert!(
        reply.ok,
        "closing the last pane should ack ok=true, got {reply:?}"
    );
}

// ---------------------------------------------------------------------------
// Test 4: Sending a command to a socket with no daemon bound returns a clean error.
// ---------------------------------------------------------------------------

// ---------------------------------------------------------------------------
// Test 5: Closing the last pane with an interactive client attached shuts down
// cleanly — no panic from compose() indexing an empty windows vec.
//
// Regression guard: with deferred shutdown, the main loop returns to
// tokio::select! after the control command. Between the control reply and
// Event::Shutdown a tick can fire. The tick arm used to call session.compose()
// unconditionally (guarded only by `dirty && !clients.is_empty()`), which panics
// via `self.windows[self.active_window]` when windows is empty. The fix guards
// with `!session.is_empty()` so the tick arm is skipped during mid-shutdown.
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

#[tokio::test]
async fn closing_last_pane_with_attached_client_shuts_down_cleanly() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().to_path_buf();
    let path = spawn_session(dir, "attached-close");
    wait_for_socket(&path).await;

    // Step 1: Open an interactive attach connection. This puts the daemon into
    // its "attached client" code path — the tick arm will see !clients.is_empty()
    // and attempt to compose+broadcast on every dirty tick.
    let stream = tokio::net::UnixStream::connect(&path).await.unwrap();
    let (mut attach_r, mut attach_w) = stream.into_split();
    write_msg(
        &mut attach_w,
        &ClientHello {
            protocol_version: PROTOCOL_VERSION,
            intent: Intent::Attach { cols: 80, rows: 24 },
        },
    )
    .await
    .unwrap();

    // Handshake: expect ServerHello::Ok.
    let hello: ServerHello = read_msg(&mut attach_r).await.unwrap().unwrap();
    assert!(
        matches!(hello, ServerHello::Ok { .. }),
        "expected ServerHello::Ok, got {hello:?}"
    );

    // Step 2: Drain frames from the attach connection in a background task.
    // We collect the final message (Closed or EOF) to confirm a clean shutdown.
    // A panic in the daemon's main loop would abort the session thread, causing
    // the connection to drop abruptly — the reader task would also end, which we
    // detect via the channel below.
    let (done_tx, done_rx) = tokio::sync::oneshot::channel::<bool>();
    tokio::spawn(async move {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        let mut saw_closed = false;
        while let Ok(Ok(Some(msg))) =
            tokio::time::timeout_at(deadline, read_msg::<_, ServerMsg>(&mut attach_r)).await
        {
            if matches!(msg, ServerMsg::Closed { .. } | ServerMsg::Detach) {
                saw_closed = true;
                break;
            }
        }
        // Either a clean Closed/Detach or connection EOF (both acceptable for
        // shutdown; panics would also cause EOF but the PaneClose reply below
        // would not arrive — that's our primary correctness signal).
        let _ = done_tx.send(saw_closed);
    });

    // Step 3: Wait until the daemon sees the attached client (Status.clients >= 1).
    // This ensures the tick arm can actually fire with !clients.is_empty() before
    // we close the pane.
    for _ in 0..50 {
        let s = status(&path).await;
        if s.clients >= 1 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }

    // Step 4: Close the last pane via a control command. This empties the session
    // and triggers deferred shutdown. In the vulnerable window (between this reply
    // and Event::Shutdown), a tick that fires compose() on an empty session would
    // panic — the !session.is_empty() guard prevents that.
    let reply = dvlpr::client::send_command(&path, ControlCommand::PaneClose)
        .await
        .expect("reply must arrive before the daemon exits");
    assert!(
        reply.ok,
        "PaneClose should ack ok=true even with an attached client; got {reply:?}"
    );

    // Step 5: The attached reader task must end cleanly within 5 s. If the daemon
    // panicked, the JoinHandle would complete but the PaneClose reply above would
    // not have arrived — so reaching here already proves no pre-reply panic. The
    // timeout here guards against the daemon hanging instead of shutting down.
    let saw_closed = tokio::time::timeout(Duration::from_secs(5), async {
        done_rx.await.unwrap_or(false)
    })
    .await
    .expect("attached client reader should complete within 5 s after PaneClose");

    assert!(
        saw_closed,
        "attached client should receive ServerMsg::Closed or ServerMsg::Detach before connection drops"
    );
}
