//! A GPUI port of Cloudflare's Kumo component library (`@cloudflare/kumo`).
//!
//! Kumo is a React + Tailwind v4 library. The originals live in
//! `packages/kumo/src/components/<name>/<name>.tsx`, where each component
//! exports a `KUMO_<NAME>_VARIANTS` table of Tailwind class strings. Each module
//! here copies one of those tables: the same variant names, the same sizes, and
//! the same geometry after the Tailwind classes resolve to pixels (see
//! [`scale`]). The comment above every variant quotes the class string it came
//! from, so a Kumo upgrade is a diff against the quoted text.
//!
//! Colors come from [`tokens::KumoTokens`], never from a literal, so a palette
//! is a data change and never a component edit: [`tokens::jade_dark`] fills the
//! same token slots as [`tokens::kumo_dark`], and every component restyles at
//! once. Jade keeps its own colors — the charcoal canvas and the emerald accent
//! — and takes only the structure: Kumo's sizes, radii, weights, and the one
//! rule that separates a surface from the surface behind it.
//!
//! # Using a component
//!
//! Each builder ends in `render(&tokens)`, which returns a GPUI element the
//! caller finishes wiring:
//!
//! ```ignore
//! Button::new("run", "Run")
//!     .variant(ButtonVariant::Primary)
//!     .icon("play")
//!     .render(&tokens)
//!     .on_click(cx.listener(|app, _, _, cx| app.action_run(cx)))
//! ```

pub mod badge;
pub mod banner;
pub mod button;
pub mod card;
pub mod empty;
pub mod input;
pub mod meter;
pub mod separator;
pub mod table;
pub mod tabs;
pub mod text;
pub mod tokens;
pub mod tooltip;

// The library surface. Every component is re-exported whether or not the app
// currently mounts it — the port is of the whole Kumo set, and a component that
// is not on screen today is still the answer for the next panel.
#[allow(unused_imports)]
pub use badge::{Badge, BadgeAppearance, BadgeVariant, DotColor};
#[allow(unused_imports)]
pub use banner::{Banner, BannerSize, BannerVariant};
#[allow(unused_imports)]
pub use button::{Button, ButtonShape, ButtonVariant};
#[allow(unused_imports)]
pub use card::{Card, CardPrimary, CardSecondary, Surface};
#[allow(unused_imports)]
pub use empty::Empty;
#[allow(unused_imports)]
pub use input::{InputVariant, TextField};
#[allow(unused_imports)]
pub use meter::Meter;
#[allow(unused_imports)]
pub use separator::{separator_h, separator_v};
#[allow(unused_imports)]
pub use table::{Table, TableCell, TableHeader, TableRow};
#[allow(unused_imports)]
pub use tabs::{TabBar, TabItem, TabsAppearance};
#[allow(unused_imports)]
pub use text::{Heading, HeadingLevel, Text, TextTone};
#[allow(unused_imports)]
pub use tokens::KumoTokens;
#[allow(unused_imports)]
pub use tooltip::tooltip_panel;

use gpui::{px, Pixels, Rgba};

/// One shared size axis. Kumo names these `xs | sm | base | lg` and reuses them
/// across Button, Input, Tabs, and Badge, so they live here once.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Size {
    Xs,
    Sm,
    Base,
    Lg,
}

impl Default for Size {
    fn default() -> Self {
        Size::Base
    }
}

/// The Tailwind v4 scale, resolved to pixels.
///
/// Tailwind's spacing unit is `0.25rem`, and Kumo's theme block overrides the
/// four text sizes (`--text-xs: 12px` … `--text-lg: 16px`). Everything the
/// ported components need is named here so a class like `h-6.5 rounded-md px-2
/// text-xs` reads back as `H_6_5`, `RADIUS_MD`, `SPACE_2`, `TEXT_XS`.
pub mod scale {
    use gpui::{px, Pixels};

    /// Tailwind's spacing unit: `0.25rem` at a 16px root.
    pub const UNIT: f32 = 4.0;

