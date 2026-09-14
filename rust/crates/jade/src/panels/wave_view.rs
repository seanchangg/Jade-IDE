//! The wave panel: the right drawer in hardware mode.
//!
//! A waveform viewer in the Surfer idiom: a names column, a value-at-cursor
//! column, and one canvas that paints every visible signal. A toolbar runs
//! the testbench, opens the signal picker, zooms, and hands the dump to the
//! external Surfer for a full session.
//!
//! The canvas paints with `paint_quad` (one-pixel quads for the traces) and
//! shaped text lines for bus values, both inside one `canvas`. The canvas
//! records its painted origin (the schematic's technique) so the mouse
//! listeners can map window space → canvas space.
//!
//! Pointer: click sets the cursor, drag pans, the wheel zooms (vertical)
//! and pans (horizontal). Keys while the panel has focus: `+`/`-` zoom, `f`
//! fits, `←`/`→` step the cursor to the previous / next change.

use std::cell::Cell;
use std::rc::Rc;
use std::sync::Arc;

use gpui::{
    canvas, div, point, prelude::*, px, size, AnyElement, Bounds, ContentMask, Context, Hsla,
    KeyDownEvent, MouseButton, MouseDownEvent, MouseMoveEvent, MouseUpEvent, Pixels, Rgba,
    ScrollDelta, ScrollWheelEvent, SharedString, TextAlign, TextRun,
};

use jade_hw::wave::{WaveFile, WaveValue};

use crate::app::{AppMode, JadeApp};
use crate::kumo::{self, scale, Badge, BadgeVariant, Button, ButtonVariant, Card, Empty, Size};
use crate::theme::Theme;
use crate::wave::{NAME_W, VAL_W};

/// One signal row's height.
const ROW_H: f32 = 22.0;
/// The time ruler's height.
const RULER_H: f32 = 20.0;
/// The trace's vertical inset inside its row.
const INSET: f32 = 4.0;
const FONT_PX: f32 = 11.0;
/// The narrowest bus segment that gets a value label.
const LABEL_PAD: f32 = 6.0;

fn alpha(mut c: Rgba, a: f32) -> Rgba {
    c.a = a;
    c
}

/// The whole drawer. Rendered only in hardware mode.
pub fn panel(app: &JadeApp, cx: &mut Context<JadeApp>, theme: &Theme) -> AnyElement {
    if app.mode != AppMode::Hardware || !app.wave_visible {
        return div().into_any_element();
    }
    let Some(focus) = app.hw_focus.clone() else {
        return div().into_any_element();
    };
    let Some(hw) = app.hw.as_ref() else {
        return div().into_any_element();
    };
    let w = app.wave_width;
    let focus_for_click = focus.clone();

    let body: AnyElement = if hw.wave.picker && hw.wave.file.is_some() {
        picker(app, cx, theme).into_any_element()
    } else if let Some(file) = hw.wave.file.clone() {
        sheet(app, file, cx, theme).into_any_element()
    } else {
        empty(app, theme).into_any_element()
    };

    let card = Card::new(&theme.kumo)
        .id("wave-panel")
        .flex()
        .flex_none()
        .flex_col()
        .gap(scale::SPACE_2)
        .w(px(w))
        .h_full()
        .min_h(px(0.))
        .p(scale::SPACE_3)
        .bg(theme.kumo.elevated)
        .track_focus(&focus)
        .on_mouse_down(
            MouseButton::Left,
            cx.listener(move |_a: &mut JadeApp, _e: &MouseDownEvent, window, cx| {
                focus_for_click.focus(window, cx);
                cx.notify();
            }),
        )
        // Panel-scoped keys, only while the panel has focus, so the editor
        // keeps every printable.
        .on_key_down(cx.listener(|a: &mut JadeApp, ev: &KeyDownEvent, _w, cx| {
            let ks = &ev.keystroke;
            if ks.modifiers.platform || ks.modifiers.control {
                return;
            }
            match ks.key.as_str() {
                "=" | "+" => a.wave_zoom_center(1.5),
                "-" | "_" => a.wave_zoom_center(1.0 / 1.5),
                "f" => a.wave_fit(),
                "left" => a.wave_step_cursor(-1),
                "right" => a.wave_step_cursor(1),
                _ => return,
            }
            cx.stop_propagation();
            cx.notify();
        }))
        .child(toolbar(app, cx, theme))
        .child(status_row(app, theme))
        .child(body);

    div()
        .debug_selector(|| "wave-panel".into())
        .flex()
        .flex_row()
        .flex_none()
        .h_full()
        .child(resize_handle(cx, theme))
        .child(card)
        .into_any_element()
}

