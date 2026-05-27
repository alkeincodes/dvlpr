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

/// Fill a divider's cells with a box-drawing glyph (heavy when it borders the
/// focused pane). Junctions where dividers cross are simple last-write-wins
/// overwrites (proper `┼` junctions are deferred polish).
fn draw_divider(buf: &mut [char], cols: u16, d: &layout::Divider, heavy: bool) {
    let glyph = match (d.dir, heavy) {
        (SplitDir::Vertical, false) => '│',
        (SplitDir::Vertical, true) => '┃',
        (SplitDir::Horizontal, false) => '─',
        (SplitDir::Horizontal, true) => '━',
    };
    for y in d.rect.y..d.rect.y + d.rect.h {
        for x in d.rect.x..d.rect.x + d.rect.w {
            let idx = y as usize * cols as usize + x as usize;
            if idx < buf.len() {
                buf[idx] = glyph;
            }
        }
    }
}

/// True if divider `d` lies on an edge of the `focused` rect (so it should be
/// drawn heavy). Adjacency = the divider's line is immediately beside the rect
/// along the split axis AND the perpendicular ranges overlap.
fn divider_touches(d: &layout::Divider, focused: Rect) -> bool {
    match d.dir {
        SplitDir::Vertical => {
            let dx = d.rect.x;
            let col_adjacent = dx == focused.x + focused.w || dx + 1 == focused.x;
            let rows_overlap =
                d.rect.y < focused.y + focused.h && focused.y < d.rect.y + d.rect.h;
            col_adjacent && rows_overlap
        }
        SplitDir::Horizontal => {
            let dy = d.rect.y;
            let row_adjacent = dy == focused.y + focused.h || dy + 1 == focused.y;
            let cols_overlap =
                d.rect.x < focused.x + focused.w && focused.x < d.rect.x + d.rect.w;
            row_adjacent && cols_overlap
        }
    }
}

/// Draw each tab region's label into row `ty` of the buffer, starting at the
/// region's `x_start` (the same x-ranges hit-testing uses).
fn draw_tabs(buf: &mut [char], cols: u16, ty: u16, regions: &[layout::TabRegion]) {
    for region in regions {
        for (i, ch) in region.label.chars().enumerate() {
            let x = region.x_start as usize + i;
            if x >= cols as usize {
                break;
            }
            let idx = ty as usize * cols as usize + x;
            if idx < buf.len() {
                buf[idx] = ch;
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

    use crate::layout::Divider;

    #[test]
    fn draw_divider_fills_with_light_or_heavy_glyph() {
        let cols: u16 = 5;
        let mut buf = vec![' '; 5 * 2];
        // Vertical divider, 1 wide x 2 tall, at x=2.
        let d = Divider {
            rect: Rect { x: 2, y: 0, w: 1, h: 2 },
            path: vec![],
            dir: SplitDir::Vertical,
        };
        let at = |r: usize, c: usize| r * cols as usize + c;
        draw_divider(&mut buf, cols, &d, false);
        assert_eq!(buf[at(0, 2)], '│');
        assert_eq!(buf[at(1, 2)], '│');
        draw_divider(&mut buf, cols, &d, true);
        assert_eq!(buf[at(0, 2)], '┃'); // heavy
    }

    #[test]
    fn divider_touches_detects_adjacency() {
        // Two panes side by side in a width-11 area: left x0..=4, divider x5, right x6..=10.
        let div = Divider {
            rect: Rect { x: 5, y: 0, w: 1, h: 4 },
            path: vec![],
            dir: SplitDir::Vertical,
        };
        let left = Rect { x: 0, y: 0, w: 5, h: 4 };
        let right = Rect { x: 6, y: 0, w: 5, h: 4 };
        assert!(divider_touches(&div, left)); // divider is on left pane's right edge
        assert!(divider_touches(&div, right)); // divider is on right pane's left edge
        let elsewhere = Rect { x: 0, y: 10, w: 5, h: 4 };
        assert!(!divider_touches(&div, elsewhere)); // no row overlap
    }

    #[test]
    fn draw_tabs_writes_labels_at_their_ranges() {
        let cols: u16 = 20;
        let mut buf = vec![' '; 20 * 2];
        let regions = layout::tab_layout(&["a".to_string(), "b".to_string()], 0, cols);
        // Tab row is the last row (y = 1).
        draw_tabs(&mut buf, cols, 1, &regions);
        // Row 1 is the tab row: its flat offset is 1*cols == cols.
        let row: String = (0..cols).map(|x| buf[cols as usize + x as usize]).collect();
        assert!(row.starts_with("[0*a]")); // active window 0 marked with '*'
        assert!(row.contains("[1:b]"));
    }
}
