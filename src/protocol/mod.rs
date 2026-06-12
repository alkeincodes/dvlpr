//! Wire protocol: framed bincode messages over the Unix socket.

use std::io;

use serde::{Deserialize, Serialize};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

/// Bumped whenever the wire format changes. Client and server must match exactly.
pub const PROTOCOL_VERSION: u32 = 7;

/// Reject oversized frames to bound memory.
pub const MAX_FRAME_BYTES: usize = 8 * 1024 * 1024;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClientHello {
    pub protocol_version: u32,
    pub intent: Intent,
}

/// What a freshly-connected client wants from the daemon.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Intent {
    /// Interactive attach (the normal flow); carries the client's terminal size.
    Attach { cols: u16, rows: u16 },
    /// One-shot query: the server replies with `StatusInfo` and closes. No attach.
    Status,
    /// Ask the daemon to shut down. The server tears down and exits.
    /// `keep_snapshot`: when true, the daemon flushes a final layout snapshot
    /// and tears down WITHOUT deleting it (used by `dvlpr update`'s restart
    /// orchestration so the session can be restored by the respawned daemon).
    /// `dvlpr stop` sends `false` (graceful stop still deletes the snapshot).
    Kill { keep_snapshot: bool },
    /// One-shot control command: the server applies it, replies `CommandReply`,
    /// and closes. No attach.
    Command,
    /// Long-lived push channel: the server replies `ServerHello::Ok`, then
    /// streams `ServerMsg::Agents` roster snapshots (on connect + on change).
    /// (Appended last to keep discriminants stable.)
    Subscribe,
}

/// Reply to an `Intent::Status` request.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StatusInfo {
    pub windows: u32,
    pub clients: u32,
}

/// One agent pane's full roster entry, pushed to Subscribe clients. String
/// `agent`/`state` (not the `detect` enums) keep this wire type serde-friendly
/// for both bincode (socket) and JSON (bridge NDJSON) without touching
/// `detect`. camelCase renames affect only JSON; bincode ignores field names.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentInfo {
    pub session: String,
    pub window_index: usize, // 0-based
    pub window_name: String,
    pub pane_id: u64,
    pub agent: String, // "claude" | "codex"
    pub state: String, // "idle" | "working" | "blocked" | "done"
    pub cwd: Option<String>,
    pub branch: Option<String>,
    pub session_label: Option<String>,
    pub agent_session_id: Option<String>, // from the pane's cached AgentResume
    pub transcript: Option<String>,       // from the pane's cached AgentResume
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ServerHello {
    Ok { protocol_version: u32 },
    Reject { reason: String },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ClientMsg {
    Input(Vec<u8>),
    Resize {
        cols: u16,
        rows: u16,
    },
    /// One-shot control command envelope. `epoch`: when Some, the server
    /// validates it against its own startup epoch BEFORE applying — closing
    /// the daemon-restart race for pane-id targeting. None (the local control
    /// CLI's path) skips the check: a human is acting on live state.
    Command {
        cmd: ControlCommand,
        epoch: Option<String>,
    },
}

/// Named keys for `PaneKey` — answering approval prompts in agent TUIs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum NamedKey {
    Enter,
    Esc,
    Up,
    Down,
    Tab,
    Digit(u8),
    Char(char),
}

impl NamedKey {
    /// The raw byte sequence a terminal would send for this key.
    pub fn bytes(self) -> Vec<u8> {
        match self {
            NamedKey::Enter => b"\r".to_vec(),
            NamedKey::Esc => b"\x1b".to_vec(),
            NamedKey::Up => b"\x1b[A".to_vec(),
            NamedKey::Down => b"\x1b[B".to_vec(),
            NamedKey::Tab => b"\t".to_vec(),
            NamedKey::Digit(d) => vec![b'0' + (d % 10)],
            NamedKey::Char(c) => c.to_string().into_bytes(),
        }
    }
}

/// Direction for a pane split, mirroring the in-session keybindings:
/// `Right` = new pane to the right (`C-b →`), `Down` = new pane below (`C-b ↓`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum SplitDir {
    Right,
    Down,
}

/// The scriptable session operations. Each maps onto an operation that already
/// exists via keybindings/menus today. Kept separate from `config::Command`
/// (which is `Copy` and parameterless) so string-bearing variants don't force
/// `config::Command` to grow serde or drop `Copy`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ControlCommand {
    WindowNew {
        name: Option<String>,
    },
    /// `window`: 0-based target; None = active window (CLI path).
    WindowRename {
        window: Option<usize>,
        name: String,
    },
    WindowClose {
        window: Option<usize>,
    },
    WindowNext,
    WindowPrev,
    /// 1-based, wire-compact (pre-existing; the bridge converts 0-based JSON).
    /// The server range-validates then casts to `usize` for
    /// `config::Command::SelectWindow(usize)`.
    WindowSelect(u8),
    PaneSplit {
        window: Option<usize>,
        dir: SplitDir,
    },
    PaneClose,
    PaneZoom,
    SidebarToggle,
    /// Targeted by pane id (stable within one daemon lifetime; epoch guards
    /// cross-restart staleness). Writes bracketed-pasted text to the PTY.
    PaneSend {
        pane: u64,
        text: String,
        submit: bool,
    },
    PaneKey {
        pane: u64,
        key: NamedKey,
    },
}