    /// `n` Tailwind spacing units in pixels — `space(2.5)` is `px-2.5` = 10px.
    pub const fn space(n: f32) -> Pixels {
        px(n * UNIT)
    }

    // Spacing values the ported components use.
    pub const SPACE_0_5: Pixels = px(2.0); // 0.5
    pub const SPACE_1: Pixels = px(4.0);
    pub const SPACE_1_5: Pixels = px(6.0);
    pub const SPACE_2: Pixels = px(8.0);
    pub const SPACE_2_5: Pixels = px(10.0);
    pub const SPACE_3: Pixels = px(12.0);
    pub const SPACE_4: Pixels = px(16.0);
    pub const SPACE_6: Pixels = px(24.0);

    // Control heights. Kumo sizes every control off these four.
    pub const H_5: Pixels = px(20.0); // size xs
    pub const H_6_5: Pixels = px(26.0); // size sm
    pub const H_9: Pixels = px(36.0); // size base
    pub const H_10: Pixels = px(40.0); // size lg

    // Tailwind v4 radii.
    pub const RADIUS_XS: Pixels = px(0.0);
    pub const RADIUS_SM: Pixels = px(0.0);
    pub const RADIUS_MD: Pixels = px(0.0);
    pub const RADIUS_LG: Pixels = px(0.0);
    pub const RADIUS_XL: Pixels = px(0.0);

    // Kumo's `@theme` type scale.
    pub const TEXT_XS: Pixels = px(12.0);
    pub const TEXT_SM: Pixels = px(13.0);
    pub const TEXT_BASE: Pixels = px(14.0);
    pub const TEXT_LG: Pixels = px(16.0);
    pub const TEXT_2XL: Pixels = px(24.0);
    pub const TEXT_3XL: Pixels = px(30.0);

    /// Tailwind's `ring` with no width — a 1px ring.
    pub const RING: Pixels = px(1.0);
}

impl Size {
    /// The control height for this size: `h-5 | h-6.5 | h-9 | h-10`.
    pub fn height(self) -> Pixels {
        match self {
            Size::Xs => scale::H_5,
            Size::Sm => scale::H_6_5,
            Size::Base => scale::H_9,
            Size::Lg => scale::H_10,
        }
    }

    /// The horizontal padding: `px-1.5 | px-2 | px-3 | px-4`.
    pub fn padding_x(self) -> Pixels {
        match self {
            Size::Xs => scale::SPACE_1_5,
            Size::Sm => scale::SPACE_2,
            Size::Base => scale::SPACE_3,
            Size::Lg => scale::SPACE_4,
        }
    }

    /// The gap between an icon and its label: `gap-1 | gap-1 | gap-1.5 | gap-2`.
    pub fn gap(self) -> Pixels {
        match self {
            Size::Xs | Size::Sm => scale::SPACE_1,
            Size::Base => scale::SPACE_1_5,
            Size::Lg => scale::SPACE_2,
        }
    }

    /// The corner radius. Jade draws every control square, so every size
    /// resolves to zero; the method stays so a size still owns its geometry.
    pub fn radius(self) -> Pixels {
        px(0.)
    }

    /// The label size: `text-xs | text-xs | text-base | text-base`.
    pub fn text(self) -> Pixels {
        match self {
            Size::Xs | Size::Sm => scale::TEXT_XS,
            Size::Base | Size::Lg => scale::TEXT_BASE,
        }
    }

    /// The height of a segmented tab trough: the same `h-6.5` / `h-9` every
    /// other control uses.
    pub fn tab_bar_height(self) -> Pixels {
        match self {
            Size::Xs | Size::Sm => scale::H_6_5,
            Size::Base | Size::Lg => scale::H_9,
        }
    }

    /// The height of a trigger inside that trough. Kumo gives the trigger
    /// `my-0.5`, so it is 2px shorter than the trough on each edge. That margin
    /// is what keeps the active pill's corners clear of the trough's radius —
    /// without it the pill sits flush and its corners get clipped.
    pub fn tab_trigger_height(self) -> Pixels {
        self.tab_bar_height() - px(4.)
    }

