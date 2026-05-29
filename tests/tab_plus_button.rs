//! End-to-end pin for the tab-bar "[+]" new-window button.
//! See `docs/superpowers/specs/2026-05-29-tab-bar-new-window-button-design.md`.

use std::time::Duration;

use dvlpr::protocol::{
    read_msg, write_msg, ClientHello, ClientMsg, Intent, ServerHello, ServerMsg, PROTOCOL_VERSION,
};
use dvlpr::server::{run, ServerConfig};

type Reader = tokio::net::unix::OwnedReadHalf;
type Writer = tokio::net::unix::OwnedWriteHalf;

fn temp_socket(name: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("dvlpr-plus-{}-{}", name, std::process::id()));
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
async fn clicking_plus_button_in_tab_bar_creates_window() {
    let cols = 80u16;
    let rows = 24u16;
    let path = temp_socket("create");
    spawn_daemon(path.clone());
    wait_for_socket(&path).await;
    let (mut r, mut w) = handshake(&path, cols, rows).await;

    // First window is named from the command ("cat"). Compute the button's
    // column with the same layout the daemon uses.
    let bar = dvlpr::layout::tab_bar_layout("default", &["cat".to_string()], 0, false, cols);
    let pb = bar.plus.expect("button present at 80 cols");

    // The initial frame should render the button.
    let initial = collect_frames(&mut r, 1).await;
    assert!(
        frames_contain(&initial, b"[+]"),
        "the tab bar should render the [+] button"
    );

    // SGR left-button press on the button, bottom row (1-based).
    let col = pb.x_start + 1;
    let click = format!("\x1b[<0;{};{}M", col, rows);
    send_input(&mut w, click.as_bytes()).await;

    // The new window (named "cat", index 1) becomes active, so its tab
    // renders as "2:cat*" — the active marker `*` proves it exists AND is
    // active. The active chip is painted in a single uniform style, so the
    // serializer emits the label as a contiguous byte run (no interleaved
    // per-cell SGR), making this substring assertion robust.
    assert!(
        until_frame_contains(&mut r, 2, b"2:cat*").await,
        "clicking [+] should create and activate a second window (a '2:cat*' tab)"
    );
}
