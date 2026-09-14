//! ASM viewer overlay (feature inventory §6 / app.ts:304-445).
//!
//! A right-half overlay over the editor showing the `-O3 -march=native`
//! assembly of the active file (monospace, read-only, virtualized). The source
//! caret line highlights every mapped asm line; clicking an asm line highlights
//! + scrolls to its source counterpart (asm→src also scrolls). The map + the
//! selection live in [`crate::asm::AsmView`]; this module is a thin projection.

use gpui::{
    div, prelude::*, px, rgb, rgba, uniform_list, Context, MouseButton, MouseDownEvent, Rgba,
};

use crate::app::JadeApp;

/// Asm row height (tighter than the editor's 20px so more instructions fit).
const ASM_LINE_H: f32 = 18.0;

fn rgba_a(rgb: u32, alpha: f32) -> Rgba {
    let a = (alpha.clamp(0.0, 1.0) * 255.0).round() as u32;
    rgba((rgb << 8) | (a & 0xff))
}

/// The right-half ASM overlay, absolutely positioned over the editor area. Only
/// built when `app.asm_visible`.
pub fn overlay(app: &JadeApp, cx: &mut Context<JadeApp>) -> gpui::AnyElement {
    let theme = app.theme.clone();

    let file = app
        .active_file
        .as_ref()
        .and_then(|p| p.file_name())
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();

    let header = div()
        .flex()
        .flex_row()
        .items_center()
        .justify_between()
        .h(px(28.))
        .pl(px(10.))
        .pr(px(4.))
        .bg(rgb(theme.panel))
        .border_b_1()
        .border_color(theme.kumo.hairline)
        .child(
            div()
                .flex()
                .flex_row()
                .items_center()
                .gap_2()
                .child(crate::assets::ui_icon("code", 13., theme.muted))
                .child(
                    div()
                        .text_color(rgb(theme.muted))
                        .text_xs()
                        .font_weight(gpui::FontWeight::SEMIBOLD)
                        .child("ASM"),
                )
                .child(div().text_color(rgb(theme.muted)).text_xs().child(file)),
        )
        .child(
            crate::kumo::button::icon_button("asm-close", "x", false, &theme.kumo).on_click(
                cx.listener(|a: &mut JadeApp, _e, _w, cx| {
                    a.toggle_asm(cx);
                    cx.notify();
                }),
            ),
        );

    let body = match &app.asm {
        Some(view) if view.line_count() > 0 => asm_list(view.line_count(), app, cx),
        _ => div()
            .flex()
            .flex_1()
            .items_center()
            .justify_center()
            .child(
                div()
                    .text_color(rgb(theme.muted))
                    .text_xs()
                    .child(if app.asm_loading {
                        "generating asm…"
                    } else {
                        "no assembly (build target / active file needed)"
                    }),
            )
            .into_any_element(),
    };

    div()
        .id("asm-overlay")
        .absolute()
        .top_0()
        .right_0()
        .bottom_0()
        .w(gpui::relative(0.5))
        .flex()
        .flex_col()
        .bg(rgb(theme.bg))
        .border_l_1()
        .border_color(rgb(theme.border))
        .child(header)
        .child(body)
        .into_any_element()
}

fn asm_list(n: usize, app: &JadeApp, cx: &mut Context<JadeApp>) -> gpui::AnyElement {
    let list = uniform_list(
        "asm-lines",
        n,
        cx.processor(move |this, range: std::ops::Range<usize>, _window, cx| {
            let theme = this.theme.clone();
            let mut rows = Vec::with_capacity(range.len());
            let Some(view) = &this.asm else {
                return rows;
            };
            for i in range {
                let text = view.lines.get(i).cloned().unwrap_or_default();
                let text = if text.is_empty() { " ".to_string() } else { text };
                let hot = view.asm_row_highlighted(i);
                let mut row = div()
                    .id(("asm-row", i))
                    .flex()
                    .flex_row()
                    .items_center()
                    .h(px(ASM_LINE_H))
                    .px(px(6.))
                    .whitespace_nowrap()
                    .overflow_hidden()
                    .cursor_pointer()
                    .text_color(rgb(theme.text))
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(move |a: &mut JadeApp, _ev: &MouseDownEvent, _w, cx| {
                            a.asm_click(i);
                            cx.notify();
                        }),
                    )
                    .child(text);
                if hot {
                    // Mapped-to-caret line gets an accent wash + left border.
                    row = row
                        .bg(rgba_a(theme.accent, 0.16))
                        .border_l_2()
                        .border_color(rgb(theme.accent));
                }
                rows.push(row);
            }
            rows
        }),
    )
    .track_scroll(&app.asm_scroll)
    .flex_1()
    .size_full()
    .py(px(4.))
    .text_size(px(11.))
    .line_height(px(ASM_LINE_H))
    .font_family(crate::fonts::mono_family());

    div().flex().flex_1().size_full().child(list).into_any_element()
}
