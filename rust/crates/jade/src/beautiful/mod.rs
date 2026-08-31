//! A GPUI port of the agent-interface primitives from Beautiful UI
//! (<https://www.beautifului.dev>, MIT, Copyright (c) 2026 Shane Levine).
//!
//! Beautiful UI is a React + Tailwind library of components for AI-native
//! interfaces — thinking traces, streaming text, tool chips, approval flows.
//! Jade uses it for exactly that surface: the Explain and Visualize cards. The
//! rest of the app stays on [`crate::kumo`].
//!
//! Two design systems in one app is a deliberate choice, not an accident. The
//! agent surface has a vocabulary the rest of the IDE does not need — a
//! shimmer that means "a model is working", a three-level ink hierarchy for
//! dense metadata, a status tint for every terminal state — and Kumo has no
//! equivalents. The two share a charcoal family, so they sit together without
//! either one looking pasted in.
//!
//! Like the Kumo port, each builder quotes the Tailwind class string it came
//! from, so an upstream change is a textual diff.
//!
//! # The three primitives
//!
//! Almost every component in the library is built from these:
//!
//!   - the **card** — [`Card`], banded by [`Card::pad`] / [`Card::bar`] /
//!     [`Card::footer`];
//!   - the **chip** — [`chip`], 22px tall on `field` with a hairline ring;
//!   - the **icon button** — [`icon_button`], 28px square.
//!
//! # Borders are shadows
//!
//! Beautiful UI draws almost no real borders. Every elevation token begins with
//! a `0 0 0 1px` ring:
//!
//! ```text
//! --shadow-hairline: 0 0 0 1px var(--line);
//! --shadow-btn:      0 0 0 1px var(--line-strong), var(--shadow-xs);
//! --shadow-card:     0 0 0 1px var(--line), var(--shadow-sm);
//! ```
//!
//! GPUI has real borders and its `shadow` does not do spread rings, so the port
//! renders the ring as a 1px border and keeps the soft drop separately. The
//! visual result matches; the mechanism does not.

pub mod loader;
pub mod markdown;
pub mod stream;
pub mod tokens;

use gpui::{div, prelude::*, px, BoxShadow, Div, ElementId, SharedString, Stateful};

pub use loader::{pixel_loader, LoaderPattern};
pub use tokens::{dark, BeautifulTokens};

/// The four named radii. The library uses almost nothing else.
///
/// ```text
/// --radius-chip:6px; --radius-control:8px; --radius-card:10px;
/// .rounded-window{border-radius:14px}
/// ```
pub mod radius {
    use gpui::{px, Pixels};
    pub const CHIP: Pixels = px(6.);
    pub const CONTROL: Pixels = px(8.);
    pub const CARD: Pixels = px(10.);
    pub const WINDOW: Pixels = px(14.);
    /// `rounded-full` on a short control.
    pub const PILL: Pixels = px(9999.);
}

/// The type scale, in half-pixel steps. Everything is 10.5–13px except one
/// 17px figure size; there is no 14px+ UI text in the library.
pub mod text {
    use gpui::{px, Pixels};
    /// Metadata, domains, diff counts. Usually `font-mono`.
    pub const META: Pixels = px(10.5);
    pub const XS: Pixels = px(11.);
    /// Chips, pills, secondary metadata.
    pub const CHIP: Pixels = px(11.5);
    /// Captions and row metadata.
    pub const SM: Pixels = px(12.);
    /// Secondary body, row labels.
    pub const BODY: Pixels = px(12.5);
    /// Titles, primary labels, streamed prose.
    pub const LG: Pixels = px(13.);
    /// The single large size, for headline figures only.
    pub const FIGURE: Pixels = px(17.);
}

