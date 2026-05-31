//! Server-side configuration: the prefix key and command keymap, loaded once
//! at daemon start from the first existing file in this order:
//!
//!   1. `$XDG_CONFIG_HOME/dvlpr/config.toml` (or `~/.config/dvlpr/config.toml`
//!      when `XDG_CONFIG_HOME` is unset)
//!   2. `~/.dvlpr/config.toml` (dotfile fallback)
//!
//! Defaults are compiled in; the file overrides. Malformed entries fall back
//! to their default and are logged; a malformed file never crashes the daemon.
//! This module owns `Command` and the key-spec types so `input`/`session`
//! depend on `config` without a cycle.

use std::path::{Path, PathBuf};
use std::str::FromStr;

use serde::Deserialize;

/// A structural command produced by the prefix keymap.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Command {
    SplitHorizontal,
    SplitVertical,
    ClosePane,
    NewWindow,
    OpenNewWindowDialog, // prefix c: open the New Window name dialog
    NextWindow,
    PrevWindow,
    SelectWindow(usize), // 1-based window number from the digit keys
    ToggleZoom,          // C-b 0: fullscreen the focused pane (toggle)
    ToggleSidebar,       // default C-b s: show/hide agent-awareness sidebar
    Detach,
    ShowHelp, // C-b ?: open/close the help overlay (toggle)
}

/// The four named special keys the keymap supports (v1).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NamedKey {
    Up,
    Down,
    Left,
    Right,
}

/// A decoded keypress the parser resolves against the keymap: either a single
/// byte (printable or control) or a named special key.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Key {
    Char(u8),
    Named(NamedKey),
}

/// A configured key binding: a printable char, a named key, or a control combo.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum KeySpec {
    Char(char),
    Named(NamedKey),
    Ctrl(char),
}

impl KeySpec {
    /// The control byte for `Ctrl(c)`, e.g. 'a' => 0x01.
    fn ctrl_byte(c: char) -> u8 {
        (c.to_ascii_uppercase() as u8) & 0x1f
    }

    /// Does this binding match a decoded key?
    pub fn matches(&self, key: &Key) -> bool {
        match (self, key) {
            (KeySpec::Char(c), Key::Char(b)) => *b == *c as u8,
            (KeySpec::Ctrl(c), Key::Char(b)) => *b == Self::ctrl_byte(*c),
            (KeySpec::Named(a), Key::Named(b)) => a == b,
            _ => false,
        }
    }

    /// The canonical byte sequence this binding stands for (used to forward a
    /// literal `prefix prefix` to the focused pane).
    pub fn bytes(&self) -> Vec<u8> {
        match self {
            KeySpec::Char(c) => vec![*c as u8],
            KeySpec::Ctrl(c) => vec![Self::ctrl_byte(*c)],
            KeySpec::Named(n) => named_csi(*n).to_vec(),
        }
    }
}

/// The canonical CSI bytes for a named key (the `ESC [` arrow encoding).
fn named_csi(n: NamedKey) -> &'static [u8] {
    match n {
        NamedKey::Up => b"\x1b[A",
        NamedKey::Down => b"\x1b[B",
        NamedKey::Right => b"\x1b[C",
        NamedKey::Left => b"\x1b[D",
    }
}

impl FromStr for KeySpec {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "Up" => return Ok(KeySpec::Named(NamedKey::Up)),
            "Down" => return Ok(KeySpec::Named(NamedKey::Down)),
            "Left" => return Ok(KeySpec::Named(NamedKey::Left)),
            "Right" => return Ok(KeySpec::Named(NamedKey::Right)),
            _ => {}
        }
        // The parser is byte-oriented, so a key spec must be a single ASCII char.
        // Reject non-ASCII rather than silently truncating with `as u8`.
        if let Some(rest) = s.strip_prefix("C-") {
            let mut chars = rest.chars();
            match (chars.next(), chars.next()) {
                // Only alphabetic control combos are accepted (e.g. C-a); they map
                // to unambiguous control bytes. Normalize case so C-A == C-a.
                (Some(c), None) if c.is_ascii_alphabetic() => {
                    return Ok(KeySpec::Ctrl(c.to_ascii_lowercase()))
                }
                _ => return Err(format!("bad control spec (want C-<a-z>): {s:?}")),
            }
        }
        let mut chars = s.chars();
        match (chars.next(), chars.next()) {
            (Some(c), None) if c.is_ascii() => Ok(KeySpec::Char(c)),
            _ => Err(format!("not a valid single-ascii key spec: {s:?}")),
        }
    }
}

