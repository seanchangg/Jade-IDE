//! CSV visualizer: a chart view of the active CSV tab with column controls.
//!
//! A `.csv` or `.tsv` tab opens as a chart; ⌘⇧D swaps to the text and back.
//! The single-pane mount and the split-pane body both route here. A control row
//! picks the X column, the Y column, an optional group column that splits
//! the rows into series, bars or lines, and a log X axis. Every control is
//! a chip that cycles on click, so there is no text input to focus.
//!
//! Speed: the file is parsed once per buffer version into typed columns,
//! and the chart data (per-series sorted points) is rebuilt only when the
//! version or the configuration changes. Both live in `CsvState` on the
//! app, filled by `JadeApp::csv_prepare` at the top of a render, so the
//! render itself only walks the prepared points. The chart paints with
//! quads and one stroked path per series on a canvas.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use gpui::{canvas, div, fill, point, prelude::*, px, rgb, size, AnyElement, Bounds, Context, PathBuilder, Pixels};

use crate::app::JadeApp;
use crate::kumo::scale;
use crate::theme::Theme;

/// Series beyond this many collapse into one "other" series.
pub const MAX_SERIES: usize = 8;
const CHART_H: f32 = 260.0;

pub fn is_csv(path: &Path) -> bool {
    matches!(
        path.extension().and_then(|e| e.to_str()).map(|e| e.to_ascii_lowercase()).as_deref(),
        Some("csv") | Some("tsv")
    )
}

// ── parsed table ────────────────────────────────────────────────────────────

/// One parsed file: headers, the cells as text, and each column as numbers
/// where every non-empty cell parsed.
#[derive(Debug, Clone, PartialEq)]
pub struct CsvTable {
    pub headers: Vec<String>,
    pub rows: Vec<Vec<String>>,
    /// `numeric[c]` is `Some` when column c is numeric throughout.
    pub numeric: Vec<Option<Vec<f64>>>,
}

impl CsvTable {
    pub fn is_numeric(&self, col: usize) -> bool {
        self.numeric.get(col).is_some_and(|c| c.is_some())
    }
}

/// Parse CSV or TSV text. The first row is the header. Quoted fields may
/// hold the delimiter and doubled quotes. Short rows are padded, long rows
/// truncated, so every row has one cell per header.
pub fn parse_csv(text: &str) -> CsvTable {
    let delim = if text.lines().next().is_some_and(|l| l.contains('\t') && !l.contains(',')) {
        '\t'
    } else {
        ','
    };
    let mut lines = text.lines().filter(|l| !l.trim().is_empty());
    let headers: Vec<String> = lines.next().map(|l| split_line(l, delim)).unwrap_or_default();
    let n = headers.len();
    let mut rows: Vec<Vec<String>> = Vec::new();
    for l in lines {
        let mut cells = split_line(l, delim);
        cells.resize(n, String::new());
        rows.push(cells);
    }
    let numeric = (0..n)
        .map(|c| {
            let mut v = Vec::with_capacity(rows.len());
            for r in &rows {
                let s = r[c].trim();
                if s.is_empty() {
                    v.push(f64::NAN);
                } else {
                    match s.replace(',', "").parse::<f64>() {
                        Ok(x) => v.push(x),
                        Err(_) => return None,
                    }
                }
            }
            (!v.is_empty()).then_some(v)
        })
        .collect();
    CsvTable {
        headers,
        rows,
        numeric,
    }
}

fn split_line(line: &str, delim: char) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut quoted = false;
    let mut chars = line.chars().peekable();
    while let Some(ch) = chars.next() {
        if quoted {
            if ch == '"' {
                if chars.peek() == Some(&'"') {
                    cur.push('"');
                    chars.next();
                } else {
                    quoted = false;
                }
            } else {
                cur.push(ch);
            }
        } else if ch == '"' {
            quoted = true;
        } else if ch == delim {
            out.push(std::mem::take(&mut cur).trim().to_string());
        } else {
            cur.push(ch);
        }
    }
    out.push(cur.trim().to_string());
    out
}

// ── configuration ───────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChartKind {
    Bars,
    Line,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CsvConfig {
    pub x: usize,
    pub y: usize,
    pub group: Option<usize>,
    pub kind: ChartKind,
    pub log_x: bool,
}

