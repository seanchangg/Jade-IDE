//! Text — a port of `packages/kumo/src/components/text/text.tsx`.
//!
//! `KUMO_TEXT_VARIANTS` crosses a tone (`body | secondary | success | error |
//! mono | mono-secondary`) with a size (`xs | sm | base | lg`), and folds the
//! three headings into the same table (`heading1` is `"text-3xl font-semibold"`,
//! `heading2` `"text-2xl font-semibold"`, `heading3` `"text-lg font-semibold"`).
//! The headings are split out into [`Heading`] here, because in the port a
//! heading sets its own size and the tone axis no longer applies to it.

use gpui::{div, prelude::*, Div, SharedString};

use super::tokens::KumoTokens;
use super::{scale, Size};

/// `KUMO_TEXT_VARIANTS.variant`, minus the headings.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TextTone {
    /// `text-kumo-default`.
    Body,
    /// `text-kumo-subtle`.
    Secondary,
    /// `text-kumo-link`. Kumo names this variant `success` but styles it with
    /// the link token; the port keeps that behavior rather than "fixing" it, so
    /// a Kumo upgrade stays a straight diff.
    Success,
    /// `text-kumo-danger`.
    Error,
    /// `font-mono`.
    Mono,
    /// `font-mono text-kumo-subtle`.
    MonoSecondary,
}

pub struct Text {
    content: SharedString,
    tone: TextTone,
    size: Size,
    /// Kumo reaches for `font-medium` inline rather than through the variant
    /// table; label rows in Jade need it often enough to be a builder method.
    medium: bool,
}

impl Text {
    pub fn new(content: impl Into<SharedString>) -> Self {
        Self {
            content: content.into(),
            tone: TextTone::Body,
            size: Size::Base,
            medium: false,
        }
    }

    pub fn tone(mut self, tone: TextTone) -> Self {
        self.tone = tone;
        self
    }

    pub fn size(mut self, size: Size) -> Self {
        self.size = size;
        self
    }

    pub fn medium(mut self, on: bool) -> Self {
        self.medium = on;
        self
    }

    /// The size classes: `text-xs | text-sm | text-base | text-lg`.
    fn size_px(size: Size) -> gpui::Pixels {
        match size {
            Size::Xs => scale::TEXT_XS,
            Size::Sm => scale::TEXT_SM,
            Size::Base => scale::TEXT_BASE,
            Size::Lg => scale::TEXT_LG,
        }
    }

    pub fn render(self, t: &KumoTokens) -> Div {
        let mut el = div().text_size(Self::size_px(self.size));
        el = match self.tone {
            TextTone::Body => el.text_color(t.text_default),
            TextTone::Secondary => el.text_color(t.text_subtle),
            TextTone::Success => el.text_color(t.text_link),
            TextTone::Error => el.text_color(t.text_danger),
            TextTone::Mono => el.font_family(crate::fonts::mono_family()).text_color(t.text_default),
            TextTone::MonoSecondary => el.font_family(crate::fonts::mono_family()).text_color(t.text_subtle),
        };
        if self.medium {
            el = el.font_weight(gpui::FontWeight::MEDIUM);
        }
        el.child(self.content)
    }
}

/// The three heading variants.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HeadingLevel {
    /// `text-3xl font-semibold`.
    One,
    /// `text-2xl font-semibold`.
    Two,
    /// `text-lg font-semibold`.
    Three,
}

pub struct Heading {
    content: SharedString,
    level: HeadingLevel,
}

impl Heading {
    pub fn new(level: HeadingLevel, content: impl Into<SharedString>) -> Self {
        Self {
            content: content.into(),
            level,
        }
    }

    pub fn render(self, t: &KumoTokens) -> Div {
        let size = match self.level {
            HeadingLevel::One => scale::TEXT_3XL,
            HeadingLevel::Two => scale::TEXT_2XL,
            HeadingLevel::Three => scale::TEXT_LG,
        };
        div()
            .text_size(size)
            .font_weight(gpui::FontWeight::SEMIBOLD)
            .text_color(t.text_strong)
            .child(self.content)
    }
}