/// The 6px left-edge grab strip (drag to resize the drawer).
fn resize_handle(cx: &mut Context<JadeApp>, theme: &Theme) -> impl IntoElement {
    div()
        .id("wave-resize")
        .w(px(6.))
        .h_full()
        .flex_none()
        .cursor(gpui::CursorStyle::ResizeLeftRight)
        .hover(|s| s.bg(theme.kumo.tint))
        .on_mouse_down(
            MouseButton::Left,
            cx.listener(|app: &mut JadeApp, ev: &MouseDownEvent, _w, cx| {
                app.wave_resize = Some((f32::from(ev.position.x), app.wave_width));
                cx.notify();
            }),
        )
}

/// Run testbench · picker · zoom in / out · fit · Surfer.
fn toolbar(app: &JadeApp, cx: &mut Context<JadeApp>, theme: &Theme) -> impl IntoElement {
    let t = &theme.kumo;
    let hw = app.hw.as_ref();
    let running = hw.is_some_and(|h| h.wave.tb_running);
    let has_file = hw.is_some_and(|h| h.wave.file.is_some());
    let picker = hw.is_some_and(|h| h.wave.picker);
    let tb_name = app
        .wave_testbench()
        .and_then(|p| p.file_name().map(|n| n.to_string_lossy().into_owned()));
    let label = match (&tb_name, running) {
        (_, true) => "Running…".to_string(),
        (Some(n), false) => format!("Run {n}"),
        (None, false) => "Run testbench".to_string(),
    };

    let run = Button::new("wave-run-tb", label)
        .icon("play")
        .variant(ButtonVariant::Tinted)
        .ink(t.brand)
        .size(Size::Sm)
        .disabled(running)
        .render(t)
        .debug_selector(|| "wave-run-tb".into())
        .on_click(cx.listener(|a: &mut JadeApp, _e, _w, cx| {
            a.hw_run_testbench(true);
            cx.notify();
        }));

    let icon = |id: &'static str,
                name: &'static str,
                active: bool,
                cx: &mut Context<JadeApp>,
                f: fn(&mut JadeApp)| {
        kumo::button::icon_button(id, name, active, t).on_click(cx.listener(
            move |a: &mut JadeApp, _e, _w, cx| {
                f(a);
                cx.notify();
            },
        ))
    };

    let mut row = div()
        .flex()
        .flex_row()
        .items_center()
        .gap(scale::SPACE_1)
        .child(run)
        .child(div().flex_1());
    if has_file {
        row = row
            .child(icon("wave-picker", "list-tree", picker, cx, |a| {
                if let Some(hw) = &mut a.hw {
                    hw.wave.picker = !hw.wave.picker;
                }
            }))
            .child(icon("wave-zoom-in", "plus", false, cx, |a| a.wave_zoom_center(1.5)))
            .child(icon("wave-zoom-out", "minus", false, cx, |a| a.wave_zoom_center(1.0 / 1.5)))
            .child(icon("wave-fit", "activity", false, cx, |a| a.wave_fit()))
            .child(icon("wave-surfer", "external-link", false, cx, |a| a.hw_open_surfer()));
    }
    row
}

