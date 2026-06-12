//! `dvlpr bridge`: NDJSON over stdio <-> bincode over daemon sockets.
//! One persistent process per host; Claire (or any consumer, or a human with
//! a pipe) drives it. See docs/superpowers/specs/2026-06-13-remote-bridge-design.md §2.

pub mod tail;
pub mod wire;

use std::collections::HashMap;
use std::io;
use std::path::{Path, PathBuf};
use std::time::Duration;

use tokio::io::{AsyncBufReadExt, AsyncWrite, AsyncWriteExt};
use tokio::sync::mpsc;

use crate::protocol::{
    read_msg, write_msg, AgentInfo, ClientHello, ControlCommand, Intent, ServerHello, ServerMsg,
    SplitDir, PROTOCOL_VERSION,
};
use wire::{BridgeCmd, BridgeEvent, Replay};

/// How often the runtime dir is re-scanned for sessions appearing/disappearing.
const SCAN_INTERVAL: Duration = Duration::from_secs(2);
/// How often watched transcripts are polled for new lines.
const TAIL_INTERVAL: Duration = Duration::from_millis(250);

/// Internal fan-in from per-session subscriber tasks to the core loop.
enum Internal {
    Roster {
        session: String,
        epoch: String,
        agents: Vec<AgentInfo>,
    },
    SessionGone {
        session: String,
    },
}

/// Per-session live state the core loop tracks.
struct SessionState {
    epoch: String,
    agents: Vec<AgentInfo>,
}

/// Run the bridge: emit `hello`, scan `runtime_dir` for session sockets,
/// subscribe to each, tail watched transcripts, dispatch stdin commands.
/// `filter`: restrict to one session name (`dvlpr bridge @name`).
/// Exits on stdin EOF. Never exits on bad input (spec: reply ok:false).
pub async fn run(
    runtime_dir: PathBuf,
    stdin: impl tokio::io::AsyncBufRead + Unpin,
    mut stdout: impl AsyncWrite + Unpin,
    filter: Option<String>,
) -> io::Result<()> {
    let mut out = NdjsonOut { w: &mut stdout };
    out.emit(&BridgeEvent::Hello {
        bridge_protocol: wire::BRIDGE_PROTOCOL,
        dvlpr_version: env!("CARGO_PKG_VERSION").to_string(),
    })
    .await?;

    let (in_tx, mut in_rx) = mpsc::unbounded_channel::<Internal>();
    let mut sessions: HashMap<String, SessionState> = HashMap::new();
    let mut subscriber_tasks: HashMap<String, tokio::task::JoinHandle<()>> = HashMap::new();
    let mut watches: HashMap<String, tail::TranscriptTail> = HashMap::new(); // key -> tail

    let mut scan_tick = tokio::time::interval(SCAN_INTERVAL);
    let mut tail_tick = tokio::time::interval(TAIL_INTERVAL);
    let mut lines = stdin.lines();

    loop {
        tokio::select! {
            // --- consumer commands -------------------------------------------------
            line = lines.next_line() => {
                let Some(line) = line? else { break }; // stdin EOF: ssh hung up
                if line.trim().is_empty() { continue; }
                match parse_cmd_line(&line) {
                    Ok(cmd) => {
                        handle_cmd(cmd, &runtime_dir, &sessions, &mut watches, &mut out).await?;
                    }
                    Err((id, message)) => {
                        out.emit(&BridgeEvent::Reply {
                            id,
                            ok: false,
                            code: Some("bad_command".into()),
                            message: Some(message),
                        }).await?;
                    }
                }
            }
            // --- roster events from subscriber tasks -------------------------------
            Some(internal) = in_rx.recv() => {
                match internal {
                    Internal::Roster { session, epoch, agents } => {
                        sessions.insert(session.clone(), SessionState {
                            epoch: epoch.clone(),
                            agents: agents.clone(),
                        });
                        out.emit(&BridgeEvent::Agents { session, epoch, agents }).await?;
                    }
                    Internal::SessionGone { session } => {
                        sessions.remove(&session);
                        subscriber_tasks.remove(&session);
                        // Drop watches belonging to the dead session.
                        watches.retain(|key, _| !key.starts_with(&format!("{session}/")));
                        out.emit(&BridgeEvent::SessionGone { session }).await?;
                    }
                }
            }
            // --- session discovery --------------------------------------------------
            _ = scan_tick.tick() => {
                for name in scan_sessions(&runtime_dir, filter.as_deref()) {
                    if subscriber_tasks.contains_key(&name) { continue; }
                    let sock = crate::server::socket::socket_path_in(&runtime_dir, &name);
                    let tx = in_tx.clone();
                    let task_name = name.clone();
                    subscriber_tasks.insert(name, tokio::spawn(async move {
                        let _ = subscribe_session(&sock, &task_name, &tx).await;
                        let _ = tx.send(Internal::SessionGone { session: task_name });
                    }));
                }
            }
            // --- transcript tails ----------------------------------------------------
            _ = tail_tick.tick() => {
                poll_watches(&mut watches, &mut out).await?;
            }
        }
    }

    for (_, t) in subscriber_tasks.drain() {
        t.abort();
    }
    Ok(())
}