/// Reply to an `Intent::Command` request: sent once, then the connection closes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CommandReply {
    pub ok: bool,
    pub message: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ServerMsg {
    /// A rendered frame. `full` is true for a complete repaint (first paint,
    /// resize, resync); false for an incremental per-row diff.
    Frame {
        data: Vec<u8>,
        full: bool,
    },
    /// The server detached this client (server-initiated); the client exits.
    Detach,
    Closed {
        reason: String,
    },
    /// Roster snapshot for Subscribe clients (appended last for discriminant
    /// stability). `epoch` identifies this daemon incarnation.
    Agents {
        epoch: String,
        agents: Vec<AgentInfo>,
    },
}

pub fn encode<T: Serialize>(value: &T) -> io::Result<Vec<u8>> {
    bincode::serde::encode_to_vec(value, bincode::config::standard())
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
}

pub fn decode<T: serde::de::DeserializeOwned>(bytes: &[u8]) -> io::Result<T> {
    let (value, consumed) = bincode::serde::decode_from_slice(bytes, bincode::config::standard())
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
    if consumed != bytes.len() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "trailing bytes after decoded frame",
        ));
    }
    Ok(value)
}

/// Write a length-prefixed bincode frame: `u32` big-endian length + payload.
pub async fn write_msg<W, T>(w: &mut W, value: &T) -> io::Result<()>
where
    W: AsyncWriteExt + Unpin,
    T: Serialize,
{
    let payload = encode(value)?;
    if payload.len() > MAX_FRAME_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "frame exceeds MAX_FRAME_BYTES",
        ));
    }
    w.write_all(&(payload.len() as u32).to_be_bytes()).await?;
    w.write_all(&payload).await?;
    w.flush().await?;
    Ok(())
}

