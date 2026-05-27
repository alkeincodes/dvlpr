//! Safe wrapper over libghostty-vt: owns one Terminal handle and exposes a
//! Screen-like API that mirrors the Phase 1 hand-written `screen::Screen`,
//! so the server (Phase 2c) can swap to it as a drop-in.
//!
//! Single-owner and `!Send`/`!Sync` (holds a raw pointer). All `unsafe` FFI is
//! confined here; the public API is fully safe.

use std::ptr;

use crate::ghostty::sys;

pub struct GhosttyScreen {
    term: sys::GhosttyTerminal,
    cols: u16,
    rows: u16,
}

impl GhosttyScreen {
    pub fn new(cols: u16, rows: u16) -> Self {
        let cols = cols.max(1);
        let rows = rows.max(1);
        let mut term: sys::GhosttyTerminal = ptr::null_mut();
        let opts = sys::GhosttyTerminalOptions {
            cols,
            rows,
            max_scrollback: 0,
        };
        // SAFETY: `term` is a valid out-pointer; `opts` is a plain POD struct;
        // a null allocator means "use the default allocator".
        let rc = unsafe { sys::ghostty_terminal_new(ptr::null(), &mut term, opts) };
        // Only failure mode is allocation failure (OOM); infallible API to stay a
        // drop-in for the infallible `Screen::new`.
        assert!(
            rc == 0 && !term.is_null(),
            "ghostty_terminal_new failed (rc={rc})"
        );
        GhosttyScreen { term, cols, rows }
    }

    pub fn cols(&self) -> u16 {
        self.cols
    }

    pub fn rows(&self) -> u16 {
        self.rows
    }
}

impl Drop for GhosttyScreen {
    fn drop(&mut self) {
        // SAFETY: `term` was created by `ghostty_terminal_new` and is freed exactly
        // once here (GhosttyScreen is the sole owner and is non-Copy).
        unsafe { sys::ghostty_terminal_free(self.term) };
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_reports_dimensions_and_drops_cleanly() {
        let s = GhosttyScreen::new(80, 24);
        assert_eq!(s.cols(), 80);
        assert_eq!(s.rows(), 24);
        for _ in 0..16 {
            let _ = GhosttyScreen::new(40, 10);
        }
    }
}
