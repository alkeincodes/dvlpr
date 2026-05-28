//! Catppuccin status-bar theme: a `Flavor` enum (Latte/Frappe/Macchiato/Mocha),
//! four const `Palette` tables of the official catppuccin colors, and a `Theme`
//! role mapping the compositor consumes. See
//! `docs/superpowers/specs/2026-05-28-status-bar-theming-design.md`.

use std::str::FromStr;

/// One of catppuccin's four flavors. `Default` is `Latte` (the light flavor),
/// chosen per the design spec.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Flavor {
    #[default]
    Latte,
    Frappe,
    Macchiato,
    Mocha,
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
            _ => Err(format!("unknown flavor: {s:?}")),
        }
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
}
