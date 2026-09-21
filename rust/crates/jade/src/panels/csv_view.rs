//! CSV visualizer: a chart view of the active CSV tab with the controls on
//! the axes they configure.
//!
//! A `.csv` or `.tsv` tab opens as a chart; ⌘⇧D swaps to the text and back.
//! The single-pane mount and the split-pane body both route here. The Y
//! column menu sits on the left edge between the Y ticks, the X column and
//! X scale menus sit under the X axis between its ticks, and the group and
//! chart-kind menus sit in the legend row above. Every menu is a dropdown
//! list; a click outside closes it.
//!
//! Hover draws a crosshair and a tooltip with the nearest point of every
//! series. The wheel zooms the X range around the cursor, a drag pans it,
//! and a double click resets. Y refits to the visible points.
//!
//! Speed: the file is parsed once per buffer version into typed columns,
//! and the chart data (per-series sorted points) is rebuilt only when the
//! version or the configuration changes. Both live in `CsvState` on the
//! app, filled by `JadeApp::csv_prepare` at the top of a render, so the
//! render itself only walks the prepared points. The chart paints with
//! quads and one stroked path per series on a canvas; the zoom and hover
//! state is a few numbers, so each interaction is one cheap repaint.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use gpui::{
    canvas, deferred, div, fill, point, prelude::*, px, rgb, size, AnyElement, Bounds, Context, MouseButton,
    MouseDownEvent, MouseMoveEvent, MouseUpEvent, PathBuilder, Pixels, ScrollWheelEvent,
};

use crate::app::JadeApp;
use crate::kumo::{self, scale};
use crate::theme::Theme;

/// Series beyond this many collapse into one "other" series.
pub const MAX_SERIES: usize = 8;
/// Zoom cannot narrow the view below this share of the full range.
const MIN_VIEW_SHARE: f64 = 1e-4;

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
                let (lo, hi) = v
                    .iter()
                    .filter(|x| x.is_finite() && **x > 0.0)
                    .fold((f64::MAX, 0.0f64), |(lo, hi), &x| (lo.min(x), hi.max(x)));
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
}

// ── chart data ──────────────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq)]
pub struct SeriesData {
    pub name: String,
    /// Points sorted by x, x already log10 when the config asks.
    pub points: Vec<(f64, f64)>,
    /// Sum of the y values: for a histogram, the number of samples.
    pub sum_y: f64,
}

impl SeriesData {
    /// The point nearest to `x`, by binary search on the sorted x values.
    pub fn nearest(&self, x: f64) -> Option<(f64, f64)> {
        if self.points.is_empty() {
            return None;
        }
        let i = self.points.partition_point(|p| p.0 < x);
        let cands = [i.checked_sub(1), (i < self.points.len()).then_some(i)];
        cands
            .into_iter()
            .flatten()
            .map(|k| self.points[k])
            .min_by(|a, b| (a.0 - x).abs().partial_cmp(&(b.0 - x).abs()).unwrap())
    }
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

impl ChartData {
    /// The full X range, widened a little when every x is the same.
    pub fn full_range(&self) -> (f64, f64) {
        if self.x_max > self.x_min {
            (self.x_min, self.x_max)
        } else {
            (self.x_min - 0.5, self.x_max + 0.5)
        }
    }

