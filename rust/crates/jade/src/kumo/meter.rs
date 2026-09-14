//! Meter — a port of `packages/kumo/src/components/meter/meter.tsx`.
//!
//! The track is `"relative h-2 w-full overflow-hidden rounded-full
//! bg-kumo-fill"`; the bar is `"absolute inset-y-0 left-0 rounded-full
//! bg-linear-to-r from-kumo-brand via-kumo-brand to-kumo-brand"`. The label row
//! above it pairs a `"text-xs text-kumo-subtle"` name with a `"text-sm
//! font-medium text-kumo-default tabular-nums"` value.

use gpui::{div, prelude::*, px, relative, Div, Rgba, SharedString};

use super::scale;
use super::tokens::KumoTokens;

pub struct Meter {
    label: Option<SharedString>,
    value_text: Option<SharedString>,
    /// 0.0 to 1.0. Values outside the range clamp, matching Kumo's
    /// `Math.min(100, Math.max(0, …))`.
    fraction: f32,
    /// Overrides the brand fill. Kumo hard-codes the brand; Jade colors a meter
    /// by what it measures (a memory bar goes danger as it fills).
    color: Option<Rgba>,
}

impl Meter {
    pub fn new(fraction: f32) -> Self {
        Self {
            label: None,
            value_text: None,
            fraction: fraction.clamp(0.0, 1.0),
            color: None,
        }
    }

    pub fn label(mut self, label: impl Into<SharedString>) -> Self {
        self.label = Some(label.into());
        self
    }

    pub fn value_text(mut self, text: impl Into<SharedString>) -> Self {
        self.value_text = Some(text.into());
        self
    }

    pub fn color(mut self, color: Rgba) -> Self {
        self.color = Some(color);
        self
    }

    pub fn render(self, t: &KumoTokens) -> Div {
        let fill = self.color.unwrap_or(t.brand);
        let mut root = div().flex().flex_col().gap(scale::SPACE_1).w_full();

        if self.label.is_some() || self.value_text.is_some() {
            let mut row = div()
                .flex()
                .flex_row()
                .items_center()
                .justify_between()
                .w_full();
            if let Some(l) = self.label {
                row = row.child(
                    div()
                        .text_size(scale::TEXT_XS)
                        .text_color(t.text_subtle)
                        .child(l),
                );
            }
            if let Some(v) = self.value_text {
                row = row.child(
                    div()
                        .text_size(scale::TEXT_SM)
                        .font_weight(gpui::FontWeight::MEDIUM)
                        .font_family(crate::fonts::mono_family()) // tabular-nums
                        .text_color(t.text_default)
                        .child(v),
                );
            }
            root = root.child(row);
        }

        root.child(
            div()
                .relative()
                .h(px(8.)) // h-2
                .w_full()
                .overflow_hidden()
                .bg(t.fill)
                .child(
                    div()
                        .absolute()
                        .top_0()
                        .bottom_0()
                        .left_0()
                        .w(relative(self.fraction))
                        .bg(fill),
                ),
        )
    }
}
