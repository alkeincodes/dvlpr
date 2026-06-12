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
    let dir = std::env::temp_dir().join(format!("dvlpr-cm-{}-{}", test, std::process::id()));
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
            ServerMsg::Agents { .. } => {}
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
            ServerMsg::Agents { .. } => {}
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
    send_input(
        &mut w,
        &[0x1b, b'[', b'<', b'0', b';', b'3', b';', b'2', b'M'],
    )
    .await;
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
    let (a_got_mouse_cap_early, mut a_accum) = collect_bytes_until(&mut ar, 5, mouse_cap).await;

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
    let a_has_mouse_cap =
        a_got_mouse_cap_early || a_accum.windows(mouse_cap.len()).any(|w| w == mouse_cap);
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

// ---------------------------------------------------------------------------
// Test 6: background client B typing during A's copy mode cannot hijack it
// ---------------------------------------------------------------------------
//
// Regression test for the multi-client copy-mode isolation bug: before the fix,
// any client that typed while copy mode was active would be promoted to foreground
// by `commit_input` and their keystrokes would be parsed as CopyKeys — so B could
// drive A's copy-mode cursor and receive the OSC 52 yank. This test verifies that:
//   (a) B typing `j j` does NOT advance A's copy-mode cursor.
//   (b) B receives NO OSC 52 yank bytes.
//   (c) B receives NO `?1003h` mouse-capture sequence.
//
// Implementation: A enters copy mode, moves the cursor up so we can track
// position. We record A's cursor position via the [copy] status line (which
// shows row:col). Then B types `j` (copy-mode down), waits, and we confirm A's
// status line row did NOT decrease (the cursor did not move). Then A yanks with
// `y` to confirm it still works (owns the session), and B should NOT receive OSC 52.
#[tokio::test]
async fn background_client_cannot_hijack_copy_mode() {
    let sock = temp_socket("isolation");
    spawn_daemon(sock.clone());
    wait_for_socket(&sock).await;

    // A connects first; B second. A is initially foreground.
    let (mut ar, mut aw) = handshake(&sock, 40, 14).await;
    let (mut br, mut bw) = handshake(&sock, 40, 14).await;

    // B connected last, so B is foreground at this point. Send a resize-only
    // no-op from A to make A active without affecting the session. We achieve
    // this by having A type nothing and drain initial frames.
    let _ = collect_bytes(&mut ar, 2).await;
    let _ = collect_bytes(&mut br, 1).await;

    // A enters copy mode. For A's input to be processed as foreground we need
    // A to be foreground — send A's prefix input which promotes A.
    send_input(&mut aw, &[0x02, b'[']).await;

    // Wait for A to confirm copy mode is active.
    assert!(
        until_frame(&mut ar, 5, |f| f.contains("[copy]")).await,
        "client A must enter copy mode"
    );

    // Drain B's frames up to this point.
    let _ = collect_bytes(&mut br, 1).await;

    // Move A's cursor UP a few rows so we have a known non-bottom position,
    // then record the status line state. We scroll up 3 rows so the cursor
    // is visibly not at the viewport bottom.
    for _ in 0..3 {
        send_input(&mut aw, b"k").await;
    }
    // Allow the frames to arrive and drain them; A's cursor is now 3 rows up.
    let _ = collect_bytes(&mut ar, 1).await;
    let _ = collect_bytes(&mut br, 1).await;

    // Now B types `j j` (copy-mode "down" key). Since B is NOT the owner, these
    // should be treated as plain pane input bytes (ASCII 'j'), NOT as copy keys
    // that would move A's cursor back down.
    send_input(&mut bw, b"j").await;
    send_input(&mut bw, b"j").await;

    // Give the server time to process B's input.
    let _ = collect_bytes(&mut ar, 1).await;
    let _ = collect_bytes(&mut br, 1).await;

    // A now yanks with `v $ y`. Only A gets the OSC 52; B must not.
    send_input(&mut aw, b"v").await;
    send_input(&mut aw, b"$").await;
    send_input(&mut aw, b"y").await;

    let osc52: &[u8] = &[0x1b, b']', b'5', b'2', b';', b'c', b';'];
    let mouse_cap: &[u8] = &[0x1b, b'[', b'?', b'1', b'0', b'0', b'3', b'h'];

    // A must receive OSC 52 (it is still the owner and yanked successfully).
    let (a_has_osc52, a_bytes) = collect_bytes_until(&mut ar, 5, osc52).await;
    assert!(
        a_has_osc52,
        "client A (copy-mode owner) must receive OSC 52 on yank; \
         A accumulated {} bytes (first 200): {:?}",
        a_bytes.len(),
        &a_bytes[..a_bytes.len().min(200)]
    );

    // B must NOT receive OSC 52 or mouse-capture enable at any point.
    // Collect a window after A's yank to catch any errant bytes to B.
    let b_bytes = collect_bytes(&mut br, 3).await;
    let b_has_osc52 = b_bytes.windows(osc52.len()).any(|w| w == osc52);
    let b_has_mouse_cap = b_bytes.windows(mouse_cap.len()).any(|w| w == mouse_cap);
    assert!(
        !b_has_osc52,
        "background client B must NOT receive OSC 52 (copy-mode isolation broken); \
         B accumulated {} bytes (first 400): {:?}",
        b_bytes.len(),
        &b_bytes[..b_bytes.len().min(400)]
    );
    assert!(
        !b_has_mouse_cap,
        "background client B must NOT receive ?1003h (mouse capture is owner-only); \
         B accumulated {} bytes (first 400): {:?}",
        b_bytes.len(),
        &b_bytes[..b_bytes.len().min(400)]
    );
}

