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

/// What a mouse click landed on.
#[derive(Clone, Debug, PartialEq)]
pub enum Hit {
    Pane(PaneId),
    Divider(SplitPath),
    Tab(usize),
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

/// Lay out window tabs left-to-right: each label is `[<idx><marker><name>]`
/// where marker is `*` for the active window else `:`, names truncated to
/// TAB_NAME_MAX cells, separated by a single space. Stops once `width` is reached.
///
/// v1 semantics: lengths/offsets are counted in Unicode scalar values
/// (`chars().count()`), which equals terminal display cells for the ASCII window
/// names we expect. Wide/zero-width characters (CJK, emoji) would make the drawn
/// label and the clickable x-range drift; supporting display-width is deferred
/// (would need a unicode-width dependency, out of scope for this std-only module).
pub fn tab_layout(names: &[String], active: usize, width: u16) -> Vec<TabRegion> {
    let mut out = Vec::new();
    let mut x: u16 = 0;
    for (i, name) in names.iter().enumerate() {
        if x >= width {
            break;
        }
        let marker = if i == active { '*' } else { ':' };
        let label = format!("[{}{}{}]", i, marker, truncate(name, TAB_NAME_MAX));
        let len = label.chars().count() as u16;
        let x_end = x.saturating_add(len.saturating_sub(1)).min(width - 1);
        out.push(TabRegion {
            window: i,
            x_start: x,
            x_end,
            label,
        });
        x = x.saturating_add(len).saturating_add(1); // one-space separator
    }
    out
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

/// Hit-test a 1-based SGR mouse coordinate against the active window's geometry.
/// Tab row is checked first, then dividers (a click exactly on a divider means
/// "resize"), then panes. Returns `Hit::None` for clicks outside everything.
pub fn hit_test(
    node: &Node,
    viewport: Rect,
    window_count: usize,
    tabs: &[TabRegion],
    col: u16,
    row: u16,
) -> Hit {
    // SGR coordinates are 1-based; 0 is invalid. Convert to 0-based.
    if col == 0 || row == 0 {
        return Hit::None;
    }
    let x = col - 1;
    let y = row - 1;

    if let Some(ty) = tab_row(viewport, window_count) {
        if y == ty {
            for t in tabs {
                if x >= t.x_start && x <= t.x_end {
                    return Hit::Tab(t.window);
                }
            }
            return Hit::None;
        }
    }

    let content = content_area(viewport, window_count);
    for d in dividers(node, content) {
        if d.rect.contains(x, y) {
            return Hit::Divider(d.path);
        }
    }
    for (id, r) in pane_rects(node, content) {
        if r.contains(x, y) {
            return Hit::Pane(id);
        }
    }
    Hit::None
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
        let tabs = tab_layout(&names, 0, 80);
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
    fn tab_layout_produces_labels_and_ranges() {
        let names = vec!["shell".to_string(), "vim".to_string(), "logs".to_string()];
        let tabs = tab_layout(&names, 1, 80);
        // Labels: "[0:shell]" (9), space, "[1*vim]" (7), space, "[2:logs]" (8).
        assert_eq!(tabs.len(), 3);
        assert_eq!(tabs[0].label, "[0:shell]");
        assert_eq!(tabs[0].x_start, 0);
        assert_eq!(tabs[0].x_end, 8); // 9 chars at x 0..=8
        assert_eq!(tabs[1].label, "[1*vim]"); // active marked with '*'
        assert_eq!(tabs[1].x_start, 10); // 9 + 1 space
        assert_eq!(tabs[1].x_end, 16); // 7 chars at 10..=16
        assert_eq!(tabs[2].label, "[2:logs]");
        assert_eq!(tabs[2].x_start, 18); // 16 + 1 + 1
    }

    #[test]
    fn tab_layout_truncates_long_names() {
        let names = vec!["a-very-long-window-name".to_string()];
        let tabs = tab_layout(&names, 0, 80);
        // name truncated to 12 cells: first 11 chars ("a-very-long") + '…'.
        assert_eq!(tabs[0].label, "[0*a-very-long…]");
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
}
