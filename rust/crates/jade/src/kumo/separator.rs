//! Separator — Kumo has no separator component; it writes the rule inline as
//! `border-kumo-hairline` on the neighbouring element, and the table's column
//! divider as `"h-5 w-[2px] rounded bg-kumo-hairline"`. Both forms are here so
//! callers stop hand-rolling a 1px div.

use gpui::{div, prelude::*, px, Div};

use super::tokens::KumoTokens;

/// A full-width horizontal rule on the hairline token.
pub fn separator_h(t: &KumoTokens) -> Div {
    div().h(px(1.)).w_full().bg(t.hairline)
}

/// A vertical rule between toolbar clusters. Kumo's table draws this one at
/// `h-5 w-[2px] rounded`.
pub fn separator_v(t: &KumoTokens, height: f32) -> Div {
    div().w(px(1.)).h(px(height)).bg(t.hairline)
}
