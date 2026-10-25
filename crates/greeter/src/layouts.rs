//! The keyboard layouts to choose from: the config's, as xkb has them (`layout` or
//! `layout(variant)`), each with a short name for the indicator. No I/O.
//!
//! The greeter starts on the first. Choosing a user switches to the layout they last
//! logged in with, if it is still configured; otherwise the layout stays as it is, so one
//! chosen before the user (on the row, or for "Other…"'s name) carries on. A layout changes
//! only what keys type from then on: what is typed already stays as it was typed.

use tracing::info;

/// A layout to type with.
#[derive(Clone, Debug, PartialEq)]
pub struct Layout {
    /// As configured: "de(nodeadkeys)".
    pub name: String,
    /// "DE".
    pub short: String,
}

/// `names`' layouts, logged. xkbcommon has no short names (they are in xkeyboard-config's
/// `evdev.xml`, which is not read), so a short name is the layout without its variant, in
/// capitals, numbered from 2 where it repeats: "us", "us(dvorak)" are US and US2.
pub fn new(names: &[String]) -> Vec<Layout> {
    let mut layouts: Vec<Layout> = Vec::new();
    let mut bases: Vec<String> = Vec::new();
    for name in names {
        let base = name.split('(').next().unwrap_or_default().to_uppercase();
        let before = bases.iter().filter(|b| **b == base).count();
        let short = if before == 0 {
            base.clone()
        } else {
            format!("{base}{}", before + 1)
        };
        bases.push(base);
        layouts.push(Layout {
            name: name.clone(),
            short,
        });
    }
    if let Some(first) = layouts.first() {
        let names: Vec<_> = layouts
            .iter()
            .map(|l| format!("{} ({})", l.name, l.short))
            .collect();
        info!(
            "layouts: {}; {} chosen (first)",
            names.join(", "),
            first.short
        );
    }
    layouts
}

/// The layout to type with for a user who last logged in with `remembered`: that one if it
/// is still one of `layouts`; else `current`.
pub fn preselect(layouts: &[Layout], remembered: Option<&str>, current: usize) -> usize {
    let remembered = remembered.and_then(|name| layouts.iter().position(|l| l.name == name));
    remembered.unwrap_or(current)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn layouts(names: &[&str]) -> Vec<Layout> {
        new(&names
            .iter()
            .map(|name| name.to_string())
            .collect::<Vec<_>>())
    }

    #[test]
    fn short_names_are_the_layout_in_capitals_numbered_where_they_repeat() {
        let shorts: Vec<_> = layouts(&["us", "de(nodeadkeys)", "us(dvorak)", "latam", "us(intl)"])
            .into_iter()
            .map(|layout| layout.short)
            .collect();
        assert_eq!(shorts, ["US", "DE", "US2", "LATAM", "US3"]);
        assert!(layouts(&[]).is_empty());
    }

    #[test]
    fn a_user_gets_their_layout_else_the_one_chosen_before() {
        let layouts = layouts(&["de", "us", "us(dvorak)"]);
        assert_eq!(preselect(&layouts, Some("us(dvorak)"), 1), 2);
        // None remembered, or one no longer configured: as it was.
        assert_eq!(preselect(&layouts, None, 1), 1);
        assert_eq!(preselect(&layouts, Some("fr"), 0), 0);
        assert_eq!(preselect(&[], Some("de"), 0), 0);
    }
}