/// Runtime configuration for the agent-awareness sidebar.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SidebarConfig {
    pub width: u16,
}

impl Default for SidebarConfig {
    fn default() -> Self {
        SidebarConfig {
            width: crate::layout::SIDEBAR_WIDTH_DEFAULT,
        }
    }
}

/// Runtime configuration for the blocked-notification sound.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SoundConfig {
    pub enabled: bool,
    /// Override path. `None` means use the embedded default sound.
    pub blocked: Option<String>,
}

impl Default for SoundConfig {
    fn default() -> Self {
        SoundConfig {
            enabled: true,
            blocked: None,
        }
    }
}

/// The command keymap (one binding per command; SelectWindow is implicit digits).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct KeyMap {
    pub split_horizontal: KeySpec,
    pub split_vertical: KeySpec,
    pub close_pane: KeySpec,
    pub new_window: KeySpec,
    pub next_window: KeySpec,
    pub prev_window: KeySpec,
    pub detach: KeySpec,
    pub help: KeySpec,
    pub toggle_sidebar: KeySpec,
}

impl Default for KeyMap {
    fn default() -> Self {
        KeyMap {
            split_horizontal: KeySpec::Named(NamedKey::Down),
            split_vertical: KeySpec::Named(NamedKey::Right),
            close_pane: KeySpec::Char('x'),
            new_window: KeySpec::Char('c'),
            next_window: KeySpec::Char('n'),
            prev_window: KeySpec::Char('p'),
            detach: KeySpec::Char('d'),
            help: KeySpec::Char('?'),
            toggle_sidebar: KeySpec::Char('s'),
        }
    }
}

/// The compiled-in default prefix key. Ctrl-B (matching tmux's own default)
/// leaves Ctrl-A free for readline's beginning-of-line inside the foreground
/// CLI, which is how the user actually edits prompts in `claude`/`codex`/zsh.
/// Single source of truth — both `Config::default()` and the
/// malformed-prefix fallback in `from_toml_str` consume this.
const DEFAULT_PREFIX: KeySpec = KeySpec::Ctrl('b');

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Config {
    pub prefix: KeySpec,
    pub keys: KeyMap,
    pub theme: crate::theme::Theme,
    pub sidebar: SidebarConfig,
    pub sound: SoundConfig,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            prefix: DEFAULT_PREFIX,
            keys: KeyMap::default(),
            theme: crate::theme::Theme::default(),
            sidebar: SidebarConfig::default(),
            sound: SoundConfig::default(),
        }
    }
}

/// Raw TOML shape: every field optional so a partial file overrides only what it
/// names. Values are strings parsed through `KeySpec::from_str`.
#[derive(Default, Deserialize)]
struct RawConfig {
    prefix: Option<String>,
    #[serde(default)]
    keys: RawKeys,
    #[serde(default)]
    theme: RawTheme,
    #[serde(default)]
    sidebar: RawSidebar,
    #[serde(default)]
    sound: RawSound,
}

#[derive(Default, Deserialize)]
struct RawSidebar {
    width: Option<u16>,
}

#[derive(Default, Deserialize)]
struct RawSound {
    enabled: Option<bool>,
    blocked: Option<String>,
}