    /// The icon edge that reads correctly inside this control.
    pub fn icon(self) -> f32 {
        match self {
            Size::Xs => 11.0,
            Size::Sm => 12.0,
            Size::Base => 14.0,
            Size::Lg => 16.0,
        }
    }
}

/// [`crate::assets::ui_icon`] against a token color.
///
/// `ui_icon` takes a packed `0xRRGGBB`; the tokens are [`Rgba`]. This drops the
/// alpha, which is correct for every icon Kumo paints — it tints icons with a
/// solid text token, never with a `_tint`.
pub fn icon(name: &str, size: f32, color: Rgba) -> gpui::Svg {
    crate::assets::ui_icon(name, size, pack(color))
}

/// A token back to the packed `0xRRGGBB` that older helpers in this crate take.
pub fn pack(c: Rgba) -> u32 {
    let ch = |v: f32| ((v.clamp(0.0, 1.0) * 255.0).round() as u32) & 0xFF;
    (ch(c.r) << 16) | (ch(c.g) << 8) | ch(c.b)
}

/// Tailwind's `shadow-xs`: `0 1px 2px 0 rgb(0 0 0 / 0.05)`.
pub fn shadow_xs() -> Vec<gpui::BoxShadow> {
    vec![gpui::BoxShadow::new(px(0.), px(1.), gpui::hsla(0., 0., 0., 0.05)).blur_radius(px(2.))]
}

/// Tailwind's `shadow-sm`: `0 1px 3px 0 rgb(0 0 0 / 0.1)` over
/// `0 1px 2px -1px rgb(0 0 0 / 0.1)`.
pub fn shadow_sm() -> Vec<gpui::BoxShadow> {
    vec![
        gpui::BoxShadow::new(px(0.), px(1.), gpui::hsla(0., 0., 0., 0.10)).blur_radius(px(3.)),
        gpui::BoxShadow::new(px(0.), px(1.), gpui::hsla(0., 0., 0., 0.10))
            .blur_radius(px(2.))
            .spread_radius(px(-1.)),
    ]
}

/// Tailwind's `shadow-md`: `0 4px 6px -1px rgb(0 0 0 / 0.1)` over
/// `0 2px 4px -2px rgb(0 0 0 / 0.1)`. Popovers and tooltips use it.
pub fn shadow_md() -> Vec<gpui::BoxShadow> {
    vec![
        gpui::BoxShadow::new(px(0.), px(4.), gpui::hsla(0., 0., 0., 0.10))
            .blur_radius(px(6.))
            .spread_radius(px(-1.)),
        gpui::BoxShadow::new(px(0.), px(2.), gpui::hsla(0., 0., 0., 0.10))
            .blur_radius(px(4.))
            .spread_radius(px(-2.)),
    ]
}

#[cfg(test)]
mod tests {
    use super::tokens::{jade_dark, jade_light, kumo_dark, kumo_light};
    use super::{pack, scale, Size};
    use gpui::px;

    /// The four control sizes must resolve to the pixel values Kumo's class
    /// strings name: `h-5 rounded-sm px-1.5 text-xs`, `h-6.5 rounded-md px-2
    /// text-xs`, `h-9 rounded-lg px-3 text-base`, `h-10 rounded-lg px-4
    /// text-base`. A Kumo upgrade that moves one of these should fail here.
    #[test]
    fn the_size_axis_matches_the_kumo_class_table() {
        assert_eq!(
            (
                Size::Xs.height(),
                Size::Xs.padding_x(),
                Size::Xs.radius(),
                Size::Xs.text()
            ),
            (px(20.), px(6.), px(0.), px(12.))
        );
        assert_eq!(
            (
                Size::Sm.height(),
                Size::Sm.padding_x(),
                Size::Sm.radius(),
                Size::Sm.text()
            ),
            (px(26.), px(8.), px(0.), px(12.))
        );
        assert_eq!(
            (
                Size::Base.height(),
                Size::Base.padding_x(),
                Size::Base.radius(),
                Size::Base.text()
            ),
            (px(36.), px(12.), px(0.), px(14.))
        );
        assert_eq!(
            (
                Size::Lg.height(),
                Size::Lg.padding_x(),
                Size::Lg.radius(),
                Size::Lg.text()
            ),
            (px(40.), px(16.), px(0.), px(14.))
        );
    }

