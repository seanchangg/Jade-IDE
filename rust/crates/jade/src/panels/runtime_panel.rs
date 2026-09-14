//! RUNTIME panel (feature inventory §5.4, Phase-4 wave 2).
//!
//! Right-sidebar panel mounted above TRAINING, toggled by the Runtime chip.
//! Sections:
//!   - **SPEED**: last-run duration (live-ticking while running), personal best
//!     accented when beaten, vs-last delta colored (faster green / slower red).
//!   - **MEMORY**: heap / peak / allocs / frees / leaks from [`MemoryBarState`]
//!     (leaks in red).
//!   - **HOTSPOTS**: the top-10 executed source lines as horizontal bars; click
//!     a row to (re)open the run's file in the viewer. There is no scroll-to-line
//!     in the read-only viewer yet, so click only opens the file (noted below).
//!   - **HISTORY**: the last 10 runs (#n · duration · peak).
//!
//! BENCHMARKS (named snapshots) and history persistence are DEFERRED — the model
//! keeps the last runs in memory only; a note is rendered in the section.
//!
//! The hotspot selection is a pure, unit-tested function; the rest is a thin
//! projection over `JadeApp` state.

use std::collections::HashMap;

use gpui::{div, prelude::*, px, rgb, Context, FocusHandle, KeyDownEvent};

use crate::app::JadeApp;
use crate::benchmark::{self, Delta};
use crate::format::{format_bytes, format_duration};
use crate::theme::Theme;

/// One completed run, retained for the HISTORY section (last 10 shown).
#[derive(Debug, Clone)]
pub struct RunRecord {
    /// 1-based run number.
    pub n: usize,
    pub duration_ms: u128,
    /// Peak live heap bytes observed during the run.
    pub peak: i64,
}

/// Select the top-`n` executed lines (by execution count, ties broken by lower
/// line number for determinism). Ported from the renderer's hotspot ranking.
pub fn top_hotspots(lines: &HashMap<u32, u32>, n: usize) -> Vec<(u32, u32)> {
    let mut v: Vec<(u32, u32)> = lines.iter().map(|(&l, &c)| (l, c)).collect();
    v.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    v.truncate(n);
    v
}

pub fn render(app: &JadeApp, bench_handle: FocusHandle, cx: &mut Context<JadeApp>) -> impl IntoElement {
    let theme = app.theme.clone();

    div()
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
                .child(crate::assets::ui_icon("gauge", 13., theme.muted))
                .child("RUNTIME"),
        )
        .child(speed_section(app, &theme))
        .child(memory_section(app, &theme))
        .child(hotspots_section(app, &theme, cx))
        .child(benchmarks_section(app, &theme, bench_handle, cx))
        .child(history_section(app, &theme, cx))
}

fn section_label(text: &str, theme: &Theme) -> impl IntoElement {
    div()
        .text_color(rgb(theme.muted))
        .text_size(px(10.))
        .font_weight(gpui::FontWeight::MEDIUM)
        .child(text.to_string())
}

fn kv(label: &str, value: String, color: u32, theme: &Theme) -> impl IntoElement {
    div()
        .flex()
        .flex_row()
        .justify_between()
        .text_size(px(11.))
        .child(div().text_color(rgb(theme.muted)).child(label.to_string()))
        .child(div().text_color(rgb(color)).child(value))
}

fn speed_section(app: &JadeApp, theme: &Theme) -> impl IntoElement {
    // Live-ticking elapsed while running (the pump re-renders on each event),
    // else the last completed duration.
    let (headline, head_color) = if app.running {
        let ms = app
            .run_started
            .map(|t| t.elapsed().as_millis())
            .unwrap_or(0);
        (format!("{ms} ms …"), theme.periwinkle)
    } else if let Some(r) = &app.last_run {
        let color = if app.last_was_best {
            theme.accent
        } else {
            theme.text
        };
        (format!("{} ms", r.duration_ms), color)
    } else {
        ("— no runs yet".to_string(), theme.muted)
    };

    let mut col = div()
        .flex()
        .flex_col()
        .gap_1()
        .child(section_label("SPEED", theme))
        .child(
            div()
                .text_color(rgb(head_color))
                .text_size(px(15.))
                .child(headline),
        );

    if !app.running {
        if app.last_was_best && app.last_run.is_some() {
            col = col.child(
                div()
                    .text_color(rgb(theme.accent))
                    .text_size(px(10.))
                    .child("★ personal best"),
            );
        }
        if let Some(best) = app.best_run_ms {
            col = col.child(kv("best", format!("{best} ms"), theme.muted, theme));
        }
        if let Some(delta) = app.last_delta_ms {
            let (txt, color) = if delta < 0 {
                (format!("{} ms vs last", delta), theme.accent)
            } else if delta > 0 {
                (format!("+{} ms vs last", delta), theme.red)
            } else {
                ("±0 ms vs last".to_string(), theme.muted)
            };
            col = col.child(
                div()
                    .text_color(rgb(color))
                    .text_size(px(10.))
                    .child(txt),
            );
        }
    }
    col
}

