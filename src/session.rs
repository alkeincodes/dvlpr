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
}

struct Window {
    name: String,
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

impl Session {
    /// Create a session with one window holding a single pane running `command`.
    /// Returns the session, the first pane's id, and its output receiver (the
    /// caller spawns a forwarder that tags the output with the pane id).
    pub fn new(
        session_name: String,
        command: Vec<String>,
        cwd: String,
        cols: u16,
        rows: u16,
        theme: crate::theme::Theme,
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
            sidebar_visible: false,
        };
        // The status bar is always present, so the pane fills the content area
        // (viewport minus the bar row), not the whole viewport.
        let content = layout::content_area(
            Rect {
                x: 0,
                y: 0,
                w: cols,
                h: rows,
            },
            1,
        );
        let (id, rx) = session.spawn_pane(content)?;
        session.windows.push(Window {
            name: initial_window_name(&session.command),
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
            },
        );
        Ok((id, rx))
    }

    fn viewport(&self) -> Rect {
        Rect {
            x: 0,
            y: 0,
            w: self.cols,
            h: self.rows,
        }
    }

    /// Resize every pane's PTY + screen to the rect the current geometry assigns
    /// it (across all windows), draining size-report replies. Called after any
    /// structural change and on viewport resize.
    fn relayout_all(&mut self) {
        let content = layout::content_area(self.viewport(), self.windows.len());
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
            &agent_entries,
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
    fn unzoom_active(&mut self) {
        if let Some(win) = self.windows.get_mut(self.active_window) {
            win.zoomed = false;
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
                self.new_window(&mut eff)
            }
            Command::NextWindow => {
                if !self.windows.is_empty() {
                    self.unzoom_active();
                    self.active_window = (self.active_window + 1) % self.windows.len();
                }
            }
            Command::PrevWindow => {
                if !self.windows.is_empty() {
                    self.unzoom_active();
                    let n = self.windows.len();
                    self.active_window = (self.active_window + n - 1) % n;
                }
            }
            Command::SelectWindow(n) => {
                if n >= 1 {
                    let idx = n - 1;
                    if idx < self.windows.len() {
                        self.unzoom_active();
                        self.active_window = idx;
                    }
                }
            }
            Command::ToggleZoom => {
                if let Some(win) = self.windows.get_mut(self.active_window) {
                    win.zoomed = !win.zoomed;
                }
                self.relayout_all();
            }
            Command::Detach => eff.detach = true,
        }
        eff
    }

    fn split_focused(&mut self, dir: SplitDir, eff: &mut CommandEffect) {
        let wi = self.active_window;
        let Some(win) = self.windows.get(wi) else {
            return;
        };
        let focused = win.focused;
        let content = layout::content_area(self.viewport(), self.windows.len());
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
        eff.spawned.push((new_id, rx));
    }

    fn close_focused(&mut self) -> Vec<PaneRuntime> {
        let Some(win) = self.windows.get(self.active_window) else {
            return Vec::new();
        };
        let focused = win.focused;
        self.pane_exited(focused)
    }

    fn new_window(&mut self, eff: &mut CommandEffect) {
        let new_count = self.windows.len() + 1;
        let content = layout::content_area(self.viewport(), new_count);
        let (id, rx) = match self.spawn_pane(content) {
            Ok(v) => v,
            Err(_) => return,
        };
        self.windows.push(Window {
            name: initial_window_name(&self.command),
            root: Node::Leaf(id),
            focused: id,
            zoomed: false,
        });
        self.active_window = self.windows.len() - 1;
        self.relayout_all();
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
        let content = layout::content_area(self.viewport(), self.windows.len());
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
        if let Some(win) = self.windows.get_mut(self.active_window) {
            if layout::all_panes(&win.root).contains(&pane_id) {
                win.focused = pane_id;
            }
        }
    }

