//! Banner — a port of `packages/kumo/src/components/banner/banner.tsx`.
//!
//! Four variants, each a tint/ink pair: info `"bg-kumo-info-tint
//! text-kumo-info"`, warning `"bg-kumo-warning-tint text-kumo-warning"`, danger
//! `"bg-kumo-danger-tint text-kumo-danger"`, neutral `"bg-kumo-contrast/5
//! text-kumo-default/70"`. Two sizes: base is `"items-start gap-3 rounded-lg
//! px-4 py-3 text-base"`, small is `"items-center gap-2 rounded-md px-3 py-2
//! text-sm"`. The title is `"leading-snug font-medium"`.

use gpui::{div, prelude::*, Div, SharedString};

use super::tokens::KumoTokens;
use super::{icon, scale};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BannerVariant {
    Info,
    Warning,
    Danger,
    Neutral,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BannerSize {
    Base,
    Small,
}

pub struct Banner {
    title: SharedString,
    body: Option<SharedString>,
    variant: BannerVariant,
    size: BannerSize,
    icon_name: Option<SharedString>,
}

impl Banner {
    pub fn new(title: impl Into<SharedString>) -> Self {
        Self {
            title: title.into(),
            body: None,
            variant: BannerVariant::Info,
            size: BannerSize::Base,
            icon_name: None,
        }
    }

    pub fn body(mut self, body: impl Into<SharedString>) -> Self {
        self.body = Some(body.into());
        self
    }

    pub fn variant(mut self, variant: BannerVariant) -> Self {
        self.variant = variant;
        self
    }

    pub fn size(mut self, size: BannerSize) -> Self {
        self.size = size;
        self
    }

    pub fn icon(mut self, name: impl Into<SharedString>) -> Self {
        self.icon_name = Some(name.into());
        self
    }

    /// The default icon for each variant, matching what Kumo picks when the
    /// caller passes none.
    fn default_icon(variant: BannerVariant) -> &'static str {
        match variant {
            BannerVariant::Info | BannerVariant::Neutral => "info",
            BannerVariant::Warning => "triangle-alert",
            BannerVariant::Danger => "circle-x",
        }
    }

    pub fn render(self, t: &KumoTokens) -> Div {
        let (bg, ink) = match self.variant {
            BannerVariant::Info => (t.info_tint, t.text_info),
            BannerVariant::Warning => (t.warning_tint, t.text_warning),
            BannerVariant::Danger => (t.danger_tint, t.text_danger),
            BannerVariant::Neutral => (
                KumoTokens::alpha(t.contrast, 0.05),
                KumoTokens::alpha(t.text_default, 0.7),
            ),
        };
        let small = matches!(self.size, BannerSize::Small);

        let mut el = div()
            .flex()
            .flex_row()
            .bg(bg)
            .text_color(ink)
            .gap(if small {
                scale::SPACE_2
            } else {
                scale::SPACE_3
            })
            .px(if small {
                scale::SPACE_3
            } else {
                scale::SPACE_4
            })
            .py(if small {
                scale::SPACE_2
            } else {
                scale::SPACE_3
            })
            .text_size(if small {
                scale::TEXT_SM
            } else {
                scale::TEXT_BASE
            });

        el = if small {
            el.items_center()
        } else {
            el.items_start()
        };

        let name = self
            .icon_name
            .clone()
            .unwrap_or_else(|| Self::default_icon(self.variant).into());
        el = el.child(icon(&name, if small { 13. } else { 15. }, ink));

        let mut text = div().flex().flex_col().gap(scale::SPACE_0_5).child(
            div()
                .font_weight(gpui::FontWeight::MEDIUM)
                .child(self.title),
        );
        if let Some(body) = self.body {
            text = text.child(
                div()
                    .text_size(scale::TEXT_SM)
                    .text_color(KumoTokens::alpha(ink, 0.85))
                    .child(body),
            );
        }
        el.child(text)
    }
}
