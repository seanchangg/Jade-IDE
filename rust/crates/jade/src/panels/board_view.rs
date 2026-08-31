//! The board panel (§B3): a 1:1 vector rendering of the MAX 10 FPGA
//! Development Kit (DK-DEV-10M50-A) that the simulation drives live.
//!
//! The drawer replaces the runtime sidebar in hardware mode. Everything is
//! drawn from a normalized coordinate table (fractions of the real ~5.7"×3.6"
//! top view) scaled by the current drawer width, so the board keeps 1:1
//! proportions at any size. Per the fidelity request the parts are layered:
//! gradient fills, borders, inset/outset shadows, LED bloom via stacked box
//! shadows (the pinned gpui has no radial gradient).

use gpui::{
    div, hsla, linear_color_stop, linear_gradient, prelude::*, px, rgb, AnyElement, BoxShadow,
    Context, FocusHandle, KeyDownEvent, KeyUpEvent, MouseButton,
};

use crate::app::{AppMode, JadeApp};
use crate::kumo::{self, scale, Badge, BadgeVariant, Card};
use crate::theme::Theme;

/// Real board proportions: ~5.7" x 3.6" top view.
const BOARD_ASPECT: f32 = 3.6 / 5.7;

/// PCB colors (solder-mask green family).
const PCB_FILL: u32 = 0x1E6B3C;
const PCB_FILL_DARK: u32 = 0x17552F;
const PCB_EDGE: u32 = 0x14522D;
const SILK: u32 = 0xE8EDE9;
const LED_OFF: u32 = 0x1E3325;
const LED_ON: u32 = 0x7CFF9B;
const DIP_BODY: u32 = 0xB3382D;
const DIP_BODY_DARK: u32 = 0x8E2C23;
const DIP_KNOB: u32 = 0xEFE6D0;

/// Mix two `0xRRGGBB` colors: `t = 0` gives `a`, `t = 1` gives `b`.
pub fn mix_rgb(a: u32, b: u32, t: f32) -> u32 {
    let t = t.clamp(0.0, 1.0);
    let ch = |sa: u32, sb: u32| -> u32 {
        let va = ((a >> sa) & 0xFF) as f32;
        let vb = ((b >> sb) & 0xFF) as f32;
        (va + (vb - va) * t) as u32
    };
    (ch(16, 16) << 16) | (ch(8, 8) << 8) | ch(0, 0)
}

/// The whole drawer: status readout over the board, with the left-edge
/// resize handle. Rendered only in hardware mode.
pub fn panel(app: &JadeApp, cx: &mut Context<JadeApp>, theme: &Theme) -> AnyElement {
    if app.mode != AppMode::Hardware || !app.board_visible {
        return div().into_any_element();
    }
    let Some(focus) = app.hw_focus.clone() else {
        return div().into_any_element();
    };
    let w = app.board_width;

    let card = Card::new(&theme.kumo)
        .id("board-panel")
        .flex()
        .flex_none()
        .flex_col()
        .gap(scale::SPACE_3)
        .w(px(w))
        .h_full()
        .min_h(px(0.))
        .p(scale::SPACE_3)
        .bg(theme.kumo.elevated)
        .overflow_y_scroll()
        .child(status_readout(app, cx, theme))
        .child(board(app, &focus, cx, theme, w - 24.0))
        .child(keyboard_legend(app, theme));

    div()
        .debug_selector(|| "board-panel".into())
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
        .id("board-resize")
        .w(px(6.))
        .h_full()
        .flex_none()
        .cursor(gpui::CursorStyle::ResizeLeftRight)
        .hover(|s| s.bg(theme.kumo.tint))
        .on_mouse_down(
            MouseButton::Left,
            cx.listener(|app: &mut JadeApp, ev: &gpui::MouseDownEvent, _w, cx| {
                app.board_resize = Some((f32::from(ev.position.x), app.board_width));
                cx.notify();
            }),
        )
}

