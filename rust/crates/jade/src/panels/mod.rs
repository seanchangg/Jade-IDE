//! Panel renderers: the left file-tree + center code viewer (code-viewing
//! vertical), plus the right-runtime-sidebar training/telemetry views.

use gpui::{div, prelude::*, px, rgb, Div};

pub mod asm_view;
pub mod code_view;
pub mod csv_view;
pub mod debug_panel;
pub mod explain_card;
pub mod explain_popout;
pub mod file_tree;
pub mod md_view;
pub mod metric_popout;
pub mod pre_run;
pub mod quick_open;
pub mod runtime_panel;
pub mod schematic_view;
pub mod structure_panel;
pub mod telemetry_sidebar;
pub mod terminal_panel;
pub mod training_view;
pub mod visualize_card;
pub mod visualize_popout;
pub mod wave_view;

/// A captured-keystroke text field's visual line, shared by every fake input
/// (find bar, quick open, pre-run filter/bundle name). The caret is a thin
/// bar that sits BEFORE the placeholder when the value is empty — a real
/// input's caret is at offset 0, not after the ghost text — and at `cursor`
/// (a byte offset into `value`; pass `value.len()` for append-only fields)
/// while typing.
///
/// `focused` reserves the caret's slot; `lit` fills it (blink phase — pass
/// `true` for fields without a blink timer). The invisible box on the off
/// phase keeps the text from shifting sideways each blink.
pub fn input_line(
    value: &str,
    placeholder: &str,
    focused: bool,
    lit: bool,
    caret_color: u32,
    theme: &crate::theme::Theme,
    cursor: usize,
) -> Div {
    let caret = || {
        let mut c = div().w(px(1.5)).h(px(14.)).flex_none();
        if lit {
            c = c.bg(rgb(caret_color));
        }
        c
    };
    let text_run = |s: &str| {
        div()
            .whitespace_nowrap()
            .flex_none()
            .text_color(theme.kumo.text_default)
            .child(s.to_string())
    };

    let mut row = div().flex().flex_row().items_center().gap(px(1.));
    if value.is_empty() {
        if focused {
            row = row.child(caret());
        }
        // Kumo's `kumo-input-placeholder` uses its own token, one step fainter
        // than the secondary text color.
        row = row.child(
            div()
                .text_color(theme.kumo.text_placeholder)
                .child(placeholder.to_string()),
        );
    } else if focused {
        // Split around the caret so it renders mid-text; either side can be
        // empty (caret at the front / end) — empty runs are skipped so the 1px
        // child gap doesn't pad the edges.
        let at = cursor.min(value.len());
        let (before, after) = value.split_at(at);
        if !before.is_empty() {
            row = row.child(text_run(before));
        }
        row = row.child(caret());
        if !after.is_empty() {
            row = row.child(text_run(after));
        }
    } else {
        row = row.child(text_run(value));
    }
    row
}