#[derive(Default, Deserialize)]
struct RawKeys {
    #[serde(rename = "split-horizontal")]
    split_horizontal: Option<String>,
    #[serde(rename = "split-vertical")]
    split_vertical: Option<String>,
    #[serde(rename = "close-pane")]
    close_pane: Option<String>,
    #[serde(rename = "new-window")]
    new_window: Option<String>,
    #[serde(rename = "next-window")]
    next_window: Option<String>,
    #[serde(rename = "prev-window")]
    prev_window: Option<String>,
    detach: Option<String>,
    help: Option<String>,
    #[serde(rename = "toggle-sidebar")]
    toggle_sidebar: Option<String>,
}

/// Raw `[theme]` table. Only `flavor` exists in v1; missing/unknown falls back
/// to the default flavor (logged) in `from_toml_str`.
#[derive(Default, Deserialize)]
struct RawTheme {
    flavor: Option<String>,
}

/// Parse one optional spec, falling back to `default` (and logging) on absence or
/// a malformed value.
fn spec_or_default(raw: &Option<String>, field: &str, default: KeySpec) -> KeySpec {
    match raw {
        None => default,
        Some(s) => match s.parse::<KeySpec>() {
            Ok(spec) => spec,
            Err(e) => {
                tracing::warn!(field, value = %s, error = %e, "bad key spec; using default");
                default
            }
        },
    }
}

/// Parse the optional flavor string, falling back to `Theme::default()` (and
/// logging) on absence or an unknown value.
fn flavor_or_default(raw: &Option<String>) -> crate::theme::Theme {
    match raw {
        None => crate::theme::Theme::default(),
        Some(s) => match crate::theme::Flavor::from_str(s) {
            Ok(f) => crate::theme::Theme::from_flavor(f),
            Err(e) => {
                tracing::warn!(field = "theme.flavor", value = %s, error = %e, "bad flavor; using default");
                crate::theme::Theme::default()
            }
        },
    }
}

fn sidebar_from_raw(raw: &RawSidebar) -> SidebarConfig {
    let width = match raw.width {
        None => crate::layout::SIDEBAR_WIDTH_DEFAULT,
        Some(w) => {
            let clamped = w.clamp(
                crate::layout::SIDEBAR_WIDTH_MIN,
                crate::layout::SIDEBAR_WIDTH_MAX,
            );
            if clamped != w {
                tracing::warn!(
                    requested = w,
                    clamped,
                    "sidebar.width out of range; clamped"
                );
            }
            clamped
        }
    };
    SidebarConfig { width }
}

fn sound_from_raw(raw: &RawSound) -> SoundConfig {
    let enabled = raw.enabled.unwrap_or(true);
    let blocked = raw.blocked.as_ref().map(|s| expand_tilde(s));
    SoundConfig { enabled, blocked }
}

fn expand_tilde(path: &str) -> String {
    if path == "~" {
        return std::env::var("HOME").unwrap_or_default();
    }
    if let Some(rest) = path.strip_prefix("~/") {
        let home = std::env::var("HOME").unwrap_or_default();
        return format!("{home}/{rest}");
    }
    path.to_string()
}

impl Config {
    /// Load from the first existing candidate in `config_path_candidates()`.
    /// All-missing or parse failure => all defaults (logged), never fatal. A
    /// non-`NotFound` read error on a candidate (e.g. permission denied) logs a
    /// warning and falls back to defaults rather than silently trying the next
    /// candidate — a file that exists but can't be read is a misconfiguration
    /// worth surfacing.
    pub fn load() -> Config {
        for path in config_path_candidates() {
            match std::fs::read_to_string(&path) {
                Ok(text) => return Config::from_toml_str(&text),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
                Err(e) => {
                    tracing::warn!(path = %path.display(), error = %e, "cannot read config; using defaults");
                    return Config::default();
                }
            }
        }
        Config::default()
    }

