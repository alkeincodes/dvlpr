//! Integration tests for named multi-session: per-name daemons coexist, the management
//! protocol (Status/Kill) works without attaching, and Status has no attach side effects.

use std::time::Duration;

use dvlpr::protocol::{
    read_msg, write_msg, ClientHello, Intent, ServerHello, ServerMsg, StatusInfo, PROTOCOL_VERSION,
};
use dvlpr::server::{run, socket, ServerConfig};

type Reader = tokio::net::unix::OwnedReadHalf;
type Writer = tokio::net::unix::OwnedWriteHalf;

fn spawn_session(dir: std::path::PathBuf, name: &str) -> std::path::PathBuf {
    let path = socket::socket_path_in(&dir, name);
    let config = ServerConfig {
        socket_path: path.clone(),
        command: vec!["cat".into()],
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

async fn connect(path: &std::path::Path) -> (Reader, Writer) {
    let stream = tokio::net::UnixStream::connect(path).await.unwrap();
    let (r, w) = stream.into_split();
    (r, w)
}

async fn attach(path: &std::path::Path, cols: u16, rows: u16) -> (Reader, Writer) {
    let (mut r, mut w) = connect(path).await;
    write_msg(
        &mut w,
        &ClientHello {
            protocol_version: PROTOCOL_VERSION,
            intent: Intent::Attach { cols, rows },
        },
    )
    .await
    .unwrap();
    let hello: ServerHello = read_msg(&mut r).await.unwrap().unwrap();
    assert!(matches!(hello, ServerHello::Ok { .. }));
    (r, w)
}

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

#[tokio::test]
async fn distinct_named_sessions_coexist_with_correct_status() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().to_path_buf();
    let a = spawn_session(dir.clone(), "alpha");
    let b = spawn_session(dir.clone(), "beta");
    wait_for_socket(&a).await;
    wait_for_socket(&b).await;

    let sa = status(&a).await;
    let sb = status(&b).await;
    assert_eq!(sa.windows, 1, "alpha should have 1 window");
    assert_eq!(sa.clients, 0, "alpha should have 0 attached clients");
    assert_eq!(sb.windows, 1);
    assert_eq!(sb.clients, 0);
}

#[tokio::test]
async fn status_counts_attached_clients_without_disturbing_them() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().to_path_buf();
    let a = spawn_session(dir.clone(), "work");
    wait_for_socket(&a).await;

    let (mut r, _w) = attach(&a, 80, 24).await;
    let _first = next_frame(&mut r, 5).await.expect("a first frame");

    let s = status(&a).await;
    assert_eq!(s.clients, 1, "status should count the attached client");
    assert_eq!(s.windows, 1);

    assert!(
        next_frame(&mut r, 1).await.is_none(),
        "a Status query must not cause a frame to the attached client"
    );
}

#[tokio::test]
async fn kill_shuts_the_session_down() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().to_path_buf();
    let a = spawn_session(dir.clone(), "victim");
    wait_for_socket(&a).await;
    assert!(socket::is_live(&a).await);

    let (_r, mut w) = connect(&a).await;
    write_msg(
        &mut w,
        &ClientHello {
            protocol_version: PROTOCOL_VERSION,
            intent: Intent::Kill {
                keep_snapshot: false,
            },
        },
    )
    .await
    .unwrap();

    let mut down = false;
    for _ in 0..100 {
        if !socket::is_live(&a).await {
            down = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(down, "session must stop being live after Kill");
}

#[tokio::test]
async fn killing_one_session_leaves_another_running() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().to_path_buf();
    let a = spawn_session(dir.clone(), "one");
    let b = spawn_session(dir.clone(), "two");
    wait_for_socket(&a).await;
    wait_for_socket(&b).await;

    let (_r, mut w) = connect(&a).await;
    write_msg(
        &mut w,
        &ClientHello {
            protocol_version: PROTOCOL_VERSION,
            intent: Intent::Kill {
                keep_snapshot: false,
            },
        },
    )
    .await
    .unwrap();

    let mut a_down = false;
    for _ in 0..100 {
        if !socket::is_live(&a).await {
            a_down = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(a_down);
    assert!(
        socket::is_live(&b).await,
        "the other session must stay live"
    );
    assert_eq!(status(&b).await.windows, 1);
}
