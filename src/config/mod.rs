//! Server-side configuration: the prefix key and command keymap, loaded once at
//! daemon start from `~/.config/dvlpr/config.toml`. Defaults are compiled in; the
//! file overrides. Malformed entries fall back to their default and are logged; a
//! malformed file never crashes the daemon. This module owns `Command` and the
//! key-spec types so `input`/`session` depend on `config` without a cycle.

use std::str::FromStr;

use serde::Deserialize;

/// A structural command produced by the prefix keymap.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Command {
    SplitHorizontal,
    SplitVertical,
    ClosePane,
    NewWindow,
    NextWindow,
    PrevWindow,
    SelectWindow(usize), // 1-based window number from the digit keys
    ToggleZoom,          // C-a 0: fullscreen the focused pane (toggle)
    ToggleSidebar,       // C-a s: show/hide agent-awareness sidebar
    Detach,
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
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Config {
    pub prefix: KeySpec,
    pub keys: KeyMap,
    pub theme: crate::theme::Theme,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            prefix: KeySpec::Ctrl('a'),
            keys: KeyMap::default(),
            theme: crate::theme::Theme::default(),
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
}

/// Raw `[theme]` table. Only `flavor` exists in v1; missing/unknown falls back
/// to Latte (logged) in `from_toml_str`.
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

/// Parse the optional flavor string, falling back to Latte (and logging) on
/// absence or an unknown value.
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

impl Config {
    /// Load from `~/.config/dvlpr/config.toml`. Missing file or parse failure =>
    /// all defaults (logged), never fatal.
    pub fn load() -> Config {
        let Some(path) = config_path() else {
            return Config::default();
        };
        match std::fs::read_to_string(&path) {
            Ok(text) => Config::from_toml_str(&text),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Config::default(),
            Err(e) => {
                tracing::warn!(path = %path.display(), error = %e, "cannot read config; using defaults");
                Config::default()
            }
        }
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
            prefix: spec_or_default(&raw.prefix, "prefix", KeySpec::Ctrl('a')),
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
            },
            theme,
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
            Some(Command::NewWindow)
        } else if k.next_window.matches(key) {
            Some(Command::NextWindow)
        } else if k.prev_window.matches(key) {
            Some(Command::PrevWindow)
        } else if k.detach.matches(key) {
            Some(Command::Detach)
        } else {
            None
        }
    }
}

/// `~/.config/dvlpr/config.toml`, honoring `XDG_CONFIG_HOME`.
fn config_path() -> Option<std::path::PathBuf> {
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .map(std::path::PathBuf::from)
        .or_else(|| {
            std::env::var_os("HOME").map(|h| std::path::PathBuf::from(h).join(".config"))
        })?;
    Some(base.join("dvlpr").join("config.toml"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_match_the_spec() {
        let c = Config::default();
        assert_eq!(c.prefix, KeySpec::Ctrl('a'));
        assert_eq!(c.keys.split_horizontal, KeySpec::Named(NamedKey::Down));
        assert_eq!(c.keys.split_vertical, KeySpec::Named(NamedKey::Right));
        assert_eq!(c.keys.close_pane, KeySpec::Char('x'));
        assert_eq!(c.keys.new_window, KeySpec::Char('c'));
        assert_eq!(c.keys.next_window, KeySpec::Char('n'));
        assert_eq!(c.keys.prev_window, KeySpec::Char('p'));
        assert_eq!(c.keys.detach, KeySpec::Char('d'));
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
        assert_eq!(c.resolve(&Key::Char(b'c')), Some(Command::NewWindow));
        assert_eq!(c.resolve(&Key::Char(b'z')), None);
    }

    #[test]
    fn config_default_theme_is_latte() {
        let c = Config::default();
        assert_eq!(c.theme, crate::theme::Theme::from_flavor(crate::theme::Flavor::Latte));
    }

    #[test]
    fn toml_with_theme_flavor_macchiato_parses() {
        let c = Config::from_toml_str(r#"[theme]
flavor = "macchiato"
"#);
        assert_eq!(c.theme, crate::theme::Theme::from_flavor(crate::theme::Flavor::Macchiato));
    }

    #[test]
    fn toml_with_unknown_flavor_falls_back_to_latte() {
        let c = Config::from_toml_str(r#"[theme]
flavor = "cappuccino"
"#);
        assert_eq!(c.theme, crate::theme::Theme::from_flavor(crate::theme::Flavor::Latte));
    }

    #[test]
    fn toml_without_theme_section_falls_back_to_latte() {
        let c = Config::from_toml_str("prefix = \"C-a\"\n");
        assert_eq!(c.theme, crate::theme::Theme::from_flavor(crate::theme::Flavor::Latte));
    }
}
