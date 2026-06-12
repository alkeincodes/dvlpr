//! Subscribe push channel: hello + initial roster snapshot, cleanup on EOF.
use std::time::Duration;

use dvlpr::protocol::{
    read_msg, write_msg, ClientHello, Intent, ServerHello, ServerMsg, PROTOCOL_VERSION,
};
use dvlpr::server::{run, socket, ServerConfig};

fn spawn_session(dir: std::path::PathBuf, name: &str) -> std::path::PathBuf {
    let path = socket::socket_path_in(&dir, name);
    let config = ServerConfig {
        socket_path: path.clone(),
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

async fn wait_for_socket(path: &std::path::Path) {
    for _ in 0..100 {
        if path.exists() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("daemon socket never appeared");
}

/// Read one message with a 5 s timeout, so the TDD red step (server never
/// replies before Subscribe exists) FAILS FAST instead of hanging the suite.
async fn recv<T: serde::de::DeserializeOwned>(r: &mut tokio::net::unix::OwnedReadHalf) -> T {
    tokio::time::timeout(Duration::from_secs(5), read_msg::<_, T>(r))
        .await
        .expect("timed out waiting for server message")
        .unwrap()
        .expect("server closed before replying")
}

#[tokio::test]
async fn subscribe_receives_hello_then_initial_roster() {
    let tmp = tempfile::tempdir().unwrap();
    let path = spawn_session(tmp.path().to_path_buf(), "sub");
    wait_for_socket(&path).await;

    let stream = tokio::net::UnixStream::connect(&path).await.unwrap();
    let (mut r, mut w) = stream.into_split();
    write_msg(
        &mut w,
        &ClientHello {
            protocol_version: PROTOCOL_VERSION,
            intent: Intent::Subscribe,
        },
    )
    .await
    .unwrap();

    match recv::<ServerHello>(&mut r).await {
        ServerHello::Ok { protocol_version } => assert_eq!(protocol_version, PROTOCOL_VERSION),
        ServerHello::Reject { reason } => panic!("rejected: {reason}"),
    }
    match recv::<ServerMsg>(&mut r).await {
        ServerMsg::Agents { epoch, agents } => {
            assert!(!epoch.is_empty());
            assert!(agents.is_empty(), "sleep pane is not an agent");
        }
        other => panic!("expected Agents, got {other:?}"),
    }

    // A second subscriber gets its own snapshot (no interference).
    let stream2 = tokio::net::UnixStream::connect(&path).await.unwrap();
    let (mut r2, mut w2) = stream2.into_split();
    write_msg(
        &mut w2,
        &ClientHello {
            protocol_version: PROTOCOL_VERSION,
            intent: Intent::Subscribe,
        },
    )
    .await
    .unwrap();
    let _ = recv::<ServerHello>(&mut r2).await;
    assert!(matches!(
        recv::<ServerMsg>(&mut r2).await,
        ServerMsg::Agents { .. }
    ));
}