    /// Y limits of every series inside an X range.
    pub fn y_limits(&self, range: (f64, f64)) -> (f64, f64) {
        let all: Vec<usize> = (0..self.series.len()).collect();
        self.y_limits_of(range, &all)
    }
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
        let sum_y = pts.iter().map(|p| p.1).sum();
        series.push(SeriesData { name, points: pts, sum_y });
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

// ── view math ───────────────────────────────────────────────────────────────

/// Zoom `view` by `factor` (under 1 zooms in) around `cursor`, kept inside
/// `full` and never narrower than `MIN_VIEW_SHARE` of it.
pub fn zoom_range(view: (f64, f64), full: (f64, f64), cursor: f64, factor: f64) -> (f64, f64) {
    let min_w = (full.1 - full.0) * MIN_VIEW_SHARE;
    let cursor = cursor.clamp(view.0, view.1);
    let mut lo = cursor - (cursor - view.0) * factor;
    let mut hi = cursor + (view.1 - cursor) * factor;
    if hi - lo < min_w {
        let mid = (lo + hi) / 2.0;
        lo = mid - min_w / 2.0;
        hi = mid + min_w / 2.0;
    }
    clamp_range((lo, hi), full)
}

/// Shift `view` by `delta`, kept inside `full`.
pub fn pan_range(view: (f64, f64), full: (f64, f64), delta: f64) -> (f64, f64) {
    clamp_range((view.0 + delta, view.1 + delta), full)
}

fn clamp_range(mut r: (f64, f64), full: (f64, f64)) -> (f64, f64) {
    let w = (r.1 - r.0).min(full.1 - full.0);
    if r.0 < full.0 {
        r = (full.0, full.0 + w);
    }
    if r.1 > full.1 {
        r = (full.1 - w, full.1);
    }
    r
}

// ── ticks ───────────────────────────────────────────────────────────────────

/// Round tick positions for a linear axis: about `n` steps of 1, 2, or 5
/// times a power of ten, inside `[lo, hi]`.
pub fn linear_ticks(lo: f64, hi: f64, n: usize) -> Vec<f64> {
    if !(hi > lo) || n == 0 {
        return vec![lo];
    }
    let raw = (hi - lo) / n as f64;
    let mag = 10f64.powf(raw.log10().floor());
    let step = [1.0, 2.0, 5.0, 10.0]
        .iter()
        .map(|m| m * mag)
        .find(|s| *s >= raw)
        .unwrap_or(10.0 * mag);
    let mut v = Vec::new();
    let mut t = (lo / step).ceil() * step;
    while t <= hi + step * 1e-9 {
        v.push(if t.abs() < step * 1e-9 { 0.0 } else { t });
        t += step;
    }
    v
}

/// Tick positions for a log10 axis whose values are already log10: every
/// decade inside the range, plus the 2 and 5 marks when three decades or
/// fewer are visible.
pub fn log_ticks(lo: f64, hi: f64) -> Vec<f64> {
    if !(hi > lo) {
        return vec![lo];
    }
    let fine = hi - lo <= 3.0;
    let mut v = Vec::new();
    let mut d = lo.floor() as i64;
    while (d as f64) <= hi + 1e-9 {
        let marks: &[f64] = if fine { &[1.0, 2.0, 5.0] } else { &[1.0] };
        for m in marks {
            let t = d as f64 + m.log10();
            if t >= lo - 1e-9 && t <= hi + 1e-9 {
                v.push(t);
            }
        }
        d += 1;
    }
    if v.is_empty() {
        v.push(lo);
    }
    v
}

// ── app state ───────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CsvMenu {
    X,
    Y,
    Group,
    Kind,
    Scale,
}

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
    /// The open dropdown, if any.
    pub menu: Option<CsvMenu>,
    /// The zoomed X range in chart units; `None` is the full range. Reset
    /// when the chart data changes.
    pub view: Option<(f64, f64)>,
    /// Series hidden from the legend. Reset when the chart data changes.
    pub hidden: HashSet<String>,
    /// Mouse x in window px while over the chart.
    pub hover_x: Option<f32>,
    /// A pan in progress: mouse x at press and the view then.
    pub drag: Option<(f32, (f64, f64))>,
    /// The plot area of the canvas from its last paint: x, y, w, h in
    /// window px, without the axis gutters.
    pub bounds: Arc<Mutex<Option<[f32; 4]>>>,
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
            self.csv.view = None;
            self.csv.hidden.clear();
            self.csv.hover_x = None;
            self.csv.drag = None;
        }
    }

    fn csv_config_mut(&mut self) -> Option<&mut CsvConfig> {
        let path = self.editor.active_tab()?.path.clone();
        self.csv.configs.get_mut(&path)
    }

    pub fn csv_toggle_menu(&mut self, menu: CsvMenu) {
        self.csv.menu = if self.csv.menu == Some(menu) { None } else { Some(menu) };
    }

    pub fn csv_close_menu(&mut self) {
        self.csv.menu = None;
    }

    pub fn csv_pick_x(&mut self, col: usize) {
        if let Some(c) = self.csv_config_mut() {
            c.x = col;
        }
        self.csv.menu = None;
    }

    pub fn csv_pick_y(&mut self, col: usize) {
        if let Some(c) = self.csv_config_mut() {
            c.y = col;
        }
        self.csv.menu = None;
    }

    pub fn csv_pick_group(&mut self, col: Option<usize>) {
        if let Some(c) = self.csv_config_mut() {
            c.group = col;
        }
        self.csv.menu = None;
    }

    pub fn csv_pick_kind(&mut self, kind: ChartKind) {
        if let Some(c) = self.csv_config_mut() {
            c.kind = kind;
        }
        self.csv.menu = None;
    }

    pub fn csv_pick_log(&mut self, log_x: bool) {
        if let Some(c) = self.csv_config_mut() {
            c.log_x = log_x;
        }
        self.csv.menu = None;
    }

    /// Hide or show one series. Y refits to what is shown.
    pub fn csv_toggle_series(&mut self, name: &str) {
        if !self.csv.hidden.remove(name) {
            self.csv.hidden.insert(name.to_string());
        }
    }

    pub fn csv_show_all_series(&mut self) {
        self.csv.hidden.clear();
    }

    /// The series currently shown, in chart order.
    pub fn csv_visible_series(&self) -> Vec<usize> {
        match &self.csv.chart {
            Some((_, _, _, d)) => (0..d.series.len()).filter(|&i| !self.csv.hidden.contains(&d.series[i].name)).collect(),
            None => Vec::new(),
        }
    }

    /// The chart's current X range.
    pub fn csv_view_range(&self) -> Option<(f64, f64)> {
        let d = &self.csv.chart.as_ref()?.3;
        Some(self.csv.view.unwrap_or_else(|| d.full_range()))
    }

    /// Y limits over the shown series inside the current X range.
    pub fn csv_y_limits(&self) -> Option<(f64, f64)> {
        let d = &self.csv.chart.as_ref()?.3;
        Some(d.y_limits_of(self.csv_view_range()?, &self.csv_visible_series()))
    }

    /// Window px → chart x, using the last painted plot bounds.
    fn csv_x_at(&self, px_x: f32) -> Option<f64> {
        let b = (*self.csv.bounds.lock().ok()?)?;
        let range = self.csv_view_range()?;
        let t = ((px_x - b[0]) / b[2].max(1.0)) as f64;
        Some(range.0 + t * (range.1 - range.0))
    }

    /// Wheel over the chart: zoom the X range around the cursor.
    pub fn csv_zoom(&mut self, px_x: f32, factor: f64) {
        let (Some(cursor), Some(view), Some(full)) = (
            self.csv_x_at(px_x),
            self.csv_view_range(),
            self.csv.chart.as_ref().map(|c| c.3.full_range()),
        ) else {
            return;
        };
        let next = zoom_range(view, full, cursor, factor);
        self.csv.view = (next != full).then_some(next);
    }

    /// Drag over the chart: pan by the mouse travel since the press.
    pub fn csv_pan_to(&mut self, px_x: f32) {
        let (Some((start_px, start_view)), Some(b), Some(full)) = (
            self.csv.drag,
            self.csv.bounds.lock().ok().and_then(|b| *b),
            self.csv.chart.as_ref().map(|c| c.3.full_range()),
        ) else {
            return;
        };
        let delta = -((px_x - start_px) / b[2].max(1.0)) as f64 * (start_view.1 - start_view.0);
        let next = pan_range(start_view, full, delta);
        self.csv.view = (next != full).then_some(next);
    }

    pub fn csv_reset_view(&mut self) {
        self.csv.view = None;
    }

    /// Hover readout: chart x at the mouse, and the nearest point of every
    /// shown series as `(series index, x_shown, y)`, x back in the file's
    /// units.
    pub fn csv_hover_points(&self) -> Option<(f64, Vec<(usize, f64, f64)>)> {
        let hx = self.csv.hover_x?;
        let x = self.csv_x_at(hx)?;
        let d = &self.csv.chart.as_ref()?.3;
        let show = |v: f64| if d.log_x { 10f64.powf(v) } else { v };
        let pts = self
            .csv_visible_series()
            .into_iter()
            .filter_map(|i| d.series[i].nearest(x).map(|(px_, py)| (i, show(px_), py)))
            .collect();
        Some((show(x), pts))
    }
}

