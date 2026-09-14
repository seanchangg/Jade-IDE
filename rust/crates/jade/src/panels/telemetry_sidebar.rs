//! TELEMETRY sidebar (feature inventory §5.6): SCALARS / TIMERS / BUFFERS
//! sections of checkbox rows with live value readouts. Toggling a checkbox
//! flips registry state, persists the preference, and sends `track` to the
//! probe (all via `JadeApp::toggle_enabled`).
//!
//! The inline rows×cols shape editor from the TS panel (`telemetry-panel.ts`'s
//! `toggleShapeEditor`/`apply`, lines 496-559) is wired here: clicking a
//! buffer row's "⊞" glyph opens an editor row directly below it (mirroring the
//! TS `insertAdjacentElement('afterend', editor)`), sharing the captured-
//! keystroke `JadeApp::dim_edit` session with the wg3d toolbar's rows×cols
//! fields (`wg3d/render.rs::dim_fields`) — only one editor is open at a time.

use gpui::{div, prelude::*, px, rgb, Context, KeyDownEvent, SharedString};

use jade_telemetry::Kind;

use crate::app::dim_input::{display_value, DimField};
use crate::app::JadeApp;
use crate::format::{format_buffer_value, format_scalar_value, format_timer_value};
use crate::registry::{key_of, RegistryItem};
use crate::theme::Theme;

pub fn render(app: &JadeApp, cx: &mut Context<JadeApp>) -> impl IntoElement {
    let theme = &app.theme;

    let mut col = div()
        .flex()
        .flex_col()
        .gap_2()
        .child(
            div()
                .flex()
                .flex_row()
                .items_center()
                .gap_1()
                .text_color(rgb(theme.muted))
                .text_xs()
                .font_weight(gpui::FontWeight::SEMIBOLD)
                .child(crate::assets::ui_icon("activity", 13., theme.muted))
                .child("TELEMETRY"),
        );

    for (kind, title) in [
        (Kind::Scalar, "SCALARS"),
        (Kind::Timer, "TIMERS"),
        (Kind::Buffer, "BUFFERS"),
    ] {
        col = col.child(section(app, cx, kind, title));
    }

    col
}

fn section(
    app: &JadeApp,
    cx: &mut Context<JadeApp>,
    kind: Kind,
    title: &str,
) -> impl IntoElement {
    let theme = &app.theme;
    // Buffers group by their symbolicated scope ("blocks.attn.*" together);
    // scalars/timers keep probe discovery order (auto-check-first-3 etc.).
    let items = if kind == Kind::Buffer {
        app.registry.items_of_kind_grouped(kind)
    } else {
        app.registry.items_of_kind(kind)
    };

    let mut body = div().flex().flex_col().gap_1();
    if items.is_empty() {
        body = body.child(
            div()
                .text_color(rgb(theme.muted))
                .text_size(px(10.))
                .child("none discovered"),
        );
    } else {
        for item in items {
            body = body.child(row(app, cx, theme, kind, item));
            // The inline rows×cols editor (§5.6 `toggleShapeEditor`) sits
            // directly under the buffer row it edits, opened by that row's
            // "⊞" glyph.
            if kind == Kind::Buffer {
                if let Some(state) = app.dim_edit() {
                    if state.name == item.name {
                        body = body.child(dim_editor_row(app, cx, theme, &item.name));
                    }
                }
            }
        }
    }

    div()
        .flex()
        .flex_col()
        .gap_1()
        .child(
            div()
                .text_color(rgb(theme.muted))
                .text_size(px(10.))
                .font_weight(gpui::FontWeight::MEDIUM)
                .child(title.to_string()),
        )
        .child(body)
}

fn row(
    app: &JadeApp,
    cx: &mut Context<JadeApp>,
    theme: &Theme,
    kind: Kind,
    item: &RegistryItem,
) -> impl IntoElement {
    let name = item.name.clone();
    let enabled = item.enabled;
    let key = key_of(kind, &item.name);

    // Checkbox: a small box, filled when enabled.
    let mut checkbox = div()
        .w(px(12.))
        .h(px(12.))
        
        .border_1()
        .border_color(rgb(theme.accent));
    if enabled {
        checkbox = checkbox.bg(rgb(theme.accent));
    }

    let value = value_readout(kind, item);

    let mut row = div()
        .id(SharedString::from(format!("telrow-{}", key)))
        .flex()
        .items_center()
        .gap_2()
        .text_size(px(11.))
        .cursor_pointer()
        .on_click(cx.listener(move |app: &mut JadeApp, _ev, _win, cx| {
            app.toggle_enabled(kind, &name);
            cx.notify();
        }))
        .child(checkbox)
        .child(
            div()
                .flex_1()
                .min_w(px(0.))
                .overflow_hidden()
                .whitespace_nowrap()
                .text_ellipsis()
                // Seeded = remembered from a previous session, not yet
                // declared by the probe this session — dimmed until confirmed.
                .text_color(rgb(if item.seeded { theme.muted } else { theme.text }))
                .child(item.name.clone()),
        )
        .child(
            div()
                .text_color(rgb(theme.muted))
                .child(value),
        );

    // Buffers: shape editor toggle (set the tensor's true rows × cols),
    // matching `telemetry-panel.ts`'s `shapeBtn` — click opens/closes the
    // inline editor without also toggling this row's enabled checkbox.
    if kind == Kind::Buffer {
        let n = item.name.clone();
        let is_editing = app.dim_edit().map(|s| s.name == item.name).unwrap_or(false);
        row = row.child(
            div()
                .id(SharedString::from(format!("telshape-{}", key)))
                .text_color(rgb(if is_editing { theme.accent } else { theme.muted }))
                .text_size(px(10.))
                .cursor_pointer()
                .on_click(cx.listener(move |app: &mut JadeApp, _ev, _win, cx| {
                    cx.stop_propagation(); // don't also toggle the row's checkbox
                    app.start_dim_edit(&n, DimField::Rows);
                    cx.notify();
                }))
                .child("⊞"),
        );
    }

    row
}

