//! Badge — a port of `packages/kumo/src/components/badge/badge.tsx`.
//!
//! `KUMO_BADGE_BASE_STYLES` is
//! `"inline-flex w-fit flex-none shrink-0 items-center justify-self-start
//! rounded-full px-2 py-0.5 text-xs font-medium whitespace-nowrap"`, and
//! `KUMO_BADGE_VARIANTS` supplies the fill/ink pair. The `dot` appearance
//! replaces the fill with a hairline ring and a colored dot, so the variant
//! colors do not apply — the same branch Kumo takes in `badgeVariants`.

use gpui::{div, prelude::*, px, Div, Rgba, SharedString};

use super::tokens::KumoTokens;
use super::{icon, scale};

/// The stand-in for "no ring", matching [`super::button`].
const TRANSPARENT: Rgba = Rgba {
    r: 0.0,
    g: 0.0,
    b: 0.0,
    a: 0.0,
};

/// `KUMO_BADGE_VARIANTS.variant`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BadgeVariant {
    /// `bg-kumo-badge-inverted text-kumo-badge-inverted` — the default.
    Primary,
    /// `bg-kumo-fill text-kumo-badge-neutral-subtle`.
    Secondary,
    /// `bg-kumo-danger-tint text-kumo-danger`.
    Error,
    /// `bg-kumo-warning-tint text-kumo-warning`.
    Warning,
    /// `bg-kumo-success-tint text-kumo-success`.
    Success,
    /// `bg-kumo-info-tint text-kumo-info`.
    Info,
    /// `border border-dashed border-kumo-brand bg-transparent text-kumo-link`.
    Beta,
    /// `border border-kumo-fill bg-transparent text-kumo-default`.
    Outline,
    Red,
    Green,
    Neutral,
    Orange,
    Purple,
    Teal,
    Blue,
}

/// `KUMO_BADGE_VARIANTS.appearance`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BadgeAppearance {
    /// `""` — the variant's fill shows.
    Filled,
    /// `gap-1.5 bg-transparent text-kumo-default ring ring-kumo-hairline`.
    Dot,
}

/// `KUMO_BADGE_VARIANTS.dotColor`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DotColor {
    None,
    /// `bg-kumo-success`.
    Success,
    /// `bg-kumo-badge-orange`.
    Warning,
    /// `bg-kumo-badge-red`.
    Error,
    /// `bg-kumo-badge-neutral`.
    Neutral,
}

pub struct Badge {
    label: SharedString,
    icon_name: Option<SharedString>,
    variant: BadgeVariant,
    appearance: BadgeAppearance,
    dot_color: DotColor,
    /// Not a Kumo prop. Jade shows counts in badges, and a count reads better
    /// with the digits on a fixed advance.
    tabular: bool,
}

impl Badge {
    pub fn new(label: impl Into<SharedString>) -> Self {
        Self {
            label: label.into(),
            icon_name: None,
            variant: BadgeVariant::Primary,
            appearance: BadgeAppearance::Filled,
            dot_color: DotColor::None,
            tabular: false,
        }
    }

    pub fn icon(mut self, name: impl Into<SharedString>) -> Self {
        self.icon_name = Some(name.into());
        self
    }

    pub fn variant(mut self, variant: BadgeVariant) -> Self {
        self.variant = variant;
        self
    }

    pub fn appearance(mut self, appearance: BadgeAppearance) -> Self {
        self.appearance = appearance;
        self
    }

    pub fn dot(mut self, color: DotColor) -> Self {
        self.appearance = BadgeAppearance::Dot;
        self.dot_color = color;
        self
    }

    pub fn tabular(mut self, on: bool) -> Self {
        self.tabular = on;
        self
    }

    /// The (background, foreground) pair for a variant, straight off the
    /// `KUMO_BADGE_VARIANTS.variant` table.
    fn colors(variant: BadgeVariant, t: &KumoTokens) -> (Option<Rgba>, Rgba) {
        let white = gpui::rgb(0xFFFFFF);
        match variant {
            BadgeVariant::Primary => (Some(t.badge_inverted), t.text_badge_inverted),
            BadgeVariant::Secondary => (Some(t.fill), t.text_badge_neutral_subtle),
            BadgeVariant::Error => (Some(t.danger_tint), t.text_danger),
            BadgeVariant::Warning => (Some(t.warning_tint), t.text_warning),
            BadgeVariant::Success => (Some(t.success_tint), t.text_success),
            BadgeVariant::Info => (Some(t.info_tint), t.text_info),
            BadgeVariant::Beta => (None, t.text_link),
            BadgeVariant::Outline => (None, t.text_default),
            BadgeVariant::Red => (Some(t.badge_red), white),
            BadgeVariant::Green => (Some(t.badge_green), white),
            BadgeVariant::Neutral => (Some(t.badge_neutral), white),
            BadgeVariant::Orange => (Some(t.badge_orange), gpui::rgb(0x000000)),
            BadgeVariant::Purple => (Some(t.badge_purple), white),
            BadgeVariant::Teal => (Some(t.badge_teal), white),
            BadgeVariant::Blue => (Some(t.badge_blue), white),
        }
    }

    pub fn render(self, t: &KumoTokens) -> Div {
        let dot_bg = match self.dot_color {
            DotColor::None => None,
            DotColor::Success => Some(t.success),
            DotColor::Warning => Some(t.badge_orange),
            DotColor::Error => Some(t.badge_red),
            DotColor::Neutral => Some(t.badge_neutral),
        };

        // Base styles. Like Button, every appearance carries the same 1px
        // border — transparent where there is no ring — so a filled badge and
        // an outlined one in the same row keep identical heights and their
        // labels stay on one baseline.
        let mut el = div()
            .flex()
            .flex_row()
            .flex_none()
            .items_center()
            .border_1()
            .border_color(TRANSPARENT)
            .px(scale::SPACE_2)
            .py(scale::SPACE_0_5)
            .text_size(scale::TEXT_XS)
            .font_weight(gpui::FontWeight::MEDIUM);

        let ink;
        match self.appearance {
            BadgeAppearance::Dot => {
                // The dot appearance drops the variant fill entirely.
                ink = t.text_default;
                el = el
                    .gap(scale::SPACE_1_5)
                    .border_color(t.hairline)
                    .text_color(ink);
                if let Some(c) = dot_bg {
                    el = el.child(div().size(px(6.)).rounded_full().bg(c));
                }
            }
            BadgeAppearance::Filled => {
                let (bg, fg) = Self::colors(self.variant, t);
                ink = fg;
                el = el.gap(scale::SPACE_1).text_color(fg);
                if let Some(bg) = bg {
                    el = el.bg(bg);
                }
                match self.variant {
                    // `border border-dashed border-kumo-brand`. GPUI has no
                    // dashed border, so a solid brand hairline stands in.
                    BadgeVariant::Beta => el = el.border_color(t.brand),
                    BadgeVariant::Outline => el = el.border_color(t.fill),
                    _ => {}
                }
            }
        }

        if let Some(name) = &self.icon_name {
            el = el.child(icon(name, 11., ink));
        }
        if self.tabular {
            el = el.font_family(crate::fonts::mono_family());
        }
        el.child(self.label)
    }
}