    /// A segmented tab trigger must clear its trough by Kumo's `my-0.5` on
    /// every edge. That 2px margin is the only thing keeping the active pill's
    /// corners away from the trough's radius; close the gap and the pill sits
    /// flush and gets its corners shaved by the parent's rounded clip.
    #[test]
    fn a_segmented_tab_pill_clears_its_trough_on_every_edge() {
        for size in [Size::Xs, Size::Sm, Size::Base, Size::Lg] {
            let slack = size.tab_bar_height() - size.tab_trigger_height();
            assert_eq!(slack, px(4.), "2px of trough above and below the pill");
        }
        // The trough keeps the standard control heights despite the margin.
        assert_eq!(Size::Sm.tab_bar_height(), Size::Sm.height());
        assert_eq!(Size::Base.tab_bar_height(), Size::Base.height());
    }

    /// `space(n)` is Tailwind's `0.25rem` unit, so the named constants and the
    /// helper must agree.
    #[test]
    fn the_spacing_helper_agrees_with_the_named_steps() {
        assert_eq!(scale::space(0.5), scale::SPACE_0_5);
        assert_eq!(scale::space(1.0), scale::SPACE_1);
        assert_eq!(scale::space(1.5), scale::SPACE_1_5);
        assert_eq!(scale::space(2.5), scale::SPACE_2_5);
        assert_eq!(scale::space(4.0), scale::SPACE_4);
        assert_eq!(scale::space(6.0), scale::SPACE_6);
    }

    /// Kumo's `@theme` block overrides the four text sizes away from the
    /// Tailwind defaults: 12 / 13 / 14 / 16, not 12 / 14 / 16 / 18.
    #[test]
    fn the_type_scale_is_kumos_override_not_tailwinds_default() {
        assert_eq!(scale::TEXT_XS, px(12.));
        assert_eq!(scale::TEXT_SM, px(13.));
        assert_eq!(scale::TEXT_BASE, px(14.));
        assert_eq!(scale::TEXT_LG, px(16.));
    }

    /// A spot check that the oklch -> sRGB conversion landed on Tailwind's own
    /// palette. `neutral-100`, `blue-400`, and `red-600` are the values Kumo's
    /// dark tokens reference by name.
    #[test]
    fn the_ported_kumo_tokens_land_on_tailwinds_palette() {
        let d = kumo_dark();
        assert_eq!(pack(d.text_default), 0xF5F5F5); // neutral-100
        assert_eq!(pack(d.text_link), 0x51A2FF); // blue-400
        assert_eq!(pack(d.danger), 0xE7000B); // red-600
        let l = kumo_light();
        assert_eq!(pack(l.text_subtle), 0x737373); // neutral-500
        assert_eq!(pack(l.base), 0xFFFFFF); // white
    }

    /// Every palette must fill every slot with a usable color. A token left at
    /// the `Default` zero would paint transparent black and read as a hole in
    /// the window, so check the surfaces and inks that must be opaque.
    #[test]
    fn every_palette_paints_its_surfaces_opaque() {
        for t in [jade_dark(), jade_light(), kumo_dark(), kumo_light()] {
            for surface in [t.canvas, t.elevated, t.recessed, t.base, t.control] {
                assert_eq!(surface.a, 1.0, "surfaces must be opaque");
            }
            for ink in [t.text_default, t.text_strong, t.text_subtle, t.brand] {
                assert_eq!(ink.a, 1.0, "inks must be opaque");
            }
            // The status tints are washes laid over a surface, so they must not
            // be opaque or they would hide the row behind them.
            for tint in [t.info_tint, t.warning_tint, t.danger_tint, t.success_tint] {
                assert!(tint.a < 1.0, "status tints must be translucent");
            }
        }
    }

