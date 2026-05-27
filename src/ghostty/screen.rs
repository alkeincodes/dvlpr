//! Safe wrapper over libghostty-vt: owns one Terminal handle and exposes a
//! Screen-like API that mirrors the Phase 1 hand-written `screen::Screen`,
//! so the server (Phase 2c) can swap to it as a drop-in.
//!
//! Single-owner and `!Send`/`!Sync` (holds a raw pointer). All `unsafe` FFI is
//! confined here; the public API is fully safe.

use std::mem;
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

    pub fn feed(&mut self, bytes: &[u8]) {
        if bytes.is_empty() {
            return;
        }
        // SAFETY: `term` is valid; `bytes` ptr+len describe a valid slice.
        unsafe { sys::ghostty_terminal_vt_write(self.term, bytes.as_ptr(), bytes.len()) };
    }

    pub fn cell(&self, x: u16, y: u16) -> char {
        if x >= self.cols || y >= self.rows {
            return ' ';
        }
        // SAFETY: an ACTIVE-screen point at (x, y); `gref.size` set per the API's
        // sized-struct contract; all out-params are valid; errors fall back to space.
        unsafe {
            let point = sys::GhosttyPoint {
                tag: sys::GhosttyPointTag_GHOSTTY_POINT_TAG_ACTIVE,
                value: sys::GhosttyPointValue {
                    coordinate: sys::GhosttyPointCoordinate { x, y: y as u32 },
                },
            };
            let mut gref: sys::GhosttyGridRef = mem::zeroed();
            gref.size = mem::size_of::<sys::GhosttyGridRef>();
            if sys::ghostty_terminal_grid_ref(self.term, point, &mut gref) != 0 {
                return ' ';
            }
            let mut cell: sys::GhosttyCell = 0;
            if sys::ghostty_grid_ref_cell(&gref, &mut cell) != 0 {
                return ' ';
            }
            let mut cp: u32 = 0;
            if sys::ghostty_cell_get(
                cell,
                sys::GhosttyCellData_GHOSTTY_CELL_DATA_CODEPOINT,
                (&raw mut cp).cast(),
            ) != 0
            {
                return ' ';
            }
            char::from_u32(cp).filter(|c| !c.is_control()).unwrap_or(' ')
        }
    }

    pub fn cursor(&self) -> (u16, u16) {
        let mut cx: u16 = 0;
        let mut cy: u16 = 0;
        // SAFETY: `term` is valid; out-pointers are valid `u16`s matching the
        // documented output type for CURSOR_X / CURSOR_Y.
        unsafe {
            sys::ghostty_terminal_get(
                self.term,
                sys::GhosttyTerminalData_GHOSTTY_TERMINAL_DATA_CURSOR_X,
                (&raw mut cx).cast(),
            );
            sys::ghostty_terminal_get(
                self.term,
                sys::GhosttyTerminalData_GHOSTTY_TERMINAL_DATA_CURSOR_Y,
                (&raw mut cy).cast(),
            );
        }
        (cx.min(self.cols - 1), cy.min(self.rows - 1))
    }

    pub fn render_ansi(&self) -> Vec<u8> {
        let cells = self.cols as usize * self.rows as usize;
        let mut out = Vec::with_capacity(cells + self.rows as usize * 2 + 32);
        out.extend_from_slice(b"\x1b[2J\x1b[H");
        for y in 0..self.rows {
            if y > 0 {
                out.extend_from_slice(b"\r\n");
            }
            for x in 0..self.cols {
                let mut buf = [0u8; 4];
                out.extend_from_slice(self.cell(x, y).encode_utf8(&mut buf).as_bytes());
            }
        }
        let (cx, cy) = self.cursor();
        out.extend_from_slice(format!("\x1b[{};{}H", cy + 1, cx + 1).as_bytes());
        out
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

    #[test]
    fn feed_text_lands_in_cells() {
        let mut s = GhosttyScreen::new(10, 3);
        s.feed(b"hi");
        assert_eq!(s.cell(0, 0), 'h');
        assert_eq!(s.cell(1, 0), 'i');
        assert_eq!(s.cell(2, 0), ' ');
        assert_eq!(s.cell(0, 1), ' ');
    }

    #[test]
    fn out_of_bounds_cell_is_space() {
        let mut s = GhosttyScreen::new(5, 2);
        s.feed(b"x");
        assert_eq!(s.cell(99, 99), ' ');
    }

    #[test]
    fn cursor_advances_with_input() {
        let mut s = GhosttyScreen::new(10, 3);
        s.feed(b"hi");
        assert_eq!(s.cursor(), (2, 0));
    }

    #[test]
    fn cursor_moves_to_next_row_after_crlf() {
        let mut s = GhosttyScreen::new(10, 3);
        s.feed(b"ab\r\ncd");
        assert_eq!(s.cell(0, 1), 'c');
        assert_eq!(s.cell(1, 1), 'd');
        let (_cx, cy) = s.cursor();
        assert_eq!(cy, 1);
    }

    fn row_text(s: &GhosttyScreen, y: u16) -> String {
        (0..s.cols()).map(|x| s.cell(x, y)).collect()
    }

    #[test]
    fn render_ansi_matches_phase1_full_frame_format() {
        let mut s = GhosttyScreen::new(3, 2);
        s.feed(b"ab\r\ncd");
        let out = String::from_utf8(s.render_ansi()).unwrap();
        assert!(out.starts_with("\x1b[2J\x1b[H"));
        assert_eq!(row_text(&s, 0), "ab ");
        assert_eq!(row_text(&s, 1), "cd ");
        assert!(out.contains("ab "));
        assert!(out.contains("cd "));
        let (cx, cy) = s.cursor();
        assert!(out.ends_with(&format!("\x1b[{};{}H", cy + 1, cx + 1)));
    }
}
