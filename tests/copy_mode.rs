// End-to-end integration tests for copy mode.
//
// Tests: enter via prefix `[`, status line, scroll surfaces history,
// `y` emits OSC 52 and exits, mouse drag highlights, and two-client
// foreground-only OSC 52 + mouse-capture isolation.
//
// The harness is self-contained: each test spawns its own daemon and uses a
// unique socket path so tests can run in parallel without port conflicts.

use std::time::Duration;

use dvlpr::protocol::{
    read_msg, write_msg, ClientHello, ClientMsg, Intent, ServerHello, ServerMsg, PROTOCOL_VERSION,
};
use dvlpr::server::{run, ServerConfig};

type Reader = tokio::net::unix::OwnedReadHalf;
type Writer = tokio::net::unix::OwnedWriteHalf;

// ---------------------------------------------------------------------------
// Harness helpers (modelled on multi_pane.rs / help_popup.rs)
// ---------------------------------------------------------------------------

fn temp_socket(test: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "dvlpr-cm-{}-{}",
        test,
        std::process::id()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    dir.join("default.sock")
}

/// Spawn a daemon whose initial pane runs a shell that emits 50 numbered lines
/// then `cat` (keeps the pane open and in copy-able history). Default keymap is
/// pinned so tests are hermetic.
fn spawn_daemon(socket_path: std::path::PathBuf) {
    let config = ServerConfig {
        socket_path,
        command: vec![
            "sh".into(),
            "-c".into(),
            "for i in $(seq 1 50); do echo line$i; done; cat".into(),
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
}

async fn wait_for_socket(path: &std::path::Path) {
    for _ in 0..100 {
        if path.exists() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("daemon socket never appeared at {:?}", path);
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

async fn send_input(w: &mut Writer, bytes: &[u8]) {
    write_msg(w, &ClientMsg::Input(bytes.to_vec()))
        .await
        .unwrap();
}

/// Read frames until `pred` holds on the raw bytes of any single frame;
/// returns `true` if `pred` succeeded within `secs`, `false` otherwise.
async fn until_frame_bytes<F>(r: &mut Reader, secs: u64, pred: F) -> bool
where
    F: Fn(&[u8]) -> bool,
{
    let deadline = tokio::time::Instant::now() + Duration::from_secs(secs);
    while let Ok(Ok(Some(msg))) =
        tokio::time::timeout_at(deadline, read_msg::<_, ServerMsg>(r)).await
    {
        if let ServerMsg::Frame { data, .. } = msg {
            if pred(&data) {
                return true;
            }
        }
    }
    false
}

/// Read frames until `pred` holds on the UTF-8 lossy text of any frame.
async fn until_frame<F: Fn(&str) -> bool>(r: &mut Reader, secs: u64, pred: F) -> bool {
    until_frame_bytes(r, secs, |b| pred(&String::from_utf8_lossy(b))).await
}

/// Accumulate raw bytes from all frames received within `secs`.
async fn collect_bytes(r: &mut Reader, secs: u64) -> Vec<u8> {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(secs);
    let mut out = Vec::new();
    while let Ok(Ok(Some(msg))) =
        tokio::time::timeout_at(deadline, read_msg::<_, ServerMsg>(r)).await
    {
        match msg {
            ServerMsg::Frame { data, .. } => out.extend_from_slice(&data),
            ServerMsg::Detach | ServerMsg::Closed { .. } => break,
        }
    }
    out
}

/// Keep accumulating bytes from all frames until `needle` appears in the
/// running total or `secs` elapses. Returns `(true, accumulated_bytes)` if
/// found, `(false, accumulated_bytes)` otherwise.
async fn collect_bytes_until(r: &mut Reader, secs: u64, needle: &[u8]) -> (bool, Vec<u8>) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(secs);
    let mut out: Vec<u8> = Vec::new();
    while let Ok(Ok(Some(msg))) =
        tokio::time::timeout_at(deadline, read_msg::<_, ServerMsg>(r)).await
    {
        match msg {
            ServerMsg::Frame { data, .. } => {
                out.extend_from_slice(&data);
                if out.windows(needle.len()).any(|w| w == needle) {
                    return (true, out);
                }
            }
            ServerMsg::Detach | ServerMsg::Closed { .. } => break,
        }
    }
    (false, out)
}

// ---------------------------------------------------------------------------
// Test 1: entering copy mode shows the status indicator in a frame
// ---------------------------------------------------------------------------

#[tokio::test]
async fn prefix_bracket_enters_copy_mode_and_shows_status() {
    let sock = temp_socket("enter");
    spawn_daemon(sock.clone());
    wait_for_socket(&sock).await;

    let (mut r, mut w) = handshake(&sock, 40, 12).await;

    // Drain initial frames (shell output).
    let _ = collect_bytes(&mut r, 2).await;

    // prefix (Ctrl-B then '[') enters copy mode.
    send_input(&mut w, &[0x02, b'[']).await;

    assert!(
        until_frame(&mut r, 5, |f| f.contains("[copy]")).await,
        "expected [copy] status indicator after prefix ["
    );
}

// ---------------------------------------------------------------------------
// Test 2: scrolling surfaces lines that were above the live viewport
// ---------------------------------------------------------------------------

#[tokio::test]
async fn scroll_up_surfaces_history() {
    let sock = temp_socket("scroll");
    spawn_daemon(sock.clone());
    wait_for_socket(&sock).await;

    let (mut r, mut w) = handshake(&sock, 40, 12).await;

    // Wait for the shell script to produce output (line1..line50) and settle.
    // The pane has 12 rows, so ~38 lines are above the visible viewport.
    // We wait until "line50" appears in a frame to know the script has finished.
    assert!(
        until_frame(&mut r, 10, |f| f.contains("line50")).await,
        "shell script must have produced line50 before we test scrollback"
    );

    // Enter copy mode.
    send_input(&mut w, &[0x02, b'[']).await;
    assert!(
        until_frame(&mut r, 5, |f| f.contains("[copy]")).await,
        "copy mode must be entered"
    );

    // Drain any extra frames that arrive while copy mode settles.
    let _ = collect_bytes(&mut r, 1).await;

    // Scroll up far enough to surface historical content that was definitely
    // not on the live viewport.  The pane is 12 rows; with 50 lines output the
    // live bottom shows lines ~39-50.  Scrolling up 15 rows should bring lines
    // ~24-35 into view -- well into the "line1..=28" range below.
    //
    // We use the copy-mode PageUp key (Ctrl-B = 0x02, mapped to Motion::PageUp
    // in copy mode since the prefix is not armed there). Two PageUps = 2 half-
    // pages (each = rows/2 = 6) = 12 rows up, or we can use 'k' (up one row).
    // Use 15x 'k' to ensure we see historical content.
    for _ in 0..15 {
        send_input(&mut w, b"k").await;
    }

    // After scrolling up 15 rows from the live bottom (line39-50), the viewport
    // top is around line24. Lines 1..=28 were definitely in scrollback before
    // copy mode was entered. Assert that at least one such line appears.
    let scrolled_found = until_frame(&mut r, 5, |f| {
        (1..=28).any(|i| f.contains(&format!("line{}", i)))
    })
    .await;

    assert!(
        scrolled_found,
        "scrolling up in copy mode must surface lines 1..=28 which were in scrollback \
         (pane is 12 rows, 50 output lines, so live bottom showed lines 39-50)"
    );
}

// ---------------------------------------------------------------------------
// Test 3: yank emits OSC 52 and exits copy mode
// ---------------------------------------------------------------------------

#[tokio::test]
async fn yank_emits_osc52_and_exits_copy_mode() {
    let sock = temp_socket("yank");
    spawn_daemon(sock.clone());
    wait_for_socket(&sock).await;

    let (mut r, mut w) = handshake(&sock, 40, 12).await;

    // Wait for the shell to settle.
    let _ = collect_bytes(&mut r, 2).await;

    // Enter copy mode.
    send_input(&mut w, &[0x02, b'[']).await;
    assert!(
        until_frame(&mut r, 5, |f| f.contains("[copy]")).await,
        "copy mode must be entered"
    );

    // Start selection with 'v', extend to end of line with '$', then yank with 'y'.
    send_input(&mut w, b"v").await;
    send_input(&mut w, b"$").await;
    send_input(&mut w, b"y").await;

    // The byte stream for this client must contain the OSC 52 prefix.
    // OSC 52 = ESC ] 52 ; c ;
    let osc52: &[u8] = &[0x1b, b']', b'5', b'2', b';', b'c', b';'];
    let (found, accumulated) = collect_bytes_until(&mut r, 5, osc52).await;
    assert!(
        found,
        "expected OSC 52 sequence in client stream after yank, \
         but accumulated {} bytes (first 200): {:?}",
        accumulated.len(),
        &accumulated[..accumulated.len().min(200)]
    );

    // Copy mode should be exited: subsequent frames must not contain [copy].
    let post_yank_bytes = collect_bytes(&mut r, 2).await;
    let post_yank_text = String::from_utf8_lossy(&post_yank_bytes);
    if !post_yank_bytes.is_empty() {
        assert!(
            !post_yank_text.contains("[copy]"),
            "copy mode must be exited after yank but [copy] still present: {:?}",
            &post_yank_text[..post_yank_text.len().min(300)]
        );
    }
}

// ---------------------------------------------------------------------------
// Test 4: mouse drag in copy mode produces SGR inverse highlight
// ---------------------------------------------------------------------------

#[tokio::test]
async fn mouse_drag_highlights_selection() {
    let sock = temp_socket("mouse");
    spawn_daemon(sock.clone());
    wait_for_socket(&sock).await;

    let (mut r, mut w) = handshake(&sock, 40, 12).await;

    // Drain initial output.
    let _ = collect_bytes(&mut r, 2).await;

    // Enter copy mode.
    send_input(&mut w, &[0x02, b'[']).await;
    assert!(
        until_frame(&mut r, 5, |f| f.contains("[copy]")).await,
        "copy mode must be entered"
    );

    // Drain ALL frames that arrive after copy-mode entry (including the initial
    // full frame which contains the status-bar inverse row at the pane's bottom).
    // After this drain, the client has received and consumed the status-bar
    // inverse.  Any `;7m` appearing in frames produced by the subsequent mouse
    // drag must come from a SELECTION HIGHLIGHT, not the status bar, because:
    //
    //   1. The status bar row is always inverse in copy mode.  Since it does not
    //      change during a horizontal drag on a content row (scroll_offset stays
    //      the same, status text stays the same), the diff renderer emits that
    //      row only in the initial compose — it will NOT appear again in any
    //      drag-triggered diff frame.
    //   2. The selection on a content row (row 2 in 1-based wire coords = row 1
    //      in 0-based pane coords, well above the bottom status row) IS new after
    //      the drag, so its row appears in the diff with an inverse SGR run.
    //
    // This is why the post-drain `;7m` check genuinely proves the selection
    // highlight was drawn, not merely that a copy-mode frame was produced.
    let _ = collect_bytes(&mut r, 1).await;

    // SGR mouse PRESS at (col 3, row 2): ESC [ < 0 ; 3 ; 2 M
    // Bytes: 0x1b 0x5b 0x3c 0x30 0x3b 0x33 0x3b 0x32 0x4d
    send_input(&mut w, &[0x1b, b'[', b'<', b'0', b';', b'3', b';', b'2', b'M']).await;
    // SGR mouse DRAG at (col 8, row 2): ESC [ < 32 ; 8 ; 2 M
    // Bytes: 0x1b 0x5b 0x3c 0x33 0x32 0x3b 0x38 0x3b 0x32 0x4d
    send_input(
        &mut w,
        &[0x1b, b'[', b'<', b'3', b'2', b';', b'8', b';', b'2', b'M'],
    )
    .await;

    // Collect frames produced by the drag.  Any `;7m` here is a SELECTION
    // inverse on a content row — it CANNOT be the status bar because:
    //   - the status bar row was already consumed in the drain above, and
    //   - the drag does not change the status text (scroll_offset is unchanged),
    //     so diff_rows will not re-emit that row.
    let inverse_marker: &[u8] = b";7m";
    let (found, accumulated) = collect_bytes_until(&mut r, 5, inverse_marker).await;

    assert!(
        found,
        "expected SGR inverse marker (;7m) in diff frames produced by the mouse drag, \
         proving a selection highlight was drawn on a CONTENT ROW (not just the \
         always-on status-bar inverse, which was already drained before the drag); \
         accumulated {} bytes (first 400): {:?}",
        accumulated.len(),
        &accumulated[..accumulated.len().min(400)]
    );
}

// ---------------------------------------------------------------------------
// Test 5: two-client foreground-only isolation
// ---------------------------------------------------------------------------

#[tokio::test]
async fn two_clients_only_foreground_gets_osc52_and_mouse_capture() {
    let sock = temp_socket("twoclient");
    spawn_daemon(sock.clone());
    wait_for_socket(&sock).await;

    // Attach A first; A becomes foreground.
    let (mut ar, mut aw) = handshake(&sock, 40, 12).await;
    // Attach B (passive).
    let (mut br, _bw) = handshake(&sock, 40, 12).await;

    // Drain initial frames for both clients (shell output).
    let _ = collect_bytes(&mut ar, 2).await;
    let _ = collect_bytes(&mut br, 1).await;

    // A enters copy mode (A is foreground). From this point forward we
    // accumulate ALL of A's bytes without intermediate drains, so that both
    // the ESC[?1003h (sent on copy-mode entry) and the OSC 52 (sent on yank)
    // end up in the same accumulated buffer.
    send_input(&mut aw, &[0x02, b'[']).await;

    // OSC 52 = ESC ] 52 ; c ;
    let osc52: &[u8] = &[0x1b, b']', b'5', b'2', b';', b'c', b';'];
    // Mouse capture enable = ESC [ ? 1 0 0 3 h
    let mouse_cap: &[u8] = &[0x1b, b'[', b'?', b'1', b'0', b'0', b'3', b'h'];

    // Wait until A's buffer contains [copy] (to confirm copy mode is active),
    // accumulating bytes from the start into a growing buffer.
    // We use collect_bytes_until on "[copy]" text; but to also catch ?1003h we
    // use a raw-bytes check after the fact.
    //
    // Strategy: use collect_bytes_until to wait for ?1003h in A's stream
    // (which is emitted when copy-mode becomes active on A's writer).
    let (a_got_mouse_cap_early, mut a_accum) =
        collect_bytes_until(&mut ar, 5, mouse_cap).await;

    // If we didn't see ?1003h yet (e.g. timing), we might still need to yank first.
    // Either way, proceed: send select + yank, then collect until OSC52.
    // Drain B briefly while A sends input.
    let _ = collect_bytes(&mut br, 1).await;

    // A yanks: select with 'v', extend to line-end '$', then 'y'.
    send_input(&mut aw, b"v").await;
    send_input(&mut aw, b"$").await;
    send_input(&mut aw, b"y").await;

    // Collect from A until we see OSC 52. Append to the already-accumulated bytes.
    let (a_has_osc52_new, a_extra) = collect_bytes_until(&mut ar, 5, osc52).await;
    a_accum.extend_from_slice(&a_extra);

    // Check OSC 52 in either the early buffer or the new buffer.
    let a_has_osc52 = a_accum.windows(osc52.len()).any(|w| w == osc52);
    assert!(
        a_has_osc52 || a_has_osc52_new,
        "client A must receive OSC 52 on yank; \
         accumulated {} bytes (first 300): {:?}",
        a_accum.len(),
        &a_accum[..a_accum.len().min(300)]
    );

    // A must have received mouse-capture enable (either in early phase or combined).
    let a_has_mouse_cap = a_got_mouse_cap_early
        || a_accum.windows(mouse_cap.len()).any(|w| w == mouse_cap);
    assert!(
        a_has_mouse_cap,
        "client A must receive ESC[?1003h (mouse capture enable) while it is the \
         foreground copy-mode client; A accumulated {} bytes (first 400): {:?}",
        a_accum.len(),
        &a_accum[..a_accum.len().min(400)]
    );

    // B: read a bounded window and assert neither OSC 52 nor mouse capture appear.
    // Collect a bit extra to ensure B had a chance to receive any errant bytes.
    let b_bytes = collect_bytes(&mut br, 3).await;
    let b_has_osc52 = b_bytes.windows(osc52.len()).any(|w| w == osc52);
    let b_has_mouse_cap = b_bytes.windows(mouse_cap.len()).any(|w| w == mouse_cap);
    assert!(
        !b_has_osc52,
        "client B must NOT receive OSC 52 bytes (foreground-only); \
         B accumulated {} bytes (first 400): {:?}",
        b_bytes.len(),
        &b_bytes[..b_bytes.len().min(400)]
    );
    assert!(
        !b_has_mouse_cap,
        "client B must NOT receive ESC[?1003h (mouse capture is foreground-only); \
         B accumulated {} bytes (first 400): {:?}",
        b_bytes.len(),
        &b_bytes[..b_bytes.len().min(400)]
    );
}