// ---------------------------------------------------------------------------
// Test 7: non-owner MOUSE events do NOT alter the owner's copy selection
// ---------------------------------------------------------------------------
//
// Regression test for Bug A: before the fix, a non-owner's mouse reports fell
// through to Session::handle_mouse, which has a copy-mode guard that called
// handle_copy_mode_mouse and mutated the owner's selection. This test verifies:
//   (a) B sending mouse press + drag while A is in copy mode does NOT produce
//       any selection highlight (inverse SGR) exclusive to B's activity.
//   (b) B receives NO OSC 52 bytes (no accidental yank triggered by mouse).
//   (c) A's copy mode is not disrupted; A can still yank with its own input.
#[tokio::test]
async fn non_owner_mouse_does_not_drive_copy_mode() {
    let sock = temp_socket("mouse-isolation");
    spawn_daemon(sock.clone());
    wait_for_socket(&sock).await;

    // A connects first (becomes foreground). B is passive.
    let (mut ar, mut aw) = handshake(&sock, 40, 14).await;
    let (mut br, mut bw) = handshake(&sock, 40, 14).await;

    let _ = collect_bytes(&mut ar, 2).await;
    let _ = collect_bytes(&mut br, 1).await;

    // A enters copy mode (A is the owner).
    send_input(&mut aw, &[0x02, b'[']).await;
    assert!(
        until_frame(&mut ar, 5, |f| f.contains("[copy]")).await,
        "client A must enter copy mode"
    );

    // Drain both clients' frames so the initial copy-mode frame is consumed.
    let _ = collect_bytes(&mut ar, 1).await;
    let _ = collect_bytes(&mut br, 1).await;

    // B sends a mouse PRESS at (col 3, row 2): ESC [ < 0 ; 3 ; 2 M
    // then a mouse DRAG at (col 8, row 2): ESC [ < 32 ; 8 ; 2 M
    // These must be dropped server-side; they must NOT mutate A's selection
    // and must NOT cause B to receive OSC 52.
    send_input(
        &mut bw,
        &[0x1b, b'[', b'<', b'0', b';', b'3', b';', b'2', b'M'],
    )
    .await;
    send_input(
        &mut bw,
        &[0x1b, b'[', b'<', b'3', b'2', b';', b'8', b';', b'2', b'M'],
    )
    .await;

    let osc52: &[u8] = &[0x1b, b']', b'5', b'2', b';', b'c', b';'];

    // Collect B's output for a window; B must receive NO OSC 52.
    let b_bytes = collect_bytes(&mut br, 2).await;
    let b_has_osc52 = b_bytes.windows(osc52.len()).any(|w| w == osc52);
    assert!(
        !b_has_osc52,
        "non-owner B's mouse must NOT trigger OSC 52 (Bug A regression); \
         B accumulated {} bytes (first 400): {:?}",
        b_bytes.len(),
        &b_bytes[..b_bytes.len().min(400)]
    );

    // A should still own copy mode — verify A can yank successfully.
    send_input(&mut aw, b"v").await;
    send_input(&mut aw, b"$").await;
    send_input(&mut aw, b"y").await;

    let (a_has_osc52, a_bytes) = collect_bytes_until(&mut ar, 5, osc52).await;
    assert!(
        a_has_osc52,
        "client A must still be able to yank after B sent mouse events; \
         A accumulated {} bytes (first 200): {:?}",
        a_bytes.len(),
        &a_bytes[..a_bytes.len().min(200)]
    );

    // B must still not receive OSC 52 even after A's yank.
    let b_bytes2 = collect_bytes(&mut br, 2).await;
    let b_has_osc52_2 = b_bytes2.windows(osc52.len()).any(|w| w == osc52);
    assert!(
        !b_has_osc52_2,
        "background client B must NOT receive A's OSC 52 yank; \
         B accumulated {} bytes (first 400): {:?}",
        b_bytes2.len(),
        &b_bytes2[..b_bytes2.len().min(400)]
    );
}

