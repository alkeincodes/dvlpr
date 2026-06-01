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

use crate::compositor::{CellStyle, Color};
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
    pub fn new(cols: u16, rows: u16, max_scrollback: usize) -> Self {
        let cols = cols.max(1);
        let rows = rows.max(1);
        let mut term: sys::GhosttyTerminal = ptr::null_mut();
        let opts = sys::GhosttyTerminalOptions {
            cols,
            rows,
            max_scrollback,
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
        // SAFETY: a VIEWPORT point at (x, y); `gref.size` set per the API's
        // sized-struct contract; all out-params are valid; errors fall back to space.
        // With the viewport at the live bottom (default, unscrolled), this is
        // identical to an ACTIVE read; when scrolled, this reads scrollback history.
        unsafe {
            let point = sys::GhosttyPoint {
                tag: sys::GhosttyPointTag_GHOSTTY_POINT_TAG_VIEWPORT,
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

    /// Read the codepoint at active-area row `y` (0-based, top of the LIVE
    /// active screen), independent of any scrollback scroll position. Used by
    /// `tail_text` so agent-state classification never reads scrolled history.
    fn active_cell(&self, x: u16, y: u16) -> char {
        if x >= self.cols || y >= self.rows {
            return ' ';
        }
        // SAFETY: an ACTIVE-screen point always refers to the live active area
        // regardless of viewport scroll; same sized-GridRef contract as `cell`.
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

    /// Return the last `rows` rows of the screen as a single flat string
    /// with `'\n'` row separators. Plain text only — styling is dropped.
    /// Returns an empty string if `rows == 0`. If `rows` exceeds the
    /// screen height, returns all rows.
    ///
    /// Trailing whitespace on each row is stripped (cells past the last
    /// non-space character are dropped before joining). Used by the
    /// agent-state classifier; not for rendering.
    pub fn tail_text(&self, rows: u16) -> String {
        if rows == 0 || self.rows == 0 {
            return String::new();
        }
        let take = rows.min(self.rows);
        let start_y = self.rows - take;
        let mut out = String::with_capacity((take as usize) * (self.cols as usize + 1));
        for y in start_y..self.rows {
            let mut line = String::with_capacity(self.cols as usize);
            for x in 0..self.cols {
                line.push(self.active_cell(x, y));
            }
            // Strip trailing whitespace per row.
            while line.ends_with(' ') {
                line.pop();
            }
            out.push_str(&line);
            if y + 1 < self.rows {
                out.push('\n');
            }
        }
        out
    }

    /// The character AND its style at (x, y). A single grid-ref lookup feeds both the
    /// codepoint and the cell style, so this is the path the compositor blits through
    /// (color/attributes preserved). Out-of-range or any FFI failure degrades to a
    /// default-styled `' '`, mirroring `cell`.
    pub fn styled_cell(&self, x: u16, y: u16) -> (char, CellStyle) {
        if x >= self.cols || y >= self.rows {
            return (' ', CellStyle::default());
        }
        // SAFETY: same VIEWPORT point + sized GhosttyGridRef contract as `cell`;
        // all out-params are valid; every error path falls back to a default cell.
        // VIEWPORT follows the scroll position so the compositor renders scrollback
        // correctly; at the unscrolled bottom, VIEWPORT == ACTIVE.
        unsafe {
            let point = sys::GhosttyPoint {
                tag: sys::GhosttyPointTag_GHOSTTY_POINT_TAG_VIEWPORT,
                value: sys::GhosttyPointValue {
                    coordinate: sys::GhosttyPointCoordinate { x, y: y as u32 },
                },
            };
            let mut gref: sys::GhosttyGridRef = mem::zeroed();
            gref.size = mem::size_of::<sys::GhosttyGridRef>();
            if sys::ghostty_terminal_grid_ref(self.term, point, &mut gref) != 0 {
                return (' ', CellStyle::default());
            }

            // Codepoint.
            let mut ch = ' ';
            let mut cell: sys::GhosttyCell = 0;
            if sys::ghostty_grid_ref_cell(&gref, &mut cell) == 0 {
                let mut cp: u32 = 0;
                if sys::ghostty_cell_get(
                    cell,
                    sys::GhosttyCellData_GHOSTTY_CELL_DATA_CODEPOINT,
                    (&raw mut cp).cast(),
                ) == 0
                {
                    ch = char::from_u32(cp)
                        .filter(|c| !c.is_control())
                        .unwrap_or(' ');
                }
            }

            // Style (fg/bg color + text attributes).
            let mut gstyle: sys::GhosttyStyle = mem::zeroed();
            gstyle.size = mem::size_of::<sys::GhosttyStyle>();
            let style = if sys::ghostty_grid_ref_style(&gref, &mut gstyle) == 0 {
                CellStyle {
                    fg: style_color(gstyle.fg_color),
                    bg: style_color(gstyle.bg_color),
                    bold: gstyle.bold,
                    italic: gstyle.italic,
                    faint: gstyle.faint,
                    underline: gstyle.underline != 0,
                    blink: gstyle.blink,
                    inverse: gstyle.inverse,
                    strikethrough: gstyle.strikethrough,
                }
            } else {
                CellStyle::default()
            };

            (ch, style)
        }
    }

    /// Number of scrollback rows currently retained (total rows minus the
    /// visible viewport rows). 0 when no history or when scrollback is disabled.
    pub fn scrollback_rows(&self) -> usize {
        let mut n: usize = 0;
        // SAFETY: `term` is valid; SCROLLBACK_ROWS documents output type `size_t*`.
        unsafe {
            sys::ghostty_terminal_get(
                self.term,
                sys::GhosttyTerminalData_GHOSTTY_TERMINAL_DATA_SCROLLBACK_ROWS,
                (&raw mut n).cast(),
            );
        }
        n
    }

    /// Total rows in the active screen including scrollback.
    pub fn total_rows(&self) -> usize {
        let mut n: usize = 0;
        // SAFETY: `term` is valid; TOTAL_ROWS documents output type `size_t*`.
        unsafe {
            sys::ghostty_terminal_get(
                self.term,
                sys::GhosttyTerminalData_GHOSTTY_TERMINAL_DATA_TOTAL_ROWS,
                (&raw mut n).cast(),
            );
        }
        n
    }

    /// Whether the pane's foreground app currently has ANY mouse tracking mode
    /// active (X10, normal, button-event, or any-event). Used to suppress
    /// drag-to-enter copy mode so a mouse-aware TUI keeps receiving its mouse.
    pub fn mouse_tracking(&self) -> bool {
        let mut tracking: bool = false;
        // SAFETY: `term` is valid; MOUSE_TRACKING documents output type `bool *`.
        unsafe {
            sys::ghostty_terminal_get(
                self.term,
                sys::GhosttyTerminalData_GHOSTTY_TERMINAL_DATA_MOUSE_TRACKING,
                (&raw mut tracking).cast(),
            );
        }
        tracking
    }

    /// Scroll the viewport to the live bottom (active area). Idempotent.
    pub fn scroll_viewport_bottom(&mut self) {
        // SAFETY: `term` is valid; BOTTOM ignores the value union.
        unsafe {
            sys::ghostty_terminal_scroll_viewport(
                self.term,
                sys::GhosttyTerminalScrollViewport {
                    tag: sys::GhosttyTerminalScrollViewportTag_GHOSTTY_SCROLL_VIEWPORT_BOTTOM,
                    value: sys::GhosttyTerminalScrollViewportValue { delta: 0 },
                },
            );
        }
    }

    /// Scroll the viewport to the top of scrollback.
    pub fn scroll_viewport_top(&mut self) {
        // SAFETY: `term` is valid; TOP ignores the value union.
        unsafe {
            sys::ghostty_terminal_scroll_viewport(
                self.term,
                sys::GhosttyTerminalScrollViewport {
                    tag: sys::GhosttyTerminalScrollViewportTag_GHOSTTY_SCROLL_VIEWPORT_TOP,
                    value: sys::GhosttyTerminalScrollViewportValue { delta: 0 },
                },
            );
        }
    }

    /// Scroll the viewport by `delta` rows (negative = up into history).
    pub fn scroll_viewport_delta(&mut self, delta: isize) {
        // SAFETY: `term` is valid; DELTA reads `value.delta`.
        unsafe {
            sys::ghostty_terminal_scroll_viewport(
                self.term,
                sys::GhosttyTerminalScrollViewport {
                    tag: sys::GhosttyTerminalScrollViewportTag_GHOSTTY_SCROLL_VIEWPORT_DELTA,
                    value: sys::GhosttyTerminalScrollViewportValue { delta },
                },
            );
        }
    }

    /// Extract the text covered by the SCREEN-space inclusive range
    /// `(start_x, start_y)`..=`(end_x, end_y)` via libghostty's formatter,
    /// unwrapping soft wraps and trimming trailing whitespace. Returns an
    /// empty string on any FFI failure or out-of-range endpoint.
    ///
    /// `y` is measured from the top of the full screen (scrollback + active),
    /// matching the `SCREEN` point tag.
    pub fn selection_text(&self, start_x: u16, start_y: usize, end_x: u16, end_y: usize) -> String {
        // SCREEN-point y is a u32 in the C ABI; a usize row index above u32::MAX is
        // out of range (the documented empty-string result) rather than a silent
        // truncation to a wrong wrapped row.
        let (Ok(start_y), Ok(end_y)) = (u32::try_from(start_y), u32::try_from(end_y)) else {
            return String::new();
        };
        // SAFETY: all FFI below uses sized structs (size set), SCREEN-tag points,
        // and the documented buffer-size query protocol; every error path returns
        // an empty string.
        unsafe {
            let mut start_ref: sys::GhosttyGridRef = mem::zeroed();
            start_ref.size = mem::size_of::<sys::GhosttyGridRef>();
            let start_point = sys::GhosttyPoint {
                tag: sys::GhosttyPointTag_GHOSTTY_POINT_TAG_SCREEN,
                value: sys::GhosttyPointValue {
                    coordinate: sys::GhosttyPointCoordinate { x: start_x, y: start_y },
                },
            };
            if sys::ghostty_terminal_grid_ref(self.term, start_point, &mut start_ref) != 0 {
                return String::new();
            }
            let mut end_ref: sys::GhosttyGridRef = mem::zeroed();
            end_ref.size = mem::size_of::<sys::GhosttyGridRef>();
            let end_point = sys::GhosttyPoint {
                tag: sys::GhosttyPointTag_GHOSTTY_POINT_TAG_SCREEN,
                value: sys::GhosttyPointValue {
                    coordinate: sys::GhosttyPointCoordinate { x: end_x, y: end_y },
                },
            };
            if sys::ghostty_terminal_grid_ref(self.term, end_point, &mut end_ref) != 0 {
                return String::new();
            }

            let mut selection: sys::GhosttySelection = mem::zeroed();
            selection.size = mem::size_of::<sys::GhosttySelection>();
            selection.start = start_ref;
            selection.end = end_ref;
            selection.rectangle = false;

            let mut screen_extra: sys::GhosttyFormatterScreenExtra = mem::zeroed();
            screen_extra.size = mem::size_of::<sys::GhosttyFormatterScreenExtra>();
            let mut extra: sys::GhosttyFormatterTerminalExtra = mem::zeroed();
            extra.size = mem::size_of::<sys::GhosttyFormatterTerminalExtra>();
            extra.screen = screen_extra;

            let opts = sys::GhosttyFormatterTerminalOptions {
                size: mem::size_of::<sys::GhosttyFormatterTerminalOptions>(),
                emit: sys::GhosttyFormatterFormat_GHOSTTY_FORMATTER_FORMAT_PLAIN,
                unwrap: true,
                trim: true,
                extra,
                selection: &selection,
            };

            let mut formatter: sys::GhosttyFormatter = ptr::null_mut();
            if sys::ghostty_formatter_terminal_new(ptr::null(), &mut formatter, self.term, opts) != 0
                || formatter.is_null()
            {
                return String::new();
            }

            // Query the required length (buf = null → OUT_OF_SPACE with size).
            let mut needed: usize = 0;
            let rc = sys::ghostty_formatter_format_buf(formatter, ptr::null_mut(), 0, &mut needed);
            // SUCCESS with 0 needed means empty output; OUT_OF_SPACE means `needed` is the size.
            if needed == 0 {
                sys::ghostty_formatter_free(formatter);
                return String::new();
            }
            let mut buf = vec![0u8; needed];
            let mut written: usize = 0;
            let rc2 = sys::ghostty_formatter_format_buf(
                formatter,
                buf.as_mut_ptr(),
                buf.len(),
                &mut written,
            );
            sys::ghostty_formatter_free(formatter);
            let _ = rc;
            if rc2 != sys::GhosttyResult_GHOSTTY_SUCCESS {
                return String::new();
            }
            buf.truncate(written);
            String::from_utf8_lossy(&buf).into_owned()
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

    /// Read the current OSC 0 / OSC 2 terminal title from libghostty-vt.
    /// libghostty-vt returns a borrowed `GhosttyString` whose backing
    /// buffer is owned by the terminal and may be overwritten on the
    /// next OSC; we clone immediately into an owned `String`.
    pub fn title(&self) -> Option<String> {
        let mut out: sys::GhosttyString = unsafe { std::mem::zeroed() };
        // SAFETY: `term` is a valid GhosttyTerminal; the out-pointer is
        // a properly-sized GhosttyString; the tag is the documented one
        // for OSC titles.
        unsafe {
            sys::ghostty_terminal_get(
                self.term,
                sys::GhosttyTerminalData_GHOSTTY_TERMINAL_DATA_TITLE,
                (&raw mut out).cast(),
            );
        }
        if out.ptr.is_null() || out.len == 0 {
            return None;
        }
        // SAFETY: libghostty-vt guarantees ptr/len describe a valid UTF-8
        // (or at worst lossy-decodable) slice for the lifetime of this
        // call; we copy out before returning.
        let bytes: &[u8] = unsafe { std::slice::from_raw_parts(out.ptr.cast::<u8>(), out.len) };
        Some(String::from_utf8_lossy(bytes).into_owned())
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

/// Map a libghostty-vt cell color to our `Color`. `NONE` (and any unknown tag) becomes
/// `Default` so the client renders it with its own terminal theme; palette indices and
/// RGB are preserved distinctly (we re-emit `38;5;n` vs `38;2;r;g;b`).
fn style_color(c: sys::GhosttyStyleColor) -> Color {
    match c.tag {
        // SAFETY: the union member is selected by `tag`, per the C API contract.
        sys::GhosttyStyleColorTag_GHOSTTY_STYLE_COLOR_PALETTE => {
            Color::Palette(unsafe { c.value.palette })
        }
        sys::GhosttyStyleColorTag_GHOSTTY_STYLE_COLOR_RGB => {
            let rgb = unsafe { c.value.rgb };
            Color::Rgb(rgb.r, rgb.g, rgb.b)
        }
        _ => Color::Default,
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
        let s = GhosttyScreen::new(80, 24, 0);
        assert_eq!(s.cols(), 80);
        assert_eq!(s.rows(), 24);
        for _ in 0..16 {
            let _ = GhosttyScreen::new(40, 10, 0);
        }
    }

    #[test]
    fn feed_text_lands_in_cells() {
        let mut s = GhosttyScreen::new(10, 3, 0);
        s.feed(b"hi");
        assert_eq!(s.cell(0, 0), 'h');
        assert_eq!(s.cell(1, 0), 'i');
        assert_eq!(s.cell(2, 0), ' ');
        assert_eq!(s.cell(0, 1), ' ');
    }

    #[test]
    fn out_of_bounds_cell_is_space() {
        let mut s = GhosttyScreen::new(5, 2, 0);
        s.feed(b"x");
        assert_eq!(s.cell(99, 99), ' ');
    }

    #[test]
    fn styled_cell_carries_truecolor_fg_and_bold() {
        let mut s = GhosttyScreen::new(10, 2, 0);
        // SGR: bold + 24-bit fg (10,20,30), then 'X'.
        s.feed(b"\x1b[1;38;2;10;20;30mX");
        let (ch, style) = s.styled_cell(0, 0);
        assert_eq!(ch, 'X');
        assert!(style.bold, "bold attribute should survive");
        assert_eq!(style.fg, Color::Rgb(10, 20, 30));
    }

    #[test]
    fn styled_cell_carries_palette_color() {
        let mut s = GhosttyScreen::new(10, 2, 0);
        // SGR: 256-color palette index 4 as fg, then 'Z'.
        s.feed(b"\x1b[38;5;4mZ");
        let (ch, style) = s.styled_cell(0, 0);
        assert_eq!(ch, 'Z');
        assert_eq!(style.fg, Color::Palette(4));
    }

    #[test]
    fn styled_cell_for_plain_text_is_default_styled() {
        let mut s = GhosttyScreen::new(10, 2, 0);
        s.feed(b"y");
        let (ch, style) = s.styled_cell(0, 0);
        assert_eq!(ch, 'y');
        assert_eq!(style, CellStyle::default());
    }

    #[test]
    fn styled_cell_out_of_bounds_is_default() {
        let s = GhosttyScreen::new(5, 2, 0);
        assert_eq!(s.styled_cell(99, 99), (' ', CellStyle::default()));
    }

    #[test]
    fn cursor_advances_with_input() {
        let mut s = GhosttyScreen::new(10, 3, 0);
        s.feed(b"hi");
        assert_eq!(s.cursor(), (2, 0));
    }

    #[test]
    fn cursor_moves_to_next_row_after_crlf() {
        let mut s = GhosttyScreen::new(10, 3, 0);
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
        let mut s = GhosttyScreen::new(3, 2, 0);
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
        let mut s = GhosttyScreen::new(10, 3, 0);
        s.feed(b"hi");
        assert!(s.take_pty_writes().is_empty());
    }

    #[test]
    fn cursor_position_report_query_is_answered_via_pty_writes() {
        let mut s = GhosttyScreen::new(80, 24, 0);
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
        let mut s = GhosttyScreen::new(80, 24, 0);
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
        let mut s = GhosttyScreen::new(10, 4, 0);
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

    #[test]
    fn focus_mode_toggle_does_not_emit_pty_writes() {
        // The design depends on libghostty-vt absorbing DECSET 1004 (focus reporting)
        // as a stored mode without writing any PTY reply back. If a future vendor
        // update started echoing focus toggles, pane-app focus chatter would leak
        // upstream to the host terminal — exactly the "dvlpr owns host focus"
        // guarantee this test pins.
        let mut s = GhosttyScreen::new(80, 24, 0);
        s.feed(b"\x1b[?1004h");
        assert!(
            s.take_pty_writes().is_empty(),
            "DECSET 1004 (enable) must not produce a PTY reply"
        );
        s.feed(b"\x1b[?1004l");
        assert!(
            s.take_pty_writes().is_empty(),
            "DECSET 1004 (disable) must not produce a PTY reply"
        );
    }

    #[test]
    fn tail_text_returns_last_n_rows_in_order() {
        let mut s = GhosttyScreen::new(20, 5, 0);
        s.feed(b"row1\r\nrow2\r\nrow3\r\nrow4\r\nrow5");
        let tail = s.tail_text(3);
        assert!(tail.contains("row3"), "tail: {tail:?}");
        assert!(tail.contains("row4"), "tail: {tail:?}");
        assert!(tail.contains("row5"), "tail: {tail:?}");
        assert!(!tail.contains("row2"), "tail: {tail:?}");
        assert!(!tail.contains("row1"), "tail: {tail:?}");
    }

    #[test]
    fn tail_text_clamps_to_screen_height() {
        let mut s = GhosttyScreen::new(20, 3, 0);
        s.feed(b"a\r\nb\r\nc");
        let tail = s.tail_text(10);
        assert!(tail.contains("a"), "tail: {tail:?}");
        assert!(tail.contains("b"), "tail: {tail:?}");
        assert!(tail.contains("c"), "tail: {tail:?}");
    }

    #[test]
    fn tail_text_zero_rows_returns_empty() {
        let mut s = GhosttyScreen::new(20, 5, 0);
        s.feed(b"hello");
        assert_eq!(s.tail_text(0), "");
    }

    #[test]
    fn tail_text_trims_trailing_blanks_per_row() {
        let mut s = GhosttyScreen::new(20, 2, 0);
        s.feed(b"hi");
        let tail = s.tail_text(2);
        for line in tail.lines() {
            assert_eq!(line, line.trim_end(), "line should be trimmed: {line:?}");
        }
    }

    #[test]
    fn title_returns_none_when_no_osc_title_received() {
        let s = GhosttyScreen::new(20, 5, 0);
        assert_eq!(s.title(), None);
    }

    #[test]
    fn title_returns_cloned_string_after_osc_0_sequence() {
        let mut s = GhosttyScreen::new(20, 5, 0);
        // OSC 0 ; my-title BEL  — sets both icon name and window title.
        s.feed(b"\x1b]0;my-title\x07");
        assert_eq!(s.title().as_deref(), Some("my-title"));
    }

    #[test]
    fn title_returns_cloned_string_after_osc_2_sequence() {
        let mut s = GhosttyScreen::new(20, 5, 0);
        // OSC 2 ; window-title BEL — sets only the window title.
        s.feed(b"\x1b]2;window-title\x07");
        assert_eq!(s.title().as_deref(), Some("window-title"));
    }

    #[test]
    fn viewport_and_active_reads_match_at_bottom() {
        // Parity: with the viewport at the bottom (not scrolled), VIEWPORT reads
        // are identical to the historical ACTIVE reads.
        let mut s = GhosttyScreen::new(10, 3, 100);
        s.feed(b"ab\r\ncd\r\nef");
        assert_eq!(s.cell(0, 0), 'a');
        assert_eq!(s.cell(1, 0), 'b');
        assert_eq!(s.cell(0, 2), 'e');
    }

    #[test]
    fn scroll_up_then_styled_cell_reads_scrolled_content() {
        let mut s = GhosttyScreen::new(10, 2, 100);
        for i in 0..10 {
            s.feed(format!("row{i:02}\r\n").as_bytes());
        }
        // Live bottom shows the last rows; scroll up by 5 and the viewport top row
        // should now read scrolled-back content (not the live bottom).
        s.scroll_viewport_delta(-5);
        let top: String = (0..s.cols()).map(|x| s.cell(x, 0)).collect();
        assert!(top.trim_end().starts_with("row"), "viewport top after scroll: {top:?}");
        s.scroll_viewport_bottom();
        let bottom_top: String = (0..s.cols()).map(|x| s.cell(x, 0)).collect();
        assert_ne!(top, bottom_top, "scrolling must change what the viewport top shows");
    }

    #[test]
    fn tail_text_classifies_against_live_bottom_even_when_scrolled() {
        // Collateral guard: agent-state classification must read the LIVE screen
        // bottom, not the scrolled viewport.
        let mut s = GhosttyScreen::new(10, 2, 100);
        for i in 0..10 {
            s.feed(format!("r{i}\r\n").as_bytes());
        }
        s.feed(b"LIVE9");        // last line at the live bottom
        s.scroll_viewport_delta(-8); // scroll far up into history
        let tail = s.tail_text(2);
        assert!(tail.contains("LIVE9"), "tail must read live bottom, got {tail:?}");
    }

    #[test]
    fn selection_text_extracts_a_single_line_range() {
        let mut s = GhosttyScreen::new(20, 3, 100);
        s.feed(b"hello world");
        // Whole first row, columns 0..=10.
        let txt = s.selection_text(0, 0, 10, 0);
        assert!(txt.contains("hello"), "got {txt:?}");
        assert!(txt.contains("world"), "got {txt:?}");
    }

    #[test]
    fn selection_text_unwraps_a_soft_wrapped_line() {
        let mut s = GhosttyScreen::new(5, 4, 100);
        // 8 chars into a 5-col screen soft-wraps onto row 1.
        s.feed(b"abcdefgh");
        // Select the whole wrapped logical line (rows 0..=1).
        let txt = s.selection_text(0, 0, 4, 1);
        assert!(txt.contains("abcdefgh"), "soft-wrap should unwrap to one line: {txt:?}");
    }

    #[test]
    fn selection_text_empty_for_inverted_or_oob_range_is_safe() {
        let s = GhosttyScreen::new(5, 2, 0);
        let _ = s.selection_text(0, 0, 4, 0); // must not panic
    }

    #[test]
    fn new_with_scrollback_accepts_more_than_rows_of_output() {
        let mut s = GhosttyScreen::new(10, 3, 100);
        for i in 0..20 {
            s.feed(format!("line{i}\r\n").as_bytes());
        }
        assert!(s.scrollback_rows() > 0, "feeding > rows must accumulate scrollback");
        assert!(s.total_rows() >= s.rows() as usize);
    }

    #[test]
    fn zero_scrollback_keeps_no_history() {
        let mut s = GhosttyScreen::new(10, 3, 0);
        for i in 0..20 {
            s.feed(format!("l{i}\r\n").as_bytes());
        }
        assert_eq!(s.scrollback_rows(), 0);
    }

    #[test]
    fn mouse_tracking_reflects_decset_enable_and_disable() {
        let mut s = GhosttyScreen::new(80, 24, 0);
        assert!(!s.mouse_tracking(), "fresh terminal has no mouse tracking");
        // DECSET 1002 (button-event tracking) enables a mouse-tracking mode.
        s.feed(b"\x1b[?1002h");
        assert!(s.mouse_tracking(), "?1002h must enable mouse tracking");
        s.feed(b"\x1b[?1002l");
        assert!(!s.mouse_tracking(), "?1002l must disable mouse tracking");
        // DECSET 1000 (normal tracking) likewise reads as active.
        s.feed(b"\x1b[?1000h");
        assert!(s.mouse_tracking(), "?1000h must enable mouse tracking");
    }
}