/// Serialize one event as an NDJSON line. A wrapper (not a free fn) so every
/// emit site shares the same newline + flush discipline.
struct NdjsonOut<'a, W: AsyncWrite + Unpin> {
    w: &'a mut W,
}

impl<W: AsyncWrite + Unpin> NdjsonOut<'_, W> {
    async fn emit(&mut self, ev: &BridgeEvent) -> io::Result<()> {
        // serde_json::Error converts via `?`: serde_json provides
        // `impl From<Error> for io::Error`.
        let mut line = serde_json::to_vec(ev)?;
        line.push(b'\n');
        self.w.write_all(&line).await?;
        self.w.flush().await
    }
}

/// Two-stage parse: pull `id` out of the raw JSON FIRST, so even an unknown
/// or malformed command keeps reply correlation (spec §2.2: every reply
/// echoes the command's id). Err carries (id, message) for the bad_command
/// reply; id is "" only when the line isn't JSON at all.
fn parse_cmd_line(line: &str) -> Result<BridgeCmd, (String, String)> {
    let raw: serde_json::Value = match serde_json::from_str(line) {
        Ok(v) => v,
        Err(e) => return Err((String::new(), format!("unparseable command: {e}"))),
    };
    let id = raw["id"].as_str().unwrap_or_default().to_string();
    serde_json::from_value::<BridgeCmd>(raw).map_err(|e| (id, format!("unparseable command: {e}")))
}

/// One pass over all watched transcripts: emit complete new lines as
/// `transcript` events; on truncation/rotation (or IO error) drop the watch
/// and emit `transcript_reset` so the consumer re-watches. A free function
/// (not inlined in the select arm) so the replay/tail/reset contract is
/// unit-testable without a daemon.
async fn poll_watches<W: AsyncWrite + Unpin>(
    watches: &mut HashMap<String, tail::TranscriptTail>,
    out: &mut NdjsonOut<'_, W>,
) -> io::Result<()> {
    let mut resets = Vec::new();
    for (key, t) in watches.iter_mut() {
        match t.poll() {
            Ok(tail::TailPoll::Lines(lines)) => {
                for l in lines {
                    match serde_json::from_str::<serde_json::Value>(&l) {
                        Ok(line) => {
                            out.emit(&BridgeEvent::Transcript {
                                key: key.clone(),
                                line,
                            })
                            .await?
                        }
                        Err(_) => {
                            eprintln!("dvlpr bridge: skipping unparseable transcript line ({key})")
                        }
                    }
                }
            }
            Ok(tail::TailPoll::Reset) | Err(_) => resets.push(key.clone()),
        }
    }
    for key in resets {
        watches.remove(&key);
        out.emit(&BridgeEvent::TranscriptReset { key }).await?;
    }
    Ok(())
}

/// Names of live-looking session sockets in the runtime dir.
fn scan_sessions(dir: &Path, filter: Option<&str>) -> Vec<String> {
    let mut out = Vec::new();
    let Ok(entries) = std::fs::read_dir(dir) else {
        return out; // no runtime dir yet: normal empty state
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        let Some(session) = name.strip_suffix(".sock") else {
            continue;
        };
        if filter.is_some_and(|f| f != session) {
            continue;
        }
        out.push(session.to_string());
    }
    out
}

