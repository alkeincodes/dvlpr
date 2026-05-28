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
    /// The character and its style at (x, y); returns a default-styled `' '` for
    /// out-of-range coordinates. The default impl pairs `cell` with a default style,
    /// so style-less stubs need not implement it; real screens override it to carry
    /// color/attributes.
    fn styled_cell(&self, x: u16, y: u16) -> (char, CellStyle) {
        (self.cell(x, y), CellStyle::default())
    }
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
    fn styled_cell(&self, x: u16, y: u16) -> (char, CellStyle) {
        self.styled_cell(x, y)
    }
    fn cursor(&self) -> (u16, u16) {
        self.cursor()
    }
}

/// A single cell's color. `Default` means "unset" — the client renders it with its
/// own terminal theme's default fg/bg, rather than us forcing a concrete color.
/// Palette and Rgb are kept distinct so we emit `38;5;n` vs `38;2;r;g;b` and let the
/// user's terminal apply its own palette/theme to indexed colors.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum Color {
    #[default]
    Default,
    Palette(u8),
    Rgb(u8, u8, u8),
}

/// The visual style of one cell: fg/bg color plus text-decoration flags. Mirrors the
/// subset of libghostty-vt's cell style we re-emit as SGR.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct CellStyle {
    pub fg: Color,
    pub bg: Color,
    pub bold: bool,
    pub italic: bool,
    pub faint: bool,
    pub underline: bool,
    pub blink: bool,
    pub inverse: bool,
    pub strikethrough: bool,
}

/// One composed cell: its character plus how it should be styled.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct StyledCell {
    pub ch: char,
    pub style: CellStyle,
}

impl Default for StyledCell {
    fn default() -> Self {
        StyledCell {
            ch: ' ',
            style: CellStyle::default(),
        }
    }
}

/// A composed full-viewport snapshot: the styled cell grid plus the global cursor
/// position.
#[derive(Clone)]
pub struct Grid {
    pub cols: u16,
    pub rows: u16,
    pub cells: Vec<StyledCell>,
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
        theme: &crate::theme::Theme,
        panes: &[(PaneId, &dyn PaneCells)],
    ) -> Grid {
        debug_assert!(
            viewport.x == 0 && viewport.y == 0,
            "compose expects a viewport rooted at (0, 0)"
        );
        let cols = viewport.w;
        let rows = viewport.h;
        let mut buf = vec![StyledCell::default(); cols as usize * rows as usize];

        let window_count = tab_names.len();
        let content = layout::content_area(viewport, window_count);
        let rects = if zoomed {
            vec![(focused, content)]
        } else {
            layout::pane_rects(root, content)
        };

        // Panes.
        for (id, rect) in &rects {
            if let Some(p) = lookup(panes, *id) {
                blit_pane(&mut buf, cols, *rect, p);
            }
        }

        // Dividers (skipped entirely while zoomed — only one pane is shown).
        let focused_rect = rects.iter().find(|(id, _)| *id == focused).map(|(_, r)| *r);
        if !zoomed {
            for d in layout::dividers(root, content) {
                draw_divider(&mut buf, cols, &d, focused_rect, theme);
            }
        }

        // Status/tab bar (always present): session prefix at the left, then tabs.
        if let Some(ty) = layout::tab_row(viewport, window_count) {
            let regions = layout::tab_layout(session_name, tab_names, active_window, zoomed, cols);
            draw_tabs(&mut buf, cols, ty, session_name, &regions, active_window, theme);
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
        theme: &crate::theme::Theme,
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
            theme,
            panes,
        ))
    }
}

/// Append a full SGR sequence that *resets then applies* `style`, so it is correct
/// regardless of the previous pen (a default style emits a bare `\x1b[0m`). Building
/// every change off a reset keeps the serializers stateless beyond a single "pen"
/// comparison and avoids tracking which attributes need clearing.
fn push_sgr(out: &mut Vec<u8>, style: &CellStyle) {
    out.extend_from_slice(b"\x1b[0");
    if style.bold {
        out.extend_from_slice(b";1");
    }
    if style.faint {
        out.extend_from_slice(b";2");
    }
    if style.italic {
        out.extend_from_slice(b";3");
    }
    if style.underline {
        out.extend_from_slice(b";4");
    }
    if style.blink {
        out.extend_from_slice(b";5");
    }
    if style.inverse {
        out.extend_from_slice(b";7");
    }
    if style.strikethrough {
        out.extend_from_slice(b";9");
    }
    push_color(out, style.fg, true);
    push_color(out, style.bg, false);
    out.push(b'm');
}