impl CsvConfig {
    /// A sensible start: X is the first numeric column, Y the last numeric
    /// column that is not X, the group the first text column. Log X when
    /// the X values span more than two decades, which every latency
    /// histogram does. Bars when there is a group column (a histogram per
    /// series), lines otherwise.
    pub fn default_for(t: &CsvTable) -> Self {
        let numeric: Vec<usize> = (0..t.headers.len()).filter(|&c| t.is_numeric(c)).collect();
        let x = numeric.first().copied().unwrap_or(0);
        let y = numeric.iter().rev().find(|&&c| c != x).copied().unwrap_or(x);
        let group = (0..t.headers.len()).find(|&c| !t.is_numeric(c));
        let log_x = t
            .numeric
            .get(x)
            .and_then(|c| c.as_ref())
            .map(|v| {
                let (lo, hi) = v.iter().filter(|x| x.is_finite() && **x > 0.0).fold((f64::MAX, 0.0f64), |(lo, hi), &x| (lo.min(x), hi.max(x)));
                lo < f64::MAX && hi / lo > 100.0
            })
            .unwrap_or(false);
        Self {
            x,
            y,
            group,
            kind: if group.is_some() { ChartKind::Bars } else { ChartKind::Line },
            log_x,
        }
    }

    fn cycle(cur: usize, n: usize) -> usize {
        if n == 0 { 0 } else { (cur + 1) % n }
    }
}

// ── chart data ──────────────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq)]
pub struct SeriesData {
    pub name: String,
    /// Points sorted by x, x already log10 when the config asks.
    pub points: Vec<(f64, f64)>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ChartData {
    pub series: Vec<SeriesData>,
    pub x_min: f64,
    pub x_max: f64,
    pub y_min: f64,
    pub y_max: f64,
    pub x_label: String,
    pub y_label: String,
    pub log_x: bool,
    pub kind: ChartKind,
    pub rows: usize,
}

/// Split the table into series and sort each by x. Non-finite cells are
/// dropped. The group column is looked up as text; more than
/// `MAX_SERIES` groups fold into "other".
pub fn build_chart(t: &CsvTable, cfg: &CsvConfig) -> Option<ChartData> {
    let xs = t.numeric.get(cfg.x)?.as_ref()?;
    let ys = t.numeric.get(cfg.y)?.as_ref()?;
    let mut order: Vec<String> = Vec::new();
    let mut by_name: HashMap<String, Vec<(f64, f64)>> = HashMap::new();
    for (i, (&x, &y)) in xs.iter().zip(ys).enumerate() {
        if !x.is_finite() || !y.is_finite() {
            continue;
        }
        let x = if cfg.log_x {
            if x <= 0.0 {
                continue;
            }
            x.log10()
        } else {
            x
        };
        let key = match cfg.group {
            Some(g) => t.rows[i].get(g).cloned().unwrap_or_default(),
            None => String::new(),
        };
        let key = if !by_name.contains_key(&key) && order.len() >= MAX_SERIES {
            "other".to_string()
        } else {
            key
        };
        if !by_name.contains_key(&key) {
            order.push(key.clone());
        }
        by_name.entry(key).or_default().push((x, y));
    }
    let mut series = Vec::new();
    let (mut x_min, mut x_max, mut y_min, mut y_max) = (f64::MAX, f64::MIN, f64::MAX, f64::MIN);
    for name in order {
        let mut pts = by_name.remove(&name).unwrap_or_default();
        pts.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap_or(std::cmp::Ordering::Equal));
        for &(x, y) in &pts {
            x_min = x_min.min(x);
            x_max = x_max.max(x);
            y_min = y_min.min(y);
            y_max = y_max.max(y);
        }
        series.push(SeriesData { name, points: pts });
    }
    if series.is_empty() {
        return None;
    }
    if y_min > 0.0 {
        y_min = 0.0; // bars and counts read from zero
    }
    Some(ChartData {
        series,
        x_min,
        x_max,
        y_min,
        y_max,
        x_label: t.headers.get(cfg.x).cloned().unwrap_or_default(),
        y_label: t.headers.get(cfg.y).cloned().unwrap_or_default(),
        log_x: cfg.log_x,
        kind: cfg.kind,
        rows: xs.len(),
    })
}

// ── app state ───────────────────────────────────────────────────────────────

