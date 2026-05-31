pub mod socket;

use std::collections::HashMap;
use std::io;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::net::{UnixListener, UnixStream};
use tokio::sync::{mpsc, oneshot, watch};
use tokio::task::JoinHandle;
use tokio::time::timeout;

use crate::compositor::{diff_rows, fit, serialize_full, Grid};
use crate::config::Config;
use crate::input::{InputEvent, InputParser};
use crate::layout::{PaneId, SplitPath};
use crate::pane::PaneOutput;
use crate::protocol::{
    read_msg, write_msg, ClientHello, ClientMsg, Intent, ServerHello, ServerMsg, StatusInfo,
    PROTOCOL_VERSION,
};
use crate::session::Session;

/// Per-frame payload published on the central loop's watch channel.
/// Carries the composed grid plus the menu-open bit so each per-client
/// writer can edge-trigger `\x1b[?1003h` / `\x1b[?1003l` framing. The
/// type is purely internal — never serialized over the wire.
#[derive(Clone, Debug)]
pub struct FrameSnapshot {
    pub grid: Grid,
    pub menu_open: bool,
}

/// Compute the `?1003h` / `?1003l` prefix bytes for a single per-writer
/// edge transition. Extracted so the writer's body stays a thin
/// orchestration shell and the edge logic is unit-testable in isolation.
fn one_oh_three_edge_prefix(last: bool, next: bool) -> &'static [u8] {
    match (last, next) {
        (false, true) => b"\x1b[?1003h",
        (true, false) => b"\x1b[?1003l",
        _ => b"",
    }
}

/// How a daemon should start: where to listen and what to run in the pane.
pub struct ServerConfig {
    pub socket_path: PathBuf,
    pub command: Vec<String>,
    pub cwd: String,
    /// Keymap to use. `None` => load from disk via `Config::load` (production
    /// default; see `config::config_path_candidates` for the resolution order).
    /// Tests pass `Some(Config::default())` for deterministic bindings.
    pub keymap: Option<Config>,
    /// The session name this daemon serves (its socket is `{session}.sock`).
    pub session: String,
}

/// Resolve a session's initial spawn directory: the client's launch dir
/// (`DVLPR_SESSION_CWD`) when it is set and still an existing directory, else
/// `$HOME`, else `"."`. Empty strings are treated as unset. The existence check
/// happens here — once, at daemon startup, off the async spawn path — so a
/// stale/deleted launch dir degrades to `$HOME` rather than failing pane spawns
/// (and without touching the per-pane spawn hot path). `$HOME` is trusted and
/// returned unchecked, matching the previous behavior.
fn captured_cwd(dvlpr_cwd: Option<String>, home: Option<String>) -> String {
    let home = home.filter(|s| !s.is_empty());
    if let Some(c) = dvlpr_cwd.filter(|s| !s.is_empty()) {
        if std::path::Path::new(&c).is_dir() {
            return c;
        }
    }
    home.unwrap_or_else(|| ".".to_string())
}

impl ServerConfig {
    pub fn for_session(name: &str) -> io::Result<Self> {
        socket::validate_session_name(name)?;
        let dir = socket::runtime_dir();
        socket::ensure_runtime_dir(&dir)?;
        let socket_path = socket::socket_path_in(&dir, name);
        socket::check_sun_path_len(&socket_path)?;
        Ok(ServerConfig {
            socket_path,
            command: Vec::new(), // empty => default shell
            // The client hands us its launch directory in DVLPR_SESSION_CWD (see
            // daemon::spawn_detached_server) so the session's shells open where
            // the user ran `dvlpr`. We only read it here — the var is stripped
            // from each PTY child's environment in PaneRuntime::spawn so it never
            // leaks into user shells, which keeps this a pure read (no global env
            // mutation that would race the test suite).
            cwd: captured_cwd(
                std::env::var("DVLPR_SESSION_CWD").ok(),
                std::env::var("HOME").ok(),
            ),
            keymap: None,
            session: name.to_string(),
        })
    }