impl ChartData {
    /// Y limits of the listed series inside an X range, from zero when
    /// all are positive. Falls back to the full limits when nothing is
    /// visible.
    pub fn y_limits_of(&self, range: (f64, f64), series: &[usize]) -> (f64, f64) {
        let (mut lo, mut hi) = (f64::MAX, f64::MIN);
        for &i in series {
            let s = &self.series[i];
            let a = s.points.partition_point(|p| p.0 < range.0);
            for &(x, y) in &s.points[a..] {
                if x > range.1 {
                    break;
                }
                lo = lo.min(y);
                hi = hi.max(y);
            }
        }
        if lo > hi {
            return (self.y_min, self.y_max);
        }
        if lo > 0.0 {
            lo = 0.0;
        }
        if hi <= lo {
            hi = lo + 1.0;
        }
        (lo, hi)
    }
}

// ── rendering ───────────────────────────────────────────────────────────────

/// The focused pane's chart view of its CSV tab.
pub fn pane_body(app: &JadeApp, cx: &mut Context<JadeApp>, theme: &Theme) -> AnyElement {
    let t = &theme.kumo;
    let Some(tab) = app.editor.active_tab() else {
        return div().into_any_element();
    };
    let table = app.csv.table.as_ref().map(|(_, _, t)| t.clone());
    let chart = app.csv.chart.as_ref().map(|(_, _, _, d)| d.clone());
    let cfg = app.csv.configs.get(&tab.path).cloned();

    let mut body = div()
        .id("csv-body")
        .relative()
        .flex()
        .flex_col()
        .gap(scale::SPACE_2)
        .p(scale::SPACE_3)
        .size_full()
        .min_h(px(0.))
        .bg(t.canvas)
        .text_size(scale::TEXT_XS)
        .text_color(t.text_default)
        .font_family(crate::fonts::ui_family());

    let (Some(table), Some(cfg)) = (table, cfg) else {
        return crate::panels::code_view::focus_shell(
            app,
            cx,
            body.child(div().text_color(t.text_subtle).child("no rows")).into_any_element(),
        );
    };

    body = body.child(toolbar(app, &table, &cfg, chart.as_deref(), theme, cx));

    match (chart, app.csv_view_range(), app.csv_y_limits()) {
        (Some(d), Some(range), Some(ylim)) => {
            body = body.child(chart_card(app, &table, &cfg, d, range, ylim, theme, cx));
        }
        _ => {
            body = body.child(
                kumo::Card::new(t)
                    .flex_1()
                    .flex()
                    .items_center()
                    .justify_center()
                    .text_color(t.text_subtle)
                    .child("Pick numeric X and Y columns to draw a chart."),
            );
        }
    }

    // A click anywhere else closes an open menu.
    if app.csv.menu.is_some() {
        body = body.child(deferred(
            div()
                .id("csv-menu-backdrop")
                .absolute()
                .top_0()
                .left_0()
                .size_full()
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(|a: &mut JadeApp, _e: &MouseDownEvent, _w, cx| {
                        a.csv_close_menu();
                        cx.notify();
                    }),
                ),
        ));
    }
    crate::panels::code_view::focus_shell(app, cx, body.into_any_element())
}

type Pick = Box<dyn Fn(&mut JadeApp)>;