    /// Parse TOML text into a `Config`. Unparseable TOML => all defaults (logged).
    pub fn from_toml_str(text: &str) -> Config {
        let raw: RawConfig = match toml::from_str(text) {
            Ok(r) => r,
            Err(e) => {
                tracing::warn!(error = %e, "malformed config TOML; using all defaults");
                return Config::default();
            }
        };
        let d = KeyMap::default();
        let theme = flavor_or_default(&raw.theme.flavor);
        Config {
            prefix: spec_or_default(&raw.prefix, "prefix", DEFAULT_PREFIX),
            keys: KeyMap {
                split_horizontal: spec_or_default(
                    &raw.keys.split_horizontal,
                    "split-horizontal",
                    d.split_horizontal,
                ),
                split_vertical: spec_or_default(
                    &raw.keys.split_vertical,
                    "split-vertical",
                    d.split_vertical,
                ),
                close_pane: spec_or_default(&raw.keys.close_pane, "close-pane", d.close_pane),
                new_window: spec_or_default(&raw.keys.new_window, "new-window", d.new_window),
                next_window: spec_or_default(&raw.keys.next_window, "next-window", d.next_window),
                prev_window: spec_or_default(&raw.keys.prev_window, "prev-window", d.prev_window),
                detach: spec_or_default(&raw.keys.detach, "detach", d.detach),
                help: spec_or_default(&raw.keys.help, "help", d.help),
                toggle_sidebar: spec_or_default(
                    &raw.keys.toggle_sidebar,
                    "toggle-sidebar",
                    d.toggle_sidebar,
                ),
            },
            theme,
            sidebar: sidebar_from_raw(&raw.sidebar),
            sound: sound_from_raw(&raw.sound),
        }
    }

    /// Resolve a decoded key to a command, if it binds one. SelectWindow (digits)
    /// is handled by the parser, not here.
    pub fn resolve(&self, key: &Key) -> Option<Command> {
        let k = &self.keys;
        if k.split_horizontal.matches(key) {
            Some(Command::SplitHorizontal)
        } else if k.split_vertical.matches(key) {
            Some(Command::SplitVertical)
        } else if k.close_pane.matches(key) {
            Some(Command::ClosePane)
        } else if k.new_window.matches(key) {
            Some(Command::OpenNewWindowDialog)
        } else if k.next_window.matches(key) {
            Some(Command::NextWindow)
        } else if k.prev_window.matches(key) {
            Some(Command::PrevWindow)
        } else if k.detach.matches(key) {
            Some(Command::Detach)
        } else if k.help.matches(key) {
            Some(Command::ShowHelp)
        } else if k.toggle_sidebar.matches(key) {
            Some(Command::ToggleSidebar)
        } else {
            None
        }
    }
}

/// First-match-wins candidate list, env-aware (see `build_candidates` for the
/// pure helper). Order: XDG config dir, then `~/.dvlpr/config.toml`.
fn config_path_candidates() -> Vec<PathBuf> {
    let home = std::env::var_os("HOME").map(PathBuf::from);
    let xdg = std::env::var_os("XDG_CONFIG_HOME").map(PathBuf::from);
    build_candidates(home.as_deref(), xdg.as_deref())
}

