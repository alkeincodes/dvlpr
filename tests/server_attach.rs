use std::time::Duration;

use dvlpr::protocol::{
    read_msg, write_msg, ClientHello, Intent, ServerHello, ServerMsg, PROTOCOL_VERSION,
};
use dvlpr::server::{run, ServerConfig};

#[tokio::test]
async fn client_handshakes_and_receives_a_frame_with_command_output() {
    let dir = tempfile::tempdir().unwrap();
    let socket_path = dir.path().join("test.sock");

    let config = ServerConfig {
        socket_path: socket_path.clone(),
        command: vec!["sh".into(), "-c".into(), "printf READY; sleep 5".into()],
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

    let stream = tokio::net::UnixStream::connect(&socket_path).await.unwrap();
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

    // Read frames until one contains READY (or time out).
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    let mut saw_ready = false;
    while let Ok(Ok(Some(msg))) =
        tokio::time::timeout_at(deadline, read_msg::<_, ServerMsg>(&mut r)).await
    {
        if let ServerMsg::Frame { data, .. } = msg {
            if String::from_utf8_lossy(&data).contains("READY") {
                saw_ready = true;
                break;
            }
        }
    }
    assert!(saw_ready, "expected a frame containing READY");
}

#[tokio::test]
async fn frame_preserves_truecolor_from_the_pane() {
    // End-to-end color proof: the pane emits a 24-bit fg SGR; it must survive the whole
    // pipeline (PTY env -> libghostty-vt cell style -> compositor StyledCell -> serializer
    // SGR -> wire frame) and reach the client as a truecolor sequence.
    let dir = tempfile::tempdir().unwrap();
    let socket_path = dir.path().join("color.sock");

    let config = ServerConfig {
        socket_path: socket_path.clone(),
        command: vec![
            "sh".into(),
            "-c".into(),
            // Bright red via 24-bit color, then a marker char, then idle.
            "printf '\\033[38;2;200;30;40mX\\033[0m'; sleep 5".into(),
        ],
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

    let stream = tokio::net::UnixStream::connect(&socket_path).await.unwrap();
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

    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    let mut saw_color = false;
    while let Ok(Ok(Some(msg))) =
        tokio::time::timeout_at(deadline, read_msg::<_, ServerMsg>(&mut r)).await
    {
        if let ServerMsg::Frame { data, .. } = msg {
            let s = String::from_utf8_lossy(&data);
            if s.contains("38;2;200;30;40") && s.contains('X') {
                saw_color = true;
                break;
            }
        }
    }
    assert!(
        saw_color,
        "expected a frame carrying the pane's 24-bit fg color as an SGR sequence"
    );
}
