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
            .extend(std::iter::repeat(Cell::default()).take(cols));
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

    fn csi_byte(&mut self, _b: u8) {
        // Fully implemented in Task 4/5; for now consume until a final byte.
        if (0x40..=0x7e).contains(&_b) {
            self.state = ParseState::Ground;
        }
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
}
