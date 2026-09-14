//! Empty — a port of `packages/kumo/src/components/empty/empty.tsx`.
//!
//! The shell is `"flex w-full flex-col items-center rounded-xl border
//! border-kumo-fill bg-kumo-control text-kumo-default"` with three paddings:
//! small `"px-6 py-8 gap-4"`, base `"px-10 py-16 gap-6"`, large `"px-12 py-20
//! gap-8"`. The title is `"text-2xl font-semibold"` and the body is
//! `"max-w-140 text-center text-kumo-subtle"`.

use gpui::{div, prelude::*, px, Div, SharedString};

use super::tokens::KumoTokens;
use super::{icon, scale, Size};

pub struct Empty {
    title: SharedString,
    body: Option<SharedString>,
    icon_name: Option<SharedString>,
    size: Size,
}

impl Empty {
    pub fn new(title: impl Into<SharedString>) -> Self {
        Self {
            title: title.into(),
            body: None,
            icon_name: None,
            size: Size::Base,
        }
    }

    pub fn body(mut self, body: impl Into<SharedString>) -> Self {
        self.body = Some(body.into());
        self
    }

    pub fn icon(mut self, name: impl Into<SharedString>) -> Self {
        self.icon_name = Some(name.into());
        self
    }

    pub fn size(mut self, size: Size) -> Self {
        self.size = size;
        self
    }

    pub fn render(self, t: &KumoTokens) -> Div {
        // `px-6 py-8 gap-4` | `px-10 py-16 gap-6` | `px-12 py-20 gap-8`.
        let (px_pad, py_pad, gap) = match self.size {
            Size::Xs | Size::Sm => (px(24.), px(28.), scale::SPACE_2_5),
            Size::Base => (px(40.), px(64.), scale::SPACE_6),
            Size::Lg => (px(48.), px(80.), px(32.)),
        };
        // The small sizes sit inside a sidebar or a panel, where Kumo's 24px
        // title would be the largest text on screen. They take the base
        // label size instead, with the body a step below it.
        let small = matches!(self.size, Size::Xs | Size::Sm);
        let (icon_px, title_px, body_px) = if small {
            (20., scale::TEXT_BASE, scale::TEXT_XS)
        } else {
            (28., scale::TEXT_2XL, scale::TEXT_BASE)
        };

        let mut el = div()
            .flex()
            .flex_col()
            .items_center()
            .justify_center()
            .w_full()
            .gap(gap)
            .px(px_pad)
            .py(py_pad)
            
            .border_1()
            .border_color(t.fill)
            .bg(t.control)
            .text_color(t.text_default);

        if let Some(name) = &self.icon_name {
            el = el.child(icon(name, icon_px, t.text_subtle));
        }
        el = el.child(
            div()
                .text_size(title_px)
                .font_weight(gpui::FontWeight::SEMIBOLD)
                .child(self.title),
        );
        if let Some(body) = self.body {
            el = el.child(
                div()
                    .max_w(px(560.)) // max-w-140
                    .text_center()
                    .text_size(body_px)
                    .text_color(t.text_subtle)
                    .child(body),
            );
        }
        el
    }
}
