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

#[cfg(test)]
mod tests {
    use super::*;

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
