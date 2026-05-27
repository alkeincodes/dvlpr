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
        x >= self.x && x < self.x.saturating_add(self.w) && y >= self.y && y < self.y.saturating_add(self.h)
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

/// The y of the 1-row tab bar (bottom of the viewport), or `None` when there is
/// only one window (single-window sessions use the full viewport — no tab bar).
pub fn tab_row(viewport: Rect, window_count: usize) -> Option<u16> {
    if window_count > 1 && viewport.h > 0 {
        Some(viewport.y + viewport.h - 1)
    } else {
        None
    }
}

/// The content rect (viewport minus the tab-bar row when there is more than one window).
pub fn content_area(viewport: Rect, window_count: usize) -> Rect {
    if tab_row(viewport, window_count).is_some() {
        Rect { h: viewport.h - 1, ..viewport }
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
            dir, ratio, first, second, ..
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
            let first = Rect { x: area.x, y: area.y, w: area.w, h: first_h };
            let divider = Rect { x: area.x, y: area.y + first_h, w: area.w, h: divider_h };
            let second = Rect { x: area.x, y: area.y + first_h + divider_h, w: area.w, h: second_h };
            (first, divider, second)
        }
        SplitDir::Vertical => {
            let divider_w: u16 = if area.w >= 1 { 1 } else { 0 };
            let avail = area.w - divider_w;
            let first_w = ((ratio * avail as f32).floor() as i64).clamp(0, avail as i64) as u16;
            let second_w = avail - first_w;
            let first = Rect { x: area.x, y: area.y, w: first_w, h: area.h };
            let divider = Rect { x: area.x + first_w, y: area.y, w: divider_w, h: area.h };
            let second = Rect { x: area.x + first_w + divider_w, y: area.y, w: second_w, h: area.h };
            (first, divider, second)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn single_window_has_no_tab_row_and_full_content() {
        let vp = Rect { x: 0, y: 0, w: 80, h: 24 };
        assert_eq!(tab_row(vp, 1), None);
        assert_eq!(content_area(vp, 1), vp);
    }

    #[test]
    fn multi_window_reserves_bottom_row_for_tabs() {
        let vp = Rect { x: 0, y: 0, w: 80, h: 24 };
        assert_eq!(tab_row(vp, 3), Some(23)); // bottom row
        assert_eq!(content_area(vp, 3), Rect { x: 0, y: 0, w: 80, h: 23 });
    }

    #[test]
    fn single_leaf_fills_the_area() {
        let tree = Node::Leaf(7);
        let area = Rect { x: 0, y: 0, w: 80, h: 24 };
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
        let area = Rect { x: 0, y: 0, w: 10, h: 5 };
        // avail = 5 - 1 = 4; first_h = floor(0.5*4)=2; second_h = 2.
        let rects = pane_rects(&tree, area);
        assert_eq!(rects[0], (1, Rect { x: 0, y: 0, w: 10, h: 2 }));
        assert_eq!(rects[1], (2, Rect { x: 0, y: 3, w: 10, h: 2 })); // y = 0 + 2 + 1 divider
    }

    #[test]
    fn vertical_split_places_side_by_side_with_divider() {
        let tree = Node::Split {
            dir: SplitDir::Vertical,
            ratio: 0.5,
            first: Box::new(Node::Leaf(1)),
            second: Box::new(Node::Leaf(2)),
        };
        let area = Rect { x: 0, y: 0, w: 11, h: 4 };
        // avail = 11 - 1 = 10; first_w = 5; second_w = 5.
        let rects = pane_rects(&tree, area);
        assert_eq!(rects[0], (1, Rect { x: 0, y: 0, w: 5, h: 4 }));
        assert_eq!(rects[1], (2, Rect { x: 6, y: 0, w: 5, h: 4 })); // x = 0 + 5 + 1 divider
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
        let area = Rect { x: 0, y: 0, w: 21, h: 5 };
        let rects = pane_rects(&tree, area);
        // Outer V: avail_w=20, first_w=10 (x 0..10), divider x=10, second x=11 w=10.
        // Left H: area {0,0,10,5}: avail_h=4, first_h=2, second y=3 h=2.
        assert_eq!(rects[0], (1, Rect { x: 0, y: 0, w: 10, h: 2 }));
        assert_eq!(rects[1], (2, Rect { x: 0, y: 3, w: 10, h: 2 }));
        assert_eq!(rects[2], (3, Rect { x: 11, y: 0, w: 10, h: 5 }));
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
        let area = Rect { x: 4, y: 2, w: 13, h: 7 };
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
        let area = Rect { x: 0, y: 0, w: 10, h: 0 };
        let rects = pane_rects(&tree, area);
        // Both panes are zero-height; their heights sum to exactly 0 (no overflow).
        assert_eq!(rects[0].1.h, 0);
        assert_eq!(rects[1].1.h, 0);
        assert_eq!(rects[0].1.h + rects[1].1.h, area.h);
    }

    #[test]
    fn rect_contains_checks_bounds() {
        let r = Rect { x: 2, y: 3, w: 4, h: 5 };
        assert!(r.contains(2, 3)); // top-left corner included
        assert!(r.contains(5, 7)); // bottom-right corner included (x<2+4, y<3+5)
        assert!(!r.contains(1, 3)); // left of
        assert!(!r.contains(6, 3)); // right edge excluded (2+4=6)
        assert!(!r.contains(2, 8)); // bottom edge excluded (3+5=8)
        let empty = Rect { x: 0, y: 0, w: 0, h: 0 };
        assert!(!empty.contains(0, 0)); // zero-size contains nothing
    }
}