/// The key legend under the board. A fixed key column keeps every
/// description on one left edge; each key cap is a fixed-size chip so `Q`
/// and `space` weigh the same.
fn keyboard_legend(app: &JadeApp, theme: &Theme) -> impl IntoElement {
    let t = &theme.kumo;
    let hw = app.hw.clone().unwrap_or_default();
    let key = |k: &str| {
        div()
            .min_w(px(22.))
            .h(px(18.))
            .px(px(5.))
            .flex()
            .items_center()
            .justify_center()
            .rounded(px(4.))
            .border_1()
            .border_color(t.hairline)
            .bg(t.base)
            .font_family(crate::fonts::mono_family())
            .child(k.to_string())
    };
    // Key cluster column at a fixed width, so the labels align.
    let row = |cluster: gpui::Div, label: &str| {
        let label = label.to_string();
        div()
            .flex()
            .flex_row()
            .items_center()
            .gap(scale::SPACE_2)
            .child(
                div()
                    .w(px(100.))
                    .flex_none()
                    .flex()
                    .items_center()
                    .gap(px(4.))
                    .child(cluster),
            )
            .child(label)
    };
    let range = |a: &'static str, b: &'static str| {
        div()
            .flex()
            .items_center()
            .gap(px(4.))
            .child(key(a))
            .child(div().text_color(t.text_subtle).child("–"))
            .child(key(b))
    };
    let dips: String = hw
        .dip
        .iter()
        .map(|on| if *on { '▲' } else { '▽' })
        .collect();
    div()
        .flex()
        .flex_col()
        .gap(scale::SPACE_2)
        .text_size(scale::TEXT_XS)
        .text_color(t.text_subtle)
        .child(row(range("1", "4"), "hold USER_PB0-3"))
        .child(row(range("Q", "T"), "toggle USER_DIPSW0-4"))
        .child(row(
            div().flex().items_center().gap(px(4.)).child(key("space")).child(key(".")),
            "run / pause · step one clock",
        ))
        .child(
            div()
                .flex()
                .flex_row()
                .items_center()
                .gap(scale::SPACE_2)
                .mt(px(2.))
                .child(
                    div()
                        .w(px(100.))
                        .flex_none()
                        .font_family(crate::fonts::mono_family())
                        .child(format!("DIPSW {dips}")),
                )
                .child("click the board to give it the keys"),
        )
}

/// Sim time · achieved rate · transport. One steady row — the time column
/// is width-stable so nothing shifts as digits tick — with a second row
/// only when there is state worth reporting (compiling, missing tool).
fn status_readout(app: &JadeApp, cx: &mut Context<JadeApp>, theme: &Theme) -> impl IntoElement {
    use jade_hw::HwRunState;
    let t = &theme.kumo;
    let hw = app.hw.clone().unwrap_or_default();
    let running = hw.run_state == HwRunState::Running;

    let sim_secs = hw.sim_time_ns as f64 / 1e9;
    // Fixed-width time so the row does not breathe while the sim runs.
    let time_label = format!("t {sim_secs:>8.2} s");

    // Achieved-rate badge: success at ≥95% of the real 50 MHz, warning below.
    let mhz = hw.achieved_hz / 1e6;
    let rate_variant = if hw.achieved_hz >= 0.95 * 50e6 {
        BadgeVariant::Success
    } else {
        BadgeVariant::Warning
    };
    let rate_label = if hw.slow_mo {
        "1 Hz".to_string()
    } else if mhz >= 1.0 {
        format!("{mhz:.0} MHz")
    } else {
        format!("{:.0} kHz", hw.achieved_hz / 1e3)
    };

    // Clock-edge flash dot: exists only in slow motion, where an edge is an
    // event worth seeing.
    let now = app.now_ms();
    let edge_hot = hw.slow_mo && now.saturating_sub(hw.last_clk_edge_ms) < 160 && hw.clk_level;

    let mut readout = div().flex().flex_col().gap(scale::SPACE_2);

    let mut row = div()
        .flex()
        .flex_row()
        .items_center()
        .gap(scale::SPACE_2)
        .text_size(scale::TEXT_XS)
        .text_color(t.text_subtle)
        .child(
            div()
                .font_family(crate::fonts::mono_family())
                .flex_none()
                .child(time_label),
        )
        .child(Badge::new(rate_label).variant(rate_variant).tabular(true).render(t));
    if hw.slow_mo {
        row = row.child(
            div()
                .id("hw-clk-dot")
                .w(px(8.))
                .h(px(8.))
                .rounded_full()
                .bg(if edge_hot { rgb(LED_ON) } else { t.tint }),
        );
    }
    row = row
        .child(div().flex_1())
        .child(
            kumo::button::icon_button("hw-run", if running { "pause" } else { "play" }, running, t)
                .debug_selector(|| "hw-run".into())
                .on_click(cx.listener(|a: &mut JadeApp, _e, _w, cx| {
                    a.hw_toggle_run();
                    cx.notify();
                })),
        )
        .child(
            kumo::button::icon_button("hw-step", "skip-forward", false, t).on_click(cx.listener(
                |a: &mut JadeApp, _e, _w, cx| {
                    a.hw_step();
                    cx.notify();
                },
            )),
        )
        .child(
            kumo::button::icon_button("hw-slowmo", "timer", hw.slow_mo, t).on_click(cx.listener(
                |a: &mut JadeApp, _e, _w, cx| {
                    a.hw_toggle_slowmo();
                    cx.notify();
                },
            )),
        );
    readout = readout.child(row);

    if hw.compiling || hw.tool_missing.is_some() {
        let mut status = div().flex().flex_row().items_center().gap(scale::SPACE_2);
        if hw.compiling {
            status = status.child(
                Badge::new("compiling…")
                    .variant(BadgeVariant::Warning)
                    .render(t),
            );
        }
        if let Some(hint) = &hw.tool_missing {
            status = status.child(
                Badge::new(format!("verilator missing · {hint}"))
                    .variant(BadgeVariant::Error)
                    .render(t),
            );
        }
        readout = readout.child(status);
    }
    readout
}

