//! Help overlay state, data, geometry, and hit-test. See
//! `docs/superpowers/specs/2026-05-29-help-popup-dialog-design.md`.
//! Pure module — no I/O, no async. Driven entirely by `Session`.

use crate::config::{KeyMap, KeySpec, NamedKey};
use crate::layout::Rect;

/// Which tab is showing. `Keybindings` is the default on open.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HelpTab {
    Keybindings,
    Commands,
}

impl HelpTab {
    pub fn next(self) -> Self {
        match self {
            HelpTab::Keybindings => HelpTab::Commands,
            HelpTab::Commands => HelpTab::Keybindings,
        }
    }
    /// Two tabs: prev == next.
    pub fn prev(self) -> Self {
        self.next()
    }
    pub fn index(self) -> usize {
        match self {
            HelpTab::Keybindings => 0,
            HelpTab::Commands => 1,
        }
    }
    /// Maps 1 → Commands; any other index (including out-of-range) → Keybindings.
    pub fn from_index(i: usize) -> Self {
        if i == 1 {
            HelpTab::Commands
        } else {
            HelpTab::Keybindings
        }
    }
}

/// Open-overlay state. `Session.help: Option<HelpState>` holds at most one,
/// shared across all attached clients.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct HelpState {
    pub tab: HelpTab,
    /// First visible body-row index for the active tab. Reset to 0 on tab
    /// switch; clamped to `max_scroll` on every scroll and at paint time.
    pub scroll: u16,
}

impl Default for HelpState {
    fn default() -> Self {
        HelpState {
            tab: HelpTab::Keybindings,
            scroll: 0,
        }
    }
}

/// One render-ready row: a left "keys"/"invocation" column and a right explainer.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HelpRow {
    pub keys: String,
    pub desc: String,
}

/// A per-frame render snapshot built only while help is open.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HelpView {
    pub tab: HelpTab,
    pub scroll: u16,
    pub keybindings: Vec<HelpRow>,
    pub commands: Vec<HelpRow>,
}

impl HelpView {
    /// Rows of the active tab (what the renderer and scroll math operate on).
    pub fn active_rows(&self) -> &[HelpRow] {
        match self.tab {
            HelpTab::Keybindings => &self.keybindings,
            HelpTab::Commands => &self.commands,
        }
    }
}

/// Format a key binding for display: `Ctrl('b') => "C-b"`, `Char('x') => "x"`,
/// arrows => glyphs.
pub fn render_keyspec(k: &KeySpec) -> String {
    match k {
        KeySpec::Ctrl(c) => format!("C-{c}"),
        KeySpec::Char(c) => c.to_string(),
        KeySpec::Named(NamedKey::Up) => "↑".to_string(),
        KeySpec::Named(NamedKey::Down) => "↓".to_string(),
        KeySpec::Named(NamedKey::Left) => "←".to_string(),
        KeySpec::Named(NamedKey::Right) => "→".to_string(),
    }
}

/// The static `dvlpr` CLI surface. Source of truth: `src/main.rs`'s argv parser.
/// If you add/rename a subcommand there, update this table (and its coverage
/// test in this module).
pub const COMMAND_ROWS: &[(&str, &str)] = &[
    ("dvlpr", "Create or attach to the 'default' session"),
    ("dvlpr <name>", "Create or attach to session <name>"),
    (
        "dvlpr new -s <name>",
        "Create or attach to <name> (explicit form)",
    ),
    (
        "dvlpr attach -t <name>",
        "Attach to an existing session; error if missing (alias: a)",
    ),
    ("dvlpr ls", "List live sessions with window counts"),
    (
        "dvlpr kill -t <name>",
        "Kill a session's daemon; error if missing",
    ),
    (
        "dvlpr ssh <dest> [name]",
        "SSH to <dest> and run dvlpr there",
    ),
    (
        "dvlpr server [name]",
        "Internal daemon entrypoint (spawned for you)",
    ),
    (
        "dvlpr update",
        "fetch the latest release and replace this binary",
    ),
    (
        "dvlpr --version / -V",
        "print the version and build target triple",
    ),
];

/// The `-h` / `--help` line, appended after `COMMAND_ROWS` in the CLI help.
/// Kept here (not in `COMMAND_ROWS`) so it shows in the terminal `--help` output
/// without leaking into the in-app Commands tab, which lists session subcommands.
const HELP_FLAG_ROW: (&str, &str) = ("dvlpr -h, --help", "Show this help and exit");

