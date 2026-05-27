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

/// A composed full-viewport snapshot: the cell grid plus the global cursor
/// position. Chars only (no color/attributes — matches the current cell model).
#[derive(Clone)]
pub struct Grid {
    pub cols: u16,
    pub rows: u16,
    pub cells: Vec<char>,
    /// Global cursor position as (col, row), already mapped into viewport space.
    pub cursor: (u16, u16),
}

impl Grid {
    pub fn dims(&self) -> (u16, u16) {
        (self.cols, self.rows)
    }
}

/// Composites frames. Stateless — kept as a struct so `Session` can own one.
#[derive(Default)]
pub struct Compositor;

impl Compositor {
    pub fn new() -> Self {
        Self
    }

    /// Composite the active window into a `Grid`. Contract: `viewport` must be
    /// rooted at (0, 0); `tab_names` has one entry per window; `focused` is the
    /// active window's focused pane.
    #[allow(clippy::too_many_arguments)]
    pub fn compose(
        &self,
        viewport: Rect,
        root: &Node,
        session_name: &str,
        tab_names: &[String],
        active_window: usize,
        focused: PaneId,
        zoomed: bool,
        panes: &[(PaneId, &dyn PaneCells)],
    ) -> Grid {
        debug_assert!(
            viewport.x == 0 && viewport.y == 0,
            "compose expects a viewport rooted at (0, 0)"
        );
        let cols = viewport.w;
        let rows = viewport.h;
        let mut buf = vec![' '; cols as usize * rows as usize];

        let window_count = tab_names.len();
        let content = layout::content_area(viewport, window_count);
        let rects = layout::pane_rects(root, content);

        // Panes.
        for (id, rect) in &rects {
            if let Some(p) = lookup(panes, *id) {
                blit_pane(&mut buf, cols, *rect, p);
            }
        }

        // Dividers (heavy on the focused pane's borders).
        let focused_rect = rects.iter().find(|(id, _)| *id == focused).map(|(_, r)| *r);
        for d in layout::dividers(root, content) {
            let heavy = focused_rect.is_some_and(|fr| divider_touches(&d, fr));
            draw_divider(&mut buf, cols, &d, heavy);
        }

        // Status/tab bar (always present): session prefix at the left, then tabs.
        if let Some(ty) = layout::tab_row(viewport, window_count) {
            let regions = layout::tab_layout(session_name, tab_names, active_window, zoomed, cols);
            draw_tabs(&mut buf, cols, ty, session_name, &regions);
        }

        // Cursor at the focused pane's cursor, mapped to global coordinates.
        let cursor = match focused_rect {
            Some(fr) => {
                let (cx, cy) = lookup(panes, focused).map_or((0, 0), |p| p.cursor());
                (
                    fr.x + cx.min(fr.w.saturating_sub(1)),
                    fr.y + cy.min(fr.h.saturating_sub(1)),
                )
            }
            None => (0, 0),
        };

        Grid {
            cols,
            rows,
            cells: buf,
            cursor,
        }
    }

    /// Convenience: a full-frame serialization of `compose`. Used by the compositor
    /// unit tests; `Session::render` delegates to `serialize_full` directly.
    #[allow(clippy::too_many_arguments)]
    pub fn render(
        &self,
        viewport: Rect,
        root: &Node,
        session_name: &str,
        tab_names: &[String],
        active_window: usize,
        focused: PaneId,
        zoomed: bool,
        panes: &[(PaneId, &dyn PaneCells)],
    ) -> Vec<u8> {
        serialize_full(&self.compose(
            viewport,
            root,
            session_name,
            tab_names,
            active_window,
            focused,
            zoomed,
            panes,
        ))
    }
}

