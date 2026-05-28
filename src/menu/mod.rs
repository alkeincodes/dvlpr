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
    MenuItem { label: "Split Vertically",   command: Command::SplitVertical   },
    MenuItem { label: "Split Horizontally", command: Command::SplitHorizontal },
    MenuItem { label: "Zoom",               command: Command::ToggleZoom      },
    MenuItem { label: "Exit",               command: Command::ClosePane       },
];

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
}