/// Render the full `dvlpr --help` text printed to stdout. Reuses `COMMAND_ROWS`
/// (the same table the in-app Commands tab uses) so the CLI and overlay never
/// drift, then appends the `-h/--help` line. Columns are aligned to the widest
/// command. Pure: returns the string, performs no I/O.
pub fn cli_help() -> String {
    let rows: Vec<(&str, &str)> = COMMAND_ROWS
        .iter()
        .copied()
        .chain(std::iter::once(HELP_FLAG_ROW))
        .collect();
    let width = rows.iter().map(|(k, _)| k.len()).max().unwrap_or(0);
    let mut out = String::new();
    out.push_str("dvlpr — a lightweight agent-aware terminal multiplexer\n\n");
    out.push_str("commands:\n");
    for (cmd, desc) in rows {
        out.push_str(&format!("  {cmd:<width$}  {desc}\n"));
    }
    out
}

/// Build a render-ready view from the open state plus the live prefix + keymap.
/// Keybinding chords are rendered from `prefix`/`keys` so a user's rebinds show.
pub fn build_view(state: &HelpState, prefix: KeySpec, keys: &KeyMap) -> HelpView {
    let p = render_keyspec(&prefix);
    let row = |k: &KeySpec, desc: &str| HelpRow {
        keys: format!("{p} {}", render_keyspec(k)),
        desc: desc.to_string(),
    };
    let lit = |suffix: &str, desc: &str| HelpRow {
        keys: format!("{p} {suffix}"),
        desc: desc.to_string(),
    };
    let keybindings = vec![
        row(&keys.split_vertical, "Split the focused pane left / right"),
        row(
            &keys.split_horizontal,
            "Split the focused pane top / bottom",
        ),
        row(&keys.close_pane, "Close the focused pane"),
        row(&keys.new_window, "Open the New Window dialog"),
        row(&keys.next_window, "Switch to the next window"),
        row(&keys.prev_window, "Switch to the previous window"),
        row(&keys.detach, "Detach from the session (leave it running)"),
        row(&keys.help, "Show / hide this help"),
        row(&keys.toggle_sidebar, "Toggle the agent-awareness sidebar"),
        // Implicit parser-level bindings (src/input/mod.rs::resolve_key). These
        // literals MUST stay in sync with that function.
        lit("0", "Toggle zoom (fullscreen the focused pane)"),
        lit("1-9", "Jump to window by number"),
    ];
    let commands = COMMAND_ROWS
        .iter()
        .map(|(k, d)| HelpRow {
            keys: (*k).to_string(),
            desc: (*d).to_string(),
        })
        .collect();
    HelpView {
        tab: state.tab,
        scroll: state.scroll,
        keybindings,
        commands,
    }
}

/// Fixed chrome rows: top border + tab header + separator + footer + bottom.
pub const HELP_CHROME_ROWS: u16 = 5;
/// Maximum overlay width; clamped down to `content.w` when narrower.
pub const HELP_MAX_W: u16 = 72;
/// Smallest legible box width: 2 border cols + interior room for the tab
/// header chips and a usable two-column body. Below this, draw_help bails.
pub const HELP_MIN_W: u16 = 24;

/// Centered, strictly-clamped overlay rect. NO `.max(1)` floor — the rect is
/// always inside `content_area`, even for a degenerate (h == 0 / w == 0) area.
/// A too-small area yields a rect rejected by `help_renderable`.
pub fn help_rect(content: Rect, body_rows: usize) -> Rect {
    let desired_h = HELP_CHROME_ROWS.saturating_add(body_rows as u16);
    let h = desired_h.min(content.h);
    let w = content.w.min(HELP_MAX_W);
    let x = content.x + content.w.saturating_sub(w) / 2;
    let y = content.y + content.h.saturating_sub(h) / 2;
    Rect { x, y, w, h }
}

/// True iff the rect can paint chrome + body safely. Guards every
/// `rect.y + rect.h - 2` / `- 1` index and the body window.
pub fn help_renderable(rect: Rect) -> bool {
    rect.h >= HELP_CHROME_ROWS && rect.w >= HELP_MIN_W
}

/// Number of body rows visible inside the rect (0 when too short).
pub fn visible_body_rows(rect: Rect) -> u16 {
    rect.h.saturating_sub(HELP_CHROME_ROWS)
}

/// Largest valid scroll offset for `rows_len` rows in `rect`.
pub fn max_scroll(rows_len: usize, rect: Rect) -> u16 {
    let visible = visible_body_rows(rect) as usize;
    rows_len.saturating_sub(visible).min(u16::MAX as usize) as u16
}

/// The two tab labels, in display order. Single source of truth for the tab
/// header (renderer + hit-test).
pub const HELP_TAB_LABELS: [(HelpTab, &str); 2] = [
    (HelpTab::Keybindings, "Keybindings"),
    (HelpTab::Commands, "Commands"),
];

