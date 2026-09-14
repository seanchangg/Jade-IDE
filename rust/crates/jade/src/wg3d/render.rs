//! GPUI view for the 3D weight grid (feature inventory §7.2). Pure-painting
//! port of the WebGL2 overlay: the mat4 math (`super::math`) projects each bar's
//! visible faces to screen space, which are painted as filled paths via
//! `window.paint_path` in a full-size `canvas`; the reference grid + value axis
//! are stroked polylines; text labels are absolutely-positioned `div` overlays
//! whose anchors are projected on the Rust side.
//!
//! Depth is resolved by the painter's algorithm — bars are sorted back-to-front
//! by camera-space depth each frame. Faces are shaded by the original Lambert
//! rig (top face brightest). Left-drag orbits, Shift/right-drag pans, the wheel
//! dollies; damping continues via a 16 ms repaint loop that stops when idle.

use std::cell::Cell;
use std::rc::Rc;
use std::time::Duration;

use gpui::{
    canvas, div, point, prelude::*, px, rgb, Bounds, Context, FocusHandle, KeyDownEvent,
    MouseButton, MouseDownEvent, MouseMoveEvent, MouseUpEvent, PathBuilder, Pixels, Rgba,
    ScrollDelta, ScrollWheelEvent, SharedString, Window,
};

use jade_telemetry::Kind;

use crate::app::dim_input::DimField;
use crate::app::JadeApp;
use crate::registry::DEFAULT_MAX_DIM;
use crate::theme::Theme;

use super::camera::{OrbitCamera, HEIGHT_SCALE};
use super::grid::{fmt_axis, Bar, BarGrid};
use super::math::{self, Mat4};

const TOOLBAR_H: f32 = 34.0;

/// Lazily create + return the overlay's focus handle (needed for Esc / key
/// dispatch). Called from `JadeApp::render` before building the tree.
pub fn ensure_focus(app: &mut JadeApp, cx: &mut Context<JadeApp>) -> FocusHandle {
    app.wg3d
        .focus
        .get_or_insert_with(|| cx.focus_handle())
        .clone()
}

/// Wake the damping repaint loop if it isn't already running and the overlay is
/// visible. Ticks the camera every ~16 ms and stops when motion settles — the
/// §7.2 idle-stopping contract (no `while(true)` repaint).
pub fn ensure_anim(app: &mut JadeApp, cx: &mut Context<JadeApp>) {
    if app.wg3d.anim_running || !app.wg3d.visible {
        return;
    }
    app.wg3d.anim_running = true;
    cx.spawn(async move |this, cx| {
        loop {
            cx.background_executor()
                .timer(Duration::from_millis(16))
                .await;
            let keep_going = this
                .update(cx, |app, cx| {
                    if !app.wg3d.visible {
                        return false;
                    }
                    // Keep ticking while dragging (fresh input) or damping.
                    let moving = app.wg3d.camera.update();
                    let dragging = app.wg3d.is_dragging();
                    if moving || dragging {
                        cx.notify();
                    }
                    moving || dragging
                })
                .unwrap_or(false);
            if !keep_going {
                break;
            }
        }
        let _ = this.update(cx, |app, _| app.wg3d.anim_running = false);
    })
    .detach();
}

/// Build the full-window overlay element. `focus` is the pre-created handle;
/// `vp_w`/`vp_h` are the window viewport dimensions and `scale` the window's
/// device-pixel scale factor (capped at 2 like the TS dpr cap).
pub fn overlay(
    app: &mut JadeApp,
    focus: FocusHandle,
    vp_w: f32,
    vp_h: f32,
    scale: f32,
    cx: &mut Context<JadeApp>,
) -> gpui::AnyElement {
    let theme = app.theme.clone();
    let canvas_w = vp_w;
    let canvas_h = (vp_h - TOOLBAR_H).max(1.0);

    let grid = app.wg3d.current_bars_cached();

    let root = div()
        .absolute()
        .top(px(0.))
        .left(px(0.))
        .w(px(vp_w))
        .h(px(vp_h))
        .flex()
        .flex_col()
        .bg(rgb(theme.bg)) // the editor canvas, so the overlay reads as a page of the app
        .track_focus(&focus)
        .on_key_down(cx.listener(|app, ev: &KeyDownEvent, _win, cx| {
            if ev.keystroke.key == "escape" {
                app.wg3d.close();
                cx.notify();
            }
        }))
        .child(toolbar(app, &theme, cx))
        .child(scene(app, grid, &theme, canvas_w, canvas_h, scale.min(2.0), cx));

    root.into_any_element()
}

