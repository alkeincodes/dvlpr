//! Pure layout model for multi-pane windows: a binary split tree, geometry,
//! tree mutations, and hit-testing. No I/O, no async — every function is a
//! deterministic transform, unit-tested in isolation.

pub type PaneId = u64;

/// A rectangle in 0-based cell coordinates (top-left origin).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Rect {
    pub x: u16,
    pub y: u16,
    pub w: u16,
    pub h: u16,
}

impl Rect {
    /// True if the 0-based cell (x, y) lies inside this rect.
    pub fn contains(&self, x: u16, y: u16) -> bool {
        x >= self.x
            && x < self.x.saturating_add(self.w)
            && y >= self.y
            && y < self.y.saturating_add(self.h)
    }
}

/// Orientation of a split. `Horizontal` stacks panes top/bottom (horizontal
/// divider line); `Vertical` places them side-by-side (vertical divider line).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SplitDir {
    Horizontal,
    Vertical,
}

/// A window's layout: either a single pane (leaf) or a split of two subtrees.
#[derive(Clone, Debug, PartialEq)]
pub enum Node {
    Leaf(PaneId),
    Split {
        dir: SplitDir,
        ratio: f32,
        first: Box<Node>,
        second: Box<Node>,
    },
}

/// Which child of a `Split` — used to build a path from the root to a node.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Side {
    First,
    Second,
}

/// Path from the tree root to a `Split` node (empty = the root split itself).
pub type SplitPath = Vec<Side>;

/// A divider line between a split's two children, plus the path to that split
/// (so a drag on this divider knows which `ratio` to adjust).
#[derive(Clone, Debug, PartialEq)]
pub struct Divider {
    pub rect: Rect,
    pub path: SplitPath,
    pub dir: SplitDir,
}

/// A window's tab in the tab bar: its rendered label and the inclusive 0-based
/// x-range it occupies (the single source of truth shared by the compositor's
/// drawing and the click hit-testing).
#[derive(Clone, Debug, PartialEq)]
pub struct TabRegion {
    pub window: usize,
    pub x_start: u16,
    pub x_end: u16,
    pub label: String,
}

/// The clickable "[+]" new-window button in the tab bar: the inclusive
/// 0-based x-range it occupies on the tab/status row. Single source of
/// truth shared by the compositor's drawing and click hit-testing,
/// mirroring `TabRegion`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PlusButton {
    pub x_start: u16,
    pub x_end: u16,
}

/// The literal button glyph. 3 display cells.
pub const PLUS_LABEL: &str = "[+]";
/// Display width of the button glyph, in cells.
pub const PLUS_WIDTH: u16 = PLUS_LABEL.len() as u16;
/// Cells inserted before the button, on top of the tab separator (total visual blank before the button is PLUS_GAP + 1).
pub const PLUS_GAP: u16 = 2;
/// Total cells the button reserves at the right of the tab area:
/// PLUS_GAP + PLUS_WIDTH.
pub const PLUS_RESERVE: u16 = PLUS_GAP + PLUS_WIDTH;

/// What a mouse click landed on.
#[derive(Clone, Debug, PartialEq)]
pub enum Hit {
    Pane(PaneId),
    Divider(SplitPath),
    Tab(usize),
    /// Click on a row inside the agent-awareness sidebar.
    /// `window_index` is 0-based to match `Session.active_window`.
    SidebarEntry {
        window_index: usize,
        pane_id: PaneId,
    },
    /// Click on the tab-bar "[+]" button → create a new window.
    NewWindowButton,
    None,
}

/// The y of the 1-row status/tab bar (bottom of the viewport), or `None` only
/// when the viewport has no rows. The bar is always present (single-window too),
/// so `window_count` no longer affects visibility.
pub fn tab_row(viewport: Rect, _window_count: usize) -> Option<u16> {
    if viewport.h > 0 {
        Some(viewport.y + viewport.h - 1)
    } else {
        None
    }
}

/// The content rect (viewport minus the always-present bar row).
pub fn content_area(viewport: Rect, window_count: usize) -> Rect {
    if tab_row(viewport, window_count).is_some() {
        Rect {
            h: viewport.h - 1,
            ..viewport
        }
    } else {
        viewport
    }
}

/// Default sidebar width in columns (runtime-configurable via `[sidebar] width`).
pub const SIDEBAR_WIDTH_DEFAULT: u16 = 26;
/// Minimum allowed sidebar width (clamped in `Config::from_toml_str`).
pub const SIDEBAR_WIDTH_MIN: u16 = 18;
/// Maximum allowed sidebar width (clamped in `Config::from_toml_str`).
pub const SIDEBAR_WIDTH_MAX: u16 = 36;

/// Minimum content-area width to keep the sidebar visible. If the
/// viewport is narrower than `sidebar_width + SIDEBAR_MIN_CONTENT_COLS`,
/// `compute_regions` silently suppresses the sidebar (returns None)
/// even when sidebar_visible is true.
pub const SIDEBAR_MIN_CONTENT_COLS: u16 = 20;

/// Viewport partitioning consumed by both the compositor (render) and
/// Session::hit (click dispatch). Unifies the previously-separate
/// `tab_row()` + `content_area()` helpers and carves off the sidebar
/// when visible.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Regions {
    /// Pane content area: viewport MINUS the bottom tab/status row,
    /// MINUS the sidebar columns on the right (when visible).
    pub content_area: Rect,
    /// y-coordinate of the always-present bottom tab/status row.
    /// Spans the FULL viewport width — sidebar does NOT shorten it.
    pub tab_status_row: u16,
    /// Sidebar region when visible AND viewport is wide enough. None
    /// otherwise — including when the `sidebar_visible` flag is true
    /// but `viewport.w < sidebar_width + SIDEBAR_MIN_CONTENT_COLS`.
    pub sidebar: Option<Rect>,
}

/// Compute the viewport partition. Single function the compositor and
/// hit-test both call — no API takes `(cols, rows, sidebar_visible)`
/// elsewhere.
pub fn compute_regions(viewport: Rect, sidebar_visible: bool, sidebar_width: u16) -> Regions {
    let tab_status_row = viewport.y + viewport.h.saturating_sub(1);
    let content_h = viewport.h.saturating_sub(1);

    let effective_visible =
        sidebar_visible && viewport.w >= sidebar_width + SIDEBAR_MIN_CONTENT_COLS;

    let (content_w, sidebar) = if effective_visible {
        let content_w = viewport.w - sidebar_width;
        let sidebar_rect = Rect {
            x: viewport.x + content_w,
            y: viewport.y,
            w: sidebar_width,
            h: content_h,
        };
        (content_w, Some(sidebar_rect))
    } else {
        (viewport.w, None)
    };

    Regions {
        content_area: Rect {
            x: viewport.x,
            y: viewport.y,
            w: content_w,
            h: content_h,
        },
        tab_status_row,
        sidebar,
    }
}

/// Compute the rect for every leaf pane within `area`, in left-to-right /
/// top-to-bottom tree order.
pub fn pane_rects(node: &Node, area: Rect) -> Vec<(PaneId, Rect)> {
    let mut out = Vec::new();
    collect_rects(node, area, &mut out);
    out
}