#[derive(Debug, Default)]
pub struct CsvState {
    /// Chart view on for CSV tabs (⌘⇧D toggles).
    pub visible: bool,
    /// Per-file configuration, kept across tab switches.
    pub configs: HashMap<PathBuf, CsvConfig>,
    /// The parsed table of `(path, buffer version)`.
    pub table: Option<(PathBuf, u64, Arc<CsvTable>)>,
    /// The chart data of `(path, version, config)`.
    pub chart: Option<(PathBuf, u64, CsvConfig, Arc<ChartData>)>,
}

impl JadeApp {
    /// True when the active tab is a CSV shown as a chart.
    pub fn csv_chart_active(&self) -> bool {
        self.csv.visible && self.editor.active_tab().is_some_and(|t| is_csv(&t.path))
    }

    /// Refresh the parse and chart caches for the active CSV tab. Called at
    /// the top of a render; cheap when nothing changed.
    pub fn csv_prepare(&mut self) {
        if !self.csv_chart_active() {
            return;
        }
        let Some(tab) = self.editor.active_tab() else { return };
        let path = tab.path.clone();
        let version = tab.buffer.version();
        let fresh = !matches!(&self.csv.table, Some((p, v, _)) if *p == path && *v == version);
        if fresh {
            let table = Arc::new(parse_csv(&tab.buffer.to_string()));
            self.csv.table = Some((path.clone(), version, table));
        }
        let table = self.csv.table.as_ref().map(|(_, _, t)| t.clone()).unwrap();
        let cfg = self
            .csv
            .configs
            .entry(path.clone())
            .or_insert_with(|| CsvConfig::default_for(&table))
            .clone();
        let same = matches!(&self.csv.chart, Some((p, v, c, _)) if *p == path && *v == version && *c == cfg);
        if !same {
            self.csv.chart = build_chart(&table, &cfg).map(|d| (path, version, cfg, Arc::new(d)));
            if self.csv.chart.is_none() {
                self.csv.chart = None;
            }
        }
    }

    /// The active tab's configuration, when the chart is prepared.
    fn csv_config_mut(&mut self) -> Option<&mut CsvConfig> {
        let path = self.editor.active_tab()?.path.clone();
        self.csv.configs.get_mut(&path)
    }

    fn csv_columns(&self) -> usize {
        self.csv.table.as_ref().map(|(_, _, t)| t.headers.len()).unwrap_or(0)
    }

    pub fn csv_cycle_x(&mut self) {
        let n = self.csv_columns();
        if let Some(c) = self.csv_config_mut() {
            c.x = CsvConfig::cycle(c.x, n);
        }
    }

    pub fn csv_cycle_y(&mut self) {
        let n = self.csv_columns();
        if let Some(c) = self.csv_config_mut() {
            c.y = CsvConfig::cycle(c.y, n);
        }
    }

    /// Group cycles through every column and then "none".
    pub fn csv_cycle_group(&mut self) {
        let n = self.csv_columns();
        if let Some(c) = self.csv_config_mut() {
            c.group = match c.group {
                None if n > 0 => Some(0),
                None => None,
                Some(g) if g + 1 < n => Some(g + 1),
                Some(_) => None,
            };
        }
    }

    pub fn csv_toggle_kind(&mut self) {
        if let Some(c) = self.csv_config_mut() {
            c.kind = match c.kind {
                ChartKind::Bars => ChartKind::Line,
                ChartKind::Line => ChartKind::Bars,
            };
        }
    }

    pub fn csv_toggle_log(&mut self) {
        if let Some(c) = self.csv_config_mut() {
            c.log_x = !c.log_x;
        }
    }
}

// ── rendering ───────────────────────────────────────────────────────────────