/// One session's Subscribe connection: handshake then forward every roster
/// snapshot. Returns when the daemon closes the connection or errors.
async fn subscribe_session(
    sock: &Path,
    session: &str,
    tx: &mpsc::UnboundedSender<Internal>,
) -> io::Result<()> {
    let stream = tokio::net::UnixStream::connect(sock).await?;
    let (mut r, mut w) = stream.into_split();
    write_msg(
        &mut w,
        &ClientHello {
            protocol_version: PROTOCOL_VERSION,
            intent: Intent::Subscribe,
        },
    )
    .await?;
    match read_msg::<_, ServerHello>(&mut r).await? {
        Some(ServerHello::Ok { .. }) => {}
        Some(ServerHello::Reject { reason }) => {
            eprintln!("dvlpr bridge: {session}: rejected: {reason}");
            return Ok(());
        }
        None => return Ok(()),
    }
    loop {
        match read_msg::<_, ServerMsg>(&mut r).await? {
            Some(ServerMsg::Agents { epoch, agents }) => {
                if tx
                    .send(Internal::Roster {
                        session: session.to_string(),
                        epoch,
                        agents,
                    })
                    .is_err()
                {
                    return Ok(());
                }
            }
            Some(_) => continue, // Frame/Detach/Closed: not expected on Subscribe
            None => return Ok(()),
        }
    }
}

/// Dispatch one consumer command. Every targeted command (anything with a
/// `key` or `window`, except `unwatch`) REQUIRES an epoch: the daemon's
/// `epoch: None` skip is reserved for the local interactive CLI, so a remote
/// consumer omitting it would silently bypass the stale-target safety
/// property (spec §2.2). Missing epoch → `missing_epoch`, never forwarded.
/// The daemon remains the authoritative validator of the VALUE; the bridge's
/// roster lookup is only a fast-path for better error messages.
async fn handle_cmd<W: AsyncWrite + Unpin>(
    cmd: BridgeCmd,
    runtime_dir: &Path,
    sessions: &HashMap<String, SessionState>,
    watches: &mut HashMap<String, tail::TranscriptTail>,
    out: &mut NdjsonOut<'_, W>,
) -> io::Result<()> {
    use BridgeCmd as B;
    let reply =
        |id: String, ok: bool, code: Option<&str>, message: Option<String>| BridgeEvent::Reply {
            id,
            ok,
            code: code.map(str::to_string),
            message,
        };

    match cmd {
        B::Watch {
            id,
            key,
            epoch,
            replay,
        } => {
            let Some((session, pane)) = wire::parse_key(&key) else {
                return out.emit(&reply(id, false, Some("bad_key"), None)).await;
            };
            let Some(epoch) = epoch else {
                return out
                    .emit(&reply(id, false, Some("missing_epoch"), None))
                    .await;
            };
            let Some(state) = sessions.get(&session) else {
                return out
                    .emit(&reply(id, false, Some("unknown_session"), None))
                    .await;
            };
            if epoch != state.epoch {
                return out
                    .emit(&reply(id, false, Some("stale_target"), None))
                    .await;
            }
            let Some(info) = state.agents.iter().find(|a| a.pane_id == pane) else {
                return out
                    .emit(&reply(id, false, Some("unknown_pane"), None))
                    .await;
            };
            let Some(path) = info.transcript.as_deref() else {
                return out
                    .emit(&reply(
                        id,
                        false,
                        Some("no_transcript"),
                        Some("agent transcript not discovered yet; retry shortly".into()),
                    ))
                    .await;
            };
            match tail::TranscriptTail::open(Path::new(path), replay == Replay::Full) {
                Ok(t) => {
                    watches.insert(key, t);
                    out.emit(&reply(id, true, None, None)).await
                }
                Err(e) => {
                    out.emit(&reply(id, false, Some("io"), Some(e.to_string())))
                        .await
                }
            }
        }
        B::Unwatch { id, key } => {
            watches.remove(&key);
            out.emit(&reply(id, true, None, None)).await
        }
        B::Send {
            id,
            key,
            epoch,
            text,
            submit,
        } => {
            let Some((session, pane)) = wire::parse_key(&key) else {
                return out.emit(&reply(id, false, Some("bad_key"), None)).await;
            };
            let Some(epoch) = epoch else {
                return out
                    .emit(&reply(id, false, Some("missing_epoch"), None))
                    .await;
            };
            let cmd = ControlCommand::PaneSend { pane, text, submit };
            forward(id, runtime_dir, &session, cmd, epoch, out).await
        }
        B::Key {
            id,
            key,
            epoch,
            name,
        } => {
            let Some((session, pane)) = wire::parse_key(&key) else {
                return out.emit(&reply(id, false, Some("bad_key"), None)).await;
            };
            let Some(epoch) = epoch else {
                return out
                    .emit(&reply(id, false, Some("missing_epoch"), None))
                    .await;
            };
            let Some(named) = wire::parse_key_name(&name) else {
                return out
                    .emit(&reply(id, false, Some("bad_key_name"), None))
                    .await;
            };
            let cmd = ControlCommand::PaneKey { pane, key: named };
            forward(id, runtime_dir, &session, cmd, epoch, out).await
        }
        B::WindowClose {
            id,
            session,
            epoch,
            window,
        } => {
            let Some(epoch) = epoch else {
                return out
                    .emit(&reply(id, false, Some("missing_epoch"), None))
                    .await;
            };
            let cmd = ControlCommand::WindowClose {
                window: Some(window),
            };
            forward(id, runtime_dir, &session, cmd, epoch, out).await
        }
        B::WindowRename {
            id,
            session,
            epoch,
            window,
            name,
        } => {
            let Some(epoch) = epoch else {
                return out
                    .emit(&reply(id, false, Some("missing_epoch"), None))
                    .await;
            };
            let cmd = ControlCommand::WindowRename {
                window: Some(window),
                name,
            };
            forward(id, runtime_dir, &session, cmd, epoch, out).await
        }
        B::WindowSelect {
            id,
            session,
            epoch,
            window,
        } => {
            let Some(epoch) = epoch else {
                return out
                    .emit(&reply(id, false, Some("missing_epoch"), None))
                    .await;
            };
            // Bridge JSON is 0-based; the pre-existing wire form is 1-based (spec §1.2).
            let Ok(n) = u8::try_from(window + 1) else {
                return out.emit(&reply(id, false, Some("bad_window"), None)).await;
            };
            forward(
                id,
                runtime_dir,
                &session,
                ControlCommand::WindowSelect(n),
                epoch,
                out,
            )
            .await
        }
        B::PaneSplit {
            id,
            session,
            epoch,
            window,
            dir,
        } => {
            let Some(epoch) = epoch else {
                return out
                    .emit(&reply(id, false, Some("missing_epoch"), None))
                    .await;
            };
            let dir = match dir.as_str() {
                "right" => SplitDir::Right,
                "down" => SplitDir::Down,
                _ => return out.emit(&reply(id, false, Some("bad_dir"), None)).await,
            };
            let cmd = ControlCommand::PaneSplit {
                window: Some(window),
                dir,
            };
            forward(id, runtime_dir, &session, cmd, epoch, out).await
        }
    }
}

