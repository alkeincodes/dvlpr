//! Pane right-click context menu state, items, geometry, and hit-test.
//! See `docs/superpowers/specs/2026-05-29-pane-right-click-menu-design.md`.
//! Pure module — no I/O, no async. Driven entirely by `Session`.

use crate::config::Command;
use crate::layout::PaneId;

/// What the menu is anchored to. v1 only ever populates `Pane`; the enum
/// exists so adding tab/sidebar menus later does not reshape the field on
/// `Session`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MenuKind {
    Pane { pane_id: PaneId },
}

/// One menu row: a static label and the `Command` it dispatches via
/// `Session::apply_command` when the user clicks or hits Enter on it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MenuItem {
    pub label: &'static str,
    pub command: Command,
}

/// Open-menu state. `Session.menu: Option<MenuState>` holds at most one
/// across the whole session and all attached clients.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MenuState {
    pub kind: MenuKind,
    /// 1-based (col, row) of the click that opened the menu — same coord
    /// system as `crate::input::MouseEvent` and `Session::hit`.
    pub anchor: (u16, u16),
    /// Index into `items()` of the currently highlighted row. Initial
    /// value is 0 (the topmost item, `Split Vertically`).
    pub highlighted: usize,
}

impl MenuState {
    /// The static item table for this menu's `kind`. v1 only the Pane case
    /// is populated.
    pub fn items(&self) -> &'static [MenuItem] {
        match self.kind {
            MenuKind::Pane { .. } => PANE_MENU_ITEMS,
        }
    }

    /// Move the highlight up one row, wrapping at the top.
    pub fn move_up(&mut self) {
        let n = self.items().len();
        self.highlighted = (self.highlighted + n - 1) % n;
    }

    /// Move the highlight down one row, wrapping at the bottom.
    pub fn move_down(&mut self) {
        let n = self.items().len();
        self.highlighted = (self.highlighted + 1) % n;
    }
}

/// The v1 pane-menu items. Order is render order; index 0 is the default
/// initial highlight.
pub const PANE_MENU_ITEMS: &[MenuItem] = &[
    MenuItem {
        label: "Split Vertically",
        command: Command::SplitVertical,
    },
    MenuItem {
        label: "Split Horizontally",
        command: Command::SplitHorizontal,
    },
    MenuItem {
        label: "Zoom",
        command: Command::ToggleZoom,
    },
    MenuItem {
        label: "Exit",
        command: Command::ClosePane,
    },
];

use crate::layout::Rect;

/// 1 cell of horizontal pad on each side of the label, plus 1 border cell
/// on each side. Total width contribution beyond `label_w` is 4.
const MENU_PAD_X: u16 = 1;
const MENU_BORDER: u16 = 1;

/// The horizontal pad cell count on each side of an item label. Exposed
/// so `compositor::draw_menu` can reproduce the natural unclipped rect
/// for cell→item mapping. Kept as a function (not a `pub const`) so it
/// behaves identically across crate boundaries with no `const` import.
pub fn menu_pad_x() -> u16 {
    MENU_PAD_X
}

/// The border cell count on each side. See `menu_pad_x` for rationale.
pub fn menu_border() -> u16 {
    MENU_BORDER
}

/// Resolve the menu's paint rect given the click anchor, the available
/// `content_area`, and the menu's item count + label width. The algorithm:
///
/// 1. Start with top-left at the 0-based anchor cell.
/// 2. Shift left if the natural right edge overflows `content_area.right`.
/// 3. Flip up (anchor becomes bottom-left) if the natural bottom edge
///    overflows `content_area.bottom`.
/// 4. Hard-clip to `content_area` on both axes. If the content area is
///    smaller than the natural menu, the rect ends up smaller — the
///    caller is responsible for truncating cells gracefully.
///
/// The returned rect is always fully inside `content_area`.
pub fn menu_rect(anchor: (u16, u16), content_area: Rect, items_len: usize, label_w: u16) -> Rect {
    let w = label_w
        .saturating_add(2 * MENU_PAD_X)
        .saturating_add(2 * MENU_BORDER);
    let h = (items_len as u16).saturating_add(2 * MENU_BORDER);

    let ax = anchor.0.saturating_sub(1);
    let ay = anchor.1.saturating_sub(1);
    let mut tlx = ax;
    let mut tly = ay;

    let right = content_area
        .x
        .saturating_add(content_area.w.saturating_sub(1));
    let bottom = content_area
        .y
        .saturating_add(content_area.h.saturating_sub(1));

    if tlx.saturating_add(w.saturating_sub(1)) > right {
        tlx = right
            .saturating_sub(w.saturating_sub(1))
            .max(content_area.x);
    }
    if tly.saturating_add(h.saturating_sub(1)) > bottom {
        tly = ay.saturating_sub(h.saturating_sub(1)).max(content_area.y);
    }

    let x0 = tlx.max(content_area.x);
    let y0 = tly.max(content_area.y);
    let x1 = tlx.saturating_add(w.saturating_sub(1)).min(right);
    let y1 = tly.saturating_add(h.saturating_sub(1)).min(bottom);

    Rect {
        x: x0,
        y: y0,
        w: x1.saturating_sub(x0).saturating_add(1),
        h: y1.saturating_sub(y0).saturating_add(1),
    }
}