    pub fn for_default_session() -> io::Result<Self> {
        Self::for_session("default")
    }
}

type ClientId = u64;

/// Server-initiated control messages to a client's writer task. `Resize` is sent
/// whenever the client's terminal size changes (so the writer re-fits its view and
/// repaints); `Detach`/`Closed` are terminal (the writer exits after writing them).
/// The unbounded channel keeps `send` synchronous for the sync resize/teardown paths.
enum Control {
    Detach,
    Closed(String),
    Resize(u16, u16),
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
    /// A management client asked for session status (answered without attaching).
    StatusRequest(oneshot::Sender<StatusInfo>),
    /// A control client asked to apply a `ControlCommand` (answered without attaching).
    ControlCommand {
        cmd: crate::protocol::ControlCommand,
        reply: oneshot::Sender<crate::protocol::CommandReply>,
    },
    /// A management client (or `kill`) asked the daemon to shut down.
    Shutdown,
}

/// Per-connected-client central-loop state. The writer task (spawned by the
/// central loop) owns the diff baseline; here we keep the handles to drive it:
/// the control sender, the frame mailbox sender, and the writer's join handle
/// (awaited only at full-server teardown for a deterministic `Closed` flush).
struct ClientState {
    control: mpsc::UnboundedSender<Control>,
    grid_tx: watch::Sender<Arc<FrameSnapshot>>,
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
    let runtime_cfg: Config = config.keymap.unwrap_or_else(Config::load);
    // Extract the Copy-able theme before moving runtime_cfg into `keymap` below.
    let initial_theme = runtime_cfg.theme;

    let restore_snap = (std::env::var("DVLPR_RESTORE").as_deref() == Ok("1"))
        .then(|| crate::persist::load(&crate::persist::snapshot_path(&config.session)))
        .flatten();

    let (mut session, initial_rxs): (Session, Vec<(PaneId, mpsc::UnboundedReceiver<PaneOutput>)>) =
        if let Some(snap) = restore_snap {
            let (s, rxs) = Session::restore(
                snap,
                80,
                24,
                initial_theme,
                runtime_cfg.prefix,
                runtime_cfg.keys,
            )?;
            (s, rxs)
        } else {
            let (s, first_pane, first_rx) = Session::new(
                config.session.clone(),
                config.command.clone(),
                config.cwd.clone(),
                80,
                24,
                initial_theme,
                runtime_cfg.prefix,
                runtime_cfg.keys,
                runtime_cfg.sidebar.width,
            )?;
            (s, vec![(first_pane, first_rx)])
        };
    // Wire a forwarder per spawned pane (replaces the single first-pane wiring).
    for (pane_id, rx) in initial_rxs {
        spawn_pane_forwarder(pane_id, rx, ev_tx.clone());
    }

    let keymap = runtime_cfg;

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
    // size is the composed grid's size. Each client's writer then fits that one grid to
    // its own terminal (clip if smaller, letterbox if larger) before serializing.
    let mut clients: HashMap<ClientId, ClientState> = HashMap::new();
    let mut dirty = false;
    let mut foreground: Option<ClientId> = None;
    let mut activity_seq: u64 = 0;
    let mut tick = tokio::time::interval(Duration::from_millis(16)); // ~60fps cap
    let mut autoname_tick = tokio::time::interval(Duration::from_secs(1));
    let mut agent_tick = tokio::time::interval(Duration::from_millis(500));
    agent_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut snapshot_tick = tokio::time::interval(Duration::from_secs(1));
    snapshot_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let snapshot_file = crate::persist::snapshot_path(&config.session);
    let sound_debouncer = crate::sound::SoundDebouncer::new();