/// A dropdown: a Kumo outline button with the value and a chevron; open,
/// an overlay list hangs under it. The list is deferred so it paints over
/// the chart.
#[allow(clippy::too_many_arguments)]
fn dropdown(
    id: &'static str,
    caption: &'static str,
    value: String,
    open: bool,
    options: Vec<(String, bool, Pick)>,
    menu: CsvMenu,
    theme: &Theme,
    cx: &mut Context<JadeApp>,
) -> impl IntoElement {
    let t = &theme.kumo;
    let button = kumo::Button::new(id, format!("{caption}  {value}"))
        .size(kumo::Size::Xs)
        .variant(kumo::ButtonVariant::Outline)
        .icon_right("chevron-down")
        .active(open)
        .render(t)
        .on_mouse_down(
            MouseButton::Left,
            cx.listener(move |a: &mut JadeApp, _e: &MouseDownEvent, _w, cx| {
                cx.stop_propagation();
                a.csv_toggle_menu(menu);
                cx.notify();
            }),
        );

    let mut wrap = div().relative().child(button);
    if open {
        let mut list = kumo::Surface::overlay(t)
            .id(gpui::ElementId::Name(format!("{id}-menu").into()))
            .flex()
            .flex_col()
            .min_w(px(180.))
            .max_h(px(260.))
            .overflow_y_scroll()
            .py(scale::SPACE_1);
        for (i, (label, selected, pick)) in options.into_iter().enumerate() {
            let pick = Arc::new(pick);
            list = list.child(
                div()
                    .id(gpui::ElementId::Name(format!("{id}-opt-{i}").into()))
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(scale::SPACE_2)
                    .h(scale::H_6_5)
                    .px(scale::SPACE_2_5)
                    .cursor_pointer()
                    .text_color(if selected { t.text_brand } else { t.text_default })
                    .hover(|s| s.bg(t.fill_hover))
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(move |a: &mut JadeApp, _e: &MouseDownEvent, _w, cx| {
                            cx.stop_propagation();
                            pick(a);
                            cx.notify();
                        }),
                    )
                    .child(div().w(px(12.)).flex_none().child(if selected {
                        crate::assets::ui_icon("check", 12., kumo::pack(t.text_brand)).into_any_element()
                    } else {
                        div().into_any_element()
                    }))
                    .child(label),
            );
        }
        wrap = wrap.child(deferred(
            div()
                .id(gpui::ElementId::Name(format!("{id}-list").into()))
                .absolute()
                .top(px(24.))
                .left_0()
                .child(list),
        ));
    }
    wrap
}

fn column_options(
    table: &CsvTable,
    current: usize,
    numeric_only: bool,
    pick: impl Fn(&mut JadeApp, usize) + Clone + 'static,
) -> Vec<(String, bool, Pick)> {
    table
        .headers
        .iter()
        .enumerate()
        .filter(|(c, _)| !numeric_only || table.is_numeric(*c))
        .map(|(c, h)| {
            let pick = pick.clone();
            (h.clone(), c == current, Box::new(move |a: &mut JadeApp| pick(a, c)) as Pick)
        })
        .collect()
}

/// Above the card: the series toggles on the left, the group and chart
/// kind menus on the right.
fn toolbar(
    app: &JadeApp,
    table: &CsvTable,
    cfg: &CsvConfig,
    chart: Option<&ChartData>,
    theme: &Theme,
    cx: &mut Context<JadeApp>,
) -> impl IntoElement {
    let t = &theme.kumo;
    let mut legend = div().flex().flex_row().flex_wrap().items_center().gap(scale::SPACE_1).flex_1();
    if let Some(d) = chart {
        let any_hidden = !app.csv.hidden.is_empty();
        for (i, s) in d.series.iter().enumerate() {
            let color = theme.series[i % theme.series.len()];
            let shown = !app.csv.hidden.contains(&s.name);
            let name = if s.name.is_empty() { d.y_label.clone() } else { s.name.clone() };
            let key = s.name.clone();
            legend = legend.child(
                div()
                    .id(gpui::ElementId::Name(format!("csv-series-{i}").into()))
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(scale::SPACE_1_5)
                    .h(scale::H_5)
                    .px(scale::SPACE_2)
                    .cursor_pointer()
                    .hover(|s| s.bg(t.fill_hover))
                    .text_color(if shown { t.text_default } else { t.text_inactive })
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(move |a: &mut JadeApp, _e: &MouseDownEvent, _w, cx| {
                            a.csv_toggle_series(&key);
                            cx.notify();
                        }),
                    )
                    .child(
                        div()
                            .w(px(10.))
                            .h(px(10.))
                            .bg(rgb(color).alpha(if shown { 0.85 } else { 0.25 }))
                            .border_1()
                            .border_color(rgb(color).alpha(if shown { 1.0 } else { 0.4 })),
                    )
                    .child(name)
                    .child(div().text_color(t.text_subtle).font_family(crate::fonts::mono_family()).child(match d.kind {
                        // A histogram's legend counts samples (the sum of the
                        // count column), a line's counts points.
                        ChartKind::Bars => format!("{} samples", format_num(s.sum_y)),
                        ChartKind::Line => format!("{} points", s.points.len()),
                    })),
            );
        }
        if any_hidden {
            legend = legend.child(
                kumo::Button::new("csv-show-all", "show all")
                    .size(kumo::Size::Xs)
                    .variant(kumo::ButtonVariant::Ghost)
                    .render(t)
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(|a: &mut JadeApp, _e: &MouseDownEvent, _w, cx| {
                            a.csv_show_all_series();
                            cx.notify();
                        }),
                    ),
            );
        }
    }

    let group_value = match cfg.group {
        Some(g) => table.headers.get(g).cloned().unwrap_or_default(),
        None => "none".to_string(),
    };
    let mut group_opts: Vec<(String, bool, Pick)> =
        vec![("none".to_string(), cfg.group.is_none(), Box::new(|a: &mut JadeApp| a.csv_pick_group(None)))];
    group_opts.extend(column_options(table, cfg.group.unwrap_or(usize::MAX), false, |a, c| a.csv_pick_group(Some(c))));
    let kind_value = match cfg.kind {
        ChartKind::Bars => "bars",
        ChartKind::Line => "line",
    };
    let kind_opts: Vec<(String, bool, Pick)> = vec![
        ("bars".to_string(), cfg.kind == ChartKind::Bars, Box::new(|a: &mut JadeApp| a.csv_pick_kind(ChartKind::Bars))),
        ("line".to_string(), cfg.kind == ChartKind::Line, Box::new(|a: &mut JadeApp| a.csv_pick_kind(ChartKind::Line))),
    ];

    div()
        .flex()
        .flex_row()
        .items_center()
        .gap(scale::SPACE_2)
        .child(legend)
        .child(dropdown("csv-group", "Group", group_value, app.csv.menu == Some(CsvMenu::Group), group_opts, CsvMenu::Group, theme, cx))
        .child(dropdown("csv-kind", "Chart", kind_value.to_string(), app.csv.menu == Some(CsvMenu::Kind), kind_opts, CsvMenu::Kind, theme, cx))
}

