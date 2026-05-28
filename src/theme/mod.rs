//! Status-bar theme: a `Flavor` enum (Latte/Frappe/Macchiato/Mocha + OneDark),
//! const `Palette` tables (catppuccin + Atom's One Dark), and a `Theme` role
//! mapping the compositor consumes. See the design spec at
//! `docs/superpowers/specs/2026-05-28-status-bar-one-dark-design.md`.

use crate::compositor::Color;
use std::str::FromStr;

/// One of the supported theme flavors. `Default` is `Latte` (the light
/// catppuccin flavor), chosen per the design spec.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Flavor {
    #[default]
    Latte,
    Frappe,
    Macchiato,
    Mocha,
    OneDark,
}

impl Flavor {
    /// True for light flavors. Drives per-flavor fg selection in `Theme::from_flavor`
    /// (see the design spec's "Role → color mapping" section).
    pub fn is_light(self) -> bool {
        matches!(self, Flavor::Latte)
    }
}

impl FromStr for Flavor {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "latte" => Ok(Flavor::Latte),
            "frappe" => Ok(Flavor::Frappe),
            "macchiato" => Ok(Flavor::Macchiato),
            "mocha" => Ok(Flavor::Mocha),
            "one-dark" => Ok(Flavor::OneDark),
            _ => Err(format!("unknown flavor: {s:?}")),
        }
    }
}

/// The 26 named catppuccin colors for one flavor. Field names mirror the
/// catppuccin style guide
/// (<https://github.com/catppuccin/catppuccin/blob/main/docs/style-guide.md>).
/// Vendored verbatim — no new crate dependency.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Palette {
    pub rosewater: Color,
    pub flamingo: Color,
    pub pink: Color,
    pub mauve: Color,
    pub red: Color,
    pub maroon: Color,
    pub peach: Color,
    pub yellow: Color,
    pub green: Color,
    pub teal: Color,
    pub sky: Color,
    pub sapphire: Color,
    pub blue: Color,
    pub lavender: Color,
    pub text: Color,
    pub subtext1: Color,
    pub subtext0: Color,
    pub overlay2: Color,
    pub overlay1: Color,
    pub overlay0: Color,
    pub surface2: Color,
    pub surface1: Color,
    pub surface0: Color,
    pub base: Color,
    pub mantle: Color,
    pub crust: Color,
}

pub const LATTE: Palette = Palette {
    rosewater: Color::Rgb(0xdc, 0x8a, 0x78),
    flamingo: Color::Rgb(0xdd, 0x78, 0x78),
    pink: Color::Rgb(0xea, 0x76, 0xcb),
    mauve: Color::Rgb(0x88, 0x39, 0xef),
    red: Color::Rgb(0xd2, 0x0f, 0x39),
    maroon: Color::Rgb(0xe6, 0x45, 0x53),
    peach: Color::Rgb(0xfe, 0x64, 0x0b),
    yellow: Color::Rgb(0xdf, 0x8e, 0x1d),
    green: Color::Rgb(0x40, 0xa0, 0x2b),
    teal: Color::Rgb(0x17, 0x92, 0x99),
    sky: Color::Rgb(0x04, 0xa5, 0xe5),
    sapphire: Color::Rgb(0x20, 0x9f, 0xb5),
    blue: Color::Rgb(0x1e, 0x66, 0xf5),
    lavender: Color::Rgb(0x72, 0x87, 0xfd),
    text: Color::Rgb(0x4c, 0x4f, 0x69),
    subtext1: Color::Rgb(0x5c, 0x5f, 0x77),
    subtext0: Color::Rgb(0x6c, 0x6f, 0x85),
    overlay2: Color::Rgb(0x7c, 0x7f, 0x93),
    overlay1: Color::Rgb(0x8c, 0x8f, 0xa1),
    overlay0: Color::Rgb(0x9c, 0xa0, 0xb0),
    surface2: Color::Rgb(0xac, 0xb0, 0xbe),
    surface1: Color::Rgb(0xbc, 0xc0, 0xcc),
    surface0: Color::Rgb(0xcc, 0xd0, 0xda),
    base: Color::Rgb(0xef, 0xf1, 0xf5),
    mantle: Color::Rgb(0xe6, 0xe9, 0xef),
    crust: Color::Rgb(0xdc, 0xe0, 0xe8),
};

