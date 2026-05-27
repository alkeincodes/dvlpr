//! Safe wrapper over libghostty-vt: owns one Terminal handle and exposes the
//! `new`/`feed`/`resize`/`render_ansi` surface the daemon renders panes with,
//! plus `cursor`/`cell` for tests and `take_pty_writes` for query replies.
//!
//! Single-owner and `!Send`/`!Sync` (holds a raw pointer). All `unsafe` FFI is
//! confined here; the public API is fully safe.

use std::cell::RefCell;
use std::mem;
use std::os::raw::c_void;
use std::ptr;

use crate::ghostty::sys;

pub struct GhosttyScreen {
    term: sys::GhosttyTerminal,
    cols: u16,
    rows: u16,
    // Heap-stable buffer the write_pty callback appends to. Boxed so its address is
    // stable across moves of GhosttyScreen — the terminal holds a raw userdata
    // pointer to it for the terminal's whole lifetime.
    pty_writes: Box<RefCell<Vec<u8>>>,
}

/// Trampoline libghostty-vt calls (synchronously, inside `vt_write` and inside
/// `resize` when in-band size reporting is on) when the terminal needs to send a
/// reply to the PTY (DSR cursor-position / DECRQM mode / size reports). It appends
/// the reply bytes to the `RefCell<Vec<u8>>` pointed to by `userdata`.
///
/// # Safety
/// `userdata` must be the `*const RefCell<Vec<u8>>` registered in `new`, valid
/// for the terminal's lifetime; `data`/`len` describe a valid slice for the call.
unsafe extern "C" fn write_pty_trampoline(
    _terminal: sys::GhosttyTerminal,
    userdata: *mut c_void,
    data: *const u8,
    len: usize,
) {
    if userdata.is_null() || data.is_null() || len == 0 {
        return;
    }
    // Form only `&RefCell` (never `&mut`): interior mutability keeps this sound and
    // single-threaded (GhosttyScreen is !Send/!Sync).
    let buf = &*(userdata as *const RefCell<Vec<u8>>);
    let slice = std::slice::from_raw_parts(data, len);
    buf.borrow_mut().extend_from_slice(slice);
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
        let pty_writes = Box::new(RefCell::new(Vec::new()));
        // Userdata = stable pointer to the boxed RefCell (the Box keeps the heap
        // address fixed across moves of the returned GhosttyScreen).
        let userdata = (&*pty_writes as *const RefCell<Vec<u8>>) as *const c_void;
        // SAFETY: `term` is valid; `userdata` outlives the terminal (the Box is freed
        // in Drop AFTER ghostty_terminal_free); the trampoline matches the expected
        // GhosttyTerminalWritePtyFn signature. Set USERDATA before WRITE_PTY.
        unsafe {
            let ud_rc = sys::ghostty_terminal_set(
                term,
                sys::GhosttyTerminalOption_GHOSTTY_TERMINAL_OPT_USERDATA,
                userdata,
            );
            debug_assert_eq!(ud_rc, 0, "set USERDATA failed (rc={ud_rc})");
            let cb_rc = sys::ghostty_terminal_set(
                term,
                sys::GhosttyTerminalOption_GHOSTTY_TERMINAL_OPT_WRITE_PTY,
                write_pty_trampoline as *const c_void,
            );
            debug_assert_eq!(cb_rc, 0, "set WRITE_PTY failed (rc={cb_rc})");
        }
        GhosttyScreen {
            term,
            cols,
            rows,
            pty_writes,
        }
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

    /// Drain and return any bytes the terminal asked to write back to the PTY
    /// (query/status replies accumulated during the preceding `feed`/`resize` calls).
    pub fn take_pty_writes(&mut self) -> Vec<u8> {
        std::mem::take(&mut self.pty_writes.borrow_mut())
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
            char::from_u32(cp)
                .filter(|c| !c.is_control())
                .unwrap_or(' ')
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

    pub fn resize(&mut self, cols: u16, rows: u16) {
        let cols = cols.max(1);
        let rows = rows.max(1);
        // SAFETY: `term` is valid; pixel sizes of 0 are accepted for cell-based VT.
        let rc = unsafe { sys::ghostty_terminal_resize(self.term, cols, rows, 0, 0) };
        debug_assert!(rc == 0, "ghostty_terminal_resize failed (rc={rc})");
        self.cols = cols;
        self.rows = rows;
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

    #[test]
    fn plain_text_produces_no_pty_writes() {
        let mut s = GhosttyScreen::new(10, 3);
        s.feed(b"hi");
        assert!(s.take_pty_writes().is_empty());
    }

    #[test]
    fn cursor_position_report_query_is_answered_via_pty_writes() {
        let mut s = GhosttyScreen::new(80, 24);
        // DSR — Cursor Position Report: ESC [ 6 n  → terminal replies ESC [ row ; col R.
        s.feed(b"\x1b[6n");
        let reply = s.take_pty_writes();
        assert!(!reply.is_empty(), "expected a CPR reply, got nothing");
        assert_eq!(reply[0], 0x1b, "reply should start with ESC, got {reply:?}");
        assert_eq!(
            reply[1], b'[',
            "reply should be a CSI sequence, got {reply:?}"
        );
        assert_eq!(
            *reply.last().unwrap(),
            b'R',
            "CPR reply should end in 'R', got {reply:?}"
        );
        // Draining is destructive: a second take returns empty.
        assert!(s.take_pty_writes().is_empty());
    }

    #[test]
    fn resize_with_in_band_size_reports_enabled_produces_a_pty_write() {
        let mut s = GhosttyScreen::new(80, 24);
        // Enable in-band size reporting (DEC private mode 2048). With this on,
        // libghostty-vt emits a size report through the write-pty callback on the
        // NEXT resize. This is the load-bearing reason the server drains after resize.
        s.feed(b"\x1b[?2048h");
        let _ = s.take_pty_writes(); // ignore any reply the enable itself produced
        s.resize(100, 30);
        let report = s.take_pty_writes();
        assert!(
            !report.is_empty(),
            "resize with mode 2048 enabled should emit a size report"
        );
        assert_eq!(
            report[0], 0x1b,
            "size report should start with ESC, got {report:?}"
        );
    }

    #[test]
    fn resize_updates_dimensions_and_keeps_rendering() {
        let mut s = GhosttyScreen::new(10, 4);
        s.feed(b"hello");
        s.resize(4, 2);
        assert_eq!(s.cols(), 4);
        assert_eq!(s.rows(), 2);
        assert_eq!(s.cell(0, 0), 'h');
        let (cx, cy) = s.cursor();
        assert!(cx < 4 && cy < 2);
        let out = s.render_ansi();
        assert!(out.starts_with(b"\x1b[2J\x1b[H"));
    }
}
