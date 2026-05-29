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
    ("dvlpr new -s <name>", "Create or attach to <name> (explicit form)"),
    (
        "dvlpr attach -t <name>",
        "Attach to an existing session; error if missing (alias: a)",
    ),
    ("dvlpr ls", "List live sessions with window counts"),
    ("dvlpr kill -t <name>", "Kill a session's daemon; error if missing"),
    ("dvlpr ssh <dest> [name]", "SSH to <dest> and run dvlpr there"),
    ("dvlpr server [name]", "Internal daemon entrypoint (spawned for you)"),
];

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
        row(&keys.split_horizontal, "Split the focused pane top / bottom"),
        row(&keys.close_pane, "Close the focused pane"),
        row(&keys.new_window, "Create a new window"),
        row(&keys.next_window, "Switch to the next window"),
        row(&keys.prev_window, "Switch to the previous window"),
        row(&keys.detach, "Detach from the session (leave it running)"),
        row(&keys.help, "Show / hide this help"),
        // Implicit parser-level bindings (src/input/mod.rs::resolve_key). These
        // literals MUST stay in sync with that function.
        lit("0", "Toggle zoom (fullscreen the focused pane)"),
        lit("s", "Toggle the agent-awareness sidebar"),
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
        let mut keys = KeyMap::default();
        keys.close_pane = KeySpec::Char('k');
        let v = build_view(&HelpState::default(), KeySpec::Ctrl('b'), &keys);
        assert!(v.keybindings.iter().any(|r| r.keys == "C-b k"));
    }

    #[test]
    fn build_view_includes_implicit_zoom_sidebar_select_rows() {
        let cfg = Config::default();
        let v = build_view(&HelpState::default(), cfg.prefix, &cfg.keys);
        assert!(v.keybindings.iter().any(|r| r.keys == "C-b 0"));
        assert!(v.keybindings.iter().any(|r| r.keys == "C-b s"));
        assert!(v.keybindings.iter().any(|r| r.keys == "C-b 1-9"));
    }

    #[test]
    fn command_rows_cover_every_cli_subcommand() {
        // Coverage guard: trips if a listed subcommand string is removed/renamed.
        // Cannot auto-detect a NEWLY-added subcommand (see spec maintenance note).
        assert_eq!(COMMAND_ROWS[0].0, "dvlpr", "first row must be the bare invocation");
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
        assert_eq!(kb.active_rows().len(), 8 + 3); // 8 KeyMap bindings + 3 implicit (0/s/1-9)
        assert_eq!(cmds.active_rows().len(), COMMAND_ROWS.len());
    }
}
