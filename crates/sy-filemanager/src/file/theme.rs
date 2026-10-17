//! `sy file` palette + iced theme projection. Roadmap Step 23.
//!
//! `sy file` is an xdg-toplevel (not a layer-shell surface), so it
//! shares the same gruvbox-shaped four-slot `Palette` the `sy mon`
//! popup already projects out of the bar's theme loader. Centralising
//! the re-export here means the file plane never imports
//! `crate::mon::theme` directly — Step 24's `view::pane` reaches
//! through `crate::file::theme::Palette` and stays decoupled from the
//! mon plane's lifetime.
//!
//! ## iced theme
//!
//! The journey-J1 brief calls for "gruvbox-dark". iced 0.14 ships
//! [`iced::Theme::GruvboxDark`] as a first-class built-in whose
//! [`Extended` palette][palette] already encodes the contrast ratios
//! the iced widget defaults expect. We use that directly rather than
//! constructing a `Theme::custom` from the bar's loaded TOML — the
//! bar's `Palette` and iced's are different shapes, and Step 24+'s
//! widget styling will reach for both (the iced `Theme` for built-in
//! widgets like `text` / `container`, the bar's `Palette` for the
//! hand-rolled `view::pane` chrome). Keeping them parallel is
//! cheaper than coercing one into the other.
//!
//! [palette]: iced::theme::palette::Extended

/// Re-export of the seven-slot [`crate::mon::theme::Palette`] so
/// `sy file` view code can `use crate::file::theme::Palette` without
/// reaching across plane boundaries. The mapping is documented on the
/// original type; Step 24+ widgets bind to specific slots
/// (`bg`/`bg2`/`accent`/`ink`/`ok`/`warn`/`bad`).
pub use crate::mon::theme::Palette;

/// The iced [`Theme`][iced::Theme] the `sy file` window paints under.
/// Constructed from sy's seven-slot [`Palette`] (loaded from the
/// productivised bar theme) via `Theme::custom` so the file plane
/// shares the same gruvbox tokens `sy mon` and `sy stack bar` paint
/// under. Mapping:
///
///   * background  ← `palette.bg`
///   * text        ← `palette.ink`
///   * primary     ← `palette.accent`
///   * success     ← `palette.ok`
///   * danger      ← `palette.bad`
pub fn iced_theme() -> iced::Theme {
    let p = crate::mon::theme::load_or_ink();
    iced::Theme::custom(
        "sy".to_string(),
        iced::theme::Palette {
            background: p.bg,
            text: p.ink,
            primary: p.accent,
            success: p.ok,
            warning: p.warn,
            danger: p.bad,
        },
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Step 23 DoD: gruvbox-dark via the productivised sy palette
    /// (not the iced built-in). `Theme::Custom { name == "sy" }` so a
    /// future refactor can't silently swap the journey-J1 palette
    /// source.
    #[test]
    fn iced_theme_is_sy_custom() {
        let theme = iced_theme();
        let label = format!("{theme:?}");
        assert!(
            label.contains("Custom") && label.contains("sy"),
            "expected sy Custom theme, got {label}"
        );
    }

    /// Step 23 DoD: the bar palette projection is reachable from the
    /// file plane without a direct `crate::mon::theme` import in the
    /// view code. Step 24+'s widget modules will reach for
    /// [`Palette`] via this re-export; pinning a contrast invariant
    /// today keeps the projection honest.
    #[test]
    fn palette_projection_yields_visible_contrast() {
        let p = crate::mon::theme::load_or_ink();
        let _: Palette = p; // re-export type-check
        assert_ne!(p.bg, p.ink, "bg == ink would render text invisible");
    }
}