/// The focused pane's chart view of its CSV tab.
pub fn pane_body(app: &JadeApp, cx: &mut Context<JadeApp>, theme: &Theme) -> AnyElement {
    let Some(tab) = app.editor.active_tab() else {
        return div().into_any_element();
    };
    let table = app.csv.table.as_ref().map(|(_, _, t)| t.clone());
    let chart = app.csv.chart.as_ref().map(|(_, _, _, d)| d.clone());
    let cfg = app.csv.configs.get(&tab.path).cloned();

    let mut body = div()
        .flex()
        .flex_col()
        .gap(scale::SPACE_3)
        .px(scale::SPACE_4)
        .py(scale::SPACE_3)
        .size_full()
        .text_xs();

    let (Some(table), Some(cfg)) = (table, cfg) else {
        return crate::panels::code_view::focus_shell(
            app,
            cx,
            body.child(div().text_color(rgb(theme.muted)).child("no rows")).into_any_element(),
        );
    };

    body = body.child(controls(&table, &cfg, theme, cx));

    match chart {
        Some(d) => {
            body = body
                .child(legend(&d, theme))
                .child(
                    div()
                        .relative()
                        .w_full()
                        .h(px(CHART_H))
                        .bg(rgb(theme.bg))
                        .child(chart_canvas(d.clone(), theme.series, rgb(theme.grid_line).alpha(theme.grid_alpha))),
                )
                .child(axis_row(&d, theme));
        }
        None => {
            body = body.child(
                div()
                    .text_color(rgb(theme.muted))
                    .child("pick numeric X and Y columns to draw a chart"),
            );
        }
    }
    crate::panels::code_view::focus_shell(app, cx, body.into_any_element())
}

fn chip(id: &'static str, label: String, active: bool, theme: &Theme) -> gpui::Stateful<gpui::Div> {
    div()
        .id(id)
        .flex()
        .flex_row()
        .items_center()
        .h(px(20.))
        .px(scale::SPACE_2)
        .rounded(px(4.))
        .bg(rgb(theme.panel))
        .border_1()
        .border_color(rgb(if active { theme.accent } else { theme.border }))
        .text_color(rgb(theme.text))
        .cursor_pointer()
        .hover(|s| s.bg(theme_hover()))
        .child(label)
}

fn theme_hover() -> gpui::Rgba {
    gpui::rgba(0x88888833)
}

fn controls(table: &CsvTable, cfg: &CsvConfig, theme: &Theme, cx: &mut Context<JadeApp>) -> impl IntoElement {
    let name = |c: usize| table.headers.get(c).cloned().unwrap_or_else(|| format!("col {c}"));
    let group_label = match cfg.group {
        Some(g) => name(g),
        None => "none".to_string(),
    };
    div()
        .flex()
        .flex_row()
        .flex_wrap()
        .items_center()
        .gap(scale::SPACE_2)
        .font_family(crate::fonts::mono_family())
        .child(chip("csv-x", format!("X ▸ {}", name(cfg.x)), true, theme).on_click(cx.listener(|a: &mut JadeApp, _e, _w, cx| {
            a.csv_cycle_x();
            cx.notify();
        })))
        .child(chip("csv-y", format!("Y ▸ {}", name(cfg.y)), true, theme).on_click(cx.listener(|a: &mut JadeApp, _e, _w, cx| {
            a.csv_cycle_y();
            cx.notify();
        })))
        .child(
            chip("csv-group", format!("group ▸ {group_label}"), cfg.group.is_some(), theme).on_click(cx.listener(
                |a: &mut JadeApp, _e, _w, cx| {
                    a.csv_cycle_group();
                    cx.notify();
                },
            )),
        )
        .child(
            chip(
                "csv-kind",
                match cfg.kind {
                    ChartKind::Bars => "bars".to_string(),
                    ChartKind::Line => "line".to_string(),
                },
                false,
                theme,
            )
            .on_click(cx.listener(|a: &mut JadeApp, _e, _w, cx| {
                a.csv_toggle_kind();
                cx.notify();
            })),
        )
        .child(chip("csv-log", "log x".to_string(), cfg.log_x, theme).on_click(cx.listener(|a: &mut JadeApp, _e, _w, cx| {
            a.csv_toggle_log();
            cx.notify();
        })))
        .child(
            div()
                .text_color(rgb(theme.muted))
                .child(format!("{} rows · {} columns · ⌘⇧D for text", table.rows.len(), table.headers.len())),
        )
}

fn legend(d: &ChartData, theme: &Theme) -> impl IntoElement {
    let mut row = div().flex().flex_row().flex_wrap().gap(scale::SPACE_3).font_family(crate::fonts::mono_family());
    for (i, s) in d.series.iter().enumerate() {
        let color = theme.series[i % theme.series.len()];
        let name = if s.name.is_empty() { d.y_label.clone() } else { s.name.clone() };
        row = row.child(
            div()
                .flex()
                .flex_row()
                .items_center()
                .gap(px(4.))
                .child(div().w(px(8.)).h(px(8.)).rounded(px(2.)).bg(rgb(color)))
                .child(div().text_color(rgb(theme.text)).child(format!("{name} ({})", s.points.len()))),
        );
    }
    row
}

