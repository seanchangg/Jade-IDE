//! Button — a port of `packages/kumo/src/components/button/button.tsx`.
//!
//! Copies `KUMO_BUTTON_VARIANTS` exactly: three shapes, four sizes, and six
//! visual variants, with `secondary` / `base` / `base` as the defaults that
//! `KUMO_BUTTON_DEFAULT_VARIANTS` sets.

use gpui::{div, prelude::*, px, rgb, Div, ElementId, Rgba, SharedString, Stateful};

use super::tokens::KumoTokens;
use super::{icon, shadow_xs, Size};

/// The stand-in for "no ring". See the note in [`Button::render`].
const TRANSPARENT: Rgba = Rgba {
    r: 0.0,
    g: 0.0,
    b: 0.0,
    a: 0.0,
};

/// `KUMO_BUTTON_VARIANTS.shape`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ButtonShape {
    /// `""` — the default rectangular button.
    Base,
    /// `"items-center justify-center p-0"` plus `compactSize` — a square icon
    /// button.
    Square,
    /// `"items-center justify-center p-0 rounded-full"` — a circular icon button.
    Circle,
}

/// `KUMO_BUTTON_VARIANTS.variant`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ButtonVariant {
    /// `bg-(--kumo-button-emphasis-bg) !text-white ring` — the brand fill.
    Primary,
    /// `bg-kumo-base !text-kumo-default ring ring-kumo-line hover:bg-kumo-tint`.
    Secondary,
    /// `text-kumo-default hover:bg-kumo-tint shadow-none bg-inherit`.
    Ghost,
    /// The danger fill. Same shape as `Primary` with the danger emphasis color.
    Destructive,
    /// `bg-kumo-base !text-kumo-danger ring ring-kumo-line` — danger text on the
    /// secondary shell.
    SecondaryDestructive,
    /// `bg-transparent text-kumo-default ring ring-kumo-line`.
    Outline,
    /// Not a Kumo variant. The old Jade action bar drew Build and Run as a
    /// colored outline: the label in a status color, a ring of that color at
    /// 30%, and a 14% wash of it on hover. `ink` supplies the color.
    Tinted,
}

/// A Kumo button.
///
/// `render` returns the styled element; the caller attaches `on_click`, because
/// GPUI listeners are bound to the concrete view type.
pub struct Button {
    id: ElementId,
    label: Option<SharedString>,
    icon_name: Option<SharedString>,
    /// Kumo renders a trailing icon after the label (`iconRight`).
    icon_right: Option<SharedString>,
    size: Size,
    variant: ButtonVariant,
    shape: ButtonShape,
    disabled: bool,
    /// `data-[state=open]` — a menu trigger whose menu is showing keeps the
    /// hover fill.
    active: bool,
    full_width: bool,
    /// Overrides the label + icon color. Kumo reaches for `!text-*` here; the
    /// Jade action bar uses it to tint a toggle with the brand while it is on.
    ink: Option<Rgba>,
}

impl Button {
    pub fn new(id: impl Into<ElementId>, label: impl Into<SharedString>) -> Self {
        Self {
            id: id.into(),
            label: Some(label.into()),
            icon_name: None,
            icon_right: None,
            size: Size::Base,
            variant: ButtonVariant::Secondary,
            shape: ButtonShape::Base,
            disabled: false,
            active: false,
            full_width: false,
            ink: None,
        }
    }

    /// An icon-only button. Kumo calls this `shape="square"`; the square shape
    /// drops the horizontal padding and locks the width to the height.
    pub fn icon_only(id: impl Into<ElementId>, icon_name: impl Into<SharedString>) -> Self {
        Self {
            id: id.into(),
            label: None,
            icon_name: Some(icon_name.into()),
            icon_right: None,
            size: Size::Base,
            variant: ButtonVariant::Ghost,
            shape: ButtonShape::Square,
            disabled: false,
            active: false,
            full_width: false,
            ink: None,
        }
    }

    /// Force the label + icon color, the way Kumo's `!text-*` utilities do.
    pub fn ink(mut self, color: Rgba) -> Self {
        self.ink = Some(color);
        self
    }

    pub fn icon(mut self, name: impl Into<SharedString>) -> Self {
        self.icon_name = Some(name.into());
        self
    }

