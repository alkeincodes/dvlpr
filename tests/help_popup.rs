//! Multi-client end-to-end pins for the `prefix ?` help overlay.
//! See `docs/superpowers/specs/2026-05-29-help-popup-dialog-design.md`.
//! Helpers are inlined (integration test crates each carry their own boilerplate).

use std::time::Duration;

use dvlpr::config::{Config, KeySpec};
use dvlpr::protocol::{
    read_msg, write_msg, ClientHello, ClientMsg, Intent, ServerHello, ServerMsg, PROTOCOL_VERSION,
};
use dvlpr::server::{run, ServerConfig};

type Reader = tokio::net::unix::OwnedReadHalf;
type Writer = tokio::net::unix::OwnedWriteHalf;

fn temp_socket(name: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("dvlpr-help-{}-{}", name, std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    dir.join("default.sock")
}

fn spawn_daemon_with(socket_path: std::path::PathBuf, cfg: Config) {
    let config = ServerConfig {
        socket_path,
        command: vec!["cat".into()],
        cwd: ".".into(),
        keymap: Some(cfg),
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

fn spawn_daemon(socket_path: std::path::PathBuf) {
    spawn_daemon_with(socket_path, Config::default());
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

async fn until_frame_contains(r: &mut Reader, secs: u64, needle: &[u8]) -> bool {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(secs);
    while let Ok(Ok(Some(msg))) =
        tokio::time::timeout_at(deadline, read_msg::<_, ServerMsg>(r)).await
    {
        if let ServerMsg::Frame { data, .. } = msg {
            if data.windows(needle.len()).any(|w| w == needle) {
                return true;
            }
        } else {
            break;
        }
    }
    false
}

// `prefix ?` = Ctrl-b then '?'. Ctrl-b is byte 0x02.
const PREFIX_QUESTION: &[u8] = b"\x02?";

#[tokio::test]
async fn prefix_question_opens_help_visible_on_all_clients() {
    let path = temp_socket("open-all");
    spawn_daemon(path.clone());
    wait_for_socket(&path).await;
    let (mut ar, mut aw) = handshake(&path, 80, 24).await;
    let (mut br, _bw) = handshake(&path, 80, 24).await;
    let _ = collect_frames(&mut ar, 1).await;
    let _ = collect_frames(&mut br, 1).await;

    send_input(&mut aw, PREFIX_QUESTION).await;

    assert!(
        until_frame_contains(&mut ar, 3, "Keybindings".as_bytes()).await,
        "client A did not see the help overlay"
    );
    assert!(
        until_frame_contains(&mut br, 3, "Keybindings".as_bytes()).await,
        "client B did not see the help overlay"
    );
}

#[tokio::test]
async fn clicking_commands_tab_switches_content() {
    let path = temp_socket("tab-switch");
    spawn_daemon(path.clone());
    wait_for_socket(&path).await;
    let (mut ar, mut aw) = handshake(&path, 80, 24).await;
    let _ = collect_frames(&mut ar, 1).await;

    send_input(&mut aw, PREFIX_QUESTION).await;
    assert!(until_frame_contains(&mut ar, 3, "Keybindings".as_bytes()).await);

    // Click the Commands tab. Geometry on an 80x24 client (derived the same way
    // the renderer lays it out, and pinned precisely by the Task 7 unit test
    // `left_click_on_tab_switches_tab`):
    //   content_area = 80x23 (viewport minus the bottom bar row)
    //   active tab = Keybindings → 11 rows → box height = 11 + HELP_CHROME_ROWS(5) = 16
    //   box width = HELP_MAX_W(72), centered → rect.x = (80-72)/2 = 4, rect.y = (23-16)/2 = 3
    //   tab header row (0-based) = rect.y + 1 = 4  → 1-based row 5
    //   Keybindings chip = cols [6..=18] (0-based); gap 2; Commands chip = [21..=30]
    //   Commands chip center (0-based) ≈ 25 → 1-based col 26
    // SGR left-press: ESC [ < 0 ; col ; row M  (1-based col/row).
    let click = b"\x1b[<0;26;5M"; // col 26, row 5 — center of the Commands chip
    send_input(&mut aw, click).await;

    assert!(
        until_frame_contains(&mut ar, 3, "dvlpr ls".as_bytes()).await,
        "clicking the Commands tab did not switch to the CLI table"
    );
}

#[tokio::test]
async fn q_closes_help() {
    let path = temp_socket("close-q");
    spawn_daemon(path.clone());
    wait_for_socket(&path).await;
    let (mut ar, mut aw) = handshake(&path, 80, 24).await;
    let _ = collect_frames(&mut ar, 1).await;

    send_input(&mut aw, PREFIX_QUESTION).await;
    assert!(until_frame_contains(&mut ar, 3, "Keybindings".as_bytes()).await);
    let _ = collect_frames(&mut ar, 1).await;

    send_input(&mut aw, b"q").await;
    // After closing, a fresh frame should NOT contain the overlay title.
    let frames = collect_frames(&mut ar, 2).await;
    assert!(
        !frames_contain(&frames, " Help ".as_bytes()),
        "help overlay title still present after q"
    );
}

#[tokio::test]
async fn prefix_question_again_toggles_help_closed() {
    let path = temp_socket("toggle");
    spawn_daemon(path.clone());
    wait_for_socket(&path).await;
    let (mut ar, mut aw) = handshake(&path, 80, 24).await;
    let _ = collect_frames(&mut ar, 1).await;

    send_input(&mut aw, PREFIX_QUESTION).await;
    assert!(until_frame_contains(&mut ar, 3, "Keybindings".as_bytes()).await);
    let _ = collect_frames(&mut ar, 1).await;

    send_input(&mut aw, PREFIX_QUESTION).await;
    let frames = collect_frames(&mut ar, 2).await;
    assert!(
        !frames_contain(&frames, " Help ".as_bytes()),
        "prefix ? again did not toggle help closed"
    );
}

#[tokio::test]
async fn any_client_can_drive_help() {
    let path = temp_socket("any-drive");
    spawn_daemon(path.clone());
    wait_for_socket(&path).await;
    let (mut ar, mut aw) = handshake(&path, 80, 24).await;
    let (mut br, mut bw) = handshake(&path, 80, 24).await;
    let _ = collect_frames(&mut ar, 1).await;
    let _ = collect_frames(&mut br, 1).await;

    // A opens.
    send_input(&mut aw, PREFIX_QUESTION).await;
    assert!(until_frame_contains(&mut br, 3, "Keybindings".as_bytes()).await);

    // B switches to Commands with the Right arrow.
    send_input(&mut bw, b"\x1b[C").await;
    assert!(
        until_frame_contains(&mut ar, 3, "dvlpr ls".as_bytes()).await,
        "B's tab switch was not reflected on A"
    );
}

#[tokio::test]
async fn keybindings_tab_reflects_custom_prefix_end_to_end() {
    let path = temp_socket("custom-prefix");
    let cfg = Config {
        prefix: KeySpec::Ctrl('a'), // C-a
        ..Config::default()
    };
    spawn_daemon_with(path.clone(), cfg);
    wait_for_socket(&path).await;
    let (mut ar, mut aw) = handshake(&path, 80, 24).await;
    let _ = collect_frames(&mut ar, 1).await;

    // Open help with C-a ? (Ctrl-a is 0x01).
    send_input(&mut aw, b"\x01?").await;
    assert!(
        until_frame_contains(&mut ar, 3, "C-a".as_bytes()).await,
        "help did not render the custom C-a prefix in chords"
    );
}
