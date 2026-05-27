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
use crate::ghostty::screen::GhosttyScreen;
use crate::input::{MouseEvent, MouseKind};
use crate::layout::{self, Node, PaneId, Rect, SplitDir, SplitPath};
use crate::pane::{PaneOutput, PaneRuntime};

struct Pane {
    runtime: PaneRuntime,
    screen: GhosttyScreen,
}

struct Window {
    name: String,
    root: Node,
    focused: PaneId,
}

pub struct Session {
    windows: Vec<Window>,
    active_window: usize,
    panes: HashMap<PaneId, Pane>,
    compositor: Compositor,
    next_pane_id: PaneId,
    cols: u16,
    rows: u16,
    command: Vec<String>,
    cwd: String,
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

impl Session {
    /// Create a session with one window holding a single pane running `command`.
    /// Returns the session, the first pane's id, and its output receiver (the
    /// caller spawns a forwarder that tags the output with the pane id).
    pub fn new(
        command: Vec<String>,
        cwd: String,
        cols: u16,
        rows: u16,
    ) -> io::Result<(Self, PaneId, mpsc::UnboundedReceiver<PaneOutput>)> {
        // Clamp to at least 1x1 (matches GhosttyScreen/PaneRuntime resize behavior).
        let cols = cols.max(1);
        let rows = rows.max(1);
        let mut session = Session {
            windows: Vec::new(),
            active_window: 0,
            panes: HashMap::new(),
            compositor: Compositor::new(),
            next_pane_id: 1,
            cols,
            rows,
            command,
            cwd,
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
            name: "0".to_string(),
            root: Node::Leaf(id),
            focused: id,
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
        let (runtime, rx) = PaneRuntime::spawn(&self.command, &self.cwd, w, h)?;
        let screen = GhosttyScreen::new(w, h);
        let id = self.next_pane_id;
        self.next_pane_id += 1;
        self.panes.insert(id, Pane { runtime, screen });
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
        for win in &self.windows {
            targets.extend(layout::pane_rects(&win.root, content));
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
        self.compositor.compose(
            viewport,
            &win.root,
            &names,
            self.active_window,
            win.focused,
            &refs,
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

    /// Apply a structural command, returning the side effects the run loop must
    /// perform. Marks nothing dirty itself — the caller repaints after applying.
    pub fn apply_command(&mut self, cmd: Command) -> CommandEffect {
        let mut eff = CommandEffect::default();
        match cmd {
            Command::SplitHorizontal => self.split_focused(SplitDir::Horizontal, &mut eff),
            Command::SplitVertical => self.split_focused(SplitDir::Vertical, &mut eff),
            Command::ClosePane => eff.closed = self.close_focused(),
            Command::NewWindow => self.new_window(&mut eff),
            Command::NextWindow => {
                if !self.windows.is_empty() {
                    self.active_window = (self.active_window + 1) % self.windows.len();
                }
            }
            Command::PrevWindow => {
                if !self.windows.is_empty() {
                    let n = self.windows.len();
                    self.active_window = (self.active_window + n - 1) % n;
                }
            }
            Command::SelectWindow(n) => {
                if n >= 1 {
                    let idx = n - 1;
                    if idx < self.windows.len() {
                        self.active_window = idx;
                    }
                }
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
        let name = self.windows.len().to_string();
        self.windows.push(Window {
            name,
            root: Node::Leaf(id),
            focused: id,
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

    /// The (pane id, rect) list for the active window's panes (content area).
    pub fn active_pane_rects(&self) -> Vec<(PaneId, Rect)> {
        let content = layout::content_area(self.viewport(), self.windows.len());
        match self.windows.get(self.active_window) {
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
                            self.active_window = idx;
                        }
                        *drag = None;
                    }
                    layout::Hit::Divider(path) => *drag = Some((self.active_window, path)),
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
        let tabs = layout::tab_layout(&names, self.active_window, self.cols);
        layout::hit_test(
            &win.root,
            self.viewport(),
            self.windows.len(),
            &tabs,
            col,
            row,
        )
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

    #[tokio::test]
    async fn session_renders_pane_output_as_full_frame() {
        let (mut session, pane_id, mut rx) = Session::new(
            vec!["sh".into(), "-c".into(), "printf READY; sleep 5".into()],
            ".".into(),
            40,
            10,
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
        // One window => no tab bar => same full-frame shape as before.
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
            vec!["sh".into(), "-c".into(), "sleep 5".into()],
            ".".into(),
            40,
            10,
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
            vec!["sh".into(), "-c".into(), "true".into()],
            ".".into(),
            40,
            10,
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
            vec!["sh".into(), "-c".into(), "sleep 5".into()],
            ".".into(),
            40,
            12,
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
            vec!["sh".into(), "-c".into(), "sleep 5".into()],
            ".".into(),
            40,
            3,
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
            vec!["sh".into(), "-c".into(), "sleep 5".into()],
            ".".into(),
            40,
            12,
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
            vec!["sh".into(), "-c".into(), "sleep 5".into()],
            ".".into(),
            40,
            12,
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
            vec!["sh".into(), "-c".into(), "sleep 5".into()],
            ".".into(),
            40,
            12,
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
            vec!["sh".into(), "-c".into(), "sleep 5".into()],
            ".".into(),
            40,
            24,
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
            vec!["sh".into(), "-c".into(), "sleep 5".into()],
            ".".into(),
            41,
            12,
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
            vec!["sh".into(), "-c".into(), "sleep 5".into()],
            ".".into(),
            41,
            12,
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
        let (session, _id, _rx) = Session::new(vec!["cat".into()], ".".into(), 30, 8).unwrap();
        let grid = session.compose();
        assert_eq!(grid.dims(), (30, 8));
        assert_eq!(grid.cells.len(), 30 * 8);
        // render() must still produce a full frame (clear+home prefix).
        assert!(session.render().starts_with(b"\x1b[2J\x1b[H"));
    }

    #[tokio::test]
    async fn pane_exit_adjusts_active_window_for_index_after_before_and_at() {
        // Build 3 windows (indices 0,1,2). Exercise window collapse for an index
        // AFTER the active one (no shift), BEFORE it (shift down), and AT it (clamp).
        let (mut session, _first, _rx) = Session::new(
            vec!["sh".into(), "-c".into(), "sleep 5".into()],
            ".".into(),
            40,
            12,
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
}
