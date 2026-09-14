//! TRAINING view (feature inventory §7.1): Loss / Memory / Kernel-time charts,
//! timing breakdown bars, and tensor heatmap previews. Charts are GPUI
//! `canvas` elements painted with `PathBuilder` polylines; stored runs from
//! the RUNS list overlay in per-run colors (this replaced the automatic
//! ghost-previous-run underlay); the heatmap keeps the working diverging
//! colormap from the spike.
//!
//! Rendering is a pure projection of `JadeApp` state — data prep happens here,
//! owned snapshots move into the paint closures (canvas closures are `'static`).

use gpui::{
    canvas, div, fill, point, prelude::*, px, rgb, size, Bounds, Context, PathBuilder, Pixels,
    Rgba, Window,
};

use jade_telemetry::Kind;

use crate::app::JadeApp;
use crate::format::{fmt_val, format_avg_ms, format_bytes, is_memory_name};
use crate::panels::metric_popout::MetricSection;
use crate::theme::Theme;

const CHART_H: f32 = 120.0;
/// Height of one per-pipeline kernel mini-chart (several stack in the section).
const KERNEL_CHART_H: f32 = 56.0;

/// One plotted polyline: absolute Y range is `[min, max]`, X is index-based.
/// (`pub(crate)` so the pop-out windows can reuse the chart pipeline.)
pub(crate) struct Series {
    color: Rgba,
    width: f32,
    fill: bool,
    values: Vec<f32>,
    min: f32,
    max: f32,
}

pub(crate) struct Label {
    text: String,
    color: u32,
}

pub fn render(app: &JadeApp, cx: &mut Context<JadeApp>) -> impl IntoElement {
    let theme = &app.theme;
    let grid = rgb(theme.grid_line).alpha(theme.grid_alpha);

    let mut col = div()
        .flex()
        .flex_col()
        .gap_2()
        .child(training_header(app, theme, cx));

    // ── Runs (persistent history; hidden until something is stored) ──
    if let Some(runs) = runs_section(app, theme, cx) {
        col = col.child(runs);
    }

    // ── Loss ──
    let (loss_series, loss_labels) = loss_data(app);
    col = col.child(chart_section(
        "Loss",
        MetricSection::Loss,
        theme,
        loss_series,
        loss_labels,
        grid,
        cx,
    ));

    // ── Memory ──
    let (mem_series, mem_labels) = memory_data(app);
    col = col.child(chart_section(
        "Memory",
        MetricSection::Memory,
        theme,
        mem_series,
        mem_labels,
        grid,
        cx,
    ));

    // ── Kernel time (auto-hidden until plottable): one mini-chart per
    // enabled timer, each on its own scale — pipelines show independently. ──
    if let Some(kcharts) = kernel_charts(app, theme, grid, cx) {
        col = col.child(kcharts);
    }

    // ── Timing breakdown ──
    col = col.child(timing_breakdown(app, theme, cx));

    // ── Tensors (auto-hidden until a buffer is enabled with frames) ──
    if let Some(tensors) = tensor_previews(app, theme, cx) {
        col = col.child(tensors);
    }

    col
}

// ── Section scaffolding ──────────────────────────────────────────────────────

fn section_header(text: &str, theme: &Theme) -> impl IntoElement {
    div()
        .text_color(rgb(theme.muted))
        .text_size(px(10.))
        .font_weight(gpui::FontWeight::MEDIUM)
        .child(text.to_string())
}

/// TRAINING header with the "3D" button (§7.1) that opens the weight-grid
/// overlay (§7.2). The grid's ring already filled while hidden, so it shows the
/// selected buffer's replay immediately.
fn training_header(app: &JadeApp, theme: &Theme, cx: &mut Context<JadeApp>) -> impl IntoElement {
    let has_buffers = !app.wg3d.buffer_names().is_empty();
    div()
        .flex()
        .flex_row()
        .items_center()
        .justify_between()
        .child(
            div()
                .flex()
                .flex_row()
                .items_center()
                .gap_1()
                .text_color(rgb(theme.muted))
                .text_xs()
                .font_weight(gpui::FontWeight::SEMIBOLD)
                .child(crate::assets::ui_icon("box", 13., theme.muted))
                .child("TRAINING"),
        )
        .child(
            div()
                .id("wg3d-open")
                .h(px(20.))
                .px_2()
                .flex()
                .items_center()
                .text_xs()
                .font_weight(gpui::FontWeight::MEDIUM)
                .border_1()
                .border_color(theme.kumo.line)
                .text_color(rgb(if has_buffers { theme.accent } else { theme.muted }))
                .hover(|s| s.bg(theme.kumo.tint))
                .cursor_pointer()
                .on_click(cx.listener(|app, _e, _w, cx| {
                    app.wg3d.open(None);
                    cx.notify();
                }))
                .child("3D"),
        )
}