pub const FRAPPE: Palette = Palette {
    rosewater: Color::Rgb(0xf2, 0xd5, 0xcf),
    flamingo: Color::Rgb(0xee, 0xbe, 0xbe),
    pink: Color::Rgb(0xf4, 0xb8, 0xe4),
    mauve: Color::Rgb(0xca, 0x9e, 0xe6),
    red: Color::Rgb(0xe7, 0x82, 0x84),
    maroon: Color::Rgb(0xea, 0x99, 0x9c),
    peach: Color::Rgb(0xef, 0x9f, 0x76),
    yellow: Color::Rgb(0xe5, 0xc8, 0x90),
    green: Color::Rgb(0xa6, 0xd1, 0x89),
    teal: Color::Rgb(0x81, 0xc8, 0xbe),
    sky: Color::Rgb(0x99, 0xd1, 0xdb),
    sapphire: Color::Rgb(0x85, 0xc1, 0xdc),
    blue: Color::Rgb(0x8c, 0xaa, 0xee),
    lavender: Color::Rgb(0xba, 0xbb, 0xf1),
    text: Color::Rgb(0xc6, 0xd0, 0xf5),
    subtext1: Color::Rgb(0xb5, 0xbf, 0xe2),
    subtext0: Color::Rgb(0xa5, 0xad, 0xce),
    overlay2: Color::Rgb(0x94, 0x9c, 0xbb),
    overlay1: Color::Rgb(0x83, 0x8b, 0xa7),
    overlay0: Color::Rgb(0x73, 0x79, 0x94),
    surface2: Color::Rgb(0x62, 0x68, 0x80),
    surface1: Color::Rgb(0x51, 0x57, 0x6d),
    surface0: Color::Rgb(0x41, 0x45, 0x59),
    base: Color::Rgb(0x30, 0x34, 0x46),
    mantle: Color::Rgb(0x29, 0x2c, 0x3c),
    crust: Color::Rgb(0x23, 0x26, 0x34),
};

pub const MACCHIATO: Palette = Palette {
    rosewater: Color::Rgb(0xf4, 0xdb, 0xd6),
    flamingo: Color::Rgb(0xf0, 0xc6, 0xc6),
    pink: Color::Rgb(0xf5, 0xbd, 0xe6),
    mauve: Color::Rgb(0xc6, 0xa0, 0xf6),
    red: Color::Rgb(0xed, 0x87, 0x96),
    maroon: Color::Rgb(0xee, 0x99, 0xa0),
    peach: Color::Rgb(0xf5, 0xa9, 0x7f),
    yellow: Color::Rgb(0xee, 0xd4, 0x9f),
    green: Color::Rgb(0xa6, 0xda, 0x95),
    teal: Color::Rgb(0x8b, 0xd5, 0xca),
    sky: Color::Rgb(0x91, 0xd7, 0xe3),
    sapphire: Color::Rgb(0x7d, 0xc4, 0xe4),
    blue: Color::Rgb(0x8a, 0xad, 0xf4),
    lavender: Color::Rgb(0xb7, 0xbd, 0xf8),
    text: Color::Rgb(0xca, 0xd3, 0xf5),
    subtext1: Color::Rgb(0xb8, 0xc0, 0xe0),
    subtext0: Color::Rgb(0xa5, 0xad, 0xcb),
    overlay2: Color::Rgb(0x93, 0x9a, 0xb7),
    overlay1: Color::Rgb(0x80, 0x87, 0xa2),
    overlay0: Color::Rgb(0x6e, 0x73, 0x8d),
    surface2: Color::Rgb(0x5b, 0x60, 0x78),
    surface1: Color::Rgb(0x49, 0x4d, 0x64),
    surface0: Color::Rgb(0x36, 0x3a, 0x4f),
    base: Color::Rgb(0x24, 0x27, 0x3a),
    mantle: Color::Rgb(0x1e, 0x20, 0x30),
    crust: Color::Rgb(0x18, 0x19, 0x26),
};

