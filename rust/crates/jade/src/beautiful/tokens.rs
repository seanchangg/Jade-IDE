//! Beautiful UI's token set, converted from oklch to sRGB.
//!
//! The source is written in oklch (`.dark { --ink: oklch(96.4% .002 247.839) }`);
//! GPUI paints sRGB, so the values are converted once here and the original
//! oklch is quoted beside each one — the same convention
//! [`crate::kumo::tokens`] uses, so an upstream change stays a textual diff.
//!
//! Only the dark set is ported. Jade ships one theme and it is dark; a light
//! set would be dead code that silently rots.

use gpui::{rgb, rgba, Rgba};

/// One resolved palette.
///
/// The names are Beautiful UI's, not Kumo's, on purpose. Mapping them onto
/// Kumo's slots would lose the distinctions this library actually leans on —
/// three ink levels, `line` versus `line-strong`, and a `tint` for every status
/// color — and every mapping decision would then be invisible at the call site.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct BeautifulTokens {
    // ── surfaces, darkest to lightest ────────────────────────────────────
    /// The window behind everything.
    pub page: Rgba,
    /// One step up from `page`.
    pub canvas: Rgba,
    /// Recessed well — the trough a source list or a detail drawer sits in.
    pub inset: Rgba,
    /// Cards, popovers, menus. What most things paint themselves.
    pub surface: Rgba,
    /// Hover wash.
    pub hover: Rgba,
    /// Stronger hover, for controls that already sit on `hover`.
    pub hover_2: Rgba,
    /// Form-control and chip fill.
    pub field: Rgba,

    // ── ink, three levels ────────────────────────────────────────────────
    /// Titles and primary labels. Also the "primary button" fill — Beautiful
    /// UI's emphasis color is ink, not the accent.
    pub ink: Rgba,
    /// Secondary text.
    pub ink_2: Rgba,
    /// Metadata, timestamps, counts, idle icons.
    pub ink_3: Rgba,

    // ── lines ────────────────────────────────────────────────────────────
    /// Card edges and separators.
    pub line: Rgba,
    /// Interactive or focused edges.
    pub line_strong: Rgba,

    // ── accent ───────────────────────────────────────────────────────────
    /// Used almost only for selection and focus. Status never uses it.
    pub accent: Rgba,
    pub accent_ink: Rgba,
    pub accent_tint: Rgba,

    // ── status: a saturated ink over a 14% tint, never a bare border ──────
    pub green: Rgba,
    pub green_tint: Rgba,
    pub orange: Rgba,
    pub orange_tint: Rgba,
    pub red: Rgba,
    pub red_tint: Rgba,

    pub tooltip_bg: Rgba,
    pub tooltip_fg: Rgba,
}

/// The dark palette (`.dark` in the source stylesheet).
pub fn dark() -> BeautifulTokens {
    BeautifulTokens {
        page: rgb(0x17181A),        // oklch(20.9% .004 264.477)
        canvas: rgb(0x1C1D1F),      // oklch(23.1% .004 264.487)
        inset: rgb(0x1F2022),       // oklch(24.3% .004 264.492)
        surface: rgb(0x232427),     // oklch(26%   .006 271.191)
        hover: rgb(0x2A2B2E),       // oklch(28.9% .006 271.22)
        hover_2: rgb(0x313236),     // oklch(31.8% .007 274.747)
        field: rgb(0x2B2C2F),       // oklch(29.3% .006 271.223)

        ink: rgb(0xF2F3F4),         // oklch(96.4% .002 247.839)
        ink_2: rgb(0xA5A8AD),       // oklch(73.1% .008 260.731)
        ink_3: rgb(0x6C6F75),       // oklch(54.1% .01  264.484)

        line: rgb(0x2E3033),        // oklch(30.8% .006 258.354)
        line_strong: rgb(0x3A3C40), // oklch(35.6% .007 264.474)

        accent: rgb(0x3D9AFF),      // oklch(68%   .173 253.301)
        accent_ink: rgb(0x7EC0FF),  // oklch(78.8% .113 248.33)
        // The tints are the same color at 16%/14% alpha in the source; GPUI
        // composites them the same way CSS does.
        accent_tint: rgba(0x3D9AFF29),

        green: rgb(0x3CBB72),       // oklch(70.5% .154 153.814)
        green_tint: rgba(0x3CBB7224),
        orange: rgb(0xF68F3C),      // oklch(74.6% .156 55.642)
        orange_tint: rgba(0xF68F3C24),
        red: rgb(0xEE5C61),         // oklch(66.6% .18  21.433)
        red_tint: rgba(0xEE5C6124),

        tooltip_bg: rgb(0x111214),  // oklch(18.2% .004 264.459)
        tooltip_fg: rgb(0xF2F3F4),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The surface ladder must never collapse: two equal steps means a card
    /// disappears into its own background and only the ring renders.
    #[test]
    fn the_surface_ladder_is_strictly_ordered() {
        let t = dark();
        let lum = |c: Rgba| c.r + c.g + c.b;
        let ladder = [t.page, t.canvas, t.inset, t.surface, t.hover, t.hover_2];
        for pair in ladder.windows(2) {
            assert!(
                lum(pair[1]) > lum(pair[0]),
                "surface ladder is not increasing: {:?} then {:?}",
                pair[0],
                pair[1]
            );
        }
    }

    /// Three ink levels, each clearly separated — the library carries almost
    /// all of its hierarchy on this axis rather than on size or weight.
    #[test]
    fn the_ink_ladder_is_strictly_ordered() {
        let t = dark();
        let lum = |c: Rgba| c.r + c.g + c.b;
        assert!(lum(t.ink) > lum(t.ink_2));
        assert!(lum(t.ink_2) > lum(t.ink_3));
        // And ink_3 must still out-read the surface it sits on.
        assert!(lum(t.ink_3) > lum(t.surface));
    }

    #[test]
    fn every_status_color_has_a_matching_tint() {
        let t = dark();
        for (solid, tint) in [
            (t.green, t.green_tint),
            (t.orange, t.orange_tint),
            (t.red, t.red_tint),
        ] {
            assert_eq!(
                (solid.r, solid.g, solid.b),
                (tint.r, tint.g, tint.b),
                "a tint must be its own color at low alpha"
            );
            assert!(tint.a < 0.2 && tint.a > 0.0, "tint alpha {}", tint.a);
            assert_eq!(solid.a, 1.0);
        }
    }

    /// `line_strong` is the interactive edge, so it must actually read as
    /// stronger than the resting one.
    #[test]
    fn line_strong_outreads_line() {
        let t = dark();
        let lum = |c: Rgba| c.r + c.g + c.b;
        assert!(lum(t.line_strong) > lum(t.line));
    }
}