fn section_label(text: &str, theme: &Theme) -> impl IntoElement {
    div()
        .text_color(rgb(theme.muted))
        .text_size(px(10.))
        .font_weight(gpui::FontWeight::MEDIUM)
        .child(text.to_string())
}

// ── RUNS section (persistent run history + overlay toggles) ─────────────────

/// Chart color for the `i`-th overlaid run. Cycles the series palette in
/// reverse so an overlay rarely lands on the same hue as the live series that
/// share its chart (both palettes collide only past 5 entries).
pub fn overlay_color(theme: &Theme, i: usize) -> u32 {
    let s = theme.series;
    s[s.len() - 1 - (i % s.len())]
}

/// Display label for a stored run id ("#7 a.out"-style, dbg-tagged).
fn run_title(meta: &crate::run_store::RunMeta) -> String {
    if meta.kind == crate::run_store::KIND_DEBUG {
        format!("{} (dbg)", meta.label)
    } else {
        meta.label.clone()
    }
}

fn fmt_duration_ms(ms: i64) -> String {
    if ms >= 60_000 {
        format!("{}m{}s", ms / 60_000, (ms % 60_000) / 1000)
    } else if ms >= 1000 {
        format!("{:.1}s", ms as f64 / 1000.0)
    } else {
        format!("{ms}ms")
    }
}

#[cfg(test)]
mod run_section_tests {
    use super::*;
    use crate::theme::Theme;

    #[test]
    fn durations_format_compactly() {
        assert_eq!(fmt_duration_ms(850), "850ms");
        assert_eq!(fmt_duration_ms(12_340), "12.3s");
        assert_eq!(fmt_duration_ms(83_000), "1m23s");
    }

    #[test]
    fn overlay_colors_cycle_reversed() {
        let t = Theme::jade_dark();
        assert_eq!(overlay_color(&t, 0), t.series[4]);
        assert_eq!(overlay_color(&t, 4), t.series[0]);
        assert_eq!(overlay_color(&t, 5), t.series[4]); // wraps
    }
}

/// The stored-run list: click a row to overlay it on the Loss/Memory charts
/// (chip shows the overlay color), ✕ deletes it from the DB. `None` while the
/// store is empty/unavailable so the section only appears once there's history.
fn runs_section(
    app: &JadeApp,
    theme: &Theme,
    cx: &mut Context<JadeApp>,
) -> Option<impl IntoElement> {
    const SHOWN: usize = 12;
    if app.stored_runs.is_empty() {
        return None;
    }
    let now = crate::run_store::now_epoch();

    let mut list = div().flex().flex_col();
    for meta in app.stored_runs.iter().take(SHOWN) {
        let id = meta.id;
        let overlay_pos = app.run_overlays.iter().position(|(rid, _)| *rid == id);
        let chip = match overlay_pos {
            Some(i) => rgb(overlay_color(theme, i)).into(),
            None => rgb(theme.muted).alpha(0.35),
        };
        // Failed runs read red; unfinished/unknown exits stay muted.
        let id_color = match meta.exit_code {
            Some(0) => theme.muted,
            Some(_) => theme.red,
            None => theme.muted,
        };

        let mut row = div()
            .id(("run-row", id as u64))
            .flex()
            .flex_row()
            .items_center()
            .gap_1()
            .px_1()
            
            .text_xs()
            .cursor_pointer()
            .hover(|s| s.bg(rgb(theme.bg)))
            .on_click(cx.listener(move |app, _e, _w, cx| {
                app.toggle_run_overlay(id);
                cx.notify();
            }))
            .child(div().w(px(7.)).h(px(7.)).rounded_full().flex_none().bg(chip))
            .child(div().text_color(rgb(id_color)).child(format!("#{id}")))
            .child(
                div()
                    .flex_1()
                    .overflow_hidden()
                    .text_color(rgb(theme.text))
                    .child(run_title(meta)),
            );
        if let Some(ms) = meta.duration_ms {
            row = row.child(div().text_color(rgb(theme.muted)).child(fmt_duration_ms(ms)));
        }
        row = row
            .child(
                div()
                    .text_color(rgb(theme.muted))
                    .child(crate::run_store::age_string(meta.started_epoch, now)),
            )
            .child(
                div()
                    .id(("run-del", id as u64))
                    .px_1()
                    
                    .text_color(rgb(theme.muted))
                    .hover(|s| s.text_color(rgb(theme.red)))
                    .on_click(cx.listener(move |app, _e, _w, cx| {
                        cx.stop_propagation(); // don't also toggle the overlay
                        app.delete_stored_run(id);
                        cx.notify();
                    }))
                    .child("✕"),
            );
        list = list.child(row);
    }
    if app.stored_runs.len() > SHOWN {
        list = list.child(
            div()
                .px_1()
                .text_xs()
                .text_color(rgb(theme.muted))
                .child(format!("+{} more", app.stored_runs.len() - SHOWN)),
        );
    }

    Some(
        div()
            .flex()
            .flex_col()
            .gap_1()
            .child(section_label("Runs", theme))
            .child(list),
    )
}