// ---------------------------------------------------------------------------
// Test: left-drag with no prior `prefix [` auto-enters copy mode (mouse-copy)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn left_drag_auto_enters_copy_mode_and_yanks() {
    let sock = temp_socket("drag-enter");
    spawn_daemon(sock.clone());
    wait_for_socket(&sock).await;

    let (mut r, mut w) = handshake(&sock, 40, 12).await;

    // Let the shell settle (it emits line1..line50 then `cat`). The pane is NOT
    // mouse-tracking, so drag-to-enter is armed.
    assert!(
        until_frame(&mut r, 10, |f| f.contains("line50")).await,
        "shell must finish output before we drag"
    );
    let _ = collect_bytes(&mut r, 1).await;

    // SGR left PRESS at (col 3, row 2): ESC [ < 0 ; 3 ; 2 M
    send_input(&mut w, b"\x1b[<0;3;2M").await;
    // SGR left DRAG at (col 12, row 2): button 0 + motion bit (32) = 32.
    send_input(&mut w, b"\x1b[<32;12;2M").await;

    // The drag (not a prior `prefix [`) must have entered copy mode AND painted a
    // selection. Accumulate bytes into ONE buffer and check BOTH markers against it
    // — do NOT use two separate `until_frame` calls: the single frame that carries
    // both `[copy]` and the `;7m` inverse run would be consumed by the first call,
    // timing out the second. `collect_bytes` drains for the window and returns all
    // bytes seen.
    let accum = collect_bytes(&mut r, 5).await;
    let seen = String::from_utf8_lossy(&accum);
    assert!(
        seen.contains("[copy]"),
        "left-drag must auto-enter copy mode ([copy] status); got {seen:?}"
    );
    assert!(
        seen.contains(";7m"),
        "drag must paint an inverse selection run (;7m); got {seen:?}"
    );

    // Yank with 'y' → OSC 52, then copy mode exits.
    send_input(&mut w, b"y").await;
    let osc52: &[u8] = &[0x1b, b']', b'5', b'2', b';', b'c', b';'];
    let (found, _accum) = collect_bytes_until(&mut r, 5, osc52).await;
    assert!(found, "yank after drag-to-enter must emit OSC 52");
}

// ---------------------------------------------------------------------------
// Test: mouse-up after a drag copies on its own (tmux-style) — no `y` keypress
// ---------------------------------------------------------------------------

