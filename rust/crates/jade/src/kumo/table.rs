//! Table — a port of `packages/kumo/src/components/table/table.tsx`.
//!
//! Header cells are `"[&_th]:border-b [&_th]:border-kumo-fill [&_th]:p-3
//! [&_th]:text-base [&_th]:font-semibold"` on a `bg-kumo-base` row; the compact
//! header drops to `"text-xs text-kumo-strong [&_th]:bg-kumo-elevated
//! [&_th]:py-2"`. Body rows zebra-stripe through `"even:bg-kumo-tint"`, and the
//! selected row pins to `"bg-kumo-tint"`.

use gpui::{div, prelude::*, Div, SharedString};

use super::scale;
use super::tokens::KumoTokens;

/// The table shell: `"text-left text-base text-kumo-default"` over
/// `"m-0 bg-kumo-base p-0"`.
pub struct Table {
    compact: bool,
}

impl Table {
    pub fn new() -> Self {
        Self { compact: false }
    }

    /// `data-compact` — the dense header the Jade panels use.
    pub fn compact(mut self, on: bool) -> Self {
        self.compact = on;
        self
    }

    pub fn render(self, t: &KumoTokens) -> Div {
        div()
            .flex()
            .flex_col()
            .w_full()
            .bg(t.base)
            .text_size(if self.compact {
                scale::TEXT_XS
            } else {
                scale::TEXT_BASE
            })
            .text_color(t.text_default)
    }
}

impl Default for Table {
    fn default() -> Self {
        Self::new()
    }
}

/// The header row.
pub struct TableHeader {
    compact: bool,
}

impl TableHeader {
    pub fn new(compact: bool) -> Self {
        Self { compact }
    }

    pub fn render(self, t: &KumoTokens) -> Div {
        div()
            .flex()
            .flex_row()
            .items_center()
            .w_full()
            .bg(if self.compact { t.elevated } else { t.base })
            .border_b_1()
            .border_color(t.fill)
            .px(scale::SPACE_3)
            .py(if self.compact {
                scale::SPACE_2
            } else {
                scale::SPACE_3
            })
            .text_size(if self.compact {
                scale::TEXT_XS
            } else {
                scale::TEXT_BASE
            })
            .font_weight(gpui::FontWeight::SEMIBOLD)
            .text_color(if self.compact {
                t.text_strong
            } else {
                t.text_default
            })
    }
}

/// One body row. `zebra` is the row index — Kumo stripes the even ones.
pub struct TableRow {
    index: usize,
    selected: bool,
    compact: bool,
}

impl TableRow {
    pub fn new(index: usize) -> Self {
        Self {
            index,
            selected: false,
            compact: false,
        }
    }

    pub fn selected(mut self, on: bool) -> Self {
        self.selected = on;
        self
    }

    pub fn compact(mut self, on: bool) -> Self {
        self.compact = on;
        self
    }

    pub fn render(self, t: &KumoTokens) -> Div {
        let mut el = div()
            .flex()
            .flex_row()
            .items_center()
            .w_full()
            .px(scale::SPACE_3)
            .py(if self.compact {
                scale::SPACE_1
            } else {
                scale::SPACE_2
            });
        // `even:bg-kumo-tint`, overridden by the selected row.
        if self.selected || self.index % 2 == 1 {
            el = el.bg(t.tint);
        }
        el
    }
}

/// One cell. `flex` is the column's share of the row width — Kumo lets the HTML
/// table lay this out, so the port asks the caller for the weight.
pub struct TableCell {
    text: Option<SharedString>,
    flex: f32,
    numeric: bool,
    subtle: bool,
}

impl TableCell {
    pub fn new(flex: f32) -> Self {
        Self {
            text: None,
            flex,
            numeric: false,
            subtle: false,
        }
    }

    pub fn text(mut self, text: impl Into<SharedString>) -> Self {
        self.text = Some(text.into());
        self
    }

    /// Right-aligned and monospaced — Kumo's `tabular-nums`, so a column of
    /// figures lines up on the decimal point and does not shift as it updates.
    pub fn numeric(mut self, on: bool) -> Self {
        self.numeric = on;
        self
    }

    pub fn subtle(mut self, on: bool) -> Self {
        self.subtle = on;
        self
    }

    pub fn render(self, t: &KumoTokens) -> Div {
        let mut el = div()
            .flex()
            .flex_row()
            .items_center()
            .flex_grow(1.)
            .flex_shrink(1.)
            .flex_basis(gpui::relative(self.flex))
            .overflow_hidden()
            .text_color(if self.subtle {
                t.text_subtle
            } else {
                t.text_default
            });
        if self.numeric {
            el = el.justify_end().font_family(crate::fonts::mono_family());
        }
        if let Some(text) = self.text {
            el = el.child(text);
        }
        el
    }
}