/// A section label row with the pop-out button ("open in its own window for
/// recording"): external-link glyph, muted until hovered. Clicking opens (or
/// re-focuses) the section's [`MetricPopout`] window.
fn section_label_row(
    title: &str,
    section: MetricSection,
    theme: &Theme,
    cx: &mut Context<JadeApp>,
) -> impl IntoElement {
    div()
        .flex()
        .flex_row()
        .items_center()
        .justify_between()
        .child(section_label(title, theme))
        .child(
            div()
                .id(("metric-pop", section as u64))
                .px_1()
                .cursor_pointer()
                .opacity(0.6)
                .hover(|s| s.opacity(1.0))
                .on_click(cx.listener(move |app: &mut JadeApp, _e, _w, cx| {
                    app.open_metric_popout(section, cx);
                }))
                .child(crate::assets::ui_icon("external-link", 11., theme.muted)),
        )
}

fn chart_section(
    title: &str,
    section: MetricSection,
    theme: &Theme,
    series: Vec<Series>,
    labels: Vec<Label>,
    grid: Rgba,
    cx: &mut Context<JadeApp>,
) -> impl IntoElement {
    let col = div()
        .flex()
        .flex_col()
        .gap_1()
        .child(section_label_row(title, section, theme, cx));
    // Nothing to plot: one quiet line instead of a 120px empty box, so an
    // app that emits no scalars does not fill the sidebar with blank charts.
    if series.is_empty() {
        let note = match section {
            MetricSection::Loss => "no scalars yet",
            MetricSection::Memory => "no memory samples yet",
            _ => "nothing to plot yet",
        };
        return col.child(
            div()
                .text_size(px(10.))
                .text_color(rgb(theme.muted))
                .child(note),
        );
    }
    col.child(chart_box(theme, series, labels, grid, CHART_H))
}

/// One chart body (no section label): the canvas in a rounded box with the
/// per-series labels overlaid top-left. `height: Some(px)` for the sidebar's
/// fixed cards; `None` flex-fills the parent (pop-out windows), with bigger
/// overlay labels to match the bigger canvas.
fn chart_box_sized(
    theme: &Theme,
    series: Vec<Series>,
    labels: Vec<Label>,
    grid: Rgba,
    height: Option<f32>,
) -> impl IntoElement {
    let label_px = if height.is_some() { 10. } else { 13. };
    // The readouts sit top-right, where a falling loss curve leaves room;
    // top-left is where every curve starts.
    let mut overlay = div()
        .absolute()
        .top(px(2.))
        .right(px(4.))
        .flex()
        .flex_col()
        .items_end()
        .font_family(crate::fonts::mono_family());
    for l in labels {
        overlay = overlay.child(
            div()
                .text_color(rgb(l.color))
                .text_size(px(label_px))
                .child(l.text),
        );
    }

    let mut card = div().relative().w_full().bg(rgb(theme.bg));
    card = match height {
        Some(h) => card.h(px(h)),
        None => card.flex_1().min_h(px(80.)),
    };
    card.child(chart_canvas(series, grid)).child(overlay)
}

fn chart_box(
    theme: &Theme,
    series: Vec<Series>,
    labels: Vec<Label>,
    grid: Rgba,
    height: f32,
) -> impl IntoElement {
    chart_box_sized(theme, series, labels, grid, Some(height))
}

/// Flex-filling chart body for the pop-out windows.
pub(crate) fn chart_box_fill(
    theme: &Theme,
    series: Vec<Series>,
    labels: Vec<Label>,
    grid: Rgba,
) -> impl IntoElement {
    chart_box_sized(theme, series, labels, grid, None)
}