/// Dump name · cursor time · hover time, or the last error.
fn status_row(app: &JadeApp, theme: &Theme) -> impl IntoElement {
    let t = &theme.kumo;
    let hw = app.hw.as_ref();
    let mut row = div()
        .flex()
        .flex_row()
        .items_center()
        .gap(scale::SPACE_2)
        .text_size(scale::TEXT_XS)
        .text_color(t.text_subtle)
        .font_family(crate::fonts::mono_family())
        .h(px(20.));
    let Some(hw) = hw else { return row };
    let wave = &hw.wave;
    if let Some(err) = &wave.error {
        return row.child(div().text_color(t.text_danger).child(err.clone()));
    }
    if wave.loading {
        return row.child("loading dump…");
    }
    let Some(file) = &wave.file else {
        return row.child("no dump yet");
    };
    let name = file
        .path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    row = row.child(Badge::new(name).variant(BadgeVariant::Neutral).tabular(true).render(t));
    row = row.child(div().child(format!("{} total", file.time_label(file.end_time as f64))));
    if let Some(c) = wave.cursor {
        row = row.child(
            div()
                .text_color(t.text_brand)
                .child(format!("▏{}", file.time_label(c as f64))),
        );
    }
    if let Some(h) = wave.hover_t {
        if h >= 0.0 {
            row = row.child(div().child(format!("~{}", file.time_label(h))));
        }
    }
    row
}

/// The sheet before any dump exists.
fn empty(app: &JadeApp, theme: &Theme) -> impl IntoElement {
    let body = if app.wave_testbench().is_some() {
        "Run the testbench to record a dump, then read the signals here."
    } else {
        "Add a tb_<name>.v with $dumpfile and $dumpvars, then run it."
    };
    Empty::new("No waveform")
        .body(body)
        .icon("activity")
        .size(Size::Sm)
        .render(&theme.kumo)
}

/// The picker: every scope and signal, with the shown ones checked.
fn picker(app: &JadeApp, cx: &mut Context<JadeApp>, theme: &Theme) -> impl IntoElement {
    let t = &theme.kumo;
    let hw = app.hw.as_ref().expect("picker needs hw");
    let file = hw.wave.file.clone().expect("picker needs a file");
    let rows = hw.wave.rows.clone();

    let mut list = div()
        .id("wave-picker-list")
        .flex()
        .flex_col()
        .flex_1()
        .min_h(px(0.))
        .overflow_y_scroll()
        .text_size(scale::TEXT_XS)
        .font_family(crate::fonts::mono_family());

    let done = Button::new("wave-picker-done", "Done")
        .variant(ButtonVariant::Secondary)
        .size(Size::Xs)
        .render(t)
        .on_click(cx.listener(|a: &mut JadeApp, _e, _w, cx| {
            if let Some(hw) = &mut a.hw {
                hw.wave.picker = false;
            }
            cx.notify();
        }));
    list = list.child(
        div()
            .flex()
            .flex_row()
            .items_center()
            .justify_between()
            .pb(scale::SPACE_1)
            .child(div().text_color(t.text_subtle).child("Signals"))
            .child(done),
    );

    for (si, scope) in file.scopes.iter().enumerate() {
        let indent = px(scope.depth as f32 * 12.0);
        let all_shown = !scope.signals.is_empty()
            && scope.signals.iter().all(|s| rows.contains(s));
        let scope_name = if scope.name.is_empty() { "(top)".to_string() } else { scope.name.clone() };
        list = list.child(
            div()
                .id(("wave-scope", si))
                .flex()
                .flex_row()
                .items_center()
                .gap(scale::SPACE_1)
                .h(px(ROW_H))
                .pl(indent)
                
                .cursor_pointer()
                .hover(|s| s.bg(t.tint))
                .text_color(t.text_strong)
                .child(kumo::icon(
                    if all_shown { "circle-check" } else { "circle" },
                    12.0,
                    if all_shown { t.brand } else { t.text_subtle },
                ))
                .child(scope_name)
                .on_click(cx.listener(move |a: &mut JadeApp, _e, _w, cx| {
                    a.wave_toggle_scope(si);
                    cx.notify();
                })),
        );
        for &sig in &scope.signals {
            let s = &file.signals[sig];
            let shown = rows.contains(&sig);
            let label = if s.width > 1 {
                format!("{}  ·{}", s.name, s.width)
            } else {
                s.name.clone()
            };
            list = list.child(
                div()
                    .id(("wave-pick", sig))
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(scale::SPACE_1)
                    .h(px(ROW_H))
                    .pl(px(scope.depth as f32 * 12.0 + 16.0))
                    
                    .cursor_pointer()
                    .hover(|s| s.bg(t.tint))
                    .text_color(if shown { t.text_default } else { t.text_subtle })
                    .child(kumo::icon(
                        if shown { "circle-check" } else { "circle" },
                        12.0,
                        if shown { t.brand } else { t.text_subtle },
                    ))
                    .child(label)
                    .on_click(cx.listener(move |a: &mut JadeApp, _e, _w, cx| {
                        a.wave_toggle_row(sig);
                        cx.notify();
                    })),
            );
        }
    }
    list
}

