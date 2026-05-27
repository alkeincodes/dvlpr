pub mod socket;

use std::collections::HashMap;
use std::io;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::net::{UnixListener, UnixStream};
use tokio::sync::{mpsc, watch};
use tokio::task::JoinHandle;
use tokio::time::timeout;

use crate::compositor::{diff_rows, serialize_full, Grid};
use crate::config::Config;
use crate::input::{InputEvent, InputParser};
use crate::layout::{PaneId, SplitPath};
use crate::pane::PaneOutput;
use crate::protocol::{
    read_msg, write_msg, ClientHello, ClientMsg, ServerHello, ServerMsg, PROTOCOL_VERSION,
};
use crate::session::Session;

/// How a daemon should start: where to listen and what to run in the pane.
pub struct ServerConfig {
    pub socket_path: PathBuf,
    pub command: Vec<String>,
    pub cwd: String,
    /// Keymap to use. `None` => load from `~/.config/dvlpr/config.toml` (production
    /// default). Tests pass `Some(Config::default())` for deterministic bindings.
    pub keymap: Option<Config>,
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
            keymap: None,
        })
    }
}

type ClientId = u64;

/// Server-initiated control messages to a client's writer task. At most one is
/// ever sent per client (then the writer exits), so an unbounded channel costs
/// nothing and keeps `send` synchronous (usable from sync apply/teardown paths).
enum Control {
    Detach,
    Closed(String),
}

/// Events funneled into the single central loop.
enum Event {
    ClientConnected {
        id: ClientId,
        write_half: tokio::net::unix::OwnedWriteHalf,
        cols: u16,
        rows: u16,
    },
    ClientInput {
        id: ClientId,
        bytes: Vec<u8>,
    },
    ClientResize {
        id: ClientId,
        cols: u16,
        rows: u16,
    },
    ClientGone(ClientId),
    PaneOutput {
        pane_id: PaneId,
        output: PaneOutput,
    },
}

/// Per-connected-client central-loop state. The writer task (spawned by the
/// central loop) owns the diff baseline; here we keep the handles to drive it:
/// the control sender, the frame mailbox sender, and the writer's join handle
/// (awaited only at full-server teardown for a deterministic `Closed` flush).
struct ClientState {
    control: mpsc::UnboundedSender<Control>,
    grid_tx: watch::Sender<Arc<Grid>>,
    writer: JoinHandle<()>,
    parser: InputParser,
    /// The divider being dragged, as `(window index at press, path)`.
    drag: Option<(usize, SplitPath)>,
    escape_deadline: Option<Instant>,
    /// Monotonic activity stamp; the client with the highest stamp is the foreground.
    last_activity: u64,
    /// This client's last-known terminal size (cols, rows), clamped to >= 1 per axis.
    size: (u16, u16),
}

/// Classic ESCDELAY: how long a lone trailing ESC waits for a continuation byte
/// before being treated as a standalone Escape key.
const ESCAPE_TIMEOUT: Duration = Duration::from_millis(50);

/// Upper bound on how long teardown waits for all writers to flush `Closed`.
const TEARDOWN_TIMEOUT: Duration = Duration::from_secs(1);