/// The house easing curve, `cubic-bezier(.23, 1, .32, 1)` (`--ease-out-strong`).
/// Nearly every transition in the library uses it.
///
/// GPUI's animation API takes an easing function over `0.0..=1.0`, so this is
/// the cubic-bezier evaluated by binary search on its x parameter — closed form
/// would need a cubic root solve for no visible gain at these durations.
pub fn ease_out_strong(t: f32) -> f32 {
    cubic_bezier(0.23, 1.0, 0.32, 1.0, t)
}

/// `cubic-bezier(.16, 1, .3, 1)` (`--ease-link`), used by the underline reveal.
pub fn ease_link(t: f32) -> f32 {
    cubic_bezier(0.16, 1.0, 0.3, 1.0, t)
}

fn cubic_bezier(x1: f32, y1: f32, x2: f32, y2: f32, t: f32) -> f32 {
    let t = t.clamp(0.0, 1.0);
    let bez = |a: f32, b: f32, u: f32| {
        let v = 1.0 - u;
        3.0 * v * v * u * a + 3.0 * v * u * u * b + u * u * u
    };
    // Solve x(u) == t for u. 18 halvings is well under a pixel of error at any
    // duration this library uses.
    let (mut lo, mut hi) = (0.0f32, 1.0f32);
    for _ in 0..18 {
        let mid = 0.5 * (lo + hi);
        if bez(x1, x2, mid) < t {
            lo = mid;
        } else {
            hi = mid;
        }
    }
    bez(y1, y2, 0.5 * (lo + hi))
}

/// `--shadow-sm`, the soft six-layer drop under a card. Very wide, very faint;
/// the ring that accompanies it in CSS is rendered as a border instead.
pub fn shadow_card() -> Vec<BoxShadow> {
    vec![BoxShadow::new(px(0.), px(8.), gpui::hsla(0., 0., 0., 0.34)).blur_radius(px(24.))]
}

/// `--shadow-overlay`, for a floating bar or popover.
pub fn shadow_overlay() -> Vec<BoxShadow> {
    vec![BoxShadow::new(px(0.), px(14.), gpui::hsla(0., 0., 0., 0.48)).blur_radius(px(40.))]
}

/// The card shell and its three bands.
///
/// ```text
/// card:   overflow-hidden rounded-card bg-surface shadow-card
/// pad:    .primitive-card-pad    { padding: 12px }
/// bar:    .primitive-card-bar    { padding: 10px 12px }
/// footer: .primitive-card-footer { padding: 10px 12px }
/// ```
pub struct Card;

impl Card {
    pub fn new(t: &BeautifulTokens) -> Div {
        div()
            .overflow_hidden()
            .rounded(radius::CARD)
            .bg(t.surface)
            .border_1()
            .border_color(t.line)
            .shadow(shadow_card())
    }

    /// A flatter card — hairline ring, no drop. Used where several cards stack
    /// and a shadow on each would read as noise.
    pub fn flat(t: &BeautifulTokens) -> Div {
        div()
            .overflow_hidden()
            .rounded(radius::CARD)
            .bg(t.surface)
            .border_1()
            .border_color(t.line)
    }

    /// The body band: 12px on every side.
    pub fn pad(_t: &BeautifulTokens) -> Div {
        div().p(px(12.))
    }

    /// A header rail: 10px vertical, 12px horizontal, separated by a real line.
    pub fn bar(t: &BeautifulTokens) -> Div {
        div()
            .flex()
            .flex_row()
            .items_center()
            .gap(px(10.))
            .px(px(12.))
            .py(px(10.))
            .border_b_1()
            .border_color(t.line)
    }

    /// A footer rail. Same metrics as [`Card::bar`], line on top.
    pub fn footer(t: &BeautifulTokens) -> Div {
        div()
            .flex()
            .flex_row()
            .items_center()
            .gap(px(10.))
            .px(px(12.))
            .py(px(10.))
            .border_t_1()
            .border_color(t.line)
    }
}