/// The PCB itself. `w` is the drawn width in px; height keeps the 1:1 board
/// proportions. All child geometry is computed from `w` so the board scales.
fn board(
    app: &JadeApp,
    focus: &FocusHandle,
    cx: &mut Context<JadeApp>,
    theme: &Theme,
    w: f32,
) -> impl IntoElement {
    let hw = app.hw.clone().unwrap_or_default();
    let h = w * BOARD_ASPECT;
    let focus_for_click = focus.clone();

    let mut pcb = div()
        .id("board-pcb")
        .relative()
        .w(px(w))
        .h(px(h))
        .flex_none()
        .rounded(px(w * 0.02))
        .border_1()
        .border_color(rgb(PCB_EDGE))
        // Solder-mask sheen: a diagonal two-stop gradient (the pinned gpui
        // has exactly this) reads as depth without a photo asset.
        .bg(linear_gradient(
            135.,
            linear_color_stop(rgb(PCB_FILL), 0.0),
            linear_color_stop(rgb(PCB_FILL_DARK), 1.0),
        ))
        .shadow(kumo::shadow_md())
        .track_focus(focus)
        .on_mouse_down(
            MouseButton::Left,
            cx.listener(move |_a: &mut JadeApp, _e, window, cx| {
                focus_for_click.focus(window, cx);
                cx.notify();
            }),
        )
        // Board-scoped plain keys (§B7): 1-4 hold a push button, q-t toggle a
        // DIP switch, space runs/pauses, `.` steps. Only while the board has
        // focus, so the editor keeps every printable.
        .on_key_down(cx.listener(|a: &mut JadeApp, ev: &KeyDownEvent, _w, cx| {
            let key = ev.keystroke.key.as_str();
            match key {
                "1" | "2" | "3" | "4" => {
                    let i = (key.as_bytes()[0] - b'1') as usize;
                    // Key repeat re-fires key-down; pressing twice is a no-op
                    // because hw_set_pb ignores an unchanged state.
                    a.hw_set_pb(i, true);
                }
                "q" | "w" | "e" | "r" | "t" => {
                    if ev.is_held {
                        return; // a held key must not oscillate the switch
                    }
                    let i = match key {
                        "q" => 0,
                        "w" => 1,
                        "e" => 2,
                        "r" => 3,
                        _ => 4,
                    };
                    a.hw_toggle_dip(i);
                    a.schedule_ui_save(cx); // dipSwitches (§B8)
                }
                "space" => a.hw_toggle_run(),
                "." => a.hw_step(),
                _ => return,
            }
            cx.stop_propagation();
            cx.notify();
        }))
        .on_key_up(cx.listener(|a: &mut JadeApp, ev: &KeyUpEvent, _w, cx| {
            let key = ev.keystroke.key.as_str();
            if let "1" | "2" | "3" | "4" = key {
                let i = (key.as_bytes()[0] - b'1') as usize;
                a.hw_set_pb(i, false);
                cx.stop_propagation();
                cx.notify();
            }
        }));

    // Layout note: every part and label owns a clear zone — silkscreen text
    // never sits on a component, a trace, or another label.
    //
    //   top:    hole · title ─ GPIO header ─ hole
    //   left:   DDR3 chip / crystal / USB connector
    //   center: FPGA with pad rows
    //   right:  LED cluster (top) · regulator · PB cluster (bottom)
    //   bottom: DIP block + its label

    // ── Copper traces first, under everything (clear zones only) ──
    pcb = pcb.child(trace_h(w, 0.285, 0.075, 0.475)); // crystal → FPGA
    pcb = pcb.child(trace_h(w, 0.605, 0.10, 0.42)); // FPGA → regulator
    pcb = pcb.child(trace_v(w, 0.48, 0.755, 0.10)); // FPGA → PB cluster

    // ── Corner mounting holes ──
    for (x, y) in [(0.022, 0.035), (0.935, 0.035), (0.022, 0.90), (0.935, 0.90)] {
        pcb = pcb.child(mounting_hole(w, x, y));
    }

    // ── Edge connectors: GPIO header (top) and HSMC fingers (right edge) ──
    pcb = pcb.child(header_2x(w, 0.50, 0.028, 9));
    pcb = pcb.child(edge_fingers(w));
    // ── USB-Blaster II connector on the left edge ──
    pcb = pcb.child(usb_connector(w));

    // ── Board name silkscreen (top left, clear of the header) ──
    pcb = pcb.child(silk(w, 0.075, 0.055, "MAX 10 FPGA · DK-DEV-10M50-A".to_string()));

    // ── FPGA chip (center): black package, silkscreen, pin-1 dot ──
    pcb = pcb.child(fpga_chip(w));
    // ── Gold pad rows flanking the chip ──
    pcb = pcb.child(pad_row(w, 0.365, 0.245, 12));
    pcb = pcb.child(pad_row(w, 0.365, 0.685, 12));

    // ── SDRAM (left top) + power regulator (right middle) ──
    pcb = pcb.child(small_chip(w, 0.10, 0.19, 0.14, 0.075, "DDR3"));
    pcb = pcb.child(small_chip(w, 0.76, 0.42, 0.075, 0.055, "EN63"));

    // ── Passives, sprinkled only into open regions ──
    for (x, y, gold) in [
        (0.275, 0.145, false), (0.305, 0.145, false),
        (0.115, 0.345, true), (0.155, 0.345, true),
        (0.315, 0.56, true), (0.315, 0.61, true),
        (0.695, 0.44, false), (0.87, 0.44, false),
        (0.52, 0.90, true), (0.545, 0.90, false),
    ] {
        pcb = pcb.child(passive(w, x, y, gold));
    }
    // ── 50 MHz crystal (its face already reads "50.000 MHz") ──
    pcb = pcb.child(crystal(w));

    // ── USER_LED[4:0] cluster (top right): label above, numerals below ──
    pcb = pcb.child(silk_box(w, 0.625, 0.085, 0.32, 0.20));
    pcb = pcb.child(silk(w, 0.645, 0.10, "USER_LED".to_string()));
    for i in 0..5usize {
        let x = 0.645 + 0.060 * i as f32;
        // Center each numeral under its 0.036w-wide LED die.
        pcb = pcb.child(led(w, i, x, 0.16, hw.led_duty[i]));
        pcb = pcb.child(silk(w, x + 0.012, 0.215, format!("{i}")));
    }

    // ── USER_PB[3:0] cluster (bottom right): label above, numerals below ──
    pcb = pcb.child(silk_box(w, 0.575, 0.64, 0.375, 0.30));
    pcb = pcb.child(silk(w, 0.60, 0.655, "USER_PB".to_string()));
    for i in 0..4usize {
        let x = 0.60 + 0.085 * i as f32;
        pcb = pcb.child(push_button(cx, w, i, x, 0.72, hw.pb[i]));
        // Center each numeral under its 0.058w-wide cap.
        pcb = pcb.child(silk(w, x + 0.023, 0.885, format!("{i}")));
    }

    // ── USER_DIPSW[4:0] (bottom left), label below the block ──
    pcb = pcb.child(dip_block(cx, w, &hw.dip));
    pcb = pcb.child(silk(w, 0.12, 0.905, "USER_DIPSW · ON ↑".to_string()));

    let _ = theme;
    pcb
}