// ── Toolbar ───────────────────────────────────────────────────────────────────

fn toolbar(app: &JadeApp, theme: &Theme, cx: &mut Context<JadeApp>) -> gpui::AnyElement {
    // The overlay covers the title bar, so the toolbar clears the traffic
    // lights the way the action bar does.
    let mut bar = div()
        .flex()
        .flex_row()
        .items_center()
        .gap_2()
        .h(px(TOOLBAR_H))
        .pl(px(80.))
        .pr(px(6.))
        .bg(theme.kumo.elevated)
        .border_b_1()
        .border_color(theme.kumo.hairline)
        .window_control_area(gpui::WindowControlArea::Drag)
        .child(
            div()
                .text_color(rgb(theme.muted))
                .text_xs()
                .font_weight(gpui::FontWeight::SEMIBOLD)
                .child("3D WEIGHTS"),
        );

    // Buffer selector: a clickable chip per discovered buffer.
    for name in app.wg3d.buffer_names() {
        let selected = app.wg3d.selected() == Some(name.as_str());
        let n = name.clone();
        bar = bar.child(
            div()
                .id(SharedString::from(format!("wg3d-buf-{name}")))
                .h(px(22.))
                .px_2()
                .flex()
                .items_center()
                .text_xs()
                .font_family(crate::fonts::mono_family())
                .bg(if selected { theme.kumo.tint } else { theme.kumo.elevated })
                .text_color(rgb(if selected { theme.text } else { theme.muted }))
                .hover(|s| s.bg(theme.kumo.tint))
                .cursor_pointer()
                .on_click(cx.listener(move |app, _e, _w, cx| {
                    app.wg3d.select_buffer(&n);
                    ensure_anim(app, cx);
                    cx.notify();
                }))
                .child(name.clone()),
        );
    }

    // Streaming resolution (≤64/≤128/≤256, weight-grid-3d.ts `resSelect`,
    // default 256): the chips reflect the REGISTRY's per-buffer maxDim (the
    // value actually sent to the probe), not the local paint budget — those
    // are two different caps. `set_buffer_max_dim` persists + re-pushes
    // `track`; `set_view_max_dim` is kept alongside as the separate Rust-side
    // CPU sort+paint budget (never shown to the user).
    let sel_name = app.wg3d.selected().map(|s| s.to_string());
    let cur_res = sel_name
        .as_deref()
        .and_then(|n| app.registry.get(Kind::Buffer, n))
        .and_then(|i| i.max_dim)
        .unwrap_or(DEFAULT_MAX_DIM);
    for dim in [64u32, 128, 256] {
        let name = sel_name.clone();
        let active = cur_res == dim;
        bar = bar.child(
            div()
                .id(SharedString::from(format!("wg3d-res-{dim}")))
                .px_1()
                .text_size(px(10.))
                
                .text_color(rgb(if active { theme.accent } else { theme.muted }))
                .cursor_pointer()
                .on_click(cx.listener(move |app, _e, _w, cx| {
                    app.wg3d.set_view_max_dim(dim);
                    if let Some(n) = &name {
                        app.set_buffer_max_dim(n, dim);
                    }
                    cx.notify();
                }))
                .child(format!("≤{dim}")),
        );
    }

    // Tensor shape hint: rows × cols, editable inline (weight-grid-3d.ts
    // `makeDimInput` + `applyShape`, lines 250-287/474-482). Prefilled from
    // the registry's stored hint; empty shows the latest frame's source dims
    // as a placeholder (`syncDimInputs`).
    if let Some(name) = sel_name {
        bar = bar.child(dim_fields(app, theme, cx, &name));
    }

    // Step scrubber: draggable track plus ◀ / ▶ stepping and a Live jump.
    bar = bar
        .child(step_button("wg3d-prev", "◀", theme, cx, -1))
        .child(scrub_track(app, theme, cx))
        .child(step_button("wg3d-next", "▶", theme, cx, 1))
        .child(
            div()
                .id("wg3d-live")
                .px_1()
                .text_size(px(10.))
                
                .text_color(rgb(if app.wg3d.is_live() {
                    theme.accent
                } else {
                    theme.muted
                }))
                .cursor_pointer()
                .on_click(cx.listener(move |app, _e, _w, cx| {
                    let last = app.wg3d.frames_len().saturating_sub(1);
                    app.wg3d.scrub(last);
                    ensure_anim(app, cx);
                    cx.notify();
                }))
                .child("live"),
        );

    // Readout + close.
    bar = bar
        .child(
            div()
                .flex_1()
                .overflow_hidden()
                .text_size(px(10.))
                .text_color(rgb(theme.text))
                .child(app.wg3d.readout()),
        )
        .child(
            crate::kumo::button::icon_button("wg3d-close", "x", false, &theme.kumo).on_click(
                cx.listener(|app, _e, _w, cx| {
                    app.wg3d.close();
                    cx.notify();
                }),
            ),
        );

    bar.into_any_element()
}

