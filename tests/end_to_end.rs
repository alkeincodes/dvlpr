use std::time::Duration;

use dvlpr::protocol::{read_msg, write_msg, ClientHello, ServerHello, ServerMsg, PROTOCOL_VERSION};
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
            cols: 40,
            rows: 10,
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