/// A thin horizontal copper trace (subtle PCB detail).
fn trace_h(w: f32, x: f32, len: f32, y: f32) -> impl IntoElement {
    div()
        .absolute()
        .left(px(w * x))
        .top(px(w * BOARD_ASPECT * y))
        .w(px(w * len))
        .h(px(1.))
        .bg(hsla(0.36, 0.45, 0.30, 0.85))
}

/// A thin vertical copper trace.
fn trace_v(w: f32, x: f32, y: f32, len: f32) -> impl IntoElement {
    div()
        .absolute()
        .left(px(w * x))
        .top(px(w * BOARD_ASPECT * y))
        .w(px(1.))
        .h(px(w * BOARD_ASPECT * len))
        .bg(hsla(0.36, 0.45, 0.30, 0.85))
}

/// A corner mounting hole: gold annulus around a dark drill.
fn mounting_hole(w: f32, x: f32, y: f32) -> impl IntoElement {
    let s = w * 0.040;
    div()
        .absolute()
        .left(px(w * x))
        .top(px(w * BOARD_ASPECT * y))
        .w(px(s))
        .h(px(s))
        .rounded_full()
        .bg(linear_gradient(
            135.,
            linear_color_stop(rgb(0xD9B95C), 0.0),
            linear_color_stop(rgb(0x9A7A36), 1.0),
        ))
        .flex()
        .items_center()
        .justify_center()
        .child(
            div()
                .w(px(s * 0.55))
                .h(px(s * 0.55))
                .rounded_full()
                .bg(rgb(0x101a12))
                .shadow(vec![BoxShadow {
                    color: hsla(0.0, 0.0, 0.0, 0.6),
                    offset: gpui::point(px(0.), px(1.)),
                    blur_radius: px(1.5),
                    spread_radius: px(0.),
                    inset: true,
                }]),
        )
}

