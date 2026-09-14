//! Tabs — a port of `packages/kumo/src/components/tabs/tabs.tsx`.
//!
//! Kumo ships two appearances. `segmented` puts the list in a recessed trough
//! (`"rounded-lg bg-kumo-recessed px-0.5"`) and slides a raised pill
//! (`"bg-kumo-base shadow-sm ring ring-kumo-line"`) under the active trigger.
//! `underline` leaves the list flat and draws a 2px brand bar along the bottom
//! of the active trigger (`"bottom-0 h-0.5 bg-kumo-brand"`).
//!
//! Triggers are `text-kumo-subtle` and go `text-kumo-default` when selected or
//! hovered, per `"my-0.5 text-kumo-subtle hover:text-kumo-default
//! aria-selected:text-kumo-default"`.

use gpui::{div, prelude::*, px, AnyElement, Div, ElementId, SharedString, Stateful};

use super::tokens::KumoTokens;
use super::{icon, scale, shadow_sm, Size};

/// `TabsProps["appearance"]`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TabsAppearance {
    Segmented,
    Underline,
}

/// One trigger. `render` hands back the element so the caller can attach the
/// click listener.
pub struct TabItem {
    id: ElementId,
    label: SharedString,
    icon_name: Option<SharedString>,
    selected: bool,
    /// Kumo puts a Badge inside a trigger for counts; Jade shows the diagnostic
    /// counts the same way.
    trailing: Option<AnyElement>,
}

impl TabItem {
    pub fn new(id: impl Into<ElementId>, label: impl Into<SharedString>, selected: bool) -> Self {
        Self {
            id: id.into(),
            label: label.into(),
            icon_name: None,
            selected,
            trailing: None,
        }
    }

    pub fn icon(mut self, name: impl Into<SharedString>) -> Self {
        self.icon_name = Some(name.into());
        self
    }

    pub fn trailing(mut self, el: AnyElement) -> Self {
        self.trailing = Some(el);
        self
    }

    fn render(self, appearance: TabsAppearance, size: Size, t: &KumoTokens) -> Stateful<Div> {
        let is_sm = matches!(size, Size::Xs | Size::Sm);
        let ink = if self.selected {
            t.text_default
        } else {
            t.text_subtle
        };
        let hover_ink = t.text_default;

        let mut el = div()
            .id(self.id)
            .relative()
            .flex()
            .flex_row()
            .items_center()
            .gap(scale::SPACE_1_5)
            .cursor_pointer()
            .text_color(ink)
            .text_size(if is_sm {
                scale::TEXT_XS
            } else {
                scale::TEXT_BASE
            });

        el = match appearance {
            TabsAppearance::Segmented => {
                // `isSegmented && (isSm ? "rounded-sm px-2" : "rounded-md px-2.5")`,
                // at the `my-0.5` height that leaves 2px of trough on every
                // edge of the pill.
                let e = el
                    .h(size.tab_trigger_height())
                    .px(if is_sm {
                        scale::SPACE_2
                    } else {
                        scale::SPACE_2_5
                    });
                if self.selected {
                    // The sliding indicator: `bg-kumo-base shadow-sm ring
                    // ring-kumo-line`. GPUI has no shared layout animation, so
                    // the pill paints on the trigger itself.
                    e.bg(t.base)
                        .border_1()
                        .border_color(t.line)
                        .shadow(shadow_sm())
                        .font_weight(gpui::FontWeight::MEDIUM)
                } else {
                    e
                }
            }
            TabsAppearance::Underline => {
                // `isUnderline && (isSm ? "px-1.5 py-2.5" : "px-2 py-3")`
                let e = el
                    .px(if is_sm {
                        scale::SPACE_1_5
                    } else {
                        scale::SPACE_2
                    })
                    .py(if is_sm {
                        scale::SPACE_2_5
                    } else {
                        scale::SPACE_3
                    })
                    ;
                if self.selected {
                    e.font_weight(gpui::FontWeight::MEDIUM).child(
                        // `absolute bottom-0 h-0.5 bg-kumo-brand`
                        div()
                            .absolute()
                            .bottom_0()
                            .left_0()
                            .right_0()
                            .h(px(2.))
                            .bg(t.brand),
                    )
                } else {
                    let tint = t.tint;
                    e.hover(move |s| s.bg(tint))
                }
            }
        };

        if !self.selected {
            el = el.hover(move |s| s.text_color(hover_ink));
        }
        if let Some(name) = &self.icon_name {
            el = el.child(icon(name, size.icon(), ink));
        }
        el = el.child(self.label);
        if let Some(tr) = self.trailing {
            el = el.child(tr);
        }
        el
    }
}

/// The tab list. Build it, push the triggers through [`TabBar::item`], then
/// `render`.
pub struct TabBar {
    appearance: TabsAppearance,
    size: Size,
    items: Vec<Stateful<Div>>,
}

impl TabBar {
    pub fn new(appearance: TabsAppearance) -> Self {
        Self {
            appearance,
            size: Size::Base,
            items: Vec::new(),
        }
    }

    pub fn size(mut self, size: Size) -> Self {
        self.size = size;
        self
    }

    /// Adds a trigger. The returned element is the caller's to wire, so this
    /// takes the already-rendered trigger from [`TabBar::trigger`].
    pub fn push(mut self, trigger: Stateful<Div>) -> Self {
        self.items.push(trigger);
        self
    }

    /// Renders one trigger against this bar's appearance and size.
    pub fn trigger(&self, item: TabItem, t: &KumoTokens) -> Stateful<Div> {
        item.render(self.appearance, self.size, t)
    }

    pub fn render(self, t: &KumoTokens) -> Div {
        let mut el = div().flex().flex_row();

        el = match self.appearance {
            // `isSegmented && "rounded-lg bg-kumo-recessed px-0.5"`.
            //
            // Kumo also rings the trough (`ring ring-kumo-hairline/70`), but a
            // Tailwind ring is a box-shadow and costs no layout. A GPUI border
            // costs 1px per edge, which would eat the trigger's `my-0.5` margin
            // and leave the active pill flush against the trough — its corners
            // then get shaved by the trough's own radius under `overflow`. The
            // recessed fill already separates the trough from the panel, so the
            // port drops the ring rather than the breathing room.
            TabsAppearance::Segmented => el
                .items_center()
                .gap(px(2.))
                .h(self.size.tab_bar_height())
                .px(px(2.))
                .bg(t.recessed),
            // `isUnderline` — the bar itself is flat; only the hairline under
            // the whole list separates it from the content. This one scrolls,
            // so it keeps the clip.
            TabsAppearance::Underline => el
                .items_stretch()
                .overflow_hidden()
                .gap(scale::SPACE_1)
                .border_b_1()
                .border_color(t.hairline),
        };

        for item in self.items {
            el = el.child(item);
        }
        el
    }
}
