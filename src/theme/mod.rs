//! Catppuccin status-bar theme: a `Flavor` enum (Latte/Frappe/Macchiato/Mocha),
//! four const `Palette` tables of the official catppuccin colors, and a `Theme`
//! role mapping the compositor consumes. See
//! `docs/superpowers/specs/2026-05-28-status-bar-theming-design.md`.

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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn flavor_default_is_latte() {
        assert_eq!(Flavor::default(), Flavor::Latte);
    }
}