/// The polyline canvas. Grid lines are 1px filled quads; series are stroked
/// paths; area-filled series add a closed polygon down to the baseline.
fn chart_canvas(series: Vec<Series>, grid: Rgba) -> impl IntoElement {
    canvas(
        move |_, _, _| {},
        move |bounds: Bounds<Pixels>, _, window: &mut Window, _| {
            let ox = f32::from(bounds.origin.x);
            let oy = f32::from(bounds.origin.y);
            let w = f32::from(bounds.size.width);
            let h = f32::from(bounds.size.height);
            if w <= 0.0 || h <= 0.0 {
                return;
            }

            // Horizontal grid (5 lines).
            for i in 0..5 {
                let y = oy + (h / 5.0) * i as f32;
                window.paint_quad(fill(
                    Bounds {
                        origin: point(px(ox), px(y)),
                        size: size(px(w), px(1.)),
                    },
                    grid,
                ));
            }

            for s in &series {
                // A non-finite coordinate makes lyon's path builder panic, so
                // only finite samples (and a finite range) may reach line_to.
                // NaN losses are real data — they get skipped, not the run.
                let pts: Vec<(usize, f32)> = s
                    .values
                    .iter()
                    .copied()
                    .enumerate()
                    .filter(|(_, v)| v.is_finite())
                    .collect();
                if pts.len() < 2 || !s.min.is_finite() || !s.max.is_finite() {
                    continue;
                }
                let range = (s.max - s.min).abs().max(f32::EPSILON);
                let map = |i: usize, v: f32| -> gpui::Point<Pixels> {
                    let x = ox + (i as f32 / (s.values.len() as f32 - 1.0)) * w;
                    let y = oy + h - ((v - s.min) / range) * (h - 16.0) - 8.0;
                    point(px(x), px(y))
                };

                // Optional area fill under the curve.
                if s.fill {
                    let mut fb = PathBuilder::fill();
                    fb.move_to(map(pts[0].0, pts[0].1));
                    for &(i, v) in &pts[1..] {
                        fb.line_to(map(i, v));
                    }
                    fb.line_to(point(px(ox + w), px(oy + h)));
                    fb.line_to(point(px(ox), px(oy + h)));
                    fb.close();
                    if let Ok(path) = fb.build() {
                        window.paint_path(path, s.color.alpha(0.10));
                    }
                }

                // Stroked polyline.
                let mut b = PathBuilder::stroke(px(s.width));
                b.move_to(map(pts[0].0, pts[0].1));
                for &(i, v) in &pts[1..] {
                    b.line_to(map(i, v));
                }
                if let Ok(path) = b.build() {
                    window.paint_path(path, s.color);
                }
            }
        },
    )
    .size_full()
}

// ── Data preparation ─────────────────────────────────────────────────────────

fn series_colors(theme: &Theme) -> [u32; 5] {
    theme.series
}

/// Loss: every ENABLED, non-memory scalar — the live run plus any stored run
/// overlays — per-series min/max combining every source of that name so
/// same-named curves share a scale and are directly comparable. The min/max
/// come from run-global stats (monotone), so mid-run eviction never rescales.
///
/// Live series keep their name-based palette colors; overlay runs draw all
/// their scalars in a per-run color (chart color = run, the W&B convention),
/// matching the chip shown in the RUNS list. Overlays skip the registry
/// enabled-filter: toggling a stored run on is already an explicit request.
pub(crate) fn loss_data(app: &JadeApp) -> (Vec<Series>, Vec<Label>) {
    let colors = series_colors(&app.theme);
    let mut series = Vec::new();
    let mut labels = Vec::new();

    // Stable name set: the live run (enabled only), then overlay names.
    let mut names: Vec<String> = Vec::new();
    for name in app.training.current.scalars.keys() {
        if is_memory_name(name) || !app.registry.is_enabled(Kind::Scalar, name) {
            continue;
        }
        if !names.contains(name) {
            names.push(name.clone());
        }
    }
    for (_, data) in &app.run_overlays {
        for name in data.scalars.keys() {
            if is_memory_name(name) {
                continue;
            }
            if !names.contains(name) {
                names.push(name.clone());
            }
        }
    }
    names.sort();

    // Per-name range across every source that will plot (≥2 points).
    let mut range: std::collections::HashMap<&str, (f64, f64)> =
        std::collections::HashMap::new();
    {
        let mut fold = |name: &str, data: &crate::training::RunData| {
            let plotted = data.scalars.get(name).map(|v| v.len() >= 2).unwrap_or(false);
            if !plotted {
                return;
            }
            if let Some(st) = data.scalar_stats.get(name) {
                // `range` only keeps keys borrowed from `names`.
                if let Some(key) = names.iter().find(|n| n.as_str() == name) {
                    let e = range
                        .entry(key.as_str())
                        .or_insert((f64::INFINITY, f64::NEG_INFINITY));
                    e.0 = e.0.min(st.min);
                    e.1 = e.1.max(st.max);
                }
            }
        };
        for name in &names {
            fold(name, &app.training.current);
            for (_, data) in &app.run_overlays {
                fold(name, data);
            }
        }
    }

    // Overlay runs first, so the live curves draw on top of them.
    for (oi, (id, data)) in app.run_overlays.iter().enumerate() {
        let ocolor = overlay_color(&app.theme, oi);
        for name in &names {
            let Some(v) = data.scalars.get(name) else { continue };
            let Some(&(min, max)) = range.get(name.as_str()) else { continue };
            if v.len() < 2 {
                continue;
            }
            series.push(Series {
                color: rgb(ocolor).alpha(0.55),
                width: 1.0,
                fill: false,
                values: v.iter().map(|x| x.value as f32).collect(),
                min: min as f32,
                max: max as f32,
            });
        }
        let title = app
            .stored_runs
            .iter()
            .find(|r| r.id == *id)
            .map(run_title)
            .unwrap_or_default();
        labels.push(Label { text: format!("#{id} {title}"), color: ocolor });
    }

    let mut ci = 0usize;
    for name in &names {
        let color = colors[ci % colors.len()];
        ci += 1;

        let cur = app.training.current.scalars.get(name);
        let Some(&(min, max)) = range.get(name.as_str()) else {
            continue;
        };
        if !min.is_finite() || !max.is_finite() {
            continue;
        }

        if let Some(c) = cur {
            if c.len() >= 2 {
                series.push(Series {
                    color: rgb(color),
                    width: 1.5,
                    fill: false,
                    values: c.iter().map(|x| x.value as f32).collect(),
                    min: min as f32,
                    max: max as f32,
                });
            }
        }

        if let Some(pt) = cur.and_then(|c| c.last()) {
            labels.push(Label {
                text: format!("{}: {:.4}", name, pt.value),
                color,
            });
        }
    }
    (series, labels)
}

