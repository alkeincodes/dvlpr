//! Integration tests for the single-active-writer (interaction-driven foreground) model.
//! The pane command reprints `stty size` on every SIGWINCH, so the session geometry is
//! observable as a "rows cols" string in the frame stream without sending input that
//! would itself promote a client.

use std::time::Duration;

use dvlpr::protocol::{
    read_msg, write_msg, ClientHello, ClientMsg, Intent, ServerHello, ServerMsg, PROTOCOL_VERSION,
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
            intent: Intent::Attach { cols, rows },
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

/// Read frames until a FULL frame (`full: true`) arrives; returns its bytes.
/// Skips diff frames. Per-client frames arrive in order, so callers consume the seed
/// full frame first, then assert on the next full frame produced by a geometry change.
async fn next_full_frame(r: &mut Reader, secs: u64) -> Option<Vec<u8>> {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(secs);
    while let Ok(Ok(Some(msg))) =
        tokio::time::timeout_at(deadline, read_msg::<_, ServerMsg>(r)).await
    {
        if let ServerMsg::Frame { data, full } = msg {
            if full {
                return Some(data);
            }
        }
    }
    None
}

/// Read full frames until one has exactly `cols x rows` content geometry; true if seen.
async fn until_full_frame_dims(r: &mut Reader, secs: u64, cols: u16, rows: u16) -> bool {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(secs);
    while let Ok(Ok(Some(msg))) =
        tokio::time::timeout_at(deadline, read_msg::<_, ServerMsg>(r)).await
    {
        if let ServerMsg::Frame { data, full } = msg {
            if full {
                let body = full_frame_rows(&data);
                if body.len() == rows as usize
                    && body
                        .iter()
                        .all(|line| line.chars().count() == cols as usize)
                {
                    return true;
                }
            }
        }
    }
    false
}

/// Split a FULL frame (`\x1b[2J\x1b[H` + rows joined by `\r\n` + a trailing cursor CUP)
/// into its content rows. `serialize_full` emits every cell, so each row is exactly the
/// grid's width and the row count equals the grid's height. The composed grid carries no
/// SGR, so the only remaining ESC after stripping the leading clear+home is the trailing
/// CUP, which we cut off.
fn full_frame_rows(data: &[u8]) -> Vec<String> {
    let s = String::from_utf8_lossy(data);
    let body = s.strip_prefix("\x1b[2J\x1b[H").unwrap_or(&s);
    let body = match body.rfind("\x1b[") {
        Some(i) => &body[..i],
        None => body,
    };
    body.split("\r\n").map(|r| r.to_string()).collect()
}

#[tokio::test]
async fn bigger_observer_receives_letterboxed_fitted_frame() {
    let dir = tempfile::tempdir().unwrap();
    let sock = dir.path().join("saw7.sock");
    spawn_daemon(sock.clone());
    wait_for_socket(&sock).await;
    let (mut ra, _wa) = handshake(&sock, 100, 30).await;
    let _seed = next_full_frame(&mut ra, 5)
        .await
        .expect("A seed full frame");

    let (_rb, _wb) = handshake(&sock, 60, 18).await;
    let f = next_full_frame(&mut ra, 5)
        .await
        .expect("A re-fit full frame after geometry change");
    let rows = full_frame_rows(&f);
    assert_eq!(
        rows.len(),
        30,
        "fitted frame must have the observer's 30 rows, not the foreground's 18"
    );
    assert!(
        rows.iter().all(|r| r.chars().count() == 100),
        "each row must be the observer's 100 cols"
    );
    assert!(
        rows[0].trim().is_empty(),
        "a bigger observer must have a blank letterbox top margin"
    );
    let seen =
        String::from_utf8_lossy(&f).contains("18 60") || until_text(&mut ra, 5, "18 60").await;
    assert!(
        seen,
        "a bigger observer must mirror the foreground's content, not paint blank"
    );
}

#[tokio::test]
async fn smaller_observer_receives_clipped_fitted_frame() {
    let dir = tempfile::tempdir().unwrap();
    let sock = dir.path().join("saw8.sock");
    spawn_daemon(sock.clone());
    wait_for_socket(&sock).await;
    let (mut ra, _wa) = handshake(&sock, 40, 12).await;
    let _seed = next_full_frame(&mut ra, 5)
        .await
        .expect("A seed full frame");

    let (_rb, _wb) = handshake(&sock, 100, 30).await;
    let f = next_full_frame(&mut ra, 5)
        .await
        .expect("A re-fit full frame after geometry change");
    let rows = full_frame_rows(&f);
    assert_eq!(
        rows.len(),
        12,
        "clipped frame must have the observer's 12 rows, not the foreground's 30"
    );
    assert!(
        rows.iter().all(|r| r.chars().count() == 40),
        "each row must be the observer's 40 cols"
    );
    let seen =
        String::from_utf8_lossy(&f).contains("30 100") || until_text(&mut ra, 5, "30 100").await;
    assert!(
        seen,
        "a clipped observer must mirror the foreground's top-left content, not paint blank"
    );
}

#[tokio::test]
async fn observer_resize_repaints_fitted_to_new_view() {
    let dir = tempfile::tempdir().unwrap();
    let sock = dir.path().join("saw9.sock");
    spawn_daemon(sock.clone());
    wait_for_socket(&sock).await;
    let (mut ra, mut wa) = handshake(&sock, 100, 30).await;
    let (_rb, _wb) = handshake(&sock, 60, 18).await;
    assert!(
        until_text(&mut ra, 5, "18 60").await,
        "B is foreground; A is an observer"
    );

    write_msg(
        &mut wa,
        &ClientMsg::Resize {
            cols: 120,
            rows: 40,
        },
    )
    .await
    .unwrap();
    assert!(
        until_full_frame_dims(&mut ra, 5, 120, 40).await,
        "a resized observer must get an immediate full frame fitted to its new 120x40 view"
    );
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