/// A 2×N pin header along the top edge (the GPIO header).
fn header_2x(w: f32, x: f32, y: f32, n: usize) -> impl IntoElement {
    let mut rows = div()
        .absolute()
        .left(px(w * x))
        .top(px(w * BOARD_ASPECT * y))
        .flex()
        .flex_col()
        .gap(px(w * 0.004))
        .p(px(w * 0.004))
        .rounded(px(1.5))
        .bg(rgb(0x14171A));
    for _ in 0..2 {
        let mut row = div().flex().flex_row().gap(px(w * 0.006));
        for _ in 0..n {
            row = row.child(
                div()
                    .w(px(w * 0.008))
                    .h(px(w * 0.008))
                    .rounded(px(0.5))
                    .bg(linear_gradient(
                        135.,
                        linear_color_stop(rgb(0xE8CC7A), 0.0),
                        linear_color_stop(rgb(0xA8853C), 1.0),
                    )),
            );
        }
        rows = rows.child(row);
    }
    rows
}

/// Gold edge fingers on the right edge (the HSMC connector zone).
fn edge_fingers(w: f32) -> impl IntoElement {
    let h = w * BOARD_ASPECT;
    let mut col = div()
        .absolute()
        .left(px(w * 0.966))
        .top(px(h * 0.28))
        .flex()
        .flex_col()
        .gap(px(w * 0.006));
    for _ in 0..12 {
        col = col.child(
            div()
                .w(px(w * 0.026))
                .h(px(w * 0.008))
                .rounded_l(px(1.))
                .bg(linear_gradient(
                    90.,
                    linear_color_stop(rgb(0xD9B95C), 0.0),
                    linear_color_stop(rgb(0xB8934A), 1.0),
                )),
        );
    }
    col
}

/// The USB-Blaster II connector poking off the left edge.
fn usb_connector(w: f32) -> impl IntoElement {
    let h = w * BOARD_ASPECT;
    div()
        .absolute()
        .left(px(-w * 0.008))
        .top(px(h * 0.62))
        .w(px(w * 0.075))
        .h(px(h * 0.14))
        .rounded_r(px(2.))
        .bg(linear_gradient(
            180.,
            linear_color_stop(rgb(0xB9BFC2), 0.0),
            linear_color_stop(rgb(0x83898C), 1.0),
        ))
        .border_1()
        .border_color(rgb(0x5F6568))
        .shadow(kumo::shadow_sm())
        .flex()
        .items_center()
        .justify_center()
        .child(
            div()
                .w(px(w * 0.045))
                .h(px(h * 0.055))
                .rounded(px(1.))
                .bg(rgb(0x2A2E30)),
        )
}