fn axis_row(d: &ChartData, theme: &Theme) -> impl IntoElement {
    let fmt_x = |v: f64| if d.log_x { format_num(10f64.powf(v)) } else { format_num(v) };
    div()
        .flex()
        .flex_row()
        .justify_between()
        .font_family(crate::fonts::mono_family())
        .text_color(rgb(theme.muted))
        .child(format!("{} {}", d.x_label, fmt_x(d.x_min)))
        .child(format!("{} up to {}", d.y_label, format_num(d.y_max)))
        .child(format!("{}{}", fmt_x(d.x_max), if d.log_x { " (log)" } else { "" }))
}

pub fn format_num(v: f64) -> String {
    let a = v.abs();
    if a >= 1e9 {
        format!("{:.2}G", v / 1e9)
    } else if a >= 1e6 {
        format!("{:.2}M", v / 1e6)
    } else if a >= 1e4 {
        format!("{:.1}k", v / 1e3)
    } else if a >= 100.0 || v.fract() == 0.0 {
        format!("{v:.0}")
    } else {
        format!("{v:.2}")
    }
}

/// The chart: grid, then one stroked polyline or one row of quads per
/// series. Bars of several series interleave at each x so a histogram per
/// series stays readable.
fn chart_canvas(d: Arc<ChartData>, palette: [u32; 5], grid: gpui::Rgba) -> impl IntoElement {
    canvas(
        move |_, _, _| {},
        move |bounds: Bounds<Pixels>, _, window: &mut gpui::Window, _| {
            let (ox, oy) = (f32::from(bounds.origin.x), f32::from(bounds.origin.y));
            let (w, h) = (f32::from(bounds.size.width), f32::from(bounds.size.height));
            if w <= 0.0 || h <= 0.0 {
                return;
            }
            for i in 0..5 {
                let y = oy + (h / 5.0) * i as f32;
                window.paint_quad(fill(Bounds { origin: point(px(ox), px(y)), size: size(px(w), px(1.)) }, grid));
            }
            let (pad_l, pad_r, pad_t, pad_b) = (6.0f32, 6.0f32, 8.0f32, 4.0f32);
            let xr = (d.x_max - d.x_min).abs().max(1e-12);
            let yr = (d.y_max - d.y_min).abs().max(1e-12);
            let iw = (w - pad_l - pad_r).max(1.0);
            let ih = (h - pad_t - pad_b).max(1.0);
            let map_x = |x: f64| ox + pad_l + ((x - d.x_min) / xr) as f32 * iw;
            let map_y = |y: f64| oy + pad_t + ih - ((y - d.y_min) / yr) as f32 * ih;
            let base_y = map_y(d.y_min.max(0.0).min(d.y_max));
            let n_series = d.series.len().max(1) as f32;
            for (si, s) in d.series.iter().enumerate() {
                let color = rgb(palette[si % palette.len()]);
                match d.kind {
                    ChartKind::Line => {
                        if s.points.len() < 2 {
                            continue;
                        }
                        let mut b = PathBuilder::stroke(px(1.5));
                        b.move_to(point(px(map_x(s.points[0].0)), px(map_y(s.points[0].1))));
                        for &(x, y) in &s.points[1..] {
                            b.line_to(point(px(map_x(x)), px(map_y(y))));
                        }
                        if let Ok(path) = b.build() {
                            window.paint_path(path, color);
                        }
                    }
                    ChartKind::Bars => {
                        // Bar width from the median x gap, split among series.
                        let mut gaps: Vec<f32> = s.points.windows(2).map(|p| map_x(p[1].0) - map_x(p[0].0)).filter(|g| *g > 0.0).collect();
                        gaps.sort_by(|a, b| a.partial_cmp(b).unwrap());
                        let slot = gaps.get(gaps.len() / 2).copied().unwrap_or(iw / 20.0).min(iw / 4.0);
                        let bw = (slot / n_series * 0.85).max(1.0);
                        for &(x, y) in &s.points {
                            let cx = map_x(x) - slot / 2.0 + bw * si as f32 + bw / 2.0;
                            let top = map_y(y);
                            let (y0, y1) = if top < base_y { (top, base_y) } else { (base_y, top) };
                            let hgt = (y1 - y0).max(1.0);
                            window.paint_quad(fill(
                                Bounds { origin: point(px(cx - bw / 2.0), px(y0)), size: size(px(bw), px(hgt)) },
                                color,
                            ));
                        }
                    }
                }
            }
        },
    )
    .size_full()
}

