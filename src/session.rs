//! The daemon's session state: windows (each a split-tree of panes), the pane
//! map (PTY runtime + screen per pane), and the compositor that renders the
//! active window. This is the integration point between the pure `layout`/
//! `compositor` modules and the live `PaneRuntime`/`GhosttyScreen` resources.
//!
//! Step 3 runs a single window with a single pane (behavior-preserving); the
//! split/window commands that exercise the N-pane machinery arrive in Step 4.

use std::collections::HashMap;
use std::io;

use tokio::sync::mpsc;

use crate::compositor::{Compositor, PaneCells};
use crate::config::Command;
use crate::detect;
use crate::ghostty::screen::GhosttyScreen;
use crate::input::{MouseEvent, MouseKind};
use crate::layout::{self, Node, PaneId, Rect, SplitDir, SplitPath};
use crate::pane::{PaneOutput, PaneRuntime};

struct Pane {
    runtime: PaneRuntime,
    screen: GhosttyScreen,
    /// Foreground pid we last successfully resolved an agent for. None
    /// until the first successful resolve. Cache key: when this differs
    /// from the current foreground pid, the cached `agent` is stale.
    /// INVARIANT: agent_id_pid == Some(p) implies `agent` came from
    /// successfully resolving the friendly name of pid p.
    agent_id_pid: Option<i32>,
    /// The cached agent classification of this pane's foreground process.
    /// None = not a known agent (no classification).
    agent: Option<detect::Agent>,
    /// The pane's currently displayed agent state (after stabilization).
    /// Idle is the default and applies to non-agent panes too.
    agent_state: detect::AgentState,
    /// Counter for the idle stabilizer: consecutive idle samples since
    /// the pane last classified as non-Idle. Resets to 0 on any non-Idle
    /// classification; clamped at 2.
    idle_streak: u8,
    /// Cached tmux/multiplexer session label for the sidebar, populated by
    /// `refresh_agent_meta`. None until the first successful fetch.
    session_label: Option<String>,
    /// Cached VCS branch for the sidebar, populated by `refresh_agent_meta`.
    /// None until the first successful fetch.
    branch: Option<String>,
    /// Timestamp of the last successful `refresh_agent_meta` run for this
    /// pane. Initialised 60 s in the past so the first tick fires immediately.
    meta_last_refresh: std::time::Instant,
    /// Set of `ErrorKind`s already logged for this pane's meta-refresh, so
    /// we don't spam the log on every tick for a persistent failure.
    #[allow(dead_code)]
    meta_err_seen: std::collections::HashSet<std::io::ErrorKind>,
    /// Pane's last-known cwd, captured for every pane (agent or not) on the
    /// snapshot cadence. None until first resolve.
    cwd: Option<String>,
    /// Agent-resume identity captured lazily from the foreground process's open
    /// transcript. None until captured (or non-agent).
    agent_resume: crate::persist::AgentResume,
    /// Foreground pid the `agent_resume` capture is keyed to; recapture when the
    /// foreground pid changes.
    captured_for_pid: Option<i32>,
}

struct Window {
    name: String,
    /// When true, `name` was set explicitly by the user and the periodic
    /// auto-namer (`refresh_window_names`) must not overwrite it.
    name_pinned: bool,
    root: Node,
    focused: PaneId,
    zoomed: bool,
}

/// One sidebar row's data. Public so `Compositor::draw_sidebar` can
/// receive a slice; consumed by `Session::hit` which maps it down to
/// `layout::SidebarRowInput` for hit-testing.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AgentEntry {
    /// Current session's name. Single-session v1 puts the same value
    /// on every entry; field exists for cross-session forward-compat.
    pub session_name: String,
    /// 0-based, matches `Session.windows[..]` slot and
    /// `Session.active_window`. The user-visible label renders 1-based
    /// (`W<window_index + 1>:claude`).
    pub window_index: usize,
    pub pane_id: crate::layout::PaneId,
    pub agent: crate::detect::Agent,
    pub state: crate::detect::AgentState,
    /// Cached session label from the multiplexer (e.g. tmux window name).
    /// None until `refresh_agent_meta` populates the pane's cache.
    pub session_label: Option<String>,
    /// Cached VCS branch for this pane's working directory.
    /// None until `refresh_agent_meta` populates the pane's cache.
    pub branch: Option<String>,
}

pub struct Session {
    session_name: String,
    windows: Vec<Window>,
    active_window: usize,
    panes: HashMap<PaneId, Pane>,
    compositor: Compositor,
    theme: crate::theme::Theme,
    next_pane_id: PaneId,
    cols: u16,
    rows: u16,
    command: Vec<String>,
    cwd: String,
    /// True when the user has toggled the agent-awareness sidebar visible.
    /// Default: false (hidden). Toggled by Command::ToggleSidebar.
    /// `layout::compute_regions` may still suppress the sidebar's visual
    /// presence if the viewport is too narrow (see SIDEBAR_MIN_CONTENT_COLS).
    sidebar_visible: bool,
    /// The open context menu, if any. At most one across the whole session
    /// and all attached clients. See
    /// `docs/superpowers/specs/2026-05-29-pane-right-click-menu-design.md`.
    menu: Option<crate::menu::MenuState>,
    /// Width of the sidebar in columns, as set at construction time from the
    /// user's config. Replaces the `layout::SIDEBAR_WIDTH_DEFAULT` constant
    /// in all layout calculations so the value propagates from config to
    /// every relayout call.
    sidebar_width: u16,
    /// Live prefix + keymap, captured at construction so the help overlay can
    /// render real chords without threading `Config` through `compose`.
    prefix: crate::config::KeySpec,
    keys: crate::config::KeyMap,
    /// The open help overlay, if any. Shared across all attached clients.
    help: Option<crate::help::HelpState>,
    /// The open window-name dialog, if any. At most one across the session;
    /// mutually exclusive with `menu`. See
    /// `docs/superpowers/specs/2026-05-29-window-tab-rename-design.md`.
    dialog: Option<crate::dialog::WindowNameDialog>,
    /// Set by structural mutators; the daemon's snapshot cadence consumes it.
    snapshot_dirty: bool,
}

/// Side effects of a command that the run loop must perform: attach a forwarder
/// for each newly spawned pane, tear down each removed runtime off the async
/// runtime, and detach the issuing client if `detach` is set.
#[derive(Default)]
pub struct CommandEffect {
    pub spawned: Vec<(PaneId, mpsc::UnboundedReceiver<PaneOutput>)>,
    pub closed: Vec<PaneRuntime>,
    pub detach: bool,
}

/// Minimum cells along the split axis for a split to be allowed (two 2-cell
/// children plus a 1-cell divider).
const MIN_SPLIT_AXIS: u16 = 5;

/// The seed name for a freshly-created window: the spawn command's basename, or
/// the placeholder "shell" when the command is empty (an empty command means the
/// default shell, resolved only inside PaneRuntime::spawn — not duplicated here).
/// The real name is filled in by the first `refresh_window_names`.
fn initial_window_name(command: &[String]) -> String {
    match command.first() {
        Some(c) => c.rsplit('/').next().unwrap_or(c).to_string(),
        None => "shell".to_string(),
    }
}

/// Build the per-pane spawn argv for a resume. `None` → empty (default shell).
/// The shell path is passed as argv[0] AND the trailing `$0` positional, and the
/// `-c` string re-execs it via `exec "$0"` — no shell-path interpolation, no
/// reliance on a `SHELL` env var. The id is assumed already UUID-validated by
/// `persist::plan_restore` (non-agent panes arrive as `None`).
fn restore_command(agent: &crate::persist::AgentResume, shell: &str) -> Vec<String> {
    let verb = match agent {
        crate::persist::AgentResume::None => return Vec::new(),
        crate::persist::AgentResume::Claude { session_id, .. } => {
            format!("claude --resume {session_id}")
        }
        crate::persist::AgentResume::Codex { session_id, .. } => {
            format!("codex resume {session_id}")
        }
    };
    vec![
        shell.to_string(),
        "-i".into(),
        "-c".into(),
        format!("{verb}; exec \"$0\""),
        shell.to_string(),
    ]
}

/// Outcome of `Session::refresh_agent_states`. Tracks whether anything
/// changed (gates redraws) and which pane(s) just transitioned into
/// `Blocked` (drives the sound trigger in `server::run`).
#[derive(Default, Debug, Clone, PartialEq, Eq)]
pub struct RefreshOutcome {
    pub changed: bool,
    pub blocked_transitions: Vec<crate::layout::PaneId>,
}

