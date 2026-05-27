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
}