/// Append the color portion of an SGR sequence. `Default` emits nothing (the leading
/// reset in `push_sgr` already restored the terminal default).
fn push_color(out: &mut Vec<u8>, color: Color, fg: bool) {
    let base = if fg { 38 } else { 48 };
    match color {
        Color::Default => {}
        Color::Palette(n) => out.extend_from_slice(format!(";{base};5;{n}").as_bytes()),
        Color::Rgb(r, g, b) => out.extend_from_slice(format!(";{base};2;{r};{g};{b}").as_bytes()),
    }
}

/// Serialize a full frame: clear+home, a reset to establish a known default pen, then
/// every row joined by CRLF with minimal SGR transitions, a trailing reset if the pen
/// ended non-default, and finally a cursor CUP. For all-default content this adds only
/// the single leading `\x1b[0m` over the old char-only format.
pub fn serialize_full(grid: &Grid) -> Vec<u8> {
    debug_assert_eq!(
        grid.cells.len(),
        grid.cols as usize * grid.rows as usize,
        "Grid::cells length must equal cols*rows"
    );
    let cols = grid.cols;
    let rows = grid.rows;
    let mut out = Vec::with_capacity(grid.cells.len() * 2 + rows as usize * 2 + 16);
    // Clear+home, then reset SGR so the frame paints from a known default pen
    // regardless of any leftover style on the client.
    out.extend_from_slice(b"\x1b[2J\x1b[H\x1b[0m");
    let mut pen = CellStyle::default();
    let mut tmp = [0u8; 4];
    for y in 0..rows {
        if y > 0 {
            out.extend_from_slice(b"\r\n");
        }
        for x in 0..cols {
            let cell = &grid.cells[y as usize * cols as usize + x as usize];
            if cell.style != pen {
                push_sgr(&mut out, &cell.style);
                pen = cell.style;
            }
            out.extend_from_slice(cell.ch.encode_utf8(&mut tmp).as_bytes());
        }
    }
    // Leave the client's pen at default so the next frame (esp. a diff) can assume it.
    if pen != CellStyle::default() {
        out.extend_from_slice(b"\x1b[0m");
    }
    let (gx, gy) = grid.cursor;
    out.extend_from_slice(format!("\x1b[{};{}H", gy + 1, gx + 1).as_bytes());
    out
}