    /// The dark palettes must actually be dark and the light ones light —
    /// cheap protection against a copy/paste that swaps two constructors.
    #[test]
    fn the_light_flag_agrees_with_the_canvas() {
        for t in [jade_dark(), kumo_dark()] {
            assert!(!t.is_light);
            assert!(t.canvas.r < 0.5 && t.text_default.r > 0.5);
        }
        for t in [jade_light(), kumo_light()] {
            assert!(t.is_light);
            assert!(t.canvas.r > 0.5 && t.text_default.r < 0.5);
        }
    }
}

#[cfg(test)]
mod ladder_tests {
    use super::pack;
    use super::tokens::{jade_dark, jade_light, kumo_dark, kumo_light, KumoTokens};

    /// Relative luminance, for ordering the surface steps.
    fn lum(c: gpui::Rgba) -> f32 {
        0.2126 * c.r + 0.7152 * c.g + 0.0722 * c.b
    }

    /// The surface ladder must never collapse. A control paints itself `base`
    /// and sits on `elevated`; if the two match, its fill vanishes and only the
    /// 1px ring survives — the button reads as a stray edge rather than a
    /// button. `overlay` and `control` must clear their neighbours for the same
    /// reason.
    #[test]
    fn no_two_adjacent_surfaces_collapse_onto_each_other() {
        for t in [jade_dark(), jade_light(), kumo_dark(), kumo_light()] {
            for (a, b, why) in [
                (t.base, t.elevated, "a card or button on a panel"),
                (t.elevated, t.canvas, "a panel on the window"),
                (t.recessed, t.elevated, "a tab trough on a panel"),
                (t.base, t.recessed, "an active tab pill in its trough"),
                (t.overlay, t.canvas, "a menu over the window"),
                (t.control, t.elevated, "an input on a panel"),
            ] {
                assert_ne!(pack(a), pack(b), "{why} would be invisible");
            }
        }
    }

    /// A hover wash must not land on the border color, or hovering a bordered
    /// control erases its own outline.
    #[test]
    fn the_hover_wash_clears_the_border_color() {
        for t in [jade_dark(), jade_light(), kumo_dark(), kumo_light()] {
            assert_ne!(pack(t.tint), pack(t.line));
            assert_ne!(pack(t.fill_hover), pack(t.line));
        }
    }

    /// `base` is the top of the ladder in every palette — it is what a card, a
    /// button, and an active tab pill paint themselves, so nothing may float
    /// above it. On a dark canvas the ladder also lightens on the way up.
    ///
    /// `recessed` is deliberately not in this ordering. Kumo puts it *above*
    /// `elevated` in its dark scale and *below* it in its light one, and the
    /// port keeps both; all that matters is that it clears its neighbours,
    /// which `no_two_adjacent_surfaces_collapse_onto_each_other` checks.
    #[test]
    fn base_is_the_top_of_every_ladder() {
        for t in [jade_dark(), jade_light(), kumo_dark(), kumo_light()] {
            for below in [t.canvas, t.elevated, t.recessed] {
                assert!(lum(below) < lum(t.base), "base must be the top surface");
            }
        }
        for t in [jade_dark(), kumo_dark()] {
            assert!(lum(t.canvas) < lum(t.elevated));
        }
    }

    /// The focus ring is the brand at half strength — a real ring, not an
    /// invisible one.
    #[test]
    fn the_focus_ring_is_translucent_but_visible() {
        let t: KumoTokens = jade_dark();
        assert_eq!(t.focus_ring().a, 0.5);
        assert_eq!(pack(t.focus_ring()), pack(t.focus));
    }
}