    /// Apply a mouse event: press hit-tests (focus pane / switch tab / begin
    /// divider drag), drag adjusts the dragged divider, release ends the drag.
    /// `drag` is the issuing client's per-connection drag state, recorded as
    /// `(window index at press, path)` so a drag is applied to the window it
    /// started in even if the active window changes mid-drag.
    pub fn handle_mouse(&mut self, ev: MouseEvent, drag: &mut Option<(usize, SplitPath)>) {
        match ev.kind {
            MouseKind::Press => {
                let hit = self.hit(ev.col, ev.row);
                match hit {
                    layout::Hit::Pane(id) => {
                        self.focus(id);
                        *drag = None;
                    }
                    layout::Hit::Tab(idx) => {
                        if idx < self.windows.len() {
                            self.unzoom_active();
                            self.active_window = idx;
                        }
                        *drag = None;
                    }
                    layout::Hit::Divider(path) => *drag = Some((self.active_window, path)),
                    layout::Hit::SidebarEntry { .. } => {
                        // Task 13 wires up the real handler. For now,
                        // a click on the sidebar is a no-op so the build
                        // stays green.
                        *drag = None;
                    }
                    layout::Hit::None => *drag = None,
                }
            }
            MouseKind::Drag => {
                if let Some((wi, path)) = drag.clone() {
                    self.resize_divider(wi, &path, ev.col, ev.row);
                }
            }
            MouseKind::Release => *drag = None,
        }
    }

    /// Hit-test a 1-based pointer against the active window's geometry.
    fn hit(&self, col: u16, row: u16) -> layout::Hit {
        let Some(win) = self.windows.get(self.active_window) else {
            return layout::Hit::None;
        };
        let names: Vec<String> = self.windows.iter().map(|w| w.name.clone()).collect();
        let tabs = layout::tab_layout(
            &self.session_name,
            &names,
            self.active_window,
            win.zoomed,
            self.cols,
        );
        if win.zoomed {
            if col == 0 || row == 0 {
                return layout::Hit::None;
            }
            let (x, y) = (col - 1, row - 1);
            if let Some(ty) = layout::tab_row(self.viewport(), self.windows.len()) {
                if y == ty {
                    return match layout::tab_hit(&tabs, x) {
                        Some(w) => layout::Hit::Tab(w),
                        None => layout::Hit::None,
                    };
                }
            }
            let content = layout::content_area(self.viewport(), self.windows.len());
            if content.contains(x, y) {
                return layout::Hit::Pane(win.focused);
            }
            return layout::Hit::None;
        }
        layout::hit_test(
            &win.root,
            self.viewport(),
            self.windows.len(),
            &tabs,
            col,
            row,
        )
    }

    #[cfg(test)]
    pub fn hit_for_test(&self, col: u16, row: u16) -> layout::Hit {
        self.hit(col, row)
    }

    #[cfg(test)]
    pub fn active_zoomed_for_test(&self) -> bool {
        self.windows
            .get(self.active_window)
            .map(|w| w.zoomed)
            .unwrap_or(false)
    }

    #[cfg(test)]
    pub fn tab_regions_for_test(&self) -> Vec<layout::TabRegion> {
        let names: Vec<String> = self.windows.iter().map(|w| w.name.clone()).collect();
        let zoomed = self
            .windows
            .get(self.active_window)
            .map(|w| w.zoomed)
            .unwrap_or(false);
        layout::tab_layout(
            &self.session_name,
            &names,
            self.active_window,
            zoomed,
            self.cols,
        )
    }

    /// Refresh every window's name from its focused pane's foreground process.
    /// `resolve` maps a pid to a friendly name (injected so tests can fake it;
    /// production passes `crate::procinfo::process_name`). When `resolve` yields
    /// `None` (or there is no foreground pid) the window keeps its current name —
    /// no flicker to a placeholder. Returns true if any name changed.
    pub fn refresh_window_names(&mut self, resolve: impl Fn(i32) -> Option<String>) -> bool {
        let mut changed = false;
        for win in &mut self.windows {
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
    ) -> bool {
        let mut changed = false;
        for pane in self.panes.values_mut() {
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
                        changed = true;
                    }
                    continue;
                }
            };

