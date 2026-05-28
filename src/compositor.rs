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
        sidebar_visible: bool,
        agent_entries: &[crate::session::AgentEntry],
    ) -> Grid {
        debug_assert!(
            viewport.x == 0 && viewport.y == 0,
            "compose expects a viewport rooted at (0, 0)"
        );
        let cols = viewport.w;
        let rows = viewport.h;
        let mut buf = vec![StyledCell::default(); cols as usize * rows as usize];

        let regions = layout::compute_regions(viewport, sidebar_visible);
        let content = regions.content_area;
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
        {
            let ty = regions.tab_status_row;
            let tab_regions =
                layout::tab_layout(session_name, tab_names, active_window, zoomed, cols);
            draw_tabs(
                &mut buf,
                cols,
                ty,
                session_name,
                &tab_regions,
                active_window,
                theme,
            );
        }

        // Sidebar (if visible).
        if let Some(sb) = regions.sidebar {
            draw_sidebar(&mut buf, cols, sb, theme, agent_entries);
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
            false,
            &[],
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
            let style = if heavy {
                heavy_style
            } else {
                CellStyle::default()
            };
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

/// Draw the session-name prefix at the row's left, then each tab region's label
/// at its `x_start`. Backgrounds are only painted when the corresponding theme
/// role is non-default — so when `bar_bg` / `session_bg` / `inactive_tab_bg`
/// are `Color::Default` the corresponding cells stay default-styled and the
/// host terminal background shows through. The active tab paints its full
/// `x_start..=x_end` range (including the 1-cell pads tab_layout reserves).
fn draw_tabs(
    buf: &mut [StyledCell],
    cols: u16,
    ty: u16,
    session_name: &str,
    regions: &[layout::TabRegion],
    active_window: usize,
    theme: &crate::theme::Theme,
) {
    // Step 1 — row fill: only paint if bar_bg is opaque. When it's Color::Default
    // (the shipping flavors), cells stay as StyledCell::default() and the host
    // terminal bg shows through.
    if theme.bar_bg != Color::Default {
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
    }

    // Step 2 — session prefix. fg always applied; bg only if session_bg != Default.
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

    // Step 3 — tabs.
    for region in regions.iter() {
        let is_active = region.window == active_window;
        if is_active {
            let style = CellStyle {
                bg: theme.active_tab_bg,
                fg: theme.active_tab_fg,
                bold: theme.active_tab_bold,
                ..CellStyle::default()
            };
            // Paint the full chip range (pad cells get a space; label chars
            // overwrite the middle cells).
            for x in region.x_start..=region.x_end {
                let idx = ty as usize * cols as usize + x as usize;
                if idx < buf.len() {
                    buf[idx] = StyledCell { ch: ' ', style };
                }
            }
            // Write label characters starting at x_start + ACTIVE_PAD_X (= 1).
            // Bound by x_end to honor narrow-terminal clipping: never write
            // past the chip's right edge.
            let label_x = region.x_start.saturating_add(1);
            for (j, ch) in region.label.chars().enumerate() {
                let x = label_x.saturating_add(j as u16);
                if x > region.x_end {
                    break;
                }
                let idx = ty as usize * cols as usize + x as usize;
                if idx < buf.len() {
                    buf[idx] = StyledCell { ch, style };
                }
            }
        } else {
            let style = CellStyle {
                bg: theme.inactive_tab_bg,
                fg: theme.inactive_tab_fg,
                ..CellStyle::default()
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
}

fn state_color(theme: &crate::theme::Theme, state: crate::detect::AgentState) -> Color {
    match state {
        crate::detect::AgentState::Idle => theme.agent_idle_fg,
        crate::detect::AgentState::Working => theme.agent_working_fg,
        crate::detect::AgentState::Blocked => theme.agent_blocked_fg,
    }
}

/// Fill the sidebar region with the AGENTS header, a separator, and
/// one row per agent entry. Layout per the spec:
///   column 0 (rect.x)        — vertical '│' separator (entire height)
///   columns 1..15            — 15-cell content area
///   row 0 of sidebar         — "AGENTS" centered in the content area
///   row 1 of sidebar         — horizontal '─' rule across the 15 cols
///   row 2..N                 — entries: " W<n>:claude  ●"
///                              (n is 1-based for display)
/// Empty state: rows 2 and 3 read "(no claude" / " panes)" in
/// theme.inactive_tab_fg (theme-consistent; not visually muted).
fn draw_sidebar(
    buf: &mut [StyledCell],
    cols: u16,
    rect: layout::Rect,
    theme: &crate::theme::Theme,
    entries: &[crate::session::AgentEntry],
) {
    let sep_style = CellStyle::default();
    let header_style = CellStyle::default();
    let entry_style = CellStyle::default();
    let placeholder_style = CellStyle {
        fg: theme.inactive_tab_fg,
        ..CellStyle::default()
    };

    for y in rect.y..rect.y + rect.h {
        let idx = (y as usize) * (cols as usize) + (rect.x as usize);
        if idx < buf.len() {
            buf[idx] = StyledCell {
                ch: '│',
                style: sep_style,
            };
        }
    }

    let write_into_row = |buf: &mut [StyledCell], y: u16, text: &str, style: CellStyle| {
        let start_col = rect.x + 1;
        let end_col = rect.x + SIDEBAR_LABEL_COLS_END;
        let mut col = start_col;
        for ch in text.chars() {
            if col >= end_col {
                break;
            }
            let idx = (y as usize) * (cols as usize) + (col as usize);
            if idx < buf.len() {
                buf[idx] = StyledCell { ch, style };
            }
            col += 1;
        }
    };

    if rect.h >= 1 {
        write_into_row(buf, rect.y, "    AGENTS     ", header_style);
    }

    if rect.h >= 2 {
        write_into_row(buf, rect.y + 1, "───────────────", header_style);
    }

    if entries.is_empty() {
        if rect.h >= 3 {
            write_into_row(buf, rect.y + 2, " (no claude   ", placeholder_style);
        }
        if rect.h >= 4 {
            write_into_row(buf, rect.y + 3, "  panes)      ", placeholder_style);
        }
        return;
    }

    let max_entries = if rect.h >= 2 {
        (rect.h - 2) as usize
    } else {
        0
    };
    for (i, entry) in entries.iter().take(max_entries).enumerate() {
        let y = rect.y + 2 + i as u16;
        let label = format!(" W{}:claude", entry.window_index + 1);
        write_into_row(buf, y, &label, entry_style);
        let dot_col = rect.x + 13;
        let dot_idx = (y as usize) * (cols as usize) + (dot_col as usize);
        if dot_idx < buf.len() {
            buf[dot_idx] = StyledCell {
                ch: '●',
                style: CellStyle {
                    fg: state_color(theme, entry.state),
                    ..CellStyle::default()
                },
            };
        }
    }
}

const SIDEBAR_LABEL_COLS_END: u16 = 16;

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
        let focused = Rect {
            x: 3,
            y: 0,
            w: 2,
            h: 2,
        };
        draw_divider(&mut buf, cols, &d, Some(focused), &theme);
        assert_eq!(buf[at(0, 2)].ch, '┃');
        assert_eq!(buf[at(1, 2)].ch, '┃');
        assert_eq!(buf[at(0, 2)].style.fg, Color::Rgb(99, 99, 99));
        assert_eq!(buf[at(1, 2)].style.fg, Color::Rgb(99, 99, 99));
        assert_eq!(
            buf[at(0, 2)].style.bg,
            Color::Default,
            "heavy divider has no bg"
        );
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
            rect: Rect {
                x: 2,
                y: 0,
                w: 1,
                h: 9,
            },
            path: vec![],
            dir: SplitDir::Vertical,
        };
        let focused_middle = Rect {
            x: 0,
            y: 3,
            w: 2,
            h: 3,
        };
        let theme = crate::theme::Theme {
            active_tab_bg: Color::Rgb(99, 99, 99),
            ..crate::theme::Theme::default()
        };
        let at = |r: usize, c: usize| r * cols as usize + c;

        draw_divider(&mut buf, cols, &d, Some(focused_middle), &theme);

        for y in 0..3 {
            assert_eq!(buf[at(y, 2)].ch, '│', "y={y} (top slab) should be light");
            assert_eq!(
                buf[at(y, 2)].style,
                CellStyle::default(),
                "y={y} default style"
            );
        }
        for y in 3..6 {
            assert_eq!(
                buf[at(y, 2)].ch,
                '┃',
                "y={y} (focused middle) should be heavy"
            );
            assert_eq!(
                buf[at(y, 2)].style.fg,
                Color::Rgb(99, 99, 99),
                "y={y} carries accent"
            );
        }
        for y in 6..9 {
            assert_eq!(buf[at(y, 2)].ch, '│', "y={y} (bottom slab) should be light");
            assert_eq!(
                buf[at(y, 2)].style,
                CellStyle::default(),
                "y={y} default style"
            );
        }
    }

    #[test]
    #[allow(clippy::needless_range_loop)]
    fn draw_tabs_paints_themed_blocks() {
        // 30-column bar. Three windows; active = 1 ("vim*").
        // Sentinel colors so assertions are unambiguous.
        let cols: u16 = 30;
        let mut buf = vec![StyledCell::default(); cols as usize];
        let theme = crate::theme::Theme {
            bar_bg:          Color::Default,
            session_bg:      Color::Default,
            session_fg:      Color::Rgb(2, 2, 2),
            active_tab_fg:   Color::Rgb(4, 4, 4),
            active_tab_bg:   Color::Rgb(5, 5, 5),
            active_tab_bold: true,
            inactive_tab_bg: Color::Default,
            inactive_tab_fg: Color::Rgb(6, 6, 6),
            agent_idle_fg:    Color::Rgb(7, 7, 7),
            agent_working_fg: Color::Rgb(8, 8, 8),
            agent_blocked_fg: Color::Rgb(9, 9, 9),
        };
        let names = vec!["zsh".to_string(), "vim".to_string(), "git".to_string()];
        let active_window = 1;
        let regions = layout::tab_layout("work", &names, active_window, false, cols);
        draw_tabs(&mut buf, cols, 0, "work", &regions, active_window, &theme);

        // Active region (window 1, "2:vim*") spans the full chip range.
        let r1 = &regions[1];
        for x in r1.x_start..=r1.x_end {
            let cell = buf[x as usize];
            assert_eq!(cell.style.bg, theme.active_tab_bg, "active bg at x={x}");
            assert_eq!(cell.style.fg, theme.active_tab_fg, "active fg at x={x}");
            assert!(cell.style.bold, "active bold at x={x}");
        }
        // Pad cells specifically: left pad at x_start carries a space char.
        assert_eq!(buf[r1.x_start as usize].ch, ' ');
        assert_eq!(buf[r1.x_end as usize].ch, ' ');

        // Inactive regions have NO bg paint — cells carry inactive_tab_fg
        // but bg stays Color::Default.
        for r in regions.iter().filter(|r| r.window != active_window) {
            for x in r.x_start..=r.x_end {
                let cell = buf[x as usize];
                assert_eq!(cell.style.bg, Color::Default, "inactive bg at x={x}");
                assert_eq!(cell.style.fg, theme.inactive_tab_fg, "inactive fg at x={x}");
            }
        }

        // Inter-tab gaps and bar row beyond the last tab: cells stay default.
        let last = regions.last().unwrap().x_end;
        for x in (last + 1)..cols {
            assert_eq!(buf[x as usize], StyledCell::default(), "gap/right tail at x={x}");
        }
    }

    #[test]
    fn draw_tabs_writes_labels_at_their_ranges() {
        let cols: u16 = 20;
        let mut buf = vec![StyledCell::default(); 20 * 2];
        let regions = layout::tab_layout("s", &["a".to_string(), "b".to_string()], 0, false, cols);
        // Tab row is the last row (y = 1).
        draw_tabs(
            &mut buf,
            cols,
            1,
            "s",
            &regions,
            0,
            &crate::theme::Theme::default(),
        );
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
        assert!(
            s.starts_with("\x1b[2J\x1b[H\x1b[0m"),
            "frame must start with clear+home+reset, got: {s:?}"
        );
        assert!(s.contains("ab "), "frame must contain the pane row content");
        let plain = strip_csi(&s);
        assert!(
            plain.contains("s  "),
            "frame must contain the session prefix + two-space gap, plain={plain:?}"
        );
        assert!(
            s.ends_with("\x1b[1;2H"),
            "frame must end with the cursor CUP"
        );
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
        assert!(
            plain.contains("LLL┃RRR"),
            "ordered divider content present in stripped frame; got: {plain:?}"
        );
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
        let out = c.render(
            vp,
            &tree,
            "s",
            &names,
            1,
            1,
            false,
            &crate::theme::Theme::default(),
            &[(1, &pane)],
        );
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

    #[test]
    fn draw_sidebar_writes_agents_header() {
        let cols: u16 = 16;
        let rows: u16 = 6;
        let mut buf = vec![StyledCell::default(); (cols as usize) * (rows as usize)];
        let rect = layout::Rect {
            x: 0,
            y: 0,
            w: cols,
            h: rows,
        };
        let theme = crate::theme::Theme::default();
        let entries: Vec<crate::session::AgentEntry> = vec![];
        draw_sidebar(&mut buf, cols, rect, &theme, &entries);
        let row0: String = (0..cols).map(|x| buf[x as usize].ch).collect();
        assert!(row0.contains("AGENTS"), "row0: {row0:?}");
    }

    #[test]
    fn draw_sidebar_colors_dot_by_state_from_theme() {
        let cols: u16 = 16;
        let rows: u16 = 6;
        let mut buf = vec![StyledCell::default(); (cols as usize) * (rows as usize)];
        let rect = layout::Rect {
            x: 0,
            y: 0,
            w: cols,
            h: rows,
        };
        let theme = crate::theme::Theme::default();
        let entries = vec![crate::session::AgentEntry {
            session_name: "test".into(),
            window_index: 0,
            pane_id: 1,
            agent: crate::detect::Agent::Claude,
            state: crate::detect::AgentState::Working,
        }];
        draw_sidebar(&mut buf, cols, rect, &theme, &entries);
        let dot_idx = (2 * cols as usize) + 13;
        assert_eq!(buf[dot_idx].ch, '●');
        assert_eq!(buf[dot_idx].style.fg, theme.agent_working_fg);
    }

    #[test]
    fn draw_sidebar_dot_colors_change_with_flavor() {
        let cols: u16 = 16;
        let rect = layout::Rect {
            x: 0,
            y: 0,
            w: cols,
            h: 6,
        };
        let mut buf_latte = vec![StyledCell::default(); (cols as usize) * 6];
        let mut buf_mocha = vec![StyledCell::default(); (cols as usize) * 6];
        let latte = crate::theme::Theme::from_flavor(crate::theme::Flavor::Latte);
        let mocha = crate::theme::Theme::from_flavor(crate::theme::Flavor::Mocha);
        let entries = vec![crate::session::AgentEntry {
            session_name: "test".into(),
            window_index: 0,
            pane_id: 1,
            agent: crate::detect::Agent::Claude,
            state: crate::detect::AgentState::Idle,
        }];
        draw_sidebar(&mut buf_latte, cols, rect, &latte, &entries);
        draw_sidebar(&mut buf_mocha, cols, rect, &mocha, &entries);
        let dot_idx = (2 * cols as usize) + 13;
        assert_ne!(buf_latte[dot_idx].style.fg, buf_mocha[dot_idx].style.fg);
    }

    #[test]
    fn draw_sidebar_empty_state_uses_inactive_tab_fg() {
        let cols: u16 = 16;
        let mut buf = vec![StyledCell::default(); (cols as usize) * 6];
        let rect = layout::Rect {
            x: 0,
            y: 0,
            w: cols,
            h: 6,
        };
        let theme = crate::theme::Theme::default();
        draw_sidebar(&mut buf, cols, rect, &theme, &[]);
        let row2: String = (1..cols)
            .map(|x| buf[(2 * cols as usize) + x as usize].ch)
            .collect();
        assert!(row2.contains("no claude"), "row2: {row2:?}");
        let paren_idx = (2 * cols as usize) + 1;
        assert_eq!(buf[paren_idx].style.fg, theme.inactive_tab_fg);
    }

    #[test]
    fn draw_tabs_skips_row_fill_when_bar_bg_default() {
        let cols: u16 = 20;
        let mut buf = vec![StyledCell::default(); cols as usize];
        let theme = crate::theme::Theme {
            bar_bg: Color::Default,
            ..crate::theme::Theme::default()
        };
        let names = vec!["a".to_string()];
        let regions = layout::tab_layout("s", &names, 0, false, cols);
        draw_tabs(&mut buf, cols, 0, "s", &regions, 0, &theme);

        // The cell BEFORE the active chip (after the 2-cell prefix gap) must
        // remain default — no row fill should have touched it.
        assert_eq!(buf[2], StyledCell::default(), "prefix gap stays default");
        // Cells far past the last tab also stay default.
        for x in (cols - 3)..cols {
            assert_eq!(buf[x as usize], StyledCell::default(), "right tail at x={x}");
        }
    }

    #[test]
    fn draw_tabs_writes_session_prefix_as_plain_text() {
        let cols: u16 = 20;
        let mut buf = vec![StyledCell::default(); cols as usize];
        let theme = crate::theme::Theme {
            bar_bg:     Color::Default,
            session_bg: Color::Default,
            session_fg: Color::Rgb(11, 22, 33),
            ..crate::theme::Theme::default()
        };
        let names: Vec<String> = vec![];
        let regions = layout::tab_layout("session", &names, 0, false, cols);
        draw_tabs(&mut buf, cols, 0, "session", &regions, 0, &theme);

        // Session prefix cells carry the session_fg but bg is Color::Default.
        for (x, ch) in "session".chars().enumerate() {
            assert_eq!(buf[x].ch, ch, "label char at x={x}");
            assert_eq!(buf[x].style.fg, theme.session_fg, "session_fg at x={x}");
            assert_eq!(buf[x].style.bg, Color::Default, "no session bg at x={x}");
        }
    }

    #[test]
    fn draw_tabs_inactive_tab_has_no_bg() {
        let cols: u16 = 20;
        let mut buf = vec![StyledCell::default(); cols as usize];
        let theme = crate::theme::Theme {
            inactive_tab_bg: Color::Default,
            inactive_tab_fg: Color::Rgb(44, 55, 66),
            bar_bg: Color::Default,
            session_bg: Color::Default,
            ..crate::theme::Theme::default()
        };
        // Two tabs, active = 0 so we can inspect tab 1's inactive cells.
        let names = vec!["zsh".to_string(), "vim".to_string()];
        let regions = layout::tab_layout("s", &names, 0, false, cols);
        draw_tabs(&mut buf, cols, 0, "s", &regions, 0, &theme);

        let inactive = &regions[1];
        for x in inactive.x_start..=inactive.x_end {
            let cell = buf[x as usize];
            assert_eq!(cell.style.bg, Color::Default, "no inactive bg at x={x}");
            assert_eq!(cell.style.fg, theme.inactive_tab_fg, "inactive fg at x={x}");
        }

        // The cell at inactive.x_end + 1 (if within bounds) must stay default.
        if (inactive.x_end + 1) < cols {
            let gap_x = (inactive.x_end + 1) as usize;
            assert_eq!(buf[gap_x], StyledCell::default(), "trailing gap stays default");
        }
    }

    #[test]
    fn draw_tabs_never_writes_past_x_end_for_clipped_active_chip() {
        // Synthetic case: hand-craft a TabRegion whose x_end is well below
        // the buffer's right edge, so any cells at x > x_end MUST remain
        // untouched if draw_tabs respects x_end.
        let cols: u16 = 30;
        let mut buf = vec![StyledCell::default(); cols as usize];
        let theme = crate::theme::Theme {
            active_tab_bg: Color::Rgb(99, 99, 99),
            ..crate::theme::Theme::default()
        };
        let regions = vec![layout::TabRegion {
            window: 0,
            x_start: 3,
            x_end: 8, // chip occupies 6 cells; cells 9..30 must stay default
            label: "1:zsh*".to_string(),
        }];
        draw_tabs(&mut buf, cols, 0, "x", &regions, 0, &theme);

        for x in 9..cols {
            assert_eq!(
                buf[x as usize], StyledCell::default(),
                "x={x} is past x_end and must be untouched"
            );
        }
        // Real-clip path: produce a region from tab_layout where the chip
        // overflows. Choose width so chip_w would exceed remaining width.
        // session "s" (1) + 2 = prefix 3. Label "1:zzzzz*" (active) ⇒
        // label_len 8, chip_w 10. Width = 8 ⇒ chip clips at width - 1 = 7.
        let names = vec!["zzzzz".to_string()];
        let regions2 = layout::tab_layout("s", &names, 0, false, 8);
        assert_eq!(regions2.len(), 1);
        assert_eq!(regions2[0].x_end, 7, "x_end clipped to width - 1");
    }

    #[test]
    fn draw_tabs_paints_z_suffix_inside_active_bg() {
        let cols: u16 = 40;
        let mut buf = vec![StyledCell::default(); cols as usize];
        let theme = crate::theme::Theme {
            active_tab_bg: Color::Rgb(99, 99, 99),
            active_tab_fg: Color::Rgb(11, 11, 11),
            active_tab_bold: true,
            ..crate::theme::Theme::default()
        };
        let names = vec!["zsh".to_string(), "vim".to_string()];
        // zoomed = true ⇒ active label "2:vim*Z" (7 chars), chip 9 cells wide.
        let regions = layout::tab_layout("default", &names, 1, true, cols);
        let r1 = &regions[1];
        draw_tabs(&mut buf, cols, 0, "default", &regions, 1, &theme);

        // The 'Z' character lands at x_start + 1 (left pad) + 6 (label offset)
        // = x_start + 7, which equals x_end - 1.
        let z_x = r1.x_end - 1;
        assert_eq!(buf[z_x as usize].ch, 'Z');
        assert_eq!(buf[z_x as usize].style.bg, theme.active_tab_bg);
        assert!(buf[z_x as usize].style.bold);
        // The right pad cell at x_end carries a space with the active style.
        assert_eq!(buf[r1.x_end as usize].ch, ' ');
        assert_eq!(buf[r1.x_end as usize].style.bg, theme.active_tab_bg);
    }

    #[test]
    fn serialize_full_zero_tabs_emits_zero_bg_sgrs() {
        // Stronger pin than counting: render with NO tabs (just a session
        // prefix). All theme bg roles are Color::Default; nothing should
        // ever paint a bg. The byte stream MUST contain zero "48;" SGR
        // codes. If a future refactor accidentally re-introduces a row
        // fill or session-prefix bg paint, this test fails immediately.
        let cols: u16 = 40;
        let rows: u16 = 3;
        let viewport = Rect { x: 0, y: 0, w: cols, h: rows };
        let theme = crate::theme::Theme::default(); // OneDark — transparent bgs
        let root = Node::Leaf(0);
        let pane = StubScreen::new(cols, rows - 1, &[], (0, 0));
        let tab_names: Vec<String> = vec![];
        let bytes = Compositor::new().render(
            viewport, &root, "session", &tab_names, 0, 0u64, false,
            &theme, &[(0u64, &pane as &dyn PaneCells)],
        );
        let text = String::from_utf8_lossy(&bytes);
        assert_eq!(
            text.matches("48;").count(),
            0,
            "no bg SGR anywhere when every bg role is Color::Default and there is no active tab"
        );
        // The session-prefix label still emits fg SGRs.
        assert!(text.contains("38;"), "session prefix fg SGR present");
    }

    #[test]
    fn serialize_full_active_chip_emits_exactly_one_bg_sgr() {
        // Render with one active tab. The active chip paints `1 + label + 1`
        // contiguous cells with the same pen — the serializer collapses them
        // into ONE "48;" SGR transition. Any other bg paint (row fill,
        // inactive label, gap) would push the count above 1; an accidentally
        // dropped active bg would push it to 0. Both fail this assertion.
        let cols: u16 = 40;
        let rows: u16 = 3;
        let viewport = Rect { x: 0, y: 0, w: cols, h: rows };
        let theme = crate::theme::Theme::default();
        let root = Node::Leaf(0);
        let pane = StubScreen::new(cols, rows - 1, &[], (0, 0));
        let tab_names = vec!["zsh".to_string(), "vim".to_string()];
        let bytes = Compositor::new().render(
            viewport, &root, "session", &tab_names, 1, 0u64, false,
            &theme, &[(0u64, &pane as &dyn PaneCells)],
        );
        let text = String::from_utf8_lossy(&bytes);
        assert_eq!(
            text.matches("48;").count(),
            1,
            "exactly one bg SGR transition for the single active chip; \
             gap and inactive tab must not paint bg"
        );
        assert!(text.contains("38;"), "fg SGRs are emitted for labels");
    }

    #[test]
    fn serialize_full_inter_tab_gap_resets_to_default_pen() {
        // Stronger pin for the inter-tab gap claim: render with one inactive
        // ("1:zsh") followed by one active ("2:vim*"). In the byte stream,
        // between the inactive label and the active chip, the serializer
        // MUST emit a bare `\x1b[0m` (the pen reset at the gap cell) AND
        // MUST NOT emit any `48;` (no accidental bg paint in the gap). A
        // regression that styled the gap with fg-only inactive bytes would
        // skip the reset and this test would fail.
        let cols: u16 = 40;
        let rows: u16 = 3;
        let viewport = Rect { x: 0, y: 0, w: cols, h: rows };
        let theme = crate::theme::Theme::default();
        let root = Node::Leaf(0);
        let pane = StubScreen::new(cols, rows - 1, &[], (0, 0));
        let tab_names = vec!["zsh".to_string(), "vim".to_string()];
        let bytes = Compositor::new().render(
            viewport, &root, "session", &tab_names, 1, 0u64, false,
            &theme, &[(0u64, &pane as &dyn PaneCells)],
        );
        let text = String::from_utf8_lossy(&bytes);
        let inactive_pos = text.find("1:zsh").expect("inactive label in output");
        let active_pos = text.find("2:vim*").expect("active label in output");
        assert!(
            inactive_pos < active_pos,
            "inactive label appears before active label"
        );
        // The bytes from end-of-inactive-label to start-of-"2:vim*" include
        // the gap pen-reset, the gap space cell, the active chip's SGR
        // (which legitimately contains `48;`), and the chip's left pad
        // space. Trim off the active SGR + left pad by finding the LAST
        // `\x1b[` before "2:vim*" — that's the active chip's SGR opener.
        // The remaining "gap" slice is just the gap-cell bytes.
        let span_to_active = &text[inactive_pos + "1:zsh".len()..active_pos];
        let active_sgr_start = span_to_active
            .rfind("\x1b[")
            .expect("active chip SGR present before its label");
        let gap = &span_to_active[..active_sgr_start];
        assert!(
            gap.contains("\x1b[0m"),
            "gap between inactive label and active chip must reset pen: gap = {gap:?}"
        );
        assert!(
            !gap.contains("48;"),
            "gap must not emit any bg SGR: gap = {gap:?}"
        );
    }
}