fn memory_section(app: &JadeApp, theme: &Theme) -> impl IntoElement {
    let m = &app.mem;
    let leak_color = if m.leak_count > 0 {
        theme.red
    } else {
        theme.text
    };

    div()
        .flex()
        .flex_col()
        .gap_1()
        .child(section_label("MEMORY", theme))
        .child(kv("heap", heap_str(m.heap_used), theme.text, theme))
        .child(kv("peak", heap_str(m.peak_allocation), theme.text, theme))
        .child(kv("allocs", m.alloc_count.to_string(), theme.text, theme))
        .child(kv("frees", m.free_count.to_string(), theme.text, theme))
        .child(kv("leaks", m.leak_count.to_string(), leak_color, theme))
}

fn heap_str(bytes: i64) -> String {
    if bytes <= 0 {
        "0".to_string()
    } else {
        format_bytes(bytes as f64)
    }
}

fn hotspots_section(app: &JadeApp, theme: &Theme, cx: &mut Context<JadeApp>) -> impl IntoElement {
    let hotspots = top_hotspots(&app.last_executed, 10);

    let mut col = div()
        .flex()
        .flex_col()
        .gap_1()
        .child(section_label("HOTSPOTS", theme));

    if hotspots.is_empty() {
        return col.child(
            div()
                .text_color(rgb(theme.muted))
                .text_size(px(10.))
                .child("Build + Run with flow on"),
        );
    }

    let max = hotspots.first().map(|(_, c)| *c).unwrap_or(1).max(1);
    let target = app.active_file.clone();
    for (line, count) in hotspots {
        let frac = (count as f32 / max as f32).clamp(0.05, 1.0);
        let target = target.clone();
        let row = div()
            .id(("hotspot", line as u64))
            .flex()
            .flex_col()
            .gap_1()
            .cursor_pointer()
            .on_click(cx.listener(move |app: &mut JadeApp, _ev, _win, cx| {
                // No scroll-to-line in the read-only viewer yet — just open.
                if let Some(path) = &target {
                    app.open_file(path.clone());
                }
                cx.notify();
            }))
            .child(
                div()
                    .flex()
                    .flex_row()
                    .justify_between()
                    .text_size(px(10.))
                    .child(div().text_color(rgb(theme.muted)).child(format!("L{line}")))
                    .child(div().text_color(rgb(theme.muted)).child(format!("{count}×"))),
            )
            .child(
                // Horizontal bar: filled fraction of the hottest line.
                div().h(px(4.)).w_full().bg(rgb(theme.border)).child(
                    div()
                        .h(px(4.))
                        .w(gpui::relative(frac))
                        
                        .bg(rgb(theme.accent)),
                ),
            );
        col = col.child(row);
    }
    col
}