/// Serialize a full frame: clear+home, every row joined by CRLF, then a cursor CUP.
/// Identical to the bytes the old `Compositor::render` produced.
pub fn serialize_full(grid: &Grid) -> Vec<u8> {
    debug_assert_eq!(
        grid.cells.len(),
        grid.cols as usize * grid.rows as usize,
        "Grid::cells length must equal cols*rows"
    );
    let cols = grid.cols;
    let rows = grid.rows;
    let mut out = Vec::with_capacity(grid.cells.len() * 2 + rows as usize * 2 + 16);
    out.extend_from_slice(b"\x1b[2J\x1b[H");
    let mut tmp = [0u8; 4];
    for y in 0..rows {
        if y > 0 {
            out.extend_from_slice(b"\r\n");
        }
        for x in 0..cols {
            let ch = grid.cells[y as usize * cols as usize + x as usize];
            out.extend_from_slice(ch.encode_utf8(&mut tmp).as_bytes());
        }
    }
    let (gx, gy) = grid.cursor;
    out.extend_from_slice(format!("\x1b[{};{}H", gy + 1, gx + 1).as_bytes());
    out
}

/// Serialize a per-row diff: for each row whose cells changed, a CUP to that row's
/// start followed by the whole row; then a trailing cursor CUP if any row was
/// redrawn (row writes move the cursor) or the cursor itself moved. Empty when
/// nothing changed. `prev` and `next` MUST have the same dimensions.
pub fn diff_rows(prev: &Grid, next: &Grid) -> Vec<u8> {
    debug_assert_eq!(prev.dims(), next.dims(), "diff_rows requires equal dims");
    debug_assert_eq!(
        next.cells.len(),
        next.cols as usize * next.rows as usize,
        "Grid::cells length must equal cols*rows"
    );
    let cols = next.cols as usize;
    let mut out = Vec::new();
    let mut tmp = [0u8; 4];
    for y in 0..next.rows as usize {
        let lo = y * cols;
        let hi = lo + cols;
        if prev.cells[lo..hi] != next.cells[lo..hi] {
            out.extend_from_slice(format!("\x1b[{};1H", y + 1).as_bytes());
            for &ch in &next.cells[lo..hi] {
                out.extend_from_slice(ch.encode_utf8(&mut tmp).as_bytes());
            }
        }
    }
    if !out.is_empty() || prev.cursor != next.cursor {
        let (gx, gy) = next.cursor;
        out.extend_from_slice(format!("\x1b[{};{}H", gy + 1, gx + 1).as_bytes());
    }
    out
}

/// Fit a source grid into a `cols x rows` viewport for one client. The transform is
/// computed independently per axis: on an axis where the view is larger, center with
/// blank margin (letterbox); on an axis where the view is smaller, take the top-left
/// region (clip); equal axes copy 1:1. The cursor follows the same per-axis rule.
/// `cols`/`rows` must be >= 1 — callers clamp client sizes at ingestion.
pub fn fit(src: &Grid, cols: u16, rows: u16) -> Grid {
    debug_assert!(cols >= 1 && rows >= 1, "fit requires nonzero dims");
    if (cols, rows) == src.dims() {
        return src.clone();
    }

    let offset = |view: u16, s: u16| -> u16 {
        if view > s {
            (view - s) / 2
        } else {
            0
        }
    };
    let off_x = offset(cols, src.cols);
    let off_y = offset(rows, src.rows);
    let copy_w = cols.min(src.cols);
    let copy_h = rows.min(src.rows);

    let mut cells = vec![' '; cols as usize * rows as usize];
    for j in 0..copy_h {
        for i in 0..copy_w {
            let src_idx = j as usize * src.cols as usize + i as usize;
            let dst_idx = (off_y + j) as usize * cols as usize + (off_x + i) as usize;
            cells[dst_idx] = src.cells[src_idx];
        }
    }

    let map = |c: u16, view: u16, s: u16, off: u16| -> u16 {
        if view > s {
            (c + off).min(view - 1)
        } else {
            c.min(view - 1)
        }
    };
    let cursor = (
        map(src.cursor.0, cols, src.cols, off_x),
        map(src.cursor.1, rows, src.rows, off_y),
    );

    let out = Grid {
        cols,
        rows,
        cells,
        cursor,
    };
    debug_assert_eq!(out.cells.len(), cols as usize * rows as usize);
    out
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
            let rows_overlap = d.rect.y < focused.y + focused.h && focused.y < d.rect.y + d.rect.h;
            col_adjacent && rows_overlap
        }
        SplitDir::Horizontal => {
            let dy = d.rect.y;
            let row_adjacent = dy == focused.y + focused.h || dy + 1 == focused.y;
            let cols_overlap = d.rect.x < focused.x + focused.w && focused.x < d.rect.x + d.rect.w;
            row_adjacent && cols_overlap
        }
    }
}

