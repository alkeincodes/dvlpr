//! Multi-client end-to-end pins for the pane right-click context menu.
//! See `docs/superpowers/specs/2026-05-29-pane-right-click-menu-design.md`.
//! Helpers are inlined here (matching tests/multi_pane.rs's pattern —
//! integration test crates each carry their own boilerplate).

use std::time::Duration;

use dvlpr::protocol::{
    read_msg, write_msg, ClientHello, ClientMsg, Intent, ServerHello, ServerMsg, PROTOCOL_VERSION,
};
use dvlpr::server::{run, ServerConfig};

type Reader = tokio::net::unix::OwnedReadHalf;
type Writer = tokio::net::unix::OwnedWriteHalf;

fn temp_socket(name: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("dvlpr-ctxmenu-{}-{}", name, std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    dir.join("default.sock")
}

fn spawn_daemon(socket_path: std::path::PathBuf) {
    let config = ServerConfig {
        socket_path,
        command: vec!["cat".into()],
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
    panic!("daemon socket never appeared at {path:?}");
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

async fn collect_frames(r: &mut Reader, secs: u64) -> Vec<Vec<u8>> {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(secs);
    let mut out = Vec::new();
    while let Ok(Ok(Some(msg))) =
        tokio::time::timeout_at(deadline, read_msg::<_, ServerMsg>(r)).await
    {
        match msg {
            ServerMsg::Frame { data, .. } => out.push(data),
            ServerMsg::Detach | ServerMsg::Closed { .. } => break,
        }
    }
    out
}

fn frames_contain(frames: &[Vec<u8>], needle: &[u8]) -> bool {
    frames
        .iter()
        .any(|f| f.windows(needle.len()).any(|w| w == needle))
}

/// Read frames until any frame contains `needle`; returns true if seen within `secs`.
async fn until_frame_contains(r: &mut Reader, secs: u64, needle: &[u8]) -> bool {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(secs);
    while let Ok(Ok(Some(msg))) =
        tokio::time::timeout_at(deadline, read_msg::<_, ServerMsg>(r)).await
    {
        match msg {
            ServerMsg::Frame { data, .. } => {
                if data.windows(needle.len()).any(|w| w == needle) {
                    return true;
                }
            }
            ServerMsg::Detach | ServerMsg::Closed { .. } => break,
        }
    }
    false
}

#[tokio::test]
async fn right_click_from_one_client_makes_menu_visible_on_all_clients() {
    let path = temp_socket("visibility");
    spawn_daemon(path.clone());
    wait_for_socket(&path).await;
    let (mut ar, mut aw) = handshake(&path, 80, 24).await;
    let (mut br, _bw) = handshake(&path, 80, 24).await;
    let _ = collect_frames(&mut ar, 1).await;
    let _ = collect_frames(&mut br, 1).await;

    send_input(&mut aw, b"\x1b[<2;10;5M").await;

    let frames_a = collect_frames(&mut ar, 2).await;
    let frames_b = collect_frames(&mut br, 2).await;
    let corner = "┌".as_bytes();
    assert!(
        frames_contain(&frames_a, corner),
        "client A did not see the menu corner glyph"
    );
    assert!(
        frames_contain(&frames_b, corner),
        "client B did not see the menu corner glyph"
    );
}

#[tokio::test]
async fn any_client_can_drive_the_menu() {
    let path = temp_socket("any-drive");
    spawn_daemon(path.clone());
    wait_for_socket(&path).await;
    let (mut ar, mut aw) = handshake(&path, 80, 24).await;
    let (mut br, mut bw) = handshake(&path, 80, 24).await;
    let _ = collect_frames(&mut ar, 1).await;
    let _ = collect_frames(&mut br, 1).await;

    // A opens the menu.
    send_input(&mut aw, b"\x1b[<2;10;5M").await;
    // Wait until B sees the menu corner — confirms the menu is live before B drives it.
    assert!(
        until_frame_contains(&mut br, 3, "┌".as_bytes()).await,
        "menu did not appear on client B before navigation"
    );

    // B navigates Down (to "Split Horizontally") then hits Enter.
    // Send as a single message to avoid any inter-message escape-timeout window.
    send_input(&mut bw, b"\x1b[B\r").await;

    // After the split the active pane borders the divider, making it heavy (━).
    // Accept either the light (─) or heavy (━) horizontal divider glyph.
    let deadline_split = tokio::time::Instant::now() + Duration::from_secs(5);
    let mut saw_split = false;
    while let Ok(Ok(Some(msg))) =
        tokio::time::timeout_at(deadline_split, read_msg::<_, ServerMsg>(&mut br)).await
    {
        if let ServerMsg::Frame { data, .. } = msg {
            let light = data.windows(3).any(|w| w == "\u{2500}".as_bytes()); // ─
            let heavy = data.windows(3).any(|w| w == "\u{2501}".as_bytes()); // ━
            if light || heavy {
                saw_split = true;
                break;
            }
        }
    }
    assert!(
        saw_split,
        "split did not happen — B saw no horizontal divider"
    );
    let _ = aw;
}

#[tokio::test]
async fn ten_oh_three_h_reaches_every_attached_client_when_menu_opens() {
    let path = temp_socket("1003h-all");
    spawn_daemon(path.clone());
    wait_for_socket(&path).await;
    let (mut ar, mut aw) = handshake(&path, 80, 24).await;
    let (mut br, _bw) = handshake(&path, 80, 24).await;
    let _ = collect_frames(&mut ar, 1).await;
    let _ = collect_frames(&mut br, 1).await;

    send_input(&mut aw, b"\x1b[<2;10;5M").await;

    let frames_a = collect_frames(&mut ar, 2).await;
    let frames_b = collect_frames(&mut br, 2).await;
    let needle = b"\x1b[?1003h";
    assert!(
        frames_contain(&frames_a, needle),
        "client A did not receive ?1003h"
    );
    assert!(
        frames_contain(&frames_b, needle),
        "client B did not receive ?1003h"
    );
}

#[tokio::test]
async fn ten_oh_three_l_reaches_every_attached_client_when_menu_closes() {
    let path = temp_socket("1003l-all");
    spawn_daemon(path.clone());
    wait_for_socket(&path).await;
    let (mut ar, mut aw) = handshake(&path, 80, 24).await;
    let (mut br, _bw) = handshake(&path, 80, 24).await;
    let _ = collect_frames(&mut ar, 1).await;
    let _ = collect_frames(&mut br, 1).await;

    send_input(&mut aw, b"\x1b[<2;10;5M").await;
    let _ = collect_frames(&mut ar, 1).await;
    let _ = collect_frames(&mut br, 1).await;
    send_input(&mut aw, b"\x1b").await;

    let frames_a = collect_frames(&mut ar, 2).await;
    let frames_b = collect_frames(&mut br, 2).await;
    let needle = b"\x1b[?1003l";
    assert!(
        frames_contain(&frames_a, needle),
        "client A did not receive ?1003l"
    );
    assert!(
        frames_contain(&frames_b, needle),
        "client B did not receive ?1003l"
    );
}

#[tokio::test]
async fn late_attaching_client_receives_1003h_in_first_full_frame_when_menu_already_open() {
    let path = temp_socket("late-attach");
    spawn_daemon(path.clone());
    wait_for_socket(&path).await;
    let (mut ar, mut aw) = handshake(&path, 80, 24).await;
    let _ = collect_frames(&mut ar, 1).await;
    send_input(&mut aw, b"\x1b[<2;10;5M").await;
    let _ = collect_frames(&mut ar, 1).await;

    let (mut br, _bw) = handshake(&path, 80, 24).await;
    let frames_b = collect_frames(&mut br, 2).await;
    let needle = b"\x1b[?1003h";
    assert!(
        frames_contain(&frames_b, needle),
        "late-attached client B did not receive ?1003h in its first full frame"
    );
}

#[tokio::test]
async fn right_click_from_b_while_menu_is_open_reanchors_to_b() {
    let path = temp_socket("reanchor");
    spawn_daemon(path.clone());
    wait_for_socket(&path).await;
    let (mut ar, mut aw) = handshake(&path, 80, 24).await;
    let (mut br, mut bw) = handshake(&path, 80, 24).await;
    let _ = collect_frames(&mut ar, 1).await;
    let _ = collect_frames(&mut br, 1).await;

    send_input(&mut aw, b"\x1b[<2;10;5M").await;
    let _ = collect_frames(&mut ar, 1).await;
    let _ = collect_frames(&mut br, 1).await;

    send_input(&mut bw, b"\x1b[<2;60;10M").await;
    // Generous collect window: under the full parallel `cargo test` run this
    // multi-client round-trip is slow; 2s flaked, 8s is robust.
    let frames_b = collect_frames(&mut br, 8).await;

    let corner = "┌".as_bytes();
    assert!(
        frames_contain(&frames_b, corner),
        "client B did not see a reanchored menu corner glyph"
    );
}

#[tokio::test]
async fn ten_oh_three_l_reaches_detaching_client_on_detach_even_if_menu_already_closed() {
    let path = temp_socket("detach-1003l");
    spawn_daemon(path.clone());
    wait_for_socket(&path).await;
    let (mut ar, mut aw) = handshake(&path, 80, 24).await;
    let _ = collect_frames(&mut ar, 1).await;

    send_input(&mut aw, b"\x02d").await; // Ctrl-b d = Detach

    let frames_a = collect_frames(&mut ar, 2).await;
    let needle = b"\x1b[?1003l";
    assert!(
        frames_contain(&frames_a, needle),
        "detaching client did not receive final ?1003l before Detach"
    );
}

#[tokio::test]
async fn ten_oh_three_l_reaches_client_before_closed_when_session_shuts_down() {
    let path = temp_socket("closed-1003l");
    spawn_daemon(path.clone());
    wait_for_socket(&path).await;
    let (mut ar, mut aw) = handshake(&path, 80, 24).await;
    let _ = collect_frames(&mut ar, 1).await;

    send_input(&mut aw, b"\x02x").await; // Ctrl-b x = ClosePane on last pane → shutdown

    let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
    let mut saw_1003l_frame = false;
    let mut saw_closed = false;
    while let Ok(Ok(Some(msg))) =
        tokio::time::timeout_at(deadline, read_msg::<_, ServerMsg>(&mut ar)).await
    {
        match msg {
            ServerMsg::Frame { data, .. } => {
                if data
                    .windows(b"\x1b[?1003l".len())
                    .any(|w| w == b"\x1b[?1003l")
                {
                    saw_1003l_frame = true;
                }
            }
            ServerMsg::Closed { .. } => {
                saw_closed = true;
                break;
            }
            ServerMsg::Detach => break,
        }
    }
    assert!(
        saw_1003l_frame,
        "no ?1003l Frame observed before session shutdown"
    );
    assert!(saw_closed, "ServerMsg::Closed never arrived");
}