impl Session {
    /// Create a session with one window holding a single pane running `command`.
    /// Returns the session, the first pane's id, and its output receiver (the
    /// caller spawns a forwarder that tags the output with the pane id).
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        session_name: String,
        command: Vec<String>,
        cwd: String,
        cols: u16,
        rows: u16,
        theme: crate::theme::Theme,
        prefix: crate::config::KeySpec,
        keys: crate::config::KeyMap,
        sidebar_width: u16,
    ) -> io::Result<(Self, PaneId, mpsc::UnboundedReceiver<PaneOutput>)> {
        // Clamp to at least 1x1 (matches GhosttyScreen/PaneRuntime resize behavior).
        let cols = cols.max(1);
        let rows = rows.max(1);
        let mut session = Session {
            session_name,
            windows: Vec::new(),
            active_window: 0,
            panes: HashMap::new(),
            compositor: Compositor::new(),
            theme,
            next_pane_id: 1,
            cols,
            rows,
            command,
            cwd,
            // Open by default: the agent-awareness sidebar is the product's
            // headline feature, so it's visible on startup (toggle with the
            // ToggleSidebar binding). `compute_regions` still suppresses it
            // when the viewport is too narrow to keep usable content width.
            sidebar_visible: true,
            menu: None,
            sidebar_width,
            prefix,
            keys,
            help: None,
            dialog: None,
            snapshot_dirty: true,
        };
        // The status bar is always present, so the pane fills the content area
        // (viewport minus the bar row), not the whole viewport.
        let content = layout::compute_regions(
            Rect {
                x: 0,
                y: 0,
                w: cols,
                h: rows,
            },
            session.sidebar_visible,
            session.sidebar_width,
        )
        .content_area;
        let (id, rx) = session.spawn_pane(content)?;
        session.windows.push(Window {
            name: initial_window_name(&session.command),
            name_pinned: false,
            root: Node::Leaf(id),
            focused: id,
            zoomed: false,
        });
        Ok((session, id, rx))
    }

    /// Spawn a pane sized to `rect`, insert it into the pane map, and return its
    /// id + output receiver. Does not place it into any window's tree.
    fn spawn_pane(
        &mut self,
        rect: Rect,
    ) -> io::Result<(PaneId, mpsc::UnboundedReceiver<PaneOutput>)> {
        let w = rect.w.max(1);
        let h = rect.h.max(1);
        let (runtime, rx) = PaneRuntime::spawn(&self.command, &self.cwd, w, h, &self.session_name)?;
        let screen = GhosttyScreen::new(w, h);
        let id = self.next_pane_id;
        self.next_pane_id += 1;
        self.panes.insert(
            id,
            Pane {
                runtime,
                screen,
                agent_id_pid: None,
                agent: None,
                agent_state: detect::AgentState::Idle,
                idle_streak: 0,
                session_label: None,
                branch: None,
                meta_last_refresh: std::time::Instant::now() - std::time::Duration::from_secs(60),
                meta_err_seen: std::collections::HashSet::new(),
                cwd: None,
                agent_resume: crate::persist::AgentResume::None,
                captured_for_pid: None,
            },
        );
        Ok((id, rx))
    }

    /// Like `spawn_pane`, but with an explicit cwd + command (empty = default shell).
    fn spawn_pane_with(
        &mut self,
        rect: Rect,
        cwd: &str,
        command: &[String],
    ) -> io::Result<(PaneId, mpsc::UnboundedReceiver<PaneOutput>)> {
        let w = rect.w.max(1);
        let h = rect.h.max(1);
        let (runtime, rx) = PaneRuntime::spawn(command, cwd, w, h, &self.session_name)?;
        let screen = GhosttyScreen::new(w, h);
        let id = self.next_pane_id;
        self.next_pane_id += 1;
        self.panes.insert(
            id,
            Pane {
                runtime,
                screen,
                agent_id_pid: None,
                agent: None,
                agent_state: detect::AgentState::Idle,
                idle_streak: 0,
                session_label: None,
                branch: None,
                meta_last_refresh: std::time::Instant::now()
                    - std::time::Duration::from_secs(60),
                meta_err_seen: std::collections::HashSet::new(),
                cwd: Some(cwd.to_string()),
                agent_resume: crate::persist::AgentResume::None,
                captured_for_pid: None,
            },
        );
        Ok((id, rx))
    }

    /// Build a `Session` from a snapshot, spawning each pane with its restore command.
    /// Returns the session and the per-pane output receivers (caller wires forwarders).
    #[allow(clippy::too_many_arguments)]
    /// Rebuild a live session from a persisted snapshot. The restore plan is
    /// computed HERE from `snap` (not passed in) so the per-leaf resume commands
    /// are structurally locked to the snapshot's DFS leaf order: both
    /// `plan_restore`/`collect_panes` and the `build_node` walk recurse
    /// first-then-second over the SAME `snap`, so plan pane `k` always maps to
    /// snapshot leaf `k`. A foreign/stale plan can no longer be substituted.
    /// Phase 5's server caller therefore invokes
    /// `Session::restore(snap, 80, 24, theme, prefix, keys)` with no plan arg.
    pub fn restore(
        snap: crate::persist::SessionSnapshot,
        cols: u16,
        rows: u16,
        theme: crate::theme::Theme,
        prefix: crate::config::KeySpec,
        keys: crate::config::KeyMap,
    ) -> io::Result<(Self, Vec<(PaneId, mpsc::UnboundedReceiver<PaneOutput>)>)> {
        // Re-walk the snapshot to derive the plan locally; see doc comment above.
        let plan = crate::persist::plan_restore(&snap);
        let cols = cols.max(1);
        let rows = rows.max(1);
        let mut session = Session {
            session_name: snap.session_name.clone(),
            windows: Vec::new(),
            active_window: 0,
            panes: HashMap::new(),
            compositor: Compositor::new(),
            theme,
            next_pane_id: 1,
            cols,
            rows,
            command: Vec::new(),
            cwd: std::env::var("HOME").unwrap_or_else(|_| ".".into()),
            sidebar_visible: snap.sidebar_visible,
            menu: None,
            sidebar_width: snap.sidebar_width,
            prefix,
            keys,
            help: None,
            dialog: None,
            snapshot_dirty: true,
        };

        let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".into());
        let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
        let content = layout::compute_regions(
            Rect {
                x: 0,
                y: 0,
                w: cols,
                h: rows.saturating_sub(1).max(1),
            },
            snap.sidebar_visible,
            snap.sidebar_width,
        )
        .content_area;

        let mut rxs = Vec::new();
        // Pull plan panes in tree order to mirror snapshot ordering.
        let mut plan_iter = plan.panes.iter();
        for w in &snap.windows {
            let (root, focus_order) =
                session.build_node(&w.layout, content, &shell, &home, &mut plan_iter, &mut rxs)?;
            let focused = *focus_order
                .get(w.focused_leaf)
                .or_else(|| focus_order.first())
                .unwrap();
            session.windows.push(Window {
                name: w.name.clone(),
                name_pinned: w.name_pinned,
                root,
                focused,
                zoomed: w.zoomed,
            });
        }
        session.active_window = snap.active_window.min(session.windows.len().saturating_sub(1));
        session.relayout_all();
        Ok((session, rxs))
    }

    /// Recursively rebuild a layout node, spawning a pane per leaf. Returns the node
    /// and the leaf PaneIds in tree order (for focus mapping).
    #[allow(clippy::too_many_arguments)]
    fn build_node<'a>(
        &mut self,
        node: &crate::persist::NodeSnapshot,
        rect: Rect,
        shell: &str,
        home: &str,
        plan: &mut impl Iterator<Item = &'a crate::persist::PaneRestore>,
        rxs: &mut Vec<(PaneId, mpsc::UnboundedReceiver<PaneOutput>)>,
    ) -> io::Result<(layout::Node, Vec<PaneId>)> {
        use crate::persist::NodeSnapshot;
        match node {
            NodeSnapshot::Leaf(_) => {
                let pr = plan.next();
                let (cwd, command) = match pr {
                    Some(p) => {
                        let cwd = if p.cwd_exists {
                            p.cwd.clone()
                        } else {
                            home.to_string()
                        };
                        (cwd, restore_command(&p.agent, shell))
                    }
                    None => (home.to_string(), Vec::new()),
                };
                let (id, rx) = self.spawn_pane_with(rect, &cwd, &command)?;
                rxs.push((id, rx));
                Ok((layout::Node::Leaf(id), vec![id]))
            }
            NodeSnapshot::Split {
                dir,
                ratio,
                first,
                second,
            } => {
                let d = match dir {
                    crate::persist::SplitDirSnap::Horizontal => layout::SplitDir::Horizontal,
                    crate::persist::SplitDirSnap::Vertical => layout::SplitDir::Vertical,
                };
                // Children get the parent `rect` as a NOMINAL initial size. There is no
                // public `layout::split_rect`, and duplicating the private
                // `layout::split_area` here would be pointless: `relayout_all` (called
                // once at the end of `restore`) recomputes every pane rect from the tree
                // + ratios and resizes each PTY/screen, so the approximate initial size
                // only lasts a few ms before first paint.
                let (fnode, mut forder) = self.build_node(first, rect, shell, home, plan, rxs)?;
                let (snode, sorder) = self.build_node(second, rect, shell, home, plan, rxs)?;
                forder.extend(sorder);
                Ok((
                    layout::Node::Split {
                        dir: d,
                        ratio: *ratio,
                        first: Box::new(fnode),
                        second: Box::new(snode),
                    },
                    forder,
                ))
            }
        }
    }

    /// Return the configured sidebar width (columns).
    pub fn sidebar_width(&self) -> u16 {
        self.sidebar_width
    }

    fn viewport(&self) -> Rect {
        Rect {
            x: 0,
            y: 0,
            w: self.cols,
            h: self.rows,
        }
    }

    /// Toggle the sidebar's visibility and cascade the layout change.
    /// Flips `sidebar_visible`, then calls `relayout_all()` so every
    /// pane's PTY and GhosttyScreen resize to the new content-area width.
    /// Without the relayout, child processes (vim, claude, less) keep
    /// rendering at the old width and corrupt their output.
    pub fn toggle_sidebar(&mut self) {
        self.sidebar_visible = !self.sidebar_visible;
        self.relayout_all();
        self.mark_snapshot_dirty();
    }

    /// True if the context menu is currently open. Exposed for
    /// `src/server/mod.rs::FrameSnapshot` construction so the `menu` field
    /// itself stays encapsulated.
    pub fn menu_open(&self) -> bool {
        self.menu.is_some()
    }

    /// True if a structural change occurred since the last `take_snapshot_dirty`.
    pub fn take_snapshot_dirty(&mut self) -> bool {
        std::mem::replace(&mut self.snapshot_dirty, false)
    }

    /// Re-request a snapshot so the next snapshot tick writes again. Used by the
    /// server to retry after a transient `write_atomic` failure (the dirty bit
    /// was already consumed by `take_snapshot_dirty`, so without this the write
    /// would not be retried until the next structural/volatile change).
    pub fn request_snapshot(&mut self) {
        self.snapshot_dirty = true;
    }

    fn mark_snapshot_dirty(&mut self) {
        self.snapshot_dirty = true;
    }

    /// Build a persistable snapshot of the current layout + per-pane restore identity.
    pub fn snapshot(&self) -> crate::persist::SessionSnapshot {
        use crate::persist::*;
        let windows = self
            .windows
            .iter()
            .map(|w| {
                let order = layout::all_panes(&w.root);
                let focused_leaf = order.iter().position(|p| *p == w.focused).unwrap_or(0);
                WindowSnapshot {
                    name: w.name.clone(),
                    name_pinned: w.name_pinned,
                    zoomed: w.zoomed,
                    focused_leaf,
                    layout: self.node_snapshot(&w.root),
                }
            })
            .collect();
        SessionSnapshot {
            schema_version: SCHEMA_VERSION,
            session_name: self.session_name.clone(),
            sidebar_visible: self.sidebar_visible,
            sidebar_width: self.sidebar_width,
            active_window: self.active_window,
            windows,
        }
    }

    fn node_snapshot(&self, node: &layout::Node) -> crate::persist::NodeSnapshot {
        use crate::persist::*;
        match node {
            layout::Node::Leaf(id) => {
                let pane = self.panes.get(id);
                NodeSnapshot::Leaf(PaneSnapshot {
                    cwd: pane.and_then(|p| p.cwd.clone()).unwrap_or_else(|| self.cwd.clone()),
                    agent: pane.map(|p| p.agent_resume.clone()).unwrap_or(AgentResume::None),
                })
            }
            layout::Node::Split { dir, ratio, first, second } => NodeSnapshot::Split {
                dir: match dir {
                    layout::SplitDir::Horizontal => SplitDirSnap::Horizontal,
                    layout::SplitDir::Vertical => SplitDirSnap::Vertical,
                },
                ratio: *ratio,
                first: Box::new(self.node_snapshot(first)),
                second: Box::new(self.node_snapshot(second)),
            },
        }
    }

    /// Post-parser keyboard intercept. Returns `true` if the event was
    /// consumed (caller skips its normal dispatch). Returns `None` when the
    /// event was NOT consumed by the menu (caller continues normal dispatch).
    /// Returns `Some(eff)` when the event WAS consumed; `eff` carries any
    /// side effects (spawned panes, closed runtimes, detach) that the caller
    /// must propagate. Mouse and FocusIn events always return `None` so they
    /// reach `handle_mouse` / the focus-promotion path respectively. `Command`
    /// events are swallowed so prefix-bound commands cannot mutate state while
    /// the menu is up.
    pub fn try_consume_menu_event(
        &mut self,
        ev: &crate::input::InputEvent,
    ) -> Option<crate::session::CommandEffect> {
        use crate::input::InputEvent;
        self.menu.as_ref()?;
        match ev {
            InputEvent::Mouse(_) | InputEvent::FocusIn => None,
            InputEvent::Command(_) => Some(CommandEffect::default()),
            InputEvent::Pane(bytes) => match bytes.as_slice() {
                b"\x1b" => {
                    self.menu = None;
                    Some(CommandEffect::default())
                }
                b"\x1b[A" => {
                    if let Some(m) = self.menu.as_mut() {
                        m.move_up();
                    }
                    Some(CommandEffect::default())
                }
                b"\x1b[B" => {
                    if let Some(m) = self.menu.as_mut() {
                        m.move_down();
                    }
                    Some(CommandEffect::default())
                }
                b"\r" | b"\n" => {
                    if let Some(m) = self.menu.clone() {
                        let action = m.items()[m.highlighted].action;
                        self.menu = None;
                        Some(self.dispatch_menu_action(action, m.kind))
                    } else {
                        Some(CommandEffect::default())
                    }
                }
                _ => Some(CommandEffect::default()),
            },
        }
    }

    /// Mouse dispatch when a menu is open.
    fn handle_menu_mouse(&mut self, ev: crate::input::MouseEvent) -> CommandEffect {
        use crate::input::MouseKind;
        use crate::menu::{menu_hit, MenuHit, MenuKind, MenuState};

        let Some(menu) = self.menu.clone() else {
            return CommandEffect::default();
        };
        let items = menu.items();
        let label_w = items
            .iter()
            .map(|i| i.label.chars().count())
            .max()
            .unwrap_or(0) as u16;
        let content =
            layout::compute_regions(self.viewport(), self.sidebar_visible, self.sidebar_width)
                .content_area;
        let hit = menu_hit(&menu, items.len(), label_w, content, ev.col, ev.row);

        match ev.kind {
            MouseKind::Press if ev.button == 0 => match hit {
                MenuHit::Item(i) => {
                    let action = items[i].action;
                    let kind = menu.kind;
                    self.menu = None;
                    return self.dispatch_menu_action(action, kind);
                }
                MenuHit::Border => {}
                MenuHit::Outside => {
                    self.menu = None;
                }
            },
            MouseKind::Press if ev.button == 2 => {
                self.menu = None;
                let new_hit = self.hit(ev.col, ev.row);
                if let layout::Hit::Tab(window) = new_hit {
                    self.menu = Some(MenuState {
                        kind: MenuKind::Tab { window },
                        anchor: (ev.col, ev.row),
                        highlighted: 0,
                    });
                } else if let layout::Hit::Pane(id) = new_hit {
                    self.focus(id);
                    self.menu = Some(MenuState {
                        kind: MenuKind::Pane { pane_id: id },
                        anchor: (ev.col, ev.row),
                        highlighted: 0,
                    });
                }
            }
            MouseKind::Drag => {
                if let MenuHit::Item(i) = hit {
                    if let Some(m) = self.menu.as_mut() {
                        m.highlighted = i;
                    }
                }
            }
            _ => {}
        }
        CommandEffect::default()
    }

    /// Auto-close the menu if its anchored pane is gone, or the anchor is
    /// Auto-close the menu when the anchored pane is gone, when the
    /// anchor falls outside the current content area (e.g. after a
    /// resize or sidebar toggle), or unconditionally for window-switching
    /// / detach / sidebar-toggle / resize mutators. Called at the bottom
    /// of every menu-affecting mutator.
    fn reconcile_menu(&mut self) {
        let Some(menu) = self.menu.as_ref() else {
            return;
        };
        let content_area =
            layout::compute_regions(self.viewport(), self.sidebar_visible, self.sidebar_width)
                .content_area;
        let (ax, ay) = (
            menu.anchor.0.saturating_sub(1),
            menu.anchor.1.saturating_sub(1),
        );
        let anchor_inside = content_area.contains(ax, ay);
        let pane_alive = match menu.kind {
            crate::menu::MenuKind::Pane { pane_id } => self
                .windows
                .get(self.active_window)
                .is_some_and(|w| layout::all_panes(&w.root).contains(&pane_id)),
            crate::menu::MenuKind::Tab { window } => window < self.windows.len(),
        };
        if !anchor_inside || !pane_alive {
            self.menu = None;
        }
    }

    #[cfg(test)]
    pub fn set_menu_for_test(&mut self, menu: Option<crate::menu::MenuState>) {
        self.menu = menu;
    }

    /// Build a render-ready help view from the open state + live prefix/keys.
    fn build_help_view(&self, state: &crate::help::HelpState) -> crate::help::HelpView {
        crate::help::build_view(state, self.prefix, &self.keys)
    }

    #[cfg(test)]
    pub fn set_help_for_test(&mut self, help: Option<crate::help::HelpState>) {
        self.help = help;
    }

    #[cfg(test)]
    pub fn help_open_for_test(&self) -> bool {
        self.help.is_some()
    }

    #[cfg(test)]
    pub fn dialog_is_open_for_test(&self) -> bool {
        self.dialog.is_some()
    }

    #[cfg(test)]
    pub fn window_count_for_test(&self) -> usize {
        self.windows.len()
    }

    #[cfg(test)]
    pub fn dialog_insert_for_test(&mut self, c: char) {
        if let Some(d) = self.dialog.as_mut() {
            d.insert_char(c);
        }
    }

    #[cfg(test)]
    pub fn dialog_clear_for_test(&mut self) {
        if let Some(d) = self.dialog.as_mut() {
            d.buffer.clear();
        }
    }

    #[cfg(test)]
    pub fn tab_status_row_1based_for_test(&self) -> u16 {
        let regions =
            layout::compute_regions(self.viewport(), self.sidebar_visible, self.sidebar_width);
        regions.tab_status_row + 1
    }

    #[cfg(test)]
    pub fn active_window_in_range_for_test(&self) -> bool {
        self.windows.is_empty() || self.active_window < self.windows.len()
    }

    #[cfg(test)]
    pub fn menu_kind_for_test(&self) -> Option<crate::menu::MenuKind> {
        self.menu.as_ref().map(|m| m.kind)
    }

    #[cfg(test)]
    pub fn dialog_buffer_for_test(&self) -> String {
        self.dialog.as_ref().map(|d| d.buffer.clone()).unwrap_or_default()
    }

    /// Open the New Window dialog. Closes any open menu (mutual exclusion).
    pub fn open_new_window_dialog(&mut self) {
        self.menu = None;
        self.dialog = Some(crate::dialog::WindowNameDialog::new_window());
    }

    /// Open the Rename dialog for `window`, pre-filled with its current name.
    /// No-op if the index is out of range. Closes any open menu.
    pub fn open_rename_dialog(&mut self, window: usize) {
        let Some(w) = self.windows.get(window) else { return };
        let name = w.name.clone();
        self.menu = None;
        self.dialog = Some(crate::dialog::WindowNameDialog::rename(window, &name));
    }

    /// Close every pane in `window` and remove the window. Returns the removed
    /// `PaneRuntime`s so the caller tears them down off the async runtime.
    /// Composes the existing per-pane teardown (`pane_exited`); no new semantics.
    /// Closing the only window empties the session, handled by the existing
    /// empty-session shutdown path.
    pub fn close_window(&mut self, window: usize) -> Vec<PaneRuntime> {
        let Some(win) = self.windows.get(window) else { return Vec::new() };
        let pane_ids = layout::all_panes(&win.root);
        let mut closed = Vec::new();
        for id in pane_ids {
            closed.extend(self.pane_exited(id));
        }
        closed
    }

    /// Resolve an activated menu item to its effect, given the menu's kind.
    /// Pane items dispatch their `Command`; tab items act on the kind's window.
    fn dispatch_menu_action(
        &mut self,
        action: crate::menu::MenuAction,
        kind: crate::menu::MenuKind,
    ) -> CommandEffect {
        use crate::menu::{MenuAction, MenuKind};
        match action {
            MenuAction::Command(cmd) => self.apply_command(cmd),
            MenuAction::RenameWindow => {
                if let MenuKind::Tab { window } = kind {
                    self.open_rename_dialog(window);
                }
                CommandEffect::default()
            }
            MenuAction::CloseWindow => {
                let mut eff = CommandEffect::default();
                if let MenuKind::Tab { window } = kind {
                    eff.closed = self.close_window(window);
                }
                eff
            }
        }
    }

    /// Apply the open dialog's buffer and close it. Returns the effect (a New
    /// Window submit spawns a pane). No-op effect if no dialog is open.
    pub fn submit_dialog(&mut self) -> CommandEffect {
        let mut eff = CommandEffect::default();
        let Some(dialog) = self.dialog.take() else {
            return eff;
        };
        let value = dialog.value().to_string();
        match dialog.mode {
            crate::dialog::DialogMode::NewWindow => {
                self.unzoom_active();
                let pinned = if value.is_empty() { None } else { Some(value) };
                self.new_window(pinned, &mut eff);
            }
            crate::dialog::DialogMode::RenameWindow { window } => {
                // Do NOT hold a `&mut Window` across `refresh_window_names`
                // (that would be a mutable-borrow conflict). Index for the flag
                // write and release the borrow before re-deriving.
                if window < self.windows.len() {
                    if value.is_empty() {
                        // Un-pin: revert to auto and re-derive immediately.
                        self.windows[window].name_pinned = false;
                        self.refresh_window_names(crate::procinfo::process_name);
                    } else {
                        let w = &mut self.windows[window];
                        w.name = value;
                        w.name_pinned = true;
                    }
                    self.mark_snapshot_dirty();
                }
            }
        }
        eff
    }

    /// Discard the open dialog without applying it.
    pub fn cancel_dialog(&mut self) {
        self.dialog = None;
    }

    /// If a dialog is open, consume `ev` into it and return the resulting effect
    /// (Enter may spawn a window). Returns `None` when no dialog is open, so the
    /// caller falls through to normal routing. Mouse/Command/Focus events are
    /// swallowed (return a default effect) so they do not reach the focused pane
    /// while the modal is up.
    pub fn try_consume_dialog_event(
        &mut self,
        ev: &crate::input::InputEvent,
    ) -> Option<CommandEffect> {
        use crate::input::InputEvent;
        self.dialog.as_ref()?;
        match ev {
            InputEvent::Mouse(_) | InputEvent::FocusIn | InputEvent::Command(_) => {
                Some(CommandEffect::default())
            }
            InputEvent::Pane(bytes) => match bytes.as_slice() {
                b"\x1b" => {
                    self.cancel_dialog();
                    Some(CommandEffect::default())
                }
                b"\r" | b"\n" => Some(self.submit_dialog()),
                b"\x7f" | b"\x08" => {
                    if let Some(d) = self.dialog.as_mut() {
                        d.backspace();
                    }
                    Some(CommandEffect::default())
                }
                other => {
                    // Ignore anything that begins with ESC. The parser forwards a
                    // completed CSI/escape sequence (arrow keys etc.) as ONE Pane
                    // chunk starting with 0x1b. Only ESC itself is a control char,
                    // so naive is_control filtering would insert the literal "[A"
                    // of an arrow key. Skip ESC-led chunks; else append printable
                    // chars (cursor pinned at end; minimal editing for v1).
                    if other.first() != Some(&0x1b) {
                        let text = String::from_utf8_lossy(other);
                        if let Some(d) = self.dialog.as_mut() {
                            for c in text.chars() {
                                if !c.is_control() {
                                    d.insert_char(c);
                                }
                            }
                        }
                    }
                    Some(CommandEffect::default())
                }
            },
        }
    }

    #[cfg(test)]
    pub fn help_tab_for_test(&self) -> Option<crate::help::HelpTab> {
        self.help.as_ref().map(|h| h.tab)
    }

    #[cfg(test)]
    pub fn help_scroll_for_test(&self) -> Option<u16> {
        self.help.as_ref().map(|h| h.scroll)
    }

    /// Post-parser keyboard intercept for the help overlay. Returns `None`
    /// (passthrough) when help is closed, for mouse and `FocusIn` events, and for the
    /// `ShowHelp` command (so `apply_command` can toggle help closed). All other
    /// commands and keystrokes are swallowed (`Some(CommandEffect::default())`)
    /// while help is open; recognised keys drive it.
    pub fn try_consume_help_event(
        &mut self,
        ev: &crate::input::InputEvent,
    ) -> Option<crate::session::CommandEffect> {
        use crate::input::InputEvent;
        self.help.as_ref()?;
        match ev {
            InputEvent::Mouse(_) | InputEvent::FocusIn => None,
            InputEvent::Command(crate::config::Command::ShowHelp) => None,
            InputEvent::Command(_) => Some(CommandEffect::default()),
            InputEvent::Pane(bytes) => {
                match bytes.as_slice() {
                    b"q" | b"\x1b" | b"?" => self.help = None,
                    b"\x1b[C" | b"\t" => self.help_switch_tab(crate::help::HelpTab::next),
                    b"\x1b[D" => self.help_switch_tab(crate::help::HelpTab::prev),
                    b"\x1b[A" => self.help_scroll(-1),
                    b"\x1b[B" => self.help_scroll(1),
                    _ => {}
                }
                Some(CommandEffect::default())
            }
        }
    }

    fn help_switch_tab(&mut self, f: fn(crate::help::HelpTab) -> crate::help::HelpTab) {
        if let Some(state) = self.help {
            self.help = Some(crate::help::HelpState {
                tab: f(state.tab),
                scroll: 0,
            });
        }
    }

    fn help_scroll(&mut self, delta: i16) {
        let Some(state) = self.help else { return };
        let content =
            layout::compute_regions(self.viewport(), self.sidebar_visible, self.sidebar_width)
                .content_area;
        let view = self.build_help_view(&state);
        let rect = crate::help::help_rect(content, view.active_rows().len());
        let max = crate::help::max_scroll(view.active_rows().len(), rect);
        let new = if delta < 0 {
            state.scroll.saturating_sub((-delta) as u16)
        } else {
            state.scroll.saturating_add(delta as u16).min(max)
        };
        self.help = Some(crate::help::HelpState { scroll: new, ..state });
    }

    /// Mouse dispatch when the help overlay is open. Press-only; right-button
    /// and motion are no-ops.
    fn handle_help_mouse(&mut self, ev: crate::input::MouseEvent) -> CommandEffect {
        use crate::help::{help_hit, HelpHit, HelpState, HelpTab};
        use crate::input::MouseKind;
        let Some(state) = self.help else {
            return CommandEffect::default();
        };
        let content =
            layout::compute_regions(self.viewport(), self.sidebar_visible, self.sidebar_width)
                .content_area;
        let view = self.build_help_view(&state);
        if let MouseKind::Press = ev.kind {
            if ev.button == 0 {
                match help_hit(&view, content, ev.col, ev.row) {
                    HelpHit::Tab(i) => {
                        self.help = Some(HelpState {
                            tab: HelpTab::from_index(i),
                            scroll: 0,
                        });
                    }
                    HelpHit::Outside => self.help = None,
                    HelpHit::Body => {}
                }
            }
        }
        CommandEffect::default()
    }

    /// Resize every pane's PTY + screen to the rect the current geometry assigns
    /// it (across all windows), draining size-report replies. Called after any
    /// structural change and on viewport resize.
    fn relayout_all(&mut self) {
        let content =
            layout::compute_regions(self.viewport(), self.sidebar_visible, self.sidebar_width)
                .content_area;
        let mut targets: Vec<(PaneId, Rect)> = Vec::new();
        for (wi, win) in self.windows.iter().enumerate() {
            // Invariant: only the active window is ever zoomed (every active_window change unzooms first).
            if wi == self.active_window && win.zoomed {
                targets.push((win.focused, content));
            } else {
                targets.extend(layout::pane_rects(&win.root, content));
            }
        }
        for (id, rect) in targets {
            if let Some(pane) = self.panes.get_mut(&id) {
                let w = rect.w.max(1);
                let h = rect.h.max(1);
                pane.runtime.resize(w, h);
                pane.screen.resize(w, h);
                let replies = pane.screen.take_pty_writes();
                if !replies.is_empty() {
                    pane.runtime.write_input(&replies);
                }
            }
        }
    }

    /// Feed pane-output bytes into that pane's screen and route any query replies
    /// back to its PTY. Output for an already-closed pane is ignored.
    pub fn feed(&mut self, pane_id: PaneId, bytes: &[u8]) {
        if let Some(pane) = self.panes.get_mut(&pane_id) {
            pane.screen.feed(bytes);
            let replies = pane.screen.take_pty_writes();
            if !replies.is_empty() {
                pane.runtime.write_input(&replies);
            }
        }
    }

    /// Compose the active window into a `Grid` (the diff/serialize source).
    pub fn compose(&self) -> crate::compositor::Grid {
        let viewport = self.viewport();
        let names: Vec<String> = self.windows.iter().map(|w| w.name.clone()).collect();
        let refs: Vec<(PaneId, &dyn PaneCells)> = self
            .panes
            .iter()
            .map(|(id, p)| (*id, &p.screen as &dyn PaneCells))
            .collect();
        let win = &self.windows[self.active_window];
        let agent_entries = self.agent_entries();
        let help_view = self.help.as_ref().map(|h| self.build_help_view(h));
        self.compositor.compose(
            viewport,
            &win.root,
            &self.session_name,
            &names,
            self.active_window,
            win.focused,
            win.zoomed,
            &self.theme,
            &refs,
            self.sidebar_visible,
            self.sidebar_width,
            &agent_entries,
            self.menu.as_ref(),
            help_view.as_ref(),
            self.dialog.as_ref(),
        )
    }

    /// Render the active window into a full-viewport ANSI frame.
    pub fn render(&self) -> Vec<u8> {
        crate::compositor::serialize_full(&self.compose())
    }

    /// Write user input to the focused pane of the active window.
    pub fn input(&self, bytes: &[u8]) {
        if let Some(win) = self.windows.get(self.active_window) {
            if let Some(pane) = self.panes.get(&win.focused) {
                pane.runtime.write_input(bytes);
            }
        }
    }

    /// Resize the viewport: recompute every pane's rect (across all windows) and
    /// resize each pane's PTY + screen to match, draining any size-report replies.
    pub fn resize(&mut self, cols: u16, rows: u16) {
        self.cols = cols.max(1);
        self.rows = rows.max(1);
        self.relayout_all();
        self.reconcile_menu();
    }

    /// Number of panes in the active window (0 if there is no active window).
    pub fn active_pane_count(&self) -> usize {
        self.windows
            .get(self.active_window)
            .map(|w| layout::all_panes(&w.root).len())
            .unwrap_or(0)
    }

    /// The focused pane id of the active window (0 if none — id 0 is never live).
    pub fn focused_pane(&self) -> PaneId {
        self.windows
            .get(self.active_window)
            .map(|w| w.focused)
            .unwrap_or(0)
    }

    pub fn window_count(&self) -> usize {
        self.windows.len()
    }

    pub fn active_window_index(&self) -> usize {
        self.active_window
    }

    /// Clear the active window's zoom flag (called before any layout change so the
    /// user never lands in a "split happened but I can't see it" state).
    ///
    /// Marks the snapshot dirty whenever it actually flips a window from
    /// zoomed -> unzoomed. Callers often unzoom first and only mark dirty after a
    /// follow-up op that can no-op or fail (e.g. `split_focused` bailing on tiny
    /// geometry, or a spawn failure), so persisting the unzoom here guarantees it
    /// survives the daemon snapshot cadence regardless of what the caller does next.
    /// A no-op unzoom (nothing was zoomed) does NOT mark dirty.
    fn unzoom_active(&mut self) {
        if let Some(win) = self.windows.get_mut(self.active_window) {
            if win.zoomed {
                win.zoomed = false;
                self.mark_snapshot_dirty();
            }
        }
    }

    /// Apply a structural command, returning the side effects the run loop must
    /// perform. Marks nothing dirty itself — the caller repaints after applying.
    pub fn apply_command(&mut self, cmd: Command) -> CommandEffect {
        let mut eff = CommandEffect::default();
        match cmd {
            Command::SplitHorizontal => {
                self.unzoom_active();
                self.split_focused(SplitDir::Horizontal, &mut eff)
            }
            Command::SplitVertical => {
                self.unzoom_active();
                self.split_focused(SplitDir::Vertical, &mut eff)
            }
            Command::ClosePane => {
                self.unzoom_active();
                eff.closed = self.close_focused()
            }
            Command::NewWindow => {
                self.unzoom_active(); // leaving the current window
                self.menu = None;
                self.new_window(None, &mut eff)
            }
            Command::OpenNewWindowDialog => {
                self.open_new_window_dialog();
            }
            Command::NextWindow => {
                if !self.windows.is_empty() {
                    self.unzoom_active();
                    self.active_window = (self.active_window + 1) % self.windows.len();
                    self.mark_snapshot_dirty();
                }
                self.menu = None;
            }
            Command::PrevWindow => {
                if !self.windows.is_empty() {
                    self.unzoom_active();
                    let n = self.windows.len();
                    self.active_window = (self.active_window + n - 1) % n;
                    self.mark_snapshot_dirty();
                }
                self.menu = None;
            }
            Command::SelectWindow(n) => {
                if n >= 1 {
                    let idx = n - 1;
                    if idx < self.windows.len() {
                        self.unzoom_active();
                        self.active_window = idx;
                        self.mark_snapshot_dirty();
                    }
                }
                self.menu = None;
            }
            Command::ToggleZoom => {
                if let Some(win) = self.windows.get_mut(self.active_window) {
                    win.zoomed = !win.zoomed;
                }
                self.relayout_all();
                self.mark_snapshot_dirty();
            }
            Command::ToggleSidebar => {
                self.toggle_sidebar();
                self.menu = None;
            }
            Command::Detach => {
                eff.detach = true;
                self.menu = None;
            }
            Command::ShowHelp => {
                self.help = match self.help {
                    Some(_) => None,
                    None => {
                        self.menu = None; // mutual exclusion (defensive)
                        Some(crate::help::HelpState::default())
                    }
                };
            }
        }
        self.reconcile_menu();
        eff
    }

    fn split_focused(&mut self, dir: SplitDir, eff: &mut CommandEffect) {
        let wi = self.active_window;
        let Some(win) = self.windows.get(wi) else {
            return;
        };
        let focused = win.focused;
        let content =
            layout::compute_regions(self.viewport(), self.sidebar_visible, self.sidebar_width)
                .content_area;
        let Some((_, rect)) = layout::pane_rects(&win.root, content)
            .into_iter()
            .find(|(id, _)| *id == focused)
        else {
            return;
        };
        let axis = match dir {
            SplitDir::Horizontal => rect.h,
            SplitDir::Vertical => rect.w,
        };
        if axis < MIN_SPLIT_AXIS {
            self.bell(focused);
            return;
        }
        // Size the new pane to the second child rect so it starts at a sane size.
        let child = match dir {
            SplitDir::Horizontal => Rect {
                h: rect.h / 2,
                ..rect
            },
            SplitDir::Vertical => Rect {
                w: rect.w / 2,
                ..rect
            },
        };
        let (new_id, rx) = match self.spawn_pane(child) {
            Ok(v) => v,
            Err(_) => return,
        };
        let split_ok = layout::split_pane(&mut self.windows[wi].root, focused, dir, new_id);
        debug_assert!(
            split_ok,
            "split_focused: focused leaf {focused} vanished between lookup and split"
        );
        self.windows[wi].focused = new_id;
        self.relayout_all();
        self.mark_snapshot_dirty();
        eff.spawned.push((new_id, rx));
    }

    fn close_focused(&mut self) -> Vec<PaneRuntime> {
        let Some(win) = self.windows.get(self.active_window) else {
            return Vec::new();
        };
        let focused = win.focused;
        self.pane_exited(focused)
    }

    fn new_window(&mut self, pinned_name: Option<String>, eff: &mut CommandEffect) {
        let content =
            layout::compute_regions(self.viewport(), self.sidebar_visible, self.sidebar_width)
                .content_area;
        let (id, rx) = match self.spawn_pane(content) {
            Ok(v) => v,
            Err(_) => return,
        };
        let (name, name_pinned) = match pinned_name {
            Some(n) => (n, true),
            None => (initial_window_name(&self.command), false),
        };
        self.windows.push(Window {
            name,
            name_pinned,
            root: Node::Leaf(id),
            focused: id,
            zoomed: false,
        });
        self.active_window = self.windows.len() - 1;
        self.relayout_all();
        self.mark_snapshot_dirty();
        eff.spawned.push((id, rx));
    }

    /// Ring the focused pane's bell (feedback for a rejected action).
    fn bell(&self, pane_id: PaneId) {
        if let Some(pane) = self.panes.get(&pane_id) {
            pane.runtime.write_input(b"\x07");
        }
    }

    /// Handle a pane's process exit: remove it from the pane map and from its
    /// window's tree (collapsing the split into the sibling), close the window if
    /// it becomes empty, and return the removed `PaneRuntime`(s) so the caller can
    /// tear them down off the async runtime (via `PaneRuntime::close`). The pane id
    /// is removed from the map FIRST, so any late output for it is dropped by `feed`.
    pub fn pane_exited(&mut self, pane_id: PaneId) -> Vec<PaneRuntime> {
        let mut removed = Vec::new();
        match self.panes.remove(&pane_id) {
            Some(pane) => removed.push(pane.runtime),
            None => return removed, // already gone
        }
        // If the exiting pane is in the active (possibly zoomed) window, unzoom so the
        // surviving sibling layout becomes visible — parity with the `ClosePane` command.
        // A pane dying in a *background* window must NOT disturb the active view, so this
        // is scoped to the active window (the only one that can be zoomed anyway).
        if self
            .windows
            .get(self.active_window)
            .is_some_and(|w| layout::all_panes(&w.root).contains(&pane_id))
        {
            self.unzoom_active();
        }
        let mut wi = 0;
        while wi < self.windows.len() {
            if layout::all_panes(&self.windows[wi].root).contains(&pane_id) {
                // `Node::Leaf(0)` is a throwaway placeholder (id 0 is never live).
                let root = std::mem::replace(&mut self.windows[wi].root, Node::Leaf(0));
                match layout::close_pane(root, pane_id) {
                    Some(new_root) => {
                        if self.windows[wi].focused == pane_id {
                            self.windows[wi].focused = layout::first_leaf(&new_root);
                        }
                        self.windows[wi].root = new_root;
                        wi += 1;
                    }
                    None => {
                        self.windows.remove(wi);
                        // Keep `active_window` pointing at the same logical window:
                        // removing a window BEFORE it shifts its index down by one.
                        if self.active_window > wi {
                            self.active_window -= 1;
                        }
                        // If the active window itself was the removed tail, clamp.
                        if !self.windows.is_empty() && self.active_window >= self.windows.len() {
                            self.active_window = self.windows.len() - 1;
                        }
                        // do not advance wi (the next window shifted into this slot)
                    }
                }
            } else {
                wi += 1;
            }
        }
        self.relayout_all();
        self.reconcile_menu();
        self.mark_snapshot_dirty();
        removed
    }

    /// Consume the session, returning every live pane runtime so the caller can
    /// tear them down off the async runtime (`PaneRuntime::close`). Use at daemon
    /// shutdown instead of letting `Session` drop (which would run blocking
    /// `kill()`/`wait()` on the run loop's worker).
    pub fn shutdown(mut self) -> Vec<PaneRuntime> {
        self.panes.drain().map(|(_, pane)| pane.runtime).collect()
    }

    /// True when no windows remain (the daemon should shut down).
    pub fn is_empty(&self) -> bool {
        self.windows.is_empty()
    }

    /// The (pane id, rect) list for the active window's panes (content area). When
    /// the active window is zoomed, this is just the focused pane at the full
    /// content rect — siblings are hidden (but still running).
    pub fn active_pane_rects(&self) -> Vec<(PaneId, Rect)> {
        let content =
            layout::compute_regions(self.viewport(), self.sidebar_visible, self.sidebar_width)
                .content_area;
        match self.windows.get(self.active_window) {
            Some(win) if win.zoomed => vec![(win.focused, content)],
            Some(win) => layout::pane_rects(&win.root, content),
            None => Vec::new(),
        }
    }

    /// The focused pane id of every window, in order (for tests/inspection).
    pub fn window_focused_ids(&self) -> Vec<PaneId> {
        self.windows.iter().map(|w| w.focused).collect()
    }

    /// Focus a pane by id if it belongs to the active window.
    fn focus(&mut self, pane_id: PaneId) {
        let mut changed = false;
        if let Some(win) = self.windows.get_mut(self.active_window) {
            if layout::all_panes(&win.root).contains(&pane_id) && win.focused != pane_id {
                win.focused = pane_id;
                changed = true;
            }
        }
        if changed {
            self.mark_snapshot_dirty();
        }
    }

    /// Apply a mouse event.
    ///
    /// When `self.menu` is open, all events route to `handle_menu_mouse`.
    ///
    /// When `self.menu` is closed, a `Press` with `button == 2` is the
    /// right-click branch: if `drag.is_some()` it is dropped (mid-drag
    /// guard); otherwise it hit-tests, and if the hit is `Hit::Pane(_)` the
    /// menu opens and the pane gains focus. All other right-button hits
    /// drop silently — they do NOT fall through to `handle_hit`.
    ///
    /// Left-button presses, drags, and releases run the existing
    /// `handle_hit` / `resize_divider` paths unchanged.
    pub fn handle_mouse(
        &mut self,
        ev: MouseEvent,
        drag: &mut Option<(usize, SplitPath)>,
    ) -> CommandEffect {
        use crate::menu::{MenuKind, MenuState};

        if self.help.is_some() {
            return self.handle_help_mouse(ev);
        }
        if self.menu.is_some() {
            return self.handle_menu_mouse(ev);
        }
        if let MouseKind::Press = ev.kind {
            if ev.button == 2 {
                if drag.is_some() {
                    return CommandEffect::default();
                }
                let hit = self.hit(ev.col, ev.row);
                if let layout::Hit::Tab(window) = hit {
                    self.menu = Some(MenuState {
                        kind: MenuKind::Tab { window },
                        anchor: (ev.col, ev.row),
                        highlighted: 0,
                    });
                } else if let Some(id) = should_open_pane_menu(hit) {
                    self.focus(id);
                    self.menu = Some(MenuState {
                        kind: MenuKind::Pane { pane_id: id },
                        anchor: (ev.col, ev.row),
                        highlighted: 0,
                    });
                }
                return CommandEffect::default();
            }
        }
        match ev.kind {
            MouseKind::Press => {
                let hit = self.hit(ev.col, ev.row);
                if let layout::Hit::NewWindowButton = hit {
                    // Clear any stale divider-drag, then open the New Window
                    // dialog (Enter on empty creates the default window).
                    *drag = None;
                    self.open_new_window_dialog();
                    return CommandEffect::default();
                }
                let new_drag = self.handle_hit(hit);
                *drag = new_drag.map(|path| (self.active_window, path));
            }
            MouseKind::Drag => {
                if let Some((wi, path)) = drag.clone() {
                    self.resize_divider(wi, &path, ev.col, ev.row);
                }
            }
            MouseKind::Release => *drag = None,
        }
        CommandEffect::default()
    }

    pub(crate) fn hit(&self, col: u16, row: u16) -> layout::Hit {
        if col == 0 || row == 0 {
            return layout::Hit::None;
        }
        let x = col - 1;
        let y = row - 1;

        let regions =
            layout::compute_regions(self.viewport(), self.sidebar_visible, self.sidebar_width);

        // 1. Sidebar FIRST — works in both zoomed and non-zoomed branches.
        if let Some(sb) = regions.sidebar {
            if sb.contains(x, y) {
                let entries = self.agent_entries();
                let inputs: Vec<_> = entries
                    .iter()
                    .map(|e| layout::SidebarRowInput {
                        window_index: e.window_index,
                        pane_id: e.pane_id,
                    })
                    .collect();
                for r in layout::sidebar_rows(sb, &inputs) {
                    if y >= r.y && y < r.y + r.h {
                        return layout::Hit::SidebarEntry {
                            window_index: r.window_index,
                            pane_id: r.pane_id,
                        };
                    }
                }
                return layout::Hit::None;
            }
        }

        // 2. Tab/status row. Uses FULL viewport width (self.cols) because
        // the bottom bar spans full width even when sidebar is visible.
        if y == regions.tab_status_row {
            let tab_names: Vec<String> = self.windows.iter().map(|w| w.name.clone()).collect();
            let win_zoomed = self
                .windows
                .get(self.active_window)
                .is_some_and(|w| w.zoomed);
            let bar = layout::tab_bar_layout(
                &self.session_name,
                &tab_names,
                self.active_window,
                win_zoomed,
                self.cols,
            );
            if layout::plus_button_hit(bar.plus.as_ref(), x) {
                return layout::Hit::NewWindowButton;
            }
            return match layout::tab_hit(&bar.tabs, x) {
                Some(w) => layout::Hit::Tab(w),
                None => layout::Hit::None,
            };
        }

        // 3. Content area: dispatch by active window's zoom state.
        let win = match self.windows.get(self.active_window) {
            Some(w) => w,
            None => return layout::Hit::None,
        };
        if win.zoomed {
            if regions.content_area.contains(x, y) {
                return layout::Hit::Pane(win.focused);
            }
            return layout::Hit::None;
        }
        layout::hit_within_content(&win.root, regions.content_area, col, row)
    }

    #[cfg(test)]
    pub fn hit_for_test(&self, col: u16, row: u16) -> layout::Hit {
        self.hit(col, row)
    }

    /// Dispatch a `Hit` (from `Session::hit`) into Session state
    /// mutations. Shared by `handle_mouse` (production click path) and
    /// tests (which call this directly with a synthesized `Hit`).
    /// Returns whether a divider drag should be initiated for this hit
    /// — Some(SplitPath) for a Divider hit, None otherwise.
    pub(crate) fn handle_hit(&mut self, hit: layout::Hit) -> Option<layout::SplitPath> {
        let out = match hit {
            layout::Hit::Pane(id) => {
                self.focus(id);
                None
            }
            layout::Hit::Tab(idx) => {
                if idx < self.windows.len() {
                    self.unzoom_active();
                    self.active_window = idx;
                    self.mark_snapshot_dirty();
                }
                None
            }
            layout::Hit::Divider(path) => Some(path),
            layout::Hit::SidebarEntry {
                window_index,
                pane_id,
            } => {
                // 1. Validate destination window exists.
                let dest_win = self.windows.get(window_index)?;
                // 2. Validate pane belongs to destination window's tree.
                if !layout::all_panes(&dest_win.root).contains(&pane_id) {
                    return None;
                }
                // 3. Unzoom currently active window.
                self.unzoom_active();
                // 4. Switch active.
                self.active_window = window_index;
                // 5. Focus the destination pane; clear destination zoom.
                if let Some(win) = self.windows.get_mut(window_index) {
                    win.focused = pane_id;
                    win.zoomed = false;
                }
                // 6. Cascade layout (zoom may have changed).
                self.relayout_all();
                self.mark_snapshot_dirty();
                None
            }
            // Dispatched in handle_mouse (needs a CommandEffect); never
            // reached here in production. Defensive no-op for match
            // exhaustiveness and direct test calls.
            layout::Hit::NewWindowButton => None,
            layout::Hit::None => None,
        };
        self.reconcile_menu();
        out
    }

    #[cfg(test)]
    pub fn active_zoomed_for_test(&self) -> bool {
        self.windows
            .get(self.active_window)
            .map(|w| w.zoomed)
            .unwrap_or(false)
    }

    /// Drive `unzoom_active` directly so a test can assert its dirty-marking
    /// contract independently of any follow-up split/new-window that might
    /// no-op or fail.
    #[cfg(test)]
    pub fn unzoom_active_for_test(&mut self) {
        self.unzoom_active();
    }

    /// Seed a pane's persisted agent-resume value, simulating a prior capture so
    /// `refresh_restore_meta` clearing behavior is observable.
    #[cfg(test)]
    pub fn set_pane_agent_resume_for_test(
        &mut self,
        id: PaneId,
        resume: crate::persist::AgentResume,
    ) {
        if let Some(pane) = self.panes.get_mut(&id) {
            pane.agent_resume = resume;
        }
    }

    /// Force a pane's agent classification (the `kind` that drives
    /// `refresh_restore_meta`'s clear-vs-capture branch).
    #[cfg(test)]
    pub fn set_pane_agent_kind_for_test(&mut self, id: PaneId, kind: Option<detect::Agent>) {
        if let Some(pane) = self.panes.get_mut(&id) {
            pane.agent = kind;
        }
    }

    #[cfg(test)]
    fn tab_bar_state(&self) -> (Vec<String>, bool) {
        let names: Vec<String> = self.windows.iter().map(|w| w.name.clone()).collect();
        let zoomed = self.windows.get(self.active_window).is_some_and(|w| w.zoomed);
        (names, zoomed)
    }

    #[cfg(test)]
    pub fn tab_regions_for_test(&self) -> Vec<layout::TabRegion> {
        let (names, zoomed) = self.tab_bar_state();
        layout::tab_layout(&self.session_name, &names, self.active_window, zoomed, self.cols)
    }

    #[cfg(test)]
    pub fn tab_bar_for_test(&self) -> layout::TabBar {
        let (names, zoomed) = self.tab_bar_state();
        layout::tab_bar_layout(&self.session_name, &names, self.active_window, zoomed, self.cols)
    }

    #[cfg(test)]
    pub fn window_names_for_test(&self) -> Vec<String> {
        self.windows.iter().map(|w| w.name.clone()).collect()
    }

    #[cfg(test)]
    pub fn set_window_name_pinned_for_test(&mut self, idx: usize, name: &str) {
        if let Some(w) = self.windows.get_mut(idx) {
            w.name = name.to_string();
            w.name_pinned = true;
        }
    }

    /// Refresh every window's name from its focused pane's foreground process.
    /// `resolve` maps a pid to a friendly name (injected so tests can fake it;
    /// production passes `crate::procinfo::process_name`). When `resolve` yields
    /// `None` (or there is no foreground pid) the window keeps its current name —
    /// no flicker to a placeholder. Returns true if any name changed.
    pub fn refresh_window_names(&mut self, resolve: impl Fn(i32) -> Option<String>) -> bool {
        let mut changed = false;
        for win in &mut self.windows {
            if win.name_pinned {
                continue; // user-set name; never auto-overwrite
            }
            let new = self
                .panes
                .get(&win.focused)
                .and_then(|p| p.runtime.foreground_pid())
                .and_then(&resolve);
            if let Some(new) = new {
                if new != win.name {
                    win.name = new;
                    changed = true;
                }
            }
        }
        if changed {
            self.mark_snapshot_dirty();
        }
        changed
    }

    /// Refresh per-pane restore metadata: cwd for EVERY pane, plus a lazy
    /// agent-transcript capture keyed on the foreground pid. Returns true if any
    /// pane's cwd or agent_resume changed (→ caller marks the snapshot dirty).
    /// Resolvers are injected for testability (prod: `procinfo::pid_cwd` and
    /// `procinfo::agent_transcript`).
    pub fn refresh_restore_meta(
        &mut self,
        resolve_cwd: impl Fn(i32) -> Option<std::path::PathBuf>,
        resolve_transcript: impl Fn(i32, detect::Agent) -> Option<(std::path::PathBuf, String)>,
    ) -> bool {
        let mut changed = false;
        // Collect (id, fg_pid, agent_kind) first to avoid borrow conflicts.
        let work: Vec<(PaneId, Option<i32>, Option<detect::Agent>)> = self
            .panes
            .iter()
            .map(|(id, p)| (*id, p.runtime.foreground_pid(), p.agent))
            .collect();
        for (id, fg_pid, kind) in work {
            let Some(pane) = self.panes.get_mut(&id) else { continue };
            // cwd: every pane, but only when a foreground pid exists to resolve.
            if let Some(pid) = fg_pid {
                if let Some(dir) = resolve_cwd(pid) {
                    let s = dir.to_string_lossy().to_string();
                    if pane.cwd.as_deref() != Some(s.as_str()) {
                        pane.cwd = Some(s);
                        changed = true;
                    }
                }
            }
            // agent transcript: lazy, pid-keyed. This MUST run for every pane —
            // including panes with no current foreground pid — so a pane that
            // stops being an agent gets its stale `agent_resume` cleared even
            // during a transient missing-foreground window.
            match (kind, fg_pid) {
                (Some(k), Some(pid)) if pane.captured_for_pid != Some(pid) => {
                    let resume = match resolve_transcript(pid, k) {
                        Some((path, sid)) => match k {
                            detect::Agent::Claude => crate::persist::AgentResume::Claude {
                                session_id: sid,
                                transcript: path.to_string_lossy().to_string(),
                            },
                            detect::Agent::Codex => crate::persist::AgentResume::Codex {
                                session_id: sid,
                                transcript: path.to_string_lossy().to_string(),
                            },
                        },
                        None => crate::persist::AgentResume::None,
                    };
                    if pane.agent_resume != resume {
                        pane.agent_resume = resume;
                        changed = true;
                    }
                    pane.captured_for_pid = Some(pid);
                }
                (Some(_), _) => {
                    // Still classified as an agent. If we have a fresh pid it was
                    // already handled above (or is unchanged); if the foreground
                    // pid is momentarily gone, leave the existing resume as-is —
                    // a transient missing pid must not clear a valid resume.
                }
                (None, _) => {
                    // Pane is no longer an agent; clear any stale capture. Runs
                    // regardless of foreground pid presence.
                    if pane.agent_resume != crate::persist::AgentResume::None {
                        pane.agent_resume = crate::persist::AgentResume::None;
                        changed = true;
                    }
                    pane.captured_for_pid = None;
                }
            }
        }
        changed
    }

    /// Run one agent-detection pass over every pane (not just focused —
    /// background Claude panes count for the sidebar). Returns true if
    /// any pane's `agent_state` flipped this tick.
    ///
    /// `resolve_name` matches the `Fn(i32) -> Option<String>` shape that
    /// `refresh_window_names` uses, so tests can inject a deterministic
    /// resolver.
    pub fn refresh_agent_states(
        &mut self,
        resolve_name: impl Fn(i32) -> Option<String>,
    ) -> RefreshOutcome {
        let mut outcome = RefreshOutcome::default();
        // Sentinel-file gate (vs env var): survives across server restarts you
        // don't control, and works regardless of how the server was launched.
        // `touch /tmp/dvlpr-detect.enable` to turn on, `rm` to turn off — the
        // next 500ms tick picks it up.
        let debug = std::path::Path::new("/tmp/dvlpr-detect.enable").exists();
        // The set of panes in the currently-focused window. Used to decide
        // whether a finishing agent becomes Done (unfocused) or Idle (focused),
        // and to clear Done once its window is focused. `.get()` so a transient
        // empty/clamped window index can never panic the daemon.
        let focused_panes: std::collections::HashSet<crate::layout::PaneId> = self
            .windows
            .get(self.active_window)
            .map(|w| layout::all_panes(&w.root).into_iter().collect())
            .unwrap_or_default();
        for (pane_id, pane) in self.panes.iter_mut() {
            let prev_state = pane.agent_state;

            // Step 1: Identify foreground (cached, with reset on change,
            // retry on resolver failure).
            let pid_opt = pane.runtime.foreground_pid();
            match pid_opt {
                None => {
                    if pane.agent_id_pid.is_some()
                        || pane.agent.is_some()
                        || pane.agent_state != detect::AgentState::Idle
                        || pane.idle_streak != 0
                    {
                        pane.agent_id_pid = None;
                        pane.agent = None;
                        pane.agent_state = detect::AgentState::Idle;
                        pane.idle_streak = 0;
                    }
                }
                Some(pid) => {
                    if Some(pid) != pane.agent_id_pid {
                        // Invalidate cache key FIRST, then reset state,
                        // then attempt resolve. Only commit cache key on
                        // successful resolve so a transient procinfo
                        // failure doesn't poison the cache against a
                        // returning pid.
                        pane.agent_id_pid = None;
                        pane.agent = None;
                        pane.agent_state = detect::AgentState::Idle;
                        pane.idle_streak = 0;

                        if let Some(name) = resolve_name(pid) {
                            pane.agent = detect::agent_for(&name);
                            pane.agent_id_pid = Some(pid);
                        }
                    }
                }
            }

            // Step 2: Skip non-agents (carry forward Idle).
            let agent = match pane.agent {
                Some(a) => a,
                None => {
                    if pane.agent_state != prev_state {
                        outcome.changed = true;
                    }
                    continue;
                }
            };

            // Step 3: Sample the whole active screen.
            //
            // The original design read only the bottom 20 rows ("tail") on
            // the assumption that the agent's current-state UI always sits
            // near the bottom. That holds for Claude's normal prompt box
            // and spinner — but NOT for full-screen takeover UIs like the
            // "trust this folder" safety check or AskUserQuestion menus,
            // which render at the TOP of the pane and leave the bottom
            // empty. A 20-row tail of those screens is all blank → classify
            // returns Idle while the user is staring at a blocked prompt.
            //
            // Reading the whole screen is cheap (one FFI call per cell, a
            // few milliseconds even for large panes, every 500ms) and is
            // unambiguously correct: the classifier's patterns are
            // structurally distinctive, so extra context can't false-match.
            let tail = pane.screen.tail_text(pane.screen.rows());

            // Step 4: Classify through the real agent's classifier.
            let candidate = detect::classify(agent, &tail);

            // Debug instrumentation: when the sentinel file exists, append
            // every agent pane's sampled tail + classify result to
            // /tmp/dvlpr-detect.log. Off by default; zero overhead when absent.
            if debug {
                use std::io::Write;
                if let Ok(mut f) = std::fs::OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open("/tmp/dvlpr-detect.log")
                {
                    let _ = writeln!(
                        f,
                        "--- pane={pane_id:?} agent={agent:?} prev={prev_state:?} candidate={candidate:?} ---\n{tail:?}\n",
                    );
                }
            }

            // Step 5: Stabilize (2-sample idle hysteresis) + Done lifecycle.
            let focused = focused_panes.contains(pane_id);
            match candidate {
                detect::AgentState::Working => {
                    pane.agent_state = detect::AgentState::Working;
                    pane.idle_streak = 0;
                }
                detect::AgentState::Blocked => {
                    pane.agent_state = detect::AgentState::Blocked;
                    pane.idle_streak = 0;
                }
                detect::AgentState::Idle => match pane.agent_state {
                    detect::AgentState::Working | detect::AgentState::Blocked => {
                        pane.idle_streak = pane.idle_streak.saturating_add(1);
                        if pane.idle_streak >= 2 {
                            // Confirmed finish: Done if the user isn't looking,
                            // Idle if its window is focused.
                            pane.agent_state = if focused {
                                detect::AgentState::Idle
                            } else {
                                detect::AgentState::Done
                            };
                            pane.idle_streak = 2;
                        }
                    }
                    detect::AgentState::Done => {
                        // Sticky while unfocused; clear the instant a tick sees
                        // the window focused. This single arm clears Done on
                        // EVERY focus-change path (window switch, tab/sidebar
                        // click, pane-exit clamp).
                        if focused {
                            pane.agent_state = detect::AgentState::Idle;
                        }
                        pane.idle_streak = 2;
                    }
                    detect::AgentState::Idle => {
                        pane.idle_streak = 2;
                    }
                },
                // classify() never returns Done; ignore rather than panic.
                detect::AgentState::Done => {}
            }

            if crate::sound::should_play_blocked(prev_state, pane.agent_state) {
                outcome.blocked_transitions.push(*pane_id);
            }

            if pane.agent_state != prev_state {
                outcome.changed = true;
            }
        }
        outcome
    }

    /// Probe each Claude/Codex pane's session label and current git
    /// branch from its foreground PID's cwd. Gated to once per 2 s
    /// per pane via `meta_last_refresh`. Returns true if any cached
    /// field changed.
    pub fn refresh_agent_meta(
        &mut self,
        resolve_cwd: impl Fn(i32) -> Option<std::path::PathBuf>,
    ) -> bool {
        const INTERVAL: std::time::Duration = std::time::Duration::from_secs(2);
        let mut changed = false;
        for pane in self.panes.values_mut() {
            // Only Claude/Codex panes are interesting for v1.
            let agent = match pane.agent {
                Some(a) => a,
                None => continue,
            };
            if pane.meta_last_refresh.elapsed() < INTERVAL {
                continue;
            }
            pane.meta_last_refresh = std::time::Instant::now();

            let pid = match pane.runtime.foreground_pid() {
                Some(p) => p,
                None => continue,
            };
            let cwd = match resolve_cwd(pid) {
                Some(c) => c,
                None => continue,
            };

            let new_branch = crate::agent_meta::branch::detect_branch(&cwd);
            if new_branch != pane.branch {
                pane.branch = new_branch;
                changed = true;
            }

            let new_label = match agent {
                crate::detect::Agent::Claude => crate::agent_meta::claude::session_label(&cwd),
                crate::detect::Agent::Codex => pane.screen.title(),
            };
            if new_label != pane.session_label {
                pane.session_label = new_label;
                changed = true;
            }
        }
        changed
    }

    /// Return one `AgentEntry` per Claude pane in the session, in stable
    /// order (by window index, then by pane order within the window's
    /// layout tree). Drives the sidebar render and click dispatch.
    pub fn agent_entries(&self) -> Vec<AgentEntry> {
        let mut out = Vec::new();
        for (wi, win) in self.windows.iter().enumerate() {
            for pane_id in layout::all_panes(&win.root) {
                if let Some(pane) = self.panes.get(&pane_id) {
                    if let Some(agent) = pane.agent {
                        out.push(AgentEntry {
                            session_name: self.session_name.clone(),
                            window_index: wi,
                            pane_id,
                            agent,
                            state: pane.agent_state,
                            session_label: pane.session_label.clone(),
                            branch: pane.branch.clone(),
                        });
                    }
                }
            }
        }
        out
    }

    /// Recompute the dragged split's ratio from the pointer position and relayout.
    /// Operates on the explicit `window` the drag started in (not necessarily the
    /// active one). If that window is gone or the path no longer leads to a split,
    /// it is a harmless no-op (`split_area_at`/`set_ratio` return None/false).
    fn resize_divider(&mut self, window: usize, path: &SplitPath, col: u16, row: u16) {
        let content =
            layout::compute_regions(self.viewport(), self.sidebar_visible, self.sidebar_width)
                .content_area;
        let Some(win) = self.windows.get_mut(window) else {
            return;
        };
        let Some((area, dir)) = layout::split_area_at(&win.root, content, path) else {
            return;
        };
        // Pointer to 0-based; ratio is the fraction of the available (non-divider)
        // axis that falls to the first child.
        let x = col.saturating_sub(1);
        let y = row.saturating_sub(1);
        let ratio = match dir {
            SplitDir::Vertical => {
                let avail = area.w.saturating_sub(1).max(1) as f32;
                (x.saturating_sub(area.x) as f32) / avail
            }
            SplitDir::Horizontal => {
                let avail = area.h.saturating_sub(1).max(1) as f32;
                (y.saturating_sub(area.y) as f32) / avail
            }
        };
        layout::set_ratio(&mut win.root, path, ratio); // clamps to [0.05, 0.95]
        self.relayout_all();
        self.mark_snapshot_dirty();
    }

    #[cfg(test)]
    pub fn window_pane_ids(&self, idx: usize) -> Vec<PaneId> {
        layout::all_panes(&self.windows[idx].root)
    }

    #[cfg(test)]
    pub fn pane_cwd_for_test(&self, id: PaneId) -> Option<String> {
        self.panes.get(&id).and_then(|p| p.cwd.clone())
    }

    #[cfg(test)]
    pub fn focus_for_test(&mut self, id: PaneId) {
        self.focus(id);
    }

    #[cfg(test)]
    pub fn active_window_for_test(&self) -> usize {
        self.active_window
    }

    #[cfg(test)]
    pub fn content_area_for_test(&self) -> layout::Rect {
        layout::compute_regions(self.viewport(), self.sidebar_visible, self.sidebar_width)
            .content_area
    }

    #[cfg(test)]
    pub fn menu_anchor_for_test(&self) -> Option<(u16, u16)> {
        self.menu.as_ref().map(|m| m.anchor)
    }

    #[cfg(test)]
    pub fn menu_pane_for_test(&self) -> Option<PaneId> {
        self.menu.as_ref().and_then(|m| match m.kind {
            crate::menu::MenuKind::Pane { pane_id } => Some(pane_id),
            crate::menu::MenuKind::Tab { .. } => None,
        })
    }

    #[cfg(test)]
    pub fn menu_highlighted_for_test(&self) -> Option<usize> {
        self.menu.as_ref().map(|m| m.highlighted)
    }
}