/// The plot card: a header with the Y menu, the plot canvas filling the
/// rest, and a footer with the X menu, the scale menu, and the hint.
#[allow(clippy::too_many_arguments)]
fn chart_card(
    app: &JadeApp,
    table: &CsvTable,
    cfg: &CsvConfig,
    d: Arc<ChartData>,
    range: (f64, f64),
    ylim: (f64, f64),
    theme: &Theme,
    cx: &mut Context<JadeApp>,
) -> impl IntoElement {
    let t = &theme.kumo;
    let zoomed = app.csv.view.is_some();

    let y_value = table.headers.get(cfg.y).cloned().unwrap_or_default();
    let y_opts = column_options(table, cfg.y, true, |a, c| a.csv_pick_y(c));
    let header = div()
        .flex()
        .flex_row()
        .items_center()
        .justify_between()
        .px(scale::SPACE_2_5)
        .py(scale::SPACE_1_5)
        .border_b_1()
        .border_color(t.hairline)
        .child(dropdown("csv-y", "Y", y_value, app.csv.menu == Some(CsvMenu::Y), y_opts, CsvMenu::Y, theme, cx))
        .child(
            div()
                .text_color(t.text_subtle)
                .font_family(crate::fonts::mono_family())
                .child(format!("{} rows · {} columns", table.rows.len(), table.headers.len())),
        );

    let x_value = table.headers.get(cfg.x).cloned().unwrap_or_default();
    let x_opts = column_options(table, cfg.x, true, |a, c| a.csv_pick_x(c));
    let scale_opts: Vec<(String, bool, Pick)> = vec![
        ("linear".to_string(), !cfg.log_x, Box::new(|a: &mut JadeApp| a.csv_pick_log(false))),
        ("log".to_string(), cfg.log_x, Box::new(|a: &mut JadeApp| a.csv_pick_log(true))),
    ];
    let mut footer_mid = div()
        .flex()
        .flex_row()
        .items_center()
        .gap(scale::SPACE_2)
        .child(dropdown("csv-x", "X", x_value, app.csv.menu == Some(CsvMenu::X), x_opts, CsvMenu::X, theme, cx))
        .child(dropdown(
            "csv-scale",
            "Scale",
            if cfg.log_x { "log" } else { "linear" }.to_string(),
            app.csv.menu == Some(CsvMenu::Scale),
            scale_opts,
            CsvMenu::Scale,
            theme,
            cx,
        ));
    if zoomed {
        footer_mid = footer_mid.child(
            kumo::Button::new("csv-reset", "Reset zoom")
                .size(kumo::Size::Xs)
                .variant(kumo::ButtonVariant::Ghost)
                .render(t)
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(|a: &mut JadeApp, _e: &MouseDownEvent, _w, cx| {
                        a.csv_reset_view();
                        cx.notify();
                    }),
                ),
        );
    }
    let footer = div()
        .flex()
        .flex_row()
        .items_center()
        .justify_between()
        .px(scale::SPACE_2_5)
        .py(scale::SPACE_1_5)
        .border_t_1()
        .border_color(t.hairline)
        .child(div().w(px(120.)))
        .child(footer_mid)
        .child(
            div()
                .w(px(120.))
                .flex()
                .justify_end()
                .text_color(t.text_subtle)
                .child("wheel zooms · drag pans · ⌘⇧D text"),
        );

    // Ticks, in chart units.
    let y_ticks = linear_ticks(ylim.0, ylim.1, 5);
    let x_ticks = if d.log_x { log_ticks(range.0, range.1) } else { linear_ticks(range.0, range.1, 6) };
    let log_x = d.log_x;
    let show_x = move |v: f64| if log_x { format_num(10f64.powf(v)) } else { format_num(v) };
    let x_labels: Vec<(f64, String)> = x_ticks.iter().map(|&v| (v, show_x(v))).collect();
    let y_labels: Vec<(f64, String)> = y_ticks.iter().map(|&v| (v, format_num(v))).collect();

    let visible = app.csv_visible_series();
    let bounds = app.csv.bounds.clone();
    let hover_x = app.csv.hover_x;
    let mut plot = div()
        .id("csv-canvas")
        .relative()
        .flex_1()
        .min_h(px(120.))
        .cursor_crosshair()
        .on_mouse_move(cx.listener(|a: &mut JadeApp, e: &MouseMoveEvent, _w, cx| {
            let x = f32::from(e.position.x);
            a.csv.hover_x = Some(x);
            if a.csv.drag.is_some() && e.pressed_button == Some(MouseButton::Left) {
                a.csv_pan_to(x);
            } else {
                a.csv.drag = None;
            }
            cx.notify();
        }))
        .on_mouse_down(
            MouseButton::Left,
            cx.listener(|a: &mut JadeApp, e: &MouseDownEvent, _w, cx| {
                if e.click_count >= 2 {
                    a.csv_reset_view();
                    a.csv.drag = None;
                } else if let Some(view) = a.csv_view_range() {
                    a.csv.drag = Some((f32::from(e.position.x), view));
                }
                cx.notify();
            }),
        )
        .on_mouse_up(
            MouseButton::Left,
            cx.listener(|a: &mut JadeApp, _e: &MouseUpEvent, _w, cx| {
                a.csv.drag = None;
                cx.notify();
            }),
        )
        .on_scroll_wheel(cx.listener(|a: &mut JadeApp, e: &ScrollWheelEvent, _w, cx| {
            let dy = f32::from(e.delta.pixel_delta(px(16.)).y);
            if dy != 0.0 {
                let factor = (dy as f64 * 0.005).exp().clamp(0.5, 2.0);
                a.csv_zoom(f32::from(e.position.x), factor);
                cx.notify();
            }
        }))
        .child(plot_canvas(PlotSpec {
            data: d.clone(),
            visible,
            range,
            ylim,
            x_labels,
            y_labels,
            hover_x,
            bounds,
            palette: theme.series,
            grid: t.hairline,
            text: kumo::pack(t.text_subtle),
        }));

    if let Some((x, pts)) = app.csv_hover_points() {
        if !pts.is_empty() {
            let b = app.csv.bounds.lock().ok().and_then(|b| *b);
            let hx = app.csv.hover_x.unwrap_or(0.0);
            // Tooltip left edge relative to the plot element, which starts
            // one gutter left of the plot area.
            let left = b.map(|b| hx - b[0] + Y_GUTTER_W).unwrap_or(0.0);
            let flip = b.is_some_and(|b| hx - b[0] > b[2] * 0.6);
            let mut tip = kumo::Surface::overlay(t)
                .absolute()
                .top(scale::SPACE_2)
                .flex()
                .flex_col()
                .gap(px(2.))
                .px(scale::SPACE_2_5)
                .py(scale::SPACE_1_5)
                .font_family(crate::fonts::mono_family())
                .child(div().text_color(t.text_subtle).child(format!("{} {}", d.x_label, format_num(x))));
            for (i, px_, py) in &pts {
                let color = theme.series[i % theme.series.len()];
                let s = &d.series[*i];
                let label = if s.name.is_empty() { d.y_label.clone() } else { s.name.clone() };
                tip = tip.child(
                    div()
                        .flex()
                        .flex_row()
                        .items_center()
                        .gap(scale::SPACE_1_5)
                        .child(div().w(px(8.)).h(px(8.)).bg(rgb(color)))
                        .child(div().text_color(t.text_default).child(format!("{label}  {}", format_num(*py))))
                        .child(div().text_color(t.text_subtle).child(format!("at {}", format_num(*px_)))),
                );
            }
            let total_w = b.map(|b| b[2] + Y_GUTTER_W).unwrap_or(0.0);
            tip = if flip { tip.right(px(total_w - left + 12.0)) } else { tip.left(px(left + 12.0)) };
            plot = plot.child(deferred(tip));
        }
    }

    kumo::Card::new(t)
        .flex_1()
        .min_h(px(0.))
        .flex()
        .flex_col()
        .overflow_hidden()
        .child(header)
        .child(plot)
        .child(footer)
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

