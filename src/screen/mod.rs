//! Minimal, pure server-side screen model for Phase 1.
//! Handles a VT subset: printable text, CR/LF, BS, TAB, ED/EL, cursor moves, resize.
//! Replaced by vendored libghostty-vt in Phase 2.

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Cell {
    pub ch: char,
}

impl Default for Cell {
    fn default() -> Self {
        Cell { ch: ' ' }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum ParseState {
    Ground,
    Escape,
    Csi,
}

#[derive(Clone, Debug)]
pub struct Screen {
    cols: u16,
    rows: u16,
    cells: Vec<Cell>,
    cx: u16,
    cy: u16,
    state: ParseState,
    params: Vec<u16>,
    cur_param: Option<u16>,
}

impl Screen {
    pub fn new(cols: u16, rows: u16) -> Self {
        let cols = cols.max(1);
        let rows = rows.max(1);
        Screen {
            cols,
            rows,
            cells: vec![Cell::default(); cols as usize * rows as usize],
            cx: 0,
            cy: 0,
            state: ParseState::Ground,
            params: Vec::new(),
            cur_param: None,
        }
    }

    pub fn cols(&self) -> u16 {
        self.cols
    }

    pub fn rows(&self) -> u16 {
        self.rows
    }

    pub fn cursor(&self) -> (u16, u16) {
        (self.cx, self.cy)
    }

    /// Returns the cell at `(x, y)`.
    ///
    /// # Panics
    /// Panics if `x >= self.cols()` or `y >= self.rows()`.
    pub fn cell(&self, x: u16, y: u16) -> Cell {
        self.cells[self.idx(x, y)]
    }

    fn idx(&self, x: u16, y: u16) -> usize {
        y as usize * self.cols as usize + x as usize
    }

    /// Feed raw PTY output bytes into the screen model.
    pub fn feed(&mut self, bytes: &[u8]) {
        for &b in bytes {
            self.feed_byte(b);
        }
    }

    fn feed_byte(&mut self, b: u8) {
        match self.state {
            ParseState::Ground => self.ground_byte(b),
            ParseState::Escape => self.escape_byte(b),
            ParseState::Csi => self.csi_byte(b),
        }
    }

    fn ground_byte(&mut self, b: u8) {
        match b {
            0x1b => self.state = ParseState::Escape,
            b'\r' => self.cx = 0,
            b'\n' => self.line_feed(),
            0x08 => {
                if self.cx > 0 {
                    self.cx -= 1;
                }
            }
            b'\t' => {
                let next = ((self.cx / 8) + 1) * 8;
                self.cx = next.min(self.cols - 1);
            }
            0x20..=0x7e => self.put_char(b as char),
            _ => {} // ignore other control/non-ASCII bytes in Phase 1
        }
    }

    fn put_char(&mut self, ch: char) {
        if self.cx >= self.cols {
            self.cx = 0;
            self.line_feed();
        }
        let idx = self.idx(self.cx, self.cy);
        self.cells[idx] = Cell { ch };
        self.cx += 1;
    }

    fn line_feed(&mut self) {
        if self.cy + 1 >= self.rows {
            self.scroll_up();
        } else {
            self.cy += 1;
        }
    }

    fn scroll_up(&mut self) {
        let cols = self.cols as usize;
        self.cells.drain(0..cols);
        self.cells
            .extend(std::iter::repeat_n(Cell::default(), cols));
    }

    fn escape_byte(&mut self, b: u8) {
        if b == b'[' {
            self.params.clear();
            self.cur_param = None;
            self.state = ParseState::Csi;
        } else {
            self.state = ParseState::Ground;
        }
    }

    fn csi_byte(&mut self, b: u8) {
        match b {
            b'0'..=b'9' => {
                let digit = (b - b'0') as u16;
                let v = self.cur_param.unwrap_or(0);
                self.cur_param = Some(v.saturating_mul(10).saturating_add(digit));
            }
            b';' => {
                self.params.push(self.cur_param.take().unwrap_or(0));
            }
            0x40..=0x7e => {
                if let Some(v) = self.cur_param.take() {
                    self.params.push(v);
                }
                self.dispatch_csi(b);
                self.state = ParseState::Ground;
            }
            _ => {
                // Unsupported intermediate byte: abort the sequence.
                self.state = ParseState::Ground;
            }
        }
    }

    fn param(&self, i: usize, default: u16) -> u16 {
        match self.params.get(i) {
            Some(0) | None => default,
            Some(&v) => v,
        }
    }

    fn dispatch_csi(&mut self, final_byte: u8) {
        match final_byte {
            b'A' => self.cy = self.cy.saturating_sub(self.param(0, 1)),
            b'B' => self.cy = self.cy.saturating_add(self.param(0, 1)).min(self.rows - 1),
            b'C' => self.cx = self.cx.saturating_add(self.param(0, 1)).min(self.cols - 1),
            b'D' => self.cx = self.cx.saturating_sub(self.param(0, 1)),
            b'H' | b'f' => {
                let row = self.param(0, 1);
                let col = self.param(1, 1);
                self.cy = row.saturating_sub(1).min(self.rows - 1);
                self.cx = col.saturating_sub(1).min(self.cols - 1);
            }
            b'J' => self.erase_display(),
            b'K' => self.erase_line(),
            _ => {} // ignore unsupported CSI commands in Phase 1
        }
    }

    fn erase_display(&mut self) {
        let mode = self.param(0, 0);
        let cur = self.idx(self.cx, self.cy);
        let total = self.cells.len();
        match mode {
            0 => self.clear_range(cur, total),
            1 => self.clear_range(0, cur + 1),
            _ => self.clear_range(0, total),
        }
    }

    fn erase_line(&mut self) {
        let mode = self.param(0, 0);
        let row_start = self.idx(0, self.cy);
        let row_end = row_start + self.cols as usize;
        let cur = self.idx(self.cx, self.cy);
        match mode {
            0 => self.clear_range(cur, row_end),
            1 => self.clear_range(row_start, cur + 1),
            _ => self.clear_range(row_start, row_end),
        }
    }

    fn clear_range(&mut self, start: usize, end: usize) {
        let end = end.min(self.cells.len());
        for cell in &mut self.cells[start..end] {
            *cell = Cell::default();
        }
    }

    /// Resize the grid, preserving the top-left overlapping region.
    pub fn resize(&mut self, cols: u16, rows: u16) {
        let cols = cols.max(1);
        let rows = rows.max(1);
        let mut next = vec![Cell::default(); cols as usize * rows as usize];
        let copy_rows = rows.min(self.rows);
        let copy_cols = cols.min(self.cols);
        for y in 0..copy_rows {
            for x in 0..copy_cols {
                next[y as usize * cols as usize + x as usize] = self.cell(x, y);
            }
        }
        self.cells = next;
        self.cols = cols;
        self.rows = rows;
        self.cx = self.cx.min(cols - 1);
        self.cy = self.cy.min(rows - 1);
    }

    /// Render a full repaint: clear, draw every row, then position the cursor.
    /// Phase 1 always sends full frames; diffing arrives in Phase 2.
    pub fn render_ansi(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(self.cells.len() + 32);
        out.extend_from_slice(b"\x1b[2J\x1b[H");
        for y in 0..self.rows {
            if y > 0 {
                out.extend_from_slice(b"\r\n");
            }
            for x in 0..self.cols {
                let ch = self.cell(x, y).ch;
                let mut buf = [0u8; 4];
                out.extend_from_slice(ch.encode_utf8(&mut buf).as_bytes());
            }
        }
        out.extend_from_slice(format!("\x1b[{};{}H", self.cy + 1, self.cx + 1).as_bytes());
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row_text(s: &Screen, y: u16) -> String {
        (0..s.cols()).map(|x| s.cell(x, y).ch).collect()
    }

    #[test]
    fn new_screen_is_blank_with_cursor_home() {
        let s = Screen::new(80, 24);
        assert_eq!(s.cols(), 80);
        assert_eq!(s.rows(), 24);
        assert_eq!(s.cursor(), (0, 0));
        assert_eq!(s.cell(0, 0).ch, ' ');
        assert_eq!(s.cell(79, 23).ch, ' ');
    }

    #[test]
    fn printable_text_advances_cursor() {
        let mut s = Screen::new(10, 3);
        s.feed(b"hi");
        assert_eq!(s.cell(0, 0).ch, 'h');
        assert_eq!(s.cell(1, 0).ch, 'i');
        assert_eq!(s.cursor(), (2, 0));
    }

    #[test]
    fn text_wraps_at_right_edge() {
        let mut s = Screen::new(3, 3);
        s.feed(b"abcd");
        assert_eq!(row_text(&s, 0), "abc");
        assert_eq!(s.cell(0, 1).ch, 'd');
        assert_eq!(s.cursor(), (1, 1));
    }

    #[test]
    fn carriage_return_and_line_feed() {
        let mut s = Screen::new(10, 3);
        s.feed(b"ab\r\nc");
        assert_eq!(row_text(&s, 0).trim_end(), "ab");
        assert_eq!(s.cell(0, 1).ch, 'c');
        assert_eq!(s.cursor(), (1, 1));
    }

    #[test]
    fn line_feed_scrolls_at_bottom() {
        let mut s = Screen::new(3, 2);
        s.feed(b"a\r\nb\r\nc");
        // After three lines on a 2-row screen, first line scrolled off.
        assert_eq!(row_text(&s, 0).trim_end(), "b");
        assert_eq!(row_text(&s, 1).trim_end(), "c");
        assert_eq!(s.cursor(), (1, 1));
    }

    #[test]
    fn backspace_stops_at_column_zero() {
        let mut s = Screen::new(5, 2);
        s.feed(b"\x08\x08");
        assert_eq!(s.cursor(), (0, 0));
    }

    #[test]
    fn tab_advances_to_multiple_of_eight() {
        let mut s = Screen::new(20, 2);
        s.feed(b"a\t");
        assert_eq!(s.cursor(), (8, 0));
    }

    #[test]
    fn cursor_position_absolute_cup() {
        let mut s = Screen::new(10, 5);
        s.feed(b"\x1b[3;5H"); // row 3, col 5 (1-based)
        assert_eq!(s.cursor(), (4, 2));
    }

    #[test]
    fn cup_with_no_params_homes_cursor() {
        let mut s = Screen::new(10, 5);
        s.feed(b"abc");
        s.feed(b"\x1b[H");
        assert_eq!(s.cursor(), (0, 0));
    }

    #[test]
    fn cursor_relative_moves_clamp_to_bounds() {
        let mut s = Screen::new(10, 5);
        s.feed(b"\x1b[2;2H"); // (1,1)
        s.feed(b"\x1b[1A"); // up 1 -> (1,0)
        assert_eq!(s.cursor(), (1, 0));
        s.feed(b"\x1b[10C"); // right 10, clamps to last col
        assert_eq!(s.cursor(), (9, 0));
        s.feed(b"\x1b[10B"); // down 10, clamps to last row
        assert_eq!(s.cursor(), (9, 4));
        s.feed(b"\x1b[100D"); // left 100, clamps to col 0
        assert_eq!(s.cursor(), (0, 4));
    }

    #[test]
    fn erase_display_all_clears_everything() {
        let mut s = Screen::new(5, 2);
        s.feed(b"abc\r\nde");
        s.feed(b"\x1b[2J");
        assert_eq!(row_text(&s, 0), "     ");
        assert_eq!(row_text(&s, 1), "     ");
    }

    #[test]
    fn erase_display_to_end_clears_from_cursor() {
        let mut s = Screen::new(5, 2);
        s.feed(b"abcde");
        s.feed(b"\x1b[1;3H"); // cursor at col 3 (index 2)
        s.feed(b"\x1b[0J"); // erase from cursor to end of display
        assert_eq!(row_text(&s, 0), "ab   ");
    }

    #[test]
    fn erase_line_to_end_clears_rest_of_row() {
        let mut s = Screen::new(5, 2);
        s.feed(b"abcde");
        s.feed(b"\x1b[1;3H");
        s.feed(b"\x1b[0K"); // erase from cursor to end of line
        assert_eq!(row_text(&s, 0), "ab   ");
    }

    #[test]
    fn erase_line_whole_clears_full_row() {
        let mut s = Screen::new(5, 2);
        s.feed(b"abcde");
        s.feed(b"\x1b[1;3H");
        s.feed(b"\x1b[2K");
        assert_eq!(row_text(&s, 0), "     ");
    }

    #[test]
    fn resize_preserves_top_left_and_clamps_cursor() {
        let mut s = Screen::new(5, 3);
        s.feed(b"hello");
        s.feed(b"\x1b[3;5H"); // cursor near bottom-right
        s.resize(3, 2);
        assert_eq!(s.cols(), 3);
        assert_eq!(s.rows(), 2);
        assert_eq!(row_text(&s, 0), "hel");
        // Cursor was at (4, 2) before resize; must clamp to the new bounds.
        assert_eq!(s.cursor(), (2, 1));
    }

    #[test]
    fn large_cursor_move_params_clamp_without_overflow() {
        let mut s = Screen::new(10, 5);
        s.feed(b"\x1b[2;2H"); // (1, 1)
                              // Huge params must saturate, not overflow u16 (would panic in debug).
        s.feed(b"\x1b[65535B");
        s.feed(b"\x1b[65535C");
        assert_eq!(s.cursor(), (9, 4));
    }

    #[test]
    fn render_ansi_repaints_full_screen_and_positions_cursor() {
        let mut s = Screen::new(3, 2);
        s.feed(b"ab\r\ncd");
        let out = String::from_utf8(s.render_ansi()).unwrap();
        // Starts with clear + home.
        assert!(out.starts_with("\x1b[2J\x1b[H"));
        // Contains the row contents separated by CRLF.
        assert!(out.contains("ab "));
        assert!(out.contains("cd "));
        // Ends by positioning the cursor at its current location (row 2, col 3).
        assert!(out.ends_with("\x1b[2;3H"));
    }
}
