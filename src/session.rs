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
use crate::ghostty::screen::GhosttyScreen;
use crate::layout::{self, Node, PaneId, Rect};
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
        // One window => no tab bar => the pane fills the whole viewport.
        let (id, rx) = session.spawn_pane(Rect {
            x: 0,
            y: 0,
            w: cols,
            h: rows,
        })?;
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

    /// Render the active window into a full-viewport ANSI frame.
    pub fn render(&mut self) -> Vec<u8> {
        let viewport = Rect {
            x: 0,
            y: 0,
            w: self.cols,
            h: self.rows,
        };
        let names: Vec<String> = self.windows.iter().map(|w| w.name.clone()).collect();
        let refs: Vec<(PaneId, &dyn PaneCells)> = self
            .panes
            .iter()
            .map(|(id, p)| (*id, &p.screen as &dyn PaneCells))
            .collect();
        let win = &self.windows[self.active_window];
        self.compositor.render(
            viewport,
            &win.root,
            &names,
            self.active_window,
            win.focused,
            &refs,
        )
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
        // Clamp to at least 1x1 so a 0-sized client resize can't produce a
        // degenerate viewport (matches the prior single-screen behavior).
        self.cols = cols.max(1);
        self.rows = rows.max(1);
        let viewport = Rect {
            x: 0,
            y: 0,
            w: self.cols,
            h: self.rows,
        };
        let content = layout::content_area(viewport, self.windows.len());
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
                        self.windows[wi].focused = layout::first_leaf(&new_root);
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
        removed
    }

    /// True when no windows remain (the daemon should shut down).
    pub fn is_empty(&self) -> bool {
        self.windows.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
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
}