/// The rows×cols shape-hint fields for `name` (a buffer) — shares the
/// captured-keystroke editor (`app.dim_edit`) with the telemetry sidebar's
/// inline row editor (`panels/telemetry_sidebar.rs`). When no session is open
/// for this buffer, shows the stored hint (or the placeholder, muted) and
/// starts a session on click; while a session is open, shows the two live
/// text buffers with the active one underlined + caret, and clicking either
/// field just moves the caret between them.
fn dim_fields(app: &JadeApp, theme: &Theme, cx: &mut Context<JadeApp>, name: &str) -> gpui::AnyElement {
    let editing = app.dim_edit().filter(|s| s.name == name);
    let (ph_rows, ph_cols) = app.dim_edit_placeholder(name);
    let hint = app.registry.get(Kind::Buffer, name).and_then(|i| i.effective_shape());

    let (rows_raw, cols_raw, active_field) = match editing {
        Some(s) => (s.rows.clone(), s.cols.clone(), Some(s.field)),
        None => (
            hint.map(|(r, _)| r.to_string()).unwrap_or_default(),
            hint.map(|(_, c)| c.to_string()).unwrap_or_default(),
            None,
        ),
    };
    let (rows_text, rows_ph) = crate::app::dim_input::display_value(&rows_raw, &ph_rows);
    let (cols_text, cols_ph) = crate::app::dim_input::display_value(&cols_raw, &ph_cols);

    let mut wrap = div()
        .id(SharedString::from(format!("wg3d-dim-{name}")))
        .flex()
        .flex_row()
        .items_center()
        .gap_1()
        .text_size(px(10.));

    if editing.is_some() {
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
    }

    wrap.child(dim_field_chip(
        name,
        DimField::Rows,
        rows_text,
        rows_ph,
        active_field == Some(DimField::Rows),
        theme,
        cx,
    ))
    .child(div().text_color(rgb(theme.muted)).child("×"))
    .child(dim_field_chip(
        name,
        DimField::Cols,
        cols_text,
        cols_ph,
        active_field == Some(DimField::Cols),
        theme,
        cx,
    ))
    .into_any_element()
}