/// A generic dark IC package with a tiny part mark.
fn small_chip(w: f32, x: f32, y: f32, cw: f32, ch: f32, mark: &str) -> impl IntoElement {
    let h = w * BOARD_ASPECT;
    div()
        .absolute()
        .left(px(w * x))
        .top(px(h * y))
        .w(px(w * cw))
        .h(px(w * ch))
        .rounded(px(2.))
        .bg(linear_gradient(
            135.,
            linear_color_stop(rgb(0x26292D), 0.0),
            linear_color_stop(rgb(0x15171A), 1.0),
        ))
        .border_1()
        .border_color(rgb(0x0B0C0E))
        .shadow(kumo::shadow_sm())
        .flex()
        .items_center()
        .justify_center()
        .text_size(px((w * 0.016).max(6.0)))
        .font_family(crate::fonts::mono_family())
        .text_color(rgb(0x767D85))
        .child(mark.to_string())
}

/// One SMD passive: a tan capacitor or a gold-ended resistor.
fn passive(w: f32, x: f32, y: f32, gold_ends: bool) -> impl IntoElement {
    let h = w * BOARD_ASPECT;
    let body = if gold_ends { 0xB08D5A } else { 0xC7B08A };
    div()
        .absolute()
        .left(px(w * x))
        .top(px(h * y))
        .w(px(w * 0.018))
        .h(px(w * 0.009))
        .rounded(px(1.))
        .bg(linear_gradient(
            180.,
            linear_color_stop(rgb(mix_rgb(body, 0xFFFFFF, 0.25)), 0.0),
            linear_color_stop(rgb(body), 1.0),
        ))
        .border_color(rgb(mix_rgb(body, 0x000000, 0.35)))
        .border_1()
}

/// A thin silkscreen outline box grouping one control cluster.
fn silk_box(w: f32, x: f32, y: f32, bw: f32, bh: f32) -> impl IntoElement {
    let h = w * BOARD_ASPECT;
    div()
        .absolute()
        .left(px(w * x))
        .top(px(h * y))
        .w(px(w * bw))
        .h(px(h * bh))
        .rounded(px(2.))
        .border_1()
        .border_color(hsla(0.33, 0.15, 0.88, 0.35))
}

/// One LED: a rectangular die whose fill and bloom follow the duty (PWM
/// brightness). The glow is two stacked colored box shadows.
fn led(w: f32, i: usize, x: f32, y: f32, duty: f32) -> impl IntoElement {
    let d = duty.clamp(0.0, 1.0);
    let fill = mix_rgb(LED_OFF, LED_ON, d);
    let mut el = div()
        .id(("led", i))
        .absolute()
        .left(px(w * x))
        .top(px(w * BOARD_ASPECT * y))
        .w(px(w * 0.036))
        .h(px(w * 0.018))
        .rounded(px(2.))
        .bg(rgb(fill))
        .border_1()
        .border_color(rgb(mix_rgb(0x0E1F16, 0x9BFFB8, d * 0.6)));
    if d > 0.02 {
        el = el.shadow(vec![
            BoxShadow {
                color: hsla(0.38, 0.9, 0.6, 0.55 * d),
                offset: gpui::point(px(0.), px(0.)),
                blur_radius: px(5.0 + 9.0 * d),
                spread_radius: px(1.0 + 2.0 * d),
                inset: false,
            },
            BoxShadow {
                color: hsla(0.38, 0.9, 0.75, 0.35 * d),
                offset: gpui::point(px(0.), px(0.)),
                blur_radius: px(2.),
                spread_radius: px(0.),
                inset: false,
            },
        ]);
    }
    el
}

