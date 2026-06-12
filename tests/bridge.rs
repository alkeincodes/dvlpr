//! End-to-end: real daemon + bridge core over in-memory stdio.
//! Spec: docs/superpowers/specs/2026-06-13-remote-bridge-design.md §2.

use std::time::Duration;

use dvlpr::server::{run, socket, ServerConfig};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

fn spawn_session_with_command(
    dir: std::path::PathBuf,
    name: &str,
    command: Vec<String>,
) -> std::path::PathBuf {
    let path = socket::socket_path_in(&dir, name);
    let config = ServerConfig {
        socket_path: path.clone(),
        command,
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

/// Read NDJSON lines until one satisfies `pred` (or time out after 10 s).
async fn read_until(
    reader: &mut tokio::io::Lines<BufReader<tokio::io::DuplexStream>>,
    pred: impl Fn(&serde_json::Value) -> bool,
) -> serde_json::Value {
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let line = reader.next_line().await.unwrap().expect("bridge closed");
            let v: serde_json::Value = serde_json::from_str(&line).unwrap();
            if pred(&v) {
                return v;
            }
        }
    })
    .await
    .expect("timed out waiting for bridge event")
}

#[tokio::test]
async fn bridge_end_to_end_roster_commands_and_epoch() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().to_path_buf();
    let out_file = tmp.path().join("pane-input.txt");
    // The pane runs `cat > out_file`: whatever PaneSend types arrives there.
    let path = spawn_session_with_command(
        dir.clone(),
        "e2e",
        vec![
            "sh".into(),
            "-c".into(),
            format!("cat > {}", out_file.display()),
        ],
    );
    wait_for_socket(&path).await;

    // Bridge over in-memory stdio.
    let (cmd_in, bridge_stdin) = tokio::io::duplex(64 * 1024);
    let (bridge_stdout, ev_out) = tokio::io::duplex(64 * 1024);
    let bridge = tokio::spawn(dvlpr::bridge::run(
        dir,
        BufReader::new(bridge_stdin),
        bridge_stdout,
        None,
    ));
    let mut events = BufReader::new(ev_out).lines();
    let mut cmds = cmd_in;

    // 1. hello first.
    let hello = read_until(&mut events, |v| v["event"] == "hello").await;
    assert_eq!(hello["bridge_protocol"], 1);

    // 2. roster snapshot for the session (empty: `cat` is no agent), with epoch.
    let agents = read_until(&mut events, |v| v["event"] == "agents").await;
    assert_eq!(agents["session"], "e2e");
    let epoch = agents["epoch"].as_str().unwrap().to_string();
    assert!(!epoch.is_empty());

    // 3. send with the LIVE epoch types into the pane's PTY (pane id 1 = first pane).
    cmds.write_all(
        format!(
            "{}\n",
            serde_json::json!({
                "id": "s1", "cmd": "send", "key": "e2e/1",
                "epoch": epoch, "text": "hello-bridge", "submit": true
            })
        )
        .as_bytes(),
    )
    .await
    .unwrap();
    let reply = read_until(&mut events, |v| v["event"] == "reply" && v["id"] == "s1").await;
    assert_eq!(reply["ok"], true);
    // The PTY delivered the bracketed paste to `cat`, which wrote it to the file.
    let mut seen = String::new();
    for _ in 0..100 {
        seen = std::fs::read_to_string(&out_file).unwrap_or_default();
        if seen.contains("hello-bridge") {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(
        seen.contains("hello-bridge"),
        "pane never received text: {seen:?}"
    );

    // 4. stale epoch is rejected by the DAEMON with the spec code.
    cmds.write_all(
        format!(
            "{}\n",
            serde_json::json!({
                "id": "s2", "cmd": "send", "key": "e2e/1",
                "epoch": "bogus", "text": "nope", "submit": false
            })
        )
        .as_bytes(),
    )
    .await
    .unwrap();
    let reply = read_until(&mut events, |v| v["event"] == "reply" && v["id"] == "s2").await;
    assert_eq!(reply["ok"], false);
    assert_eq!(reply["code"], "stale_target");

    // 4b. omitting the epoch entirely is rejected AT THE BRIDGE (never reaches
    // the daemon's None skip-path, which is reserved for the local CLI).
    cmds.write_all(
        format!(
            "{}\n",
            serde_json::json!({
                "id": "s3", "cmd": "send", "key": "e2e/1",
                "text": "nope", "submit": false
            })
        )
        .as_bytes(),
    )
    .await
    .unwrap();
    let reply = read_until(&mut events, |v| v["event"] == "reply" && v["id"] == "s3").await;
    assert_eq!(reply["ok"], false);
    assert_eq!(reply["code"], "missing_epoch");

    // 5. a window op round-trips (0-based JSON window).
    cmds.write_all(
        format!(
            "{}\n",
            serde_json::json!({
                "id": "w1", "cmd": "window_rename", "session": "e2e",
                "epoch": epoch, "window": 0, "name": "renamed"
            })
        )
        .as_bytes(),
    )
    .await
    .unwrap();
    let reply = read_until(&mut events, |v| v["event"] == "reply" && v["id"] == "w1").await;
    assert_eq!(reply["ok"], true);

    // 6. stdin EOF shuts the bridge down cleanly.
    drop(cmds);
    tokio::time::timeout(Duration::from_secs(5), bridge)
        .await
        .expect("bridge did not exit on stdin EOF")
        .unwrap()
        .unwrap();
}