/// One rows/cols field: static + clickable (opens the editor, or — while a
/// session for this buffer is already open — just moves the caret to this
/// field) when not active; live text + caret + accent underline when active.
fn dim_field_chip(
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
            "wg3d-dimfield-{name}-{}",
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

fn step_button(
    id: &'static str,
    glyph: &'static str,
    theme: &Theme,
    cx: &mut Context<JadeApp>,
    delta: i64,
) -> gpui::AnyElement {
    div()
        .id(id)
        .px_1()
        .text_size(px(11.))
        .text_color(rgb(theme.text))
        .cursor_pointer()
        .on_click(cx.listener(move |app, _e, _w, cx| {
            let len = app.wg3d.frames_len();
            if len == 0 {
                return;
            }
            let cur = app.wg3d.index() as i64;
            let next = (cur + delta).clamp(0, len as i64 - 1) as usize;
            app.wg3d.scrub(next);
            ensure_anim(app, cx);
            cx.notify();
        }))
        .child(glyph)
        .into_any_element()
}

/// The training-step scrubber track: click or drag to pick a frame. The track
/// origin is captured at paint time (Rc<Cell>) so the mouse listeners can map
/// window-space x → frame index; the live/freeze rule lives in `scrub()`.
fn scrub_track(app: &JadeApp, theme: &Theme, cx: &mut Context<JadeApp>) -> gpui::AnyElement {
    const W: f32 = 120.0;
    let len = app.wg3d.frames_len();
    let frac = if len <= 1 {
        1.0
    } else {
        app.wg3d.index() as f32 / (len as f32 - 1.0)
    };

    let origin: Rc<Cell<f32>> = Rc::new(Cell::new(0.0));
    let o_paint = origin.clone();
    let o_down = origin.clone();
    let o_move = origin;

    let x_to_idx = move |x: f32, origin_x: f32, len: usize| -> usize {
        if len <= 1 {
            return 0;
        }
        let f = ((x - origin_x) / W).clamp(0.0, 1.0);
        (f * (len as f32 - 1.0)).round() as usize
    };
    let x_to_idx_move = x_to_idx;

    let bg = rgb(theme.bg);
    let accent = rgb(theme.accent);

    div()
        .id("wg3d-scrub")
        .w(px(W))
        .h(px(12.))
        .flex()
        .items_center()
        .cursor_pointer()
        .on_mouse_down(
            MouseButton::Left,
            cx.listener(move |app, ev: &MouseDownEvent, _w, cx| {
                let len = app.wg3d.frames_len();
                app.wg3d
                    .scrub(x_to_idx(f32::from(ev.position.x), o_down.get(), len));
                cx.notify();
            }),
        )
        .on_mouse_move(cx.listener(move |app, ev: &MouseMoveEvent, _w, cx| {
            if ev.pressed_button == Some(MouseButton::Left) && !app.wg3d.is_dragging() {
                let len = app.wg3d.frames_len();
                app.wg3d
                    .scrub(x_to_idx_move(f32::from(ev.position.x), o_move.get(), len));
                cx.notify();
            }
        }))
        .child(
            div()
                .relative()
                .w(px(W))
                .h(px(6.))
                
                .bg(bg)
                .child(
                    // Invisible full-size canvas records the track's
                    // window-space origin for the listeners above.
                    canvas(
                        move |b: Bounds<Pixels>, _, _| {
                            o_paint.set(f32::from(b.origin.x));
                        },
                        |_, _, _, _| {},
                    )
                    .absolute()
                    .size_full(),
                )
                .child(
                    div()
                        .h_full()
                        .w(px(W * frac))
                        
                        .bg(accent),
                ),
        )
        .into_any_element()
}

// ── Scene (canvas + projected label overlays) ──────────────────────────────────

fn scene(
    app: &mut JadeApp,
    grid: Option<(std::sync::Arc<BarGrid>, u64)>,
    theme: &Theme,
    w: f32,
    h: f32,
    scale: f32,
    cx: &mut Context<JadeApp>,
) -> gpui::AnyElement {
    let camera = app.wg3d.camera.clone();

    let mut area = div().relative().flex_1().w_full().overflow_hidden();

    // Interaction surface (fills the scene): orbit / pan / dolly.
    area = area
        .on_mouse_down(
            MouseButton::Left,
            cx.listener(move |app, ev: &MouseDownEvent, _w, cx| {
                let pan = ev.modifiers.shift;
                app.wg3d
                    .pointer_down(f32::from(ev.position.x), f32::from(ev.position.y), pan);
                ensure_anim(app, cx);
            }),
        )
        .on_mouse_down(
            MouseButton::Right,
            cx.listener(move |app, ev: &MouseDownEvent, _w, cx| {
                app.wg3d
                    .pointer_down(f32::from(ev.position.x), f32::from(ev.position.y), true);
                ensure_anim(app, cx);
            }),
        )
        .on_mouse_move(cx.listener(move |app, ev: &MouseMoveEvent, _w, cx| {
            if app.wg3d.is_dragging() && ev.pressed_button.is_some() {
                let moved = app.wg3d.pointer_move(
                    f32::from(ev.position.x),
                    f32::from(ev.position.y),
                    h,
                );
                if moved {
                    ensure_anim(app, cx);
                    cx.notify();
                }
            }
        }))
        .on_mouse_up(
            MouseButton::Left,
            cx.listener(|app, _ev: &MouseUpEvent, _w, _cx| app.wg3d.pointer_up()),
        )
        .on_mouse_up(
            MouseButton::Right,
            cx.listener(|app, _ev: &MouseUpEvent, _w, _cx| app.wg3d.pointer_up()),
        )
        // End the drag even when the release lands outside the scene.
        .on_mouse_up_out(
            MouseButton::Left,
            cx.listener(|app, _ev: &MouseUpEvent, _w, _cx| app.wg3d.pointer_up()),
        )
        .on_scroll_wheel(cx.listener(move |app, ev: &ScrollWheelEvent, _w, cx| {
            let dy = match ev.delta {
                ScrollDelta::Lines(p) => p.y * 16.0,
                ScrollDelta::Pixels(p) => f32::from(p.y),
            };
            app.wg3d.wheel(dy);
            ensure_anim(app, cx);
            cx.notify();
        }));

    // Paint layer: the Metal engine when available (the WebGL port — one
    // instanced draw), else the CPU painter (headless tests, Metal-less VMs).
    if let Some((g, generation)) = grid {
        // Compute label placements on the Rust side (projected anchors).
        let labels = label_placements(&g, &camera, w, h);

        let mut painted = false;
        #[cfg(target_os = "macos")]
        {
            if let Some(gpu) = app.wg3d_gpu() {
                let w_px = (w * scale).round().max(2.0) as usize;
                let h_px = (h * scale).round().max(2.0) as usize;
                if let Some(pb) = gpu.render(&g, generation, &camera, w_px, h_px) {
                    area = area.child(
                        gpui::surface(pb)
                            .object_fit(gpui::ObjectFit::Fill)
                            .absolute()
                            .top(px(0.))
                            .left(px(0.))
                            .w(px(w))
                            .h(px(h)),
                    );
                    painted = true;
                }
            }
        }
        #[cfg(not(target_os = "macos"))]
        let _ = (scale, generation);
        if !painted {
            area = area.child(scene_canvas((*g).clone(), camera, theme.clone()));
        }

        // Label overlays, absolutely positioned over the canvas area.
        for (text, x, y) in labels {
            area = area.child(
                div()
                    .absolute()
                    .left(px(x - 12.0))
                    .top(px(y - 7.0))
                    .text_size(px(11.))
                    .text_color(rgb(0xBEC4D6))
                    .child(text),
            );
        }
    } else {
        area = area.child(
            div()
                .absolute()
                .top(px(12.))
                .left(px(12.))
                .text_color(rgb(theme.muted))
                .child("no frames — enable a buffer to stream tensor data"),
        );
    }

    area.into_any_element()
}

/// The painting canvas: projects + depth-sorts bars, shades faces, strokes the
/// grid plane + value axis. `size_full()` per the deliverable requirement.
fn scene_canvas(g: BarGrid, camera: OrbitCamera, _theme: Theme) -> gpui::AnyElement {
    canvas(
        move |_, _, _| {},
        move |bounds: Bounds<Pixels>, _, window: &mut Window, _| {
            let w = f32::from(bounds.size.width);
            let h = f32::from(bounds.size.height);
            if w <= 1.0 || h <= 1.0 {
                return;
            }
            let ox = f32::from(bounds.origin.x);
            let oy = f32::from(bounds.origin.y);

            let proj = Mat4::perspective(camera.fov_y, w / h, 0.1, 4000.0);
            let view = camera.view_matrix();
            let mvp = proj.mul(&view);
            let eye = camera.eye();

            let to_screen = |p: [f32; 3]| -> Option<gpui::Point<Pixels>> {
                math::project(&mvp, p, w, h).map(|pr| point(px(ox + pr.x), px(oy + pr.y)))
            };

            // 1) Reference grid on y=0 (painted first; bars occlude it).
            paint_grid(window, g.rows, g.cols, &to_screen);

            // 2) Bars, back-to-front (painter's algorithm).
            let mut order: Vec<usize> = (0..g.bars.len()).collect();
            order.sort_by(|&a, &b| {
                let da = math::view_depth(&view, g.bars[a].centroid());
                let db = math::view_depth(&view, g.bars[b].centroid());
                da.partial_cmp(&db).unwrap_or(std::cmp::Ordering::Equal)
            });
            for &i in &order {
                paint_bar(window, &g.bars[i], eye, &to_screen);
            }

            // 3) Value axis line at the back-left corner (drawn over bars).
            paint_value_axis(window, g.rows, g.cols, &to_screen);
        },
    )
    .size_full()
    .into_any_element()
}

/// Reference grid (weight-grid-3d.ts `buildGrid`): aspect-correct lines on y=0,
/// center line brighter, all at 0.35 alpha.
fn paint_grid(
    window: &mut Window,
    rows: u32,
    cols: u32,
    to_screen: &impl Fn([f32; 3]) -> Option<gpui::Point<Pixels>>,
) {
    let size = rows.max(cols).max(1);
    let sx = cols as f32 / size as f32;
    let sz = rows as f32 / size as f32;
    let half = size as f32 / 2.0;
    let center = Rgba { r: 0x44 as f32 / 255.0, g: 0x44 as f32 / 255.0, b: 0x55 as f32 / 255.0, a: 0.35 };
    let line = Rgba { r: 0x2a as f32 / 255.0, g: 0x2a as f32 / 255.0, b: 0x34 as f32 / 255.0, a: 0.35 };
    for i in 0..=size {
        let k = -half + i as f32;
        let color = if i * 2 == size { center } else { line };
        stroke_line(window, [-half * sx, 0.0, k * sz], [half * sx, 0.0, k * sz], color, to_screen);
        stroke_line(window, [k * sx, 0.0, -half * sz], [k * sx, 0.0, half * sz], color, to_screen);
    }
}

fn paint_value_axis(
    window: &mut Window,
    rows: u32,
    cols: u32,
    to_screen: &impl Fn([f32; 3]) -> Option<gpui::Point<Pixels>>,
) {
    let margin = (rows.max(cols) as f32 * 0.07).max(2.5);
    let cx = -(cols as f32) / 2.0 - margin;
    let cz = -(rows as f32) / 2.0 - margin;
    let color = Rgba { r: 0x66 as f32 / 255.0, g: 0x6a as f32 / 255.0, b: 0x7a as f32 / 255.0, a: 0.8 };
    stroke_line(window, [cx, -HEIGHT_SCALE, cz], [cx, HEIGHT_SCALE, cz], color, to_screen);
}

fn stroke_line(
    window: &mut Window,
    a: [f32; 3],
    b: [f32; 3],
    color: Rgba,
    to_screen: &impl Fn([f32; 3]) -> Option<gpui::Point<Pixels>>,
) {
    if let (Some(pa), Some(pb)) = (to_screen(a), to_screen(b)) {
        let mut pb_builder = PathBuilder::stroke(px(1.0));
        pb_builder.move_to(pa);
        pb_builder.line_to(pb);
        if let Ok(path) = pb_builder.build() {
            window.paint_path(path, color);
        }
    }
}

/// Light rig from the original fragment shader (weight-grid-3d.ts:63-74):
/// ambient 0.65 + two directionals; top face ends up brightest.
fn face_light(n: [f32; 3]) -> f32 {
    const L1: [f32; 3] = [0.4240, 0.8480, 0.3180];
    const L2: [f32; 3] = [-0.4243, 0.5657, -0.7071];
    let d1 = (n[0] * L1[0] + n[1] * L1[1] + n[2] * L1[2]).max(0.0);
    let d2 = (n[0] * L2[0] + n[1] * L2[1] + n[2] * L2[2]).max(0.0);
    (0.65 + 0.9 * d1 + 0.3 * d2).min(1.0)
}

/// Paint one bar's camera-facing faces as shaded filled quads. Sides are drawn
/// first, the cap last (so it reads on top within the bar).
fn paint_bar(
    window: &mut Window,
    bar: &Bar,
    eye: [f32; 3],
    to_screen: &impl Fn([f32; 3]) -> Option<gpui::Point<Pixels>>,
) {
    let x0 = bar.x - 0.45;
    let x1 = bar.x + 0.45;
    let z0 = bar.z - 0.45;
    let z1 = bar.z + 0.45;
    let y_lo = bar.height.min(0.0);
    let y_hi = bar.height.max(0.0);

    // (normal, 4 corners). Caps last so they paint over the sides.
    let faces: [([f32; 3], [[f32; 3]; 4]); 6] = [
        ([1.0, 0.0, 0.0], [[x1, y_lo, z0], [x1, y_hi, z0], [x1, y_hi, z1], [x1, y_lo, z1]]),
        ([-1.0, 0.0, 0.0], [[x0, y_lo, z0], [x0, y_lo, z1], [x0, y_hi, z1], [x0, y_hi, z0]]),
        ([0.0, 0.0, 1.0], [[x0, y_lo, z1], [x1, y_lo, z1], [x1, y_hi, z1], [x0, y_hi, z1]]),
        ([0.0, 0.0, -1.0], [[x0, y_lo, z0], [x0, y_hi, z0], [x1, y_hi, z0], [x1, y_lo, z0]]),
        ([0.0, 1.0, 0.0], [[x0, y_hi, z0], [x0, y_hi, z1], [x1, y_hi, z1], [x1, y_hi, z0]]),
        ([0.0, -1.0, 0.0], [[x0, y_lo, z0], [x1, y_lo, z0], [x1, y_lo, z1], [x0, y_lo, z1]]),
    ];

    for (n, corners) in faces {
        // Visibility: face is camera-facing when its outward normal points
        // toward the eye.
        let center = [
            (corners[0][0] + corners[2][0]) * 0.5,
            (corners[0][1] + corners[2][1]) * 0.5,
            (corners[0][2] + corners[2][2]) * 0.5,
        ];
        let dir = [eye[0] - center[0], eye[1] - center[1], eye[2] - center[2]];
        if n[0] * dir[0] + n[1] * dir[1] + n[2] * dir[2] <= 0.0 {
            continue;
        }
        // Project the 4 corners; skip if any is culled.
        let mut pts = [point(px(0.), px(0.)); 4];
        let mut ok = true;
        for (j, c) in corners.iter().enumerate() {
            match to_screen(*c) {
                Some(p) => pts[j] = p,
                None => {
                    ok = false;
                    break;
                }
            }
        }
        if !ok {
            continue;
        }
        let shade = face_light(n);
        let color = Rgba {
            r: (bar.r * shade).clamp(0.0, 1.0),
            g: (bar.g * shade).clamp(0.0, 1.0),
            b: (bar.b * shade).clamp(0.0, 1.0),
            a: 1.0,
        };
        let mut pb = PathBuilder::fill();
        pb.move_to(pts[0]);
        pb.line_to(pts[1]);
        pb.line_to(pts[2]);
        pb.line_to(pts[3]);
        pb.close();
        if let Ok(path) = pb.build() {
            window.paint_path(path, color);
        }
    }
}

// ── Projected axis labels (source tensor indices + value scale) ─────────────────

fn label_placements(g: &BarGrid, camera: &OrbitCamera, w: f32, h: f32) -> Vec<(String, f32, f32)> {
    let proj = Mat4::perspective(camera.fov_y, w / h, 0.1, 4000.0);
    let view = camera.view_matrix();
    let mvp = proj.mul(&view);

    let rows = g.rows as f32;
    let cols = g.cols as f32;
    let src_r = g.src_rows.max(1) as f32;
    let src_c = g.src_cols.max(1) as f32;
    let size_lbl = (g.rows.max(g.cols) as f32 * 0.055).max(2.0);
    let margin = (g.rows.max(g.cols) as f32 * 0.07).max(2.5);

    let mut anchors: Vec<(String, [f32; 3])> = Vec::new();
    const TICKS: i32 = 4;
    for t in 0..=TICKS {
        let f = t as f32 / TICKS as f32;
        // Column indices along the front (X) edge.
        anchors.push((
            format!("{}", (f * (src_c - 1.0)).round() as i64),
            [f * (cols - 1.0) - cols / 2.0, 0.0, rows / 2.0 + margin],
        ));
        // Row indices along the left (Z) edge.
        anchors.push((
            format!("{}", (f * (src_r - 1.0)).round() as i64),
            [-cols / 2.0 - margin, 0.0, f * (rows - 1.0) - rows / 2.0],
        ));
    }
    anchors.push(("col".to_string(), [0.0, 0.0, rows / 2.0 + margin * 2.4]));
    anchors.push(("row".to_string(), [-cols / 2.0 - margin * 2.4, 0.0, 0.0]));

    // Value axis labels.
    let cx = -cols / 2.0 - margin;
    let cz = -rows / 2.0 - margin;
    anchors.push(("0".to_string(), [cx, 0.0, cz]));
    let mag = if g.max_abs > 0.0 { fmt_axis(g.max_abs) } else { "0".to_string() };
    anchors.push((format!("+{mag}"), [cx, HEIGHT_SCALE + size_lbl * 0.9, cz]));
    anchors.push((format!("−{mag}"), [cx, -HEIGHT_SCALE - size_lbl * 0.9, cz]));

    let mut out = Vec::new();
    for (text, p) in anchors {
        if let Some(pr) = math::project(&mvp, p, w, h) {
            if pr.x >= -40.0 && pr.x <= w + 40.0 && pr.y >= -20.0 && pr.y <= h + 20.0 {
                out.push((text, pr.x, pr.y));
            }
        }
    }
    out
}
