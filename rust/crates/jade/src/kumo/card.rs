//! LayerCard and Surface — a port of
//! `packages/kumo/src/components/layer-card/layer-card.tsx` and
//! `.../surface/surface.tsx`.
//!
//! Kumo defines four class strings there, quoted on each builder below. A plain
//! [`Card`] is the `LAYER_CARD_SURFACE_CLASSES` shell. A layered card is the
//! `LAYER_CARD_LAYERED_ROOT_CLASSES` root holding a [`CardPrimary`] body and one
//! or more [`CardSecondary`] strips — the header/footer rails that sit on the
//! recessed layer behind the body.

use gpui::{div, prelude::*, Div};

use super::tokens::KumoTokens;
use super::{scale, shadow_xs};

/// `"overflow-hidden rounded-lg bg-kumo-base shadow-xs ring ring-kumo-line"`.
pub struct Card;

impl Card {
    /// The single-layer card: the default panel shell.
    pub fn new(t: &KumoTokens) -> Div {
        div()
            .overflow_hidden()
            
            .bg(t.base)
            .border_1()
            .border_color(t.line)
            .shadow(shadow_xs())
    }

    /// `"flex w-full flex-col overflow-hidden rounded-lg bg-kumo-elevated
    /// text-base ring ring-kumo-hairline"` — the root of a layered card.
    pub fn layered(t: &KumoTokens) -> Div {
        div()
            .flex()
            .flex_col()
            .w_full()
            .overflow_hidden()
            
            .bg(t.elevated)
            .text_size(scale::TEXT_BASE)
            .border_1()
            .border_color(t.hairline)
    }
}

/// `"relative flex flex-col gap-2 overflow-hidden rounded-lg bg-kumo-base p-4
/// pr-3 text-inherit no-underline ring ring-kumo-fill"` — the body layer.
pub struct CardPrimary;

impl CardPrimary {
    pub fn new(t: &KumoTokens) -> Div {
        div()
            .relative()
            .flex()
            .flex_col()
            .gap(scale::SPACE_2)
            .overflow_hidden()
            
            .bg(t.base)
            .p(scale::SPACE_4)
            .pr(scale::SPACE_3)
            .border_1()
            .border_color(t.fill)
    }
}

/// `"-my-2 flex items-center gap-2 bg-kumo-elevated p-4 text-base font-medium
/// text-kumo-subtle"` — the rail above or below the body layer.
pub struct CardSecondary;

impl CardSecondary {
    pub fn new(t: &KumoTokens) -> Div {
        div()
            .flex()
            .flex_row()
            .items_center()
            .gap(scale::SPACE_2)
            .bg(t.elevated)
            .p(scale::SPACE_4)
            .text_size(scale::TEXT_BASE)
            .font_weight(gpui::FontWeight::MEDIUM)
            .text_color(t.text_subtle)
    }
}

/// Surface — `packages/kumo/src/components/surface/surface.tsx`.
///
/// Surface carries no geometry of its own (`"overflow-visible rounded-none"`);
/// it only declares which background layer its subtree sits on, through
/// `data-surface-color`. The port makes that explicit: pick the layer and the
/// caller gets a bare `div` already painted with it.
pub struct Surface;

impl Surface {
    /// `data-surface-color="primary"` — the window canvas.
    pub fn canvas(t: &KumoTokens) -> Div {
        div().bg(t.canvas).text_color(t.text_default)
    }

    /// `data-surface-color="secondary"` — one step up from the canvas.
    pub fn elevated(t: &KumoTokens) -> Div {
        div().bg(t.elevated).text_color(t.text_default)
    }

    /// The well a segmented control or a tab trough sits in.
    pub fn recessed(t: &KumoTokens) -> Div {
        div().bg(t.recessed).text_color(t.text_default)
    }

    /// A menu or popover panel: `bg-kumo-overlay` with the standard ring.
    pub fn overlay(t: &KumoTokens) -> Div {
        div()
            .bg(t.overlay)
            .text_color(t.text_default)
            
            .border_1()
            .border_color(t.line)
            .shadow(super::shadow_md())
    }
}