/// Memory: a `memory`/`heap`-named scalar if present, else live heap history;
/// baseline 0, area-filled live run, plus any stored-run overlays. The scale
/// is the run-global peak (monotone) so eviction can't move it.
pub(crate) fn memory_data(app: &JadeApp) -> (Vec<Series>, Vec<Label>) {
    let accent = app.theme.mem_accent;
    let mut series = Vec::new();
    let mut labels = Vec::new();

    let mem_name = app
        .training
        .current
        .scalars
        .keys()
        .find(|n| is_memory_name(n))
        .cloned();

    let (cur_vals, max_val, label_text): (Vec<f32>, f64, String) = if let Some(name) = &mem_name {
        let cur: Vec<f32> = app
            .training
            .current
            .scalars
            .get(name)
            .map(|a| a.iter().map(|p| p.value as f32).collect())
            .unwrap_or_default();
        let cs = app
            .training
            .current
            .scalar_stats
            .get(name)
            .map(|s| s.max)
            .unwrap_or(0.0);
        let last = app
            .training
            .current
            .scalars
            .get(name)
            .and_then(|a| a.last())
            .map(|p| p.value)
            .unwrap_or(0.0);
        (
            cur,
            cs.max(1.0),
            format!("{}: {}", name, memory_scalar_label(name, last)),
        )
    } else {
        let cur: Vec<f32> = app
            .training
            .current
            .memory
            .iter()
            .map(|m| m.heap_used as f32)
            .collect();
        let max = app.training.current.memory_max.max(1.0);
        let last = app
            .training
            .current
            .memory
            .last()
            .map(|m| m.heap_used)
            .unwrap_or(0.0);
        (cur, max, format!("Heap: {}", format_bytes(last)))
    };

    // Overlay runs: same memory-named scalar when they have it, else their
    // live-heap samples. Values collected before the plottability check so a
    // chart with only overlays still renders.
    let overlays: Vec<(i64, u32, Vec<f32>, f64)> = app
        .run_overlays
        .iter()
        .enumerate()
        .map(|(oi, (id, data))| {
            let vals: Vec<f32> = mem_name
                .as_ref()
                .and_then(|n| data.scalars.get(n))
                .map(|a| a.iter().map(|p| p.value as f32).collect())
                .unwrap_or_else(|| data.memory.iter().map(|m| m.heap_used as f32).collect());
            let max = mem_name
                .as_ref()
                .and_then(|n| data.scalar_stats.get(n))
                .map(|s| s.max)
                .unwrap_or(data.memory_max);
            (*id, overlay_color(&app.theme, oi), vals, max)
        })
        .collect();

    if cur_vals.len() < 2 && overlays.iter().all(|(_, _, v, _)| v.len() < 2) {
        return (series, labels);
    }

    // Shared 0-based scale across live + overlaid runs.
    let max_val = overlays
        .iter()
        .fold(max_val, |m, (_, _, _, omax)| m.max(*omax))
        .max(1.0);

    // Overlays under the live curves.
    for (id, ocolor, vals, _) in overlays {
        if vals.len() < 2 {
            continue;
        }
        series.push(Series {
            color: rgb(ocolor).alpha(0.55),
            width: 1.0,
            fill: false,
            values: vals,
            min: 0.0,
            max: max_val as f32,
        });
        labels.push(Label {
            text: format!("#{id}"),
            color: ocolor,
        });
    }

    if cur_vals.len() >= 2 {
        series.push(Series {
            color: rgb(accent),
            width: 1.5,
            fill: true,
            values: cur_vals,
            min: 0.0,
            max: max_val as f32,
        });
    }
    labels.push(Label {
        text: label_text,
        color: accent,
    });
    (series, labels)
}