pub const MOCHA: Palette = Palette {
    rosewater: Color::Rgb(0xf5, 0xe0, 0xdc),
    flamingo: Color::Rgb(0xf2, 0xcd, 0xcd),
    pink: Color::Rgb(0xf5, 0xc2, 0xe7),
    mauve: Color::Rgb(0xcb, 0xa6, 0xf7),
    red: Color::Rgb(0xf3, 0x8b, 0xa8),
    maroon: Color::Rgb(0xeb, 0xa0, 0xac),
    peach: Color::Rgb(0xfa, 0xb3, 0x87),
    yellow: Color::Rgb(0xf9, 0xe2, 0xaf),
    green: Color::Rgb(0xa6, 0xe3, 0xa1),
    teal: Color::Rgb(0x94, 0xe2, 0xd5),
    sky: Color::Rgb(0x89, 0xdc, 0xeb),
    sapphire: Color::Rgb(0x74, 0xc7, 0xec),
    blue: Color::Rgb(0x89, 0xb4, 0xfa),
    lavender: Color::Rgb(0xb4, 0xbe, 0xfe),
    text: Color::Rgb(0xcd, 0xd6, 0xf4),
    subtext1: Color::Rgb(0xba, 0xc2, 0xde),
    subtext0: Color::Rgb(0xa6, 0xad, 0xc8),
    overlay2: Color::Rgb(0x93, 0x99, 0xb2),
    overlay1: Color::Rgb(0x7f, 0x84, 0x9c),
    overlay0: Color::Rgb(0x6c, 0x70, 0x86),
    surface2: Color::Rgb(0x58, 0x5b, 0x70),
    surface1: Color::Rgb(0x45, 0x47, 0x5a),
    surface0: Color::Rgb(0x31, 0x32, 0x44),
    base: Color::Rgb(0x1e, 0x1e, 0x2e),
    mantle: Color::Rgb(0x18, 0x18, 0x25),
    crust: Color::Rgb(0x11, 0x11, 0x1b),
};

pub const ONE_DARK: Palette = Palette {
    rosewater: Color::Rgb(0xe0, 0x6c, 0x75), // reuses red — no canonical rosewater
    flamingo: Color::Rgb(0xe0, 0x6c, 0x75),
    pink: Color::Rgb(0xc6, 0x78, 0xdd),
    mauve: Color::Rgb(0xc6, 0x78, 0xdd), // magenta — drives session_fg
    red: Color::Rgb(0xe0, 0x6c, 0x75),
    maroon: Color::Rgb(0xbe, 0x5a, 0x65),
    peach: Color::Rgb(0xd1, 0x9a, 0x66), // orange — drives active_tab_bg
    yellow: Color::Rgb(0xe5, 0xc0, 0x7b),
    green: Color::Rgb(0x98, 0xc3, 0x79),
    teal: Color::Rgb(0x56, 0xb6, 0xc2),
    sky: Color::Rgb(0x56, 0xb6, 0xc2),
    sapphire: Color::Rgb(0x61, 0xaf, 0xef),
    blue: Color::Rgb(0x61, 0xaf, 0xef),
    lavender: Color::Rgb(0xc6, 0x78, 0xdd),
    text: Color::Rgb(0xab, 0xb2, 0xbf), // fg — drives inactive_tab_fg
    subtext1: Color::Rgb(0x9a, 0xa0, 0xae),
    subtext0: Color::Rgb(0x82, 0x88, 0x97),
    overlay2: Color::Rgb(0x6e, 0x74, 0x82),
    overlay1: Color::Rgb(0x5c, 0x63, 0x70),
    overlay0: Color::Rgb(0x4b, 0x52, 0x5d),
    surface2: Color::Rgb(0x4b, 0x52, 0x5d),
    surface1: Color::Rgb(0x44, 0x49, 0x55),
    surface0: Color::Rgb(0x3e, 0x44, 0x51), // bg-lighter
    base: Color::Rgb(0x28, 0x2c, 0x34), // bg
    mantle: Color::Rgb(0x24, 0x27, 0x2e),
    crust: Color::Rgb(0x21, 0x25, 0x2b), // bg-darker — drives active_tab_fg
};