/// Forward a control command to the owning session's daemon and emit the reply.
/// `epoch` is required (the bridge never uses the daemon's None skip-path);
/// daemon-side "stale_target" surfaces with the spec's `code`.
async fn forward<W: AsyncWrite + Unpin>(
    id: String,
    runtime_dir: &Path,
    session: &str,
    cmd: ControlCommand,
    epoch: String,
    out: &mut NdjsonOut<'_, W>,
) -> io::Result<()> {
    let sock = crate::server::socket::socket_path_in(runtime_dir, session);
    let ev = match crate::client::send_command(&sock, cmd, Some(epoch)).await {
        Ok(r) => {
            let code = (!r.ok && r.message.as_deref() == Some("stale_target"))
                .then(|| "stale_target".to_string());
            BridgeEvent::Reply {
                id,
                ok: r.ok,
                code,
                message: r.message,
            }
        }
        Err(e) => BridgeEvent::Reply {
            id,
            ok: false,
            code: Some("unreachable".into()),
            message: Some(e.to_string()),
        },
    };
    out.emit(&ev).await
}

#[cfg(test)]
mod tests {
    use super::*;

    fn agent_info(session: &str, pane: u64, transcript: &Path) -> AgentInfo {
        AgentInfo {
            session: session.to_string(),
            window_index: 0,
            window_name: "w".into(),
            pane_id: pane,
            agent: "claude".into(),
            state: "working".into(),
            cwd: None,
            branch: None,
            session_label: None,
            agent_session_id: Some("sid".into()),
            transcript: Some(transcript.to_string_lossy().into_owned()),
        }
    }

    fn parse_lines(buf: &[u8]) -> Vec<serde_json::Value> {
        String::from_utf8_lossy(buf)
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect()
    }

    fn one_session(transcript: &Path) -> HashMap<String, SessionState> {
        let mut sessions = HashMap::new();
        sessions.insert(
            "s".to_string(),
            SessionState {
                epoch: "e1".into(),
                agents: vec![agent_info("s", 3, transcript)],
            },
        );
        sessions
    }