const HELP_TAB_GAP: u16 = 2; // blank cells between adjacent tab chips
const HELP_TAB_LEFT_PAD: u16 = 1; // interior pad before the first chip

/// A tab chip's clickable x-range (inclusive), 0-based, including a 1-cell pad
/// on each side of the label. The renderer paints the highlight over exactly
/// this range; `help_hit` tests against it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct HelpTabRegion {
    pub tab: HelpTab,
    pub x_start: u16,
    pub x_end: u16,
}

/// Lay out the tab chips inside `rect`'s header row.
pub fn tab_regions(rect: Rect) -> Vec<HelpTabRegion> {
    let mut out = Vec::new();
    // Interior begins at rect.x + 1 (after the left border).
    let mut x = rect.x.saturating_add(1).saturating_add(HELP_TAB_LEFT_PAD);
    for (tab, label) in HELP_TAB_LABELS {
        let label_w = label.chars().count() as u16;
        let chip_w = label_w.saturating_add(2); // 1 pad + label + 1 pad
        let x_start = x;
        let x_end = x_start.saturating_add(chip_w).saturating_sub(1);
        out.push(HelpTabRegion {
            tab,
            x_start,
            x_end,
        });
        x = x_end.saturating_add(1).saturating_add(HELP_TAB_GAP);
    }
    out
}

/// Result of `help_hit`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HelpHit {
    Tab(usize),
    Body,
    Outside,
}

