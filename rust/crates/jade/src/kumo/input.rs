//! Input — a port of `packages/kumo/src/components/input/input.tsx`.
//!
//! Base: `"border-0 bg-kumo-control text-kumo-default ring ring-kumo-line
//! outline-none"`. The size axis matches Button exactly (`h-5 rounded-sm px-1.5
//! text-xs` … `h-10 rounded-lg px-4 text-base`), and the variant axis is the
//! focus ring: default is `"focus:ring-kumo-focus/50 focus:ring-[1.5px]"`,
//! error is `"!ring-kumo-danger focus:ring-kumo-danger/50"`.
//!
//! GPUI has no text input primitive. Jade already runs its own caret + key
//! handling (see `crate::app::dim_input`), so this renders the *shell* and the
//! caller drops the live text and caret in as children — which is also how the
//! find bar and the benchmark-name field already work.

use gpui::{div, prelude::*, px, Div, SharedString};

use super::tokens::KumoTokens;
use super::{icon, Size};

/// `KUMO_INPUT_VARIANTS.variant`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InputVariant {
    Default,
    Error,
}

pub struct TextField {
    value: SharedString,
    placeholder: Option<SharedString>,
    icon_name: Option<SharedString>,
    size: Size,
    variant: InputVariant,
    focused: bool,
    /// Draws the 1px caret after the value. The caller owns the blink phase.
    caret: bool,
    monospace: bool,
    full_width: bool,
}

impl TextField {
    pub fn new(value: impl Into<SharedString>) -> Self {
        Self {
            value: value.into(),
            placeholder: None,
            icon_name: None,
            size: Size::Base,
            variant: InputVariant::Default,
            focused: false,
            caret: false,
            monospace: false,
            full_width: false,
        }
    }

    pub fn placeholder(mut self, text: impl Into<SharedString>) -> Self {
        self.placeholder = Some(text.into());
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

    pub fn variant(mut self, variant: InputVariant) -> Self {
        self.variant = variant;
        self
    }

    pub fn focused(mut self, on: bool) -> Self {
        self.focused = on;
        self
    }

    pub fn caret(mut self, on: bool) -> Self {
        self.caret = on;
        self
    }

    pub fn monospace(mut self, on: bool) -> Self {
        self.monospace = on;
        self
    }

    pub fn full_width(mut self, on: bool) -> Self {
        self.full_width = on;
        self
    }

    pub fn render(self, t: &KumoTokens) -> Div {
        // `ring` is 1px; the focused ring is `ring-[1.5px]`. GPUI borders take
        // whole pixels well, so focus goes to 2px and the color carries the
        // state, which is what reads at this size anyway.
        let (ring, ring_w) = match (self.variant, self.focused) {
            (InputVariant::Error, true) => (KumoTokens::alpha(t.danger, 0.5), px(2.)),
            (InputVariant::Error, false) => (t.danger, px(1.)),
            (InputVariant::Default, true) => (t.focus_ring(), px(2.)),
            (InputVariant::Default, false) => (t.line, px(1.)),
        };

        let mut el = div()
            .flex()
            .flex_row()
            .items_center()
            .h(self.size.height())
            .gap(self.size.gap())
            .px(self.size.padding_x())
            .rounded(self.size.radius())
            .bg(t.control)
            .text_size(self.size.text())
            .text_color(t.text_default)
            .border(ring_w)
            .border_color(ring);

        if self.full_width {
            el = el.w_full();
        }
        if self.monospace {
            el = el.font_family(crate::fonts::mono_family());
        }
        if let Some(name) = &self.icon_name {
            el = el.child(icon(name, self.size.icon(), t.text_subtle));
        }

        // `kumo-input-placeholder` — the placeholder shows only while empty.
        if self.value.is_empty() {
            if let Some(ph) = self.placeholder {
                el = el.child(div().text_color(t.text_placeholder).child(ph));
            }
        } else {
            el = el.child(div().child(self.value));
        }

        if self.caret {
            el = el.child(div().w(px(1.)).h(self.size.text() * 1.2).bg(t.text_default));
        }
        el
    }
}