            // Step 3: Sample tail (20 rows).
            let tail = pane.screen.tail_text(20);

            // Step 4: Classify.
            let candidate = detect::classify(agent, &tail);

            // Step 5: Stabilize.
            match candidate {
                detect::AgentState::Working | detect::AgentState::Blocked => {
                    pane.agent_state = candidate;
                    pane.idle_streak = 0;
                }
                detect::AgentState::Idle => {
                    if pane.agent_state != detect::AgentState::Idle {
                        pane.idle_streak = pane.idle_streak.saturating_add(1);
                        if pane.idle_streak >= 2 {
                            pane.agent_state = detect::AgentState::Idle;
                            pane.idle_streak = 2;
                        }
                    } else {
                        pane.idle_streak = pane.idle_streak.min(2).max(2);
                    }
                }
            }

            if pane.agent_state != prev_state {
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
        let content = layout::content_area(self.viewport(), self.windows.len());
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
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Command;
    use crate::input::{MouseEvent, MouseKind};
    use crate::layout::SplitPath;
    use std::time::Duration;

    async fn build_session_with_one_pane() -> (
        Session,
        crate::layout::PaneId,
        tokio::sync::mpsc::UnboundedReceiver<crate::pane::PaneOutput>,
    ) {
        let cwd = std::env::current_dir().unwrap().to_string_lossy().to_string();
        let (session, pane_id, rx) = Session::new(
            "test".to_string(),
            vec!["sh".to_string(), "-c".to_string(), "sleep 30".to_string()],
            cwd,
            80,
            10,
            crate::theme::Theme::default(),
        )
        .expect("Session::new");
        (session, pane_id, rx)
    }

    #[tokio::test]
    async fn refresh_agent_states_marks_pane_working_after_busy_sample() {
        let (mut session, pane_id, _rx) = build_session_with_one_pane().await;
        session.feed(pane_id, b"esc to interrupt\n");
        let changed = session.refresh_agent_states(|_pid| Some("claude".to_string()));
        assert!(changed, "first refresh should flip pane state");
        let pane = session.panes.get(&pane_id).expect("pane present");
        assert_eq!(pane.agent, Some(detect::Agent::Claude));
        assert_eq!(pane.agent_state, detect::AgentState::Working);
    }

    #[tokio::test]
    async fn refresh_agent_states_marks_pane_blocked_after_blocked_sample() {
        let (mut session, pane_id, _rx) = build_session_with_one_pane().await;
        session.feed(pane_id, b"Do you want to proceed?\n\xe2\x9d\xaf 1. Yes\n");
        let changed = session.refresh_agent_states(|_pid| Some("claude".to_string()));
        assert!(changed);
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
        let changed = session.refresh_agent_states(|_pid| Some("claude".to_string()));
        assert!(!changed);
        let _ = pane_id;
    }

    #[tokio::test]
    async fn refresh_agent_states_ignores_non_agent_panes() {
        let (mut session, pane_id, _rx) = build_session_with_one_pane().await;
        session.feed(pane_id, b"esc to interrupt\n");
        let changed = session.refresh_agent_states(|_pid| Some("zsh".to_string()));
        assert!(!changed);
        let pane = session.panes.get(&pane_id).unwrap();
        assert!(pane.agent.is_none());
        assert_eq!(pane.agent_state, detect::AgentState::Idle);
    }

    #[tokio::test]
    async fn refresh_agent_states_retries_resolver_on_transient_failure() {
        let (mut session, pane_id, _rx) = build_session_with_one_pane().await;
        session.feed(pane_id, b"esc to interrupt\n");
        let changed = session.refresh_agent_states(|_pid| None);
        assert!(!changed);
        let pane = session.panes.get(&pane_id).unwrap();
        assert!(pane.agent.is_none());
        assert!(pane.agent_id_pid.is_none(), "cache key NOT poisoned");

        let changed = session.refresh_agent_states(|_pid| Some("claude".to_string()));
        assert!(changed);
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
        )
        .expect("session");
        // Vertical split: left = first (focused after split is the NEW right pane).
        session.apply_command(Command::SplitVertical);
        let focused_after_split = session.focused_pane();
        assert_ne!(focused_after_split, first);

        // Click column 1 (left pane = the original `first`). 1-based coords.
        let mut drag: Option<(usize, SplitPath)> = None;
        session.handle_mouse(
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
        )
        .expect("session");
        session.apply_command(Command::SplitVertical);
        // The root divider sits at the middle column. Press on it, then drag left.
        let mut drag: Option<(usize, SplitPath)> = None;
        // avail = 41 - 1 = 40; ratio 0.5 => first_w 20 => divider at x 20 => col 21.
        session.handle_mouse(
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
        session.handle_mouse(
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
        session.handle_mouse(
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
        let (session, _id, _rx) =
            Session::new("test".into(), vec!["cat".into()], ".".into(), 30, 8, crate::theme::Theme::default()).unwrap();
        let grid = session.compose();
        assert_eq!(grid.dims(), (30, 8));
        assert_eq!(grid.cells.len(), 30 * 8);
        // render() must still produce a full frame (clear+home prefix).
        assert!(session.render().starts_with(b"\x1b[2J\x1b[H"));
    }

    #[tokio::test]
    async fn refresh_window_names_updates_only_on_change() {
        let (mut session, _id, _rx) =
            Session::new("test".into(), vec!["cat".into()], ".".into(), 30, 8, crate::theme::Theme::default()).unwrap();
        // First resolve to "claude": name changes -> true.
        assert!(session.refresh_window_names(|_pid| Some("claude".to_string())));
        // Same value again: no change -> false.
        assert!(!session.refresh_window_names(|_pid| Some("claude".to_string())));
        // Resolver returns None: keep current name -> false.
        assert!(!session.refresh_window_names(|_pid| None));
    }

    #[tokio::test]
    async fn zoom_shows_only_focused_pane_then_restores() {
        let (mut session, _first, _rx) =
            Session::new("test".into(), vec!["cat".into()], ".".into(), 40, 12, crate::theme::Theme::default()).unwrap();
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
        let (mut session, _first, _rx) =
            Session::new("test".into(), vec!["cat".into()], ".".into(), 40, 12, crate::theme::Theme::default()).unwrap();
        session.apply_command(Command::SplitVertical);
        session.apply_command(Command::ToggleZoom);
        assert_eq!(session.active_pane_rects().len(), 1); // zoomed
        session.apply_command(Command::SplitVertical); // layout change -> unzoom
        assert!(session.active_pane_rects().len() >= 2);
    }

    #[tokio::test]
    async fn focused_pane_exit_in_active_window_auto_unzooms() {
        let (mut session, _first, _rx) =
            Session::new("test".into(), vec!["cat".into()], ".".into(), 40, 12, crate::theme::Theme::default()).unwrap();
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
        let (mut session, _first, _rx) =
            Session::new("test".into(), vec!["cat".into()], ".".into(), 40, 12, crate::theme::Theme::default()).unwrap();
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
        let (mut session, _first, _rx) =
            Session::new("test".into(), vec!["cat".into()], ".".into(), 40, 12, crate::theme::Theme::default()).unwrap();
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
        let (mut session, _first, _rx) =
            Session::new("test".into(), vec!["cat".into()], ".".into(), 40, 12, crate::theme::Theme::default()).unwrap();
        session.apply_command(Command::SplitVertical);
        session.apply_command(Command::ToggleZoom);
        // A click in the right half of the body (where the sibling used to be)
        // must resolve to the focused pane, not the hidden sibling or a divider.
        let hit = session.hit_for_test(35, 5);
        assert_eq!(hit, crate::layout::Hit::Pane(session.focused_pane()));
    }

    #[tokio::test]
    async fn hit_while_zoomed_still_switches_windows_via_tab_click() {
        let (mut session, _first, _rx) =
            Session::new("test".into(), vec!["cat".into()], ".".into(), 40, 12, crate::theme::Theme::default()).unwrap();
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
}