/// Ruler over names + values + the trace canvas.
fn sheet(
    app: &JadeApp,
    file: Arc<WaveFile>,
    cx: &mut Context<JadeApp>,
    theme: &Theme,
) -> impl IntoElement {
    let t = &theme.kumo;
    let hw = app.hw.as_ref().expect("sheet needs hw");
    let wave = &hw.wave;
    let rows = wave.rows.clone();
    let read_t = wave.read_time();
    let (t0, ppu, cursor) = (wave.t0, wave.ppu, wave.cursor);
    let canvas_w = app.wave_canvas_w();
    let sheet_h = (rows.len() as f32 * ROW_H).max(ROW_H);

    // ── Ruler ──────────────────────────────────────────────────────────────
    let ruler = {
        let file = file.clone();
        let ink: Hsla = t.text_subtle.into();
        let line = alpha(t.text_subtle, 0.5);
        let hover = wave.hover_t;
        let brand = t.brand;
        canvas(
            |_, _, _| (),
            move |bounds: Bounds<Pixels>, _, window, cx| {
                let ox = f32::from(bounds.origin.x);
                let oy = f32::from(bounds.origin.y);
                let w = f32::from(bounds.size.width);
                let h = f32::from(bounds.size.height);
                window.with_content_mask(Some(ContentMask { bounds }), |window| {
                    // Baseline.
                    window.paint_quad(gpui::fill(
                        Bounds {
                            origin: point(px(ox), px(oy + h - 1.0)),
                            size: size(px(w), px(1.0)),
                        },
                        line,
                    ));
                    let step = nice_step(70.0 / ppu);
                    let minor = step / 5.0;
                    let t_end = t0 + w as f64 / ppu;
                    let mut k = (t0 / minor).floor();
                    let mut guard = 0;
                    while k * minor <= t_end && guard < 4000 {
                        guard += 1;
                        let tt = k * minor;
                        k += 1.0;
                        if tt < 0.0 {
                            continue;
                        }
                        let x = ox + ((tt - t0) * ppu) as f32;
                        let major = ((tt / step).round() * step - tt).abs() < minor * 0.01;
                        let tick_h = if major { 7.0 } else { 3.0 };
                        window.paint_quad(gpui::fill(
                            Bounds {
                                origin: point(px(x), px(oy + h - tick_h)),
                                size: size(px(1.0), px(tick_h)),
                            },
                            line,
                        ));
                        if major {
                            // A label that would run past the right edge is
                            // dropped, not clipped mid-glyph.
                            let text = file.time_label(tt);
                            let room = ox + w - (x + 3.0);
                            paint_text(window, cx, &text, x + 3.0, oy + 1.0, ink, Some(room));
                        }
                    }
                    if let Some(c) = cursor {
                        let x = ox + ((c as f64 - t0) * ppu) as f32;
                        window.paint_quad(gpui::fill(
                            Bounds {
                                origin: point(px(x), px(oy)),
                                size: size(px(1.0), px(h)),
                            },
                            brand,
                        ));
                    }
                    if let Some(hv) = hover {
                        let x = ox + ((hv - t0) * ppu) as f32;
                        window.paint_quad(gpui::fill(
                            Bounds {
                                origin: point(px(x), px(oy + h - 10.0)),
                                size: size(px(1.0), px(10.0)),
                            },
                            alpha(brand, 0.5),
                        ));
                    }
                });
            },
        )
        .w(px(canvas_w))
        .h(px(RULER_H))
        .flex_none()
    };

    // ── Names + values ─────────────────────────────────────────────────────
    let mut names = div().flex().flex_col().flex_none().w(px(NAME_W + VAL_W));
    for (i, &sig_idx) in rows.iter().enumerate() {
        let Some(sig) = file.signals.get(sig_idx) else { continue };
        let value = sig.value_at(read_t);
        let unknown = value.is_some_and(|v| v.is_unknown());
        let text = value.map(|v| v.label(sig.width)).unwrap_or_else(|| "–".to_string());
        let stripe = if i % 2 == 0 { alpha(t.tint, 0.35) } else { alpha(t.tint, 0.0) };
        names = names.child(
            div()
                .debug_selector(move || format!("wave-row-{i}").into())
                .flex()
                .flex_row()
                .items_center()
                .h(px(ROW_H))
                .bg(stripe)
                .text_size(px(FONT_PX))
                .font_family(crate::fonts::mono_family())
                .child(
                    div()
                        .w(px(NAME_W))
                        .flex_none()
                        .overflow_hidden()
                        .whitespace_nowrap()
                        .text_color(t.text_default)
                        .child(sig.name.clone()),
                )
                .child(
                    div()
                        .w(px(VAL_W - 18.0))
                        .flex_none()
                        .overflow_hidden()
                        .whitespace_nowrap()
                        .text_color(if unknown { t.text_danger } else { t.text_subtle })
                        .child(text),
                )
                .child(
                    Button::icon_only(("wave-rm", i), "x")
                        .size(Size::Xs)
                        .variant(ButtonVariant::Ghost)
                        .ink(t.text_subtle)
                        .render(t)
                        .on_click(cx.listener(move |a: &mut JadeApp, _e, _w, cx| {
                            a.wave_toggle_row(sig_idx);
                            cx.notify();
                        })),
                ),
        );
    }

    // ── Trace canvas ───────────────────────────────────────────────────────
    let origin: Rc<Cell<(f32, f32)>> = Rc::new(Cell::new((0.0, 0.0)));
    let o_paint = origin.clone();
    let o_down = origin.clone();
    let o_move = origin.clone();
    let o_wheel = origin;
    let traces = {
        let file = file.clone();
        let rows = rows.clone();
        let one = t.text_success;
        let bus = t.text_info;
        let bad = t.text_danger;
        let brand = t.brand;
        let stripe = alpha(t.tint, 0.35);
        let hair = alpha(t.text_subtle, 0.12);
        let ink: Hsla = t.text_default.into();
        let bad_ink: Hsla = t.text_danger.into();
        canvas(
            |_, _, _| (),
            move |bounds: Bounds<Pixels>, _, window, cx| {
                let ox = f32::from(bounds.origin.x);
                let oy = f32::from(bounds.origin.y);
                let w = f32::from(bounds.size.width);
                o_paint.set((ox, oy));
                window.with_content_mask(Some(ContentMask { bounds }), |window| {
                    for (i, &sig_idx) in rows.iter().enumerate() {
                        let y = oy + i as f32 * ROW_H;
                        if i % 2 == 0 {
                            window.paint_quad(gpui::fill(
                                Bounds {
                                    origin: point(px(ox), px(y)),
                                    size: size(px(w), px(ROW_H)),
                                },
                                stripe,
                            ));
                        }
                        window.paint_quad(gpui::fill(
                            Bounds {
                                origin: point(px(ox), px(y + ROW_H - 1.0)),
                                size: size(px(w), px(1.0)),
                            },
                            hair,
                        ));
                        let Some(sig) = file.signals.get(sig_idx) else { continue };
                        let colors = TraceInk { one, bus, bad, ink, bad_ink };
                        paint_signal(window, cx, sig, ox, y, w, t0, ppu, &colors);
                    }
                    if let Some(c) = cursor {
                        let x = ox + ((c as f64 - t0) * ppu) as f32;
                        window.paint_quad(gpui::fill(
                            Bounds {
                                origin: point(px(x), px(oy)),
                                size: size(px(1.0), px(f32::from(bounds.size.height))),
                            },
                            brand,
                        ));
                    }
                });
            },
        )
        .w(px(canvas_w))
        .h(px(sheet_h))
        .flex_none()
    };

    let canvas_box = div()
        .id("wave-canvas")
        .debug_selector(|| "wave-canvas".into())
        .relative()
        .w(px(canvas_w))
        .h(px(sheet_h))
        .flex_none()
        .cursor(gpui::CursorStyle::Crosshair)
        .on_mouse_down(
            MouseButton::Left,
            cx.listener(move |a: &mut JadeApp, ev: &MouseDownEvent, _w, cx| {
                let (ox, _) = o_down.get();
                let x = f32::from(ev.position.x) - ox;
                if let Some(hw) = &mut a.hw {
                    hw.wave.set_cursor_x(x);
                    hw.wave.drag = Some((f32::from(ev.position.x), hw.wave.t0));
                    hw.wave.picker = false;
                }
                cx.notify();
            }),
        )
        .on_mouse_move(cx.listener(move |a: &mut JadeApp, ev: &MouseMoveEvent, _w, cx| {
            let (ox, _) = o_move.get();
            let x = f32::from(ev.position.x) - ox;
            let canvas_w = a.wave_canvas_w();
            let Some(hw) = &mut a.hw else { return };
            let mut changed = false;
            if let Some((sx, st0)) = hw.wave.drag {
                if ev.pressed_button == Some(MouseButton::Left) {
                    let dx = f32::from(ev.position.x) - sx;
                    hw.wave.t0 = st0;
                    hw.wave.pan(-dx, canvas_w);
                    changed = true;
                } else {
                    hw.wave.drag = None;
                }
            }
            let ht = Some(hw.wave.time_at(x));
            if hw.wave.hover_t != ht {
                hw.wave.hover_t = ht;
                changed = true;
            }
            if changed {
                cx.notify();
            }
        }))
        .on_mouse_up(
            MouseButton::Left,
            cx.listener(|a: &mut JadeApp, _ev: &MouseUpEvent, _w, cx| {
                if let Some(hw) = &mut a.hw {
                    if hw.wave.drag.take().is_some() {
                        cx.notify();
                    }
                }
            }),
        )
        .on_hover(cx.listener(|a: &mut JadeApp, hovered: &bool, _w, cx| {
            if !hovered {
                if let Some(hw) = &mut a.hw {
                    if hw.wave.hover_t.take().is_some() {
                        cx.notify();
                    }
                }
            }
        }))
        .on_scroll_wheel(cx.listener(move |a: &mut JadeApp, ev: &ScrollWheelEvent, _w, cx| {
            let (dx, dy) = match ev.delta {
                ScrollDelta::Pixels(p) => (f32::from(p.x), f32::from(p.y)),
                ScrollDelta::Lines(p) => (p.x * 20.0, p.y * 20.0),
            };
            let canvas_w = a.wave_canvas_w();
            let (ox, _) = o_wheel.get();
            let anchor = (f32::from(ev.position.x) - ox).clamp(0.0, canvas_w);
            let Some(hw) = &mut a.hw else { return };
            if dy.abs() > 0.01 {
                hw.wave.zoom((-dy as f64 * 0.01).exp(), anchor, canvas_w);
            }
            if dx.abs() > 0.01 {
                hw.wave.pan(-dx, canvas_w);
            }
            cx.stop_propagation();
            cx.notify();
        }))
        .child(traces);

    div()
        .flex()
        .flex_col()
        .flex_1()
        .min_h(px(0.))
        .child(
            div()
                .flex()
                .flex_row()
                .flex_none()
                .child(div().w(px(NAME_W + VAL_W)).flex_none())
                .child(ruler),
        )
        .child(
            div()
                .id("wave-scroll")
                .flex()
                .flex_row()
                .flex_1()
                .min_h(px(0.))
                .overflow_y_scroll()
                .child(names)
                .child(canvas_box),
        )
}

