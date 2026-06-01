//! Pure model for copy mode: the frozen-pane scrollback selection state, the
//! vi-style key→action resolver, and SCREEN<->viewport coordinate projection.
//! No I/O, no FFI, no `Session` dependency — driven by `Session`, which owns
//! `Option<CopyModeState>` and applies actions against the pane's screen.
//! Mirrors `src/dialog/mod.rs` so the logic is unit-testable in isolation.

use crate::layout::PaneId;

/// A SCREEN-space coordinate: `y` is the row index from the top of the full
/// screen (scrollback + active), matching libghostty's `SCREEN` point tag.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AbsPoint {
    pub x: u16,
    pub y: usize,
}

/// A linear (character-wise) selection between two SCREEN-space points.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Selection {
    pub anchor: AbsPoint,
    pub head: AbsPoint,
}

impl Selection {
    /// Return the endpoints ordered top-left .. bottom-right (by row then col).
    pub fn normalized(&self) -> (AbsPoint, AbsPoint) {
        let (a, b) = (self.anchor, self.head);
        if (a.y, a.x) <= (b.y, b.x) {
            (a, b)
        } else {
            (b, a)
        }
    }
}

/// A decoded copy-mode key (from the parser).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CopyKey {
    Char(u8),
    Ctrl(u8),
    Up,
    Down,
    Left,
    Right,
    PageUp,
    PageDown,
    Home,
    End,
}

/// A cursor/scroll motion.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Motion {
    Left,
    Right,
    Up,
    Down,
    LineStart,
    LineEnd,
    WordFwd,
    WordBack,
    Top,
    Bottom,
    HalfPageUp,
    HalfPageDown,
    PageUp,
    PageDown,
}

/// The resolved action for a copy-mode key.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CopyAction {
    Move(Motion),
    ToggleSelect,
    Yank,
    Exit,
    None,
}

/// vi-style key → action.
pub fn resolve_copy_key(k: &CopyKey) -> CopyAction {
    use CopyKey as K;
    match k {
        K::Char(b'h') | K::Left => CopyAction::Move(Motion::Left),
        K::Char(b'l') | K::Right => CopyAction::Move(Motion::Right),
        K::Char(b'k') | K::Up => CopyAction::Move(Motion::Up),
        K::Char(b'j') | K::Down => CopyAction::Move(Motion::Down),
        K::Char(b'0') => CopyAction::Move(Motion::LineStart),
        K::Char(b'$') => CopyAction::Move(Motion::LineEnd),
        K::Char(b'w') => CopyAction::Move(Motion::WordFwd),
        K::Char(b'b') => CopyAction::Move(Motion::WordBack),
        K::Char(b'g') => CopyAction::Move(Motion::Top),
        K::Char(b'G') => CopyAction::Move(Motion::Bottom),
        K::Ctrl(0x15) => CopyAction::Move(Motion::HalfPageUp), // C-u
        K::Ctrl(0x04) => CopyAction::Move(Motion::HalfPageDown), // C-d
        K::Ctrl(0x02) | K::PageUp => CopyAction::Move(Motion::PageUp), // C-b / PageUp
        K::Ctrl(0x06) | K::PageDown => CopyAction::Move(Motion::PageDown), // C-f / PageDown
        K::Home => CopyAction::Move(Motion::LineStart),
        K::End => CopyAction::Move(Motion::LineEnd),
        K::Char(b'v') => CopyAction::ToggleSelect,
        K::Char(b'y') | K::Char(b'\r') | K::Char(b'\n') => CopyAction::Yank,
        K::Char(b'q') | K::Char(0x1b) | K::Ctrl(0x1b) => CopyAction::Exit, // q / ESC
        _ => CopyAction::None,
    }
}

/// Copy-mode state: the frozen pane, viewport-relative cursor, scroll offset,
/// optional selection, and whether a mouse drag is in progress.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CopyModeState {
    pub pane: PaneId,
    pub cursor: (u16, u16),
    pub scroll_offset: usize,
    pub selection: Option<Selection>,
    pub dragging: bool,
}

impl CopyModeState {
    pub fn enter(pane: PaneId, cursor: (u16, u16)) -> Self {
        CopyModeState {
            pane,
            cursor,
            scroll_offset: 0,
            selection: None,
            dragging: false,
        }
    }

    /// Toggle selection: start one anchored at `here`, or clear an existing one.
    pub fn toggle_select(&mut self, here: AbsPoint) {
        self.selection = match self.selection {
            Some(_) => None,
            None => Some(Selection {
                anchor: here,
                head: here,
            }),
        };
    }

    /// Extend the selection head (during drag or while selecting with motion).
    pub fn set_head(&mut self, head: AbsPoint) {
        if let Some(sel) = self.selection.as_mut() {
            sel.head = head;
        }
    }

    /// Begin a fresh mouse-drag selection anchored at `here`.
    pub fn begin_drag(&mut self, here: AbsPoint) {
        self.selection = Some(Selection {
            anchor: here,
            head: here,
        });
        self.dragging = true;
    }