#[tokio::test]
async fn left_drag_release_copies_without_y_keypress() {
    let sock = temp_socket("drag-release-copy");
    spawn_daemon(sock.clone());
    wait_for_socket(&sock).await;

    let (mut r, mut w) = handshake(&sock, 40, 12).await;
    assert!(
        until_frame(&mut r, 10, |f| f.contains("line50")).await,
        "shell must finish output before we drag"
    );
    let _ = collect_bytes(&mut r, 1).await;

    // Press → drag (auto-enters copy mode + selects) → RELEASE (mouse-up).
    send_input(&mut w, b"\x1b[<0;3;2M").await; // press   (button 0, M)
    send_input(&mut w, b"\x1b[<32;12;2M").await; // drag  (button 0 + motion bit 32, M)
    send_input(&mut w, b"\x1b[<0;12;2m").await; // release (button 0, lowercase m)

    // The mouse-up ALONE must emit OSC 52 — no `y` was sent. This is the reported
    // gesture: drag highlights, mouse-up copies (and exits copy mode; exit is pinned
    // by the session unit tests `copy_mode_mouse_release_after_drag_yanks_and_exits`).
    let osc52: &[u8] = &[0x1b, b']', b'5', b'2', b';', b'c', b';'];
    let (found, _accum) = collect_bytes_until(&mut r, 5, osc52).await;
    assert!(
        found,
        "mouse-up after a drag must emit OSC 52 with no `y` keypress"
    );
}

// ---------------------------------------------------------------------------
// Test: a left click (Press + Release, no Drag) does NOT enter copy mode
// ---------------------------------------------------------------------------

#[tokio::test]
async fn click_does_not_enter_copy_mode() {
    let sock = temp_socket("click-no-enter");
    spawn_daemon(sock.clone());
    wait_for_socket(&sock).await;

    let (mut r, mut w) = handshake(&sock, 40, 12).await;

    // Let the shell settle.
    assert!(
        until_frame(&mut r, 10, |f| f.contains("line50")).await,
        "shell must finish output before we click"
    );
    let _ = collect_bytes(&mut r, 1).await;

    // SGR left PRESS at (col 3, row 2): ESC [ < 0 ; 3 ; 2 M
    send_input(&mut w, b"\x1b[<0;3;2M").await;
    // SGR left RELEASE at the same position: ESC [ < 0 ; 3 ; 2 m  (lowercase m = release)
    send_input(&mut w, b"\x1b[<0;3;2m").await;

    // A bounded drain: no `[copy]` must appear; a click must NOT enter copy mode.
    let accum = collect_bytes(&mut r, 2).await;
    let seen = String::from_utf8_lossy(&accum);
    assert!(
        !seen.contains("[copy]"),
        "a click (Press+Release, no Drag) must NOT enter copy mode; got {seen:?}"
    );
}

// ---------------------------------------------------------------------------
// Test: a left-drag on a divider does NOT enter copy mode (resizes instead)
// ---------------------------------------------------------------------------
//
// Create a vertical split (prefix + ArrowRight = the default split-vertical binding),
// then send a Press at the divider column (col 20 in a 40-col viewport with ratio 0.5)
// followed by a Drag. Copy mode must NOT be entered — the drag resizes the pane.
// Column arithmetic: avail = 40-1 = 39, first_w = floor(0.5*39) = 19;
// divider occupies x=19 (0-based) = col 20 (1-based SGR wire).

#[tokio::test]
async fn divider_drag_does_not_enter_copy_mode() {
    let sock = temp_socket("divider-no-copy");
    spawn_daemon(sock.clone());
    wait_for_socket(&sock).await;

    let (mut r, mut w) = handshake(&sock, 40, 12).await;

    // Let the shell settle before doing the split.
    assert!(
        until_frame(&mut r, 10, |f| f.contains("line50")).await,
        "shell must finish output before we create a split"
    );
    let _ = collect_bytes(&mut r, 1).await;

    // Create a vertical split: prefix (Ctrl-B = 0x02) + ArrowRight (ESC [ C).
    // The default `split-vertical` binding maps prefix + Right to SplitVertical.
    send_input(&mut w, &[0x02]).await;
    send_input(&mut w, b"\x1b[C").await;

    // Wait for the new pane to appear (the layout changes, a new frame arrives).
    // We just wait for any frame to confirm the split happened; the divider is
    // now at col 20.
    let _ = collect_bytes(&mut r, 2).await;

    // SGR left PRESS at (col 20, row 2): the divider column.
    send_input(&mut w, b"\x1b[<0;20;2M").await;
    // SGR left DRAG to (col 25, row 2): moves the divider right.
    send_input(&mut w, b"\x1b[<32;25;2M").await;

    // Drain the frames produced by the divider drag. The `[copy]` status must
    // NOT appear — the drag resized the pane, not entered copy mode.
    let accum = collect_bytes(&mut r, 3).await;
    let seen = String::from_utf8_lossy(&accum);
    assert!(
        !seen.contains("[copy]"),
        "a divider drag must NOT enter copy mode; got {seen:?}"
    );
}