fn value_readout(kind: Kind, item: &RegistryItem) -> String {
    match kind {
        Kind::Buffer => {
            let r = item.last_rows.or(item.meta_rows);
            let c = item.last_cols.or(item.meta_cols);
            format_buffer_value(r, c, item.last_step)
        }
        Kind::Timer => item
            .last_value
            .map(format_timer_value)
            .unwrap_or_else(|| "—".to_string()),
        Kind::Scalar => item
            .last_value
            .map(format_scalar_value)
            .unwrap_or_else(|| "—".to_string()),
    }
}

/// The inline rows×cols editor row (`telemetry-panel.ts`'s `toggleShapeEditor`
/// editor: two fields + a "×" separator + a "set" button). Enter/Escape are
/// handled the same way as the wg3d toolbar's fields (`JadeApp::dim_edit_key`);
/// "set" mirrors the TS `applyBtn`, which the panel wires to the same
/// apply/commit Enter uses.
fn dim_editor_row(
    app: &JadeApp,
    cx: &mut Context<JadeApp>,
    theme: &Theme,
    name: &str,
) -> impl IntoElement {
    // `section` only calls this when `app.dim_edit` names this buffer.
    let state = app.dim_edit().expect("dim_edit open for this row");
    let (ph_rows, ph_cols) = app.dim_edit_placeholder(name);
    let (rows_text, rows_ph) = display_value(&state.rows, &ph_rows);
    let (cols_text, cols_ph) = display_value(&state.cols, &ph_cols);
    let rows_active = state.field == DimField::Rows;
    let cols_active = state.field == DimField::Cols;

    let mut wrap = div()
        .id(SharedString::from(format!("telshape-editor-{name}")))
        .flex()
        .flex_row()
        .items_center()
        .gap_1()
        .pl(px(18.)) // indent under the row's checkbox
        .text_size(px(10.));

    if let Some(focus) = app.dim_edit_focus_handle() {
        wrap = wrap.track_focus(&focus).on_key_down(cx.listener(
            |a: &mut JadeApp, ev: &KeyDownEvent, _w, cx| {
                if a.dim_edit_key(&ev.keystroke) {
                    cx.stop_propagation();
                    cx.notify();
                }
            },
        ));
    }

    wrap.child(dim_field(name, DimField::Rows, rows_text, rows_ph, rows_active, theme, cx))
        .child(div().text_color(rgb(theme.muted)).child("×"))
        .child(dim_field(name, DimField::Cols, cols_text, cols_ph, cols_active, theme, cx))
        .child(
            div()
                .id(SharedString::from(format!("telshape-set-{name}")))
                .px_1()
                .text_color(rgb(theme.accent))
                .cursor_pointer()
                .on_click(cx.listener(|app: &mut JadeApp, _e, _w, cx| {
                    app.commit_dim_edit_if_valid();
                    cx.notify();
                }))
                .child("set"),
        )
}

/// One rows/cols field, shared presentation with the wg3d toolbar's
/// `dim_field_chip` (duplicated rather than shared across modules — it's a
/// handful of styling lines and the two toolbars live in unrelated files).
fn dim_field(
    name: &str,
    field: DimField,
    text: String,
    is_placeholder: bool,
    active: bool,
    theme: &Theme,
    cx: &mut Context<JadeApp>,
) -> gpui::AnyElement {
    let n = name.to_string();
    let label = if active { format!("{text}▏") } else { text };
    let mut el = div()
        .id(SharedString::from(format!(
            "telshape-field-{name}-{}",
            if field == DimField::Rows { "r" } else { "c" }
        )))
        .px_1()
        .min_w(px(14.))
        
        .cursor_pointer()
        .text_color(rgb(if is_placeholder { theme.muted } else { theme.text }))
        .on_click(cx.listener(move |app: &mut JadeApp, _e, _w, cx| {
            if app.dim_edit().map(|s| s.name == n).unwrap_or(false) {
                app.set_dim_edit_field(field);
            } else {
                app.start_dim_edit(&n, field);
            }
            cx.notify();
        }))
        .child(label);
    if active {
        el = el.border_b_1().border_color(rgb(theme.accent));
    }
    el.into_any_element()
}
