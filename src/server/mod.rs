pub mod socket;

use std::collections::HashMap;
use std::io;
use std::path::PathBuf;
use std::time::Duration;

use tokio::net::{UnixListener, UnixStream};
use tokio::sync::mpsc;

use crate::pane::{PaneOutput, PaneRuntime};
use crate::protocol::{
    read_msg, write_msg, ClientHello, ClientMsg, ServerHello, ServerMsg, PROTOCOL_VERSION,
};
use crate::screen::Screen;

/// How a daemon should start: where to listen and what to run in the pane.
pub struct ServerConfig {
    pub socket_path: PathBuf,
    pub command: Vec<String>,
    pub cwd: String,
}

impl ServerConfig {
    pub fn for_default_session() -> io::Result<Self> {
        let dir = socket::runtime_dir();
        socket::ensure_runtime_dir(&dir)?;
        let socket_path = socket::socket_path_in(&dir, "default");
        socket::check_sun_path_len(&socket_path)?;
        Ok(ServerConfig {
            socket_path,
            command: Vec::new(), // empty => default shell
            cwd: std::env::var("HOME").unwrap_or_else(|_| ".".to_string()),
        })
    }
}

type ClientId = u64;

/// Events funneled into the single ServerTask.
enum Event {
    ClientConnected {
        id: ClientId,
        frames: mpsc::UnboundedSender<ServerMsg>,
        cols: u16,
        rows: u16,
    },
    ClientInput(Vec<u8>),
    ClientResize {
        cols: u16,
        rows: u16,
    },
    ClientGone(ClientId),
    PaneBytes(Vec<u8>),
    PaneExited,
}

/// Run the daemon to completion (until the pane exits).
pub async fn run(config: ServerConfig) -> io::Result<()> {
    // Bind: remove a stale socket first, reject a live one.
    if config.socket_path.exists() {
        if socket::is_live(&config.socket_path).await {
            return Err(io::Error::new(
                io::ErrorKind::AddrInUse,
                "a dvlpr server is already running on this socket",
            ));
        }
        let _ = std::fs::remove_file(&config.socket_path);
    }
    let listener = UnixListener::bind(&config.socket_path)?;
    socket::lock_down_socket(&config.socket_path)?;
    tracing::info!(socket = %config.socket_path.display(), "dvlpr server listening");

    let (ev_tx, mut ev_rx) = mpsc::unbounded_channel::<Event>();

    // Spawn the pane at a default size; first client resizes it.
    let (pane, mut pane_rx) = PaneRuntime::spawn(&config.command, &config.cwd, 80, 24)?;
    let mut screen = Screen::new(80, 24);

    // Forward pane output into the event loop.
    {
        let ev_tx = ev_tx.clone();
        tokio::spawn(async move {
            while let Some(out) = pane_rx.recv().await {
                let event = match out {
                    PaneOutput::Bytes(b) => Event::PaneBytes(b),
                    PaneOutput::Exited => Event::PaneExited,
                };
                if ev_tx.send(event).is_err() {
                    break;
                }
            }
        });
    }

    // Accept connections.
    {
        let ev_tx = ev_tx.clone();
        tokio::spawn(async move {
            let mut next_id: ClientId = 1;
            while let Ok((stream, _)) = listener.accept().await {
                let id = next_id;
                next_id += 1;
                spawn_client(id, stream, ev_tx.clone());
            }
        });
    }

    // Central loop: owns the Screen and the PTY writer; renders on a coalesced tick.
    //
    // PHASE 1 MULTI-CLIENT POLICY (deliberate simplification — NOT the final model):
    // Phase 1 targets a single attached client. Multiple clients are tolerated, but
    // there is no foreground/observer distinction: input is accepted from any client
    // and the most recent connect/resize drives the shared pane size (last-writer-
    // wins). The SPEC's full single-active-writer model — one foreground writer drives
    // geometry+input, observers get clipped/letterboxed frames, a second writer needs
    // `--takeover`, and foreground is promoted on disconnect — is Phase 3 work
    // (see "Out of scope"). It is intentionally NOT implemented here.
    let mut clients: HashMap<ClientId, mpsc::UnboundedSender<ServerMsg>> = HashMap::new();
    let mut dirty = false;
    let mut tick = tokio::time::interval(Duration::from_millis(16)); // ~60fps cap

    loop {
        tokio::select! {
            maybe_event = ev_rx.recv() => {
                let Some(event) = maybe_event else { break };
                match event {
                    Event::ClientConnected { id, frames, cols, rows } => {
                        // Last connecting client drives the shared pane size (Phase 1).
                        screen.resize(cols, rows);
                        pane.resize(cols, rows);
                        // Immediate full repaint for the newcomer.
                        let _ = frames.send(ServerMsg::Frame {
                            data: screen.render_ansi(),
                            full: true,
                        });
                        clients.insert(id, frames);
                        // Resized geometry should repaint any other clients too.
                        dirty = true;
                    }
                    Event::ClientInput(bytes) => pane.write_input(&bytes),
                    Event::ClientResize { cols, rows } => {
                        screen.resize(cols, rows);
                        pane.resize(cols, rows);
                        dirty = true;
                    }
                    Event::ClientGone(id) => { clients.remove(&id); }
                    Event::PaneBytes(bytes) => {
                        screen.feed(&bytes);
                        dirty = true;
                    }
                    Event::PaneExited => {
                        tracing::info!("pane process exited; shutting down server");
                        for (_, tx) in clients.drain() {
                            let _ = tx.send(ServerMsg::Closed {
                                reason: "pane process exited".to_string(),
                            });
                        }
                        break;
                    }
                }
            }
            _ = tick.tick() => {
                if dirty && !clients.is_empty() {
                    let frame = ServerMsg::Frame { data: screen.render_ansi(), full: true };
                    clients.retain(|_, tx| tx.send(frame.clone()).is_ok());
                    dirty = false;
                }
            }
        }
    }

    let _ = std::fs::remove_file(&config.socket_path);
    Ok(())
}

