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
