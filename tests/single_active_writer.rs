//! Integration tests for the single-active-writer (interaction-driven foreground) model.
//! The pane command reprints `stty size` on every SIGWINCH, so the session geometry is
//! observable as a "rows cols" string in the frame stream without sending input that
//! would itself promote a client.

use std::time::Duration;

use dvlpr::protocol::{
    read_msg, write_msg, ClientHello, ClientMsg, ServerHello, ServerMsg, PROTOCOL_VERSION,
};
use dvlpr::server::{run, ServerConfig};

type Reader = tokio::net::unix::OwnedReadHalf;
type Writer = tokio::net::unix::OwnedWriteHalf;

/// Prints the tty size once at startup and again on every window-size change (SIGWINCH),
/// then idles. Each session resize -> PTY resize -> SIGWINCH -> a fresh "rows cols" line.
const SIZE_REPORTER: &str = "trap 'stty size' WINCH; stty size; while :; do sleep 0.1; done";

fn spawn_daemon(socket_path: std::path::PathBuf) {
    let config = ServerConfig {
        socket_path,
        command: vec!["sh".into(), "-c".into(), SIZE_REPORTER.into()],
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

/// Read frames until one's painted text contains `needle`; returns true if seen in `secs`.
async fn until_text(r: &mut Reader, secs: u64, needle: &str) -> bool {
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
async fn first_client_drives_session_geometry() {
    let dir = tempfile::tempdir().unwrap();
    let sock = dir.path().join("saw1.sock");
    spawn_daemon(sock.clone());
    wait_for_socket(&sock).await;
    let (mut r, _w) = handshake(&sock, 100, 30).await;
    assert!(
        until_text(&mut r, 5, "30 100").await,
        "session geometry should track the (only) client's size"
    );
}

#[tokio::test]
async fn smaller_client_connecting_shrinks_geometry() {
    let dir = tempfile::tempdir().unwrap();
    let sock = dir.path().join("saw2.sock");
    spawn_daemon(sock.clone());
    wait_for_socket(&sock).await;
    let (mut ra, _wa) = handshake(&sock, 100, 30).await;
    assert!(until_text(&mut ra, 5, "30 100").await, "A drives 100x30");

    let (_rb, _wb) = handshake(&sock, 60, 18).await;
    assert!(
        until_text(&mut ra, 5, "18 60").await,
        "a newly-connected smaller client becomes foreground and shrinks geometry"
    );
}

#[tokio::test]
async fn foreground_disconnect_promotes_survivor() {
    let dir = tempfile::tempdir().unwrap();
    let sock = dir.path().join("saw3.sock");
    spawn_daemon(sock.clone());
    wait_for_socket(&sock).await;
    let (mut ra, _wa) = handshake(&sock, 100, 30).await;
    let (rb, wb) = handshake(&sock, 60, 18).await;
    assert!(
        until_text(&mut ra, 5, "18 60").await,
        "B (60x18) is foreground"
    );

    drop(rb);
    drop(wb);
    assert!(
        until_text(&mut ra, 5, "30 100").await,
        "foreground disconnect must promote the surviving client and resize to its size"
    );
}

#[tokio::test]
async fn foreground_detach_repromotes_survivor() {
    let dir = tempfile::tempdir().unwrap();
    let sock = dir.path().join("saw4.sock");
    spawn_daemon(sock.clone());
    wait_for_socket(&sock).await;
    let (mut ra, _wa) = handshake(&sock, 100, 30).await;
    let (_rb, mut wb) = handshake(&sock, 60, 18).await;
    assert!(
        until_text(&mut ra, 5, "18 60").await,
        "B (60x18) is foreground"
    );

    write_msg(&mut wb, &ClientMsg::Input(b"\x01d".to_vec()))
        .await
        .unwrap();
    assert!(
        until_text(&mut ra, 5, "30 100").await,
        "foreground Detach must re-promote the survivor (geometry must not go stale)"
    );
}

#[tokio::test]
async fn esc_timeout_input_promotes_sender() {
    let dir = tempfile::tempdir().unwrap();
    let sock = dir.path().join("saw5.sock");
    spawn_daemon(sock.clone());
    wait_for_socket(&sock).await;
    let (mut ra, mut wa) = handshake(&sock, 100, 30).await;
    let (_rb, _wb) = handshake(&sock, 60, 18).await;
    assert!(
        until_text(&mut ra, 5, "18 60").await,
        "B (60x18) is foreground"
    );

    write_msg(&mut wa, &ClientMsg::Input(b"\x1b".to_vec()))
        .await
        .unwrap();
    assert!(
        until_text(&mut ra, 5, "30 100").await,
        "an ESC committed on the tick-time timeout must promote its sender to foreground"
    );
}

#[tokio::test]
async fn dropping_a_client_keeps_the_session_responsive() {
    let dir = tempfile::tempdir().unwrap();
    let sock = dir.path().join("saw6.sock");
    spawn_daemon(sock.clone());
    wait_for_socket(&sock).await;
    let (mut ra, _wa) = handshake(&sock, 100, 30).await;
    assert!(until_text(&mut ra, 5, "30 100").await, "A drives 100x30");

    let (rb, wb) = handshake(&sock, 60, 18).await;
    assert!(until_text(&mut ra, 5, "18 60").await, "B is foreground");
    drop(rb);
    drop(wb);

    assert!(
        until_text(&mut ra, 5, "30 100").await,
        "after a client drops, the surviving client keeps receiving frames"
    );
}