/// Per-client connection: handshake, then split read/write halves.
fn spawn_client(id: ClientId, stream: UnixStream, ev_tx: mpsc::UnboundedSender<Event>) {
    tokio::spawn(async move {
        let (mut read_half, mut write_half) = stream.into_split();

        // Handshake.
        let hello: ClientHello = match read_msg(&mut read_half).await {
            Ok(Some(h)) => h,
            Ok(None) => return, // clean disconnect before handshake
            Err(e) => {
                tracing::warn!(error = %e, "client handshake read failed");
                return;
            }
        };
        if hello.protocol_version != PROTOCOL_VERSION {
            tracing::warn!(
                client = hello.protocol_version,
                server = PROTOCOL_VERSION,
                "rejecting client: protocol mismatch"
            );
            let _ = write_msg(
                &mut write_half,
                &ServerHello::Reject {
                    reason: format!(
                        "protocol mismatch: server {PROTOCOL_VERSION}, client {}",
                        hello.protocol_version
                    ),
                },
            )
            .await;
            return;
        }
        if write_msg(
            &mut write_half,
            &ServerHello::Ok {
                protocol_version: PROTOCOL_VERSION,
            },
        )
        .await
        .is_err()
        {
            return;
        }

        // Register for outgoing frames.
        let (frame_tx, mut frame_rx) = mpsc::unbounded_channel::<ServerMsg>();
        if ev_tx
            .send(Event::ClientConnected {
                id,
                frames: frame_tx,
                cols: hello.cols,
                rows: hello.rows,
            })
            .is_err()
        {
            return;
        }

        // Writer half: drain frames to the socket.
        tokio::spawn(async move {
            while let Some(msg) = frame_rx.recv().await {
                if write_msg(&mut write_half, &msg).await.is_err() {
                    break;
                }
            }
        });

        // Reader half: forward client messages into the event loop.
        loop {
            match read_msg::<_, ClientMsg>(&mut read_half).await {
                Ok(Some(ClientMsg::Input(bytes))) => {
                    let _ = ev_tx.send(Event::ClientInput(bytes));
                }
                Ok(Some(ClientMsg::Resize { cols, rows })) => {
                    let _ = ev_tx.send(Event::ClientResize { cols, rows });
                }
                Ok(Some(ClientMsg::Detach)) | Ok(None) | Err(_) => break,
            }
        }
        let _ = ev_tx.send(Event::ClientGone(id));
    });
}