/// One push button: a silver washer with a dimensional cap. Held = darker
/// cap + inset look; press with the pointer (hold) or keys 1-4.
fn push_button(
    cx: &mut Context<JadeApp>,
    w: f32,
    i: usize,
    x: f32,
    y: f32,
    pressed: bool,
) -> impl IntoElement {
    let s = w * 0.058; // washer diameter
    let cap = s * 0.62;
    let cap_fill = if pressed {
        linear_gradient(
            180.,
            linear_color_stop(rgb(0x2A2D2B), 0.0),
            linear_color_stop(rgb(0x3A3E3B), 1.0),
        )
    } else {
        linear_gradient(
            180.,
            linear_color_stop(rgb(0x565B57), 0.0),
            linear_color_stop(rgb(0x2E322F), 1.0),
        )
    };
    let cap_shadow = if pressed {
        vec![BoxShadow {
            color: hsla(0.0, 0.0, 0.0, 0.6),
            offset: gpui::point(px(0.), px(1.)),
            blur_radius: px(2.),
            spread_radius: px(0.),
            inset: true,
        }]
    } else {
        vec![BoxShadow {
            color: hsla(0.0, 0.0, 0.0, 0.45),
            offset: gpui::point(px(0.), px(2.)),
            blur_radius: px(3.),
            spread_radius: px(0.),
            inset: false,
        }]
    };
    div()
        .id(("pb", i))
        .debug_selector(move || format!("pb-{i}"))
        .absolute()
        .left(px(w * x))
        .top(px(w * BOARD_ASPECT * y))
        .w(px(s))
        .h(px(s))
        .rounded_full()
        // The metal washer: light top edge, dark bottom edge.
        .bg(linear_gradient(
            180.,
            linear_color_stop(rgb(0xC9CFCA), 0.0),
            linear_color_stop(rgb(0x7E847F), 1.0),
        ))
        .border_1()
        .border_color(rgb(0x5A5F5B))
        .flex()
        .items_center()
        .justify_center()
        .cursor_pointer()
        .on_mouse_down(
            MouseButton::Left,
            cx.listener(move |a: &mut JadeApp, _e, _w, cx| {
                a.hw_set_pb(i, true);
                cx.notify();
            }),
        )
        .on_mouse_up(
            MouseButton::Left,
            cx.listener(move |a: &mut JadeApp, _e, _w, cx| {
                a.hw_set_pb(i, false);
                cx.notify();
            }),
        )
        .child(
            div()
                .w(px(cap))
                .h(px(cap))
                .rounded_full()
                .bg(cap_fill)
                .shadow(cap_shadow)
                // A highlight strip on the cap's upper edge sells the dome.
                .child(
                    div()
                        .mt(px(cap * 0.12))
                        .ml(px(cap * 0.22))
                        .w(px(cap * 0.55))
                        .h(px(cap * 0.16))
                        .rounded_full()
                        .bg(hsla(0.0, 0.0, 1.0, if pressed { 0.10 } else { 0.22 })),
                ),
        )
}

/// The 5-position DIP switch block: red body, cream knobs that sit at the
/// top (ON) or the bottom (OFF). Click a knob track to toggle.
fn dip_block(cx: &mut Context<JadeApp>, w: f32, dip: &[bool; 5]) -> impl IntoElement {
    let h = w * BOARD_ASPECT;
    let sw_w = w * 0.030;
    let sw_h = w * 0.064;
    let mut block = div()
        .absolute()
        .left(px(w * 0.12))
        .top(px(h * 0.68))
        .flex()
        .flex_row()
        .items_center()
        .gap(px(w * 0.012))
        .p(px(w * 0.010))
        .rounded(px(3.))
        .bg(linear_gradient(
            180.,
            linear_color_stop(rgb(DIP_BODY), 0.0),
            linear_color_stop(rgb(DIP_BODY_DARK), 1.0),
        ))
        .border_1()
        .border_color(rgb(0x6E211A))
        .shadow(kumo::shadow_sm());
    for (i, on) in dip.iter().enumerate() {
        let on = *on;
        block = block.child(
            div()
                .id(("dip", i))
                .debug_selector(move || format!("dip-{i}"))
                .w(px(sw_w))
                .h(px(sw_h))
                .rounded(px(2.))
                // The recessed track the knob slides in.
                .bg(rgb(0x611C15))
                .border_1()
                .border_color(rgb(0x4A140F))
                .flex()
                .flex_col()
                .when(on, |d| d.justify_start())
                .when(!on, |d| d.justify_end())
                .p(px(1.5))
                .cursor_pointer()
                .on_click(cx.listener(move |a: &mut JadeApp, _e, _w, cx| {
                    a.hw_toggle_dip(i);
                    a.schedule_ui_save(cx); // dipSwitches (§B8)
                    cx.notify();
                }))
                .child(
                    div()
                        .w_full()
                        .h(px(sw_h * 0.42))
                        .rounded(px(1.5))
                        .bg(linear_gradient(
                            180.,
                            linear_color_stop(rgb(DIP_KNOB), 0.0),
                            linear_color_stop(rgb(0xCBBFa4), 1.0),
                        ))
                        .shadow(vec![BoxShadow {
                            color: hsla(0.0, 0.0, 0.0, 0.4),
                            offset: gpui::point(px(0.), px(1.)),
                            blur_radius: px(1.5),
                            spread_radius: px(0.),
                            inset: false,
                        }]),
                ),
        );
    }
    block
}

