//! Composites the active window's panes + dividers + tab bar into one full-viewport
//! ANSI frame, in the same wire format the thin client already paints. Reads pane
//! contents through the `PaneCells` trait, so it is unit-testable against stubs
//! (no PTY/FFI). No server wiring lives here — Step 3 drives it from `run()`.

use crate::layout::{self, Node, PaneId, Rect, SplitDir};

/// Read-only cell access the compositor needs from a pane's screen.
///
/// Contract: `cell(x, y)` MUST be safe for any `x`/`y` and return `' '` for
/// out-of-range coordinates (both `GhosttyScreen` and the test stub do this), so
/// `blit_pane` can read a pane's rect without bounds-checking against cols/rows.
pub trait PaneCells {
    fn cols(&self) -> u16;
    fn rows(&self) -> u16;
    /// The character at (x, y); returns `' '` for out-of-range coordinates.
    fn cell(&self, x: u16, y: u16) -> char;
    fn cursor(&self) -> (u16, u16);
}

impl PaneCells for crate::ghostty::screen::GhosttyScreen {
    fn cols(&self) -> u16 {
        self.cols()
    }
    fn rows(&self) -> u16 {
        self.rows()
    }
    fn cell(&self, x: u16, y: u16) -> char {
        self.cell(x, y)
    }
    fn cursor(&self) -> (u16, u16) {
        self.cursor()
    }
}

/// Composites frames; owns a reusable viewport-sized cell buffer.
#[derive(Default)]
pub struct Compositor {
    buf: Vec<char>,
}

impl Compositor {
    pub fn new() -> Self {
        Self::default()
    }
}

/// Copy a pane's cells into `buf` (a `cols`-wide grid) at `rect`'s offset.
fn blit_pane(buf: &mut [char], cols: u16, rect: Rect, pane: &dyn PaneCells) {
    for y in 0..rect.h {
        for x in 0..rect.w {
            let bx = rect.x + x;
            let by = rect.y + y;
            let idx = by as usize * cols as usize + bx as usize;
            if idx < buf.len() {
                buf[idx] = pane.cell(x, y); // pane.cell returns ' ' for out-of-range
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A fake pane screen for tests: fixed-size grid of rows + a cursor.
    struct StubScreen {
        cols: u16,
        rows: u16,
        lines: Vec<String>,
        cursor: (u16, u16),
    }

    impl StubScreen {
        fn new(cols: u16, rows: u16, lines: &[&str], cursor: (u16, u16)) -> Self {
            StubScreen {
                cols,
                rows,
                lines: lines.iter().map(|s| s.to_string()).collect(),
                cursor,
            }
        }
    }

    impl PaneCells for StubScreen {
        fn cols(&self) -> u16 {
            self.cols
        }
        fn rows(&self) -> u16 {
            self.rows
        }
        fn cell(&self, x: u16, y: u16) -> char {
            self.lines
                .get(y as usize)
                .and_then(|r| r.chars().nth(x as usize))
                .unwrap_or(' ')
        }
        fn cursor(&self) -> (u16, u16) {
            self.cursor
        }
    }

    #[test]
    fn compositor_constructs() {
        let c = Compositor::new();
        assert!(c.buf.is_empty());
    }

    #[test]
    fn blit_copies_cells_at_offset() {
        // 4x3 viewport buffer, blit a 2x2 pane "ab"/"cd" at offset (1,1).
        let cols: u16 = 4;
        let mut buf = vec![' '; 4 * 3];
        let pane = StubScreen::new(2, 2, &["ab", "cd"], (0, 0));
        blit_pane(&mut buf, cols, Rect { x: 1, y: 1, w: 2, h: 2 }, &pane);
        let at = |r: usize, c: usize| r * cols as usize + c;
        // Row 1: positions 1,2 == 'a','b'. Row 2: positions 1,2 == 'c','d'.
        assert_eq!(buf[at(1, 1)], 'a');
        assert_eq!(buf[at(1, 2)], 'b');
        assert_eq!(buf[at(2, 1)], 'c');
        assert_eq!(buf[at(2, 2)], 'd');
        assert_eq!(buf[at(0, 0)], ' '); // untouched cell stays blank
    }
}