/// Classify a 1-based `(col, row)` click against the open overlay.
pub fn help_hit(view: &HelpView, content_area: Rect, col: u16, row: u16) -> HelpHit {
    let rect = help_rect(content_area, view.active_rows().len());
    if !help_renderable(rect) {
        // Not shown → swallow clicks (don't close an invisible overlay).
        return HelpHit::Body;
    }
    let x = col.saturating_sub(1);
    let y = row.saturating_sub(1);
    if !rect.contains(x, y) {
        return HelpHit::Outside;
    }
    if y == rect.y + 1 {
        for r in tab_regions(rect) {
            if x >= r.x_start && x <= r.x_end {
                return HelpHit::Tab(r.tab.index());
            }
        }
    }
    HelpHit::Body
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;

    #[test]
    fn render_keyspec_formats_ctrl_char_and_named() {
        assert_eq!(render_keyspec(&KeySpec::Ctrl('b')), "C-b");
        assert_eq!(render_keyspec(&KeySpec::Char('x')), "x");
        assert_eq!(render_keyspec(&KeySpec::Named(NamedKey::Right)), "→");
        assert_eq!(render_keyspec(&KeySpec::Named(NamedKey::Down)), "↓");
        assert_eq!(render_keyspec(&KeySpec::Named(NamedKey::Left)), "←");
        assert_eq!(render_keyspec(&KeySpec::Named(NamedKey::Up)), "↑");
    }

    #[test]
    fn build_view_keybindings_use_live_prefix() {
        let cfg = Config::default();
        let v = build_view(&HelpState::default(), KeySpec::Ctrl('a'), &cfg.keys);
        assert!(
            v.keybindings.iter().all(|r| r.keys.starts_with("C-a ")),
            "every keybinding row must render the live prefix C-a"
        );
    }

    #[test]
    fn build_view_keybindings_use_live_rebind() {
        let keys = KeyMap {
            close_pane: KeySpec::Char('k'),
            ..KeyMap::default()
        };
        let v = build_view(&HelpState::default(), KeySpec::Ctrl('b'), &keys);
        assert!(v.keybindings.iter().any(|r| r.keys == "C-b k"));
    }

    #[test]
    fn build_view_includes_implicit_zoom_and_select_rows() {
        let cfg = Config::default();
        let v = build_view(&HelpState::default(), cfg.prefix, &cfg.keys);
        assert!(v.keybindings.iter().any(|r| r.keys == "C-b 0"));
        assert!(v.keybindings.iter().any(|r| r.keys == "C-b 1-9"));
    }

    #[test]
    fn command_rows_cover_every_cli_subcommand() {
        // Coverage guard: trips if a listed subcommand string is removed/renamed.
        // Cannot auto-detect a NEWLY-added subcommand (see spec maintenance note).
        assert_eq!(
            COMMAND_ROWS[0].0, "dvlpr",
            "first row must be the bare invocation"
        );
        for needle in [
            "dvlpr <name>",
            "new -s",
            "attach -t",
            " a)",
            "dvlpr ls",
            "kill -t",
            "ssh",
            "server",
        ] {
            assert!(
                COMMAND_ROWS
                    .iter()
                    .any(|(k, d)| k.contains(needle) || d.contains(needle)),
                "no command row mentions {needle:?}"
            );
        }
    }

    #[test]
    fn cli_help_lists_every_command_and_the_help_flag() {
        let text = cli_help();
        // Every subcommand from the shared table appears...
        for (cmd, desc) in COMMAND_ROWS {
            assert!(text.contains(cmd), "cli help missing command {cmd:?}");
            assert!(text.contains(desc), "cli help missing description {desc:?}");
        }
        // ...plus the help flag itself.
        assert!(
            text.contains("-h, --help"),
            "cli help missing the help flag row"
        );
    }

    #[test]
    fn help_view_active_rows_follow_tab() {
        let cfg = Config::default();
        let kb = build_view(
            &HelpState {
                tab: HelpTab::Keybindings,
                scroll: 0,
            },
            cfg.prefix,
            &cfg.keys,
        );
        let cmds = build_view(
            &HelpState {
                tab: HelpTab::Commands,
                scroll: 0,
            },
            cfg.prefix,
            &cfg.keys,
        );
        assert_eq!(kb.active_rows().len(), 9 + 2); // 9 KeyMap bindings + 2 implicit (0/1-9)
        assert_eq!(cmds.active_rows().len(), COMMAND_ROWS.len());
    }

    fn area(x: u16, y: u16, w: u16, h: u16) -> Rect {
        Rect { x, y, w, h }
    }

    #[test]
    fn help_rect_centers_within_content_area() {
        let r = help_rect(area(0, 0, 80, 23), 8);
        assert_eq!(r.h, HELP_CHROME_ROWS + 8);
        assert_eq!(r.w, HELP_MAX_W);
        assert_eq!(r.x, (80 - HELP_MAX_W) / 2);
        assert_eq!(r.y, (23 - (HELP_CHROME_ROWS + 8)) / 2);
    }

    #[test]
    fn help_rect_shrinks_to_body_when_list_is_short() {
        let r = help_rect(area(0, 0, 80, 40), 3);
        assert_eq!(r.h, HELP_CHROME_ROWS + 3);
    }

    #[test]
    fn help_rect_clamps_height_to_content_and_relies_on_scroll() {
        // 100 rows can't fit in 12 rows of content; rect.h is capped at content.h.
        let r = help_rect(area(0, 0, 80, 12), 100);
        assert_eq!(r.h, 12);
        assert!(max_scroll(100, r) > 0);
    }

    #[test]
    fn help_rect_clamps_width_to_content_when_narrower() {
        let r = help_rect(area(0, 0, 40, 24), 8);
        assert_eq!(r.w, 40);
    }

    #[test]
    fn help_rect_is_strictly_inside_content_for_degenerate_viewport() {
        // zero-height content area (e.g. terminal too small after reserving the status row)
        let content = area(0, 0, 80, 0);
        let r = help_rect(content, 8);
        assert_eq!(r.h, 0);
        assert!(r.x >= content.x && r.y >= content.y);
        assert!(r.x + r.w <= content.x + content.w);
    }

    #[test]
    fn help_renderable_false_for_tiny_rect_true_for_normal() {
        assert!(!help_renderable(area(0, 0, 80, 4))); // < HELP_CHROME_ROWS rows
        assert!(!help_renderable(area(0, 0, 10, 24))); // < HELP_MIN_W cols
        assert!(help_renderable(area(0, 0, 60, 20)));
    }

    #[test]
    fn help_renderable_at_chrome_height_has_zero_body() {
        let r = area(0, 0, 60, HELP_CHROME_ROWS); // exactly chrome height
        assert!(help_renderable(r));
        assert_eq!(visible_body_rows(r), 0);
        let r1 = area(0, 0, 60, HELP_CHROME_ROWS + 1);
        assert_eq!(visible_body_rows(r1), 1);
    }

    #[test]
    fn visible_body_rows_and_max_scroll_are_consistent() {
        let r = area(0, 0, 60, 20); // body = 15
        assert_eq!(visible_body_rows(r), 15);
        assert_eq!(max_scroll(10, r), 0); // fewer rows than body
        assert_eq!(max_scroll(20, r), 5); // 20 - 15
        let tiny = area(0, 0, 60, 3); // h < chrome
        assert_eq!(visible_body_rows(tiny), 0);
        assert_eq!(max_scroll(10, tiny), 10);
    }

    fn view_with(tab: HelpTab) -> HelpView {
        let cfg = Config::default();
        build_view(&HelpState { tab, scroll: 0 }, cfg.prefix, &cfg.keys)
    }

    #[test]
    fn help_hit_returns_tab_for_header_label_cells() {
        let v = view_with(HelpTab::Keybindings);
        let content = area(0, 0, 80, 24);
        let rect = help_rect(content, v.active_rows().len());
        let regs = tab_regions(rect);
        // Click the middle of each tab's chip; expect that tab's index.
        for r in &regs {
            let mid = (r.x_start + r.x_end) / 2;
            let row_1based = (rect.y + 1) + 1;
            assert_eq!(
                help_hit(&v, content, mid + 1, row_1based),
                HelpHit::Tab(r.tab.index())
            );
        }
    }

    #[test]
    fn help_hit_returns_body_for_borders_and_body() {
        let v = view_with(HelpTab::Keybindings);
        let content = area(0, 0, 80, 24);
        let rect = help_rect(content, v.active_rows().len());
        // Top-left border corner.
        assert_eq!(help_hit(&v, content, rect.x + 1, rect.y + 1), HelpHit::Body);
        // A body row interior cell.
        let body_row = (rect.y + 3) + 1;
        assert_eq!(help_hit(&v, content, rect.x + 3, body_row), HelpHit::Body);
    }

    #[test]
    fn help_hit_returns_outside_past_rect() {
        let v = view_with(HelpTab::Keybindings);
        let content = area(0, 0, 80, 24);
        let rect = help_rect(content, v.active_rows().len());
        // One column left of the rect (still 1-based coords).
        assert_eq!(
            help_hit(&v, content, rect.x, rect.y + 2 + 1),
            HelpHit::Outside
        );
        assert_eq!(help_hit(&v, content, 1, 1), HelpHit::Outside);
    }

    #[test]
    fn help_hit_returns_body_when_not_renderable() {
        let v = view_with(HelpTab::Keybindings);
        let content = area(0, 0, 10, 3); // too small to render
        assert_eq!(help_hit(&v, content, 1, 1), HelpHit::Body);
    }

    #[test]
    fn build_view_renders_custom_toggle_sidebar_binding() {
        // Custom binding overrides the default.
        let keys = KeyMap {
            toggle_sidebar: KeySpec::Char('v'),
            ..KeyMap::default()
        };
        let v = build_view(&HelpState::default(), KeySpec::Ctrl('b'), &keys);
        let toggle = v
            .keybindings
            .iter()
            .find(|r| r.desc.contains("agent-awareness sidebar"))
            .expect("toggle-sidebar row present");
        assert_eq!(toggle.keys, "C-b v");

        // Default keymap must render sidebar toggle as "C-b s".
        let default_v =
            build_view(&HelpState::default(), KeySpec::Ctrl('b'), &KeyMap::default());
        let default_toggle = default_v
            .keybindings
            .iter()
            .find(|r| r.desc.contains("agent-awareness sidebar"))
            .expect("toggle-sidebar row present in default keymap");
        assert_eq!(default_toggle.keys, "C-b s");
    }

    #[test]
    fn command_rows_includes_update_subcommand() {
        let found = COMMAND_ROWS.iter().any(|row| row.0.contains("update"));
        assert!(
            found,
            "COMMAND_ROWS must list 'update' so the help overlay shows it"
        );
    }

    #[test]
    fn command_rows_includes_version_flag() {
        let found = COMMAND_ROWS.iter().any(|row| row.0.contains("--version"));
        assert!(
            found,
            "COMMAND_ROWS must list '--version' so the help overlay shows it"
        );
    }

    #[test]
    fn tab_regions_match_help_hit_targets() {
        let rect = area(10, 4, 60, 16);
        let regs = tab_regions(rect);
        assert_eq!(regs.len(), 2);
        assert_eq!(regs[0].tab, HelpTab::Keybindings);
        assert_eq!(regs[1].tab, HelpTab::Commands);
        // Regions are ordered left-to-right and non-overlapping.
        assert!(regs[0].x_end < regs[1].x_start);
    }

    #[test]
    fn help_hit_returns_body_for_inter_tab_gap() {
        let v = view_with(HelpTab::Keybindings);
        let content = area(0, 0, 80, 24);
        let rect = help_rect(content, v.active_rows().len());
        let regs = tab_regions(rect);
        // A column strictly between the two chips, on the tab header row.
        let gap_x = regs[0].x_end + 1; // first cell of the gap (0-based)
        assert!(gap_x < regs[1].x_start, "there must be a gap between chips");
        let row_1based = (rect.y + 1) + 1;
        assert_eq!(help_hit(&v, content, gap_x + 1, row_1based), HelpHit::Body);
    }
}
