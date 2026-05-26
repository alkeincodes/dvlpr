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
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_screen_is_blank_with_cursor_home() {
        let s = Screen::new(80, 24);
        assert_eq!(s.cols(), 80);
        assert_eq!(s.rows(), 24);
        assert_eq!(s.cursor(), (0, 0));
        assert_eq!(s.cell(0, 0).ch, ' ');
        assert_eq!(s.cell(79, 23).ch, ' ');
    }
}
