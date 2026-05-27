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

    /// Composite the active window into one full-viewport ANSI frame.
    ///
    /// `tab_names` has one entry per window (so `window_count == tab_names.len()`);
    /// `focused` is the active window's focused pane; `panes` provides cell access
    /// for the (visible) panes of the active window.
    ///
    /// Contract: `viewport` is the full client viewport rooted at the origin
    /// (`viewport.x == 0 && viewport.y == 0`); the buffer is sized `w*h` and indexed
    /// by absolute coords, so a non-zero origin is not supported (the daemon always
    /// composites the whole viewport at (0,0)).
    pub fn render(
        &mut self,
        viewport: Rect,
        root: &Node,
        tab_names: &[String],
        active_window: usize,
        focused: PaneId,
        panes: &[(PaneId, &dyn PaneCells)],
    ) -> Vec<u8> {
        debug_assert!(
            viewport.x == 0 && viewport.y == 0,
            "render expects a viewport rooted at (0, 0)"
        );
        let cols = viewport.w;
        let rows = viewport.h;
        self.buf.clear();
        self.buf.resize(cols as usize * rows as usize, ' ');

        let window_count = tab_names.len();
        let content = layout::content_area(viewport, window_count);
        let rects = layout::pane_rects(root, content);

        // Panes.
        for (id, rect) in &rects {
            if let Some(p) = lookup(panes, *id) {
                blit_pane(&mut self.buf, cols, *rect, p);
            }
        }

        // Dividers (heavy on the focused pane's borders).
        let focused_rect = rects.iter().find(|(id, _)| *id == focused).map(|(_, r)| *r);
        for d in layout::dividers(root, content) {
            let heavy = focused_rect.is_some_and(|fr| divider_touches(&d, fr));
            draw_divider(&mut self.buf, cols, &d, heavy);
        }

        // Tab bar (only when >1 window).
        if let Some(ty) = layout::tab_row(viewport, window_count) {
            let regions = layout::tab_layout(tab_names, active_window, cols);
            draw_tabs(&mut self.buf, cols, ty, &regions);
        }

        // Serialize the buffer to the full-frame wire format.
        let mut out = Vec::with_capacity(self.buf.len() * 2 + rows as usize * 2 + 16);
        out.extend_from_slice(b"\x1b[2J\x1b[H");
        let mut tmp = [0u8; 4];
        for y in 0..rows {
            if y > 0 {
                out.extend_from_slice(b"\r\n");
            }
            for x in 0..cols {
                let ch = self.buf[y as usize * cols as usize + x as usize];
                out.extend_from_slice(ch.encode_utf8(&mut tmp).as_bytes());
            }
        }

        // Cursor at the focused pane's cursor, mapped to global coordinates.
        let (gx, gy) = match focused_rect {
            Some(fr) => {
                let (cx, cy) = lookup(panes, focused).map_or((0, 0), |p| p.cursor());
                (
                    fr.x + cx.min(fr.w.saturating_sub(1)),
                    fr.y + cy.min(fr.h.saturating_sub(1)),
                )
            }
            None => (0, 0),
        };
        out.extend_from_slice(format!("\x1b[{};{}H", gy + 1, gx + 1).as_bytes());
        out
    }
}

/// Find a pane's cell source by id (linear scan — pane counts are small).
fn lookup<'a>(panes: &'a [(PaneId, &'a dyn PaneCells)], id: PaneId) -> Option<&'a dyn PaneCells> {
    panes.iter().find(|(pid, _)| *pid == id).map(|(_, p)| *p)
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

    #[test]
    fn render_single_pane_matches_full_frame_format() {
        // One window, one pane filling a 3x2 viewport. No tab bar, no dividers.
        let tree = Node::Leaf(1);
        let pane = StubScreen::new(3, 2, &["ab", "cd"], (1, 0));
        let mut c = Compositor::new();
        let vp = Rect { x: 0, y: 0, w: 3, h: 2 };
        let out = c.render(vp, &tree, &["w0".to_string()], 0, 1, &[(1, &pane)]);
        let s = String::from_utf8(out).unwrap();
        // Clear+home prefix, the two rows joined by CRLF, then the cursor at the
        // focused pane's cursor (1,0) -> global (1,0) -> CUP "\x1b[1;2H".
        assert_eq!(s, "\x1b[2J\x1b[Hab \r\ncd \x1b[1;2H");
    }

    #[test]
    fn render_two_panes_shows_divider_between_them() {
        // Vertical split of a width-7 viewport: left pane (1), right pane (2).
        let tree = Node::Split {
            dir: SplitDir::Vertical,
            ratio: 0.5,
            first: Box::new(Node::Leaf(1)),
            second: Box::new(Node::Leaf(2)),
        };
        let left = StubScreen::new(3, 1, &["LLL"], (0, 0));
        let right = StubScreen::new(3, 1, &["RRR"], (0, 0));
        let mut c = Compositor::new();
        let vp = Rect { x: 0, y: 0, w: 7, h: 1 };
        // window 1 focused -> divider on its right edge is heavy.
        let out = c.render(vp, &tree, &["w".to_string()], 0, 1, &[(1, &left), (2, &right)]);
        let s = String::from_utf8(out).unwrap();
        // avail=6, first_w=3 (x0..2), divider x3, right x4..6.
        assert!(s.contains("LLL┃RRR")); // heavy divider because pane 1 is focused
    }

    #[test]
    fn render_includes_tab_bar_only_when_multiple_windows() {
        let tree = Node::Leaf(1);
        let pane = StubScreen::new(20, 1, &["hello"], (0, 0));
        let mut c = Compositor::new();
        let vp = Rect { x: 0, y: 0, w: 20, h: 2 };
        // Two windows -> the bottom row is a tab bar.
        let names = vec!["one".to_string(), "two".to_string()];
        let out = c.render(vp, &tree, &names, 1, 1, &[(1, &pane)]);
        let s = String::from_utf8(out).unwrap();
        assert!(s.contains("[0:one]"));
        assert!(s.contains("[1*two]")); // active window 1 marked with '*'

        // With a single window, no tab bar is drawn (no labels in the frame).
        let single = c.render(vp, &tree, &["one".to_string()], 0, 1, &[(1, &pane)]);
        let s2 = String::from_utf8(single).unwrap();
        assert!(!s2.contains("[0:one]") && !s2.contains("[0*one]"));
    }

    #[test]
    fn render_places_cursor_at_focused_pane_in_global_coords() {
        // Two stacked panes; focus the bottom one; its cursor maps to global rows.
        let tree = Node::Split {
            dir: SplitDir::Horizontal,
            ratio: 0.5,
            first: Box::new(Node::Leaf(1)),
            second: Box::new(Node::Leaf(2)),
        };
        let top = StubScreen::new(4, 1, &["topp"], (0, 0));
        let bottom = StubScreen::new(4, 1, &["bott"], (2, 0)); // cursor col 2
        let mut c = Compositor::new();
        let vp = Rect { x: 0, y: 0, w: 4, h: 3 };
        // h=3: avail=2, first_h=1 (y0), divider y1, second y2. Focus pane 2 (bottom).
        let out = c.render(vp, &tree, &["w".to_string()], 0, 2, &[(1, &top), (2, &bottom)]);
        let s = String::from_utf8(out).unwrap();
        // Bottom pane rect is {x0,y2}; its cursor (2,0) -> global (2,2) -> "\x1b[3;3H".
        assert!(s.ends_with("\x1b[3;3H"));
    }
}
