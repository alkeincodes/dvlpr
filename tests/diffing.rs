use std::time::Duration;

use dvlpr::protocol::{
    read_msg, write_msg, ClientHello, ClientMsg, ServerHello, ServerMsg, PROTOCOL_VERSION,
};
use dvlpr::server::{run, ServerConfig};

type Reader = tokio::net::unix::OwnedReadHalf;
type Writer = tokio::net::unix::OwnedWriteHalf;

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

/// Read the next `ServerMsg::Frame` payload (skips non-Frame messages), or None on timeout.
async fn next_frame(r: &mut Reader, secs: u64) -> Option<Vec<u8>> {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(secs);
    while let Ok(Ok(Some(msg))) =
        tokio::time::timeout_at(deadline, read_msg::<_, ServerMsg>(r)).await
    {
        if let ServerMsg::Frame { data, .. } = msg {
            return Some(data);
        }
    }
    None
}

/// Read frames until one satisfies `pred`; returns true if found within `secs`.
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

#[tokio::test]
async fn first_frame_is_full_then_keystroke_is_a_diff() {
    let dir = tempfile::tempdir().unwrap();
    let sock = dir.path().join("diff.sock");
    spawn_daemon(sock.clone());
    wait_for_socket(&sock).await;
    let (mut r, mut w) = handshake(&sock, 40, 12).await;

    // First frame is a full repaint.
    let first = next_frame(&mut r, 5).await.expect("a first frame");
    let first = String::from_utf8_lossy(&first);
    assert!(
        first.contains("\x1b[2J"),
        "first frame should be a full repaint (clear screen)"
    );

    // Send a printable char; `cat` echoes it, so the screen changes at the cursor.
    // The resulting frame must be an incremental diff: no clear-screen, but a CUP.
    write_msg(&mut w, &ClientMsg::Input(b"x".to_vec()))
        .await
        .unwrap();
    let got_diff = until_frame(&mut r, 5, |f| !f.contains("\x1b[2J") && f.contains("\x1b[")).await;
    assert!(
        got_diff,
        "after a keystroke the server should send a diff frame (no clear, has a CUP)"
    );
}

#[tokio::test]
async fn resize_produces_a_full_frame() {
    let dir = tempfile::tempdir().unwrap();
    let sock = dir.path().join("resize.sock");
    spawn_daemon(sock.clone());
    wait_for_socket(&sock).await;
    let (mut r, mut w) = handshake(&sock, 40, 12).await;

    // Consume the initial full frame.
    let _ = next_frame(&mut r, 5).await.expect("a first frame");

    // Resize: the next frame for this client must be a full repaint at the new size.
    write_msg(&mut w, &ClientMsg::Resize { cols: 50, rows: 14 })
        .await
        .unwrap();
    assert!(
        until_frame(&mut r, 5, |f| f.contains("\x1b[2J")).await,
        "a resize should produce a full repaint"
    );
}