/// The readout for a memory-named scalar. A name that carries its own unit
/// (`memory_mb`, `heap_gb`) is printed in that unit; a bare name is bytes.
pub(crate) fn memory_scalar_label(name: &str, value: f64) -> String {
    let lower = name.to_lowercase();
    for (suffix, unit) in [("_kb", "KB"), ("_mb", "MB"), ("_gb", "GB")] {
        if lower.ends_with(suffix) {
            return format!("{}{unit}", fmt_val(value));
        }
    }
    format_bytes(value)
}

/// One kernel mini-chart's prepared data (shared with the pop-out window).
pub(crate) struct KernelChart {
    pub name: String,
    pub series: Series,
    pub label: Label,
}

/// Kernel time data prep: one independent series per ENABLED timer (raw timer
/// or bundle) with ≥2 samples, each on its OWN run-global ms scale — so a
/// 0.05ms embed kernel stays readable next to a 5ms attention kernel, and
/// pipelines chart independently without having to be bundled.
pub(crate) fn kernel_chart_data(app: &JadeApp) -> Vec<KernelChart> {
    let colors = series_colors(&app.theme);

    let mut order: Vec<String> = Vec::new();
    let mut map: std::collections::HashMap<String, Vec<f32>> = std::collections::HashMap::new();
    for t in &app.training.current.timings {
        if !app.registry.is_enabled(Kind::Timer, &t.name) {
            continue;
        }
        if !map.contains_key(&t.name) {
            order.push(t.name.clone());
        }
        map.entry(t.name.clone()).or_default().push(t.ms as f32);
    }
    order.retain(|n| map[n].len() >= 2);

    order
        .into_iter()
        .enumerate()
        .map(|(ci, name)| {
            let values = map.remove(&name).unwrap_or_default();
            let color = colors[ci % colors.len()];
            // Per-series monotone max (never the shared run max): each pipeline
            // fills its own chart, and eviction can't rescale it mid-run.
            let max_ms = app
                .training
                .current
                .timing_max_by_name
                .get(&name)
                .copied()
                .unwrap_or(0.0)
                .max(1e-6) as f32;
            let last = values.last().copied().unwrap_or(0.0);
            KernelChart {
                series: Series {
                    color: rgb(color),
                    width: 1.5,
                    fill: true,
                    values,
                    min: 0.0,
                    max: max_ms,
                },
                label: Label {
                    text: format!("{}: {:.3}ms", name, last),
                    color,
                },
                name,
            }
        })
        .collect()
}

/// The sidebar's kernel-time section: one mini-chart per plottable series.
/// Returns `None` when nothing is plottable (section stays hidden).
fn kernel_charts(
    app: &JadeApp,
    theme: &Theme,
    grid: Rgba,
    cx: &mut Context<JadeApp>,
) -> Option<gpui::AnyElement> {
    let charts = kernel_chart_data(app);
    if charts.is_empty() {
        return None;
    }

    let mut col = div()
        .flex()
        .flex_col()
        .gap_1()
        .child(section_label_row("Kernel time (ms)", MetricSection::Kernels, theme, cx));
    for k in charts {
        let sel = k.name.clone();
        col = col.child(
            div()
                .w_full()
                .debug_selector(move || format!("kernel-chart-{sel}"))
                .child(chart_box(theme, vec![k.series], vec![k.label], grid, KERNEL_CHART_H)),
        );
    }
    Some(col.into_any_element())
}

/// One timing-breakdown bar's prepared data (shared with the pop-out window).
pub(crate) struct TimingRow {
    pub name: String,
    /// Bar length as a fraction of the largest total.
    pub frac: f32,
    /// Preformatted per-call average ("1.23ms" / seconds at ≥1000ms).
    pub avg: String,
}