/// Run the daemon to completion (until the last pane exits).
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

    let keymap = config.keymap.unwrap_or_else(Config::load);

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

    // Central loop: owns the Session; composes once per dirty tick (and on connect)
    // and publishes the latest Arc<Grid> to each client's watch mailbox. Per-client
    // writer tasks diff against their own baseline and write at their own pace.
    //
    // Single-active-writer model: one interaction-driven `foreground` client (most
    // recently connected/typed) drives the session geometry via `session.resize`; its
    // size is the composed grid's size. Other clients still receive that same grid here
    // (per-client clip/letterbox fitting is wired into the writer in a later step).
    let mut clients: HashMap<ClientId, ClientState> = HashMap::new();
    let mut dirty = false;
    let mut foreground: Option<ClientId> = None;
    let mut activity_seq: u64 = 0;
    let mut tick = tokio::time::interval(Duration::from_millis(16)); // ~60fps cap

    let reason: String = loop {
        tokio::select! {
            maybe_event = ev_rx.recv() => {
                let Some(event) = maybe_event else { break "server stopped".to_string() };
                match event {
                    Event::ClientConnected { id, write_half, cols, rows } => {
                        let (cols, rows) = (cols.max(1), rows.max(1));
                        session.resize(cols, rows);
                        let grid = Arc::new(session.compose());
                        for st in clients.values() {
                            let _ = st.grid_tx.send(grid.clone());
                        }
                        let (grid_tx, grid_rx) = watch::channel(grid.clone());
                        let (control_tx, control_rx) = mpsc::unbounded_channel::<Control>();
                        let writer = spawn_writer(id, write_half, grid_rx, control_rx, ev_tx.clone());
                        clients.insert(
                            id,
                            ClientState {
                                control: control_tx,
                                grid_tx,
                                writer,
                                parser: InputParser::new(),
                                drag: None,
                                escape_deadline: None,
                                last_activity: 0,
                                size: (cols, rows),
                            },
                        );
                        // The newcomer is the freshest activity: it becomes foreground.
                        promote(&mut clients, &mut foreground, &mut activity_seq, id);
                        dirty = false;
                    }
                    Event::ClientInput { id, bytes } => {
                        let events = match clients.get_mut(&id) {
                            Some(st) => {
                                let now = Instant::now();
                                let mut evs = Vec::new();
                                // Commit a standalone Escape whose deadline already
                                // passed BEFORE interpreting the new bytes.
                                if matches!(st.escape_deadline, Some(dl) if dl <= now) {
                                    evs.extend(st.parser.flush_escape_timeout());
                                }
                                evs.extend(st.parser.feed(&keymap, &bytes));
                                st.escape_deadline = st
                                    .parser
                                    .pending_escape()
                                    .then(|| Instant::now() + ESCAPE_TIMEOUT);
                                evs
                            }
                            None => Vec::new(),
                        };
                        commit_input(
                            &mut session, &mut clients, &mut foreground, &mut activity_seq,
                            &ev_tx, &mut dirty, id, events,
                        );
                        if session.is_empty() {
                            break "pane process exited".to_string();
                        }
                    }
                    Event::ClientResize { id, cols, rows } => {
                        let (cols, rows) = (cols.max(1), rows.max(1));
                        if let Some(st) = clients.get_mut(&id) {
                            st.size = (cols, rows);
                        }
                        // Only the foreground client's size drives the session geometry.
                        if foreground == Some(id) {
                            session.resize(cols, rows);
                            dirty = true;
                        }
                    }
                    Event::ClientGone(id) => {
                        remove_client(&mut clients, &mut foreground, &mut session, &mut dirty, id);
                    }
                    Event::PaneOutput { pane_id, output } => match output {
                        PaneOutput::Bytes(bytes) => {
                            session.feed(pane_id, &bytes);
                            dirty = true;
                        }
                        PaneOutput::Exited => {
                            for runtime in session.pane_exited(pane_id) {
                                runtime.close();
                            }
                            if session.is_empty() {
                                tracing::info!("last pane exited; shutting down server");
                                break "pane process exited".to_string();
                            }
                            dirty = true;
                        }
                    },
                }
            }
            _ = tick.tick() => {
                // Flush any client whose lone-ESC timer has elapsed.
                let now = Instant::now();
                let expired: Vec<ClientId> = clients
                    .iter()
                    .filter_map(|(id, st)| match st.escape_deadline {
                        Some(dl) if dl <= now => Some(*id),
                        _ => None,
                    })
                    .collect();
                for id in expired {
                    let events = match clients.get_mut(&id) {
                        Some(st) => {
                            st.escape_deadline = None;
                            st.parser.flush_escape_timeout()
                        }
                        None => Vec::new(),
                    };
                    // Same promote-then-route path as live keystrokes, so an ESC committed
                    // on timeout promotes the sender to foreground before reaching the pane.
                    commit_input(
                        &mut session, &mut clients, &mut foreground, &mut activity_seq,
                        &ev_tx, &mut dirty, id, events,
                    );
                }
                if dirty && !clients.is_empty() {
                    let grid = Arc::new(session.compose());
                    for st in clients.values() {
                        let _ = st.grid_tx.send(grid.clone());
                    }
                    dirty = false;
                }
            }
        }
    };

    // Deterministic teardown: enqueue `Closed` on every (unbounded) control channel,
    // drop the watch senders (by dropping the client entries), then await the writer
    // handles under one aggregate timeout so `Closed` is actually flushed. The biased
    // writer select guarantees `Closed` is written before the watch-closed exit.
    let writers = close_all(&mut clients, &reason);
    let _ = timeout(TEARDOWN_TIMEOUT, async {
        for w in writers {
            let _ = w.await;
        }
    })
    .await;

    for runtime in session.shutdown() {
        runtime.close();
    }
    let _ = std::fs::remove_file(&config.socket_path);
    Ok(())
}