/// Draw the session-name prefix at the row's left, then each tab region's label at
/// its `x_start` (the same x-ranges hit-testing uses). The prefix is not clickable.
fn draw_tabs(
    buf: &mut [char],
    cols: u16,
    ty: u16,
    session_name: &str,
    regions: &[layout::TabRegion],
) {
    for (x, ch) in session_name.chars().enumerate() {
        if x >= cols as usize {
            break;
        }
        let idx = ty as usize * cols as usize + x;
        if idx < buf.len() {
            buf[idx] = ch;
        }
    }
    // (the two-space gap is left blank; regions' x_start already accounts for it)
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
        let _c = Compositor::new();
    }

    #[test]
    fn blit_copies_cells_at_offset() {
        // 4x3 viewport buffer, blit a 2x2 pane "ab"/"cd" at offset (1,1).
        let cols: u16 = 4;
        let mut buf = vec![' '; 4 * 3];
        let pane = StubScreen::new(2, 2, &["ab", "cd"], (0, 0));
        blit_pane(
            &mut buf,
            cols,
            Rect {
                x: 1,
                y: 1,
                w: 2,
                h: 2,
            },
            &pane,
        );
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
            rect: Rect {
                x: 2,
                y: 0,
                w: 1,
                h: 2,
            },
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
            rect: Rect {
                x: 5,
                y: 0,
                w: 1,
                h: 4,
            },
            path: vec![],
            dir: SplitDir::Vertical,
        };
        let left = Rect {
            x: 0,
            y: 0,
            w: 5,
            h: 4,
        };
        let right = Rect {
            x: 6,
            y: 0,
            w: 5,
            h: 4,
        };
        assert!(divider_touches(&div, left)); // divider is on left pane's right edge
        assert!(divider_touches(&div, right)); // divider is on right pane's left edge
        let elsewhere = Rect {
            x: 0,
            y: 10,
            w: 5,
            h: 4,
        };
        assert!(!divider_touches(&div, elsewhere)); // no row overlap
    }

    #[test]
    fn draw_tabs_writes_labels_at_their_ranges() {
        let cols: u16 = 20;
        let mut buf = vec![' '; 20 * 2];
        let regions = layout::tab_layout("s", &["a".to_string(), "b".to_string()], 0, false, cols);
        // Tab row is the last row (y = 1).
        draw_tabs(&mut buf, cols, 1, "s", &regions);
        // Row 1 is the tab row: its flat offset is 1*cols == cols.
        let row: String = (0..cols).map(|x| buf[cols as usize + x as usize]).collect();
        // prefix "s" at x=0, gap at x=1,2; tabs start at x=3 ("s" + 2 spaces = 3).
        assert!(row.starts_with("s")); // session prefix
        assert!(row.contains("1:a*")); // active window 0 → 1-based label with '*'
        assert!(row.contains("2:b")); // inactive window 1 → 1-based label without marker
    }

    #[test]
    fn render_single_pane_matches_full_frame_format() {
        // One window, one pane in a 3x2 viewport. The bottom row is the always-on
        // status bar, so the content area is 3x1. Row 0 gets "ab ", row 1 gets
        // the bar: session prefix "s" + two-space gap + "1:w0*", clipped to 3 cols
        // → "s  " (prefix + gap, tabs don't fit).
        let tree = Node::Leaf(1);
        let pane = StubScreen::new(3, 2, &["ab", "cd"], (1, 0));
        let c = Compositor::new();
        let vp = Rect {
            x: 0,
            y: 0,
            w: 3,
            h: 2,
        };
        let out = c.render(
            vp,
            &tree,
            "s",
            &["w0".to_string()],
            0,
            1,
            false,
            &[(1, &pane)],
        );
        let s = String::from_utf8(out).unwrap();
        // Content area is h=1; pane row 0 ("ab ") fills it.
        // Bar row (y=1): "s  " (prefix at x=0, two blank spaces for the gap).
        // Cursor: pane rect {x:0,y:0,w:3,h:1}, pane cursor (1,0) -> global (1,0) -> "\x1b[1;2H".
        assert_eq!(s, "\x1b[2J\x1b[Hab \r\ns  \x1b[1;2H");
    }

    #[test]
    fn render_two_panes_shows_divider_between_them() {
        // Vertical split of a width-7 viewport: left pane (1), right pane (2).
        // h=2: content area is h=1 (bar always occupies the bottom row).
        let tree = Node::Split {
            dir: SplitDir::Vertical,
            ratio: 0.5,
            first: Box::new(Node::Leaf(1)),
            second: Box::new(Node::Leaf(2)),
        };
        let left = StubScreen::new(3, 1, &["LLL"], (0, 0));
        let right = StubScreen::new(3, 1, &["RRR"], (0, 0));
        let c = Compositor::new();
        let vp = Rect {
            x: 0,
            y: 0,
            w: 7,
            h: 2,
        };
        // window 1 focused -> divider on its right edge is heavy.
        let out = c.render(
            vp,
            &tree,
            "s",
            &["w".to_string()],
            0,
            1,
            false,
            &[(1, &left), (2, &right)],
        );
        let s = String::from_utf8(out).unwrap();
        // content h=1: avail=6, first_w=3 (x0..2), divider x3, right x4..6.
        assert!(s.contains("LLL┃RRR")); // heavy divider because pane 1 is focused
    }

    #[test]
    fn render_includes_tab_bar_for_both_single_and_multiple_windows() {
        let tree = Node::Leaf(1);
        let pane = StubScreen::new(20, 1, &["hello"], (0, 0));
        let c = Compositor::new();
        let vp = Rect {
            x: 0,
            y: 0,
            w: 20,
            h: 2,
        };
        // Two windows -> the bottom row has session prefix + tab labels.
        let names = vec!["one".to_string(), "two".to_string()];
        let out = c.render(vp, &tree, "s", &names, 1, 1, false, &[(1, &pane)]);
        let s = String::from_utf8(out).unwrap();
        assert!(s.contains("1:one")); // inactive window 0 (1-based, no marker)
        assert!(s.contains("2:two*")); // active window 1 marked with '*'

        // With a single window, the status bar is still drawn (always-on).
        let single = c.render(
            vp,
            &tree,
            "s",
            &["one".to_string()],
            0,
            1,
            false,
            &[(1, &pane)],
        );
        let s2 = String::from_utf8(single).unwrap();
        assert!(s2.contains("1:one*")); // active single window (1-based, '*' marker)
    }

    #[test]
    fn diff_rows_emits_only_changed_rows_then_cursor() {
        // 4x2 grids: row 0 identical, row 1 differs. Cursor moves (0,0)->(1,1).
        let prev = Grid {
            cols: 4,
            rows: 2,
            cells: vec!['a', 'b', 'c', 'd', 'e', 'f', 'g', 'h'],
            cursor: (0, 0),
        };
        let next = Grid {
            cols: 4,
            rows: 2,
            cells: vec!['a', 'b', 'c', 'd', 'X', 'Y', 'Z', 'W'],
            cursor: (1, 1),
        };
        let out = String::from_utf8(diff_rows(&prev, &next)).unwrap();
        // No full-clear, row 1 redrawn (CUP to row 2 col 1), then cursor CUP to (2,2).
        assert!(!out.contains("\x1b[2J"));
        assert_eq!(out, "\x1b[2;1HXYZW\x1b[2;2H");
    }

    #[test]
    fn diff_rows_unchanged_grid_is_empty() {
        let g = Grid {
            cols: 2,
            rows: 1,
            cells: vec!['a', 'b'],
            cursor: (0, 0),
        };
        assert!(diff_rows(&g, &g).is_empty());
    }

    #[test]
    fn diff_rows_cursor_only_move_emits_just_a_cup() {
        let prev = Grid {
            cols: 2,
            rows: 1,
            cells: vec!['a', 'b'],
            cursor: (0, 0),
        };
        let next = Grid {
            cols: 2,
            rows: 1,
            cells: vec!['a', 'b'],
            cursor: (1, 0),
        };
        let out = String::from_utf8(diff_rows(&prev, &next)).unwrap();
        assert_eq!(out, "\x1b[1;2H");
    }

    #[test]
    fn render_places_cursor_at_focused_pane_in_global_coords() {
        // Two stacked panes; focus the bottom one; its cursor maps to global rows.
        // h=4: content area is h=3 (bar always occupies bottom row).
        let tree = Node::Split {
            dir: SplitDir::Horizontal,
            ratio: 0.5,
            first: Box::new(Node::Leaf(1)),
            second: Box::new(Node::Leaf(2)),
        };
        let top = StubScreen::new(4, 1, &["topp"], (0, 0));
        let bottom = StubScreen::new(4, 1, &["bott"], (2, 0)); // cursor col 2
        let c = Compositor::new();
        let vp = Rect {
            x: 0,
            y: 0,
            w: 4,
            h: 4,
        };
        // content h=3: avail=2, first_h=1 (y0), divider y1, second y2. Focus pane 2 (bottom).
        let out = c.render(
            vp,
            &tree,
            "s",
            &["w".to_string()],
            0,
            2,
            false,
            &[(1, &top), (2, &bottom)],
        );
        let s = String::from_utf8(out).unwrap();
        // Bottom pane rect is {x0,y2}; its cursor (2,0) -> global (2,2) -> "\x1b[3;3H".
        assert!(s.ends_with("\x1b[3;3H"));
    }

    #[test]
    fn diff_rows_dirty_row_unchanged_cursor_still_emits_cup() {
        let prev = Grid {
            cols: 2,
            rows: 1,
            cells: vec!['a', 'b'],
            cursor: (0, 0),
        };
        let next = Grid {
            cols: 2,
            rows: 1,
            cells: vec!['X', 'Y'],
            cursor: (0, 0),
        };
        let out = String::from_utf8(diff_rows(&prev, &next)).unwrap();
        assert_eq!(out, "\x1b[1;1HXY\x1b[1;1H");
    }

    fn grid(cols: u16, rows: u16, cells: &str, cursor: (u16, u16)) -> Grid {
        let cells: Vec<char> = cells.chars().collect();
        assert_eq!(
            cells.len(),
            cols as usize * rows as usize,
            "test grid size mismatch"
        );
        Grid {
            cols,
            rows,
            cells,
            cursor,
        }
    }

    #[test]
    fn fit_identity_returns_equal_grid() {
        let g = grid(3, 2, "abcdef", (1, 0));
        let f = fit(&g, 3, 2);
        assert_eq!((f.cols, f.rows), (3, 2));
        assert_eq!(f.cells, g.cells);
        assert_eq!(f.cursor, (1, 0));
    }

    #[test]
    fn fit_letterbox_centers_and_pads_and_shifts_cursor() {
        let g = grid(2, 1, "ab", (0, 0));
        let f = fit(&g, 4, 3);
        assert_eq!((f.cols, f.rows), (4, 3));
        assert_eq!(f.cells.iter().collect::<String>(), "     ab     ");
        assert_eq!(f.cursor, (1, 1));
    }

    #[test]
    fn fit_clip_takes_top_left_and_clamps_cursor() {
        let g = grid(4, 3, "abcdefghijkl", (3, 2));
        let f = fit(&g, 2, 1);
        assert_eq!((f.cols, f.rows), (2, 1));
        assert_eq!(f.cells.iter().collect::<String>(), "ab");
        assert_eq!(f.cursor, (1, 0));
    }

    #[test]
    fn fit_mixed_axes_center_x_clip_y() {
        let g = grid(2, 3, "abcdef", (0, 2));
        let f = fit(&g, 4, 1);
        assert_eq!((f.cols, f.rows), (4, 1));
        assert_eq!(f.cells.iter().collect::<String>(), " ab ");
        assert_eq!(f.cursor, (1, 0));
    }

    #[test]
    fn fit_output_cell_count_always_matches_dims() {
        let g = grid(5, 4, &"x".repeat(20), (2, 2));
        for (c, r) in [(1, 1), (5, 4), (10, 8), (3, 9), (7, 2)] {
            let f = fit(&g, c, r);
            assert_eq!(f.cells.len(), c as usize * r as usize);
            assert!(
                f.cursor.0 < c && f.cursor.1 < r,
                "cursor must stay in-range"
            );
        }
    }
}