/// Timing breakdown data prep: every enabled timer, sorted by total ms
/// descending. Callers truncate to taste (the sidebar shows 8).
pub(crate) fn timing_rows(app: &JadeApp) -> Vec<TimingRow> {
    let mut order: Vec<String> = Vec::new();
    let mut agg: std::collections::HashMap<String, (f64, u32)> = std::collections::HashMap::new();
    for t in &app.training.current.timings {
        if !app.registry.is_enabled(Kind::Timer, &t.name) {
            continue;
        }
        if !agg.contains_key(&t.name) {
            order.push(t.name.clone());
        }
        let e = agg.entry(t.name.clone()).or_insert((0.0, 0));
        e.0 += t.ms;
        e.1 += 1;
    }
    let mut rows: Vec<(String, f64, u32)> =
        order.into_iter().map(|n| { let (t, c) = agg[&n]; (n, t, c) }).collect();
    rows.sort_by(|a, b| b.1.total_cmp(&a.1));
    let max_total = rows.first().map(|r| r.1).unwrap_or(1.0).max(1.0);

    rows.into_iter()
        .map(|(name, total, count)| TimingRow {
            name,
            frac: (total / max_total) as f32,
            avg: format_avg_ms(total / count.max(1) as f64),
        })
        .collect()
}

/// Timing breakdown: top 8 enabled timers by total ms, horizontal bars with an
/// average label (seconds at ≥1000ms).
fn timing_breakdown(app: &JadeApp, theme: &Theme, cx: &mut Context<JadeApp>) -> impl IntoElement {
    let mut rows = timing_rows(app);
    rows.truncate(8);

    const TRACK_W: f32 = 110.0;
    let mut list = div().flex().flex_col().gap_1();
    for row in rows {
        list = list.child(
            div()
                .flex()
                .items_center()
                .gap_2()
                .text_size(px(10.))
                .child(
                    div()
                        .w(px(96.))
                        .text_color(rgb(theme.text))
                        .overflow_hidden()
                        .child(row.name),
                )
                .child(
                    div()
                        .w(px(TRACK_W))
                        .h(px(6.))
                        
                        .bg(rgb(theme.panel))
                        .child(
                            div()
                                .h_full()
                                .w(px(TRACK_W * row.frac))
                                
                                .bg(rgb(theme.accent)),
                        ),
                )
                .child(
                    div()
                        .text_color(rgb(theme.muted))
                        .child(row.avg),
                ),
        );
    }

    div()
        .flex()
        .flex_col()
        .gap_1()
        .child(section_label_row("Timing", MetricSection::Timing, theme, cx))
        .child(list)
}

/// One tensor preview's prepared data (shared with the pop-out window).
pub(crate) struct TensorPreviewData {
    pub name: String,
    /// "name  rows×cols @step  [min…max]" caption.
    pub label: String,
    pub image: std::sync::Arc<gpui::RenderImage>,
    /// Source rows/cols ratio, for aspect-preserving boxes at any width.
    pub aspect: f32,
}

/// Tensor preview data prep: one entry per enabled buffer that has frames AND
/// a baked texture, in the registry's discovery order — the same order as the
/// sidebar's BUFFERS list. `training.tensors` is a HashMap whose iteration
/// order reshuffles as buffers stream in, which made previews jump around
/// mid-run and land in a different order every run.
pub(crate) fn tensor_preview_data(app: &JadeApp) -> Vec<TensorPreviewData> {
    let mut out = Vec::new();
    for item in app.registry.items_of_kind(Kind::Buffer) {
        let name = &item.name;
        if !item.enabled {
            continue;
        }
        let Some(ring) = app.training.tensors.get(name) else {
            continue;
        };
        let Some(frame) = ring.back() else {
            continue;
        };
        let Some(preview) = app.preview_images.get(name) else {
            continue; // baked on the next render pass
        };

        let range = if preview.mn.is_finite() {
            format!(
                "  [{}…{}]",
                fmt_val(preview.mn as f64),
                fmt_val(preview.mx as f64)
            )
        } else {
            String::new()
        };
        let dim_r = frame.src_rows.unwrap_or(frame.rows);
        let dim_c = frame.src_cols.unwrap_or(frame.cols);
        out.push(TensorPreviewData {
            label: format!("{}  {}×{} @{}{}", name, dim_r, dim_c, frame.step, range),
            image: preview.image.clone(),
            aspect: dim_r as f32 / dim_c.max(1) as f32,
            name: name.clone(),
        });
    }
    out
}

