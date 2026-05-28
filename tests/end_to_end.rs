use std::time::Duration;

use dvlpr::protocol::{
    read_msg, write_msg, ClientHello, Intent, ServerHello, ServerMsg, PROTOCOL_VERSION,
};
use dvlpr::server::{run, ServerConfig};

async fn connect_and_handshake(
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
            intent: Intent::Attach { cols: 40, rows: 10 },
        },
    )
    .await
    .unwrap();
    let hello: ServerHello = read_msg(&mut r).await.unwrap().unwrap();
    assert!(matches!(hello, ServerHello::Ok { .. }));
    (r, w)
}

async fn read_frame_containing(
    r: &mut tokio::net::unix::OwnedReadHalf,
    needle: &str,
    secs: u64,
) -> bool {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(secs);
    while let Ok(Ok(Some(msg))) =
        tokio::time::timeout_at(deadline, read_msg::<_, ServerMsg>(r)).await
    {
        if let ServerMsg::Frame { data, .. } = msg {
            if String::from_utf8_lossy(&data).contains(needle) {
                return true;
            }
        }
    }
    false
}

#[tokio::test]
async fn pane_survives_detach_and_screen_is_restored_on_reattach() {
    let dir = tempfile::tempdir().unwrap();
    let socket_path = dir.path().join("e2e.sock");

    // A pane that prints MARKER and then idles, so its screen content is stable.
    let config = ServerConfig {
        socket_path: socket_path.clone(),
        command: vec!["sh".into(), "-c".into(), "printf MARKER; sleep 30".into()],
        cwd: ".".into(),
        keymap: Some(dvlpr::config::Config::default()),
        session: "default".into(),
    };
    std::thread::spawn(move || {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let _ = rt.block_on(run(config));
    });

    for _ in 0..50 {
        if socket_path.exists() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }

    // First attach: see MARKER, then detach by dropping the connection (the daemon
    // keeps the pane alive). Server-initiated `Ctrl-a d` detach is covered in
    // tests/multi_pane.rs (added in a later task).
    let (mut r1, w1) = connect_and_handshake(&socket_path).await;
    assert!(read_frame_containing(&mut r1, "MARKER", 5).await);
    drop(r1);
    drop(w1);

    // The pane kept running; reattach and the screen should still show MARKER
    // (the immediate full repaint on connect carries prior screen state).
    let (mut r2, _w2) = connect_and_handshake(&socket_path).await;
    assert!(
        read_frame_containing(&mut r2, "MARKER", 5).await,
        "reattached client should receive a frame with prior screen content"
    );
}

#[tokio::test]
async fn focus_in_bytes_are_not_forwarded_to_the_pane() {
    use dvlpr::protocol::ClientMsg;

    let dir = tempfile::tempdir().unwrap();
    let socket_path = dir.path().join("focus.sock");
    let pane_log = dir.path().join("pane-stdin.log");

    // Pane command: append everything that reaches our stdin to a file, then idle.
    // `cat >> file` reads from the PTY (its stdin) and writes to the file; nothing
    // we write via `session.input()` should bypass this. The file becomes the
    // ground truth for "what bytes the parser forwarded to the pane".
    let pane_log_str = pane_log.to_string_lossy().into_owned();
    let cmd = format!("cat >> {} ; sleep 30", shell_single_quote(&pane_log_str));
    let config = ServerConfig {
        socket_path: socket_path.clone(),
        command: vec!["sh".into(), "-c".into(), cmd],
        cwd: ".".into(),
        keymap: Some(dvlpr::config::Config::default()),
        session: "default".into(),
    };
    std::thread::spawn(move || {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let _ = rt.block_on(run(config));
    });

    // Wait for the socket to appear.
    for _ in 0..50 {
        if socket_path.exists() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }

    let (mut r, mut w) = connect_and_handshake(&socket_path).await;

    // Send a line-terminated printable byte first. The PTY is in cooked mode
    // by default, so `cat` won't see input until a newline (or EOF); a bare 'a'
    // would sit in the line-discipline buffer forever and the pre-size sanity
    // check below would fail despite the wire path being healthy. Sending "a\n"
    // flushes a single line through. This also proves the pane is alive end-to-end
    // before we test the no-leak property (otherwise the post-size assertion
    // could pass vacuously if the wire→pane path were broken from the start).
    write_msg(&mut w, &ClientMsg::Input(b"a\n".to_vec()))
        .await
        .unwrap();

    // Wait for at least one frame so we know the server has processed our input.
    let mut got_frame = false;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    while let Ok(Ok(Some(msg))) =
        tokio::time::timeout_at(deadline, read_msg::<_, ServerMsg>(&mut r)).await
    {
        if matches!(msg, ServerMsg::Frame { .. }) {
            got_frame = true;
            break;
        }
    }
    assert!(got_frame, "expected at least one frame after sending input");

    // Give the pane PTY a moment to flush 'a' to the file.
    tokio::time::sleep(Duration::from_millis(200)).await;
    let pre_size = std::fs::metadata(&pane_log).map(|m| m.len()).unwrap_or(0);
    assert!(
        pre_size >= 1,
        "sanity: pane should have received the 'a' byte before we test the no-leak property; \
         file size = {pre_size}"
    );

    // Now send raw FocusIn bytes inside a ClientMsg::Input. If the parser fix
    // works, the server consumes the sequence and emits InputEvent::FocusIn —
    // which produces NO pane write. If the parser is broken (the latent bug
    // this feature also fixes), the bytes are forwarded to the pane PTY and
    // appended to our log file.
    write_msg(&mut w, &ClientMsg::Input(b"\x1b[I".to_vec()))
        .await
        .unwrap();

    // Allow the server's input-pump + the pane's stdin reader plenty of time
    // to react. If the bytes were going to leak, 250ms is more than enough.
    tokio::time::sleep(Duration::from_millis(250)).await;
    let post_size = std::fs::metadata(&pane_log).map(|m| m.len()).unwrap_or(0);
    assert_eq!(
        post_size,
        pre_size,
        "FocusIn bytes must not be forwarded to the pane PTY: \
         pre-size {pre_size}, post-size {post_size} (delta = {} byte(s))",
        post_size.saturating_sub(pre_size)
    );

    drop(r);
    drop(w);
}

/// Wrap `s` in single quotes for safe inclusion in a shell command. Any embedded
/// single quote is escaped as `'\''` (close, escaped-quote, reopen). Sufficient
/// for tempdir paths.
fn shell_single_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}