/// The FPGA package: black epoxy square, silkscreen part marks, pin-1 dot.
fn fpga_chip(w: f32) -> impl IntoElement {
    let h = w * BOARD_ASPECT;
    let s = w * 0.24;
    div()
        .absolute()
        .left(px(w * 0.36))
        .top(px(h * 0.28))
        .w(px(s))
        .h(px(s))
        .rounded(px(3.))
        .bg(linear_gradient(
            135.,
            linear_color_stop(rgb(0x23262A), 0.0),
            linear_color_stop(rgb(0x121417), 1.0),
        ))
        .border_1()
        .border_color(rgb(0x0B0C0E))
        .shadow(kumo::shadow_sm())
        .flex()
        .flex_col()
        .items_center()
        .justify_center()
        .gap(px(2.))
        .text_size(px((w * 0.020).max(8.0)))
        .font_family(crate::fonts::mono_family())
        .text_color(rgb(0x8B929B))
        .child("intel MAX 10")
        .child("10M50DA")
        .child("F484C6GES")
        // Pin-1 index dot.
        .child(
            div()
                .absolute()
                .left(px(s * 0.08))
                .top(px(s * 0.08))
                .w(px(s * 0.06))
                .h(px(s * 0.06))
                .rounded_full()
                .bg(rgb(0x3A3E44)),
        )
}

/// The 50 MHz can oscillator.
fn crystal(w: f32) -> impl IntoElement {
    let h = w * BOARD_ASPECT;
    div()
        .absolute()
        .left(px(w * 0.17))
        .top(px(h * 0.40))
        .w(px(w * 0.11))
        .h(px(w * 0.055))
        .rounded(px(w * 0.024))
        .bg(linear_gradient(
            180.,
            linear_color_stop(rgb(0xC4C9CC), 0.0),
            linear_color_stop(rgb(0x8A9094), 1.0),
        ))
        .border_1()
        .border_color(rgb(0x6C7276))
        .shadow(kumo::shadow_sm())
        .flex()
        .items_center()
        .justify_center()
        .text_size(px((w * 0.018).max(7.0)))
        .font_family(crate::fonts::mono_family())
        .text_color(rgb(0x3E4448))
        .child("50.000 MHz")
}

/// A row of gold pads (decorative PCB detail near the chip).
fn pad_row(w: f32, x: f32, y: f32, n: usize) -> impl IntoElement {
    let h = w * BOARD_ASPECT;
    let mut row = div()
        .absolute()
        .left(px(w * x))
        .top(px(h * y))
        .flex()
        .flex_row()
        .gap(px(w * 0.006));
    for _ in 0..n {
        row = row.child(
            div()
                .w(px(w * 0.008))
                .h(px(w * 0.012))
                .rounded(px(0.5))
                .bg(linear_gradient(
                    180.,
                    linear_color_stop(rgb(0xD9B95C), 0.0),
                    linear_color_stop(rgb(0xA8853C), 1.0),
                )),
        );
    }
    row
}

/// One silkscreen label, positioned in board fractions.
fn silk(w: f32, x: f32, y: f32, text: String) -> impl IntoElement {
    div()
        .absolute()
        .left(px(w * x))
        .top(px(w * BOARD_ASPECT * y))
        .text_size(px((w * 0.019).max(7.0)))
        .font_family(crate::fonts::mono_family())
        .text_color(rgb(SILK))
        .child(text)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mix_rgb_endpoints_and_midpoint() {
        assert_eq!(mix_rgb(LED_OFF, LED_ON, 0.0), LED_OFF);
        assert_eq!(mix_rgb(LED_OFF, LED_ON, 1.0), LED_ON);
        let mid = mix_rgb(0x000000, 0xFFFFFF, 0.5);
        assert_eq!(mid, 0x7F7F7F);
    }
}