/// Result of `menu_hit`: which menu region a click landed in.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MenuHit {
    /// The interior cell of item index `usize` — execute on press, hover
    /// on motion.
    Item(usize),
    /// A border cell (corner or edge). Swallow without execute or close.
    Border,
    /// Outside the resolved rect entirely. Close-on-press.
    Outside,
}

/// Classify a 1-based `(col, row)` click against the open menu. Recomputes
/// `menu_rect` internally so callers don't need to.
pub fn menu_hit(
    menu: &MenuState,
    items_len: usize,
    label_w: u16,
    content_area: Rect,
    col: u16,
    row: u16,
) -> MenuHit {
    let rect = menu_rect(menu.anchor, content_area, items_len, label_w);
    let x = col.saturating_sub(1);
    let y = row.saturating_sub(1);

    // Outside the rect.
    if x < rect.x || x >= rect.x + rect.w || y < rect.y || y >= rect.y + rect.h {
        return MenuHit::Outside;
    }

    // Border cells: outer ring of the rect.
    let is_left = x == rect.x;
    let is_right = x == rect.x + rect.w - 1;
    let is_top = y == rect.y;
    let is_bottom = y == rect.y + rect.h - 1;
    if is_left || is_right || is_top || is_bottom {
        return MenuHit::Border;
    }

    // Interior: y - rect.y - 1 indexes into `items`.
    let i = (y - rect.y - 1) as usize;
    if i < items_len {
        MenuHit::Item(i)
    } else {
        // Shouldn't happen if rect.h matches items_len + 2, but guard against
        // clipped rects where the visible item rows are fewer than items_len.
        MenuHit::Border
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Command;

    #[test]
    fn pane_kind_items_are_split_v_split_h_zoom_exit_in_order() {
        let menu = MenuState {
            kind: MenuKind::Pane { pane_id: 1 },
            anchor: (10, 5),
            highlighted: 0,
        };
        let items = menu.items();
        assert_eq!(items.len(), 4);
        assert_eq!(items[0].label, "Split Vertically");
        assert_eq!(items[0].command, Command::SplitVertical);
        assert_eq!(items[1].label, "Split Horizontally");
        assert_eq!(items[1].command, Command::SplitHorizontal);
        assert_eq!(items[2].label, "Zoom");
        assert_eq!(items[2].command, Command::ToggleZoom);
        assert_eq!(items[3].label, "Exit");
        assert_eq!(items[3].command, Command::ClosePane);
    }

    #[test]
    fn new_menu_starts_with_highlighted_zero() {
        let menu = MenuState {
            kind: MenuKind::Pane { pane_id: 7 },
            anchor: (1, 1),
            highlighted: 0,
        };
        assert_eq!(menu.highlighted, 0);
    }

    #[test]
    fn menu_kind_pane_carries_pane_id() {
        let menu = MenuState {
            kind: MenuKind::Pane { pane_id: 42 },
            anchor: (1, 1),
            highlighted: 0,
        };
        match menu.kind {
            MenuKind::Pane { pane_id } => assert_eq!(pane_id, 42),
        }
    }

    #[test]
    fn arrow_down_wraps_from_last_to_first() {
        let mut menu = MenuState {
            kind: MenuKind::Pane { pane_id: 1 },
            anchor: (1, 1),
            highlighted: 3, // last item index for 4-item list
        };
        menu.move_down();
        assert_eq!(menu.highlighted, 0);
    }

    #[test]
    fn arrow_up_wraps_from_first_to_last() {
        let mut menu = MenuState {
            kind: MenuKind::Pane { pane_id: 1 },
            anchor: (1, 1),
            highlighted: 0,
        };
        menu.move_up();
        assert_eq!(menu.highlighted, 3);
    }

    #[test]
    fn arrow_down_moves_one_step_from_middle() {
        let mut menu = MenuState {
            kind: MenuKind::Pane { pane_id: 1 },
            anchor: (1, 1),
            highlighted: 1,
        };
        menu.move_down();
        assert_eq!(menu.highlighted, 2);
    }

    #[test]
    fn arrow_up_moves_one_step_from_middle() {
        let mut menu = MenuState {
            kind: MenuKind::Pane { pane_id: 1 },
            anchor: (1, 1),
            highlighted: 2,
        };
        menu.move_up();
        assert_eq!(menu.highlighted, 1);
    }

    use crate::layout::Rect;

    fn area(x: u16, y: u16, w: u16, h: u16) -> Rect {
        Rect { x, y, w, h }
    }

    #[test]
    fn menu_rect_no_clip_when_anchor_fits() {
        let r = menu_rect((10, 5), area(0, 0, 80, 24), 4, 18);
        assert_eq!(
            r,
            Rect {
                x: 9,
                y: 4,
                w: 22,
                h: 6
            }
        );
    }

    #[test]
    fn menu_rect_shifts_left_when_overflowing_right() {
        let r = menu_rect((75, 5), area(0, 0, 80, 24), 4, 18);
        assert_eq!(r.x, 58);
        assert_eq!(r.w, 22);
        assert_eq!(r.x + r.w - 1, 79);
    }

    #[test]
    fn menu_rect_flips_up_when_overflowing_bottom() {
        let r = menu_rect((10, 22), area(0, 0, 80, 24), 4, 18);
        assert_eq!(r.x, 9);
        assert_eq!(r.y, 16);
        assert_eq!(r.h, 6);
        assert_eq!(r.y + r.h - 1, 21);
    }

    #[test]
    fn menu_rect_shifts_and_flips_when_both_edges_overflow() {
        let r = menu_rect((75, 22), area(0, 0, 80, 24), 4, 18);
        assert_eq!(r.x, 58);
        assert_eq!(r.y, 16);
        assert_eq!(r.w, 22);
        assert_eq!(r.h, 6);
    }

    #[test]
    fn menu_rect_clamps_to_content_area_when_tiny() {
        let r = menu_rect((1, 1), area(0, 0, 10, 4), 4, 18);
        assert!(r.w <= 10);
        assert!(r.h <= 4);
        assert!(r.x + r.w <= 10);
        assert!(r.y + r.h <= 4);
    }

    #[test]
    fn menu_rect_never_overlaps_status_bar_or_sidebar() {
        let content = area(20, 0, 60, 23);
        let r = menu_rect((79, 23), content, 4, 18);
        assert!(r.x >= content.x);
        assert!(r.y >= content.y);
        assert!(r.x + r.w <= content.x + content.w);
        assert!(r.y + r.h <= content.y + content.h);
    }

    #[test]
    fn menu_rect_respects_content_area_left_origin() {
        let r = menu_rect((6, 3), area(5, 2, 30, 10), 4, 18);
        assert_eq!(r.x, 5);
        assert_eq!(r.y, 2);
    }

    #[test]
    fn menu_hit_returns_item_for_interior_row() {
        let menu = MenuState {
            kind: MenuKind::Pane { pane_id: 1 },
            anchor: (10, 5),
            highlighted: 0,
        };
        let content = area(0, 0, 80, 24);
        let label_w = 18;
        let r = menu_rect(menu.anchor, content, 4, label_w);

        // Item 0: row r.y + 1, interior column.
        let col_1based = (r.x + 5) + 1;
        let row_1based = (r.y + 1) + 1;
        assert_eq!(
            menu_hit(&menu, 4, label_w, content, col_1based, row_1based),
            MenuHit::Item(0)
        );

        // Item 3: row r.y + 4.
        let row_1based = (r.y + 4) + 1;
        assert_eq!(
            menu_hit(&menu, 4, label_w, content, col_1based, row_1based),
            MenuHit::Item(3)
        );
    }

    #[test]
    fn menu_hit_returns_border_for_chrome_cells() {
        let menu = MenuState {
            kind: MenuKind::Pane { pane_id: 1 },
            anchor: (10, 5),
            highlighted: 0,
        };
        let content = area(0, 0, 80, 24);
        let r = menu_rect(menu.anchor, content, 4, 18);

        // Top-left corner.
        assert_eq!(
            menu_hit(&menu, 4, 18, content, r.x + 1, r.y + 1),
            MenuHit::Border
        );
        // Right border, interior row.
        assert_eq!(
            menu_hit(&menu, 4, 18, content, r.x + r.w - 1 + 1, r.y + 2 + 1),
            MenuHit::Border
        );
        // Bottom-right corner.
        assert_eq!(
            menu_hit(&menu, 4, 18, content, r.x + r.w - 1 + 1, r.y + r.h - 1 + 1),
            MenuHit::Border
        );
    }

    #[test]
    fn menu_hit_returns_outside_for_cells_past_rect() {
        let menu = MenuState {
            kind: MenuKind::Pane { pane_id: 1 },
            anchor: (10, 5),
            highlighted: 0,
        };
        let content = area(0, 0, 80, 24);
        let r = menu_rect(menu.anchor, content, 4, 18);

        // One column right of the rect.
        assert_eq!(
            menu_hit(&menu, 4, 18, content, r.x + r.w + 1, r.y + 1 + 1),
            MenuHit::Outside
        );
        // One row below the rect.
        assert_eq!(
            menu_hit(&menu, 4, 18, content, r.x + 1, r.y + r.h + 1),
            MenuHit::Outside
        );
        // Origin (1, 1) when menu is anchored far away.
        assert_eq!(menu_hit(&menu, 4, 18, content, 1, 1), MenuHit::Outside);
    }

    #[test]
    fn menu_hit_after_flip_resolves_correct_item() {
        let menu = MenuState {
            kind: MenuKind::Pane { pane_id: 1 },
            anchor: (10, 22),
            highlighted: 0,
        };
        let content = area(0, 0, 80, 24);
        let r = menu_rect(menu.anchor, content, 4, 18);
        assert_eq!(r.y + r.h - 1, 21);

        let row_1based = (r.y + 4) + 1;
        let col_1based = (r.x + 2) + 1;
        assert_eq!(
            menu_hit(&menu, 4, 18, content, col_1based, row_1based),
            MenuHit::Item(3)
        );
    }
}