/// Bar widths in px for bucket edges at `xs` (sorted, px): each bar spans
/// to the next edge, the last reuses the previous width, and a lone point
/// gets `default_w` centered on itself. Nothing narrower than 2 px, so a
/// bar never vanishes.
pub fn bar_widths(xs: &[f32], default_w: f32) -> Vec<f32> {
    let n = xs.len();
    let mut out = Vec::with_capacity(n);
    for k in 0..n {
        let w = if k + 1 < n {
            xs[k + 1] - xs[k]
        } else if n >= 2 {
            xs[n - 1] - xs[n - 2]
        } else {
            default_w
        };
        out.push(w.max(2.0));
    }
    out
}

/// Everything one paint of the plot needs.
struct PlotSpec {
    data: Arc<ChartData>,
    visible: Vec<usize>,
    range: (f64, f64),
    ylim: (f64, f64),
    x_labels: Vec<(f64, String)>,
    y_labels: Vec<(f64, String)>,
    hover_x: Option<f32>,
    bounds: Arc<Mutex<Option<[f32; 4]>>>,
    palette: [u32; 5],
    grid: gpui::Rgba,
    text: u32,
}

const Y_GUTTER_W: f32 = 56.0;
const X_GUTTER_H: f32 = 20.0;
const TICK_FONT: f32 = 11.0;

