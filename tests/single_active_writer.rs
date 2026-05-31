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

/// The `stty size` line ("rows cols") the pane PTY reports for a client whose
/// viewport is `cols`x`rows`. The single pane fills the content area: viewport
/// height minus the always-present status/tab bar row, and viewport width minus
/// the always-on AGENTS sidebar — which `compute_regions` suppresses (pane keeps
/// the full width) when the viewport is narrower than
/// `SIDEBAR_WIDTH_DEFAULT + SIDEBAR_MIN_CONTENT_COLS`.
///
/// Derived from the layout constants on purpose: the original hardcoded "rows
/// cols" strings assumed a full-width pane and silently rotted when the sidebar
/// became always-on, turning this whole suite red. Deriving keeps it correct if
/// the sidebar width ever changes again.
fn pane_size(cols: u16, rows: u16) -> String {
    use dvlpr::layout::{SIDEBAR_MIN_CONTENT_COLS, SIDEBAR_WIDTH_DEFAULT};
    let content_cols = if cols >= SIDEBAR_WIDTH_DEFAULT + SIDEBAR_MIN_CONTENT_COLS {
        cols - SIDEBAR_WIDTH_DEFAULT
    } else {
        cols
    };
    format!("{} {}", rows - 1, content_cols)
}

fn spawn_daemon(socket_path: std::path::PathBuf) {
    let config = ServerConfig {
        socket_path,
        command: vec!["sh".into(), "-c".into(), SIZE_REPORTER.into()],
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

/// Strip all ANSI CSI escape sequences from `s`, leaving only printable
/// characters. A CSI sequence is `ESC [` followed by any number of parameter
/// bytes (0x30-0x3F) and intermediate bytes (0x20-0x2F), terminated by a final
/// byte in the range 0x40-0x7E.
fn strip_csi(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = String::with_capacity(s.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == 0x1b && i + 1 < bytes.len() && bytes[i + 1] == b'[' {
            i += 2;
            while i < bytes.len() && !(0x40..=0x7E).contains(&bytes[i]) {
                i += 1;
            }
            i += 1; // consume final byte
        } else {
            let ch = s[i..].chars().next().unwrap();
            out.push(ch);
            i += ch.len_utf8();
        }
    }
    out
}

/// Split a serialized full-frame frame into its content rows (one `String` per row).
/// Strips ALL CSI/SGR escapes (clear+home, leading SGR reset, the themed status-bar
/// runs, and the trailing cursor CUP) so each row contains only printable
/// characters and the count of `chars()` per row equals the grid's column width.
fn full_frame_rows(data: &[u8]) -> Vec<String> {
    let s = String::from_utf8_lossy(data);
    let plain = strip_csi(&s);
    plain.split("\r\n").map(|r| r.to_string()).collect()
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
    // The pane PTY height is the content area (viewport rows - 1 for the status bar).
    // Foreground B is 60x18, so the pane content is 34x17 (60-26 sidebar, 18-1 status): stty reports "17 34".
    let seen = String::from_utf8_lossy(&f).contains(&pane_size(60, 18))
        || until_text(&mut ra, 5, &pane_size(60, 18)).await;
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
    // The pane PTY height is the content area (viewport rows - 1 for the status bar).
    // Foreground B is 100x30, so the pane content is 74x29 (100-26 sidebar, 30-1 status): stty reports "29 74".
    let seen = String::from_utf8_lossy(&f).contains(&pane_size(100, 30))
        || until_text(&mut ra, 5, &pane_size(100, 30)).await;
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
    // Pane PTY height = viewport rows - 1 (status bar). B=60x18 → content is 34x17 (rows-1 status, cols-26 sidebar).
    assert!(
        until_text(&mut ra, 5, &pane_size(60, 18)).await,
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
    // Pane PTY height = viewport rows - 1 (status bar). Client is 100x30 → content is 74x29 (rows-1 status, cols-26 sidebar).
    assert!(
        until_text(&mut r, 5, &pane_size(100, 30)).await,
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
    // Pane PTY height = viewport rows - 1 (status bar). 100x30 → content is 74x29 (rows-1 status, cols-26 sidebar).
    assert!(
        until_text(&mut ra, 5, &pane_size(100, 30)).await,
        "A drives 100x30"
    );

    let (_rb, _wb) = handshake(&sock, 60, 18).await;
    // B=60x18 → content is 34x17 (rows-1 status, cols-26 sidebar).
    assert!(
        until_text(&mut ra, 5, &pane_size(60, 18)).await,
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
    // B=60x18 → content is 34x17 (rows-1 status, cols-26 sidebar).
    assert!(
        until_text(&mut ra, 5, &pane_size(60, 18)).await,
        "B (60x18) is foreground"
    );

    drop(rb);
    drop(wb);
    // A=100x30 → content is 74x29 (rows-1 status, cols-26 sidebar).
    assert!(
        until_text(&mut ra, 5, &pane_size(100, 30)).await,
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
    // B=60x18 → content is 34x17 (rows-1 status, cols-26 sidebar).
    assert!(
        until_text(&mut ra, 5, &pane_size(60, 18)).await,
        "B (60x18) is foreground"
    );

    write_msg(&mut wb, &ClientMsg::Input(b"\x02d".to_vec()))
        .await
        .unwrap();
    // A=100x30 → content is 74x29 (rows-1 status, cols-26 sidebar).
    assert!(
        until_text(&mut ra, 5, &pane_size(100, 30)).await,
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
    // B=60x18 → content is 34x17 (rows-1 status, cols-26 sidebar).
    assert!(
        until_text(&mut ra, 5, &pane_size(60, 18)).await,
        "B (60x18) is foreground"
    );

    write_msg(&mut wa, &ClientMsg::Input(b"\x1b".to_vec()))
        .await
        .unwrap();
    // A=100x30 → content is 74x29 (rows-1 status, cols-26 sidebar).
    assert!(
        until_text(&mut ra, 5, &pane_size(100, 30)).await,
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
    // Pane PTY height = viewport rows - 1 (status bar). 100x30 → content is 74x29 (rows-1 status, cols-26 sidebar).
    assert!(
        until_text(&mut ra, 5, &pane_size(100, 30)).await,
        "A drives 100x30"
    );

    let (rb, wb) = handshake(&sock, 60, 18).await;
    // B=60x18 → content is 34x17 (rows-1 status, cols-26 sidebar).
    assert!(
        until_text(&mut ra, 5, &pane_size(60, 18)).await,
        "B is foreground"
    );
    drop(rb);
    drop(wb);

    // A=100x30 → content is 74x29 (rows-1 status, cols-26 sidebar).
    assert!(
        until_text(&mut ra, 5, &pane_size(100, 30)).await,
        "after a client drops, the surviving client keeps receiving frames"
    );
}