/// Ink for one trace.
struct TraceInk {
    one: Rgba,
    bus: Rgba,
    bad: Rgba,
    ink: Hsla,
    bad_ink: Hsla,
}

/// Paint one signal's trace across the visible window.
///
/// Segments narrower than one pixel are merged into an activity block, so a
/// zoomed-out clock still reads as a solid band without a quad per edge.
#[allow(clippy::too_many_arguments)]
fn paint_signal(
    window: &mut gpui::Window,
    cx: &mut gpui::App,
    sig: &jade_hw::wave::WaveSignal,
    ox: f32,
    y: f32,
    w: f32,
    t0: f64,
    ppu: f64,
    ink: &TraceInk,
) {
    let y_hi = y + INSET;
    let y_lo = y + ROW_H - INSET - 1.0;
    let t_end = t0 + w as f64 / ppu;
    let changes = &sig.changes;
    if changes.is_empty() {
        return;
    }
    // The change in force at the left edge, then every change in view.
    let start = changes.partition_point(|c| (c.time as f64) < t0).saturating_sub(1);
    let one_bit = sig.width == 1 && !matches!(changes[0].value, WaveValue::Real(_) | WaveValue::Str(_));
    let color = if one_bit { ink.one } else { ink.bus };

    let mut i = start;
    let mut dense_from: Option<f32> = None;
    while i < changes.len() {
        let c = &changes[i];
        let ta = c.time as f64;
        if ta > t_end {
            break;
        }
        let tb = changes.get(i + 1).map(|n| n.time as f64).unwrap_or(f64::INFINITY);
        let xa = ((ta - t0) * ppu) as f32;
        let xb = if tb.is_finite() { ((tb - t0) * ppu) as f32 } else { w + 2.0 };
        let xa_c = xa.max(-1.0);
        let xb_c = xb.min(w + 1.0);

        // Merge sub-pixel segments into one activity block.
        if xb - xa < 1.0 {
            if dense_from.is_none() {
                dense_from = Some(xa_c);
            }
            i += 1;
            continue;
        }
        if let Some(from) = dense_from.take() {
            let to = xa_c.max(from + 1.0);
            window.paint_quad(gpui::fill(
                Bounds {
                    origin: point(px(ox + from), px(y_hi)),
                    size: size(px(to - from), px(y_lo - y_hi + 1.0)),
                },
                alpha(color, 0.55),
            ));
        }

        let unknown = c.value.is_unknown();
        if one_bit {
            let level = c.value.bit();
            let yy = match level {
                Some(true) => y_hi,
                Some(false) => y_lo,
                None => (y_hi + y_lo) / 2.0,
            };
            let col = if level.is_none() { ink.bad } else { color };
            window.paint_quad(gpui::fill(
                Bounds {
                    origin: point(px(ox + xa_c), px(yy)),
                    size: size(px((xb_c - xa_c).max(1.0)), px(1.0)),
                },
                col,
            ));
            // The edge at the segment start.
            if i > start || (ta - t0).abs() < 1e-9 {
                if xa >= 0.0 && xa <= w {
                    window.paint_quad(gpui::fill(
                        Bounds {
                            origin: point(px(ox + xa), px(y_hi)),
                            size: size(px(1.0), px(y_lo - y_hi + 1.0)),
                        },
                        col,
                    ));
                }
            }
        } else {
            let col = if unknown { ink.bad } else { color };
            let band = Bounds {
                origin: point(px(ox + xa_c), px(y_hi)),
                size: size(px((xb_c - xa_c).max(1.0)), px(y_lo - y_hi + 1.0)),
            };
            window.paint_quad(gpui::fill(band, alpha(col, if unknown { 0.22 } else { 0.10 })));
            for yy in [y_hi, y_lo] {
                window.paint_quad(gpui::fill(
                    Bounds {
                        origin: point(px(ox + xa_c), px(yy)),
                        size: size(px((xb_c - xa_c).max(1.0)), px(1.0)),
                    },
                    col,
                ));
            }
            if xa >= 0.0 && xa <= w {
                window.paint_quad(gpui::fill(
                    Bounds {
                        origin: point(px(ox + xa), px(y_hi)),
                        size: size(px(1.0), px(y_lo - y_hi + 1.0)),
                    },
                    col,
                ));
            }
            // The value label, when the visible part of the segment fits it.
            let vis_a = xa.max(0.0);
            let vis_b = xb.min(w);
            if vis_b - vis_a > LABEL_PAD * 2.0 + 8.0 {
                let text = c.value.label(sig.width);
                let text_ink = if unknown { ink.bad_ink } else { ink.ink };
                paint_text(
                    window,
                    cx,
                    &text,
                    ox + vis_a + LABEL_PAD,
                    y + (ROW_H - FONT_PX) / 2.0 - 2.0,
                    text_ink,
                    Some(vis_b - vis_a - LABEL_PAD * 2.0),
                );
            }
        }
        i += 1;
    }
    if let Some(from) = dense_from.take() {
        let to = w.min(from + 1.0).max(from + 1.0);
        window.paint_quad(gpui::fill(
            Bounds {
                origin: point(px(ox + from), px(y_hi)),
                size: size(px(to - from), px(y_lo - y_hi + 1.0)),
            },
            alpha(color, 0.55),
        ));
    }
}