/// The plot: a left gutter with the Y tick labels, a bottom gutter with
/// the X tick labels, grid lines at the ticks, then the series. Bars span
/// from each x to the next in the same series, as a histogram's buckets
/// do, and are translucent so overlapping series both show. Lines are
/// one stroked path per series. The hover crosshair comes last. The paint
/// records the plot area so mouse positions map back to data.
fn plot_canvas(spec: PlotSpec) -> impl IntoElement {
    canvas(
        move |_, _, _| {},
        move |bounds: Bounds<Pixels>, _, window: &mut gpui::Window, cx: &mut gpui::App| {
            let (bx, by) = (f32::from(bounds.origin.x), f32::from(bounds.origin.y));
            let (bw, bh) = (f32::from(bounds.size.width), f32::from(bounds.size.height));
            let (ox, oy) = (bx + Y_GUTTER_W, by + 6.0);
            let (w, h) = (bw - Y_GUTTER_W - 8.0, bh - X_GUTTER_H - 6.0);
            if w <= 4.0 || h <= 4.0 {
                return;
            }
            if let Ok(mut b) = spec.bounds.lock() {
                *b = Some([ox, oy, w, h]);
            }
            let d = &spec.data;
            let (range, ylim) = (spec.range, spec.ylim);
            let xr = (range.1 - range.0).abs().max(1e-12);
            let yr = (ylim.1 - ylim.0).abs().max(1e-12);
            let map_x = |x: f64| ox + ((x - range.0) / xr) as f32 * w;
            let map_y = |y: f64| oy + h - ((y - ylim.0) / yr) as f32 * h;

            // Text runs for the tick labels, in the mono face.
            let mut font = window.text_style().font();
            font.family = crate::fonts::mono_family().into();
            let text_color: gpui::Hsla = rgb(spec.text).into();
            let run = |len: usize| gpui::TextRun {
                len,
                font: font.clone(),
                color: text_color,
                background_color: None,
                underline: None,
                strikethrough: None,
            };
            let line_h = px(TICK_FONT + 3.0);
            let paint_label = |window: &mut gpui::Window, cx: &mut gpui::App, text: &str, x: f32, y: f32, right_align: bool| {
                let shaped = window.text_system().shape_line(text.to_string().into(), px(TICK_FONT), &[run(text.len())], None);
                let lw = f32::from(shaped.width);
                let x0 = if right_align { x - lw } else { x - lw / 2.0 };
                let _ = shaped.paint(point(px(x0), px(y)), line_h, gpui::TextAlign::Left, None, window, cx);
            };

            // Grid and Y labels.
            for (v, label) in &spec.y_labels {
                let y = map_y(*v);
                if y < oy - 1.0 || y > oy + h + 1.0 {
                    continue;
                }
                window.paint_quad(fill(Bounds { origin: point(px(ox), px(y)), size: size(px(w), px(1.)) }, spec.grid));
                paint_label(window, cx, label, ox - 8.0, y - f32::from(line_h) / 2.0, true);
            }
            // X ticks and labels.
            for (v, label) in &spec.x_labels {
                let x = map_x(*v);
                if x < ox - 1.0 || x > ox + w + 1.0 {
                    continue;
                }
                window.paint_quad(fill(Bounds { origin: point(px(x), px(oy)), size: size(px(1.), px(h)) }, spec.grid.alpha(0.5)));
                paint_label(window, cx, label, x, oy + h + 4.0, false);
            }
            // Axes.
            window.paint_quad(fill(Bounds { origin: point(px(ox), px(oy + h)), size: size(px(w), px(1.)) }, spec.grid.alpha(1.0)));
            window.paint_quad(fill(Bounds { origin: point(px(ox), px(oy)), size: size(px(1.), px(h)) }, spec.grid.alpha(1.0)));

            let base_y = map_y(ylim.0.max(0.0).min(ylim.1));
            let margin = xr * 0.02;
            for &si in &spec.visible {
                let s = &d.series[si];
                let color = rgb(spec.palette[si % spec.palette.len()]);
                let a = s.points.partition_point(|p| p.0 < range.0 - margin);
                let b = s.points.partition_point(|p| p.0 <= range.1 + margin);
                match d.kind {
                    ChartKind::Line => {
                        let a2 = a.saturating_sub(1);
                        let b2 = (b + 1).min(s.points.len());
                        let seg = &s.points[a2..b2];
                        if seg.len() < 2 {
                            continue;
                        }
                        let mut pb = PathBuilder::stroke(px(1.5));
                        pb.move_to(point(px(map_x(seg[0].0).clamp(ox, ox + w)), px(map_y(seg[0].1))));
                        for &(x, y) in &seg[1..] {
                            pb.line_to(point(px(map_x(x).clamp(ox, ox + w)), px(map_y(y))));
                        }
                        if let Ok(path) = pb.build() {
                            window.paint_path(path, color);
                        }
                    }
                    ChartKind::Bars => {
                        // Each bar spans to the next point of its series; see
                        // `bar_widths` for the last bar and single points.
                        let xs: Vec<f32> = s.points[a..b].iter().map(|p| map_x(p.0)).collect();
                        let widths = bar_widths(&xs, w * 0.03);
                        for (k, &bw) in widths.iter().enumerate() {
                            let (_, y) = s.points[a + k];
                            let x0 = if xs.len() == 1 { xs[0] - bw / 2.0 } else { xs[k] };
                            let gap = if bw > 3.0 { 1.0 } else { 0.0 };
                            let left = x0.max(ox);
                            let right = (x0 + bw - gap).min(ox + w);
                            if right <= left {
                                continue;
                            }
                            let top = map_y(y);
                            let (y0, y1) = if top < base_y { (top, base_y) } else { (base_y, top) };
                            let hgt = (y1 - y0).max(1.0);
                            window.paint_quad(fill(
                                Bounds { origin: point(px(left), px(y0)), size: size(px(right - left), px(hgt)) },
                                color.alpha(0.55),
                            ));
                            window.paint_quad(fill(
                                Bounds { origin: point(px(left), px(y0)), size: size(px(right - left), px(1.)) },
                                color,
                            ));
                        }
                    }
                }
            }
            if let Some(hx) = spec.hover_x {
                if hx >= ox && hx <= ox + w {
                    window.paint_quad(fill(
                        Bounds { origin: point(px(hx), px(oy)), size: size(px(1.), px(h)) },
                        rgb(spec.text).alpha(0.6),
                    ));
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
        assert_eq!(d.series[0].sum_y, 685.0 + 2651.0 + 3.0, "the legend's sample count");
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
    fn nearest_point_and_visible_y_limits() {
        let s = SeriesData { name: "s".into(), points: vec![(1.0, 10.0), (2.0, 20.0), (4.0, 40.0)], sum_y: 70.0 };
        assert_eq!(s.nearest(0.0), Some((1.0, 10.0)));
        assert_eq!(s.nearest(2.9), Some((2.0, 20.0)));
        assert_eq!(s.nearest(3.1), Some((4.0, 40.0)));
        assert_eq!(s.nearest(9.0), Some((4.0, 40.0)));
        assert_eq!(SeriesData { name: "e".into(), points: vec![], sum_y: 0.0 }.nearest(1.0), None);

        let d = ChartData {
            series: vec![s],
            x_min: 1.0,
            x_max: 4.0,
            y_min: 0.0,
            y_max: 40.0,
            x_label: "x".into(),
            y_label: "y".into(),
            log_x: false,
            kind: ChartKind::Line,
            rows: 3,
        };
        assert_eq!(d.y_limits((1.0, 4.0)), (0.0, 40.0));
        assert_eq!(d.y_limits((1.5, 2.5)), (0.0, 20.0), "refits to the visible points");
        assert_eq!(d.y_limits((2.5, 3.5)), (0.0, 40.0), "nothing visible falls back to the full limits");
    }

    #[test]
    fn zoom_and_pan_stay_inside_the_full_range() {
        let full = (0.0, 100.0);
        let z = zoom_range(full, full, 50.0, 0.5);
        assert_eq!(z, (25.0, 75.0));
        let z2 = zoom_range(z, full, 25.0, 0.5);
        assert_eq!(z2, (25.0, 50.0), "zooms around the cursor at the left edge");
        assert_eq!(zoom_range(z, full, 50.0, 4.0), full, "zooming out clamps to the full range");
        let tiny = zoom_range(full, full, 50.0, 1e-9);
        assert!((tiny.1 - tiny.0 - 100.0 * MIN_VIEW_SHARE).abs() < 1e-9, "never narrower than the floor");
        assert_eq!(pan_range(z, full, 10.0), (35.0, 85.0));
        assert_eq!(pan_range(z, full, 90.0), (50.0, 100.0), "panning stops at the edge");
        assert_eq!(pan_range(z, full, -90.0), (0.0, 50.0));
    }

    #[test]
    fn ticks_are_round_and_inside_the_range() {
        assert_eq!(linear_ticks(0.0, 24300.0, 5), vec![0.0, 5000.0, 10000.0, 15000.0, 20000.0]);
        assert_eq!(linear_ticks(0.3, 2.2, 5), vec![0.5, 1.0, 1.5, 2.0]);
        assert_eq!(linear_ticks(5.0, 5.0, 5), vec![5.0]);
        // log10 of 40 ns to 4.19 ms: decades only, over three of them.
        let t = log_ticks(40f64.log10(), 4.19e6f64.log10());
        assert_eq!(t.len(), 5, "{t:?}");
        assert!((t[0] - 2.0).abs() < 1e-9 && (t[4] - 6.0).abs() < 1e-9);
        // Under three decades: the 2 and 5 marks come in.
        let f = log_ticks(2.0, 3.0);
        assert_eq!(f.len(), 4, "{f:?}"); // 100, 200, 500, 1000
    }

    #[test]
    fn hidden_series_leave_the_y_fit() {
        let d = ChartData {
            series: vec![
                SeriesData { name: "big".into(), points: vec![(1.0, 1000.0)], sum_y: 1000.0 },
                SeriesData { name: "small".into(), points: vec![(1.0, 10.0)], sum_y: 10.0 },
            ],
            x_min: 1.0, x_max: 1.0, y_min: 0.0, y_max: 1000.0,
            x_label: "x".into(), y_label: "y".into(), log_x: false, kind: ChartKind::Bars, rows: 2,
        };
        assert_eq!(d.y_limits_of((0.0, 2.0), &[0, 1]), (0.0, 1000.0));
        assert_eq!(d.y_limits_of((0.0, 2.0), &[1]), (0.0, 10.0), "only the shown series sets the scale");
        assert_eq!(d.y_limits_of((0.0, 2.0), &[]), (0.0, 1000.0), "nothing shown falls back");
    }

    #[test]
    fn bar_widths_never_vanish() {
        assert_eq!(bar_widths(&[10.0, 20.0, 35.0], 30.0), vec![10.0, 15.0, 15.0], "last reuses the previous width");
        assert_eq!(bar_widths(&[10.0], 30.0), vec![30.0], "a lone point gets the default");
        assert_eq!(bar_widths(&[10.0, 10.5], 30.0), vec![2.0, 2.0], "a hairline gap still draws");
        assert!(bar_widths(&[], 30.0).is_empty());
    }

    #[test]
    fn numbers_format_short() {
        assert_eq!(format_num(2651.0), "2651");
        assert_eq!(format_num(12345.0), "12.3k");
        assert_eq!(format_num(2.5e6), "2.50M");
        assert_eq!(format_num(0.5), "0.50");
    }
}