    #[tokio::test]
    async fn watch_replays_full_then_tails_then_resets() {
        let tmp = tempfile::tempdir().unwrap();
        let t = tmp.path().join("t.jsonl");
        std::fs::write(&t, "{\"n\":1}\n{\"n\":2}\n").unwrap();
        let sessions = one_session(&t);
        let mut watches = HashMap::new();
        let mut buf: Vec<u8> = Vec::new();
        let mut out = NdjsonOut { w: &mut buf };

        handle_cmd(
            BridgeCmd::Watch {
                id: "1".into(),
                key: "s/3".into(),
                epoch: Some("e1".into()),
                replay: Replay::Full,
            },
            Path::new("/nonexistent"),
            &sessions,
            &mut watches,
            &mut out,
        )
        .await
        .unwrap();
        poll_watches(&mut watches, &mut out).await.unwrap();

        // Tail: a line appended after the watch arrives on the next poll.
        {
            use std::io::Write as _;
            let mut f = std::fs::OpenOptions::new().append(true).open(&t).unwrap();
            f.write_all(b"{\"n\":3}\n").unwrap();
        }
        poll_watches(&mut watches, &mut out).await.unwrap();

        // Truncation: reset event + watch dropped.
        std::fs::write(&t, "").unwrap();
        poll_watches(&mut watches, &mut out).await.unwrap();
        assert!(watches.is_empty(), "reset must drop the watch");

        let evs = parse_lines(&buf);
        assert_eq!(evs[0]["event"], "reply");
        assert_eq!(evs[0]["ok"], true);
        let transcripts: Vec<_> = evs.iter().filter(|v| v["event"] == "transcript").collect();
        assert_eq!(transcripts.len(), 3, "replay 2 + tail 1, in order");
        assert_eq!(transcripts[0]["line"]["n"], 1);
        assert_eq!(transcripts[1]["line"]["n"], 2);
        assert_eq!(transcripts[2]["line"]["n"], 3);
        assert!(evs.iter().any(|v| v["event"] == "transcript_reset"));
    }

    #[tokio::test]
    async fn watch_with_wrong_or_missing_epoch_never_opens_a_tail() {
        let tmp = tempfile::tempdir().unwrap();
        let t = tmp.path().join("t.jsonl");
        std::fs::write(&t, "{\"n\":1}\n").unwrap();
        let sessions = one_session(&t);
        let mut watches = HashMap::new();
        let mut buf: Vec<u8> = Vec::new();

        for (id, epoch, code) in [
            ("1", Some("stale".to_string()), "stale_target"),
            ("2", None, "missing_epoch"),
        ] {
            handle_cmd(
                BridgeCmd::Watch {
                    id: id.into(),
                    key: "s/3".into(),
                    epoch,
                    replay: Replay::Full,
                },
                Path::new("/nonexistent"),
                &sessions,
                &mut watches,
                &mut NdjsonOut { w: &mut buf },
            )
            .await
            .unwrap();
            let evs = parse_lines(&buf);
            assert_eq!(evs.last().unwrap()["code"], code);
        }
        assert!(watches.is_empty());
    }

    #[test]
    fn bad_commands_keep_reply_correlation_id() {
        // Unknown cmd with a valid id: the id survives for the reply.
        let (id, _msg) = parse_cmd_line(r#"{"id":"x","cmd":"bogus"}"#).unwrap_err();
        assert_eq!(id, "x");
        // Not JSON at all: id falls back to "".
        let (id, _msg) = parse_cmd_line("not json").unwrap_err();
        assert_eq!(id, "");
        // A valid command still parses.
        assert!(parse_cmd_line(r#"{"id":"1","cmd":"unwatch","key":"s/1"}"#).is_ok());
    }

    #[tokio::test]
    async fn targeted_commands_without_epoch_are_rejected_before_forwarding() {
        // Empty runtime dir: if the bridge tried to forward, the reply code
        // would be "unreachable", not "missing_epoch".
        let tmp = tempfile::tempdir().unwrap();
        let sessions = HashMap::new();
        let mut watches = HashMap::new();
        let mut buf: Vec<u8> = Vec::new();
        let mut out = NdjsonOut { w: &mut buf };

        handle_cmd(
            BridgeCmd::Send {
                id: "1".into(),
                key: "s/3".into(),
                epoch: None,
                text: "x".into(),
                submit: false,
            },
            tmp.path(),
            &sessions,
            &mut watches,
            &mut out,
        )
        .await
        .unwrap();
        handle_cmd(
            BridgeCmd::WindowRename {
                id: "2".into(),
                session: "s".into(),
                epoch: None,
                window: 0,
                name: "x".into(),
            },
            tmp.path(),
            &sessions,
            &mut watches,
            &mut out,
        )
        .await
        .unwrap();

        let evs = parse_lines(&buf);
        assert_eq!(evs.len(), 2);
        for ev in evs {
            assert_eq!(ev["ok"], false);
            assert_eq!(ev["code"], "missing_epoch");
        }
    }
}