#[cfg(test)]
mod tests {
    use super::*;

    const LATENCY: &str = "name,lower_ns,upper_ns,count\napply warm,40,43,685\napply warm,80,87,2651\napply warm,10000,11263,3\napply cold,80,87,10\napply cold,960,1023,400\n";

    #[test]
    fn parses_headers_rows_and_numeric_columns() {
        let t = parse_csv(LATENCY);
        assert_eq!(t.headers, vec!["name", "lower_ns", "upper_ns", "count"]);
        assert_eq!(t.rows.len(), 5);
        assert!(!t.is_numeric(0) && t.is_numeric(1) && t.is_numeric(3));
        assert_eq!(t.numeric[3].as_ref().unwrap()[1], 2651.0);
    }

    #[test]
    fn quoted_fields_tabs_and_short_rows() {
        let t = parse_csv("a,b,c\n\"x, y\",\"say \"\"hi\"\"\",1\nshort\n");
        assert_eq!(t.rows[0], vec!["x, y", "say \"hi\"", "1"]);
        assert_eq!(t.rows[1], vec!["short", "", ""]);
        assert!(t.is_numeric(2), "an empty cell in a numeric column is NaN, the column stays numeric");
        assert!(t.numeric[2].as_ref().unwrap()[1].is_nan());
        let tsv = parse_csv("a\tb\n1\t2\n");
        assert_eq!(tsv.headers, vec!["a", "b"]);
        assert!(tsv.is_numeric(0) && tsv.is_numeric(1));
        assert!(parse_csv("").headers.is_empty());
    }

    #[test]
    fn default_config_picks_histogram_layout() {
        let t = parse_csv(LATENCY);
        let c = CsvConfig::default_for(&t);
        assert_eq!((c.x, c.y, c.group), (1, 3, Some(0)));
        assert_eq!(c.kind, ChartKind::Bars);
        assert!(c.log_x, "40 to 10000 spans over two decades");
        let plain = parse_csv("t,v\n1,5\n2,7\n3,9\n");
        let c = CsvConfig::default_for(&plain);
        assert_eq!((c.x, c.y, c.group, c.kind, c.log_x), (0, 1, None, ChartKind::Line, false));
    }

    #[test]
    fn chart_groups_sorts_and_logs() {
        let t = parse_csv(LATENCY);
        let cfg = CsvConfig::default_for(&t);
        let d = build_chart(&t, &cfg).unwrap();
        assert_eq!(d.series.len(), 2);
        assert_eq!(d.series[0].name, "apply warm");
        assert_eq!(d.series[0].points.len(), 3);
        assert!((d.series[0].points[0].0 - 40f64.log10()).abs() < 1e-9);
        assert!(d.series[1].points.windows(2).all(|p| p[0].0 <= p[1].0));
        assert_eq!(d.y_min, 0.0);
        assert_eq!(d.y_max, 2651.0);
        let none = build_chart(&t, &CsvConfig { x: 0, ..cfg.clone() });
        assert!(none.is_none(), "a text X column draws nothing");
    }

    #[test]
    fn too_many_groups_fold_into_other() {
        let mut s = String::from("g,x,y\n");
        for i in 0..12 {
            s.push_str(&format!("g{i},{i},1\n"));
        }
        let t = parse_csv(&s);
        let d = build_chart(&t, &CsvConfig { x: 1, y: 2, group: Some(0), kind: ChartKind::Bars, log_x: false }).unwrap();
        assert_eq!(d.series.len(), MAX_SERIES + 1);
        assert_eq!(d.series.last().unwrap().name, "other");
        assert_eq!(d.series.last().unwrap().points.len(), 12 - MAX_SERIES);
    }

    #[test]
    fn numbers_format_short() {
        assert_eq!(format_num(2651.0), "2651");
        assert_eq!(format_num(12345.0), "12.3k");
        assert_eq!(format_num(2.5e6), "2.50M");
        assert_eq!(format_num(0.5), "0.50");
    }
}