/// Renderer-facing role mapping. The compositor reads these fields directly to
/// style status-bar cells and the heavy divider glyph. Constructed once per
/// session via `from_flavor` and stored immutably; the hot render path never
/// re-parses a flavor string.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Theme {
    pub bar_bg: Color,
    pub session_fg: Color,
    pub session_bg: Color,
    pub active_tab_fg: Color,
    pub active_tab_bg: Color,
    pub active_tab_bold: bool,
    pub inactive_tab_fg: Color,
    pub inactive_tab_bg: Color,
    pub agent_idle_fg: Color,
    pub agent_working_fg: Color,
    pub agent_blocked_fg: Color,
}

impl Theme {
    /// Materialise the role mapping for a given flavor. Background roles are
    /// uniform `Color::Default` (transparent) across every flavor: the host
    /// terminal background shows through everywhere except the active-tab
    /// chip. Foreground roles split on `Flavor::is_light`: Latte uses `text`
    /// for `active_tab_fg` (text on vivid peach, bold-acceptable contrast);
    /// dark flavors use `crust` (≥7:1 on pastel peach / muted orange).
    pub fn from_flavor(flavor: Flavor) -> Self {
        let p: &Palette = match flavor {
            Flavor::Latte     => &LATTE,
            Flavor::Frappe    => &FRAPPE,
            Flavor::Macchiato => &MACCHIATO,
            Flavor::Mocha     => &MOCHA,
            Flavor::OneDark   => &ONE_DARK,
        };
        let active_tab_fg = if flavor.is_light() { p.text } else { p.crust };
        Theme {
            bar_bg:          Color::Default,
            session_bg:      Color::Default,
            session_fg:      p.mauve,
            active_tab_bg:   p.peach,
            active_tab_fg,
            active_tab_bold: true,
            inactive_tab_bg: Color::Default,
            inactive_tab_fg: p.text,
            agent_idle_fg:    p.green,
            agent_working_fg: p.yellow,
            agent_blocked_fg: p.red,
        }
    }
}

