pub mod socket;

use std::collections::HashMap;
use std::io;
use std::path::PathBuf;
use std::time::Duration;

use tokio::net::{UnixListener, UnixStream};
use tokio::sync::mpsc;

use crate::layout::PaneId;
use crate::pane::PaneOutput;
use crate::session::Session;
use crate::protocol::{
    read_msg, write_msg, ClientHello, ClientMsg, ServerHello, ServerMsg, PROTOCOL_VERSION,
};

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
    PaneOutput { pane_id: PaneId, output: PaneOutput },
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

    // The session owns all windows/panes and the compositor; start with one pane.
    let (mut session, first_pane, first_rx) =
        Session::new(config.command.clone(), config.cwd.clone(), 80, 24)?;
    spawn_pane_forwarder(first_pane, first_rx, ev_tx.clone());

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

    // Central loop: owns the Session (windows/panes/compositor); renders on a coalesced tick.
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
                        // Last connecting client drives the shared viewport size (Phase 1).
                        session.resize(cols, rows);
                        // Immediate full repaint for the newcomer.
                        let _ = frames.send(ServerMsg::Frame {
                            data: session.render(),
                            full: true,
                        });
                        clients.insert(id, frames);
                        dirty = true; // resized geometry repaints other clients too
                    }
                    Event::ClientInput(bytes) => session.input(&bytes),
                    Event::ClientResize { cols, rows } => {
                        session.resize(cols, rows);
                        dirty = true;
                    }
                    Event::ClientGone(id) => {
                        clients.remove(&id);
                    }
                    Event::PaneOutput { pane_id, output } => match output {
                        PaneOutput::Bytes(bytes) => {
                            session.feed(pane_id, &bytes);
                            dirty = true;
                        }
                        PaneOutput::Exited => {
                            // Tear the exited pane's runtime down off the run loop.
                            for runtime in session.pane_exited(pane_id) {
                                runtime.close();
                            }
                            if session.is_empty() {
                                tracing::info!("last pane exited; shutting down server");
                                for (_, tx) in clients.drain() {
                                    let _ = tx.send(ServerMsg::Closed {
                                        reason: "pane process exited".to_string(),
                                    });
                                }
                                break;
                            }
                            dirty = true;
                        }
                    },
                }
            }
            _ = tick.tick() => {
                if dirty && !clients.is_empty() {
                    let frame = ServerMsg::Frame { data: session.render(), full: true };
                    clients.retain(|_, tx| tx.send(frame.clone()).is_ok());
                    dirty = false;
                }
            }
        }
    }

    let _ = std::fs::remove_file(&config.socket_path);
    Ok(())
}

/// Forward one pane's output into the event loop, tagged with its pane id. Ends
/// when the pane's output channel closes (the pane is gone).
fn spawn_pane_forwarder(
    pane_id: PaneId,
    mut rx: mpsc::UnboundedReceiver<PaneOutput>,
    ev_tx: mpsc::UnboundedSender<Event>,
) {
    tokio::spawn(async move {
        while let Some(output) = rx.recv().await {
            if ev_tx.send(Event::PaneOutput { pane_id, output }).is_err() {
                break;
            }
        }
    });
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