fn collect_rects(node: &Node, area: Rect, out: &mut Vec<(PaneId, Rect)>) {
    match node {
        Node::Leaf(id) => out.push((*id, area)),
        Node::Split {
            dir,
            ratio,
            first,
            second,
            ..
        } => {
            let (a, _divider, b) = split_area(area, *dir, *ratio);
            collect_rects(first, a, out);
            collect_rects(second, b, out);
        }
    }
}

/// Split `area` into (first, divider, second). Deterministic rounding: the first
/// child gets `floor(ratio * available)`, the second gets the remainder, and a
/// divider sits between them — so the three sum EXACTLY to `area` for ANY size.
/// The divider takes 1 cell along the split axis only when the area has room for
/// it (>= 1 cell); for a degenerate 0-sized axis the divider is 0 cells, so the
/// exact-sum invariant still holds (0 + 0 + 0 == 0) and nothing overflows.
fn split_area(area: Rect, dir: SplitDir, ratio: f32) -> (Rect, Rect, Rect) {
    match dir {
        SplitDir::Horizontal => {
            let divider_h: u16 = if area.h >= 1 { 1 } else { 0 };
            let avail = area.h - divider_h;
            let first_h = ((ratio * avail as f32).floor() as i64).clamp(0, avail as i64) as u16;
            let second_h = avail - first_h;
            let first = Rect {
                x: area.x,
                y: area.y,
                w: area.w,
                h: first_h,
            };
            let divider = Rect {
                x: area.x,
                y: area.y + first_h,
                w: area.w,
                h: divider_h,
            };
            let second = Rect {
                x: area.x,
                y: area.y + first_h + divider_h,
                w: area.w,
                h: second_h,
            };
            (first, divider, second)
        }
        SplitDir::Vertical => {
            let divider_w: u16 = if area.w >= 1 { 1 } else { 0 };
            let avail = area.w - divider_w;
            let first_w = ((ratio * avail as f32).floor() as i64).clamp(0, avail as i64) as u16;
            let second_w = avail - first_w;
            let first = Rect {
                x: area.x,
                y: area.y,
                w: first_w,
                h: area.h,
            };
            let divider = Rect {
                x: area.x + first_w,
                y: area.y,
                w: divider_w,
                h: area.h,
            };
            let second = Rect {
                x: area.x + first_w + divider_w,
                y: area.y,
                w: second_w,
                h: area.h,
            };
            (first, divider, second)
        }
    }
}

/// All divider lines in the tree, each tagged with the path to its split.
pub fn dividers(node: &Node, area: Rect) -> Vec<Divider> {
    let mut out = Vec::new();
    let mut path = Vec::new();
    collect_dividers(node, area, &mut path, &mut out);
    out
}

fn collect_dividers(node: &Node, area: Rect, path: &mut SplitPath, out: &mut Vec<Divider>) {
    if let Node::Split {
        dir,
        ratio,
        first,
        second,
        ..
    } = node
    {
        let (a, divider, b) = split_area(area, *dir, *ratio);
        out.push(Divider {
            rect: divider,
            path: path.clone(),
            dir: *dir,
        });
        path.push(Side::First);
        collect_dividers(first, a, path, out);
        path.pop();
        path.push(Side::Second);
        collect_dividers(second, b, path, out);
        path.pop();
    }
}

/// Replace the focused leaf with a split of (focused, new_id) at ratio 0.5.
/// Returns true if the focused pane was found.
pub fn split_pane(node: &mut Node, focused: PaneId, dir: SplitDir, new_id: PaneId) -> bool {
    match node {
        Node::Leaf(id) if *id == focused => {
            let existing = std::mem::replace(node, Node::Leaf(focused));
            *node = Node::Split {
                dir,
                ratio: 0.5,
                first: Box::new(existing),
                second: Box::new(Node::Leaf(new_id)),
            };
            true
        }
        Node::Leaf(_) => false,
        Node::Split { first, second, .. } => {
            split_pane(first, focused, dir, new_id) || split_pane(second, focused, dir, new_id)
        }
    }
}

/// Every pane id in the tree, in tree order.
pub fn all_panes(node: &Node) -> Vec<PaneId> {
    let mut out = Vec::new();
    fn go(n: &Node, out: &mut Vec<PaneId>) {
        match n {
            Node::Leaf(id) => out.push(*id),
            Node::Split { first, second, .. } => {
                go(first, out);
                go(second, out);
            }
        }
    }
    go(node, &mut out);
    out
}

/// Remove `target` from the tree, collapsing its parent split into the surviving
/// sibling. Returns the new tree, or `None` if `target` was the only pane (the
/// caller then closes the window). Pane ids are unique, so `target` appears once.
pub fn close_pane(node: Node, target: PaneId) -> Option<Node> {
    match node {
        Node::Leaf(id) => {
            if id == target {
                None
            } else {
                Some(Node::Leaf(id))
            }
        }
        Node::Split {
            dir,
            ratio,
            first,
            second,
        } => {
            // If a direct child is the target leaf, collapse to the other child.
            if matches!(*first, Node::Leaf(id) if id == target) {
                return Some(*second);
            }
            if matches!(*second, Node::Leaf(id) if id == target) {
                return Some(*first);
            }
            // Otherwise recurse; exactly one side contains the target and changes.
            match (close_pane(*first, target), close_pane(*second, target)) {
                (Some(f), Some(s)) => Some(Node::Split {
                    dir,
                    ratio,
                    first: Box::new(f),
                    second: Box::new(s),
                }),
                (Some(f), None) => Some(f),
                (None, Some(s)) => Some(s),
                (None, None) => None,
            }
        }
    }
}

/// The leftmost/topmost leaf — used to pick a new focus after a close.
pub fn first_leaf(node: &Node) -> PaneId {
    match node {
        Node::Leaf(id) => *id,
        Node::Split { first, .. } => first_leaf(first),
    }
}

/// Set the `ratio` of the split reached by following `path` from the root,
/// clamped to [0.05, 0.95] so neither child ever collapses to nothing. Returns
/// false if the path does not lead to a split.
pub fn set_ratio(node: &mut Node, path: &[Side], ratio: f32) -> bool {
    let mut cur = node;
    for side in path {
        match cur {
            Node::Split { first, second, .. } => {
                cur = match side {
                    Side::First => first,
                    Side::Second => second,
                };
            }
            Node::Leaf(_) => return false,
        }
    }
    match cur {
        Node::Split { ratio: r, .. } => {
            *r = ratio.clamp(0.05, 0.95);
            true
        }
        Node::Leaf(_) => false,
    }
}