/// Tensor heatmap previews — one per enabled buffer with frames; clicking a
/// card opens the 3D weight-grid overlay focused on that buffer
/// (training-view.ts:732 `wrapper.addEventListener('click', …)`). Returns
/// `None` when the section should stay hidden.
///
/// Each preview draws a single textured quad from the image
/// [`JadeApp::ensure_preview_images`] baked when the frame ARRIVED — painting
/// one quad per cell here (the old `heatmap_canvas`) rebuilt up to 65k
/// primitives per preview per repaint and was the training-view lag. This is
/// the TS architecture: `putImageData` once per frame, GPU `drawImage` after.
fn tensor_previews(
    app: &JadeApp,
    theme: &Theme,
    cx: &mut Context<JadeApp>,
) -> Option<impl IntoElement> {
    let mut previews = div().flex().flex_col().gap_2();
    let mut any = false;

    for (preview_ix, data) in tensor_preview_data(app).into_iter().enumerate() {
        any = true;

        // Preserve source aspect ratio inside a fixed-width preview box.
        let preview_w = 236.0f32;
        let box_h = (preview_w * data.aspect).clamp(40.0, 180.0);
        let label = data.label;

        let open_name = data.name;
        previews = previews.child(
            div()
                .id(("tensor-preview", preview_ix))
                .flex()
                .flex_col()
                .gap_1()
                .cursor_pointer()
                .on_click(cx.listener(move |app: &mut JadeApp, _e, _w, cx| {
                    app.wg3d.open(Some(&open_name));
                    cx.notify();
                }))
                .child(
                    div()
                        .text_color(rgb(theme.muted))
                        .text_size(px(9.))
                        .child(label),
                )
                .child(
                    // Absolute img size: gpui's `img` force-sets the style's
                    // aspect-ratio from the texture's natural size, which
                    // overrides percentage sizing and let near-square tensors
                    // draw square past the 180px box (bleeding over the panels
                    // below). Definite w/h sidesteps that; overflow_hidden is
                    // the backstop.
                    div()
                        .w_full()
                        .h(px(box_h))
                        
                        .bg(rgb(theme.bg))
                        .overflow_hidden()
                        .child(
                            gpui::img(data.image)
                                .object_fit(gpui::ObjectFit::Fill)
                                .w(px(preview_w))
                                .h(px(box_h)),
                        ),
                ),
        );
    }

    if any {
        Some(
            div()
                .flex()
                .flex_col()
                .gap_2()
                .child(section_label_row("Tensors", MetricSection::Tensors, theme, cx))
                .child(previews),
        )
    } else {
        None
    }
}

/// A tensor preview baked to a texture once per NEW frame, cached on
/// `JadeApp::preview_images` by buffer name. `mn`/`mx` ride along so the label
/// doesn't rescan the tensor every repaint.
pub struct PreviewImage {
    pub step: i64,
    pub image: std::sync::Arc<gpui::RenderImage>,
    pub mn: f32,
    pub mx: f32,
}

/// Bake one frame into a [`gpui::RenderImage`] — the TS `drawHeatmap`
/// offscreen-canvas step (training-view.ts:750-792): diverging colormap
/// normalized by the frame's max-abs, written once, drawn scaled thereafter.
/// gpui's sprite atlas expects BGRA byte order (`RenderImage` docs). Small
/// grids are integer-replicated toward ~256px so the GPU's linear sampling
/// keeps the crisp blocky look (`image-rendering: pixelated`).
pub fn build_preview_image(frame: &crate::training::TensorFrame) -> PreviewImage {
    let rows = frame.rows.max(1);
    let cols = frame.cols.max(1);
    let n = (rows as usize) * (cols as usize);

    let mut mn = f32::INFINITY;
    let mut mx = f32::NEG_INFINITY;
    let mut max_abs = 0.0f32;
    for &v in frame.data.iter().take(n) {
        if v < mn {
            mn = v;
        }
        if v > mx {
            mx = v;
        }
        let a = v.abs();
        if a.is_finite() && a > max_abs {
            max_abs = a;
        }
    }

    let k = (256 / rows.max(cols)).clamp(1, 32);
    let mut buf = image::RgbaImage::new(cols * k, rows * k);
    for r in 0..rows {
        for c in 0..cols {
            let v = frame
                .data
                .get((r as usize) * (cols as usize) + c as usize)
                .copied()
                .unwrap_or(0.0);
            let color = diverging(v, max_abs);
            let px = image::Rgba([
                (color.b * 255.0).round() as u8,
                (color.g * 255.0).round() as u8,
                (color.r * 255.0).round() as u8,
                255,
            ]);
            for dy in 0..k {
                for dx in 0..k {
                    buf.put_pixel(c * k + dx, r * k + dy, px);
                }
            }
        }
    }

    PreviewImage {
        step: frame.step,
        image: std::sync::Arc::new(gpui::RenderImage::new(smallvec::smallvec![
            image::Frame::new(buf)
        ])),
        mn,
        mx,
    }
}

/// The preview shares the 3D grid's colormap, so a cell reads the same
/// color in the sidebar and in the overlay.
fn diverging(value: f32, max_abs: f32) -> Rgba {
    let inv = if max_abs > 0.0 { 1.0 / max_abs } else { 0.0 };
    let [r, g, b] = crate::wg3d::grid::diverging(value, inv);
    Rgba { r, g, b, a: 1.0 }
}