/// Pure candidate-list builder (testable without env mutation). Order:
///   1. `$XDG_CONFIG_HOME/dvlpr/config.toml` if `xdg` is set, otherwise
///      `$HOME/.config/dvlpr/config.toml` if `home` is set
///   2. `$HOME/.dvlpr/config.toml` if `home` is set
///
/// Returns an empty vec when neither `home` nor `xdg` are available — in that
/// (degenerate) environment, `Config::load` returns all defaults.
fn build_candidates(home: Option<&Path>, xdg: Option<&Path>) -> Vec<PathBuf> {
    let mut out = Vec::new();
    if let Some(xdg) = xdg {
        out.push(xdg.join("dvlpr").join("config.toml"));
    } else if let Some(home) = home {
        out.push(home.join(".config").join("dvlpr").join("config.toml"));
    }
    if let Some(home) = home {
        out.push(home.join(".dvlpr").join("config.toml"));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_match_the_spec() {
        let c = Config::default();
        assert_eq!(c.prefix, KeySpec::Ctrl('b'));
        assert_eq!(c.keys.split_horizontal, KeySpec::Named(NamedKey::Down));
        assert_eq!(c.keys.split_vertical, KeySpec::Named(NamedKey::Right));
        assert_eq!(c.keys.close_pane, KeySpec::Char('x'));
        assert_eq!(c.keys.new_window, KeySpec::Char('c'));
        assert_eq!(c.keys.next_window, KeySpec::Char('n'));
        assert_eq!(c.keys.prev_window, KeySpec::Char('p'));
        assert_eq!(c.keys.detach, KeySpec::Char('d'));
        assert_eq!(c.keys.help, KeySpec::Char('?'));
        assert_eq!(c.keys.toggle_sidebar, KeySpec::Char('s'));
    }

    #[test]
    fn default_prefix_is_ctrl_b() {
        // Pins the user-facing contract: dvlpr's compiled-in default prefix is
        // Ctrl-B (matching tmux's default), so Ctrl-A is free for readline's
        // beginning-of-line inside the foreground CLI. Redundant with the
        // prefix assertion in `defaults_match_the_spec`, but named to make the
        // rationale searchable in the codebase.
        assert_eq!(Config::default().prefix, KeySpec::Ctrl('b'));
    }

    #[test]
    fn keyspec_parses_each_grammar_form() {
        assert_eq!("x".parse::<KeySpec>().unwrap(), KeySpec::Char('x'));
        assert_eq!(
            "Down".parse::<KeySpec>().unwrap(),
            KeySpec::Named(NamedKey::Down)
        );
        assert_eq!("C-a".parse::<KeySpec>().unwrap(), KeySpec::Ctrl('a'));
        assert!("".parse::<KeySpec>().is_err());
        assert!("Nope".parse::<KeySpec>().is_err());
        assert!("C-".parse::<KeySpec>().is_err());
        assert!("é".parse::<KeySpec>().is_err()); // non-ASCII rejected, not truncated
        assert!("C-é".parse::<KeySpec>().is_err());
        assert_eq!("C-A".parse::<KeySpec>().unwrap(), KeySpec::Ctrl('a')); // case-normalized
        assert!("C-[".parse::<KeySpec>().is_err()); // non-alphabetic ctrl rejected
    }

    #[test]
    fn keyspec_matches_keys() {
        assert!(KeySpec::Char('x').matches(&Key::Char(b'x')));
        assert!(!KeySpec::Char('x').matches(&Key::Char(b'y')));
        assert!(KeySpec::Ctrl('a').matches(&Key::Char(0x01)));
        assert!(KeySpec::Named(NamedKey::Down).matches(&Key::Named(NamedKey::Down)));
        assert!(!KeySpec::Named(NamedKey::Down).matches(&Key::Named(NamedKey::Up)));
    }

    #[test]
    fn keyspec_emits_canonical_bytes() {
        assert_eq!(KeySpec::Char('x').bytes(), vec![b'x']);
        assert_eq!(KeySpec::Ctrl('a').bytes(), vec![0x01]);
        assert_eq!(KeySpec::Named(NamedKey::Down).bytes(), b"\x1b[B".to_vec());
    }

    #[test]
    fn toml_overrides_selected_keys_and_keeps_other_defaults() {
        let toml = r#"
            prefix = "C-b"
            [keys]
            close-pane = "q"
        "#;
        let c = Config::from_toml_str(toml);
        assert_eq!(c.prefix, KeySpec::Ctrl('b'));
        assert_eq!(c.keys.close_pane, KeySpec::Char('q'));
        // Unspecified keys keep their defaults.
        assert_eq!(c.keys.split_horizontal, KeySpec::Named(NamedKey::Down));
    }

    #[test]
    fn malformed_entry_falls_back_to_default_for_that_key() {
        let toml = r#"
            [keys]
            close-pane = "Nonsense"
        "#;
        let c = Config::from_toml_str(toml);
        assert_eq!(c.keys.close_pane, KeySpec::Char('x')); // default kept
    }

    #[test]
    fn unparseable_toml_yields_all_defaults() {
        let c = Config::from_toml_str("this is not = = toml [[[");
        assert_eq!(c, Config::default());
    }

    #[test]
    fn resolve_maps_a_key_to_its_command() {
        let c = Config::default();
        assert_eq!(
            c.resolve(&Key::Named(NamedKey::Down)),
            Some(Command::SplitHorizontal)
        );
        assert_eq!(
            c.resolve(&Key::Named(NamedKey::Right)),
            Some(Command::SplitVertical)
        );
        assert_eq!(c.resolve(&Key::Char(b'x')), Some(Command::ClosePane));
        assert_eq!(c.resolve(&Key::Char(b'c')), Some(Command::OpenNewWindowDialog));
        assert_eq!(c.resolve(&Key::Char(b'?')), Some(Command::ShowHelp));
        assert_eq!(c.resolve(&Key::Char(b'z')), None);
    }

    #[test]
    fn config_default_theme_is_one_dark() {
        let c = Config::default();
        assert_eq!(
            c.theme,
            crate::theme::Theme::from_flavor(crate::theme::Flavor::OneDark)
        );
    }

    #[test]
    fn toml_with_theme_flavor_macchiato_parses() {
        let c = Config::from_toml_str(
            r#"[theme]
flavor = "macchiato"
"#,
        );
        assert_eq!(
            c.theme,
            crate::theme::Theme::from_flavor(crate::theme::Flavor::Macchiato)
        );
    }

    #[test]
    fn toml_with_unknown_flavor_falls_back_to_one_dark() {
        let c = Config::from_toml_str(
            r#"[theme]
flavor = "cappuccino"
"#,
        );
        assert_eq!(
            c.theme,
            crate::theme::Theme::from_flavor(crate::theme::Flavor::OneDark)
        );
    }

    #[test]
    fn toml_without_theme_section_falls_back_to_one_dark() {
        let c = Config::from_toml_str("prefix = \"C-a\"\n");
        assert_eq!(
            c.theme,
            crate::theme::Theme::from_flavor(crate::theme::Flavor::OneDark)
        );
    }

    #[test]
    fn toml_with_theme_flavor_one_dark_parses() {
        let c = Config::from_toml_str(
            r#"[theme]
flavor = "one-dark"
"#,
        );
        assert_eq!(
            c.theme,
            crate::theme::Theme::from_flavor(crate::theme::Flavor::OneDark)
        );
    }

    #[test]
    fn build_candidates_with_xdg_uses_xdg_then_dotfile() {
        // When XDG_CONFIG_HOME is set, the XDG path is the primary; ~/.config
        // is NOT also consulted (preserves the XDG convention). The dotfile
        // fallback is appended as a second candidate.
        let home = PathBuf::from("/home/alice");
        let xdg = PathBuf::from("/custom/xdg");
        let candidates = build_candidates(Some(&home), Some(&xdg));
        assert_eq!(
            candidates,
            vec![
                PathBuf::from("/custom/xdg/dvlpr/config.toml"),
                PathBuf::from("/home/alice/.dvlpr/config.toml"),
            ]
        );
    }

    #[test]
    fn build_candidates_without_xdg_uses_dot_config_then_dotfile() {
        let home = PathBuf::from("/home/alice");
        let candidates = build_candidates(Some(&home), None);
        assert_eq!(
            candidates,
            vec![
                PathBuf::from("/home/alice/.config/dvlpr/config.toml"),
                PathBuf::from("/home/alice/.dvlpr/config.toml"),
            ]
        );
    }

    #[test]
    fn build_candidates_with_no_home_is_empty() {
        // No HOME and no XDG => nothing to read; load() returns defaults.
        assert!(build_candidates(None, None).is_empty());
    }

    #[test]
    fn build_candidates_with_xdg_but_no_home_returns_only_xdg() {
        let xdg = PathBuf::from("/custom/xdg");
        let candidates = build_candidates(None, Some(&xdg));
        assert_eq!(
            candidates,
            vec![PathBuf::from("/custom/xdg/dvlpr/config.toml")]
        );
    }

    #[test]
    fn config_default_sidebar_width_is_26() {
        let cfg = Config::default();
        assert_eq!(cfg.sidebar.width, 26);
    }

    #[test]
    fn config_default_sound_is_enabled_with_no_override_path() {
        let cfg = Config::default();
        assert!(cfg.sound.enabled);
        assert!(cfg.sound.blocked.is_none(), "default uses embedded sound");
    }

    #[test]
    fn config_parses_explicit_sidebar_width() {
        let cfg = Config::from_toml_str("[sidebar]\nwidth = 30\n");
        assert_eq!(cfg.sidebar.width, 30);
    }

    #[test]
    fn config_clamps_sidebar_width_above_max() {
        let cfg = Config::from_toml_str("[sidebar]\nwidth = 99\n");
        assert_eq!(cfg.sidebar.width, 36);
    }

    #[test]
    fn config_clamps_sidebar_width_below_min() {
        let cfg = Config::from_toml_str("[sidebar]\nwidth = 4\n");
        assert_eq!(cfg.sidebar.width, 18);
    }

    #[test]
    fn config_parses_sound_disabled() {
        let cfg = Config::from_toml_str("[sound]\nenabled = false\n");
        assert!(!cfg.sound.enabled);
    }

    #[test]
    fn config_parses_sound_blocked_path_with_tilde_expansion() {
        std::env::set_var("HOME", "/tmp/fake-home");
        let cfg = Config::from_toml_str("[sound]\nblocked = \"~/x.aiff\"\n");
        assert_eq!(cfg.sound.blocked.as_deref(), Some("/tmp/fake-home/x.aiff"));
    }

    #[test]
    fn toml_overrides_help_key() {
        let c = Config::from_toml_str("[keys]\nhelp = \"h\"\n");
        assert_eq!(c.keys.help, KeySpec::Char('h'));
        assert_eq!(c.resolve(&Key::Char(b'h')), Some(Command::ShowHelp));
    }

    #[test]
    fn malformed_help_key_falls_back_to_question_mark() {
        let c = Config::from_toml_str("[keys]\nhelp = \"Nonsense\"\n");
        assert_eq!(c.keys.help, KeySpec::Char('?'));
    }

    #[test]
    fn new_window_key_resolves_to_open_dialog() {
        let c = Config::default();
        assert_eq!(
            c.resolve(&Key::Char(b'c')),
            Some(Command::OpenNewWindowDialog)
        );
    }

    #[test]
    fn config_parses_explicit_toggle_sidebar() {
        let cfg = Config::from_toml_str("[keys]\ntoggle-sidebar = \"v\"\n");
        assert_eq!(cfg.keys.toggle_sidebar, KeySpec::Char('v'));
    }

    #[test]
    fn resolve_maps_default_s_to_toggle_sidebar() {
        let cfg = Config::default();
        assert_eq!(cfg.resolve(&Key::Char(b's')), Some(Command::ToggleSidebar));
    }

    #[test]
    fn resolve_maps_custom_key_to_toggle_sidebar() {
        let cfg = Config::from_toml_str("[keys]\ntoggle-sidebar = \"v\"\n");
        assert_eq!(cfg.resolve(&Key::Char(b'v')), Some(Command::ToggleSidebar));
        assert_eq!(cfg.resolve(&Key::Char(b's')), None);
    }
}
