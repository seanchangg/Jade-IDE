//! Tooltip — a port of `packages/kumo/src/components/tooltip/tooltip.tsx`.
//!
//! The popup is `"flex origin-[var(--transform-origin)] flex-col rounded-md
//! bg-kumo-base px-2.5 py-1.5 text-sm text-kumo-default"`, with the
//! `--color-kumo-tip-stroke` hairline that only the dark theme paints.
//!
//! Positioning is the caller's job. GPUI has no floating-element engine, so this
//! renders the panel and the caller anchors it (`.absolute()` inside a
//! `.relative()` parent, the pattern the rest of the Jade overlays use).

use gpui::{div, prelude::*, Div, SharedString};

use super::tokens::KumoTokens;
use super::{scale, shadow_md};

/// The tooltip panel.
pub fn tooltip_panel(text: impl Into<SharedString>, t: &KumoTokens) -> Div {
    div()
        .flex()
        .flex_col()
        
        .bg(t.base)
        .px(scale::SPACE_2_5)
        .py(scale::SPACE_1_5)
        .text_size(scale::TEXT_SM)
        .text_color(t.text_default)
        .border_1()
        .border_color(t.tip_stroke)
        .shadow(shadow_md())
        .child(text.into())
}