/// BENCHMARKS (§5.4): saved named runs sorted fastest-first, fastest accented,
/// delta vs the latest run, × delete; plus the inline ⚑ name input when naming.
fn benchmarks_section(
    app: &JadeApp,
    theme: &Theme,
    bench_handle: FocusHandle,
    cx: &mut Context<JadeApp>,
) -> impl IntoElement {
    let mut col = div()
        .flex()
        .flex_col()
        .gap_1()
        .child(section_label("BENCHMARKS", theme));

    // Inline name input (prefilled `#<run> <flags>`), shown while naming.
    if let Some(naming) = &app.bench_naming {
        col = col.child(
            div()
                .id("bench-name-input")
                .track_focus(&bench_handle)
                .flex()
                .flex_row()
                .items_center()
                .h(px(18.))
                .px(px(4.))
                
                .bg(rgb(theme.bg))
                .border_1()
                .border_color(rgb(theme.accent))
                .text_size(px(10.))
                .on_key_down(cx.listener(|a: &mut JadeApp, ev: &KeyDownEvent, _w, cx| {
                    if a.bench_key(&ev.keystroke) {
                        a.save_ui_state();
                        cx.stop_propagation();
                        cx.notify();
                    }
                }))
                .child(div().text_color(rgb(theme.text)).child(naming.buffer.clone()))
                .child(div().w(px(1.)).h(px(12.)).bg(rgb(theme.accent))),
        );
    }

    if app.benchmarks.is_empty() && app.bench_naming.is_none() {
        return col.child(
            div()
                .text_color(rgb(theme.muted))
                .text_size(px(10.))
                .child("Save a run from history"),
        );
    }

    let order = benchmark::sorted_fastest_first(&app.benchmarks);
    let fastest = benchmark::fastest_duration(&app.benchmarks);
    let latest = app.latest_run_ms();
    for idx in order {
        let bm = &app.benchmarks[idx];
        let is_fastest = fastest == Some(bm.duration);
        let dur_color = if is_fastest { theme.accent } else { theme.text };

        // Delta vs latest run (accent ↓ / error ↑ / `=`).
        let delta = benchmark::delta_vs_last(bm.duration, latest);
        let (delta_txt, delta_color) = match delta {
            Delta::None => (String::new(), theme.muted),
            Delta::Equal => ("=".to_string(), theme.muted),
            Delta::Down(_) => (delta.label().unwrap_or_default(), theme.accent),
            Delta::Up(_) => (delta.label().unwrap_or_default(), theme.red),
        };

        let mut row = div()
            .flex()
            .flex_row()
            .items_center()
            .justify_between()
            .gap_1()
            .text_size(px(10.))
            .child(
                div()
                    .flex_1()
                    .overflow_hidden()
                    .text_color(rgb(theme.text))
                    .child(bm.name.clone()),
            )
            .child(div().text_color(rgb(dur_color)).child(format_duration(bm.duration)))
            .child(div().text_color(rgb(theme.muted)).child(heap_str(bm.peak_allocation)));
        if !delta_txt.is_empty() {
            row = row.child(div().text_color(rgb(delta_color)).child(delta_txt));
        }
        row = row.child(
            div()
                .id(("bench-del", idx as u64))
                .text_color(rgb(theme.muted))
                .cursor_pointer()
                .on_click(cx.listener(move |a: &mut JadeApp, _e, _w, cx| {
                    a.delete_benchmark(idx);
                    a.save_ui_state();
                    cx.notify();
                }))
                .child("×"),
        );
        col = col.child(row);
    }
    col
}

fn history_section(app: &JadeApp, theme: &Theme, cx: &mut Context<JadeApp>) -> impl IntoElement {
    let mut col = div()
        .flex()
        .flex_col()
        .gap_1()
        .child(section_label("HISTORY", theme));

    if app.run_history.is_empty() {
        return col.child(
            div()
                .text_color(rgb(theme.muted))
                .text_size(px(10.))
                .child("no runs yet"),
        );
    }

    // Last 10, most recent first. Each row carries a ⚑ to save it as a benchmark.
    for rec in app.run_history.iter().rev().take(10) {
        let run_n = rec.n;
        col = col.child(
            div()
                .flex()
                .flex_row()
                .items_center()
                .justify_between()
                .gap_1()
                .text_size(px(10.))
                .child(div().text_color(rgb(theme.text)).child(format!("#{}", rec.n)))
                .child(
                    div()
                        .flex_1()
                        .text_color(rgb(theme.muted))
                        .child(format!("{} ms · {}", rec.duration_ms, heap_str(rec.peak))),
                )
                .child(
                    // ⚑ save-named-benchmark (§5.4).
                    div()
                        .id(("bench-flag", run_n as u64))
                        .text_color(rgb(theme.muted))
                        .cursor_pointer()
                        .on_click(cx.listener(move |a: &mut JadeApp, _e, _w, cx| {
                            a.begin_benchmark(run_n);
                            cx.notify();
                        }))
                        .child("⚑"),
                ),
        );
    }
    col
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hotspots_top_10_by_count_then_line() {
        let mut lines = HashMap::new();
        // 12 distinct lines with varied counts.
        for (line, count) in [
            (10u32, 5u32),
            (20, 100),
            (30, 100), // tie with line 20 → 20 ranks first (lower line)
            (40, 3),
            (50, 50),
            (60, 7),
            (70, 8),
            (80, 9),
            (90, 11),
            (100, 12),
            (110, 13),
            (120, 1),
        ] {
            lines.insert(line, count);
        }
        let top = top_hotspots(&lines, 10);
        assert_eq!(top.len(), 10, "capped at 10");
        // Hottest first; tie (20 vs 30 both 100) broken by lower line number.
        assert_eq!(top[0], (20, 100));
        assert_eq!(top[1], (30, 100));
        assert_eq!(top[2], (50, 50));
        // The two coldest lines (40:3 and 120:1) fall outside the top 10.
        assert!(!top.iter().any(|&(l, _)| l == 40 || l == 120));
    }

    #[test]
    fn hotspots_fewer_than_n() {
        let mut lines = HashMap::new();
        lines.insert(1, 9);
        lines.insert(2, 4);
        let top = top_hotspots(&lines, 10);
        assert_eq!(top, vec![(1, 9), (2, 4)]);
    }

    #[test]
    fn hotspots_empty() {
        assert!(top_hotspots(&HashMap::new(), 10).is_empty());
    }
}