/// Promote `id` to foreground: bump its activity stamp and point `foreground` at it.
/// No-op (returns false) if `id` is not a connected client. Returns whether the
/// foreground id actually changed.
fn promote(
    clients: &mut HashMap<ClientId, ClientState>,
    foreground: &mut Option<ClientId>,
    activity_seq: &mut u64,
    id: ClientId,
) -> bool {
    let Some(st) = clients.get_mut(&id) else {
        return false;
    };
    *activity_seq += 1;
    st.last_activity = *activity_seq;
    let changed = *foreground != Some(id);
    *foreground = Some(id);
    changed
}

/// Pick the remaining client with the highest activity stamp as the new foreground and
/// resize the session to its size. With no clients left, clears `foreground` and leaves
/// the geometry as-is (the daemon persists headless until the next connect resizes it).
fn promote_latest_remaining(
    clients: &HashMap<ClientId, ClientState>,
    foreground: &mut Option<ClientId>,
    session: &mut Session,
    dirty: &mut bool,
) {
    let next = clients
        .iter()
        .max_by_key(|(_, st)| st.last_activity)
        .map(|(id, _)| *id);
    *foreground = next;
    if let Some(st) = next.and_then(|id| clients.get(&id)) {
        session.resize(st.size.0, st.size.1);
        *dirty = true;
    }
}

/// Remove a client from the map. If it was the foreground, re-promote the
/// most-recently-active survivor (and resize). Returns the removed `ClientState` (so the
/// caller can flush its writer), or `None` if `id` was already gone.
fn remove_client(
    clients: &mut HashMap<ClientId, ClientState>,
    foreground: &mut Option<ClientId>,
    session: &mut Session,
    dirty: &mut bool,
    id: ClientId,
) -> Option<ClientState> {
    let was_foreground = *foreground == Some(id);
    let removed = clients.remove(&id);
    if removed.is_some() && was_foreground {
        *foreground = None;
        promote_latest_remaining(clients, foreground, session, dirty);
    }
    removed
}

/// Commit decoded input events for client `id`: promote the sender if the parse yielded
/// any events (interaction), resizing the session when the foreground changes, then route
/// the events into the session. Used by BOTH `ClientInput` and the tick-time ESC flush.
#[allow(clippy::too_many_arguments)]
fn commit_input(
    session: &mut Session,
    clients: &mut HashMap<ClientId, ClientState>,
    foreground: &mut Option<ClientId>,
    activity_seq: &mut u64,
    ev_tx: &mpsc::UnboundedSender<Event>,
    dirty: &mut bool,
    id: ClientId,
    events: Vec<InputEvent>,
) {
    if !events.is_empty() && promote(clients, foreground, activity_seq, id) {
        if let Some(st) = clients.get(&id) {
            session.resize(st.size.0, st.size.1);
        }
        *dirty = true;
    }
    apply_events(session, clients, foreground, ev_tx, id, events, dirty);
}

/// Route decoded input events for client `id` into the session, performing command
/// side effects (attach forwarders for new panes, async-close removed runtimes,
/// detach the issuing client).
fn apply_events(
    session: &mut Session,
    clients: &mut HashMap<ClientId, ClientState>,
    foreground: &mut Option<ClientId>,
    ev_tx: &mpsc::UnboundedSender<Event>,
    id: ClientId,
    events: Vec<InputEvent>,
    dirty: &mut bool,
) {
    for ev in events {
        match ev {
            InputEvent::Pane(bytes) => session.input(&bytes),
            InputEvent::Mouse(m) => {
                if let Some(st) = clients.get_mut(&id) {
                    session.handle_mouse(m, &mut st.drag);
                }
            }
            InputEvent::Command(cmd) => {
                let eff = session.apply_command(cmd);
                for (pane_id, rx) in eff.spawned {
                    spawn_pane_forwarder(pane_id, rx, ev_tx.clone());
                }
                for runtime in eff.closed {
                    runtime.close();
                }
                if eff.detach {
                    // NB: if this detaches the issuing client, later events in this same
                    // vector that look up the client (e.g. Mouse) become no-ops — acceptable.
                    // Funnel through remove_client so the foreground is re-promoted if
                    // the detaching client was driving. Then bound the writer flush in a
                    // self-reaping task (the biased writer writes Detach before exiting).
                    if let Some(st) = remove_client(clients, foreground, session, dirty, id) {
                        let _ = st.control.send(Control::Detach);
                        let mut writer = st.writer;
                        tokio::spawn(async move {
                            // Dropping a JoinHandle does NOT cancel the task, so a writer
                            // wedged in write_msg could outlive the timeout. Abort it.
                            if timeout(TEARDOWN_TIMEOUT, &mut writer).await.is_err() {
                                writer.abort();
                            }
                        });
                    }
                }
            }
        }
        *dirty = true;
    }
}