/// Pure decision function: does the closed-menu right-click branch open
/// a pane menu for this hit? Returns `Some(pane_id)` only for
/// `Hit::Pane(_)`; every other hit (Tab, Divider, SidebarEntry,
/// NewWindowButton, None) returns `None`. Extracted so the variant
/// drop rule is unit-testable without requiring a real
/// `Hit::SidebarEntry` (which depends on detected agent state).
pub fn should_open_pane_menu(hit: layout::Hit) -> Option<layout::PaneId> {
    if let layout::Hit::Pane(id) = hit {
        Some(id)
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Command;
    use crate::input::{MouseEvent, MouseKind};
    use crate::layout::SplitPath;
    use std::time::Duration;

    // NOTE: the missing-cwd fallback is intentionally NOT covered by a
    // spawn-based integration test. Exercising it end-to-end requires spawning a
    // shell in `$HOME` (the fallback target), and the test sandbox denies
    // spawning a PTY child outside the workspace tree (EPERM) even though it
    // works in production. The behavior is covered by
    // `effective_cwd_uses_existing_dir_else_home` (the fallback decision) and
    // `pane::tests::spawn_uses_given_cwd` (the spawn honors the resolved dir);
    // `spawn_pane` is trivial glue between the two.

    async fn build_session_with_one_pane() -> (
        Session,
        crate::layout::PaneId,
        tokio::sync::mpsc::UnboundedReceiver<crate::pane::PaneOutput>,
    ) {
        let cwd = std::env::current_dir()
            .unwrap()
            .to_string_lossy()
            .to_string();
        let (mut session, pane_id, rx) = Session::new(
            "test".to_string(),
            vec!["sh".to_string(), "-c".to_string(), "sleep 30".to_string()],
            cwd,
            80,
            10,
            crate::theme::Theme::default(),
            crate::config::KeySpec::Ctrl('b'),
            crate::config::KeyMap::default(),
            crate::layout::SIDEBAR_WIDTH_DEFAULT,
        )
        .expect("Session::new");
        // Pin a hidden sidebar as the geometry baseline so pane-width / cursor
        // assertions stay stable. The production default is now visible (see
        // `new_session_defaults_to_visible_sidebar`); sidebar-specific tests
        // toggle it on explicitly.
        session.sidebar_visible = false;
        session.relayout_all();
        (session, pane_id, rx)
    }

    fn test_session() -> Session {
        let (s, _id, _rx) = Session::new(
            "test".to_string(),
            vec!["sh".to_string(), "-c".to_string(), "sleep 30".to_string()],
            ".".to_string(),
            40,
            12,
            crate::theme::Theme::default(),
            crate::config::KeySpec::Ctrl('b'),
            crate::config::KeyMap::default(),
            crate::layout::SIDEBAR_WIDTH_DEFAULT,
        )
        .expect("session");
        s
    }

    #[tokio::test]
    async fn refresh_agent_states_marks_pane_working_after_busy_sample() {
        let (mut session, pane_id, _rx) = build_session_with_one_pane().await;
        session.feed(pane_id, b"esc to interrupt\n");
        let outcome = session.refresh_agent_states(|_pid| Some("claude".to_string()));
        assert!(outcome.changed, "first refresh should flip pane state");
        let pane = session.panes.get(&pane_id).expect("pane present");
        assert_eq!(pane.agent, Some(detect::Agent::Claude));
        assert_eq!(pane.agent_state, detect::AgentState::Working);
    }

    #[tokio::test]
    async fn refresh_agent_states_marks_pane_blocked_after_blocked_sample() {
        let (mut session, pane_id, _rx) = build_session_with_one_pane().await;
        session.feed(pane_id, b"Do you want to proceed?\n\xe2\x9d\xaf 1. Yes\n");
        let outcome = session.refresh_agent_states(|_pid| Some("claude".to_string()));
        assert!(outcome.changed);
        let pane = session.panes.get(&pane_id).expect("pane present");
        assert_eq!(pane.agent_state, detect::AgentState::Blocked);
    }

    #[tokio::test]
    async fn refresh_agent_states_does_not_flip_to_idle_on_single_idle_sample() {
        let (mut session, pane_id, _rx) = build_session_with_one_pane().await;
        session.feed(pane_id, b"esc to interrupt\n");
        session.refresh_agent_states(|_pid| Some("claude".to_string()));
        session.feed(pane_id, b"\x1b[2J\x1b[H");
        session.refresh_agent_states(|_pid| Some("claude".to_string()));
        let pane = session.panes.get(&pane_id).unwrap();
        assert_eq!(pane.agent_state, detect::AgentState::Working);
        assert_eq!(pane.idle_streak, 1);
    }

    #[tokio::test]
    async fn refresh_agent_states_flips_to_idle_on_two_consecutive_idle_samples() {
        let (mut session, pane_id, _rx) = build_session_with_one_pane().await;
        session.feed(pane_id, b"esc to interrupt\n");
        session.refresh_agent_states(|_pid| Some("claude".to_string()));
        session.feed(pane_id, b"\x1b[2J\x1b[H");
        session.refresh_agent_states(|_pid| Some("claude".to_string())); // streak=1
        session.refresh_agent_states(|_pid| Some("claude".to_string())); // streak=2 → Idle
        let pane = session.panes.get(&pane_id).unwrap();
        assert_eq!(pane.agent_state, detect::AgentState::Idle);
    }

    #[tokio::test]
    async fn refresh_agent_states_working_to_blocked_is_immediate() {
        let (mut session, pane_id, _rx) = build_session_with_one_pane().await;
        session.feed(pane_id, b"esc to interrupt\n");
        session.refresh_agent_states(|_pid| Some("claude".to_string()));
        session.feed(pane_id, b"\x1b[2J\x1b[HDo you want to proceed?\nYes / No\n");
        session.refresh_agent_states(|_pid| Some("claude".to_string()));
        let pane = session.panes.get(&pane_id).unwrap();
        assert_eq!(pane.agent_state, detect::AgentState::Blocked);
        assert_eq!(pane.idle_streak, 0, "non-idle transitions reset streak");
    }

    #[tokio::test]
    async fn refresh_agent_states_returns_false_when_nothing_changed() {
        let (mut session, pane_id, _rx) = build_session_with_one_pane().await;
        let outcome = session.refresh_agent_states(|_pid| Some("claude".to_string()));
        assert!(!outcome.changed);
        let _ = pane_id;
    }

    #[tokio::test]
    async fn refresh_agent_states_ignores_non_agent_panes() {
        let (mut session, pane_id, _rx) = build_session_with_one_pane().await;
        session.feed(pane_id, b"esc to interrupt\n");
        let outcome = session.refresh_agent_states(|_pid| Some("zsh".to_string()));
        assert!(!outcome.changed);
        let pane = session.panes.get(&pane_id).unwrap();
        assert!(pane.agent.is_none());
        assert_eq!(pane.agent_state, detect::AgentState::Idle);
    }

    #[tokio::test]
    async fn refresh_agent_states_retries_resolver_on_transient_failure() {
        let (mut session, pane_id, _rx) = build_session_with_one_pane().await;
        session.feed(pane_id, b"esc to interrupt\n");
        let outcome = session.refresh_agent_states(|_pid| None);
        assert!(!outcome.changed);
        let pane = session.panes.get(&pane_id).unwrap();
        assert!(pane.agent.is_none());
        assert!(pane.agent_id_pid.is_none(), "cache key NOT poisoned");

        let outcome = session.refresh_agent_states(|_pid| Some("claude".to_string()));
        assert!(outcome.changed);
        let pane = session.panes.get(&pane_id).unwrap();
        assert_eq!(pane.agent_state, detect::AgentState::Working);
    }

    #[tokio::test]
    async fn session_renders_pane_output_as_full_frame() {
        let (mut session, pane_id, mut rx) = Session::new(
            "test".into(),
            vec!["sh".into(), "-c".into(), "printf READY; sleep 5".into()],
            ".".into(),
            40,
            10,
            crate::theme::Theme::default(),
            crate::config::KeySpec::Ctrl('b'),
            crate::config::KeyMap::default(),
            crate::layout::SIDEBAR_WIDTH_DEFAULT,
        )
        .expect("session");

        // Pump pane output into the session until a rendered frame shows READY.
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        let mut found = false;
        while let Ok(Some(out)) = tokio::time::timeout_at(deadline, rx.recv()).await {
            match out {
                PaneOutput::Bytes(b) => session.feed(pane_id, &b),
                PaneOutput::Exited => break,
            }
            if String::from_utf8_lossy(&session.render()).contains("READY") {
                found = true;
                break;
            }
        }
        assert!(found, "expected a rendered frame containing READY");
        // The status bar is always present; the frame still starts with clear+home.
        assert!(session.render().starts_with(b"\x1b[2J\x1b[H"));
        // Tear the pane down off-loop rather than dropping a live runtime inline.
        // The test module can see Session's private fields, so this works without
        // `pane_exited` (which is implemented and exercised in Task 4).
        if let Some(p) = session.panes.remove(&pane_id) {
            p.runtime.close();
        }
    }

    #[tokio::test]
    async fn input_and_resize_do_not_panic_and_keep_rendering() {
        let (mut session, pane_id, _rx) = Session::new(
            "test".into(),
            vec!["sh".into(), "-c".into(), "sleep 5".into()],
            ".".into(),
            40,
            10,
            crate::theme::Theme::default(),
            crate::config::KeySpec::Ctrl('b'),
            crate::config::KeyMap::default(),
            crate::layout::SIDEBAR_WIDTH_DEFAULT,
        )
        .expect("session");

        session.input(b"echo hi\n"); // routed to the focused pane's PTY
        session.resize(20, 6); // resizes the pane's PTY + screen
        let frame = session.render();
        assert!(frame.starts_with(b"\x1b[2J\x1b[H"));
        // The rendered frame is sized to the new viewport: 6 rows => 5 CRLF
        // separators between the 6 content rows.
        let crlfs = frame.windows(2).filter(|w| w == b"\r\n").count();
        assert_eq!(crlfs, 5);
        // Tear the pane down off-loop rather than dropping a live runtime inline.
        // The test module can see Session's private fields, so this works without
        // `pane_exited` (which is implemented and exercised in Task 4).
        if let Some(p) = session.panes.remove(&pane_id) {
            p.runtime.close();
        }
    }

    #[tokio::test]
    async fn pane_exit_removes_pane_and_empties_single_window_session() {
        let (mut session, pane_id, mut rx) = Session::new(
            "test".into(),
            vec!["sh".into(), "-c".into(), "true".into()],
            ".".into(),
            40,
            10,
            crate::theme::Theme::default(),
            crate::config::KeySpec::Ctrl('b'),
            crate::config::KeyMap::default(),
            crate::layout::SIDEBAR_WIDTH_DEFAULT,
        )
        .expect("session");

        // Drain until the pane reports it exited.
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        loop {
            match tokio::time::timeout_at(deadline, rx.recv()).await {
                Ok(Some(PaneOutput::Exited)) => break,
                Ok(Some(_)) => continue,
                _ => panic!("pane did not exit in time"),
            }
        }

        let removed = session.pane_exited(pane_id);
        assert_eq!(removed.len(), 1); // the exited pane's runtime is returned
        assert!(session.is_empty()); // its only window collapsed away

        // Tearing the returned runtime down off-loop must be safe.
        for rt in removed {
            rt.close();
        }

        // A second pane_exited for the same id is a harmless no-op.
        assert!(session.pane_exited(pane_id).is_empty());
    }

    #[tokio::test]
    async fn split_horizontal_spawns_a_second_pane_and_focuses_it() {
        let (mut session, first, _rx) = Session::new(
            "test".into(),
            vec!["sh".into(), "-c".into(), "sleep 5".into()],
            ".".into(),
            40,
            12,
            crate::theme::Theme::default(),
            crate::config::KeySpec::Ctrl('b'),
            crate::config::KeyMap::default(),
            crate::layout::SIDEBAR_WIDTH_DEFAULT,
        )
        .expect("session");

        let eff = session.apply_command(Command::SplitHorizontal);
        assert_eq!(eff.spawned.len(), 1, "a new pane was spawned");
        let (new_id, _new_rx) = eff.spawned.into_iter().next().unwrap();
        assert_ne!(new_id, first);
        // Active window now has two panes; the new one is focused.
        assert_eq!(session.active_pane_count(), 2);
        assert_eq!(session.focused_pane(), new_id);

        // Teardown: close both panes off-loop.
        for rt in session.shutdown() {
            rt.close();
        }
    }

    #[tokio::test]
    async fn split_is_rejected_when_too_small() {
        // 3-row viewport can't host a horizontal split (needs >= 5 rows).
        let (mut session, _first, _rx) = Session::new(
            "test".into(),
            vec!["sh".into(), "-c".into(), "sleep 5".into()],
            ".".into(),
            40,
            3,
            crate::theme::Theme::default(),
            crate::config::KeySpec::Ctrl('b'),
            crate::config::KeyMap::default(),
            crate::layout::SIDEBAR_WIDTH_DEFAULT,
        )
        .expect("session");
        let eff = session.apply_command(Command::SplitHorizontal);
        assert!(
            eff.spawned.is_empty(),
            "no pane spawned for a too-small split"
        );
        assert_eq!(session.active_pane_count(), 1);
        for rt in session.shutdown() {
            rt.close();
        }
    }

    #[tokio::test]
    async fn new_window_then_select_switches_active_window() {
        let (mut session, _first, _rx) = Session::new(
            "test".into(),
            vec!["sh".into(), "-c".into(), "sleep 5".into()],
            ".".into(),
            40,
            12,
            crate::theme::Theme::default(),
            crate::config::KeySpec::Ctrl('b'),
            crate::config::KeyMap::default(),
            crate::layout::SIDEBAR_WIDTH_DEFAULT,
        )
        .expect("session");
        let eff = session.apply_command(Command::NewWindow);
        assert_eq!(eff.spawned.len(), 1);
        assert_eq!(session.window_count(), 2);
        assert_eq!(session.active_window_index(), 1);
        // Back to window 1 (1-based) => index 0.
        session.apply_command(Command::SelectWindow(1));
        assert_eq!(session.active_window_index(), 0);
        for rt in session.shutdown() {
            rt.close();
        }
    }

    #[tokio::test]
    async fn close_pane_collapses_split_and_returns_a_runtime() {
        let (mut session, _first, _rx) = Session::new(
            "test".into(),
            vec!["sh".into(), "-c".into(), "sleep 5".into()],
            ".".into(),
            40,
            12,
            crate::theme::Theme::default(),
            crate::config::KeySpec::Ctrl('b'),
            crate::config::KeyMap::default(),
            crate::layout::SIDEBAR_WIDTH_DEFAULT,
        )
        .expect("session");
        session.apply_command(Command::SplitVertical);
        assert_eq!(session.active_pane_count(), 2);
        let eff = session.apply_command(Command::ClosePane);
        assert_eq!(eff.closed.len(), 1, "the closed pane's runtime is returned");
        assert_eq!(session.active_pane_count(), 1);
        for rt in eff.closed {
            rt.close();
        }
        for rt in session.shutdown() {
            rt.close();
        }
    }

    #[tokio::test]
    async fn detach_command_sets_the_detach_flag() {
        let (mut session, _first, _rx) = Session::new(
            "test".into(),
            vec!["sh".into(), "-c".into(), "sleep 5".into()],
            ".".into(),
            40,
            12,
            crate::theme::Theme::default(),
            crate::config::KeySpec::Ctrl('b'),
            crate::config::KeyMap::default(),
            crate::layout::SIDEBAR_WIDTH_DEFAULT,
        )
        .expect("session");
        let eff = session.apply_command(Command::Detach);
        assert!(eff.detach);
        assert!(eff.spawned.is_empty() && eff.closed.is_empty());
        for rt in session.shutdown() {
            rt.close();
        }
    }

    #[tokio::test]
    async fn pane_exit_keeps_focus_when_a_non_focused_pane_closes() {
        let (mut session, first, _rx) = Session::new(
            "test".into(),
            vec!["sh".into(), "-c".into(), "sleep 5".into()],
            ".".into(),
            40,
            24,
            crate::theme::Theme::default(),
            crate::config::KeySpec::Ctrl('b'),
            crate::config::KeyMap::default(),
            crate::layout::SIDEBAR_WIDTH_DEFAULT,
        )
        .expect("session");
        // Two horizontal splits => three panes in one window; newest pane is focused.
        session.apply_command(Command::SplitHorizontal);
        session.apply_command(Command::SplitHorizontal);
        let focused = session.focused_pane();
        assert_eq!(session.active_pane_count(), 3);
        assert_ne!(focused, first);

        // Close `first` — a NON-focused pane. Focus must NOT move.
        for rt in session.pane_exited(first) {
            rt.close();
        }
        assert_eq!(session.active_pane_count(), 2);
        assert_eq!(
            session.focused_pane(),
            focused,
            "focus must stay put when a non-focused pane closes"
        );
        for rt in session.shutdown() {
            rt.close();
        }
    }

    #[tokio::test]
    async fn click_focuses_the_pane_under_the_pointer() {
        let (mut session, first, _rx) = Session::new(
            "test".into(),
            vec!["sh".into(), "-c".into(), "sleep 5".into()],
            ".".into(),
            41,
            12,
            crate::theme::Theme::default(),
            crate::config::KeySpec::Ctrl('b'),
            crate::config::KeyMap::default(),
            crate::layout::SIDEBAR_WIDTH_DEFAULT,
        )
        .expect("session");
        // Vertical split: left = first (focused after split is the NEW right pane).
        session.apply_command(Command::SplitVertical);
        let focused_after_split = session.focused_pane();
        assert_ne!(focused_after_split, first);

        // Click column 1 (left pane = the original `first`). 1-based coords.
        let mut drag: Option<(usize, SplitPath)> = None;
        let _ = session.handle_mouse(
            MouseEvent {
                button: 0,
                col: 1,
                row: 1,
                kind: MouseKind::Press,
            },
            &mut drag,
        );
        assert_eq!(
            session.focused_pane(),
            first,
            "clicking the left pane focuses it"
        );
        assert!(drag.is_none());
        for rt in session.shutdown() {
            rt.close();
        }
    }

    #[tokio::test]
    async fn drag_on_a_divider_changes_the_split_ratio() {
        let (mut session, _first, _rx) = Session::new(
            "test".into(),
            vec!["sh".into(), "-c".into(), "sleep 5".into()],
            ".".into(),
            41,
            12,
            crate::theme::Theme::default(),
            crate::config::KeySpec::Ctrl('b'),
            crate::config::KeyMap::default(),
            crate::layout::SIDEBAR_WIDTH_DEFAULT,
        )
        .expect("session");
        session.apply_command(Command::SplitVertical);
        // The root divider sits at the middle column. Press on it, then drag left.
        let mut drag: Option<(usize, SplitPath)> = None;
        // avail = 41 - 1 = 40; ratio 0.5 => first_w 20 => divider at x 20 => col 21.
        let _ = session.handle_mouse(
            MouseEvent {
                button: 0,
                col: 21,
                row: 3,
                kind: MouseKind::Press,
            },
            &mut drag,
        );
        // Active window is 0, root divider path is empty.
        assert_eq!(
            drag,
            Some((0, vec![])),
            "press on the root divider records a drag"
        );
        // Drag to column 11 (x 10): first pane should shrink to ~10 cells.
        let _ = session.handle_mouse(
            MouseEvent {
                button: 0,
                col: 11,
                row: 3,
                kind: MouseKind::Drag,
            },
            &mut drag,
        );
        let left_w = session.active_pane_rects()[0].1.w;
        assert!(
            left_w < 20,
            "left pane shrank after dragging the divider left (was 20, now {left_w})"
        );
        // Release clears the drag.
        let _ = session.handle_mouse(
            MouseEvent {
                button: 0,
                col: 11,
                row: 3,
                kind: MouseKind::Release,
            },
            &mut drag,
        );
        assert!(drag.is_none());
        for rt in session.shutdown() {
            rt.close();
        }
    }

    #[tokio::test]
    async fn compose_returns_grid_matching_viewport_dims() {
        let (session, _id, _rx) = Session::new(
            "test".into(),
            vec!["cat".into()],
            ".".into(),
            30,
            8,
            crate::theme::Theme::default(),
            crate::config::KeySpec::Ctrl('b'),
            crate::config::KeyMap::default(),
            crate::layout::SIDEBAR_WIDTH_DEFAULT,
        )
        .unwrap();
        let grid = session.compose();
        assert_eq!(grid.dims(), (30, 8));
        assert_eq!(grid.cells.len(), 30 * 8);
        // render() must still produce a full frame (clear+home prefix).
        assert!(session.render().starts_with(b"\x1b[2J\x1b[H"));
    }

    #[tokio::test]
    async fn refresh_window_names_updates_only_on_change() {
        let (mut session, _id, _rx) = Session::new(
            "test".into(),
            vec!["cat".into()],
            ".".into(),
            30,
            8,
            crate::theme::Theme::default(),
            crate::config::KeySpec::Ctrl('b'),
            crate::config::KeyMap::default(),
            crate::layout::SIDEBAR_WIDTH_DEFAULT,
        )
        .unwrap();
        // First resolve to "claude": name changes -> true.
        assert!(session.refresh_window_names(|_pid| Some("claude".to_string())));
        // Same value again: no change -> false.
        assert!(!session.refresh_window_names(|_pid| Some("claude".to_string())));
        // Resolver returns None: keep current name -> false.
        assert!(!session.refresh_window_names(|_pid| None));
    }

    #[tokio::test]
    async fn zoom_shows_only_focused_pane_then_restores() {
        let (mut session, _first, _rx) = Session::new(
            "test".into(),
            vec!["cat".into()],
            ".".into(),
            40,
            12,
            crate::theme::Theme::default(),
            crate::config::KeySpec::Ctrl('b'),
            crate::config::KeyMap::default(),
            crate::layout::SIDEBAR_WIDTH_DEFAULT,
        )
        .unwrap();
        session.apply_command(Command::SplitVertical); // now two panes
        assert_eq!(session.active_pane_rects().len(), 2);

        session.apply_command(Command::ToggleZoom);
        // Zoomed: exactly one rect, the focused pane, at the full content area.
        let zoomed = session.active_pane_rects();
        assert_eq!(zoomed.len(), 1);
        assert_eq!(zoomed[0].0, session.focused_pane());
        let content = crate::layout::content_area(
            crate::layout::Rect {
                x: 0,
                y: 0,
                w: 40,
                h: 12,
            },
            session.window_count(),
        );
        assert_eq!(zoomed[0].1, content);

        session.apply_command(Command::ToggleZoom); // unzoom
        assert_eq!(session.active_pane_rects().len(), 2);
    }

    #[tokio::test]
    async fn split_auto_unzooms() {
        let (mut session, _first, _rx) = Session::new(
            "test".into(),
            vec!["cat".into()],
            ".".into(),
            40,
            12,
            crate::theme::Theme::default(),
            crate::config::KeySpec::Ctrl('b'),
            crate::config::KeyMap::default(),
            crate::layout::SIDEBAR_WIDTH_DEFAULT,
        )
        .unwrap();
        session.apply_command(Command::SplitVertical);
        session.apply_command(Command::ToggleZoom);
        assert_eq!(session.active_pane_rects().len(), 1); // zoomed
        session.apply_command(Command::SplitVertical); // layout change -> unzoom
        assert!(session.active_pane_rects().len() >= 2);
    }

    #[tokio::test]
    async fn focused_pane_exit_in_active_window_auto_unzooms() {
        let (mut session, _first, _rx) = Session::new(
            "test".into(),
            vec!["cat".into()],
            ".".into(),
            40,
            12,
            crate::theme::Theme::default(),
            crate::config::KeySpec::Ctrl('b'),
            crate::config::KeyMap::default(),
            crate::layout::SIDEBAR_WIDTH_DEFAULT,
        )
        .unwrap();
        session.apply_command(Command::SplitVertical); // two panes in the active window
        session.apply_command(Command::ToggleZoom);
        assert!(session.active_zoomed_for_test());
        // The focused pane's process exits on its own (not a ClosePane command).
        let focused = session.focused_pane();
        let _ = session.pane_exited(focused);
        // Parity with ClosePane: the active window auto-unzooms so the survivor shows.
        assert!(!session.active_zoomed_for_test());
    }

    #[tokio::test]
    async fn background_window_pane_exit_does_not_unzoom_active() {
        let (mut session, _first, _rx) = Session::new(
            "test".into(),
            vec!["cat".into()],
            ".".into(),
            40,
            12,
            crate::theme::Theme::default(),
            crate::config::KeySpec::Ctrl('b'),
            crate::config::KeyMap::default(),
            crate::layout::SIDEBAR_WIDTH_DEFAULT,
        )
        .unwrap();
        // Window 1 (index 0) gets a second pane; remember one of its pane ids.
        session.apply_command(Command::SplitVertical);
        let bg_pane = session.focused_pane();
        // Switch to a new window (index 1) and zoom it.
        session.apply_command(Command::NewWindow);
        session.apply_command(Command::ToggleZoom);
        assert!(session.active_zoomed_for_test());
        // A pane in the BACKGROUND window exits — must not disturb the active zoom.
        let _ = session.pane_exited(bg_pane);
        assert!(session.active_zoomed_for_test());
    }

    #[tokio::test]
    async fn window_switch_auto_unzooms() {
        let (mut session, _first, _rx) = Session::new(
            "test".into(),
            vec!["cat".into()],
            ".".into(),
            40,
            12,
            crate::theme::Theme::default(),
            crate::config::KeySpec::Ctrl('b'),
            crate::config::KeyMap::default(),
            crate::layout::SIDEBAR_WIDTH_DEFAULT,
        )
        .unwrap();
        session.apply_command(Command::SplitVertical);
        session.apply_command(Command::ToggleZoom);
        assert_eq!(session.active_pane_rects().len(), 1);
        session.apply_command(Command::NewWindow); // creates + switches to window 2
        session.apply_command(Command::SelectWindow(1)); // SelectWindow is 1-based; (1) -> index 0 (window 1)
                                                         // Window 1 was auto-unzoomed when we switched away.
        assert!(session.active_pane_rects().len() >= 2);
    }

    #[tokio::test]
    async fn hit_while_zoomed_resolves_body_to_focused_pane() {
        let (mut session, _first, _rx) = Session::new(
            "test".into(),
            vec!["cat".into()],
            ".".into(),
            40,
            12,
            crate::theme::Theme::default(),
            crate::config::KeySpec::Ctrl('b'),
            crate::config::KeyMap::default(),
            crate::layout::SIDEBAR_WIDTH_DEFAULT,
        )
        .unwrap();
        session.apply_command(Command::SplitVertical);
        session.apply_command(Command::ToggleZoom);
        // A click in the right half of the body (where the sibling used to be)
        // must resolve to the focused pane, not the hidden sibling or a divider.
        let hit = session.hit_for_test(35, 5);
        assert_eq!(hit, crate::layout::Hit::Pane(session.focused_pane()));
    }

    #[tokio::test]
    async fn hit_while_zoomed_still_switches_windows_via_tab_click() {
        let (mut session, _first, _rx) = Session::new(
            "test".into(),
            vec!["cat".into()],
            ".".into(),
            40,
            12,
            crate::theme::Theme::default(),
            crate::config::KeySpec::Ctrl('b'),
            crate::config::KeyMap::default(),
            crate::layout::SIDEBAR_WIDTH_DEFAULT,
        )
        .unwrap();
        session.apply_command(Command::NewWindow); // 2 windows -> tab bar has tabs
        session.apply_command(Command::SplitVertical); // window 2 now has 2 panes
        session.apply_command(Command::ToggleZoom); // zoom window 2
                                                    // Find a tab region's x and click it on the tab row (1-based coords).
        let tabs = session.tab_regions_for_test();
        assert!(tabs.len() >= 2, "expected at least two tab regions");
        let target = &tabs[0]; // window index 0's tab
                               // viewport height 12 -> bar is the last row (0-based y=11), 1-based = 12
        let tab_row_1based = layout::tab_row(
            layout::Rect {
                x: 0,
                y: 0,
                w: 40,
                h: 12,
            },
            session.window_count(),
        )
        .map(|y| y + 1)
        .unwrap_or(12);
        let hit = session.hit_for_test(target.x_start + 1, tab_row_1based);
        assert_eq!(hit, crate::layout::Hit::Tab(target.window));
    }

    #[tokio::test]
    async fn pane_exit_adjusts_active_window_for_index_after_before_and_at() {
        // Build 3 windows (indices 0,1,2). Exercise window collapse for an index
        // AFTER the active one (no shift), BEFORE it (shift down), and AT it (clamp).
        let (mut session, _first, _rx) = Session::new(
            "test".into(),
            vec!["sh".into(), "-c".into(), "sleep 5".into()],
            ".".into(),
            40,
            12,
            crate::theme::Theme::default(),
            crate::config::KeySpec::Ctrl('b'),
            crate::config::KeyMap::default(),
            crate::layout::SIDEBAR_WIDTH_DEFAULT,
        )
        .expect("session");
        session.apply_command(Command::NewWindow); // window 1
        session.apply_command(Command::NewWindow); // window 2 (active)
        assert_eq!(session.window_count(), 3);
        assert_eq!(session.active_window_index(), 2);

        // Collect each window's single pane id: [w0, w1, w2].
        let ids = session.window_focused_ids();
        assert_eq!(ids.len(), 3);

        // Make window 1 active so we can remove a window AFTER it.
        session.apply_command(Command::SelectWindow(2)); // 1-based => index 1
        assert_eq!(session.active_window_index(), 1);

        // (1) Close window 2's pane (index AFTER active 1): active stays 1.
        for rt in session.pane_exited(ids[2]) {
            rt.close();
        }
        assert_eq!(session.window_count(), 2);
        assert_eq!(session.active_window_index(), 1);

        // (2) Close window 0's pane (index BEFORE active 1): active shifts 1 -> 0.
        for rt in session.pane_exited(ids[0]) {
            rt.close();
        }
        assert_eq!(session.window_count(), 1);
        assert_eq!(session.active_window_index(), 0);

        // (3) Close the last remaining (AT active): session empties.
        let active_id = session.focused_pane();
        for rt in session.pane_exited(active_id) {
            rt.close();
        }
        assert!(session.is_empty());

        for rt in session.shutdown() {
            rt.close();
        }
    }

    #[tokio::test]
    async fn agent_entries_lists_only_claude_panes_in_window_order() {
        let (mut session, pane_id, _rx) = build_session_with_one_pane().await;
        assert!(session.agent_entries().is_empty());

        session.feed(pane_id, b"esc to interrupt\n");
        session.refresh_agent_states(|_pid| Some("claude".to_string()));

        let entries = session.agent_entries();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].pane_id, pane_id);
        assert_eq!(entries[0].window_index, 0);
        assert_eq!(entries[0].agent, detect::Agent::Claude);
        assert_eq!(entries[0].state, detect::AgentState::Working);
    }

    #[tokio::test]
    async fn agent_entries_uses_zero_based_window_index() {
        let (mut session, pane_id, _rx) = build_session_with_one_pane().await;
        session.feed(pane_id, b"esc to interrupt\n");
        session.refresh_agent_states(|_pid| Some("claude".to_string()));
        let entries = session.agent_entries();
        assert_eq!(entries[0].window_index, 0);
        assert_ne!(entries[0].window_index, 1);
    }

    #[tokio::test]
    async fn agent_entries_includes_session_name_on_every_entry() {
        let (mut session, pane_id, _rx) = build_session_with_one_pane().await;
        session.feed(pane_id, b"esc to interrupt\n");
        session.refresh_agent_states(|_pid| Some("claude".to_string()));
        let entries = session.agent_entries();
        assert_eq!(entries[0].session_name, "test");
    }

    #[tokio::test]
    async fn toggle_sidebar_flips_visible() {
        // The shared helper pins the sidebar hidden as its geometry baseline.
        let (mut session, _pane_id, _rx) = build_session_with_one_pane().await;
        assert!(!session.sidebar_visible, "helper baseline is hidden");
        session.toggle_sidebar();
        assert!(session.sidebar_visible);
        session.toggle_sidebar();
        assert!(!session.sidebar_visible);
    }

    #[tokio::test]
    async fn new_session_defaults_to_visible_sidebar() {
        // Production default: a freshly constructed session shows the sidebar
        // when the viewport is wide enough to keep usable content width.
        let cwd = std::env::current_dir().unwrap().to_string_lossy().to_string();
        let (session, _pane, _rx) = Session::new(
            "test".to_string(),
            vec!["sh".to_string(), "-c".to_string(), "sleep 30".to_string()],
            cwd,
            80,
            10,
            crate::theme::Theme::default(),
            crate::config::KeySpec::Ctrl('b'),
            crate::config::KeyMap::default(),
            crate::layout::SIDEBAR_WIDTH_DEFAULT,
        )
        .expect("Session::new");
        assert!(session.sidebar_visible, "sidebar open by default");
        // At 80 cols the sidebar is wide enough to actually render.
        let regions = layout::compute_regions(
            session.viewport(),
            session.sidebar_visible,
            session.sidebar_width,
        );
        assert!(regions.sidebar.is_some(), "sidebar region present at 80 cols");
    }

    #[tokio::test]
    async fn toggle_sidebar_resizes_panes_via_relayout_all() {
        let (mut session, pane_id, _rx) = build_session_with_one_pane().await;
        let initial_cols = session.panes.get(&pane_id).unwrap().screen.cols();
        session.toggle_sidebar();
        let after_cols = session.panes.get(&pane_id).unwrap().screen.cols();
        // Sidebar default is SIDEBAR_WIDTH_DEFAULT (26), not the old SIDEBAR_COLS (16).
        assert_eq!(after_cols, initial_cols - layout::SIDEBAR_WIDTH_DEFAULT);
        session.toggle_sidebar();
        assert_eq!(
            session.panes.get(&pane_id).unwrap().screen.cols(),
            initial_cols
        );
    }

    #[tokio::test]
    async fn command_toggle_sidebar_via_apply_command() {
        let (mut session, pane_id, _rx) = build_session_with_one_pane().await;
        let _eff = session.apply_command(crate::config::Command::ToggleSidebar);
        assert!(session.sidebar_visible);
        let after_cols = session.panes.get(&pane_id).unwrap().screen.cols();
        // initial cols from helper is 80; sidebar shrinks by SIDEBAR_WIDTH_DEFAULT (26).
        assert_eq!(after_cols, 80 - layout::SIDEBAR_WIDTH_DEFAULT);
    }

    #[tokio::test]
    async fn session_hit_returns_sidebar_entry_when_click_lands_in_sidebar_region() {
        let (mut session, pane_id, _rx) = build_session_with_one_pane().await;
        session.toggle_sidebar();
        session.feed(pane_id, b"esc to interrupt\n");
        session.refresh_agent_states(|_pid| Some("claude".to_string()));

        // Sidebar width is SIDEBAR_WIDTH_DEFAULT (26); starts at 0-based col 54 (1-based 55..80).
        // New layout: first entry at sidebar row 3 (0-based) = row 4 1-based (header=0, divider=1, blank=2).
        // h=2 so rows 4 and 5 (1-based) are both clickable. Col 70 is inside the sidebar.
        let hit = session.hit(70, 4);
        match hit {
            layout::Hit::SidebarEntry {
                window_index,
                pane_id: hit_pane,
            } => {
                assert_eq!(window_index, 0);
                assert_eq!(hit_pane, pane_id);
            }
            other => panic!("expected SidebarEntry, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn session_hit_returns_none_for_sidebar_header_or_separator_click() {
        let (mut session, pane_id, _rx) = build_session_with_one_pane().await;
        session.toggle_sidebar();
        session.feed(pane_id, b"esc to interrupt\n");
        session.refresh_agent_states(|_pid| Some("claude".to_string()));

        let hit = session.hit(70, 1); // header row
        assert_eq!(hit, layout::Hit::None);
        let hit = session.hit(70, 2); // separator row
        assert_eq!(hit, layout::Hit::None);
    }

    #[tokio::test]
    async fn handle_hit_sidebar_entry_switches_window_and_focuses_pane() {
        let (mut session, pane_id, _rx) = build_session_with_one_pane().await;
        let hit = layout::Hit::SidebarEntry {
            window_index: 0,
            pane_id,
        };
        session.handle_hit(hit);
        assert_eq!(session.active_window_index(), 0);
        assert_eq!(session.focused_pane(), pane_id);
    }

    #[tokio::test]
    async fn handle_hit_sidebar_entry_ignores_stale_pane_id() {
        let (mut session, _pane_id, _rx) = build_session_with_one_pane().await;
        let bogus_pane = 99_999_999;
        let active_before = session.active_window_index();
        session.handle_hit(layout::Hit::SidebarEntry {
            window_index: 0,
            pane_id: bogus_pane,
        });
        assert_eq!(session.active_window_index(), active_before);
    }

    #[tokio::test]
    async fn handle_hit_sidebar_entry_ignores_out_of_range_window_index() {
        let (mut session, pane_id, _rx) = build_session_with_one_pane().await;
        let active_before = session.active_window_index();
        session.handle_hit(layout::Hit::SidebarEntry {
            window_index: 99,
            pane_id,
        });
        assert_eq!(session.active_window_index(), active_before);
    }

    use crate::menu::{MenuKind, MenuState};

    #[tokio::test]
    async fn session_menu_starts_as_none() {
        let (session, _pane_id, _rx) = build_session_with_one_pane().await;
        assert!(!session.menu_open());
    }

    #[tokio::test]
    async fn session_menu_open_accessor_returns_menu_is_some() {
        let (mut session, _pane_id, _rx) = build_session_with_one_pane().await;
        session.set_menu_for_test(Some(MenuState {
            kind: MenuKind::Pane { pane_id: 1 },
            anchor: (10, 5),
            highlighted: 0,
        }));
        assert!(session.menu_open());
        session.set_menu_for_test(None);
        assert!(!session.menu_open());
    }

    fn right_press(col: u16, row: u16) -> MouseEvent {
        MouseEvent {
            button: 2,
            col,
            row,
            kind: MouseKind::Press,
        }
    }
    fn left_press(col: u16, row: u16) -> MouseEvent {
        MouseEvent {
            button: 0,
            col,
            row,
            kind: MouseKind::Press,
        }
    }

    #[tokio::test]
    async fn right_click_on_pane_opens_menu_and_focuses_that_pane() {
        let (mut session, _pane_id, _rx) = build_session_with_one_pane().await;
        session.apply_command(Command::SplitVertical);
        let panes = session.window_pane_ids(0);
        let second = *panes.last().unwrap();
        let first = panes[0];
        session.focus_for_test(first);
        assert_eq!(session.window_focused_ids()[0], first);

        let mut drag: Option<(usize, layout::SplitPath)> = None;
        let _ = session.handle_mouse(right_press(60, 5), &mut drag);

        assert!(session.menu_open());
        assert_eq!(session.window_focused_ids()[0], second);
    }

    #[tokio::test]
    async fn right_click_on_none_is_noop() {
        let (mut session, _pane_id, _rx) = build_session_with_one_pane().await;
        let mut drag = None;
        let _ = session.handle_mouse(right_press(0, 0), &mut drag);
        assert!(!session.menu_open());
    }

    #[tokio::test]
    async fn right_click_on_tab_does_not_switch_tab_v1() {
        let (mut session, _pane_id, _rx) = build_session_with_one_pane().await;
        session.apply_command(Command::NewWindow);
        assert_eq!(session.active_window_for_test(), 1);
        let mut drag = None;
        let _ = session.handle_mouse(right_press(5, 10), &mut drag);
        assert!(!session.menu_open());
        assert_eq!(session.active_window_for_test(), 1);
    }

    #[tokio::test]
    async fn right_click_during_divider_drag_is_dropped() {
        let (mut session, _pane_id, _rx) = build_session_with_one_pane().await;
        session.apply_command(Command::SplitVertical);
        let mut drag: Option<(usize, layout::SplitPath)> = Some((0usize, Vec::new()));
        let _ = session.handle_mouse(right_press(10, 5), &mut drag);
        assert!(!session.menu_open(), "menu must not open during a drag");
        assert!(drag.is_some(), "drag state must persist");
    }

    #[tokio::test]
    async fn left_click_on_item_dispatches_command_and_closes() {
        let (mut session, _pane_id, _rx) = build_session_with_one_pane().await;
        let panes = session.window_pane_ids(0);
        let first = panes[0];
        session.set_menu_for_test(Some(MenuState {
            kind: MenuKind::Pane { pane_id: first },
            anchor: (10, 5),
            highlighted: 0,
        }));
        let menu_rect = crate::menu::menu_rect((10, 5), session.content_area_for_test(), 4, 18);
        let click_col = (menu_rect.x + 5) + 1;
        let click_row = (menu_rect.y + 1) + 1;
        let mut drag = None;
        let _ = session.handle_mouse(left_press(click_col, click_row), &mut drag);
        assert!(!session.menu_open(), "menu closes on item click");
        assert_eq!(session.window_pane_ids(0).len(), 2);
    }

    #[tokio::test]
    async fn left_click_outside_menu_closes_without_dispatch() {
        let (mut session, _pane_id, _rx) = build_session_with_one_pane().await;
        let panes = session.window_pane_ids(0);
        let first = panes[0];
        session.set_menu_for_test(Some(MenuState {
            kind: MenuKind::Pane { pane_id: first },
            anchor: (10, 5),
            highlighted: 0,
        }));
        let mut drag = None;
        let _ = session.handle_mouse(left_press(70, 20), &mut drag);
        assert!(!session.menu_open());
        assert_eq!(session.window_pane_ids(0).len(), 1);
        assert!(drag.is_none());
    }

    #[tokio::test]
    async fn right_click_on_divider_does_not_initiate_drag_v1() {
        let (mut session, _pane_id, _rx) = build_session_with_one_pane().await;
        session.apply_command(Command::SplitVertical);
        let mut drag = None;
        let _ = session.handle_mouse(right_press(40, 5), &mut drag);
        assert!(!session.menu_open());
        assert!(
            drag.is_none(),
            "right-click on divider must not start a drag"
        );
    }

    #[tokio::test]
    async fn right_click_on_sidebar_entry_does_not_open_menu_v1() {
        let hit = layout::Hit::SidebarEntry {
            window_index: 0,
            pane_id: 99,
        };
        assert_eq!(crate::session::should_open_pane_menu(hit), None);

        let (mut session, _pane_id, _rx) = build_session_with_one_pane().await;
        session.toggle_sidebar(); // col 75 lands in the sidebar region
        let pre_focus = session.window_focused_ids();
        let mut drag = None;
        let _ = session.handle_mouse(right_press(75, 5), &mut drag);
        assert!(!session.menu_open());
        assert_eq!(session.window_focused_ids(), pre_focus);
    }

    #[test]
    fn should_open_pane_menu_returns_some_for_pane_hit() {
        let hit = layout::Hit::Pane(7);
        assert_eq!(crate::session::should_open_pane_menu(hit), Some(7));
    }

    #[test]
    fn should_open_pane_menu_returns_none_for_tab_hit() {
        assert_eq!(
            crate::session::should_open_pane_menu(layout::Hit::Tab(2)),
            None
        );
    }

    #[test]
    fn should_open_pane_menu_returns_none_for_divider_hit() {
        assert_eq!(
            crate::session::should_open_pane_menu(layout::Hit::Divider(Vec::new())),
            None
        );
    }

    #[test]
    fn should_open_pane_menu_returns_none_for_none_hit() {
        assert_eq!(
            crate::session::should_open_pane_menu(layout::Hit::None),
            None
        );
    }

    #[tokio::test]
    async fn motion_with_menu_open_updates_highlight_not_divider_drag() {
        let (mut session, _pane_id, _rx) = build_session_with_one_pane().await;
        let panes = session.window_pane_ids(0);
        session.set_menu_for_test(Some(MenuState {
            kind: MenuKind::Pane { pane_id: panes[0] },
            anchor: (10, 5),
            highlighted: 0,
        }));
        let menu_rect = crate::menu::menu_rect((10, 5), session.content_area_for_test(), 4, 18);
        let hover_col = (menu_rect.x + 2) + 1;
        let hover_row = (menu_rect.y + 3) + 1;
        let mut drag = None;
        let motion = MouseEvent {
            button: 3,
            col: hover_col,
            row: hover_row,
            kind: MouseKind::Drag,
        };
        let _ = session.handle_mouse(motion, &mut drag);
        assert_eq!(session.menu_highlighted_for_test(), Some(2));
        assert!(
            drag.is_none(),
            "menu hover must not initiate a divider drag"
        );
    }

    #[tokio::test]
    async fn motion_with_menu_closed_routes_to_existing_drag_path() {
        let (mut session, _pane_id, _rx) = build_session_with_one_pane().await;
        session.apply_command(Command::SplitVertical);
        let mut drag: Option<(usize, layout::SplitPath)> = Some((0, Vec::new()));
        let motion = MouseEvent {
            button: 3,
            col: 50,
            row: 5,
            kind: MouseKind::Drag,
        };
        let _ = session.handle_mouse(motion, &mut drag);
        assert!(
            !session.menu_open(),
            "Drag while menu closed must not open menu"
        );
        assert!(
            drag.is_some(),
            "Drag tuple must persist — proves event reached existing dispatcher"
        );
    }

    #[tokio::test]
    async fn right_click_while_menu_open_close_then_reopens_at_new_anchor() {
        let (mut session, _pane_id, _rx) = build_session_with_one_pane().await;
        session.apply_command(Command::SplitVertical);
        let panes = session.window_pane_ids(0);
        let first = panes[0];
        let second = *panes.last().unwrap();

        session.set_menu_for_test(Some(MenuState {
            kind: MenuKind::Pane { pane_id: first },
            anchor: (10, 5),
            highlighted: 2,
        }));
        let mut drag = None;
        let _ = session.handle_mouse(right_press(60, 5), &mut drag);

        assert!(session.menu_open());
        let anchor = session.menu_anchor_for_test();
        assert_eq!(anchor, Some((60, 5)), "menu must re-anchor at B's click");
        assert_eq!(session.menu_highlighted_for_test(), Some(0));
        assert_eq!(session.menu_pane_for_test(), Some(second));
    }

    #[tokio::test]
    async fn left_click_on_split_item_returns_effect_with_spawned_pane() {
        let (mut session, _pane_id, _rx) = build_session_with_one_pane().await;
        let panes = session.window_pane_ids(0);
        let first = panes[0];
        session.set_menu_for_test(Some(MenuState {
            kind: MenuKind::Pane { pane_id: first },
            anchor: (10, 5),
            highlighted: 0, // Split Vertically
        }));
        let menu_rect = crate::menu::menu_rect((10, 5), session.content_area_for_test(), 4, 18);
        let click_col = (menu_rect.x + 5) + 1;
        let click_row = (menu_rect.y + 1) + 1;
        let mut drag = None;
        let eff = session.handle_mouse(left_press(click_col, click_row), &mut drag);
        assert!(
            !eff.spawned.is_empty(),
            "Split must produce a spawned-pane effect"
        );
    }

    use crate::input::InputEvent;

    fn pane_event(bytes: &[u8]) -> InputEvent {
        InputEvent::Pane(bytes.to_vec())
    }

    #[tokio::test]
    async fn try_consume_returns_false_when_menu_closed() {
        let (mut session, _pane_id, _rx) = build_session_with_one_pane().await;
        assert!(session
            .try_consume_menu_event(&pane_event(b"\x1b"))
            .is_none());
        assert!(session
            .try_consume_menu_event(&InputEvent::FocusIn)
            .is_none());
    }

    #[tokio::test]
    async fn escape_with_menu_open_closes_menu_and_does_not_reach_pane() {
        let (mut session, _pane_id, _rx) = build_session_with_one_pane().await;
        let panes = session.window_pane_ids(0);
        session.set_menu_for_test(Some(MenuState {
            kind: MenuKind::Pane { pane_id: panes[0] },
            anchor: (10, 5),
            highlighted: 0,
        }));
        let consumed = session.try_consume_menu_event(&pane_event(b"\x1b"));
        assert!(consumed.is_some());
        assert!(!session.menu_open());
    }

    #[tokio::test]
    async fn arrow_down_with_menu_open_moves_highlight() {
        let (mut session, _pane_id, _rx) = build_session_with_one_pane().await;
        let panes = session.window_pane_ids(0);
        session.set_menu_for_test(Some(MenuState {
            kind: MenuKind::Pane { pane_id: panes[0] },
            anchor: (10, 5),
            highlighted: 0,
        }));
        assert!(session
            .try_consume_menu_event(&pane_event(b"\x1b[B"))
            .is_some());
        assert!(session.menu_open());
        assert_eq!(session.menu_highlighted_for_test(), Some(1));
    }

    #[tokio::test]
    async fn arrow_up_with_menu_open_moves_highlight() {
        let (mut session, _pane_id, _rx) = build_session_with_one_pane().await;
        let panes = session.window_pane_ids(0);
        session.set_menu_for_test(Some(MenuState {
            kind: MenuKind::Pane { pane_id: panes[0] },
            anchor: (10, 5),
            highlighted: 2,
        }));
        assert!(session
            .try_consume_menu_event(&pane_event(b"\x1b[A"))
            .is_some());
        assert_eq!(session.menu_highlighted_for_test(), Some(1));
    }

    #[tokio::test]
    async fn enter_with_menu_open_dispatches_highlighted_command() {
        let (mut session, _pane_id, _rx) = build_session_with_one_pane().await;
        let panes = session.window_pane_ids(0);
        session.set_menu_for_test(Some(MenuState {
            kind: MenuKind::Pane { pane_id: panes[0] },
            anchor: (10, 5),
            highlighted: 0,
        }));
        assert!(session.try_consume_menu_event(&pane_event(b"\r")).is_some());
        assert!(!session.menu_open());
        assert_eq!(session.window_pane_ids(0).len(), 2);
    }

    #[tokio::test]
    async fn enter_on_split_item_returns_effect_with_spawned_pane() {
        let (mut session, _pane_id, _rx) = build_session_with_one_pane().await;
        let panes = session.window_pane_ids(0);
        session.set_menu_for_test(Some(MenuState {
            kind: MenuKind::Pane { pane_id: panes[0] },
            anchor: (10, 5),
            highlighted: 0, // Split Vertically
        }));
        let eff = session
            .try_consume_menu_event(&pane_event(b"\r"))
            .expect("Enter on Item must return Some(effect)");
        assert!(
            !eff.spawned.is_empty(),
            "Enter on Split must propagate a spawned-pane effect"
        );
    }

    #[tokio::test]
    async fn arbitrary_keystroke_with_menu_open_is_swallowed() {
        let (mut session, _pane_id, _rx) = build_session_with_one_pane().await;
        let panes = session.window_pane_ids(0);
        session.set_menu_for_test(Some(MenuState {
            kind: MenuKind::Pane { pane_id: panes[0] },
            anchor: (10, 5),
            highlighted: 0,
        }));
        assert!(session.try_consume_menu_event(&pane_event(b"q")).is_some());
        assert!(session.menu_open());
    }

    #[tokio::test]
    async fn prefix_byte_with_menu_open_arms_prefix_state_but_resulting_command_is_dropped() {
        let (mut session, _pane_id, _rx) = build_session_with_one_pane().await;
        let panes = session.window_pane_ids(0);
        session.set_menu_for_test(Some(MenuState {
            kind: MenuKind::Pane { pane_id: panes[0] },
            anchor: (10, 5),
            highlighted: 0,
        }));
        let cmd = InputEvent::Command(Command::ClosePane);
        assert!(session.try_consume_menu_event(&cmd).is_some());
        assert_eq!(session.window_pane_ids(0).len(), 1);
    }

    #[tokio::test]
    async fn focus_in_event_with_menu_open_passes_through() {
        let (mut session, _pane_id, _rx) = build_session_with_one_pane().await;
        let panes = session.window_pane_ids(0);
        session.set_menu_for_test(Some(MenuState {
            kind: MenuKind::Pane { pane_id: panes[0] },
            anchor: (10, 5),
            highlighted: 0,
        }));
        assert!(session
            .try_consume_menu_event(&InputEvent::FocusIn)
            .is_none());
    }

    #[tokio::test]
    async fn mouse_event_with_menu_open_is_not_swallowed_by_try_consume_menu_event() {
        let (mut session, _pane_id, _rx) = build_session_with_one_pane().await;
        let panes = session.window_pane_ids(0);
        session.set_menu_for_test(Some(MenuState {
            kind: MenuKind::Pane { pane_id: panes[0] },
            anchor: (10, 5),
            highlighted: 0,
        }));
        let mouse = InputEvent::Mouse(MouseEvent {
            button: 0,
            col: 1,
            row: 1,
            kind: MouseKind::Press,
        });
        assert!(session.try_consume_menu_event(&mouse).is_none());
    }

    #[tokio::test]
    async fn menu_auto_closes_when_anchored_pane_closes_via_command() {
        let (mut session, _pane_id, _rx) = build_session_with_one_pane().await;
        session.apply_command(Command::SplitVertical);
        let panes = session.window_pane_ids(0);
        let first = panes[0];
        let second = *panes.last().unwrap();
        session.set_menu_for_test(Some(MenuState {
            kind: MenuKind::Pane { pane_id: second },
            anchor: (60, 5),
            highlighted: 0,
        }));
        session.focus_for_test(second);
        let _ = session.apply_command(Command::ClosePane);
        assert!(!session.menu_open());
        let _ = first;
    }

    #[tokio::test]
    async fn menu_auto_closes_when_anchored_pane_exits_naturally() {
        let (mut session, _pane_id, _rx) = build_session_with_one_pane().await;
        session.apply_command(Command::SplitVertical);
        let panes = session.window_pane_ids(0);
        let second = *panes.last().unwrap();
        session.set_menu_for_test(Some(MenuState {
            kind: MenuKind::Pane { pane_id: second },
            anchor: (60, 5),
            highlighted: 0,
        }));
        let _ = session.pane_exited(second);
        assert!(!session.menu_open());
    }

    #[tokio::test]
    async fn menu_auto_closes_on_select_window() {
        let (mut session, _pane_id, _rx) = build_session_with_one_pane().await;
        let panes = session.window_pane_ids(0);
        session.apply_command(Command::NewWindow);
        session.apply_command(Command::SelectWindow(0));
        session.set_menu_for_test(Some(MenuState {
            kind: MenuKind::Pane { pane_id: panes[0] },
            anchor: (10, 5),
            highlighted: 0,
        }));
        let _ = session.apply_command(Command::SelectWindow(1));
        assert!(!session.menu_open());
    }

    #[tokio::test]
    async fn menu_auto_closes_on_next_window() {
        let (mut session, _pane_id, _rx) = build_session_with_one_pane().await;
        let panes = session.window_pane_ids(0);
        session.apply_command(Command::NewWindow);
        session.apply_command(Command::SelectWindow(0));
        session.set_menu_for_test(Some(MenuState {
            kind: MenuKind::Pane { pane_id: panes[0] },
            anchor: (10, 5),
            highlighted: 0,
        }));
        let _ = session.apply_command(Command::NextWindow);
        assert!(!session.menu_open());
    }

    #[tokio::test]
    async fn menu_auto_closes_on_toggle_sidebar() {
        let (mut session, _pane_id, _rx) = build_session_with_one_pane().await;
        let panes = session.window_pane_ids(0);
        session.set_menu_for_test(Some(MenuState {
            kind: MenuKind::Pane { pane_id: panes[0] },
            anchor: (10, 5),
            highlighted: 0,
        }));
        let _ = session.apply_command(Command::ToggleSidebar);
        assert!(!session.menu_open());
    }

    #[tokio::test]
    async fn menu_auto_closes_on_resize_when_anchor_falls_outside_new_content_area() {
        let (mut session, _pane_id, _rx) = build_session_with_one_pane().await;
        let panes = session.window_pane_ids(0);
        session.set_menu_for_test(Some(MenuState {
            kind: MenuKind::Pane { pane_id: panes[0] },
            anchor: (70, 20),
            highlighted: 0,
        }));
        session.resize(40, 10);
        assert!(!session.menu_open());
    }

    #[tokio::test]
    async fn menu_survives_resize_when_anchor_still_inside_new_content_area() {
        let (mut session, _pane_id, _rx) = build_session_with_one_pane().await;
        let panes = session.window_pane_ids(0);
        session.set_menu_for_test(Some(MenuState {
            kind: MenuKind::Pane { pane_id: panes[0] },
            anchor: (10, 5),
            highlighted: 0,
        }));
        session.resize(80, 30);
        assert!(session.menu_open());
    }

    #[tokio::test]
    async fn menu_auto_closes_on_detach() {
        let (mut session, _pane_id, _rx) = build_session_with_one_pane().await;
        let panes = session.window_pane_ids(0);
        session.set_menu_for_test(Some(MenuState {
            kind: MenuKind::Pane { pane_id: panes[0] },
            anchor: (10, 5),
            highlighted: 0,
        }));
        let _ = session.apply_command(Command::Detach);
        assert!(!session.menu_open());
    }

    #[tokio::test]
    async fn menu_auto_closes_on_prev_window() {
        let (mut session, _pane_id, _rx) = build_session_with_one_pane().await;
        let panes = session.window_pane_ids(0);
        session.apply_command(Command::NewWindow);
        session.apply_command(Command::SelectWindow(0));
        session.set_menu_for_test(Some(MenuState {
            kind: MenuKind::Pane { pane_id: panes[0] },
            anchor: (10, 5),
            highlighted: 0,
        }));
        let _ = session.apply_command(Command::PrevWindow);
        assert!(!session.menu_open());
    }

    #[tokio::test]
    async fn menu_does_not_auto_close_on_focus_change_within_window() {
        let (mut session, _pane_id, _rx) = build_session_with_one_pane().await;
        session.apply_command(Command::SplitVertical);
        let panes = session.window_pane_ids(0);
        session.focus_for_test(panes[1]);
        session.set_menu_for_test(Some(MenuState {
            kind: MenuKind::Pane { pane_id: panes[0] },
            anchor: (10, 5),
            highlighted: 0,
        }));
        session.focus_for_test(panes[1]);
        assert!(session.menu_open());
    }

    #[tokio::test]
    async fn session_new_stores_sidebar_width_argument() {
        let cwd = std::env::current_dir()
            .unwrap()
            .to_string_lossy()
            .to_string();
        let (session, _pid, _rx) = Session::new(
            "test".to_string(),
            vec!["sh".to_string(), "-c".to_string(), "sleep 30".to_string()],
            cwd,
            80,
            10,
            crate::theme::Theme::default(),
            crate::config::KeySpec::Ctrl('b'),
            crate::config::KeyMap::default(),
            30, // sidebar_width
        )
        .expect("Session::new");
        assert_eq!(session.sidebar_width(), 30);
    }

    #[tokio::test]
    async fn refresh_agent_states_returns_empty_outcome_when_no_change() {
        let (mut session, _pid, _rx) = build_session_with_one_pane().await;
        // Drive the resolver to None — pane has no agent, no transitions.
        let outcome = session.refresh_agent_states(|_| None);
        assert!(outcome.blocked_transitions.is_empty());
    }

    #[tokio::test]
    async fn refresh_agent_states_emits_pane_id_on_working_to_blocked() {
        let (mut session, pane_id, _rx) = build_session_with_one_pane().await;
        // Pass 1: classify as Working.
        session.feed(pane_id, b"esc to interrupt\n");
        let _ = session.refresh_agent_states(|_| Some("claude".to_string()));
        // Pass 2: clear screen and feed a blocked-style tail.
        session.feed(
            pane_id,
            b"\x1b[2J\x1b[HDo you want to proceed?\n\xe2\x9d\xaf 1. Yes\n",
        );
        let outcome = session.refresh_agent_states(|_| Some("claude".to_string()));
        assert!(outcome.changed);
        assert_eq!(outcome.blocked_transitions, vec![pane_id]);
    }

    #[tokio::test]
    async fn refresh_agent_states_does_not_emit_when_already_blocked() {
        let (mut session, pane_id, _rx) = build_session_with_one_pane().await;
        // Two passes that both classify as Blocked → only one transition.
        session.feed(pane_id, b"Do you want to proceed?\n\xe2\x9d\xaf 1. Yes\n");
        let first = session.refresh_agent_states(|_| Some("claude".to_string()));
        assert_eq!(first.blocked_transitions, vec![pane_id]);
        let second = session.refresh_agent_states(|_| Some("claude".to_string()));
        assert!(
            second.blocked_transitions.is_empty(),
            "Blocked→Blocked must not re-emit"
        );
    }

    #[tokio::test]
    async fn refresh_agent_states_classifies_codex_working() {
        let (mut session, pane_id, _rx) = build_session_with_one_pane().await;
        session.feed(pane_id, b"Working (3s \xe2\x80\xa2 esc to interrupt)\n");
        let outcome = session.refresh_agent_states(|_| Some("codex".to_string()));
        assert!(outcome.changed);
        let pane = session.panes.get(&pane_id).expect("pane");
        assert_eq!(pane.agent, Some(crate::detect::Agent::Codex));
        assert_eq!(pane.agent_state, crate::detect::AgentState::Working);
    }

    #[tokio::test]
    async fn refresh_agent_states_classifies_codex_blocked_and_emits() {
        let (mut session, pane_id, _rx) = build_session_with_one_pane().await;
        session.feed(
            pane_id,
            b"Would you like to run the following command?\n\xe2\x80\xba 1. Yes, proceed\n",
        );
        let outcome = session.refresh_agent_states(|_| Some("codex".to_string()));
        let pane = session.panes.get(&pane_id).expect("pane");
        assert_eq!(pane.agent_state, crate::detect::AgentState::Blocked);
        assert_eq!(
            outcome.blocked_transitions,
            vec![pane_id],
            "Codex blocked edge must emit like Claude"
        );
    }

    #[tokio::test]
    async fn refresh_agent_states_codex_ignores_claude_only_prompt() {
        let (mut session, pane_id, _rx) = build_session_with_one_pane().await;
        // Claude-style prompt contains no Codex marker → Codex classifies Idle.
        session.feed(pane_id, b"Do you want to proceed?\n\xe2\x9d\xaf 1. Yes\n");
        let outcome = session.refresh_agent_states(|_| Some("codex".to_string()));
        assert!(outcome.blocked_transitions.is_empty());
        let pane = session.panes.get(&pane_id).expect("pane");
        assert_eq!(pane.agent, Some(crate::detect::Agent::Codex));
        assert_eq!(pane.agent_state, crate::detect::AgentState::Idle);
    }

    #[tokio::test]
    async fn refresh_agent_states_codex_clears_to_idle() {
        let (mut session, pane_id, _rx) = build_session_with_one_pane().await;
        session.feed(pane_id, b"Working (1s \xe2\x80\xa2 esc to interrupt)\n");
        let outcome = session.refresh_agent_states(|_| Some("codex".to_string()));
        // Precondition: Codex must actually reach Working before we test
        // clearing — guards against false-green (pre-Task-2 the short-circuit
        // leaves it Idle, so this assert fails → genuine RED).
        assert!(outcome.changed);
        assert_eq!(
            session.panes.get(&pane_id).unwrap().agent_state,
            crate::detect::AgentState::Working
        );
        session.feed(pane_id, b"\x1b[2J\x1b[H");
        session.refresh_agent_states(|_| Some("codex".to_string())); // streak 1
        session.refresh_agent_states(|_| Some("codex".to_string())); // streak 2 → Idle
        // Single window → pane is focused → confirmed-idle is Idle, not Done.
        let pane = session.panes.get(&pane_id).expect("pane");
        assert_eq!(pane.agent_state, crate::detect::AgentState::Idle);
    }

    /// Build a session whose first pane (window 0) is UNFOCUSED: opening a
    /// second window moves `active_window` to index 1.
    async fn two_windows_first_unfocused() -> (Session, crate::layout::PaneId) {
        let (mut session, pane_a, _rx) = build_session_with_one_pane().await;
        let eff = session.apply_command(Command::NewWindow);
        assert_eq!(eff.spawned.len(), 1);
        assert_eq!(session.active_window_index(), 1, "new window is focused");
        (session, pane_a)
    }

    /// Drive pane to confirmed-idle from a prior non-idle state (2 idle ticks).
    /// The pane MUST already be Working or Blocked when called; on an already-Idle
    /// pane this is a no-op (stays Idle, never Done).
    fn finish_after(session: &mut Session, pane: crate::layout::PaneId) {
        session.feed(pane, b"\x1b[2J\x1b[H");
        session.refresh_agent_states(|_| Some("claude".to_string())); // streak 1
        session.refresh_agent_states(|_| Some("claude".to_string())); // streak 2
    }

    #[tokio::test]
    async fn finishing_while_unfocused_becomes_done() {
        let (mut session, pane_a) = two_windows_first_unfocused().await;
        session.feed(pane_a, b"esc to interrupt\n");
        session.refresh_agent_states(|_| Some("claude".to_string())); // Working
        finish_after(&mut session, pane_a);
        assert_eq!(
            session.panes.get(&pane_a).unwrap().agent_state,
            detect::AgentState::Done
        );
    }

    #[tokio::test]
    async fn finishing_while_focused_goes_straight_to_idle_not_done() {
        // Single window → the only pane is always focused.
        let (mut session, pane_a, _rx) = build_session_with_one_pane().await;
        session.feed(pane_a, b"esc to interrupt\n");
        session.refresh_agent_states(|_| Some("claude".to_string())); // Working
        finish_after(&mut session, pane_a);
        assert_eq!(
            session.panes.get(&pane_a).unwrap().agent_state,
            detect::AgentState::Idle
        );
    }

    #[tokio::test]
    async fn done_is_sticky_while_unfocused() {
        let (mut session, pane_a) = two_windows_first_unfocused().await;
        session.feed(pane_a, b"esc to interrupt\n");
        session.refresh_agent_states(|_| Some("claude".to_string()));
        finish_after(&mut session, pane_a); // → Done
        session.refresh_agent_states(|_| Some("claude".to_string())); // extra idle tick
        assert_eq!(
            session.panes.get(&pane_a).unwrap().agent_state,
            detect::AgentState::Done
        );
    }

    #[tokio::test]
    async fn done_clears_when_window_becomes_focused() {
        let (mut session, pane_a) = two_windows_first_unfocused().await;
        session.feed(pane_a, b"esc to interrupt\n");
        session.refresh_agent_states(|_| Some("claude".to_string()));
        finish_after(&mut session, pane_a);
        assert_eq!(
            session.panes.get(&pane_a).unwrap().agent_state,
            detect::AgentState::Done,
            "must actually be Done before we test clearing (guards false-green)"
        );
        // Focus window 0 (1-based 1) via SelectWindow, then a refresh tick clears.
        session.apply_command(Command::SelectWindow(1));
        assert_eq!(session.active_window_index(), 0);
        session.refresh_agent_states(|_| Some("claude".to_string()));
        assert_eq!(
            session.panes.get(&pane_a).unwrap().agent_state,
            detect::AgentState::Idle
        );
    }

    #[tokio::test]
    async fn done_clears_after_sidebar_entry_focus() {
        let (mut session, pane_a) = two_windows_first_unfocused().await;
        session.feed(pane_a, b"esc to interrupt\n");
        session.refresh_agent_states(|_| Some("claude".to_string()));
        finish_after(&mut session, pane_a);
        assert_eq!(
            session.panes.get(&pane_a).unwrap().agent_state,
            detect::AgentState::Done,
            "must actually be Done before we test clearing (guards false-green)"
        );
        // The path the spec originally missed: focusing via a sidebar-entry hit.
        let _ = session.handle_hit(layout::Hit::SidebarEntry {
            window_index: 0,
            pane_id: pane_a,
        });
        assert_eq!(session.active_window_index(), 0);
        session.refresh_agent_states(|_| Some("claude".to_string()));
        assert_eq!(
            session.panes.get(&pane_a).unwrap().agent_state,
            detect::AgentState::Idle
        );
    }

    #[tokio::test]
    async fn done_clears_when_pane_exit_promotes_its_window() {
        // The other focus-change path the spec lists (§4.5/§6): closing the
        // focused window so `pane_exited` clamps `active_window` onto pane A's
        // window. Written inline because we need the second window's pane id.
        let (mut session, pane_a, _rx) = build_session_with_one_pane().await;
        let eff = session.apply_command(Command::NewWindow);
        let pane_b = eff.spawned[0].0;
        assert_eq!(session.active_window_index(), 1, "window 1 focused");
        // Pane A (window 0) finishes while unfocused → Done.
        session.feed(pane_a, b"esc to interrupt\n");
        session.refresh_agent_states(|_| Some("claude".to_string()));
        finish_after(&mut session, pane_a);
        assert_eq!(
            session.panes.get(&pane_a).unwrap().agent_state,
            detect::AgentState::Done
        );
        // Close window 1; its removal clamps active_window to 0 (pane A focused).
        for rt in session.pane_exited(pane_b) {
            rt.close();
        }
        assert_eq!(session.active_window_index(), 0);
        // Next tick sees pane A focused → clears Done to Idle.
        session.refresh_agent_states(|_| Some("claude".to_string()));
        assert_eq!(
            session.panes.get(&pane_a).unwrap().agent_state,
            detect::AgentState::Idle
        );
    }

    #[tokio::test]
    async fn unfocused_blocked_stays_blocked_not_done() {
        let (mut session, pane_a) = two_windows_first_unfocused().await;
        session.feed(pane_a, b"Do you want to proceed?\n\xe2\x9d\xaf 1. Yes\n");
        session.refresh_agent_states(|_| Some("claude".to_string()));
        assert_eq!(
            session.panes.get(&pane_a).unwrap().agent_state,
            detect::AgentState::Blocked
        );
    }

    #[tokio::test]
    async fn blocked_to_idle_while_unfocused_becomes_done() {
        let (mut session, pane_a) = two_windows_first_unfocused().await;
        session.feed(pane_a, b"Do you want to proceed?\n\xe2\x9d\xaf 1. Yes\n");
        session.refresh_agent_states(|_| Some("claude".to_string())); // Blocked
        finish_after(&mut session, pane_a); // → Done
        assert_eq!(
            session.panes.get(&pane_a).unwrap().agent_state,
            detect::AgentState::Done
        );
    }

    #[tokio::test]
    async fn never_worked_pane_stays_idle_not_done() {
        let (mut session, pane_a) = two_windows_first_unfocused().await;
        for _ in 0..3 {
            session.refresh_agent_states(|_| Some("claude".to_string()));
        }
        assert_eq!(
            session.panes.get(&pane_a).unwrap().agent_state,
            detect::AgentState::Idle
        );
    }

    #[tokio::test]
    async fn done_transition_is_silent() {
        let (mut session, pane_a) = two_windows_first_unfocused().await;
        session.feed(pane_a, b"esc to interrupt\n");
        session.refresh_agent_states(|_| Some("claude".to_string()));
        session.feed(pane_a, b"\x1b[2J\x1b[H");
        session.refresh_agent_states(|_| Some("claude".to_string()));
        let outcome = session.refresh_agent_states(|_| Some("claude".to_string()));
        assert!(
            outcome.blocked_transitions.is_empty(),
            "Done must not ring the blocked sound"
        );
        assert_eq!(
            session.panes.get(&pane_a).unwrap().agent_state,
            detect::AgentState::Done
        );
    }

    #[tokio::test]
    async fn done_resumes_to_working_when_agent_restarts_unfocused() {
        let (mut session, pane_a) = two_windows_first_unfocused().await;
        session.feed(pane_a, b"esc to interrupt\n");
        session.refresh_agent_states(|_| Some("claude".to_string()));
        finish_after(&mut session, pane_a); // → Done
        assert_eq!(
            session.panes.get(&pane_a).unwrap().agent_state,
            detect::AgentState::Done
        );
        // Agent picks up a new task while still unfocused → back to Working
        // (Done is not sticky against real activity).
        session.feed(pane_a, b"esc to interrupt\n");
        session.refresh_agent_states(|_| Some("claude".to_string()));
        assert_eq!(
            session.panes.get(&pane_a).unwrap().agent_state,
            detect::AgentState::Working
        );
    }

    #[tokio::test]
    async fn agent_entries_carry_session_label_and_branch_when_cached() {
        let (mut session, pane_id, _rx) = build_session_with_one_pane().await;
        // First, classify the pane as Claude so it survives the
        // agent_entries filter.
        session.feed(pane_id, b"esc to interrupt\n");
        let _ = session.refresh_agent_states(|_| Some("claude".to_string()));
        // Manually seed the cache fields the way refresh_agent_meta would.
        {
            let pane = session.panes.get_mut(&pane_id).expect("pane");
            pane.session_label = Some("hello world".to_string());
            pane.branch = Some("main".to_string());
        }
        let entries = session.agent_entries();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].session_label.as_deref(), Some("hello world"));
        assert_eq!(entries[0].branch.as_deref(), Some("main"));
    }

    #[tokio::test]
    async fn refresh_agent_meta_writes_branch_for_pane_with_resolvable_cwd() {
        let (mut session, _pid, _rx) = build_session_with_one_pane().await;
        // Classify the pane as Claude so refresh_agent_meta has work to do.
        session.feed(_pid, b"esc to interrupt\n");
        let _ = session.refresh_agent_states(|_| Some("claude".to_string()));

        // Initialize a tempdir as a git repo to feed back to the resolver.
        let tmp = tempfile::tempdir().unwrap();
        let canon = std::fs::canonicalize(tmp.path()).unwrap();
        std::process::Command::new("git")
            .args(["init", "-q", "-b", "main"])
            .current_dir(&canon)
            .status()
            .unwrap();
        let target_cwd = canon.clone();

        let changed = session.refresh_agent_meta(move |_pid| Some(target_cwd.clone()));
        assert!(changed);

        let entries = session.agent_entries();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].branch.as_deref(), Some("main"));
    }

    #[tokio::test]
    async fn refresh_agent_meta_skips_when_within_interval() {
        let (mut session, pane_id, _rx) = build_session_with_one_pane().await;
        session.feed(pane_id, b"esc to interrupt\n");
        let _ = session.refresh_agent_states(|_| Some("claude".to_string()));

        // First refresh forces the work (meta_last_refresh was pre-aged
        // on Pane construction).
        let _ = session.refresh_agent_meta(|_| Some(std::path::PathBuf::from("/")));
        // Second refresh within 2 s skips and returns false.
        let changed = session.refresh_agent_meta(|_| Some(std::path::PathBuf::from("/")));
        assert!(!changed, "second refresh within 2s must be a no-op");
    }

    #[tokio::test]
    async fn hit_on_plus_button_returns_new_window_button() {
        let (session, _p, _rx) = build_session_with_one_pane().await;
        let pb = session.tab_bar_for_test().plus.expect("button present");
        // 80x10 viewport → bottom row 0-based 9 → 1-based 10.
        let hit = session.hit_for_test(pb.x_start + 1, 10);
        assert_eq!(hit, crate::layout::Hit::NewWindowButton);
    }

    #[tokio::test]
    async fn hit_on_tab_still_returns_tab() {
        let (mut session, _p, _rx) = build_session_with_one_pane().await;
        session.apply_command(Command::NewWindow); // now 2 tabs
        let tabs = session.tab_regions_for_test();
        let target = &tabs[0];
        let hit = session.hit_for_test(target.x_start + 1, 10);
        assert_eq!(hit, crate::layout::Hit::Tab(target.window));
    }

    #[tokio::test]
    async fn left_click_on_plus_button_opens_dialog_then_enter_creates_and_switches() {
        let (mut session, _p, _rx) = build_session_with_one_pane().await;
        let pb = session.tab_bar_for_test().plus.expect("button present");
        let mut drag = None;
        // [+] now opens the New Window dialog rather than creating immediately.
        let eff = session.handle_mouse(left_press(pb.x_start + 1, 10), &mut drag);
        assert!(eff.spawned.is_empty(), "[+] opens the dialog, does not spawn yet");
        assert!(session.dialog_is_open_for_test());
        assert_eq!(session.window_count(), 1);
        // Submitting the (empty) dialog creates the default window and switches.
        let eff = session.submit_dialog();
        assert_eq!(eff.spawned.len(), 1, "submit spawns the new window's pane");
        assert_eq!(session.window_count(), 2);
        assert_eq!(session.active_window_index(), 1);
    }

    #[tokio::test]
    async fn right_click_on_plus_button_is_noop() {
        let (mut session, _p, _rx) = build_session_with_one_pane().await;
        let pb = session.tab_bar_for_test().plus.expect("button present");
        let mut drag = None;
        let _ = session.handle_mouse(right_press(pb.x_start + 1, 10), &mut drag);
        assert_eq!(session.window_count(), 1);
        assert!(!session.menu_open());
    }

    #[tokio::test]
    async fn handle_hit_new_window_button_is_noop() {
        let (mut session, _p, _rx) = build_session_with_one_pane().await;
        let out = session.handle_hit(crate::layout::Hit::NewWindowButton);
        assert!(out.is_none());
        assert_eq!(session.window_count(), 1);
    }

    #[tokio::test]
    async fn compose_includes_help_overlay_when_open() {
        let (mut session, _pid, _rx) = build_session_with_one_pane().await;
        session.set_help_for_test(Some(crate::help::HelpState::default()));
        let frame = session.render();
        // The Keybindings tab label is painted into the frame bytes.
        let needle = "Keybindings".as_bytes();
        assert!(
            frame.windows(needle.len()).any(|w| w == needle),
            "open help overlay must render the Keybindings tab"
        );
    }

    #[tokio::test]
    async fn compose_help_keybindings_reflect_prefix() {
        // Session built via build_session_with_one_pane uses the default C-b prefix.
        let (mut session, _pid, _rx) = build_session_with_one_pane().await;
        session.set_help_for_test(Some(crate::help::HelpState::default()));
        let frame = session.render();
        let needle = "C-b".as_bytes();
        assert!(frame.windows(needle.len()).any(|w| w == needle), "help keybindings must render the C-b prefix");
    }

    use crate::help::{HelpState, HelpTab};

    #[tokio::test]
    async fn try_consume_help_event_returns_none_when_help_closed() {
        let (mut session, _pid, _rx) = build_session_with_one_pane().await;
        assert!(session.try_consume_help_event(&pane_event(b"q")).is_none());
        assert!(session.try_consume_help_event(&InputEvent::FocusIn).is_none());
    }

    #[tokio::test]
    async fn show_help_command_opens_when_closed() {
        let (mut session, _pid, _rx) = build_session_with_one_pane().await;
        let _ = session.apply_command(Command::ShowHelp);
        assert!(session.help_open_for_test());
        // Defensive: menu stays cleared.
        assert!(!session.menu_open());
    }

    #[tokio::test]
    async fn prefix_question_while_menu_open_is_noop() {
        let (mut session, _pid, _rx) = build_session_with_one_pane().await;
        let panes = session.window_pane_ids(0);
        session.set_menu_for_test(Some(crate::menu::MenuState {
            kind: crate::menu::MenuKind::Pane { pane_id: panes[0] },
            anchor: (10, 5),
            highlighted: 0,
        }));
        // While the menu is open, the menu swallows ALL commands, including ShowHelp.
        let consumed = session.try_consume_menu_event(&InputEvent::Command(Command::ShowHelp));
        assert!(consumed.is_some(), "menu must swallow the command");
        assert!(!session.help_open_for_test(), "help must not open");
        assert!(session.menu_open(), "menu must stay open");
    }

    #[tokio::test]
    async fn show_help_command_toggles_closed_when_open() {
        let (mut session, _pid, _rx) = build_session_with_one_pane().await;
        session.set_help_for_test(Some(HelpState::default()));
        // ShowHelp passes through try_consume_help_event so apply_command toggles.
        assert!(session
            .try_consume_help_event(&InputEvent::Command(Command::ShowHelp))
            .is_none());
        let _ = session.apply_command(Command::ShowHelp);
        assert!(!session.help_open_for_test());
    }

    #[tokio::test]
    async fn q_escape_or_question_closes_help() {
        for key in [&b"q"[..], &b"\x1b"[..], &b"?"[..]] {
            let (mut session, _pid, _rx) = build_session_with_one_pane().await;
            session.set_help_for_test(Some(HelpState::default()));
            assert!(session.try_consume_help_event(&pane_event(key)).is_some());
            assert!(!session.help_open_for_test(), "key {:?} must close help", key);
        }
    }

    #[tokio::test]
    async fn right_left_tab_switch_active_tab_and_reset_scroll() {
        let (mut session, _pid, _rx) = build_session_with_one_pane().await;
        session.set_help_for_test(Some(HelpState { tab: HelpTab::Keybindings, scroll: 3 }));
        assert!(session.try_consume_help_event(&pane_event(b"\x1b[C")).is_some());
        assert_eq!(session.help_tab_for_test(), Some(HelpTab::Commands));
        assert_eq!(session.help_scroll_for_test(), Some(0));
        // Tab key also switches.
        assert!(session.try_consume_help_event(&pane_event(b"\t")).is_some());
        assert_eq!(session.help_tab_for_test(), Some(HelpTab::Keybindings));
    }

    #[tokio::test]
    async fn up_down_scroll_clamps_at_bounds() {
        let (mut session, _pid, _rx) = build_session_with_one_pane().await;
        session.resize(80, 24); // 80x24 fits all keybinding rows, so max_scroll == 0 → Down is a no-op.
        session.set_help_for_test(Some(HelpState::default()));
        assert!(session.try_consume_help_event(&pane_event(b"\x1b[B")).is_some());
        assert_eq!(session.help_scroll_for_test(), Some(0));
        // Up from 0 stays at 0.
        assert!(session.try_consume_help_event(&pane_event(b"\x1b[A")).is_some());
        assert_eq!(session.help_scroll_for_test(), Some(0));
    }

    #[tokio::test]
    async fn arbitrary_key_with_help_open_is_swallowed() {
        let (mut session, _pid, _rx) = build_session_with_one_pane().await;
        session.set_help_for_test(Some(HelpState::default()));
        assert!(session.try_consume_help_event(&pane_event(b"z")).is_some());
        assert!(session.help_open_for_test());
    }

    #[tokio::test]
    async fn command_other_than_show_help_is_swallowed_while_help_open() {
        let (mut session, _pid, _rx) = build_session_with_one_pane().await;
        session.set_help_for_test(Some(HelpState::default()));
        assert!(session
            .try_consume_help_event(&InputEvent::Command(Command::ClosePane))
            .is_some());
        assert_eq!(session.window_pane_ids(0).len(), 1, "pane must not close");
    }

    #[tokio::test]
    async fn focus_in_passes_through_while_help_open() {
        let (mut session, _pid, _rx) = build_session_with_one_pane().await;
        session.set_help_for_test(Some(HelpState::default()));
        assert!(session.try_consume_help_event(&InputEvent::FocusIn).is_none());
    }

    #[tokio::test]
    async fn mouse_event_passes_through_try_consume_help_event() {
        let (mut session, _pid, _rx) = build_session_with_one_pane().await;
        session.set_help_for_test(Some(HelpState::default()));
        let mouse = InputEvent::Mouse(MouseEvent {
            button: 0,
            col: 1,
            row: 1,
            kind: MouseKind::Press,
        });
        assert!(session.try_consume_help_event(&mouse).is_none());
    }

    #[tokio::test]
    async fn left_click_on_tab_switches_tab() {
        let (mut session, _pid, _rx) = build_session_with_one_pane().await;
        session.set_help_for_test(Some(HelpState::default()));
        // Compute the Commands tab's chip center.
        let content = session.content_area_for_test();
        let view = crate::help::build_view(
            &HelpState::default(),
            crate::config::KeySpec::Ctrl('b'),
            &crate::config::KeyMap::default(),
        );
        let rect = crate::help::help_rect(content, view.active_rows().len());
        let regs = crate::help::tab_regions(rect);
        let mid = (regs[1].x_start + regs[1].x_end) / 2;
        let row_1based = (rect.y + 1) + 1;
        let mut drag = None;
        let _ = session.handle_mouse(left_press(mid + 1, row_1based), &mut drag);
        assert_eq!(session.help_tab_for_test(), Some(HelpTab::Commands));
    }

    #[tokio::test]
    async fn left_click_outside_closes_help() {
        let (mut session, _pid, _rx) = build_session_with_one_pane().await;
        session.set_help_for_test(Some(HelpState::default()));
        let mut drag = None;
        // (1,1) is outside the centered overlay on an 80x24 viewport.
        let _ = session.handle_mouse(left_press(1, 1), &mut drag);
        assert!(!session.help_open_for_test());
    }

    #[tokio::test]
    async fn bare_question_mark_with_help_closed_is_not_intercepted() {
        let (mut session, _pid, _rx) = build_session_with_one_pane().await;
        // Help closed → '?' is not consumed; it would flow to the focused pane.
        assert!(session.try_consume_help_event(&pane_event(b"?")).is_none());
    }

    #[tokio::test]
    async fn help_gate_precedes_menu_gate_in_handle_mouse() {
        let (mut session, _pid, _rx) = build_session_with_one_pane().await;
        let panes = session.window_pane_ids(0);
        // Artificially set BOTH (never both in production) to pin that
        // handle_mouse checks help FIRST.
        session.set_menu_for_test(Some(crate::menu::MenuState {
            kind: crate::menu::MenuKind::Pane { pane_id: panes[0] },
            anchor: (10, 5),
            highlighted: 0,
        }));
        session.set_help_for_test(Some(HelpState::default()));
        let mut drag = None;
        // Left-click outside the help overlay → help path closes help; menu untouched.
        let _ = session.handle_mouse(left_press(1, 1), &mut drag);
        assert!(!session.help_open_for_test(), "help gate must run first, closing help");
        assert!(session.menu_open(), "menu must be untouched by the help gate");
    }

    #[tokio::test]
    async fn help_survives_resize_and_scroll_reclamps() {
        let (mut session, _pid, _rx) = build_session_with_one_pane().await;
        session.set_help_for_test(Some(HelpState {
            tab: HelpTab::Keybindings,
            scroll: 5,
        }));
        session.resize(40, 8); // tiny viewport — no auto-close (help has no anchor)
        assert!(session.help_open_for_test(), "help must survive resize");
        // A scroll re-clamps the stored offset to the new (smaller) max. On a
        // 40x8 viewport content is 40x7 → visible body = 2 → max_scroll = 11-2 = 9.
        // From a stale scroll of 5, one Down clamps to min(6, 9) = 6.
        let _ = session.try_consume_help_event(&pane_event(b"\x1b[B"));
        assert_eq!(
            session.help_scroll_for_test(),
            Some(6),
            "scroll must re-clamp against the resized geometry"
        );
        // Rendering with the small viewport must not panic.
        let _ = session.render();
        assert!(session.help_open_for_test());
    }

    #[tokio::test]
    async fn left_click_on_body_or_border_is_noop() {
        let (mut session, _pid, _rx) = build_session_with_one_pane().await;
        session.set_help_for_test(Some(HelpState::default()));
        let content = session.content_area_for_test();
        let view = crate::help::build_view(
            &HelpState::default(),
            crate::config::KeySpec::Ctrl('b'),
            &crate::config::KeyMap::default(),
        );
        let rect = crate::help::help_rect(content, view.active_rows().len());
        // Click a body interior cell (1-based): a row below the tab header/separator.
        let body_col = rect.x + 3 + 1;
        let body_row = (rect.y + 3) + 1;
        let mut drag = None;
        let _ = session.handle_mouse(left_press(body_col, body_row), &mut drag);
        assert!(session.help_open_for_test(), "body click must not close help");
        assert_eq!(
            session.help_tab_for_test(),
            Some(HelpTab::Keybindings),
            "body click must not switch tabs"
        );
    }

    #[tokio::test]
    async fn pinned_window_name_survives_refresh() {
        // Two windows; pin window 0's name, leave window 1 auto.
        let mut s = test_session(); // see "Test harness" preamble
        s.apply_command(Command::NewWindow); // second window (auto-named)
        s.set_window_name_pinned_for_test(0, "my-build");
        // A resolver that would rename everything to "zsh" if allowed.
        let changed = s.refresh_window_names(|_pid| Some("zsh".to_string()));
        let names = s.window_names_for_test();
        assert_eq!(names[0], "my-build", "pinned window keeps its custom name");
        assert_eq!(names[1], "zsh", "auto window still tracks the process");
        assert!(changed, "the auto window changed, so refresh reports true");
    }

    #[tokio::test]
    async fn opening_new_window_dialog_sets_state_and_renders_title() {
        let mut s = test_session();
        s.open_new_window_dialog();
        assert!(s.dialog_is_open_for_test());
        let frame = String::from_utf8_lossy(&s.render()).to_string();
        assert!(frame.contains("New Window"), "dialog title should render");
    }

    #[tokio::test]
    async fn submitting_new_window_dialog_with_name_creates_pinned_window() {
        let mut s = test_session();
        let before = s.window_count_for_test();
        s.open_new_window_dialog();
        for c in "build".chars() {
            s.dialog_insert_for_test(c);
        }
        let _eff = s.submit_dialog();
        assert!(!s.dialog_is_open_for_test(), "submit closes the dialog");
        assert_eq!(s.window_count_for_test(), before + 1);
        let names = s.window_names_for_test();
        assert_eq!(*names.last().unwrap(), "build");
        // Pinned: a refresh must not rename it.
        s.refresh_window_names(|_| Some("zsh".to_string()));
        assert_eq!(*s.window_names_for_test().last().unwrap(), "build");
    }

    #[tokio::test]
    async fn submitting_new_window_dialog_empty_creates_auto_window() {
        let mut s = test_session();
        s.open_new_window_dialog();
        let _eff = s.submit_dialog();
        assert!(!s.dialog_is_open_for_test());
        // The new window is auto: a refresh CAN rename it.
        s.refresh_window_names(|_| Some("zsh".to_string()));
        assert_eq!(*s.window_names_for_test().last().unwrap(), "zsh");
    }

    #[tokio::test]
    async fn plus_button_click_opens_dialog_not_immediate_window() {
        use crate::input::{MouseEvent, MouseKind};
        let mut s = test_session();
        let before = s.window_count_for_test();
        // Compute the [+] button column from the same layout the daemon uses.
        let bar = s.tab_bar_for_test();
        let pb = bar.plus.expect("plus button present at test width");
        let mut drag = None;
        let _eff = s.handle_mouse(
            MouseEvent {
                button: 0,
                col: pb.x_start + 2,
                row: s.tab_status_row_1based_for_test(),
                kind: MouseKind::Press,
            },
            &mut drag,
        );
        assert!(s.dialog_is_open_for_test(), "[+] opens the New Window dialog");
        assert_eq!(s.window_count_for_test(), before, "no window created yet");
    }

    #[tokio::test]
    async fn right_click_on_tab_opens_tab_menu() {
        use crate::input::{MouseEvent, MouseKind};
        use crate::menu::MenuKind;
        let mut s = test_session();
        let row = s.tab_status_row_1based_for_test();
        // Column of tab 0 (first tab) from the real layout.
        let regions = s.tab_regions_for_test();
        let t0 = &regions[0];
        let mut drag = None;
        let _eff = s.handle_mouse(
            MouseEvent {
                button: 2,
                col: t0.x_start + 1,
                row,
                kind: MouseKind::Press,
            },
            &mut drag,
        );
        match s.menu_kind_for_test() {
            Some(MenuKind::Tab { window }) => assert_eq!(window, 0),
            other => panic!("expected a tab menu, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn right_click_tab_while_menu_open_reanchors_to_tab_menu() {
        use crate::input::{MouseEvent, MouseKind};
        use crate::menu::{MenuKind, MenuState};
        let mut s = test_session();
        let row = s.tab_status_row_1based_for_test();
        // A menu is already open, anchored away from the target tab.
        s.set_menu_for_test(Some(MenuState {
            kind: MenuKind::Tab { window: 0 },
            anchor: (1, row),
            highlighted: 0,
        }));
        let regions = s.tab_regions_for_test();
        let t0 = &regions[0];
        let mut drag = None;
        // Right-click a tab while a menu is open → reanchor to that tab's menu
        // (exercises the Hit::Tab arm of handle_menu_mouse's button-2 reanchor).
        let _ = s.handle_mouse(
            MouseEvent {
                button: 2,
                col: t0.x_start + 1,
                row,
                kind: MouseKind::Press,
            },
            &mut drag,
        );
        match s.menu_kind_for_test() {
            Some(MenuKind::Tab { window }) => assert_eq!(window, 0),
            other => panic!("expected reanchored tab menu, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn right_click_on_pane_still_opens_pane_menu() {
        use crate::input::{MouseEvent, MouseKind};
        use crate::menu::MenuKind;
        let mut s = test_session();
        let mut drag = None;
        // A click in the content area (row 1 is top of content) resolves to a pane.
        let _eff = s.handle_mouse(
            MouseEvent {
                button: 2,
                col: 2,
                row: 1,
                kind: MouseKind::Press,
            },
            &mut drag,
        );
        assert!(matches!(s.menu_kind_for_test(), Some(MenuKind::Pane { .. })));
    }

    #[tokio::test]
    async fn close_window_removes_window_and_returns_runtimes() {
        let mut s = test_session();
        s.apply_command(Command::NewWindow); // now 2 windows, active = 1
        let before = s.window_count_for_test();
        let closed = s.close_window(1);
        assert_eq!(s.window_count_for_test(), before - 1);
        assert!(!closed.is_empty(), "closing a window returns its pane runtime(s)");
        assert!(s.active_window_in_range_for_test(), "active_window stays valid");
        // Tear the runtimes down properly — a bare drop runs blocking kill()/wait()
        // (see PaneRuntime::close in src/pane/mod.rs); existing tests do the same.
        for rt in closed {
            rt.close();
        }
    }

    #[tokio::test]
    async fn tab_menu_rename_action_opens_rename_dialog() {
        use crate::input::InputEvent;
        use crate::menu::{MenuKind, MenuState};
        let mut s = test_session();
        let row = s.tab_status_row_1based_for_test();
        // Open a tab menu for window 0, highlight "Rename" (index 0), press Enter.
        s.set_menu_for_test(Some(MenuState {
            kind: MenuKind::Tab { window: 0 },
            anchor: (3, row),
            highlighted: 0,
        }));
        let _eff = s.try_consume_menu_event(&InputEvent::Pane(b"\r".to_vec()));
        assert!(s.dialog_is_open_for_test(), "Rename opens the dialog");
        assert_eq!(s.dialog_buffer_for_test(), s.window_names_for_test()[0]);
    }

    #[tokio::test]
    async fn tab_menu_close_action_closes_window() {
        use crate::input::InputEvent;
        use crate::menu::{MenuKind, MenuState};
        let mut s = test_session();
        s.apply_command(Command::NewWindow); // 2 windows
        let before = s.window_count_for_test();
        let row = s.tab_status_row_1based_for_test();
        // Tab menu for window 1, highlight "Close" (index 1), Enter.
        s.set_menu_for_test(Some(MenuState {
            kind: MenuKind::Tab { window: 1 },
            anchor: (3, row),
            highlighted: 1,
        }));
        let eff = s.try_consume_menu_event(&InputEvent::Pane(b"\r".to_vec()));
        assert_eq!(s.window_count_for_test(), before - 1);
        // Close the runtimes the dispatch returned (a bare drop blocks).
        for rt in eff.unwrap_or_default().closed {
            rt.close();
        }
    }

    #[tokio::test]
    async fn rename_submit_pins_and_empty_submit_unpins() {
        let mut s = test_session();
        // Pin window 0 to "api" via the rename dialog.
        s.open_rename_dialog(0);
        // Clear the pre-filled buffer first.
        s.dialog_clear_for_test();
        for c in "api".chars() {
            s.dialog_insert_for_test(c);
        }
        s.submit_dialog();
        assert_eq!(s.window_names_for_test()[0], "api");
        s.refresh_window_names(|_| Some("zsh".to_string()));
        assert_eq!(s.window_names_for_test()[0], "api", "pinned, not overwritten");

        // Now open rename again, clear to empty, submit → un-pin, re-derive.
        s.open_rename_dialog(0);
        s.dialog_clear_for_test();
        s.submit_dialog();
        s.refresh_window_names(|_| Some("zsh".to_string()));
        assert_eq!(s.window_names_for_test()[0], "zsh", "un-pinned, now auto again");
    }

    #[tokio::test]
    async fn dialog_consumes_keys_and_enter_submits() {
        use crate::input::InputEvent;
        let mut s = test_session();
        let before = s.window_count_for_test();
        s.open_new_window_dialog();
        for b in b"hey" {
            let consumed = s.try_consume_dialog_event(&InputEvent::Pane(vec![*b]));
            assert!(consumed.is_some(), "dialog must consume key events while open");
        }
        assert_eq!(s.dialog_buffer_for_test(), "hey");
        s.try_consume_dialog_event(&InputEvent::Pane(vec![0x7f]));
        assert_eq!(s.dialog_buffer_for_test(), "he");
        let eff = s.try_consume_dialog_event(&InputEvent::Pane(vec![b'\r']));
        assert!(eff.is_some());
        assert!(!s.dialog_is_open_for_test());
        assert_eq!(s.window_count_for_test(), before + 1);
    }

    #[tokio::test]
    async fn dialog_escape_cancels_without_creating() {
        use crate::input::InputEvent;
        let mut s = test_session();
        let before = s.window_count_for_test();
        s.open_new_window_dialog();
        s.try_consume_dialog_event(&InputEvent::Pane(vec![0x1b]));
        assert!(!s.dialog_is_open_for_test());
        assert_eq!(s.window_count_for_test(), before, "Esc creates nothing");
    }

    #[tokio::test]
    async fn dialog_event_ignored_when_closed() {
        use crate::input::InputEvent;
        let mut s = test_session();
        assert!(s.try_consume_dialog_event(&InputEvent::Pane(vec![b'x'])).is_none());
    }

    #[tokio::test]
    async fn dialog_ignores_arrow_key_escape_sequences() {
        use crate::input::InputEvent;
        let mut s = test_session();
        s.open_new_window_dialog();
        for c in "ab".chars() {
            s.try_consume_dialog_event(&InputEvent::Pane(vec![c as u8]));
        }
        let consumed = s.try_consume_dialog_event(&InputEvent::Pane(b"\x1b[A".to_vec()));
        assert!(consumed.is_some(), "the dialog still swallows the event");
        assert_eq!(s.dialog_buffer_for_test(), "ab", "arrow keys do not edit the name");
        assert!(s.dialog_is_open_for_test(), "an arrow key does not close the dialog");
    }

    #[tokio::test]
    async fn open_new_window_dialog_command_opens_dialog_without_creating() {
        let mut s = test_session();
        let before = s.window_count_for_test();
        let _eff = s.apply_command(Command::OpenNewWindowDialog);
        assert!(s.dialog_is_open_for_test());
        assert_eq!(s.window_count_for_test(), before, "no window yet — dialog only");
    }

    #[tokio::test]
    async fn new_window_command_still_creates_directly() {
        let mut s = test_session();
        let before = s.window_count_for_test();
        let _eff = s.apply_command(Command::NewWindow);
        assert_eq!(s.window_count_for_test(), before + 1, "NewWindow stays a direct create");
    }

    /// Session + first pane for the snapshot/restore-meta tests. Uses a long-lived
    /// `sleep` in the workspace dir so the spawn is allowed by the test sandbox
    /// (cwd values used in assertions come from injected resolvers, not the spawn).
    fn snapshot_test_session() -> (
        Session,
        crate::layout::PaneId,
        tokio::sync::mpsc::UnboundedReceiver<crate::pane::PaneOutput>,
    ) {
        Session::new(
            "work".into(),
            vec!["sh".into(), "-c".into(), "sleep 30".into()],
            ".".into(),
            80,
            24,
            crate::theme::Theme::default(),
            crate::config::KeySpec::Ctrl('b'),
            crate::config::KeyMap::default(),
            26,
        )
        .unwrap()
    }

    #[tokio::test]
    async fn pane_output_does_not_mark_snapshot_dirty_but_split_does() {
        let (mut s, id, _rx) = snapshot_test_session();
        // Consume the initial (construction) dirty flag.
        assert!(s.take_snapshot_dirty());
        // Ordinary terminal output is applied via feed() — must NOT mark dirty.
        s.feed(id, b"hello world\x1b[2J some redraw traffic");
        assert!(!s.take_snapshot_dirty(), "pane output must not trigger a snapshot write");
        // A structural change DOES mark dirty.
        let mut eff = CommandEffect::default();
        s.split_focused(layout::SplitDir::Vertical, &mut eff);
        assert!(s.take_snapshot_dirty(), "a split must trigger a snapshot write");
    }

    #[tokio::test]
    async fn snapshot_captures_tree_shape_names_and_focus() {
        let (mut s, _id, _rx) = snapshot_test_session();
        // One vertical split → two leaves; focus is the new (second) leaf.
        let mut eff = CommandEffect::default();
        s.split_focused(layout::SplitDir::Vertical, &mut eff);

        let snap = s.snapshot();
        assert_eq!(snap.session_name, "work");
        assert_eq!(snap.windows.len(), 1);
        match &snap.windows[0].layout {
            crate::persist::NodeSnapshot::Split { first, second, .. } => {
                assert!(matches!(**first, crate::persist::NodeSnapshot::Leaf(_)));
                assert!(matches!(**second, crate::persist::NodeSnapshot::Leaf(_)));
            }
            _ => panic!("expected a split"),
        }
        // Focus is the second leaf (index 1 in tree order).
        assert_eq!(snap.windows[0].focused_leaf, 1);
    }

    #[tokio::test]
    async fn refresh_restore_meta_caches_cwd_for_non_agent_pane() {
        let (mut s, id, _rx) = snapshot_test_session();
        // Resolver stubs: cwd resolves to /work for any pid; no agent transcript.
        let changed = s.refresh_restore_meta(
            |_pid| Some(std::path::PathBuf::from("/work")),
            |_pid, _kind| None,
        );
        assert!(changed);
        assert_eq!(s.pane_cwd_for_test(id).as_deref(), Some("/work"));
    }

    // Fix 1: unzoom_active must persist the unzoom itself (mark snapshot dirty)
    // whenever it really flips zoomed -> unzoomed, because callers often unzoom
    // first and only mark dirty after a follow-up op that can no-op or fail. A
    // no-op unzoom (nothing was zoomed) must NOT mark dirty.
    #[tokio::test]
    async fn unzoom_marks_snapshot_dirty() {
        let (mut s, _id, _rx) = snapshot_test_session();
        // Two panes so a zoom is meaningful, then zoom the active window.
        s.apply_command(Command::SplitVertical);
        s.apply_command(Command::ToggleZoom);
        assert!(s.active_zoomed_for_test(), "window should be zoomed");
        // Consume any pending dirty from the split/zoom.
        let _ = s.take_snapshot_dirty();

        // A real unzoom (zoomed -> unzoomed) must mark dirty on its own.
        s.unzoom_active_for_test();
        assert!(!s.active_zoomed_for_test());
        assert!(
            s.take_snapshot_dirty(),
            "a real unzoom must mark the snapshot dirty even if no follow-up op does"
        );

        // A no-op unzoom (nothing was zoomed) must NOT mark dirty.
        s.unzoom_active_for_test();
        assert!(
            !s.take_snapshot_dirty(),
            "a no-op unzoom must not mark the snapshot dirty"
        );
    }

    // Fix 2: when a pane stops being an agent (kind == None) it must have its
    // stale `agent_resume` cleared even if the foreground pid is momentarily
    // absent — the clearing branch must not be gated behind a present fg_pid.
    #[tokio::test]
    async fn refresh_restore_meta_clears_agent_resume_when_pane_stops_being_agent() {
        let (mut s, id, _rx) = snapshot_test_session();
        // Seed a prior Claude capture, then classify the pane as a non-agent.
        s.set_pane_agent_resume_for_test(
            id,
            crate::persist::AgentResume::Claude {
                session_id: "abc-123".into(),
                transcript: "/tmp/transcript.jsonl".into(),
            },
        );
        s.set_pane_agent_kind_for_test(id, None);

        let changed = s.refresh_restore_meta(
            |_pid| Some(std::path::PathBuf::from("/work")),
            |_pid, _kind| None,
        );

        assert!(changed, "clearing a stale agent_resume counts as a change");
        let snap = s.snapshot();
        let leaf = match &snap.windows[0].layout {
            crate::persist::NodeSnapshot::Leaf(l) => l,
            _ => panic!("expected single leaf"),
        };
        assert_eq!(
            leaf.agent,
            crate::persist::AgentResume::None,
            "non-agent pane must have its stale agent_resume cleared"
        );
    }

    #[test]
    fn resume_argv_uses_positional_dollar_zero_and_validated_uuid() {
        let shell = "/bin/zsh";
        let claude = restore_command(
            &crate::persist::AgentResume::Claude {
                session_id: "11111111-2222-3333-4444-555555555555".into(),
                transcript: "/t.jsonl".into(),
            },
            shell,
        );
        assert_eq!(
            claude,
            vec![
                "/bin/zsh".to_string(),
                "-i".into(),
                "-c".into(),
                "claude --resume 11111111-2222-3333-4444-555555555555; exec \"$0\"".into(),
                "/bin/zsh".into(),
            ]
        );
        // Codex verb.
        let codex = restore_command(
            &crate::persist::AgentResume::Codex {
                session_id: "11111111-2222-3333-4444-555555555555".into(),
                transcript: "/t.jsonl".into(),
            },
            shell,
        );
        assert!(codex[3].starts_with("codex resume 11111111-"));
        // None → empty (default shell).
        assert!(restore_command(&crate::persist::AgentResume::None, shell).is_empty());
    }

    #[tokio::test]
    async fn restore_round_trips_tree_focus_and_active_window() {
        // Build a 2-window session: window 0 has a vertical split, window 1 a single pane.
        let (mut s, _id, _rx) = snapshot_test_session();
        let mut eff = CommandEffect::default();
        s.split_focused(layout::SplitDir::Vertical, &mut eff);
        s.new_window(None, &mut eff); // window 1
                                      // Snapshot, then force every pane's agent to None + a real cwd so restore
                                      // spawns plain shells (no claude/codex needed). The test sandbox only
                                      // allows spawning inside the workspace tree, so use "." (not "/tmp").
        let mut snap = s.snapshot();
        for w in &mut snap.windows {
            set_all_cwd(&mut w.layout, ".");
        }

        let (r, rxs) = Session::restore(
            snap.clone(),
            80,
            24,
            crate::theme::Theme::default(),
            crate::config::KeySpec::Ctrl('b'),
            crate::config::KeyMap::default(),
        )
        .unwrap();

        let back = r.snapshot();
        // Same window count, same active window, same tree shapes, same focus indices.
        assert_eq!(back.windows.len(), snap.windows.len());
        assert_eq!(back.active_window, snap.active_window);
        for (a, b) in snap.windows.iter().zip(&back.windows) {
            assert_eq!(node_shape(&a.layout), node_shape(&b.layout));
            assert_eq!(a.focused_leaf, b.focused_leaf);
            assert_eq!(a.name, b.name);
            assert_eq!(a.zoomed, b.zoomed);
        }
        assert_eq!(rxs.len(), 3); // 2 panes in window 0 + 1 in window 1
    }

    /// Regression for the plan/snapshot coupling: each restored leaf must inherit
    /// ITS OWN snapshot leaf's cwd, never a swapped sibling's. We build a split
    /// with two leaves carrying DISTINCT existing cwds, restore, then walk the
    /// restored root's leaves in tree order and check pane k == snapshot leaf k.
    ///
    /// Lockstep-test variant: workspace-subdir. The sandbox only permits PTY
    /// spawns inside the workspace tree, so the two distinct cwds are created
    /// UNDER the workspace (`./.tmp_restore_a`, `./.tmp_restore_b`) and cleaned
    /// up at test end, rather than via `tempfile::tempdir()` (which lands in
    /// /tmp and would block the spawn).
    #[tokio::test]
    async fn restore_maps_each_leaf_to_its_own_snapshot_cwd() {
        use crate::persist::{
            AgentResume, NodeSnapshot, PaneSnapshot, SessionSnapshot, SplitDirSnap, WindowSnapshot,
            SCHEMA_VERSION,
        };

        // Two distinct, existing cwds under the workspace so plan_restore keeps
        // them (cwd_exists == true) instead of downgrading to $HOME.
        std::fs::create_dir_all("./.tmp_restore_a").unwrap();
        std::fs::create_dir_all("./.tmp_restore_b").unwrap();
        let cwd_a = std::fs::canonicalize("./.tmp_restore_a")
            .unwrap()
            .to_string_lossy()
            .into_owned();
        let cwd_b = std::fs::canonicalize("./.tmp_restore_b")
            .unwrap()
            .to_string_lossy()
            .into_owned();
        assert_ne!(cwd_a, cwd_b);

        let leaf = |cwd: &str| {
            NodeSnapshot::Leaf(PaneSnapshot { cwd: cwd.into(), agent: AgentResume::None })
        };
        let snap = SessionSnapshot {
            schema_version: SCHEMA_VERSION,
            session_name: "lockstep".into(),
            sidebar_visible: false,
            sidebar_width: 26,
            active_window: 0,
            windows: vec![WindowSnapshot {
                name: "w".into(),
                name_pinned: false,
                zoomed: false,
                focused_leaf: 0,
                layout: NodeSnapshot::Split {
                    dir: SplitDirSnap::Vertical,
                    ratio: 0.5,
                    first: Box::new(leaf(&cwd_a)),
                    second: Box::new(leaf(&cwd_b)),
                },
            }],
        };

        let (r, _rxs) = Session::restore(
            snap,
            80,
            24,
            crate::theme::Theme::default(),
            crate::config::KeySpec::Ctrl('b'),
            crate::config::KeyMap::default(),
        )
        .unwrap();

        // Walk the restored window's leaves in tree order; leaf 0 must carry
        // cwd_a, leaf 1 cwd_b — proving no cross-leaf swap.
        let leaves = layout::all_panes(&r.windows[0].root);
        assert_eq!(leaves.len(), 2);
        assert_eq!(r.pane_cwd_for_test(leaves[0]).as_deref(), Some(cwd_a.as_str()));
        assert_eq!(r.pane_cwd_for_test(leaves[1]).as_deref(), Some(cwd_b.as_str()));

        let _ = std::fs::remove_dir_all("./.tmp_restore_a");
        let _ = std::fs::remove_dir_all("./.tmp_restore_b");
    }

    // Test helpers.
    fn set_all_cwd(node: &mut crate::persist::NodeSnapshot, cwd: &str) {
        match node {
            crate::persist::NodeSnapshot::Leaf(p) => {
                p.cwd = cwd.into();
                p.agent = crate::persist::AgentResume::None;
            }
            crate::persist::NodeSnapshot::Split { first, second, .. } => {
                set_all_cwd(first, cwd);
                set_all_cwd(second, cwd);
            }
        }
    }
    fn node_shape(node: &crate::persist::NodeSnapshot) -> String {
        match node {
            crate::persist::NodeSnapshot::Leaf(_) => "L".into(),
            crate::persist::NodeSnapshot::Split { first, second, dir, .. } => {
                format!("({:?} {} {})", dir, node_shape(first), node_shape(second))
            }
        }
    }
}