    /// Clamp both selection endpoints into the valid screen bounds. Called each
    /// frame before projection so an evicted/out-of-range endpoint shrinks to a
    /// valid cell rather than pointing at wrong text. `total_rows`/`cols` are the
    /// current full-screen dimensions.
    pub fn clamp_to_screen(&mut self, total_rows: usize, cols: u16) {
        let max_y = total_rows.saturating_sub(1);
        let max_x = cols.saturating_sub(1);
        if let Some(sel) = self.selection.as_mut() {
            for p in [&mut sel.anchor, &mut sel.head] {
                p.y = p.y.min(max_y);
                p.x = p.x.min(max_x);
            }
        }
    }
}

/// Project a SCREEN-space point into viewport-relative `(col, row)`, given the
/// current scroll offset (rows up from the live bottom), the viewport height,
/// and the total screen rows. Returns `None` if the point is scrolled out of the
/// visible viewport.
///
/// The visible viewport top in SCREEN-space is
/// `total_rows - viewport_rows - scroll_offset`; row `r` of the viewport maps to
/// SCREEN row `top + r`.
pub fn project(
    abs: AbsPoint,
    scroll_offset: usize,
    viewport_rows: u16,
    total_rows: usize,
) -> Option<(u16, u16)> {
    let vp = viewport_rows as usize;
    let top = total_rows.saturating_sub(vp).saturating_sub(scroll_offset);
    if abs.y < top {
        return None;
    }
    let row = abs.y - top;
    if row >= vp {
        return None;
    }
    Some((abs.x, row as u16))
}

/// Inverse of `project`: viewport-relative `(col, row)` → SCREEN-space point.
pub fn unproject(
    col: u16,
    row: u16,
    scroll_offset: usize,
    viewport_rows: u16,
    total_rows: usize,
) -> AbsPoint {
    let vp = viewport_rows as usize;
    let top = total_rows.saturating_sub(vp).saturating_sub(scroll_offset);
    AbsPoint {
        x: col,
        y: top + row as usize,
    }
}