/// Drain all clients, enqueueing `Closed` on each control channel and collecting
/// their writer handles for the caller to await. Dropping each `ClientState` drops
/// its watch sender; the biased writer still processes the queued `Closed` first.
fn close_all(clients: &mut HashMap<ClientId, ClientState>, reason: &str) -> Vec<JoinHandle<()>> {
    let mut writers = Vec::new();
    for (_, st) in clients.drain() {
        let _ = st.control.send(Control::Closed(reason.to_string()));
        writers.push(st.writer);
    }
    writers
}

/// Per-client writer task: emit the first full frame from the seeded grid, then on
/// each grid update send a per-row diff (or a full frame on a dimension change).
/// A biased `select!` polls the control channel first so `Detach`/`Closed` always
/// win over a concurrently-closed watch channel.
fn spawn_writer(
    id: ClientId,
    mut write_half: tokio::net::unix::OwnedWriteHalf,
    mut grid_rx: watch::Receiver<Arc<Grid>>,
    mut control: mpsc::UnboundedReceiver<Control>,
    ev_tx: mpsc::UnboundedSender<Event>,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        let mut last: Arc<Grid> = grid_rx.borrow_and_update().clone();
        if write_msg(
            &mut write_half,
            &ServerMsg::Frame {
                data: serialize_full(last.as_ref()),
                full: true,
            },
        )
        .await
        .is_err()
        {
            // Proactive prune: tell the loop this client is gone (the reader may be
            // stalled). remove_client de-dups against the reader's own ClientGone.
            let _ = ev_tx.send(Event::ClientGone(id));
            return;
        }
        loop {
            tokio::select! {
                biased;
                ctrl = control.recv() => match ctrl {
                    Some(Control::Detach) => {
                        let _ = write_msg(&mut write_half, &ServerMsg::Detach).await;
                        return;
                    }
                    Some(Control::Closed(reason)) => {
                        let _ = write_msg(&mut write_half, &ServerMsg::Closed { reason }).await;
                        return;
                    }
                    None => return,
                },
                changed = grid_rx.changed() => {
                    if changed.is_err() {
                        return;
                    }
                    let next: Arc<Grid> = grid_rx.borrow_and_update().clone();
                    let resized = next.dims() != last.dims();
                    let data = if resized {
                        serialize_full(next.as_ref())
                    } else {
                        diff_rows(last.as_ref(), next.as_ref())
                    };
                    if !data.is_empty()
                        && write_msg(&mut write_half, &ServerMsg::Frame { data, full: resized })
                            .await
                            .is_err()
                    {
                        let _ = ev_tx.send(Event::ClientGone(id));
                        return;
                    }
                    last = next;
                }
            }
        }
    })
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

/// Per-client connection: handshake, hand the write half to the central loop via
/// `ClientConnected`, then run the reader loop forwarding client messages.
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

        // Hand the write half to the central loop, which spawns the writer task.
        if ev_tx
            .send(Event::ClientConnected {
                id,
                write_half,
                cols: hello.cols,
                rows: hello.rows,
            })
            .is_err()
        {
            return;
        }

        // Reader half: forward client messages into the event loop.
        loop {
            match read_msg::<_, ClientMsg>(&mut read_half).await {
                Ok(Some(ClientMsg::Input(bytes))) => {
                    let _ = ev_tx.send(Event::ClientInput { id, bytes });
                }
                Ok(Some(ClientMsg::Resize { cols, rows })) => {
                    let _ = ev_tx.send(Event::ClientResize { id, cols, rows });
                }
                Ok(None) | Err(_) => break,
            }
        }
        let _ = ev_tx.send(Event::ClientGone(id));
    });
}