/// Read one length-prefixed bincode frame. Returns `Ok(None)` on clean EOF.
pub async fn read_msg<R, T>(r: &mut R) -> io::Result<Option<T>>
where
    R: AsyncReadExt + Unpin,
    T: serde::de::DeserializeOwned,
{
    let mut len_buf = [0u8; 4];
    // Probe the first byte: 0 bytes => clean EOF at a frame boundary (Ok(None)).
    // A short read AFTER the first byte is a truncated header => error, not EOF.
    if r.read(&mut len_buf[..1]).await? == 0 {
        return Ok(None);
    }
    r.read_exact(&mut len_buf[1..]).await?;
    let len = u32::from_be_bytes(len_buf) as usize;
    if len > MAX_FRAME_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "incoming frame exceeds MAX_FRAME_BYTES",
        ));
    }
    let mut payload = vec![0u8; len];
    r.read_exact(&mut payload).await?;
    Ok(Some(decode(&payload)?))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn protocol_version_is_seven() {
        assert_eq!(PROTOCOL_VERSION, 7);
    }

    #[test]
    fn v7_messages_round_trip_through_bincode() {
        let cases = vec![
            ClientMsg::Command {
                cmd: ControlCommand::PaneSend {
                    pane: 3,
                    text: "fix the failing test".into(),
                    submit: true,
                },
                epoch: Some("123-456".into()),
            },
            ClientMsg::Command {
                cmd: ControlCommand::PaneKey {
                    pane: 3,
                    key: NamedKey::Digit(1),
                },
                epoch: None,
            },
            ClientMsg::Command {
                cmd: ControlCommand::WindowRename {
                    window: Some(2),
                    name: "api".into(),
                },
                epoch: None,
            },
            ClientMsg::Command {
                cmd: ControlCommand::WindowClose { window: None },
                epoch: None,
            },
            ClientMsg::Command {
                cmd: ControlCommand::PaneSplit {
                    window: Some(0),
                    dir: SplitDir::Right,
                },
                epoch: None,
            },
        ];
        for c in cases {
            let bytes = encode(&c).unwrap();
            assert_eq!(c, decode::<ClientMsg>(&bytes).unwrap());
        }
        let hello = ClientHello {
            protocol_version: PROTOCOL_VERSION,
            intent: Intent::Subscribe,
        };
        assert_eq!(
            hello,
            decode::<ClientHello>(&encode(&hello).unwrap()).unwrap()
        );
        let agents = ServerMsg::Agents {
            epoch: "e1".into(),
            agents: vec![],
        };
        assert_eq!(
            agents,
            decode::<ServerMsg>(&encode(&agents).unwrap()).unwrap()
        );
    }

    #[test]
    fn named_key_maps_to_terminal_bytes() {
        assert_eq!(NamedKey::Enter.bytes(), b"\r".to_vec());
        assert_eq!(NamedKey::Esc.bytes(), b"\x1b".to_vec());
        assert_eq!(NamedKey::Up.bytes(), b"\x1b[A".to_vec());
        assert_eq!(NamedKey::Down.bytes(), b"\x1b[B".to_vec());
        assert_eq!(NamedKey::Tab.bytes(), b"\t".to_vec());
        assert_eq!(NamedKey::Digit(2).bytes(), b"2".to_vec());
        assert_eq!(NamedKey::Char('y').bytes(), b"y".to_vec());
    }

    #[test]
    fn kill_intent_round_trips_with_keep_snapshot() {
        for keep in [false, true] {
            let msg = ClientHello {
                protocol_version: PROTOCOL_VERSION,
                intent: Intent::Kill {
                    keep_snapshot: keep,
                },
            };
            let bytes = encode(&msg).unwrap();
            let back: ClientHello = decode(&bytes).unwrap();
            assert_eq!(msg, back);
        }
    }

    #[test]
    fn client_hello_round_trips_through_bincode() {
        let msg = ClientHello {
            protocol_version: PROTOCOL_VERSION,
            intent: Intent::Attach {
                cols: 100,
                rows: 30,
            },
        };
        let bytes = encode(&msg).unwrap();
        let back: ClientHello = decode(&bytes).unwrap();
        assert_eq!(msg, back);
    }

    #[test]
    fn status_info_round_trips_through_bincode() {
        let msg = StatusInfo {
            windows: 3,
            clients: 1,
        };
        let bytes = encode(&msg).unwrap();
        let back: StatusInfo = decode(&bytes).unwrap();
        assert_eq!(msg, back);
    }

    #[test]
    fn client_msg_round_trips_through_bincode() {
        let msg = ClientMsg::Resize {
            cols: 120,
            rows: 40,
        };
        let bytes = encode(&msg).unwrap();
        let back: ClientMsg = decode(&bytes).unwrap();
        assert_eq!(msg, back);
    }

    #[test]
    fn server_msg_round_trips_through_bincode() {
        let msg = ServerMsg::Frame {
            data: b"\x1b[2J\x1b[Hhi".to_vec(),
            full: true,
        };
        let bytes = encode(&msg).unwrap();
        let back: ServerMsg = decode(&bytes).unwrap();
        assert_eq!(msg, back);
    }

    #[tokio::test]
    async fn write_then_read_msg_round_trips() {
        let (mut a, mut b) = tokio::io::duplex(1024);
        let sent = ClientMsg::Input(b"ls -la\n".to_vec());
        write_msg(&mut a, &sent).await.unwrap();
        let got: ClientMsg = read_msg(&mut b).await.unwrap().unwrap();
        assert_eq!(sent, got);
    }

    #[tokio::test]
    async fn read_msg_returns_none_on_clean_eof() {
        let (a, mut b) = tokio::io::duplex(1024);
        drop(a); // close the writer end
        let got: Option<ClientMsg> = read_msg(&mut b).await.unwrap();
        assert!(got.is_none());
    }

    #[tokio::test]
    async fn read_msg_errors_on_truncated_header() {
        let (mut a, mut b) = tokio::io::duplex(1024);
        // Send only 2 of the 4 header bytes, then close: a truncated frame,
        // which must surface as an error, NOT a clean EOF.
        a.write_all(&[0u8, 0u8]).await.unwrap();
        drop(a);
        let result: io::Result<Option<ClientMsg>> = read_msg(&mut b).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn read_msg_rejects_oversized_frame() {
        let (mut a, mut b) = tokio::io::duplex(1024);
        // Header announces a length above MAX_FRAME_BYTES.
        let len = (MAX_FRAME_BYTES as u32) + 1;
        a.write_all(&len.to_be_bytes()).await.unwrap();
        let err = read_msg::<_, ClientMsg>(&mut b).await.unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
    }

    #[test]
    fn control_command_roundtrips_through_frame_codec() {
        let env = |cmd: ControlCommand| ClientMsg::Command { cmd, epoch: None };
        let cmds = vec![
            env(ControlCommand::WindowNew {
                name: Some("api".into()),
            }),
            env(ControlCommand::WindowRename {
                window: None,
                name: "db".into(),
            }),
            env(ControlCommand::WindowClose { window: None }),
            env(ControlCommand::WindowNext),
            env(ControlCommand::WindowPrev),
            env(ControlCommand::WindowSelect(3)),
            env(ControlCommand::PaneSplit {
                window: None,
                dir: SplitDir::Right,
            }),
            env(ControlCommand::PaneSplit {
                window: None,
                dir: SplitDir::Down,
            }),
            env(ControlCommand::PaneClose),
            env(ControlCommand::PaneZoom),
            env(ControlCommand::SidebarToggle),
        ];
        for c in cmds {
            let bytes = encode(&c).unwrap();
            let back: ClientMsg = decode(&bytes).unwrap();
            assert_eq!(c, back);
        }
        let reply = CommandReply {
            ok: false,
            message: Some("nope".into()),
        };
        assert_eq!(
            reply,
            decode::<CommandReply>(&encode(&reply).unwrap()).unwrap()
        );
    }
}