/// Clip a selection to the visible viewport and project it to viewport-relative
/// `(col, row)` endpoints. Returns `None` if the selection is entirely off-screen
/// (above or below the visible rows).
///
/// The single source of truth shared by rendering (`Session::compose`) and yank
/// (`Session::handle_copy_mode_key`) so the copied text equals the highlighted
/// cells exactly. When an endpoint is above the viewport it clamps to the
/// top-left `(0, 0)`; when below, it clamps to the bottom-right last visible cell
/// `(viewport_cols - 1, viewport_rows - 1)` — matching the inverse highlight which
/// runs to the last column/row of the pane.
pub fn clip_selection(
    sel: &Selection,
    scroll_offset: usize,
    viewport_rows: u16,
    viewport_cols: u16,
    total_rows: usize,
) -> Option<((u16, u16), (u16, u16))> {
    let (a, b) = sel.normalized();
    let vp = viewport_rows as usize;
    let top = total_rows.saturating_sub(vp).saturating_sub(scroll_offset);
    let bottom = top + vp.saturating_sub(1);
    if b.y < top || a.y > bottom {
        return None; // entirely off-screen
    }
    let pa = project(a, scroll_offset, viewport_rows, total_rows).unwrap_or((0, 0));
    let pb = project(b, scroll_offset, viewport_rows, total_rows).unwrap_or((
        viewport_cols.saturating_sub(1),
        viewport_rows.saturating_sub(1),
    ));
    Some((pa, pb))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn vi_keys_resolve_to_expected_actions() {
        assert_eq!(
            resolve_copy_key(&CopyKey::Char(b'j')),
            CopyAction::Move(Motion::Down)
        );
        assert_eq!(resolve_copy_key(&CopyKey::Up), CopyAction::Move(Motion::Up));
        assert_eq!(
            resolve_copy_key(&CopyKey::Char(b'$')),
            CopyAction::Move(Motion::LineEnd)
        );
        assert_eq!(
            resolve_copy_key(&CopyKey::Char(b'G')),
            CopyAction::Move(Motion::Bottom)
        );
        assert_eq!(
            resolve_copy_key(&CopyKey::Ctrl(0x02)),
            CopyAction::Move(Motion::PageUp)
        ); // C-b
        assert_eq!(
            resolve_copy_key(&CopyKey::Ctrl(0x15)),
            CopyAction::Move(Motion::HalfPageUp)
        ); // C-u
        assert_eq!(
            resolve_copy_key(&CopyKey::PageDown),
            CopyAction::Move(Motion::PageDown)
        );
        assert_eq!(
            resolve_copy_key(&CopyKey::Char(b'v')),
            CopyAction::ToggleSelect
        );
        assert_eq!(resolve_copy_key(&CopyKey::Char(b'y')), CopyAction::Yank);
        assert_eq!(resolve_copy_key(&CopyKey::Char(b'\r')), CopyAction::Yank);
        assert_eq!(resolve_copy_key(&CopyKey::Char(b'q')), CopyAction::Exit);
        assert_eq!(resolve_copy_key(&CopyKey::Char(0x1b)), CopyAction::Exit);
        assert_eq!(resolve_copy_key(&CopyKey::Char(b'z')), CopyAction::None);
    }

    #[test]
    fn toggle_select_starts_then_clears() {
        let mut s = CopyModeState::enter(1, (0, 0));
        assert!(s.selection.is_none());
        s.toggle_select(AbsPoint { x: 2, y: 5 });
        assert_eq!(
            s.selection,
            Some(Selection {
                anchor: AbsPoint { x: 2, y: 5 },
                head: AbsPoint { x: 2, y: 5 }
            })
        );
        s.toggle_select(AbsPoint { x: 9, y: 9 });
        assert!(s.selection.is_none());
    }

    #[test]
    fn normalized_orders_endpoints() {
        let sel = Selection {
            anchor: AbsPoint { x: 4, y: 7 },
            head: AbsPoint { x: 1, y: 3 },
        };
        let (a, b) = sel.normalized();
        assert_eq!(a, AbsPoint { x: 1, y: 3 });
        assert_eq!(b, AbsPoint { x: 4, y: 7 });
    }

    #[test]
    fn project_maps_screen_to_viewport_and_clips_offscreen() {
        // total=100 rows, viewport 10 rows, scrolled up by 5 → viewport top = 85.
        assert_eq!(project(AbsPoint { x: 3, y: 85 }, 5, 10, 100), Some((3, 0)));
        assert_eq!(project(AbsPoint { x: 3, y: 94 }, 5, 10, 100), Some((3, 9)));
        assert_eq!(project(AbsPoint { x: 3, y: 84 }, 5, 10, 100), None); // above viewport
        assert_eq!(project(AbsPoint { x: 3, y: 95 }, 5, 10, 100), None); // below viewport
    }

    #[test]
    fn unproject_is_inverse_of_project_at_bottom() {
        let abs = unproject(2, 4, 0, 10, 100);
        assert_eq!(abs, AbsPoint { x: 2, y: 94 });
        assert_eq!(project(abs, 0, 10, 100), Some((2, 4)));
    }

    #[test]
    fn clip_selection_fully_visible_is_unchanged() {
        // total=100, viewport 10 rows/20 cols, unscrolled → top = 90.
        let sel = Selection {
            anchor: AbsPoint { x: 2, y: 92 },
            head: AbsPoint { x: 7, y: 95 },
        };
        assert_eq!(clip_selection(&sel, 0, 10, 20, 100), Some(((2, 2), (7, 5))));
    }

    #[test]
    fn clip_selection_clamps_partial_top_and_bottom_to_viewport_edges() {
        // top = 90, bottom = 99. anchor above (y=80), head below (y=150).
        let sel = Selection {
            anchor: AbsPoint { x: 5, y: 80 },
            head: AbsPoint { x: 9, y: 150 },
        };
        // start clamps to top-left (0,0); end clamps to last visible cell
        // (viewport_cols-1, viewport_rows-1) = (19, 9).
        assert_eq!(
            clip_selection(&sel, 0, 10, 20, 100),
            Some(((0, 0), (19, 9)))
        );
    }

    #[test]
    fn clip_selection_partial_top_only_keeps_visible_end() {
        // anchor above viewport (y=80 < top=90), head visible at y=93.
        let sel = Selection {
            anchor: AbsPoint { x: 5, y: 80 },
            head: AbsPoint { x: 4, y: 93 },
        };
        assert_eq!(clip_selection(&sel, 0, 10, 20, 100), Some(((0, 0), (4, 3))));
    }

    #[test]
    fn clip_selection_fully_offscreen_is_none() {
        // Whole selection above the viewport (top=90).
        let sel = Selection {
            anchor: AbsPoint { x: 0, y: 10 },
            head: AbsPoint { x: 4, y: 20 },
        };
        assert_eq!(clip_selection(&sel, 0, 10, 20, 100), None);
        // Whole selection below the viewport.
        let sel2 = Selection {
            anchor: AbsPoint { x: 0, y: 200 },
            head: AbsPoint { x: 4, y: 210 },
        };
        assert_eq!(clip_selection(&sel2, 0, 10, 20, 100), None);
    }

    #[test]
    fn clamp_shrinks_evicted_endpoint_to_valid_range() {
        let mut s = CopyModeState::enter(1, (0, 0));
        s.selection = Some(Selection {
            anchor: AbsPoint { x: 99, y: 999 },
            head: AbsPoint { x: 0, y: 0 },
        });
        s.clamp_to_screen(/*total_rows*/ 50, /*cols*/ 10);
        let sel = s.selection.unwrap();
        assert_eq!(sel.anchor, AbsPoint { x: 9, y: 49 });
        assert_eq!(sel.head, AbsPoint { x: 0, y: 0 });
    }
}