    pub fn icon_right(mut self, name: impl Into<SharedString>) -> Self {
        self.icon_right = Some(name.into());
        self
    }

    pub fn size(mut self, size: Size) -> Self {
        self.size = size;
        self
    }

    pub fn variant(mut self, variant: ButtonVariant) -> Self {
        self.variant = variant;
        self
    }

    pub fn shape(mut self, shape: ButtonShape) -> Self {
        self.shape = shape;
        self
    }

    pub fn disabled(mut self, disabled: bool) -> Self {
        self.disabled = disabled;
        self
    }

    pub fn active(mut self, active: bool) -> Self {
        self.active = active;
        self
    }

    pub fn full_width(mut self, full: bool) -> Self {
        self.full_width = full;
        self
    }

    pub fn render(self, t: &KumoTokens) -> Stateful<Div> {
        let compact = matches!(self.shape, ButtonShape::Square | ButtonShape::Circle);

        // Base styles: "group flex w-max shrink-0 items-center font-medium
        // select-none border-0 shadow-xs cursor-pointer".
        //
        // Kumo rings its buttons with Tailwind's `ring`, which is a box-shadow
        // and costs no layout. GPUI has no ring, so the port draws a real
        // border — and a border eats a pixel off each edge of the content box.
        // Every variant therefore carries the same 1px border, transparent
        // where the variant has no ring, so a Ghost button and a Secondary
        // button side by side lay their labels out on the same box. Without
        // this the two sit 1px apart on the action bar.
        let mut el = div()
            .border_1()
            .border_color(TRANSPARENT)
            .id(self.id)
            .flex()
            .flex_row()
            .flex_none()
            .items_center()
            .h(self.size.height())
            .gap(self.size.gap())
            .text_size(self.size.text())
            .font_weight(gpui::FontWeight::MEDIUM);

        if self.full_width {
            el = el.w_full().justify_center();
        }

        // shape
        match self.shape {
            ButtonShape::Base => {
                el = el.px(self.size.padding_x());
            }
            ButtonShape::Square => {
                // "items-center justify-center p-0" + compactSize `size-*`.
                el = el.justify_center().w(self.size.height());
            }
            ButtonShape::Circle => {
                el = el.justify_center().w(self.size.height());
            }
        }

        // radius — `rounded-full` for the circle, else the size's radius.
        el = if matches!(self.shape, ButtonShape::Circle) {
            el.rounded_full()
        } else {
            el.rounded(self.size.radius())
        };

        // The emphasis fill Kumo sets through `--kumo-button-emphasis-bg`.
        let emphasis = match self.variant {
            ButtonVariant::Destructive => t.danger,
            _ => t.brand,
        };
        let emphasis_hover = match self.variant {
            ButtonVariant::Destructive => KumoTokens::alpha(t.danger, 0.85),
            _ => t.brand_hover,
        };

        let interactive = !self.disabled;

        match self.variant {
            ButtonVariant::Primary | ButtonVariant::Destructive => {
                // `!text-white` — Kumo forces white on both emphasis fills.
                el = el
                    .bg(if self.disabled {
                        KumoTokens::alpha(emphasis, 0.5)
                    } else {
                        emphasis
                    })
                    .text_color(rgb(0xFFFFFF))
                    // Kumo's emphasis ring is the emphasis color itself. A
                    // translucent ring over the same fill would composite to a
                    // visible rim around the button, so it stays solid.
                    .border_color(if self.disabled {
                        KumoTokens::alpha(emphasis, 0.5)
                    } else {
                        emphasis
                    })
                    .shadow(shadow_xs());
                if interactive {
                    el = el.hover(move |s| s.bg(emphasis_hover));
                }
            }
            ButtonVariant::Secondary => {
                let tint = t.tint;
                el = el
                    .bg(t.base)
                    .text_color(if self.disabled {
                        KumoTokens::alpha(t.text_default, 0.7)
                    } else {
                        t.text_default
                    })
                    .border_color(t.line)
                    .shadow(shadow_xs());
                if self.active {
                    el = el.bg(tint);
                } else if interactive {
                    el = el.hover(move |s| s.bg(tint));
                }
            }
            ButtonVariant::Ghost => {
                // "shadow-none bg-inherit" — no fill until hover.
                let tint = t.tint;
                el = el.text_color(if self.disabled {
                    t.text_subtle
                } else {
                    t.text_default
                });
                if self.active {
                    el = el.bg(tint);
                } else if interactive {
                    el = el.hover(move |s| s.bg(tint));
                }
            }
            ButtonVariant::SecondaryDestructive => {
                let danger = t.danger;
                el = el
                    .bg(t.base)
                    .text_color(if self.disabled {
                        KumoTokens::alpha(danger, 0.7)
                    } else {
                        danger
                    })
                    .border_color(t.line)
                    .shadow(shadow_xs());
                if interactive {
                    // `hover:ring-kumo-danger/30`
                    el = el.hover(move |s| s.border_color(KumoTokens::alpha(danger, 0.3)));
                }
            }
            ButtonVariant::Outline => {
                let strong = t.text_strong;
                let focus_25 = KumoTokens::alpha(t.focus, 0.25);
                el = el
                    .text_color(if self.disabled {
                        t.text_subtle
                    } else {
                        t.text_default
                    })
                    .border_color(t.line);
                if interactive {
                    el = el.hover(move |s| s.text_color(strong).border_color(focus_25));
                }
            }
            ButtonVariant::Tinted => {
                let color = self.ink.unwrap_or(t.brand);
                let ring = KumoTokens::alpha(color, if self.disabled { 0.18 } else { 0.35 });
                let ring_hover = KumoTokens::alpha(color, 0.5);
                let wash = KumoTokens::alpha(color, 0.12);
                el = el
                    .text_color(if self.disabled {
                        KumoTokens::alpha(color, 0.5)
                    } else {
                        color
                    })
                    .border_color(ring);
                if interactive {
                    el = el.hover(move |s| s.bg(wash).border_color(ring_hover));
                }
            }
        }

        // The icon inherits the label color, so resolve it the same way.
        let ink = match self.variant {
            ButtonVariant::Primary | ButtonVariant::Destructive => rgb(0xFFFFFF),
            ButtonVariant::SecondaryDestructive if !self.disabled => t.danger,
            ButtonVariant::SecondaryDestructive => KumoTokens::alpha(t.danger, 0.7),
            ButtonVariant::Tinted if self.disabled => {
                KumoTokens::alpha(self.ink.unwrap_or(t.brand), 0.5)
            }
            _ if self.disabled => t.text_subtle,
            _ => t.text_default,
        };
        let ink = match self.variant {
            ButtonVariant::Tinted => ink,
            _ => self.ink.unwrap_or(ink),
        };
        if self.ink.is_some() && self.variant != ButtonVariant::Tinted {
            el = el.text_color(ink);
        }

        if let Some(name) = &self.icon_name {
            el = el.child(icon(name, self.size.icon(), ink));
        }
        if let Some(label) = self.label {
            if !compact {
                el = el.child(label);
            }
        }
        if let Some(name) = &self.icon_right {
            el = el.child(icon(name, self.size.icon(), ink));
        }

        if interactive {
            el = el.cursor_pointer();
        } else {
            el = el.cursor_default();
        }
        el
    }
}

/// A bare icon button at `size="sm"`, the density the Jade action bar runs at.
/// Kumo spells this `<Button shape="square" size="sm" variant="ghost" />`.
pub fn icon_button(
    id: impl Into<ElementId>,
    name: impl Into<SharedString>,
    active: bool,
    t: &KumoTokens,
) -> Stateful<Div> {
    // An active toggle takes the brand ink. Kumo leaves this to
    // `aria-selected`; GPUI has no selector engine, so the caller's `active`
    // flag drives it here.
    let mut b = Button::icon_only(id, name)
        .size(Size::Sm)
        .variant(ButtonVariant::Ghost)
        .active(active);
    if active {
        b = b.ink(t.brand);
    } else {
        b = b.ink(t.text_subtle);
    }
    b.render(t)
}

/// The 8px square status dot Kumo puts in `Badge appearance="dot"` and that the
/// Jade status strip reuses.
pub fn dot(color: Rgba) -> Div {
    div().size(px(6.)).rounded_full().bg(color)
}