// ---------------------------------------------------------------------------
// Test: modeless mouse-wheel scroll (no copy mode entered)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn mouse_wheel_scrolls_history_and_returns_to_live() {
    let sock = temp_socket("wheel");
    spawn_daemon(sock.clone());
    wait_for_socket(&sock).await;

    let (mut r, mut w) = handshake(&sock, 40, 12).await;
    assert!(
        until_frame(&mut r, 10, |f| f.contains("line50")).await,
        "shell script must have produced line50 first"
    );
    let _ = collect_bytes(&mut r, 1).await;

    // Wheel up many notches over the pane (col 5, row 5): ESC [ < 64 ; 5 ; 5 M.
    // No copy mode is entered (modeless); history must surface in the frame.
    for _ in 0..8 {
        send_input(&mut w, b"\x1b[<64;5;5M").await;
    }
    let surfaced = until_frame(&mut r, 5, |f| {
        (1..=28).any(|i| f.contains(&format!("line{}", i)))
    })
    .await;
    assert!(
        surfaced,
        "wheel-up must surface scrollback history without copy mode"
    );

    // Wheel down past the bottom returns to live (line50 visible again).
    for _ in 0..12 {
        send_input(&mut w, b"\x1b[<65;5;5M").await;
    }
    assert!(
        until_frame(&mut r, 5, |f| f.contains("line50")).await,
        "wheel-down must return to the live bottom"
    );
}

// ---------------------------------------------------------------------------
// Test 8: owner disconnect exits copy mode; remaining client can re-enter
// ---------------------------------------------------------------------------
//
// Regression test for Bug B: before the fix, if the owner client disconnected
// while copy mode was active, copy_mode_owner stayed a dead ClientId and the
// Session remained in copy mode forever — no live client could exit it.
// This test verifies:
//   (a) After A (the copy-mode owner) disconnects, the session exits copy mode.
//   (b) A remaining client C can subsequently re-enter copy mode (prefix [
//       previously no-ops when copy mode is already active).
#[tokio::test]
async fn owner_disconnect_exits_copy_mode() {
    let sock = temp_socket("owner-disconnect");
    spawn_daemon(sock.clone());
    wait_for_socket(&sock).await;

    // A connects first (becomes foreground/owner).
    let (mut ar, mut aw) = handshake(&sock, 40, 14).await;
    // C is a passive observer that will survive after A disconnects.
    let (mut cr, mut cw) = handshake(&sock, 40, 14).await;

    let _ = collect_bytes(&mut ar, 2).await;
    let _ = collect_bytes(&mut cr, 1).await;

    // A enters copy mode.
    send_input(&mut aw, &[0x02, b'[']).await;
    assert!(
        until_frame(&mut ar, 5, |f| f.contains("[copy]")).await,
        "client A must enter copy mode"
    );
    // Drain C's frames to consume the copy-mode status update.
    let _ = collect_bytes(&mut cr, 2).await;

    // A disconnects (drop the writer; the server will receive ClientGone).
    // The read half is also dropped here; the server detects EOF.
    drop(aw);
    drop(ar);

    // Give the server time to process A's disconnect and exit copy mode.
    // C should subsequently see frames WITHOUT [copy] in the status.
    let copy_cleared = until_frame(&mut cr, 5, |f| !f.contains("[copy]")).await;
    assert!(
        copy_cleared,
        "after copy-mode owner disconnects, the session must exit copy mode \
         ([copy] must no longer appear in C's frames)"
    );

    // C should now be able to re-enter copy mode (prefix [). This would be a
    // no-op if copy mode were still stuck active, proving the fix worked.
    // Drain any pending frames first.
    let _ = collect_bytes(&mut cr, 1).await;
    send_input(&mut cw, &[0x02, b'[']).await;
    assert!(
        until_frame(&mut cr, 5, |f| f.contains("[copy]")).await,
        "remaining client C must be able to re-enter copy mode after A's disconnect \
         cleared it (would be a no-op if copy mode were still stuck)"
    );
}
