use std::time::Duration;

use dvlpr::protocol::{
    read_msg, write_msg, ClientHello, ClientMsg, ServerHello, ServerMsg, PROTOCOL_VERSION,
};
use dvlpr::server::{run, ServerConfig};

type Reader = tokio::net::unix::OwnedReadHalf;
type Writer = tokio::net::unix::OwnedWriteHalf;

/// Spawn a daemon whose panes all run `cat`, returning the socket path. Pins the
/// default keymap so the tests are hermetic regardless of any local user config.
fn spawn_daemon(socket_path: std::path::PathBuf) {
    let config = ServerConfig {
        socket_path,
        command: vec!["cat".into()],
        cwd: ".".into(),
        keymap: Some(dvlpr::config::Config::default()),
    };
    std::thread::spawn(move || {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let _ = rt.block_on(run(config));
    });
}

async fn wait_for_socket(path: &std::path::Path) {
    for _ in 0..100 {
        if path.exists() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("daemon socket never appeared");
}

async fn handshake(path: &std::path::Path, cols: u16, rows: u16) -> (Reader, Writer) {
    let stream = tokio::net::UnixStream::connect(path).await.unwrap();
    let (mut r, mut w) = stream.into_split();
    write_msg(
        &mut w,
        &ClientHello {
            protocol_version: PROTOCOL_VERSION,
            cols,
            rows,
        },
    )
    .await
    .unwrap();
    let hello: ServerHello = read_msg(&mut r).await.unwrap().unwrap();
    assert!(matches!(hello, ServerHello::Ok { .. }));
    (r, w)
}

async fn send_input(w: &mut Writer, bytes: &[u8]) {
    write_msg(w, &ClientMsg::Input(bytes.to_vec()))
        .await
        .unwrap();
}

/// Read frames until `pred(frame_text)` holds; returns true if it did within `secs`.
async fn until_frame<F: Fn(&str) -> bool>(r: &mut Reader, secs: u64, pred: F) -> bool {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(secs);
    while let Ok(Ok(Some(msg))) =
        tokio::time::timeout_at(deadline, read_msg::<_, ServerMsg>(r)).await
    {
        if let ServerMsg::Frame { data, .. } = msg {
            if pred(&String::from_utf8_lossy(&data)) {
                return true;
            }
        }
    }
    false
}

/// The 1-based column of the trailing `ESC [ row ; col H` cursor-position escape.
fn last_cursor_col(frame: &str) -> Option<u16> {
    let idx = frame.rfind("\x1b[")?;
    let tail = &frame[idx + 2..];
    let h = tail.find('H')?;
    let body = &tail[..h];
    let col = body.split(';').nth(1)?;
    col.parse().ok()
}

#[tokio::test]
async fn ctrl_a_down_splits_into_two_panes_with_a_divider() {
    let dir = tempfile::tempdir().unwrap();
    let sock = dir.path().join("split.sock");
    spawn_daemon(sock.clone());
    wait_for_socket(&sock).await;
    let (mut r, mut w) = handshake(&sock, 40, 12).await;
    // Ctrl-a (0x01) then Down arrow (ESC [ B).
    send_input(&mut w, &[0x01, 0x1b, b'[', b'B']).await;
    assert!(
        until_frame(&mut r, 5, |f| f.contains('─') || f.contains('━')).await,
        "expected a horizontal divider after Ctrl-a Down"
    );
}

#[tokio::test]
async fn ctrl_a_c_creates_a_window_and_shows_the_tab_bar() {
    let dir = tempfile::tempdir().unwrap();
    let sock = dir.path().join("win.sock");
    spawn_daemon(sock.clone());
    wait_for_socket(&sock).await;
    let (mut r, mut w) = handshake(&sock, 40, 12).await;
    send_input(&mut w, &[0x01, b'c']).await; // Ctrl-a c => new window
    assert!(
        until_frame(&mut r, 5, |f| f.contains("[0:") && f.contains("[1*")).await,
        "expected a tab bar with window 0 (inactive) and window 1 (active)"
    );
}

#[tokio::test]
async fn ctrl_a_d_detaches_the_client() {
    let dir = tempfile::tempdir().unwrap();
    let sock = dir.path().join("detach.sock");
    spawn_daemon(sock.clone());
    wait_for_socket(&sock).await;
    let (mut r, mut w) = handshake(&sock, 40, 12).await;
    send_input(&mut w, &[0x01, b'd']).await; // Ctrl-a d => detach
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    let mut detached = false;
    while let Ok(Ok(Some(msg))) =
        tokio::time::timeout_at(deadline, read_msg::<_, ServerMsg>(&mut r)).await
    {
        if matches!(msg, ServerMsg::Detach) {
            detached = true;
            break;
        }
    }
    assert!(detached, "expected ServerMsg::Detach after Ctrl-a d");
}

#[tokio::test]
async fn clicking_a_pane_moves_focus_between_panes() {
    let dir = tempfile::tempdir().unwrap();
    let sock = dir.path().join("click.sock");
    spawn_daemon(sock.clone());
    wait_for_socket(&sock).await;
    // 41 cols: vertical split => left x0..=19 (cols 1..=20), divider col 21,
    // right x21..=40 (cols 22..=41). New right pane is focused after the split.
    let (mut r, mut w) = handshake(&sock, 41, 12).await;
    send_input(&mut w, &[0x01, 0x1b, b'[', b'C']).await; // Ctrl-a Right => split vertical
                                                         // Cursor should land in the right (focused) pane: col >= 22.
    assert!(
        until_frame(&mut r, 5, |f| last_cursor_col(f)
            .map(|c| c >= 22)
            .unwrap_or(false))
        .await,
        "after split the focused (right) pane's cursor should be at col >= 22"
    );
    // Click the left pane at col 1, row 1 (SGR press).
    send_input(&mut w, b"\x1b[<0;1;1M").await;
    assert!(
        until_frame(&mut r, 5, |f| last_cursor_col(f)
            .map(|c| c <= 20)
            .unwrap_or(false))
        .await,
        "clicking the left pane should move the cursor into the left region (col <= 20)"
    );
}

#[tokio::test]
async fn ctrl_a_x_closing_the_last_pane_shuts_the_session_down() {
    let dir = tempfile::tempdir().unwrap();
    let sock = dir.path().join("close.sock");
    spawn_daemon(sock.clone());
    wait_for_socket(&sock).await;
    let (mut r, mut w) = handshake(&sock, 40, 12).await;
    send_input(&mut w, &[0x01, b'x']).await; // Ctrl-a x => close the only pane
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    let mut closed = false;
    while let Ok(Ok(Some(msg))) =
        tokio::time::timeout_at(deadline, read_msg::<_, ServerMsg>(&mut r)).await
    {
        if matches!(msg, ServerMsg::Closed { .. }) {
            closed = true;
            break;
        }
    }
    assert!(
        closed,
        "closing the last pane should end the session (ServerMsg::Closed)"
    );
}