/// The area and direction of the split reached by following `path` from the root.
/// Returns `None` if the path does not lead to a split. Used by divider-drag to
/// map a pointer position back to a ratio within the dragged split.
pub fn split_area_at(node: &Node, area: Rect, path: &[Side]) -> Option<(Rect, SplitDir)> {
    let mut cur = node;
    let mut cur_area = area;
    for side in path {
        match cur {
            Node::Split {
                dir,
                ratio,
                first,
                second,
                ..
            } => {
                let (a, _divider, b) = split_area(cur_area, *dir, *ratio);
                match side {
                    Side::First => {
                        cur = first;
                        cur_area = a;
                    }
                    Side::Second => {
                        cur = second;
                        cur_area = b;
                    }
                }
            }
            Node::Leaf(_) => return None,
        }
    }
    match cur {
        Node::Split { dir, .. } => Some((cur_area, *dir)),
        Node::Leaf(_) => None,
    }
}

const TAB_NAME_MAX: usize = 12;

/// Cells of background-colored padding on each side of the active tab's
/// label. Inactive tabs get no padding (no painted bg ⇒ no visible chip).
const ACTIVE_PAD_X: u16 = 1;

/// Lay out window tabs after a (separately-drawn) session-name prefix. Each label
/// is `<1-based-idx>:<name>`, with a trailing `*` on the active window and an extra
/// `Z` when the active window is zoomed. Names are truncated to TAB_NAME_MAX,
/// separated by a single space. The first region's `x_start` is offset past the
/// prefix (`session_name` width + two spaces); the prefix itself is NOT a region
/// (it is not clickable). Stops once `width` is reached.
///
/// v1 semantics: lengths/offsets are counted in Unicode scalar values
/// (`chars().count()`), which equals terminal display cells for the ASCII names we
/// expect; wide/zero-width chars would drift (deferred, as before).
pub fn tab_layout(
    session_name: &str,
    names: &[String],
    active: usize,
    zoomed: bool,
    width: u16,
) -> Vec<TabRegion> {
    let mut out = Vec::new();
    let prefix = session_name.chars().count() as u16 + 2;
    let mut x: u16 = prefix;
    for (i, name) in names.iter().enumerate() {
        if x >= width {
            break;
        }
        let mut label = format!("{}:{}", i + 1, truncate(name, TAB_NAME_MAX));
        if i == active {
            label.push('*');
            if zoomed {
                label.push('Z');
            }
        }
        let label_len = label.chars().count() as u16;
        let pad = if i == active { ACTIVE_PAD_X } else { 0 };
        let chip_w = label_len + 2 * pad;
        let x_end = x.saturating_add(chip_w.saturating_sub(1)).min(width - 1);
        out.push(TabRegion {
            window: i,
            x_start: x,
            x_end,
            label,
        });
        x = x.saturating_add(chip_w).saturating_add(1); // one-space separator
    }
    out
}

/// The full tab-bar layout: window tabs plus the "[+]" new-window button.
#[derive(Clone, Debug, PartialEq)]
pub struct TabBar {
    pub tabs: Vec<TabRegion>,
    pub plus: Option<PlusButton>,
}

/// Lay out the tab bar: window tabs followed by the "[+]" new-window
/// button. The button's width (`PLUS_RESERVE`) is reserved from the
/// available tab width FIRST, so tabs lay out and clip within the
/// reduced area and the button is always placed just past the last
/// drawn tab (the "reserve button space" rule — tabs clip before the
/// button is dropped). The button is emitted only when it fits fully
/// within `width`; otherwise `plus` is `None` and `prefix c` is the
/// fallback for that degenerate width.
pub fn tab_bar_layout(
    session_name: &str,
    names: &[String],
    active: usize,
    zoomed: bool,
    width: u16,
) -> TabBar {
    let prefix = session_name.chars().count() as u16 + 2;

    // Reserve the button's cells so tabs clip before the button is lost.
    let tab_width = width.saturating_sub(PLUS_RESERVE);
    let tabs = tab_layout(session_name, names, active, zoomed, tab_width);

    // Anchor: just past the last drawn tab (or just past the prefix when
    // no tab was drawn). The +1 mirrors the one-space separator
    // tab_layout puts between tabs.
    let after_tabs = tabs
        .last()
        .map(|t| t.x_end.saturating_add(1))
        .unwrap_or(prefix);
    let x_start = after_tabs.saturating_add(PLUS_GAP);
    let x_end = x_start.saturating_add(PLUS_WIDTH - 1); // "[+]" is 3 cells: x_start..=x_start+2

    let plus = if x_end < width {
        Some(PlusButton { x_start, x_end })
    } else {
        None
    };

    TabBar { tabs, plus }
}

/// Resolve a 0-based x within the tab row to the "[+]" button, if it
/// landed inside the button's rect. Checked BEFORE `tab_hit` by the
/// caller so the button's cells are never mistaken for a tab.
pub fn plus_button_hit(plus: Option<&PlusButton>, x: u16) -> bool {
    matches!(plus, Some(b) if x >= b.x_start && x <= b.x_end)
}

/// Truncate to `max` cells, replacing the tail with `…` when over length.
fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let mut t: String = s.chars().take(max - 1).collect();
        t.push('…');
        t
    }
}

/// What `Session::hit` passes to `sidebar_rows` — just the click-target
/// ids. Layout-local so this module doesn't import `session::AgentEntry`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SidebarRowInput {
    pub window_index: usize, // 0-based
    pub pane_id: PaneId,
}

/// One entry's click region within the sidebar. `y` is viewport-rooted;
/// `h` covers both rendered rows (row a + row b). Gap row falls
/// outside the half-open `[y, y + h)` and is not clickable.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SidebarRow {
    pub y: u16,
    pub h: u16,
    pub window_index: usize,
    pub pane_id: PaneId,
}

/// Compute per-row click targets for the visible sidebar entries.
/// Layout is fixed at the top of `sidebar`:
///   row 0 (rect.y + 0) — "AGENTS" header (NOT a click target)
///   row 1 (rect.y + 1) — horizontal '─' separator
///   row 2 (rect.y + 2) — leading blank row
///   rows 3..N — 3 rows per entry (row a + row b + gap), truncated to fit
///               within `rect.h`. Only rows a+b are clickable (h=2);
///               the gap row is a miss zone.
///
/// Used by `Session::hit` to translate a click to a SidebarEntry.
pub fn sidebar_rows(rect: Rect, entries: &[SidebarRowInput]) -> Vec<SidebarRow> {
    let mut out = Vec::with_capacity(entries.len());
    // 2 header rows + 1 leading blank + 3 rows per entry (a + b + gap).
    let max_entries = (rect.h.saturating_sub(3) / 3) as usize;
    if max_entries == 0 {
        return out;
    }
    for (i, e) in entries.iter().take(max_entries).enumerate() {
        let y = rect.y + 3 + (i as u16) * 3;
        out.push(SidebarRow {
            y,
            h: 2,
            window_index: e.window_index,
            pane_id: e.pane_id,
        });
    }
    out
}

/// Resolve a 0-based x within the tab row to the window index whose `TabRegion`
/// contains it, or `None` if the click landed in the gap/prefix.
pub fn tab_hit(tabs: &[TabRegion], x: u16) -> Option<usize> {
    tabs.iter()
        .find(|t| x >= t.x_start && x <= t.x_end)
        .map(|t| t.window)
}