/// Paint one line of mono text at (x, y). With `max_w`, the text is
/// skipped when it would not fit.
fn paint_text(
    window: &mut gpui::Window,
    cx: &mut gpui::App,
    text: &str,
    x: f32,
    y: f32,
    color: Hsla,
    max_w: Option<f32>,
) {
    let run = TextRun {
        len: text.len(),
        font: gpui::font(crate::fonts::mono_family()),
        color,
        background_color: None,
        underline: None,
        strikethrough: None,
    };
    let line = window
        .text_system()
        .shape_line(SharedString::from(text.to_string()), px(FONT_PX), &[run], None);
    if let Some(max_w) = max_w {
        if f32::from(line.width) > max_w {
            return;
        }
    }
    let _ = line.paint(point(px(x), px(y)), px(FONT_PX + 4.0), TextAlign::Left, None, window, cx);
}

/// The 1-2-5 step, in file units, no smaller than `min_step`.
fn nice_step(min_step: f64) -> f64 {
    if !(min_step > 0.0) || !min_step.is_finite() {
        return 1.0;
    }
    let exp = min_step.log10().floor();
    let base = 10f64.powf(exp);
    for m in [1.0, 2.0, 5.0, 10.0] {
        if base * m >= min_step {
            return (base * m).max(1.0);
        }
    }
    (base * 10.0).max(1.0)
}

#[cfg(test)]
mod tests {
    use super::nice_step;

    #[test]
    fn steps_are_one_two_five() {
        assert_eq!(nice_step(0.3), 1.0);
        assert_eq!(nice_step(1.0), 1.0);
        assert_eq!(nice_step(1.5), 2.0);
        assert_eq!(nice_step(3.0), 5.0);
        assert_eq!(nice_step(7.0), 10.0);
        assert_eq!(nice_step(70.0), 100.0);
        assert_eq!(nice_step(130.0), 200.0);
    }
}
