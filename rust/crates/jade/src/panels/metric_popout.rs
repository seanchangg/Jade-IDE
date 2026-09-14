//! Pop-out metric windows (recording aid): every TRAINING-view section — Loss,
//! Memory, Kernel time, Timing breakdown, Tensor previews — can open in its
//! own resizable window via the "⧉" (external-link) button next to the section
//! label. The window is a pure projection of the same `JadeApp` state the
//! sidebar renders — an observer on the app entity repaints it whenever
//! telemetry lands, so charts stream live at whatever size the user drags the
//! window to (nice and big for screen recordings). Data prep is shared with
//! the sidebar (`training_view`'s `pub(crate)` `*_data`/`*_rows` helpers).

use gpui::{div, prelude::*, px, relative, rgb, Context, Entity, Window};

use crate::app::JadeApp;
use crate::panels::training_view::{
    chart_box_fill, kernel_chart_data, loss_data, memory_data, tensor_preview_data, timing_rows,
};

/// Which TRAINING section a pop-out window shows.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum MetricSection {
    Loss,
    Memory,
    Kernels,
    Timing,
    Tensors,
}

impl MetricSection {
    pub fn title(self) -> &'static str {
        match self {
            MetricSection::Loss => "Loss",
            MetricSection::Memory => "Memory",
            MetricSection::Kernels => "Kernel time (ms)",
            MetricSection::Timing => "Timing",
            MetricSection::Tensors => "Tensors",
        }
    }
}

/// Root view of one pop-out window.
pub struct MetricPopout {
    app: Entity<JadeApp>,
    section: MetricSection,
}

impl MetricPopout {
    pub fn new(app: Entity<JadeApp>, section: MetricSection, cx: &mut Context<Self>) -> Self {
        // The main app notifies on every applied telemetry event — piggyback
        // on that to repaint this window's charts.
        cx.observe(&app, |_, _, cx| cx.notify()).detach();
        Self { app, section }
    }
}

/// Centered muted placeholder while a section has nothing plottable yet.
fn waiting(text: &str, muted: u32) -> gpui::AnyElement {
    div()
        .flex_1()
        .flex()
        .items_center()
        .justify_center()
        .text_color(rgb(muted))
        .text_sm()
        .child(text.to_string())
        .into_any_element()
}

impl Render for MetricPopout {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let app = self.app.read(cx);
        let theme = app.theme.clone();
        let grid = rgb(theme.grid_line).alpha(theme.grid_alpha);

        let mut root = div()
            .size_full()
            .flex()
            .flex_col()
            .gap_2()
            .p_4()
            .bg(rgb(theme.bg));

        match self.section {
            MetricSection::Loss => {
                let (series, labels) = loss_data(app);
                root = if series.is_empty() {
                    root.child(waiting("waiting for scalar data…", theme.muted))
                } else {
                    root.child(chart_box_fill(&theme, series, labels, grid))
                };
            }
            MetricSection::Memory => {
                let (series, labels) = memory_data(app);
                root = if series.is_empty() {
                    root.child(waiting("waiting for memory samples…", theme.muted))
                } else {
                    root.child(chart_box_fill(&theme, series, labels, grid))
                };
            }
            MetricSection::Kernels => {
                let charts = kernel_chart_data(app);
                if charts.is_empty() {
                    root = root.child(waiting("no enabled timers with data yet…", theme.muted));
                } else {
                    // Every chart flex-fills, so N kernels split the window
                    // evenly; the per-series overlay label names each one.
                    for k in charts {
                        root = root.child(chart_box_fill(
                            &theme,
                            vec![k.series],
                            vec![k.label],
                            grid,
                        ));
                    }
                }
            }
            MetricSection::Timing => {
                let rows = timing_rows(app);
                if rows.is_empty() {
                    root = root.child(waiting("no enabled timers with data yet…", theme.muted));
                } else {
                    // All rows (the sidebar truncates to 8), scaled up: the
                    // bar track flex-fills, so bars grow with the window.
                    let mut list = div()
                        .id("timing-pop-scroll")
                        .flex_1()
                        .overflow_y_scroll()
                        .flex()
                        .flex_col()
                        .gap_2();
                    for row in rows {
                        list = list.child(
                            div()
                                .flex()
                                .items_center()
                                .gap_3()
                                .text_size(px(13.))
                                .child(
                                    div()
                                        .w(px(220.))
                                        .flex_none()
                                        .text_color(rgb(theme.text))
                                        .overflow_hidden()
                                        .child(row.name),
                                )
                                .child(
                                    div()
                                        .flex_1()
                                        .h(px(10.))
                                        
                                        .bg(rgb(theme.panel))
                                        .child(
                                            div()
                                                .h_full()
                                                .w(relative(row.frac))
                                                
                                                .bg(rgb(theme.accent)),
                                        ),
                                )
                                .child(
                                    div()
                                        .w(px(80.))
                                        .flex_none()
                                        .text_color(rgb(theme.muted))
                                        .child(row.avg),
                                ),
                        );
                    }
                    root = root.child(list);
                }
            }
            MetricSection::Tensors => {
                let previews = tensor_preview_data(app);
                if previews.is_empty() {
                    root = root.child(waiting("no enabled buffers with frames yet…", theme.muted));
                } else {
                    // Wrapping card grid, ~2.5× the sidebar's preview width.
                    // Textures are baked by the MAIN window's render pass
                    // (`ensure_preview_images`), which runs on the same
                    // notifies that repaint this window.
                    let mut wrap = div()
                        .id("tensor-pop-scroll")
                        .flex_1()
                        .overflow_y_scroll()
                        .flex()
                        .flex_row()
                        .flex_wrap()
                        .gap_4();
                    for p in previews {
                        let w = 560.0f32;
                        let h = (w * p.aspect).clamp(80.0, 560.0);
                        wrap = wrap.child(
                            div()
                                .flex()
                                .flex_col()
                                .gap_1()
                                .child(
                                    div()
                                        .text_color(rgb(theme.muted))
                                        .text_size(px(12.))
                                        .child(p.label),
                                )
                                .child(
                                    // Definite w/h, same reason as the
                                    // sidebar: `img` force-sets aspect-ratio
                                    // from the texture's natural size.
                                    div()
                                        .w(px(w))
                                        .h(px(h))
                                        
                                        .overflow_hidden()
                                        .child(
                                            gpui::img(p.image)
                                                .object_fit(gpui::ObjectFit::Fill)
                                                .w(px(w))
                                                .h(px(h)),
                                        ),
                                ),
                        );
                    }
                    root = root.child(wrap);
                }
            }
        }

        root
    }
}