impl Default for Theme {
    fn default() -> Self {
        Theme::from_flavor(Flavor::Latte)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn flavor_default_is_latte() {
        assert_eq!(Flavor::default(), Flavor::Latte);
    }

    #[test]
    fn flavor_from_str_parses_each_name() {
        assert_eq!(Flavor::from_str("latte"), Ok(Flavor::Latte));
        assert_eq!(Flavor::from_str("frappe"), Ok(Flavor::Frappe));
        assert_eq!(Flavor::from_str("macchiato"), Ok(Flavor::Macchiato));
        assert_eq!(Flavor::from_str("mocha"), Ok(Flavor::Mocha));
    }

    #[test]
    fn flavor_from_str_rejects_unknown_and_wrong_case() {
        assert!(Flavor::from_str("").is_err());
        assert!(Flavor::from_str("Latte").is_err()); // case-sensitive
        assert!(Flavor::from_str("MOCHA").is_err());
        assert!(Flavor::from_str("cappuccino").is_err());
    }

    #[test]
    fn is_light_is_true_only_for_latte() {
        assert!(Flavor::Latte.is_light());
        assert!(!Flavor::Frappe.is_light());
        assert!(!Flavor::Macchiato.is_light());
        assert!(!Flavor::Mocha.is_light());
    }

    #[test]
    fn latte_palette_spot_check() {
        assert_eq!(LATTE.mauve, Color::Rgb(0x88, 0x39, 0xef));
        assert_eq!(LATTE.peach, Color::Rgb(0xfe, 0x64, 0x0b));
        assert_eq!(LATTE.text, Color::Rgb(0x4c, 0x4f, 0x69));
        assert_eq!(LATTE.crust, Color::Rgb(0xdc, 0xe0, 0xe8));
        assert_eq!(LATTE.surface0, Color::Rgb(0xcc, 0xd0, 0xda));
    }

    #[test]
    fn mocha_palette_spot_check() {
        assert_eq!(MOCHA.mauve, Color::Rgb(0xcb, 0xa6, 0xf7));
        assert_eq!(MOCHA.peach, Color::Rgb(0xfa, 0xb3, 0x87));
        assert_eq!(MOCHA.crust, Color::Rgb(0x11, 0x11, 0x1b));
        assert_eq!(MOCHA.surface0, Color::Rgb(0x31, 0x32, 0x44));
    }

    #[test]
    fn frappe_and_macchiato_spot_check() {
        assert_eq!(FRAPPE.peach, Color::Rgb(0xef, 0x9f, 0x76));
        assert_eq!(FRAPPE.crust, Color::Rgb(0x23, 0x26, 0x34));
        assert_eq!(MACCHIATO.peach, Color::Rgb(0xf5, 0xa9, 0x7f));
        assert_eq!(MACCHIATO.crust, Color::Rgb(0x18, 0x19, 0x26));
    }

    #[test]
    fn theme_default_is_latte() {
        let t = Theme::default();
        assert_eq!(t, Theme::from_flavor(Flavor::Latte));
        // Spot-check the new uniform rule: bar_bg is transparent in every
        // flavor, including the current default.
        assert_eq!(t.bar_bg, Color::Default);
    }

    #[test]
    fn active_tab_is_bold_in_every_flavor() {
        for f in [
            Flavor::Latte,
            Flavor::Frappe,
            Flavor::Macchiato,
            Flavor::Mocha,
            Flavor::OneDark,
        ] {
            assert!(Theme::from_flavor(f).active_tab_bold, "{f:?}");
        }
    }

    #[test]
    fn latte_role_mapping_uses_expected_palette_colors() {
        let t = Theme::from_flavor(Flavor::Latte);
        // Backgrounds: uniform Color::Default (transparent) across all flavors.
        assert_eq!(t.bar_bg, Color::Default);
        assert_eq!(t.session_bg, Color::Default);
        assert_eq!(t.inactive_tab_bg, Color::Default);
        // Active tab background: still per-flavor.
        assert_eq!(t.active_tab_bg, LATTE.peach);
        // Foregrounds:
        //  - session_fg is now the accent (mauve), not crust.
        //  - inactive_tab_fg unchanged.
        //  - active_tab_fg per-flavor: Latte (light) uses text; dark uses crust.
        assert_eq!(t.session_fg, LATTE.mauve);
        assert_eq!(t.active_tab_fg, LATTE.text);
        assert_eq!(t.inactive_tab_fg, LATTE.text);
        assert_eq!(t.agent_idle_fg, LATTE.green);
        assert_eq!(t.agent_working_fg, LATTE.yellow);
        assert_eq!(t.agent_blocked_fg, LATTE.red);
    }

    #[test]
    fn frappe_role_mapping_uses_expected_palette_colors() {
        let t = Theme::from_flavor(Flavor::Frappe);
        assert_eq!(t.bar_bg, Color::Default);
        assert_eq!(t.session_bg, Color::Default);
        assert_eq!(t.inactive_tab_bg, Color::Default);
        assert_eq!(t.active_tab_bg, FRAPPE.peach);
        assert_eq!(t.session_fg, FRAPPE.mauve);
        assert_eq!(t.active_tab_fg, FRAPPE.crust);
        assert_eq!(t.inactive_tab_fg, FRAPPE.text);
        assert_eq!(t.agent_idle_fg, FRAPPE.green);
        assert_eq!(t.agent_working_fg, FRAPPE.yellow);
        assert_eq!(t.agent_blocked_fg, FRAPPE.red);
    }

    #[test]
    fn macchiato_role_mapping_uses_expected_palette_colors() {
        let t = Theme::from_flavor(Flavor::Macchiato);
        assert_eq!(t.bar_bg, Color::Default);
        assert_eq!(t.session_bg, Color::Default);
        assert_eq!(t.inactive_tab_bg, Color::Default);
        assert_eq!(t.active_tab_bg, MACCHIATO.peach);
        assert_eq!(t.session_fg, MACCHIATO.mauve);
        assert_eq!(t.active_tab_fg, MACCHIATO.crust);
        assert_eq!(t.inactive_tab_fg, MACCHIATO.text);
        assert_eq!(t.agent_idle_fg, MACCHIATO.green);
        assert_eq!(t.agent_working_fg, MACCHIATO.yellow);
        assert_eq!(t.agent_blocked_fg, MACCHIATO.red);
    }

    #[test]
    fn mocha_role_mapping_uses_expected_palette_colors() {
        let t = Theme::from_flavor(Flavor::Mocha);
        assert_eq!(t.bar_bg, Color::Default);
        assert_eq!(t.session_bg, Color::Default);
        assert_eq!(t.inactive_tab_bg, Color::Default);
        assert_eq!(t.active_tab_bg, MOCHA.peach);
        assert_eq!(t.session_fg, MOCHA.mauve);
        assert_eq!(t.active_tab_fg, MOCHA.crust);
        assert_eq!(t.inactive_tab_fg, MOCHA.text);
        assert_eq!(t.agent_idle_fg, MOCHA.green);
        assert_eq!(t.agent_working_fg, MOCHA.yellow);
        assert_eq!(t.agent_blocked_fg, MOCHA.red);
    }

    #[test]
    fn flavor_from_str_parses_one_dark() {
        assert_eq!(Flavor::from_str("one-dark"), Ok(Flavor::OneDark));
    }

    #[test]
    fn flavor_from_str_rejects_one_dark_wrong_spellings() {
        assert!(Flavor::from_str("OneDark").is_err());
        assert!(Flavor::from_str("onedark").is_err());
        assert!(Flavor::from_str("ONE-DARK").is_err());
        assert!(Flavor::from_str("one_dark").is_err());
    }

    #[test]
    fn is_light_is_false_for_one_dark() {
        assert!(!Flavor::OneDark.is_light());
    }

    #[test]
    fn one_dark_palette_spot_check() {
        assert_eq!(ONE_DARK.mauve, Color::Rgb(0xc6, 0x78, 0xdd));
        assert_eq!(ONE_DARK.peach, Color::Rgb(0xd1, 0x9a, 0x66));
        assert_eq!(ONE_DARK.text, Color::Rgb(0xab, 0xb2, 0xbf));
        assert_eq!(ONE_DARK.crust, Color::Rgb(0x21, 0x25, 0x2b));
        assert_eq!(ONE_DARK.surface0, Color::Rgb(0x3e, 0x44, 0x51));
        assert_eq!(ONE_DARK.green, Color::Rgb(0x98, 0xc3, 0x79));
        assert_eq!(ONE_DARK.yellow, Color::Rgb(0xe5, 0xc0, 0x7b));
        assert_eq!(ONE_DARK.red, Color::Rgb(0xe0, 0x6c, 0x75));
    }

    #[test]
    fn one_dark_role_mapping_uses_expected_palette_colors() {
        let t = Theme::from_flavor(Flavor::OneDark);
        assert_eq!(t.bar_bg, Color::Default);
        assert_eq!(t.session_bg, Color::Default);
        assert_eq!(t.inactive_tab_bg, Color::Default);
        assert_eq!(t.active_tab_bg, ONE_DARK.peach);
        assert_eq!(t.session_fg, ONE_DARK.mauve);
        assert_eq!(t.active_tab_fg, ONE_DARK.crust); // dark-flavor branch
        assert_eq!(t.inactive_tab_fg, ONE_DARK.text);
        assert_eq!(t.agent_idle_fg, ONE_DARK.green);
        assert_eq!(t.agent_working_fg, ONE_DARK.yellow);
        assert_eq!(t.agent_blocked_fg, ONE_DARK.red);
    }
}