/// Serialize a per-row diff: for each row whose cells changed (character OR style), a
/// CUP to that row's start followed by the whole row, with SGR emitted only when a
/// cell's style differs from the running pen. The pen is tracked continuously across
/// redrawn rows (SGR persists across cursor jumps in the terminal, and unredrawn rows
/// keep their already-rendered attributes); it starts at default because every frame
/// leaves the client's pen at default. A trailing reset restores that invariant if any
/// styled cell was emitted; then a cursor CUP if any row was redrawn or the cursor
/// moved. Empty when nothing changed. For all-default content this is byte-identical to
/// the old char-only diff. `prev` and `next` MUST have the same dimensions.
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
    let mut pen = CellStyle::default();
    for y in 0..next.rows as usize {
        let lo = y * cols;
        let hi = lo + cols;
        if prev.cells[lo..hi] != next.cells[lo..hi] {
            out.extend_from_slice(format!("\x1b[{};1H", y + 1).as_bytes());
            for cell in &next.cells[lo..hi] {
                if cell.style != pen {
                    push_sgr(&mut out, &cell.style);
                    pen = cell.style;
                }
                out.extend_from_slice(cell.ch.encode_utf8(&mut tmp).as_bytes());
            }
        }
    }
    // Restore the default-pen invariant for the next frame (only reachable when a row
    // was redrawn, so `out` is already non-empty).
    if pen != CellStyle::default() {
        out.extend_from_slice(b"\x1b[0m");
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

    let mut cells = vec![StyledCell::default(); cols as usize * rows as usize];
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

/// Copy a pane's styled cells into `buf` (a `cols`-wide grid) at `rect`'s offset.
fn blit_pane(buf: &mut [StyledCell], cols: u16, rect: Rect, pane: &dyn PaneCells) {
    for y in 0..rect.h {
        for x in 0..rect.w {
            let bx = rect.x + x;
            let by = rect.y + y;
            let idx = by as usize * cols as usize + bx as usize;
            if idx < buf.len() {
                // styled_cell returns a default-styled ' ' for out-of-range.
                let (ch, style) = pane.styled_cell(x, y);
                buf[idx] = StyledCell { ch, style };
            }
        }
    }
}

/// Fill a divider's cells with a box-drawing glyph. Each cell is heavy iff it
/// is immediately adjacent to the focused pane along the perpendicular axis —
/// so a long divider that runs past several stacked panes only goes heavy on
/// the segment that borders the focused one, and stays light elsewhere.
/// Junctions where dividers cross are simple last-write-wins overwrites
/// (proper `┼` junctions are deferred polish).
fn draw_divider(
    buf: &mut [StyledCell],
    cols: u16,
    d: &layout::Divider,
    focused: Option<Rect>,
    theme: &crate::theme::Theme,
) {
    let heavy_style = CellStyle {
        fg: theme.active_tab_bg,
        ..CellStyle::default()
    };
    for y in d.rect.y..d.rect.y + d.rect.h {
        for x in d.rect.x..d.rect.x + d.rect.w {
            let heavy = focused.is_some_and(|fr| cell_borders_focused(d.dir, x, y, fr));
            let glyph = match (d.dir, heavy) {
                (SplitDir::Vertical, false) => '│',
                (SplitDir::Vertical, true) => '┃',
                (SplitDir::Horizontal, false) => '─',
                (SplitDir::Horizontal, true) => '━',
            };
            let style = if heavy { heavy_style } else { CellStyle::default() };
            let idx = y as usize * cols as usize + x as usize;
            if idx < buf.len() {
                // Chrome (dividers) overwrites any pane style underneath.
                buf[idx] = StyledCell { ch: glyph, style };
            }
        }
    }
}

/// True if a single divider cell at `(x, y)` along axis `dir` lies on an edge
/// of the focused rect. For a vertical divider, the cell is heavy when its
/// column is the focused rect's left or right edge AND its row falls within
/// the focused rect's vertical span. Horizontal is the dual.
fn cell_borders_focused(dir: SplitDir, x: u16, y: u16, focused: Rect) -> bool {
    match dir {
        SplitDir::Vertical => {
            let col_adjacent = x == focused.x + focused.w || x + 1 == focused.x;
            let row_inside = focused.y <= y && y < focused.y + focused.h;
            col_adjacent && row_inside
        }
        SplitDir::Horizontal => {
            let row_adjacent = y == focused.y + focused.h || y + 1 == focused.y;
            let col_inside = focused.x <= x && x < focused.x + focused.w;
            row_adjacent && col_inside
        }
    }
}

/// Draw the session-name prefix at the row's left, then each tab region's label at
/// its `x_start` (the same x-ranges hit-testing uses). The prefix is not clickable.
fn draw_tabs(
    buf: &mut [StyledCell],
    cols: u16,
    ty: u16,
    session_name: &str,
    regions: &[layout::TabRegion],
    active_window: usize,
    theme: &crate::theme::Theme,
) {
    // Step 1 — row fill: paint the whole bar row with bar_bg so gaps and the
    // right edge carry the bar color. Subsequent segment writes overwrite the
    // specific cells they occupy.
    let bar_style = CellStyle {
        bg: theme.bar_bg,
        fg: theme.inactive_tab_fg,
        ..CellStyle::default()
    };
    for x in 0..cols as usize {
        let idx = ty as usize * cols as usize + x;
        if idx < buf.len() {
            buf[idx] = StyledCell { ch: ' ', style: bar_style };
        }
    }

    // Step 2 — session prefix segment (mauve / contrast fg).
    let session_style = CellStyle {
        bg: theme.session_bg,
        fg: theme.session_fg,
        ..CellStyle::default()
    };
    for (x, ch) in session_name.chars().enumerate() {
        if x >= cols as usize {
            break;
        }
        let idx = ty as usize * cols as usize + x;
        if idx < buf.len() {
            buf[idx] = StyledCell { ch, style: session_style };
        }
    }
    // (The two-space gap after the session prefix is left untouched — it keeps
    // the bar_bg from the row-fill step.)

    // Step 3 — tab segments. Active tab (region.window == active_window) gets
    // peach bg + bold; inactive tabs get surface0 bg. The single-space gaps
    // between tabs are not overwritten and retain bar_bg from the row-fill.
    for region in regions.iter() {
        let style = if region.window == active_window {
            CellStyle {
                bg: theme.active_tab_bg,
                fg: theme.active_tab_fg,
                bold: theme.active_tab_bold,
                ..CellStyle::default()
            }
        } else {
            CellStyle {
                bg: theme.inactive_tab_bg,
                fg: theme.inactive_tab_fg,
                ..CellStyle::default()
            }
        };
        for (j, ch) in region.label.chars().enumerate() {
            let x = region.x_start as usize + j;
            if x >= cols as usize {
                break;
            }
            let idx = ty as usize * cols as usize + x;
            if idx < buf.len() {
                buf[idx] = StyledCell { ch, style };
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

    /// A default-styled cell, for terse grid literals in tests.
    fn sc(ch: char) -> StyledCell {
        StyledCell {
            ch,
            style: CellStyle::default(),
        }
    }

    /// A row of default-styled cells from a `&str`.
    fn scs(s: &str) -> Vec<StyledCell> {
        s.chars().map(sc).collect()
    }

    /// Strip all ANSI CSI escape sequences from `s`, leaving only printable
    /// characters. A CSI sequence is `ESC [` followed by any number of parameter
    /// bytes (0x30-0x3F) and intermediate bytes (0x20-0x2F), terminated by a final
    /// byte in the range 0x40-0x7E. Used in tests that assert structural content
    /// of rendered frames without being coupled to exact SGR byte sequences.
    fn strip_csi(s: &str) -> String {
        let bytes = s.as_bytes();
        let mut out = String::with_capacity(s.len());
        let mut i = 0;
        while i < bytes.len() {
            if bytes[i] == 0x1b && i + 1 < bytes.len() && bytes[i + 1] == b'[' {
                // CSI sequence: skip until we hit a byte in 0x40–0x7E (the final byte).
                i += 2;
                while i < bytes.len() && !(0x40..=0x7E).contains(&bytes[i]) {
                    i += 1;
                }
                i += 1; // consume final byte
            } else {
                out.push(s[i..].chars().next().unwrap());
                i += s[i..].chars().next().unwrap().len_utf8();
            }
        }
        out
    }

    #[test]
    fn compositor_constructs() {
        let _c = Compositor::new();
    }

    #[test]
    fn blit_copies_cells_at_offset() {
        // 4x3 viewport buffer, blit a 2x2 pane "ab"/"cd" at offset (1,1).
        let cols: u16 = 4;
        let mut buf = vec![StyledCell::default(); 4 * 3];
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
        assert_eq!(buf[at(1, 1)].ch, 'a');
        assert_eq!(buf[at(1, 2)].ch, 'b');
        assert_eq!(buf[at(2, 1)].ch, 'c');
        assert_eq!(buf[at(2, 2)].ch, 'd');
        assert_eq!(buf[at(0, 0)].ch, ' '); // untouched cell stays blank
    }

    use crate::layout::Divider;

    #[test]
    fn draw_divider_fills_with_light_or_heavy_glyph() {
        let cols: u16 = 5;
        let mut buf = vec![StyledCell::default(); 5 * 2];
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
        // Theme with a sentinel active accent so we can recognise it in the buffer.
        let theme = crate::theme::Theme {
            active_tab_bg: Color::Rgb(99, 99, 99),
            ..crate::theme::Theme::default()
        };

        // No focused rect: every cell is light (default glyph + default style).
        draw_divider(&mut buf, cols, &d, None, &theme);
        assert_eq!(buf[at(0, 2)].ch, '│');
        assert_eq!(buf[at(1, 2)].ch, '│');
        assert_eq!(buf[at(0, 2)].style, CellStyle::default());
        assert_eq!(buf[at(1, 2)].style, CellStyle::default());

        // Focused rect immediately to the right of the divider, spanning both
        // rows: every cell of the divider is heavy with the active accent fg.
        let focused = Rect { x: 3, y: 0, w: 2, h: 2 };
        draw_divider(&mut buf, cols, &d, Some(focused), &theme);
        assert_eq!(buf[at(0, 2)].ch, '┃');
        assert_eq!(buf[at(1, 2)].ch, '┃');
        assert_eq!(buf[at(0, 2)].style.fg, Color::Rgb(99, 99, 99));
        assert_eq!(buf[at(1, 2)].style.fg, Color::Rgb(99, 99, 99));
        assert_eq!(buf[at(0, 2)].style.bg, Color::Default, "heavy divider has no bg");
        assert!(!buf[at(0, 2)].style.bold, "heavy divider is not bold");
    }

    #[test]
    fn draw_divider_only_heavy_in_focused_segment() {
        // Regression: a vertical divider that spans three stacked panes used to
        // go fully heavy whenever any of those panes was focused, because the
        // adjacency check was per-divider, not per-cell. With the focused pane
        // being the middle slab, only the middle three rows of the divider
        // should be heavy; the rows adjacent to the top and bottom slabs stay
        // light.
        let cols: u16 = 5;
        let mut buf = vec![StyledCell::default(); 5 * 9];
        // Vertical divider 1 col wide x 9 tall at x=2. Three slabs to its left:
        // top y=0..3, middle y=3..6, bottom y=6..9. Focus = middle slab.
        let d = Divider {
            rect: Rect { x: 2, y: 0, w: 1, h: 9 },
            path: vec![],
            dir: SplitDir::Vertical,
        };
        let focused_middle = Rect { x: 0, y: 3, w: 2, h: 3 };
        let theme = crate::theme::Theme {
            active_tab_bg: Color::Rgb(99, 99, 99),
            ..crate::theme::Theme::default()
        };
        let at = |r: usize, c: usize| r * cols as usize + c;

        draw_divider(&mut buf, cols, &d, Some(focused_middle), &theme);

        for y in 0..3 {
            assert_eq!(buf[at(y, 2)].ch, '│', "y={y} (top slab) should be light");
            assert_eq!(buf[at(y, 2)].style, CellStyle::default(), "y={y} default style");
        }
        for y in 3..6 {
            assert_eq!(buf[at(y, 2)].ch, '┃', "y={y} (focused middle) should be heavy");
            assert_eq!(buf[at(y, 2)].style.fg, Color::Rgb(99, 99, 99), "y={y} carries accent");
        }
        for y in 6..9 {
            assert_eq!(buf[at(y, 2)].ch, '│', "y={y} (bottom slab) should be light");
            assert_eq!(buf[at(y, 2)].style, CellStyle::default(), "y={y} default style");
        }
    }

    #[test]
    #[allow(clippy::needless_range_loop)]
    fn draw_tabs_paints_themed_blocks() {
        // 30-column bar row. Three windows: active = index 1 ("vim"). Sentinel
        // colors per role so the assertions are unambiguous.
        let cols: u16 = 30;
        let mut buf = vec![StyledCell::default(); cols as usize];
        let theme = crate::theme::Theme {
            bar_bg: Color::Rgb(1, 1, 1),
            session_fg: Color::Rgb(2, 2, 2),
            session_bg: Color::Rgb(3, 3, 3),
            active_tab_fg: Color::Rgb(4, 4, 4),
            active_tab_bg: Color::Rgb(5, 5, 5),
            active_tab_bold: true,
            inactive_tab_fg: Color::Rgb(6, 6, 6),
            inactive_tab_bg: Color::Rgb(7, 7, 7),
        };
        let names = vec!["zsh".to_string(), "vim".to_string(), "git".to_string()];
        let active_window = 1;
        let regions = layout::tab_layout("work", &names, active_window, false, cols);
        draw_tabs(&mut buf, cols, 0, "work", &regions, active_window, &theme);

        // Session prefix "work" occupies x=0..=3.
        for x in 0..4 {
            assert_eq!(buf[x].style.bg, theme.session_bg, "session bg at x={x}");
            assert_eq!(buf[x].style.fg, theme.session_fg, "session fg at x={x}");
        }
        // Two-space gap (x=4..=5) retains bar_bg and inactive_tab_fg from
        // the row-fill step (fg is set so future overlay characters in gap
        // cells have a sensible color rather than the terminal default).
        for x in 4..6 {
            assert_eq!(buf[x].style.bg, theme.bar_bg, "gap bg at x={x}");
            assert_eq!(buf[x].style.fg, theme.inactive_tab_fg, "gap fg at x={x}");
        }
        // Active tab (window 1, "2:vim*") starts at the second region's x_start.
        let active_region = &regions[1];
        for x in active_region.x_start..=active_region.x_end {
            let i = x as usize;
            assert_eq!(buf[i].style.bg, theme.active_tab_bg, "active bg at x={x}");
            assert_eq!(buf[i].style.fg, theme.active_tab_fg, "active fg at x={x}");
            assert!(buf[i].style.bold, "active is bold at x={x}");
        }
        // Inactive tabs (regions 0 and 2) carry the inactive bg/fg and are NOT bold.
        for r in [&regions[0], &regions[2]] {
            for x in r.x_start..=r.x_end {
                let i = x as usize;
                assert_eq!(buf[i].style.bg, theme.inactive_tab_bg, "inactive bg at x={x}");
                assert_eq!(buf[i].style.fg, theme.inactive_tab_fg, "inactive fg at x={x}");
                assert!(!buf[i].style.bold, "inactive is not bold at x={x}");
            }
        }
        // The right-edge fill: every cell beyond the last region carries bar_bg.
        let last = regions.last().unwrap().x_end;
        for x in (last + 1)..cols {
            let i = x as usize;
            assert_eq!(buf[i].style.bg, theme.bar_bg, "right-edge fill at x={x}");
            assert_eq!(buf[i].style.fg, theme.inactive_tab_fg, "right-edge fg at x={x}");
        }
    }

    #[test]
    fn draw_tabs_writes_labels_at_their_ranges() {
        let cols: u16 = 20;
        let mut buf = vec![StyledCell::default(); 20 * 2];
        let regions = layout::tab_layout("s", &["a".to_string(), "b".to_string()], 0, false, cols);
        // Tab row is the last row (y = 1).
        draw_tabs(&mut buf, cols, 1, "s", &regions, 0, &crate::theme::Theme::default());
        // Row 1 is the tab row: its flat offset is 1*cols == cols.
        let row: String = (0..cols)
            .map(|x| buf[cols as usize + x as usize].ch)
            .collect();
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
            &crate::theme::Theme::default(),
            &[(1, &pane)],
        );
        let s = String::from_utf8(out).unwrap();
        // Themed bar: the rendered bytes include SGR escapes for the row-fill
        // and the session prefix. Assert structure rather than exact bytes so
        // palette tweaks don't churn this test.
        assert!(s.starts_with("\x1b[2J\x1b[H\x1b[0m"), "frame must start with clear+home+reset, got: {s:?}");
        assert!(s.contains("ab "), "frame must contain the pane row content");
        let plain = strip_csi(&s);
        assert!(plain.contains("s  "), "frame must contain the session prefix + two-space gap, plain={plain:?}");
        assert!(s.ends_with("\x1b[1;2H"), "frame must end with the cursor CUP");
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
            &crate::theme::Theme::default(),
            &[(1, &left), (2, &right)],
        );
        let s = String::from_utf8(out).unwrap();
        // content h=1: avail=6, first_w=3 (x0..2), divider x3, right x4..6.
        // The heavy divider now carries an fg SGR escape between "LLL" and
        // "┃", so the contiguous "LLL┃RRR" substring no longer holds in the
        // raw bytes. Strip CSI escapes first, then the original ordered check
        // still works.
        let plain = strip_csi(&s);
        assert!(plain.contains("LLL┃RRR"), "ordered divider content present in stripped frame; got: {plain:?}");
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
        let out = c.render(vp, &tree, "s", &names, 1, 1, false, &crate::theme::Theme::default(), &[(1, &pane)]);
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
            &crate::theme::Theme::default(),
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
            cells: scs("abcdefgh"),
            cursor: (0, 0),
        };
        let next = Grid {
            cols: 4,
            rows: 2,
            cells: scs("abcdXYZW"),
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
            cells: scs("ab"),
            cursor: (0, 0),
        };
        assert!(diff_rows(&g, &g).is_empty());
    }

    #[test]
    fn diff_rows_cursor_only_move_emits_just_a_cup() {
        let prev = Grid {
            cols: 2,
            rows: 1,
            cells: scs("ab"),
            cursor: (0, 0),
        };
        let next = Grid {
            cols: 2,
            rows: 1,
            cells: scs("ab"),
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
            &crate::theme::Theme::default(),
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
            cells: scs("ab"),
            cursor: (0, 0),
        };
        let next = Grid {
            cols: 2,
            rows: 1,
            cells: scs("XY"),
            cursor: (0, 0),
        };
        let out = String::from_utf8(diff_rows(&prev, &next)).unwrap();
        assert_eq!(out, "\x1b[1;1HXY\x1b[1;1H");
    }

    #[test]
    fn serialize_full_emits_sgr_for_styled_cells() {
        // 2x1 grid: a bold red-fg 'A' followed by a default 'B'. The transition to 'B'
        // re-emits a bare reset; the frame already ends at default so no trailing reset.
        let red_bold = CellStyle {
            fg: Color::Rgb(255, 0, 0),
            bold: true,
            ..CellStyle::default()
        };
        let grid = Grid {
            cols: 2,
            rows: 1,
            cells: vec![
                StyledCell {
                    ch: 'A',
                    style: red_bold,
                },
                StyledCell {
                    ch: 'B',
                    style: CellStyle::default(),
                },
            ],
            cursor: (0, 0),
        };
        let s = String::from_utf8(serialize_full(&grid)).unwrap();
        assert_eq!(
            s,
            "\x1b[2J\x1b[H\x1b[0m\x1b[0;1;38;2;255;0;0mA\x1b[0mB\x1b[1;1H"
        );
    }

    #[test]
    fn diff_rows_resets_pen_after_a_styled_change() {
        // A cell gains a palette background: the diff emits the SGR, then a trailing reset
        // so the next frame can assume a default pen.
        let prev = Grid {
            cols: 1,
            rows: 1,
            cells: vec![StyledCell::default()],
            cursor: (0, 0),
        };
        let next = Grid {
            cols: 1,
            rows: 1,
            cells: vec![StyledCell {
                ch: ' ',
                style: CellStyle {
                    bg: Color::Palette(4),
                    ..CellStyle::default()
                },
            }],
            cursor: (0, 0),
        };
        let out = String::from_utf8(diff_rows(&prev, &next)).unwrap();
        assert_eq!(out, "\x1b[1;1H\x1b[0;48;5;4m \x1b[0m\x1b[1;1H");
    }

    fn grid(cols: u16, rows: u16, cells: &str, cursor: (u16, u16)) -> Grid {
        let cells = scs(cells);
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
        assert_eq!(
            f.cells.iter().map(|c| c.ch).collect::<String>(),
            "     ab     "
        );
        assert_eq!(f.cursor, (1, 1));
    }

    #[test]
    fn fit_clip_takes_top_left_and_clamps_cursor() {
        let g = grid(4, 3, "abcdefghijkl", (3, 2));
        let f = fit(&g, 2, 1);
        assert_eq!((f.cols, f.rows), (2, 1));
        assert_eq!(f.cells.iter().map(|c| c.ch).collect::<String>(), "ab");
        assert_eq!(f.cursor, (1, 0));
    }

    #[test]
    fn fit_mixed_axes_center_x_clip_y() {
        let g = grid(2, 3, "abcdef", (0, 2));
        let f = fit(&g, 4, 1);
        assert_eq!((f.cols, f.rows), (4, 1));
        assert_eq!(f.cells.iter().map(|c| c.ch).collect::<String>(), " ab ");
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