/// A chip: 22px tall, 6px radius, `field` fill, hairline ring, 11.5px text.
///
/// ```text
/// inline-flex h-5.5 items-center rounded-chip bg-field px-1.5
/// text-[11.5px] text-ink-2 shadow-hairline
/// ```
pub fn chip(t: &BeautifulTokens, label: impl Into<SharedString>) -> Div {
    div()
        .flex()
        .flex_row()
        .items_center()
        .gap(px(6.))
        .h(px(22.))
        .px(px(6.))
        .rounded(radius::CHIP)
        .bg(t.field)
        .border_1()
        .border_color(t.line)
        .text_size(text::CHIP)
        .text_color(t.ink_2)
        .child(label.into())
}

/// A status pill: saturated text over its own 14% tint, fully round.
///
/// ```text
/// inline-flex h-5.5 items-center rounded-full bg-green-tint px-2
/// text-[11.5px] font-medium text-green
/// ```
///
/// Status is ALWAYS this pair. Never the accent, and never a bare border —
/// that is the rule the whole library's state signalling rests on.
pub fn status_pill(
    label: impl Into<SharedString>,
    solid: gpui::Rgba,
    tint: gpui::Rgba,
) -> Div {
    div()
        .flex()
        .flex_row()
        .items_center()
        .gap(px(6.))
        .h(px(22.))
        .px(px(8.))
        .rounded(radius::PILL)
        .bg(tint)
        .text_size(text::CHIP)
        .font_weight(gpui::FontWeight::MEDIUM)
        .text_color(solid)
        .child(label.into())
}

/// A 28px square icon button.
///
/// ```text
/// .primitive-icon-button { border-radius: var(--radius-control);
///   width: 28px; height: 28px; display: inline-flex;
///   align-items: center; justify-content: center }
/// ```
/// plus `text-ink-3 hover:bg-hover hover:text-ink`.
pub fn icon_button(
    id: impl Into<ElementId>,
    icon_name: &'static str,
    t: &BeautifulTokens,
) -> Stateful<Div> {
    let hover_bg = t.hover;
    div()
        .id(id)
        .flex()
        .items_center()
        .justify_center()
        .size(px(28.))
        .flex_none()
        .rounded(radius::CONTROL)
        .text_color(t.ink_3)
        .cursor_pointer()
        .hover(move |s| s.bg(hover_bg))
        // 15px, `strokeWidth 1.8` in the source.
        .child(crate::kumo::icon(icon_name, 15.0, t.ink_3))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The easing must be a real ease-out: fast at the start, settling at the
    /// end. A linear or reversed curve here would make every animation in the
    /// port feel wrong in a way that is hard to see in a still.
    #[test]
    fn ease_out_strong_is_monotonic_and_front_loaded() {
        assert!((ease_out_strong(0.0) - 0.0).abs() < 1e-3);
        assert!((ease_out_strong(1.0) - 1.0).abs() < 1e-3);
        let mut prev = 0.0;
        for i in 0..=100 {
            let v = ease_out_strong(i as f32 / 100.0);
            assert!(v >= prev - 1e-4, "not monotonic at {i}: {prev} then {v}");
            prev = v;
        }
        // Over half the distance is covered in the first fifth of the time.
        assert!(
            ease_out_strong(0.2) > 0.5,
            "expected a front-loaded curve, got {}",
            ease_out_strong(0.2)
        );
    }

    #[test]
    fn ease_link_is_even_more_front_loaded() {
        assert!(ease_link(0.2) > ease_out_strong(0.2));
        assert!((ease_link(1.0) - 1.0).abs() < 1e-3);
    }

    /// Out-of-range inputs are clamped, not extrapolated — an animation driver
    /// that overshoots must not send a color or an offset past its endpoint.
    #[test]
    fn easing_clamps_out_of_range_input() {
        assert!((ease_out_strong(-1.0) - 0.0).abs() < 1e-3);
        assert!((ease_out_strong(2.0) - 1.0).abs() < 1e-3);
    }
}