/// Hit-test within an already-computed content area. Does NOT handle
/// the tab/status row, sidebar region, or any other chrome — those are
/// dispatched by `Session::hit` upstream. Tests divider rects first,
/// then pane rects. Returns `Hit::None` when the click misses everything.
///
/// Use this from `Session::hit` after `compute_regions` produces a
/// `content_area`. Do NOT pass `viewport` here — `hit_test` is the
/// wrapper for callers that haven't switched to the new API yet.
pub fn hit_within_content(node: &Node, content_area: Rect, col: u16, row: u16) -> Hit {
    if col == 0 || row == 0 {
        return Hit::None;
    }
    let x = col - 1;
    let y = row - 1;

    if !content_area.contains(x, y) {
        return Hit::None;
    }

    for d in dividers(node, content_area) {
        if d.rect.contains(x, y) {
            return Hit::Divider(d.path);
        }
    }
    for (id, r) in pane_rects(node, content_area) {
        if r.contains(x, y) {
            return Hit::Pane(id);
        }
    }
    Hit::None
}

/// Backward-compatible hit-test that takes the full viewport. Computes
/// `Regions` internally and delegates content hits to `hit_within_content`.
/// Tab-row and sidebar dispatch are NOT handled here — callers that need
/// them should use `compute_regions` and call `hit_within_content`
/// directly (see `Session::hit`).
pub fn hit_test(
    node: &Node,
    viewport: Rect,
    window_count: usize,
    tabs: &[TabRegion],
    col: u16,
    row: u16,
) -> Hit {
    if col == 0 || row == 0 {
        return Hit::None;
    }
    let x = col - 1;
    let y = row - 1;

    if let Some(ty) = tab_row(viewport, window_count) {
        if y == ty {
            return match tab_hit(tabs, x) {
                Some(w) => Hit::Tab(w),
                None => Hit::None,
            };
        }
    }

    let content = content_area(viewport, window_count);
    hit_within_content(node, content, col, row)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_tree() -> Node {
        // Vertical split: leaf 1 (left) | leaf 2 (right).
        Node::Split {
            dir: SplitDir::Vertical,
            ratio: 0.5,
            first: Box::new(Node::Leaf(1)),
            second: Box::new(Node::Leaf(2)),
        }
    }

    #[test]
    fn hit_test_maps_clicks_to_panes() {
        let tree = sample_tree();
        let vp = Rect {
            x: 0,
            y: 0,
            w: 11,
            h: 4,
        };
        // Single window => bar at y=3, content area is h=3.
        // Left pane is x 0..=4 (w 5); divider at x 5; right pane x 6..=10.
        // SGR coords are 1-based: col 1 => x 0 (left pane).
        assert_eq!(hit_test(&tree, vp, 1, &[], 1, 1), Hit::Pane(1));
        // col 7 => x 6 (right pane).
        assert_eq!(hit_test(&tree, vp, 1, &[], 7, 1), Hit::Pane(2));
    }

    #[test]
    fn hit_test_detects_the_divider() {
        let tree = sample_tree();
        let vp = Rect {
            x: 0,
            y: 0,
            w: 11,
            h: 4,
        };
        // Divider column is x 5 => SGR col 6.
        assert_eq!(hit_test(&tree, vp, 1, &[], 6, 1), Hit::Divider(vec![]));
    }

    #[test]
    fn hit_test_detects_a_tab_click() {
        let tree = Node::Leaf(1);
        let vp = Rect {
            x: 0,
            y: 0,
            w: 80,
            h: 24,
        };
        let names = vec!["a".to_string(), "b".to_string()];
        let tabs = tab_layout("s", &names, 0, false, 80);
        // 2 windows => tab row at y 23 => SGR row 24. Click within tab[1]'s range.
        let col = tabs[1].x_start + 1; // 1-based
        assert_eq!(hit_test(&tree, vp, 2, &tabs, col, 24), Hit::Tab(1));
    }

    #[test]
    fn hit_test_outside_everything_is_none() {
        let tree = Node::Leaf(1);
        let vp = Rect {
            x: 0,
            y: 0,
            w: 10,
            h: 5,
        };
        assert_eq!(hit_test(&tree, vp, 1, &[], 0, 0), Hit::None); // 0 is not a valid 1-based coord
        assert_eq!(hit_test(&tree, vp, 1, &[], 99, 99), Hit::None); // out of bounds
    }

    #[test]
    fn tab_layout_is_one_based_with_active_marker() {
        let names = vec!["zsh".to_string(), "claude".to_string()];
        let tabs = tab_layout("work", &names, 1, false, 80);
        // prefix "work" + two spaces = 6 cells; first tab starts at x=6.
        assert_eq!(tabs.len(), 2);
        assert_eq!(tabs[0].x_start, 6);
        assert_eq!(tabs[0].label, "1:zsh");
        assert_eq!(tabs[1].label, "2:claude*"); // active gets a trailing '*'
        assert_eq!(tabs[1].window, 1);
    }

    #[test]
    fn tab_layout_marks_zoomed_active_window() {
        let names = vec!["zsh".to_string(), "claude".to_string()];
        let tabs = tab_layout("work", &names, 1, true, 80);
        assert_eq!(tabs[1].label, "2:claude*Z"); // active + zoomed
        assert_eq!(tabs[0].label, "1:zsh"); // inactive unmarked
    }

    #[test]
    fn tab_layout_truncates_long_names() {
        let names = vec!["a-very-long-window-name".to_string()];
        let tabs = tab_layout("s", &names, 0, false, 80);
        // "1:" + truncate(name, 12) + "*"
        assert_eq!(tabs[0].label, "1:a-very-long…*");
    }

    #[test]
    fn set_ratio_at_root_updates_the_split() {
        let mut tree = Node::Split {
            dir: SplitDir::Vertical,
            ratio: 0.5,
            first: Box::new(Node::Leaf(1)),
            second: Box::new(Node::Leaf(2)),
        };
        assert!(set_ratio(&mut tree, &[], 0.7));
        if let Node::Split { ratio, .. } = tree {
            assert!((ratio - 0.7).abs() < 1e-6);
        } else {
            panic!("expected a split");
        }
    }

    #[test]
    fn set_ratio_follows_a_path_and_clamps() {
        let mut tree = Node::Split {
            dir: SplitDir::Vertical,
            ratio: 0.5,
            first: Box::new(Node::Split {
                dir: SplitDir::Horizontal,
                ratio: 0.5,
                first: Box::new(Node::Leaf(1)),
                second: Box::new(Node::Leaf(2)),
            }),
            second: Box::new(Node::Leaf(3)),
        };
        // Path [First] reaches the inner horizontal split. 0.99 clamps to 0.95.
        assert!(set_ratio(&mut tree, &[Side::First], 0.99));
        if let Node::Split { first, .. } = &tree {
            if let Node::Split { ratio, .. } = first.as_ref() {
                assert!((ratio - 0.95).abs() < 1e-6);
            } else {
                panic!("expected inner split");
            }
        }
    }

    #[test]
    fn set_ratio_on_a_bad_path_returns_false() {
        let mut tree = Node::Leaf(1);
        assert!(!set_ratio(&mut tree, &[Side::First], 0.5)); // leaf has no children
    }

    #[test]
    fn closing_the_only_pane_returns_none() {
        assert_eq!(close_pane(Node::Leaf(1), 1), None);
    }

    #[test]
    fn closing_a_pane_collapses_its_parent_into_the_sibling() {
        let tree = Node::Split {
            dir: SplitDir::Vertical,
            ratio: 0.5,
            first: Box::new(Node::Leaf(1)),
            second: Box::new(Node::Leaf(2)),
        };
        assert_eq!(close_pane(tree, 1), Some(Node::Leaf(2)));
    }

    #[test]
    fn closing_a_nested_pane_keeps_the_rest_of_the_tree() {
        // V( H(1,2), 3 ); close 2  =>  V( 1, 3 )
        let tree = Node::Split {
            dir: SplitDir::Vertical,
            ratio: 0.5,
            first: Box::new(Node::Split {
                dir: SplitDir::Horizontal,
                ratio: 0.5,
                first: Box::new(Node::Leaf(1)),
                second: Box::new(Node::Leaf(2)),
            }),
            second: Box::new(Node::Leaf(3)),
        };
        let result = close_pane(tree, 2).unwrap();
        assert_eq!(all_panes(&result), vec![1, 3]);
    }

    #[test]
    fn first_leaf_returns_leftmost_topmost_pane() {
        let tree = Node::Split {
            dir: SplitDir::Vertical,
            ratio: 0.5,
            first: Box::new(Node::Split {
                dir: SplitDir::Horizontal,
                ratio: 0.5,
                first: Box::new(Node::Leaf(7)),
                second: Box::new(Node::Leaf(8)),
            }),
            second: Box::new(Node::Leaf(9)),
        };
        assert_eq!(first_leaf(&tree), 7);
    }

    #[test]
    fn split_replaces_focused_leaf_with_a_split() {
        let mut tree = Node::Leaf(1);
        let ok = split_pane(&mut tree, 1, SplitDir::Horizontal, 2);
        assert!(ok);
        assert_eq!(
            tree,
            Node::Split {
                dir: SplitDir::Horizontal,
                ratio: 0.5,
                first: Box::new(Node::Leaf(1)),
                second: Box::new(Node::Leaf(2)),
            }
        );
    }

    #[test]
    fn split_targets_the_focused_leaf_in_a_tree() {
        let mut tree = Node::Split {
            dir: SplitDir::Vertical,
            ratio: 0.5,
            first: Box::new(Node::Leaf(1)),
            second: Box::new(Node::Leaf(2)),
        };
        let ok = split_pane(&mut tree, 2, SplitDir::Vertical, 3);
        assert!(ok);
        // Leaf 2 became a split of (2, 3).
        assert_eq!(all_panes(&tree), vec![1, 2, 3]);
    }

    #[test]
    fn split_unknown_pane_is_a_noop() {
        let mut tree = Node::Leaf(1);
        let ok = split_pane(&mut tree, 99, SplitDir::Vertical, 2);
        assert!(!ok);
        assert_eq!(tree, Node::Leaf(1));
    }

    #[test]
    fn single_leaf_has_no_dividers() {
        assert_eq!(
            dividers(
                &Node::Leaf(1),
                Rect {
                    x: 0,
                    y: 0,
                    w: 10,
                    h: 5
                }
            ),
            vec![]
        );
    }

    #[test]
    fn one_split_has_one_divider_with_empty_path() {
        let tree = Node::Split {
            dir: SplitDir::Vertical,
            ratio: 0.5,
            first: Box::new(Node::Leaf(1)),
            second: Box::new(Node::Leaf(2)),
        };
        let ds = dividers(
            &tree,
            Rect {
                x: 0,
                y: 0,
                w: 11,
                h: 4,
            },
        );
        assert_eq!(ds.len(), 1);
        assert_eq!(
            ds[0].rect,
            Rect {
                x: 5,
                y: 0,
                w: 1,
                h: 4
            }
        ); // vertical divider column
        assert_eq!(ds[0].path, Vec::<Side>::new()); // root split
        assert_eq!(ds[0].dir, SplitDir::Vertical);
    }

    #[test]
    fn nested_splits_record_paths() {
        // Outer vertical; its first child is a horizontal split.
        let tree = Node::Split {
            dir: SplitDir::Vertical,
            ratio: 0.5,
            first: Box::new(Node::Split {
                dir: SplitDir::Horizontal,
                ratio: 0.5,
                first: Box::new(Node::Leaf(1)),
                second: Box::new(Node::Leaf(2)),
            }),
            second: Box::new(Node::Leaf(3)),
        };
        let ds = dividers(
            &tree,
            Rect {
                x: 0,
                y: 0,
                w: 21,
                h: 5,
            },
        );
        // Outer divider (root, empty path) + inner horizontal divider (path [First]).
        assert_eq!(ds.len(), 2);
        assert_eq!(ds[0].path, Vec::<Side>::new());
        assert_eq!(ds[0].dir, SplitDir::Vertical);
        assert_eq!(ds[1].path, vec![Side::First]);
        assert_eq!(ds[1].dir, SplitDir::Horizontal);
    }

    #[test]
    fn single_window_still_reserves_bottom_row_for_the_bar() {
        let vp = Rect {
            x: 0,
            y: 0,
            w: 80,
            h: 24,
        };
        // The status bar is always present now, even with one window.
        assert_eq!(tab_row(vp, 1), Some(23));
        assert_eq!(
            content_area(vp, 1),
            Rect {
                x: 0,
                y: 0,
                w: 80,
                h: 23,
            }
        );
    }

    #[test]
    fn zero_height_viewport_has_no_bar() {
        let vp = Rect {
            x: 0,
            y: 0,
            w: 80,
            h: 0,
        };
        assert_eq!(tab_row(vp, 1), None);
        assert_eq!(content_area(vp, 1), vp);
    }

    #[test]
    fn multi_window_reserves_bottom_row_for_tabs() {
        let vp = Rect {
            x: 0,
            y: 0,
            w: 80,
            h: 24,
        };
        assert_eq!(tab_row(vp, 3), Some(23)); // bottom row
        assert_eq!(
            content_area(vp, 3),
            Rect {
                x: 0,
                y: 0,
                w: 80,
                h: 23
            }
        );
    }

    #[test]
    fn single_leaf_fills_the_area() {
        let tree = Node::Leaf(7);
        let area = Rect {
            x: 0,
            y: 0,
            w: 80,
            h: 24,
        };
        assert_eq!(pane_rects(&tree, area), vec![(7, area)]);
    }

    #[test]
    fn horizontal_split_stacks_top_bottom_with_divider() {
        // Horizontal split: first pane on top, 1-row divider, second below.
        let tree = Node::Split {
            dir: SplitDir::Horizontal,
            ratio: 0.5,
            first: Box::new(Node::Leaf(1)),
            second: Box::new(Node::Leaf(2)),
        };
        let area = Rect {
            x: 0,
            y: 0,
            w: 10,
            h: 5,
        };
        // avail = 5 - 1 = 4; first_h = floor(0.5*4)=2; second_h = 2.
        let rects = pane_rects(&tree, area);
        assert_eq!(
            rects[0],
            (
                1,
                Rect {
                    x: 0,
                    y: 0,
                    w: 10,
                    h: 2
                }
            )
        );
        assert_eq!(
            rects[1],
            (
                2,
                Rect {
                    x: 0,
                    y: 3,
                    w: 10,
                    h: 2
                }
            )
        ); // y = 0 + 2 + 1 divider
    }

    #[test]
    fn vertical_split_places_side_by_side_with_divider() {
        let tree = Node::Split {
            dir: SplitDir::Vertical,
            ratio: 0.5,
            first: Box::new(Node::Leaf(1)),
            second: Box::new(Node::Leaf(2)),
        };
        let area = Rect {
            x: 0,
            y: 0,
            w: 11,
            h: 4,
        };
        // avail = 11 - 1 = 10; first_w = 5; second_w = 5.
        let rects = pane_rects(&tree, area);
        assert_eq!(
            rects[0],
            (
                1,
                Rect {
                    x: 0,
                    y: 0,
                    w: 5,
                    h: 4
                }
            )
        );
        assert_eq!(
            rects[1],
            (
                2,
                Rect {
                    x: 6,
                    y: 0,
                    w: 5,
                    h: 4
                }
            )
        ); // x = 0 + 5 + 1 divider
    }

    #[test]
    fn nested_split_geometry() {
        // Vertical split: left is a horizontal split (1 over 2), right is leaf 3.
        let tree = Node::Split {
            dir: SplitDir::Vertical,
            ratio: 0.5,
            first: Box::new(Node::Split {
                dir: SplitDir::Horizontal,
                ratio: 0.5,
                first: Box::new(Node::Leaf(1)),
                second: Box::new(Node::Leaf(2)),
            }),
            second: Box::new(Node::Leaf(3)),
        };
        let area = Rect {
            x: 0,
            y: 0,
            w: 21,
            h: 5,
        };
        let rects = pane_rects(&tree, area);
        // Outer V: avail_w=20, first_w=10 (x 0..10), divider x=10, second x=11 w=10.
        // Left H: area {0,0,10,5}: avail_h=4, first_h=2, second y=3 h=2.
        assert_eq!(
            rects[0],
            (
                1,
                Rect {
                    x: 0,
                    y: 0,
                    w: 10,
                    h: 2
                }
            )
        );
        assert_eq!(
            rects[1],
            (
                2,
                Rect {
                    x: 0,
                    y: 3,
                    w: 10,
                    h: 2
                }
            )
        );
        assert_eq!(
            rects[2],
            (
                3,
                Rect {
                    x: 11,
                    y: 0,
                    w: 10,
                    h: 5
                }
            )
        );
    }

    #[test]
    fn exact_sum_invariant_holds() {
        // Children + 1-cell divider must sum exactly to the parent on odd sizes.
        let tree = Node::Split {
            dir: SplitDir::Vertical,
            ratio: 0.3,
            first: Box::new(Node::Leaf(1)),
            second: Box::new(Node::Leaf(2)),
        };
        let area = Rect {
            x: 4,
            y: 2,
            w: 13,
            h: 7,
        };
        let rects = pane_rects(&tree, area);
        let (_, a) = rects[0];
        let (_, b) = rects[1];
        // a.w + 1 (divider) + b.w == area.w, and both start/heights line up.
        assert_eq!(a.w + 1 + b.w, area.w);
        assert_eq!(a.x, area.x);
        assert_eq!(b.x, a.x + a.w + 1);
        assert_eq!(a.h, area.h);
        assert_eq!(b.h, area.h);
    }

    #[test]
    fn degenerate_zero_axis_does_not_overflow() {
        // A split whose split-axis dimension is 0 (e.g. content height 0 when a
        // 1-row viewport reserves its only row for the tab bar) must NOT produce a
        // 1-cell divider that overflows the parent — the divider degrades to 0.
        let tree = Node::Split {
            dir: SplitDir::Horizontal,
            ratio: 0.5,
            first: Box::new(Node::Leaf(1)),
            second: Box::new(Node::Leaf(2)),
        };
        let area = Rect {
            x: 0,
            y: 0,
            w: 10,
            h: 0,
        };
        let rects = pane_rects(&tree, area);
        // Both panes are zero-height; their heights sum to exactly 0 (no overflow).
        assert_eq!(rects[0].1.h, 0);
        assert_eq!(rects[1].1.h, 0);
        assert_eq!(rects[0].1.h + rects[1].1.h, area.h);
    }

    #[test]
    fn split_area_at_root_returns_full_area_and_dir() {
        let tree = Node::Split {
            dir: SplitDir::Vertical,
            ratio: 0.5,
            first: Box::new(Node::Leaf(1)),
            second: Box::new(Node::Leaf(2)),
        };
        let area = Rect {
            x: 0,
            y: 0,
            w: 20,
            h: 6,
        };
        assert_eq!(
            split_area_at(&tree, area, &[]),
            Some((area, SplitDir::Vertical))
        );
    }

    #[test]
    fn split_area_at_nested_returns_child_area() {
        // Outer V (w=21): left child is an H split occupying x 0..=9 (w 10).
        let tree = Node::Split {
            dir: SplitDir::Vertical,
            ratio: 0.5,
            first: Box::new(Node::Split {
                dir: SplitDir::Horizontal,
                ratio: 0.5,
                first: Box::new(Node::Leaf(1)),
                second: Box::new(Node::Leaf(2)),
            }),
            second: Box::new(Node::Leaf(3)),
        };
        let area = Rect {
            x: 0,
            y: 0,
            w: 21,
            h: 5,
        };
        let (a, dir) = split_area_at(&tree, area, &[Side::First]).unwrap();
        assert_eq!(dir, SplitDir::Horizontal);
        assert_eq!(
            a,
            Rect {
                x: 0,
                y: 0,
                w: 10,
                h: 5
            }
        );
    }

    #[test]
    fn split_area_at_bad_path_is_none() {
        let tree = Node::Leaf(1);
        let area = Rect {
            x: 0,
            y: 0,
            w: 10,
            h: 5,
        };
        assert_eq!(split_area_at(&tree, area, &[Side::First]), None);
    }

    #[test]
    fn rect_contains_checks_bounds() {
        let r = Rect {
            x: 2,
            y: 3,
            w: 4,
            h: 5,
        };
        assert!(r.contains(2, 3)); // top-left corner included
        assert!(r.contains(5, 7)); // bottom-right corner included (x<2+4, y<3+5)
        assert!(!r.contains(1, 3)); // left of
        assert!(!r.contains(6, 3)); // right edge excluded (2+4=6)
        assert!(!r.contains(2, 8)); // bottom edge excluded (3+5=8)
        let empty = Rect {
            x: 0,
            y: 0,
            w: 0,
            h: 0,
        };
        assert!(!empty.contains(0, 0)); // zero-size contains nothing
    }

    #[test]
    fn compute_regions_gives_full_width_when_sidebar_hidden() {
        let vp = Rect {
            x: 0,
            y: 0,
            w: 80,
            h: 24,
        };
        let r = compute_regions(vp, false, 26);
        assert_eq!(r.content_area.w, 80);
        assert_eq!(r.content_area.h, 23);
        assert_eq!(r.tab_status_row, 23);
        assert_eq!(r.sidebar, None);
    }

    #[test]
    fn compute_regions_reserves_sidebar_width_when_visible() {
        let vp = Rect {
            x: 0,
            y: 0,
            w: 80,
            h: 24,
        };
        // Use the default width (26). Old test used SIDEBAR_COLS (16); update to match
        // the new default so the test pins the new contract.
        let r = compute_regions(vp, true, SIDEBAR_WIDTH_DEFAULT);
        assert_eq!(r.content_area.w, 80 - SIDEBAR_WIDTH_DEFAULT);
        assert_eq!(r.content_area.h, 23);
        assert_eq!(r.tab_status_row, 23);
        let sb = r.sidebar.expect("sidebar present");
        assert_eq!(sb.x, 80 - SIDEBAR_WIDTH_DEFAULT);
        assert_eq!(sb.w, SIDEBAR_WIDTH_DEFAULT);
        assert_eq!(sb.h, 23);
    }

    #[test]
    fn compute_regions_suppresses_sidebar_below_threshold() {
        // New threshold: SIDEBAR_WIDTH_DEFAULT (26) + SIDEBAR_MIN_CONTENT_COLS (20) = 46.
        // A viewport of width 45 is below the threshold and must suppress the sidebar.
        let vp = Rect {
            x: 0,
            y: 0,
            w: 45,
            h: 24,
        };
        let r = compute_regions(vp, true, SIDEBAR_WIDTH_DEFAULT);
        assert_eq!(r.sidebar, None);
        assert_eq!(r.content_area.w, 45, "content should keep full width");
    }

    #[test]
    fn compute_regions_does_not_panic_on_zero_width() {
        let vp = Rect {
            x: 0,
            y: 0,
            w: 0,
            h: 0,
        };
        let r = compute_regions(vp, true, 26);
        assert_eq!(r.content_area.w, 0);
        assert_eq!(r.content_area.h, 0);
        assert_eq!(r.tab_status_row, 0);
        assert_eq!(r.sidebar, None);
    }

    #[test]
    fn sidebar_rows_assigns_two_row_targets_per_entry_with_gaps() {
        let rect = Rect {
            x: 0,
            y: 0,
            w: 26,
            h: 20,
        };
        let entries = vec![
            SidebarRowInput {
                window_index: 0,
                pane_id: 0,
            },
            SidebarRowInput {
                window_index: 1,
                pane_id: 1,
            },
        ];
        let rows = sidebar_rows(rect, &entries);
        assert_eq!(rows.len(), 2);
        // Row 0 starts at y=3 (header=0, divider=1, blank=2).
        assert_eq!(rows[0].y, 3);
        assert_eq!(rows[0].h, 2);
        assert_eq!(rows[0].window_index, 0);
        // Row 1 starts at y=3 + 3 = 6 (rows a+b + gap).
        assert_eq!(rows[1].y, 6);
        assert_eq!(rows[1].h, 2);
        assert_eq!(rows[1].window_index, 1);
    }

    #[test]
    fn sidebar_rows_max_entries_uses_saturating_sub() {
        // rect.h = 2 → zero entries (no room for content).
        let rect = Rect {
            x: 0,
            y: 0,
            w: 26,
            h: 2,
        };
        let entries = vec![SidebarRowInput {
            window_index: 0,
            pane_id: 0,
        }];
        assert!(sidebar_rows(rect, &entries).is_empty());
    }

    #[test]
    fn sidebar_rows_truncates_when_rect_too_short_for_all_entries() {
        // rect.h = 5 → (5 - 3) / 3 = 0 entries.
        // rect.h = 6 → (6 - 3) / 3 = 1 entry.
        let rect_one = Rect {
            x: 0,
            y: 0,
            w: 26,
            h: 6,
        };
        let entries = vec![
            SidebarRowInput {
                window_index: 0,
                pane_id: 0,
            },
            SidebarRowInput {
                window_index: 1,
                pane_id: 1,
            },
            SidebarRowInput {
                window_index: 2,
                pane_id: 2,
            },
        ];
        let rows = sidebar_rows(rect_one, &entries);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].window_index, 0);
    }

    #[test]
    fn hit_within_content_returns_pane_for_pane_click() {
        let tree = Node::Leaf(7);
        let content = Rect {
            x: 0,
            y: 0,
            w: 20,
            h: 10,
        };
        let hit = hit_within_content(&tree, content, 5, 5);
        assert_eq!(hit, Hit::Pane(7));
    }

    #[test]
    fn hit_within_content_returns_none_outside_content() {
        let tree = Node::Leaf(7);
        let content = Rect {
            x: 0,
            y: 0,
            w: 20,
            h: 10,
        };
        let hit = hit_within_content(&tree, content, 5, 12);
        assert_eq!(hit, Hit::None);
    }

    #[test]
    fn hit_test_backward_compat_wrapper_still_works() {
        let tree = Node::Leaf(1);
        let vp = Rect {
            x: 0,
            y: 0,
            w: 20,
            h: 10,
        };
        let tabs = vec![];
        let hit = hit_test(&tree, vp, 1, &tabs, 5, 5);
        assert_eq!(hit, Hit::Pane(1));
    }

    #[test]
    fn tab_layout_active_chip_reserves_padding_x() {
        let names = vec!["zsh".to_string(), "vim".to_string(), "git".to_string()];
        // active = 1, not zoomed. Labels: "1:zsh", "2:vim*", "3:git".
        let regions = tab_layout("default", &names, 1, false, 80);
        assert_eq!(regions.len(), 3);

        // Region 0 (inactive "1:zsh", label 5 chars): width = 5,
        // so x_end - x_start = 4.
        let r0 = &regions[0];
        assert_eq!(r0.label, "1:zsh");
        assert_eq!(r0.x_end - r0.x_start, 4);

        // Region 1 (active "2:vim*", label 6 chars): width = 6 + 2*1 = 8,
        // so x_end - x_start = 7.
        let r1 = &regions[1];
        assert_eq!(r1.label, "2:vim*");
        assert_eq!(r1.x_end - r1.x_start, 7);

        // Region 2 (inactive "3:git", label 5 chars): width 5,
        // x_end - x_start = 4.
        let r2 = &regions[2];
        assert_eq!(r2.label, "3:git");
        assert_eq!(r2.x_end - r2.x_start, 4);
    }

    #[test]
    fn tab_hit_resolves_active_pad_cells_to_the_tab() {
        let names = vec!["zsh".to_string(), "vim".to_string()];
        // active = 1, label "2:vim*" with pad ⇒ chip 8 cells wide.
        let regions = tab_layout("default", &names, 1, false, 80);
        let active = &regions[1];

        // Both pad cells (x_start and x_end) MUST resolve to the active tab.
        assert_eq!(tab_hit(&regions, active.x_start), Some(1), "left pad");
        assert_eq!(tab_hit(&regions, active.x_end), Some(1), "right pad");

        // The label range is hittable too (sanity).
        assert_eq!(tab_hit(&regions, active.x_start + 1), Some(1));
        assert_eq!(tab_hit(&regions, active.x_end - 1), Some(1));
    }

    #[test]
    fn tab_hit_returns_none_for_inter_tab_gap() {
        let names = vec!["zsh".to_string(), "vim".to_string()];
        // active = 0, label "1:zsh*" with pad ⇒ chip 8 cells wide.
        // Gap cell is at regions[0].x_end + 1, BEFORE regions[1].x_start.
        let regions = tab_layout("default", &names, 0, false, 80);
        let gap_x = regions[0].x_end + 1;
        assert!(
            gap_x < regions[1].x_start,
            "expected a transparent gap cell between regions"
        );
        assert_eq!(tab_hit(&regions, gap_x), None);
    }

    #[test]
    fn tab_layout_clips_active_chip_at_viewport_right_edge() {
        // Choose a width that ends INSIDE the active chip's natural width.
        // session "x" (1 char) + 2 = prefix 3.
        // names: "aa" (inactive, label "1:aa" = 4 chars), "bb" (active, label
        // "2:bb*" = 5 chars, chip width 5 + 2 = 7).
        //   r0: x_start = 3, x_end = 3 + 3 = 6, inactive width 4 ⇒ x_end - x_start = 3.
        //   gap: x = 7
        //   r1: x_start = 8, natural x_end = 8 + 6 = 14, clipped to width - 1.
        // Picking width = 12 means r1.x_end = 11 (clipped from 14).
        let names = vec!["aa".to_string(), "bb".to_string()];
        let regions = tab_layout("x", &names, 1, false, 12);
        assert_eq!(regions.len(), 2);
        let r1 = &regions[1];
        assert_eq!(r1.x_start, 8);
        assert_eq!(r1.x_end, 11, "clipped to width - 1");
    }

    #[test]
    fn tab_layout_zoomed_active_label_includes_z_suffix_in_chip() {
        let names = vec!["zsh".to_string(), "vim".to_string()];
        // active = 1, zoomed = true ⇒ label "2:vim*Z" (7 chars).
        let regions = tab_layout("default", &names, 1, true, 80);
        let r1 = &regions[1];
        assert_eq!(r1.label, "2:vim*Z");
        // Chip width = 7 + 2 = 9 cells ⇒ x_end - x_start = 8.
        assert_eq!(r1.x_end - r1.x_start, 8);
    }

    #[test]
    fn sidebar_width_constants_match_spec_defaults() {
        assert_eq!(SIDEBAR_WIDTH_MIN, 18);
        assert_eq!(SIDEBAR_WIDTH_MAX, 36);
        assert_eq!(SIDEBAR_WIDTH_DEFAULT, 26);
    }

    #[test]
    fn compute_regions_reserves_explicit_sidebar_width() {
        let v = Rect {
            x: 0,
            y: 0,
            w: 100,
            h: 30,
        };
        let regions = compute_regions(v, true, 30);
        assert_eq!(regions.sidebar.unwrap().w, 30);
        assert_eq!(regions.content_area.w, 100 - 30);
    }

    #[test]
    fn compute_regions_suppresses_sidebar_when_viewport_too_narrow_for_min_width() {
        // 18 cell sidebar + 20 min content = 38 threshold.
        let v = Rect {
            x: 0,
            y: 0,
            w: 37,
            h: 20,
        };
        let regions = compute_regions(v, true, 18);
        assert!(regions.sidebar.is_none());
    }

    #[test]
    fn plus_button_sits_two_cells_past_last_tab() {
        let names = vec!["a".to_string(), "b".to_string()];
        let bar = tab_bar_layout("s", &names, 0, false, 40);
        let last = bar.tabs.last().expect("at least one tab");
        let pb = bar.plus.expect("button present in a wide bar");
        assert_eq!(pb.x_start, last.x_end + 1 + PLUS_GAP);
        assert_eq!(pb.x_end, pb.x_start + 2, "[+] is 3 cells wide");
    }

    #[test]
    fn plus_button_present_with_single_window() {
        let names = vec!["only".to_string()];
        let bar = tab_bar_layout("s", &names, 0, false, 40);
        assert!(bar.plus.is_some());
    }

    #[test]
    fn plus_button_space_is_reserved_so_tabs_clip_first() {
        // Width tight enough that tabs would consume the whole row without reservation.
        let names = vec![
            "alpha".to_string(),
            "bravo".to_string(),
            "charlie".to_string(),
        ];
        let width = 14;
        let bar = tab_bar_layout("s", &names, 0, false, width);
        let last = bar.tabs.last().expect("at least one tab fits");
        // Tabs are bounded into width - PLUS_RESERVE, so the last tab cannot
        // extend past (width - PLUS_RESERVE - 1).
        assert!(last.x_end <= width.saturating_sub(PLUS_RESERVE + 1));
        let pb = bar.plus.expect("button kept even when tabs clip");
        assert!(pb.x_end < width);
    }

    #[test]
    fn plus_button_fits_within_width() {
        let names = vec!["one".to_string(), "two".to_string(), "three".to_string()];
        for width in 8u16..=60 {
            let bar = tab_bar_layout("sess", &names, 1, false, width);
            if let Some(pb) = bar.plus {
                assert!(pb.x_end < width, "button overflowed at width {width}");
            }
        }
    }

    #[test]
    fn plus_button_dropped_only_when_bar_too_narrow() {
        let names = vec!["x".to_string()];
        let bar = tab_bar_layout("s", &names, 0, false, 6);
        assert!(
            bar.plus.is_none(),
            "no room for the button after the prefix"
        );
    }

    #[test]
    fn plus_button_hit_true_inside_rect_false_outside() {
        let pb = PlusButton {
            x_start: 15,
            x_end: 17,
        };
        assert!(plus_button_hit(Some(&pb), 15));
        assert!(plus_button_hit(Some(&pb), 16));
        assert!(plus_button_hit(Some(&pb), 17));
        assert!(!plus_button_hit(Some(&pb), 14));
        assert!(!plus_button_hit(Some(&pb), 18));
        assert!(!plus_button_hit(None, 16));
    }

    #[test]
    fn plus_button_and_tab_ranges_never_overlap() {
        let names = vec!["aa".to_string(), "bb".to_string(), "cc".to_string()];
        let bar = tab_bar_layout("s", &names, 0, false, 40);
        let pb = bar.plus.expect("button present");
        for t in &bar.tabs {
            assert!(
                pb.x_start > t.x_end,
                "button x_start {} overlaps tab ending at {}",
                pb.x_start,
                t.x_end
            );
        }
    }

    #[test]
    fn plus_button_anchors_past_prefix_when_no_tabs() {
        // No windows → no tabs drawn; button anchors just past the prefix.
        let bar = tab_bar_layout("sess", &[], 0, false, 40);
        assert!(bar.tabs.is_empty());
        let pb = bar.plus.expect("button present in a wide empty bar");
        // prefix = "sess".len() + 2 = 6; x_start = prefix + PLUS_GAP.
        assert_eq!(pb.x_start, 6 + PLUS_GAP);
    }
}