    let reason: String = loop {
        tokio::select! {
            maybe_event = ev_rx.recv() => {
                let Some(event) = maybe_event else { break "server stopped".to_string() };
                match event {
                    Event::ClientConnected { id, write_half, cols, rows } => {
                        let (cols, rows) = (cols.max(1), rows.max(1));
                        session.resize(cols, rows);
                        // Make the very first frame show real process names.
                        session.refresh_window_names(crate::procinfo::process_name);
                        let snapshot = Arc::new(FrameSnapshot { grid: session.compose(), menu_open: session.menu_open() });
                        for st in clients.values() {
                            let _ = st.grid_tx.send(snapshot.clone());
                        }
                        let (grid_tx, grid_rx) = watch::channel(snapshot.clone());
                        let (control_tx, control_rx) = mpsc::unbounded_channel::<Control>();
                        let writer = spawn_writer(id, cols, rows, write_half, grid_rx, control_rx, ev_tx.clone());
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
                            let _ = st.control.send(Control::Resize(cols, rows));
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
                    Event::StatusRequest(reply) => {
                        let _ = reply.send(StatusInfo {
                            windows: session.window_count() as u32,
                            clients: clients.len() as u32,
                        });
                    }
                    Event::ControlCommand { cmd, reply } => {
                        let result = apply_control_command(&mut session, cmd, &ev_tx, &mut dirty);
                        let _ = reply.send(result);
                    }
                    Event::Shutdown => break "killed".to_string(),
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
                // Defensive: never compose a window-less session. Control commands
                // refuse to close the last pane/window, so they can't empty it; this
                // guards the synchronous keystroke/last-pane-exit teardown path, where
                // rendering zero windows is pointless and a panic risk (compose indexes
                // self.windows[self.active_window] unconditionally).
                if dirty && !clients.is_empty() && !session.is_empty() {
                    let snapshot = Arc::new(FrameSnapshot { grid: session.compose(), menu_open: session.menu_open() });
                    for st in clients.values() {
                        let _ = st.grid_tx.send(snapshot.clone());
                    }
                    dirty = false;
                }
            }
            _ = autoname_tick.tick() => {
                if session.refresh_window_names(crate::procinfo::process_name) {
                    dirty = true;
                }
            }
            _ = snapshot_tick.tick() => {
                let volatile_changed = session.refresh_restore_meta(
                    crate::procinfo::pid_cwd,
                    crate::procinfo::agent_transcripts,
                    crate::procinfo::proc_start_time,
                );
                if session.take_snapshot_dirty() || volatile_changed {
                    if let Err(e) = crate::persist::write_atomic(&snapshot_file, &session.snapshot()) {
                        tracing::warn!("snapshot write failed: {e}");
                        session.request_snapshot(); // retry on the next tick
                    }
                }
            }
            _ = agent_tick.tick() => {
                let outcome = session.refresh_agent_states(crate::procinfo::process_name);
                let meta_changed = session.refresh_agent_meta(crate::procinfo::pid_cwd);
                if outcome.changed || meta_changed {
                    dirty = true;
                }
                if !outcome.blocked_transitions.is_empty()
                    && keymap.sound.enabled
                    && sound_debouncer.try_fire()
                {
                    crate::sound::play_blocked(&keymap.sound);
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
    crate::persist::delete(&crate::persist::snapshot_path(&config.session));
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

/// Apply the parts of a `CommandEffect` that are identical for every caller:
/// spawn output forwarders for new panes, tear down closed panes, and mark the
/// session dirty so the next tick recomposes + broadcasts. Returns whether a
/// detach was requested; the caller handles detach (it is client-specific).
fn apply_command_effect(
    eff: crate::session::CommandEffect,
    ev_tx: &mpsc::UnboundedSender<Event>,
    dirty: &mut bool,
) -> bool {
    for (pane_id, rx) in eff.spawned {
        spawn_pane_forwarder(pane_id, rx, ev_tx.clone());
    }
    for runtime in eff.closed {
        runtime.close();
    }
    *dirty = true;
    eff.detach
}

/// Map a `ControlCommand` onto a session mutation, apply its effect through the
/// shared helper, and compute the reply. Runs in the main loop with the live
/// `session`, so it can range-check against current window state.
fn apply_control_command(
    session: &mut Session,
    cmd: crate::protocol::ControlCommand,
    ev_tx: &mpsc::UnboundedSender<Event>,
    dirty: &mut bool,
) -> crate::protocol::CommandReply {
    use crate::config::Command;
    use crate::protocol::{CommandReply, ControlCommand as C, SplitDir};

    let ok = || CommandReply {
        ok: true,
        message: None,
    };
    let err = |m: String| CommandReply {
        ok: false,
        message: Some(m),
    };
    let mut eff = crate::session::CommandEffect::default();

    let reply = match cmd {
        C::PaneSplit(SplitDir::Right) => {
            eff = session.apply_command(Command::SplitVertical);
            ok()
        }
        C::PaneSplit(SplitDir::Down) => {
            eff = session.apply_command(Command::SplitHorizontal);
            ok()
        }
        C::PaneClose => {
            if session.window_count() == 1 && session.active_pane_count() == 1 {
                return err(
                    "cannot close the last pane via control; use 'dvlpr kill' to stop the session"
                        .into(),
                );
            }
            eff = session.apply_command(Command::ClosePane);
            ok()
        }
        C::PaneZoom => {
            eff = session.apply_command(Command::ToggleZoom);
            ok()
        }
        C::SidebarToggle => {
            eff = session.apply_command(Command::ToggleSidebar);
            ok()
        }
        C::WindowNext => {
            eff = session.apply_command(Command::NextWindow);
            ok()
        }
        C::WindowPrev => {
            eff = session.apply_command(Command::PrevWindow);
            ok()
        }
        C::WindowSelect(n) => {
            let count = session.window_count();
            if n >= 1 && (n as usize) <= count {
                eff = session.apply_command(Command::SelectWindow(n as usize));
                ok()
            } else {
                err(format!("window {n} out of range (1..={count})"))
            }
        }
        C::WindowNew { name } => {
            eff = session.new_named_window(name);
            ok()
        }
        C::WindowRename(name) => {
            if session.window_count() == 0 {
                err("no active window to rename".into())
            } else {
                session.rename_active_window(name);
                ok()
            }
        }
        C::WindowClose => {
            if session.window_count() == 0 {
                err("no active window to close".into())
            } else if session.window_count() == 1 {
                err("cannot close the last window via control; use 'dvlpr kill' to stop the session".into())
            } else {
                eff.closed = session.close_active_window();
                ok()
            }
        }
    };

    apply_command_effect(eff, ev_tx, dirty);
    reply
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
        if let Some(eff) = session.try_consume_help_event(&ev) {
            // Help effects are always default (scroll/tab/dismiss only); the
            // spawned/closed/detach handling below is inert today but mirrors the
            // menu branch for parity if help ever dispatches commands.
            for (pane_id, rx) in eff.spawned {
                spawn_pane_forwarder(pane_id, rx, ev_tx.clone());
            }
            for runtime in eff.closed {
                runtime.close();
            }
            if eff.detach {
                if let Some(st) = remove_client(clients, foreground, session, dirty, id) {
                    let _ = st.control.send(Control::Detach);
                    let mut writer = st.writer;
                    tokio::spawn(async move {
                        if timeout(TEARDOWN_TIMEOUT, &mut writer).await.is_err() {
                            writer.abort();
                        }
                    });
                }
            }
            if session.refresh_window_names(crate::procinfo::process_name) {
                *dirty = true;
            }
            *dirty = true;
            continue;
        }
        if let Some(eff) = session.try_consume_dialog_event(&ev) {
            for (pane_id, rx) in eff.spawned {
                spawn_pane_forwarder(pane_id, rx, ev_tx.clone());
            }
            for runtime in eff.closed {
                runtime.close();
            }
            if eff.detach {
                if let Some(st) = remove_client(clients, foreground, session, dirty, id) {
                    let _ = st.control.send(Control::Detach);
                    let mut writer = st.writer;
                    tokio::spawn(async move {
                        if timeout(TEARDOWN_TIMEOUT, &mut writer).await.is_err() {
                            writer.abort();
                        }
                    });
                }
            }
            if session.refresh_window_names(crate::procinfo::process_name) {
                *dirty = true;
            }
            *dirty = true;
            continue;
        }
        if let Some(eff) = session.try_consume_menu_event(&ev) {
            for (pane_id, rx) in eff.spawned {
                spawn_pane_forwarder(pane_id, rx, ev_tx.clone());
            }
            for runtime in eff.closed {
                runtime.close();
            }
            if eff.detach {
                if let Some(st) = remove_client(clients, foreground, session, dirty, id) {
                    let _ = st.control.send(Control::Detach);
                    let mut writer = st.writer;
                    tokio::spawn(async move {
                        if timeout(TEARDOWN_TIMEOUT, &mut writer).await.is_err() {
                            writer.abort();
                        }
                    });
                }
            }
            if session.refresh_window_names(crate::procinfo::process_name) {
                *dirty = true;
            }
            *dirty = true;
            continue;
        }
        match ev {
            InputEvent::Pane(bytes) => session.input(&bytes),
            InputEvent::Mouse(m) => {
                let eff = if let Some(st) = clients.get_mut(&id) {
                    session.handle_mouse(m, &mut st.drag)
                } else {
                    crate::session::CommandEffect::default()
                };
                for (pane_id, rx) in eff.spawned {
                    spawn_pane_forwarder(pane_id, rx, ev_tx.clone());
                }
                for runtime in eff.closed {
                    runtime.close();
                }
                if eff.detach {
                    if let Some(st) = remove_client(clients, foreground, session, dirty, id) {
                        let _ = st.control.send(Control::Detach);
                        let mut writer = st.writer;
                        tokio::spawn(async move {
                            if timeout(TEARDOWN_TIMEOUT, &mut writer).await.is_err() {
                                writer.abort();
                            }
                        });
                    }
                }
                if session.refresh_window_names(crate::procinfo::process_name) {
                    *dirty = true;
                }
            }
            InputEvent::Command(cmd) => {
                let eff = session.apply_command(cmd);
                let detach = apply_command_effect(eff, ev_tx, dirty);
                if detach {
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
                if session.refresh_window_names(crate::procinfo::process_name) {
                    *dirty = true;
                }
            }
            InputEvent::FocusIn => {
                // Promotion is already handled by commit_input's
                // !events.is_empty() check above (the parser returning a non-empty
                // vec is what flags this client as recently-active). No pane write,
                // no other side effect. DO NOT add `session.input(b"\x1b[I")` here
                // — that would write focus bytes into the pane PTY, the exact
                // leak the parser intercept is fixing.
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
    cols: u16,
    rows: u16,
    mut write_half: tokio::net::unix::OwnedWriteHalf,
    mut grid_rx: watch::Receiver<Arc<FrameSnapshot>>,
    mut control: mpsc::UnboundedReceiver<Control>,
    ev_tx: mpsc::UnboundedSender<Event>,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        let mut view = (cols.max(1), rows.max(1));
        let first: Arc<FrameSnapshot> = grid_rx.borrow_and_update().clone();
        let mut last_src: Arc<Grid> = Arc::new(first.grid.clone());
        let mut last_fitted = fit(last_src.as_ref(), view.0, view.1);
        let first_prefix: &[u8] = if first.menu_open { b"\x1b[?1003h" } else { b"" };
        let first_body = serialize_full(&last_fitted);
        let mut first_data = Vec::with_capacity(first_prefix.len() + first_body.len());
        first_data.extend_from_slice(first_prefix);
        first_data.extend_from_slice(&first_body);
        if write_msg(
            &mut write_half,
            &ServerMsg::Frame {
                data: first_data,
                full: true,
            },
        )
        .await
        .is_err()
        {
            let _ = ev_tx.send(Event::ClientGone(id));
            return;
        }
        let mut last_menu_open: bool = first.menu_open;
        loop {
            tokio::select! {
                biased;
                ctrl = control.recv() => match ctrl {
                    Some(Control::Detach) => {
                        let _ = write_msg(
                            &mut write_half,
                            &ServerMsg::Frame { data: b"\x1b[?1003l".to_vec(), full: false },
                        )
                        .await;
                        let _ = write_msg(&mut write_half, &ServerMsg::Detach).await;
                        return;
                    }
                    Some(Control::Closed(reason)) => {
                        let _ = write_msg(
                            &mut write_half,
                            &ServerMsg::Frame { data: b"\x1b[?1003l".to_vec(), full: false },
                        )
                        .await;
                        let _ = write_msg(&mut write_half, &ServerMsg::Closed { reason }).await;
                        return;
                    }
                    Some(Control::Resize(c, r)) => {
                        view = (c.max(1), r.max(1));
                        last_fitted = fit(last_src.as_ref(), view.0, view.1);
                        if write_msg(
                            &mut write_half,
                            &ServerMsg::Frame { data: serialize_full(&last_fitted), full: true },
                        )
                        .await
                        .is_err()
                        {
                            let _ = ev_tx.send(Event::ClientGone(id));
                            return;
                        }
                    }
                    None => return,
                },
                changed = grid_rx.changed() => {
                    if changed.is_err() { return; }
                    let next: Arc<FrameSnapshot> = grid_rx.borrow_and_update().clone();
                    let next_src = Arc::new(next.grid.clone());
                    let resized = next_src.dims() != last_src.dims();
                    let fitted = fit(next_src.as_ref(), view.0, view.1);
                    let prefix: &[u8] = one_oh_three_edge_prefix(last_menu_open, next.menu_open);
                    last_menu_open = next.menu_open;
                    let body = if resized {
                        serialize_full(&fitted)
                    } else {
                        diff_rows(&last_fitted, &fitted)
                    };
                    if prefix.is_empty() && body.is_empty() {
                        // Nothing to send.
                    } else {
                        let mut data = Vec::with_capacity(prefix.len() + body.len());
                        data.extend_from_slice(prefix);
                        data.extend_from_slice(&body);
                        if write_msg(&mut write_half, &ServerMsg::Frame { data, full: resized })
                            .await
                            .is_err()
                        {
                            let _ = ev_tx.send(Event::ClientGone(id));
                            return;
                        }
                    }
                    last_src = next_src;
                    last_fitted = fitted;
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

        // Handshake: read the intent-carrying hello.
        let hello: ClientHello = match read_msg(&mut read_half).await {
            Ok(Some(h)) => h,
            Ok(None) => return, // clean disconnect before handshake
            Err(e) => {
                // A pre-v4 client's hello will not even decode into the new shape; close.
                tracing::warn!(error = %e, "client handshake read failed");
                return;
            }
        };

        // Version guard. For Attach we reply Reject (a decodable v4 hello with the wrong
        // version); for management intents we just close without acting.
        if hello.protocol_version != PROTOCOL_VERSION {
            tracing::warn!(
                client = hello.protocol_version,
                server = PROTOCOL_VERSION,
                "rejecting client: protocol mismatch"
            );
            if matches!(hello.intent, Intent::Attach { .. }) {
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
            }
            return;
        }

        match hello.intent {
            Intent::Status => {
                let (tx, rx) = oneshot::channel();
                if ev_tx.send(Event::StatusRequest(tx)).is_err() {
                    return;
                }
                if let Ok(info) = rx.await {
                    let _ = write_msg(&mut write_half, &info).await;
                }
            }
            Intent::Kill => {
                let _ = ev_tx.send(Event::Shutdown);
            }
            Intent::Command => {
                if let Ok(Some(ClientMsg::Command(cmd))) =
                    read_msg::<_, ClientMsg>(&mut read_half).await
                {
                    let (tx, rx) = oneshot::channel();
                    if ev_tx
                        .send(Event::ControlCommand { cmd, reply: tx })
                        .is_err()
                    {
                        return;
                    }
                    if let Ok(result) = rx.await {
                        let _ = write_msg(&mut write_half, &result).await;
                    }
                }
                // empty/garbage/non-command frame: close without replying
            }
            Intent::Attach { cols, rows } => {
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
                        cols,
                        rows,
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
                        Ok(Some(ClientMsg::Command(_))) => {
                            // A control command on an attach stream is unexpected — the
                            // control CLI uses a dedicated `Intent::Command` connection.
                            // Ignore it.
                        }
                        Ok(None) | Err(_) => break,
                    }
                }
                let _ = ev_tx.send(Event::ClientGone(id));
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn for_session_rejects_an_invalid_name() {
        // Validation runs first, so no runtime dir or socket is created.
        assert!(ServerConfig::for_session("bad/name").is_err());
        assert!(ServerConfig::for_session("").is_err());
    }

    #[test]
    fn for_session_sets_name_and_socket_path() {
        let cfg = ServerConfig::for_session("okname").unwrap();
        assert_eq!(cfg.session, "okname");
        assert!(
            cfg.socket_path.ends_with("okname.sock"),
            "socket path should be <runtime>/okname.sock, got {:?}",
            cfg.socket_path
        );
    }

    #[test]
    fn captured_cwd_uses_existing_launch_dir_else_home_else_dot() {
        let tmp = std::env::temp_dir().to_string_lossy().to_string();
        // An existing launch dir wins.
        assert_eq!(captured_cwd(Some(tmp.clone()), Some("/home".into())), tmp);
        // A missing launch dir degrades to $HOME (returned unchecked).
        assert_eq!(
            captured_cwd(Some("/nonexistent/dvlpr-xyz".into()), Some("/home".into())),
            "/home"
        );
        // Empty / absent launch dir → $HOME.
        assert_eq!(captured_cwd(Some("".into()), Some("/home".into())), "/home");
        assert_eq!(captured_cwd(None, Some("/home".into())), "/home");
        // No usable home → ".".
        assert_eq!(captured_cwd(None, Some("".into())), ".");
        assert_eq!(captured_cwd(None, None), ".");
    }

    use crate::compositor::Grid;

    #[test]
    fn frame_snapshot_carries_menu_open_bit_alongside_grid() {
        let g = Grid {
            cols: 10,
            rows: 5,
            cells: vec![],
            cursor: (0, 0),
        };
        let s = FrameSnapshot {
            grid: g.clone(),
            menu_open: false,
        };
        assert!(!s.menu_open);
        let s2 = FrameSnapshot {
            grid: g,
            menu_open: true,
        };
        assert!(s2.menu_open);
    }

    #[test]
    fn one_oh_three_edge_prefix_emits_h_on_false_to_true_transition() {
        assert_eq!(one_oh_three_edge_prefix(false, true), b"\x1b[?1003h");
    }

    #[test]
    fn one_oh_three_edge_prefix_emits_l_on_true_to_false_transition() {
        assert_eq!(one_oh_three_edge_prefix(true, false), b"\x1b[?1003l");
    }

    #[test]
    fn one_oh_three_edge_prefix_emits_empty_when_unchanged_false() {
        assert_eq!(one_oh_three_edge_prefix(false, false), b"");
    }

    #[test]
    fn one_oh_three_edge_prefix_emits_empty_when_unchanged_true() {
        assert_eq!(one_oh_three_edge_prefix(true, true), b"");
    }
}
