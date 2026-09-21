//! Instruments trace analysis: open a `.trace` bundle and show what it says.
//!
//! An Instruments recording is a directory bundle. Its content is only
//! readable through `xcrun xctrace export`, which prints XML tables. The
//! export runs off the UI thread; the parsed summary comes back through
//! `AppEvent::Trace` and the popup in `panels::trace_popup` renders it.
//!
//! First pass: the CPU Counters template. Its per-process table gives one
//! row per 10 ms with four cycle buckets. A GPU counters trace only lists
//! its instruments and counter names, because its sample table is too large
//! to export interactively.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;

use crate::app::{AppEvent, JadeApp, ToastKind};

/// Names of the four CPU cycle buckets, in the order the export lists them.
/// The order was established empirically on an M4: a throughput loop fills
/// column 0, a pointer chase fills column 1, an unpredictable branch fills
/// column 2, and the remainder of that branch run lands in column 3.
pub const CPU_BUCKETS: [&str; 4] = [
    "Useful work",
    "Processing stall",
    "Discarded work",
    "Delivery stall",
];

/// What kind of recording the bundle holds.
#[derive(Debug, Clone, PartialEq)]
pub enum TraceKind {
    /// CPU Counters template: four cycle buckets per window.
    CpuCounters(CpuBottleneck),
    /// Metal GPU Counters: the counter names only.
    GpuCounters { counters: Vec<String> },
    /// Something else: the instrument list is all we show.
    Other,
}

/// The CPU Counters per-process aggregate.
#[derive(Debug, Clone, PartialEq)]
pub struct CpuBottleneck {
    /// One entry per 10 ms window: the four bucket shares, each 0..=1.
    pub windows: Vec<[f32; 4]>,
    /// Mean share per bucket over every window with data.
    pub means: [f32; 4],
    /// Window length in seconds (10 ms in every trace seen).
    pub window_s: f32,
    /// Totals of the raw performance events, when the sample table parsed.
    pub events: Option<CpuEvents>,
}

impl CpuBottleneck {
    /// No bucket table: a custom event set records samples only.
    pub fn empty() -> Self {
        Self {
            windows: Vec::new(),
            means: [0.0; 4],
            window_s: 0.01,
            events: None,
        }
    }
}

/// The raw performance events of the bottleneck counting mode, in column
/// order. Identified on an M4 by which benchmark inflated each column: a
/// throughput loop, a pointer chase, and an unpredictable branch. Columns
/// 8 to 11 are always zero.
pub const BOTTLENECK_EVENTS: [&str; 8] = [
    "Cycles",
    "Instructions",
    "Retired uops",
    "Active cycles",
    "Mapped uops",
    "Front-end bubble slots",
    "Branch mispredictions",
    "Back-end stall cycles",
];

/// Totals of the raw per-sample counters over the whole recording, summed
/// across threads, plus the ratios a reader wants first.
#[derive(Debug, Clone, PartialEq)]
pub struct CpuEvents {
    /// The counting mode named in the table of contents.
    pub mode: String,
    /// Column names, as many as the mode defines; extra columns are `pmc N`.
    pub names: Vec<String>,
    /// Total count per column.
    pub totals: Vec<f64>,
    /// `(label, value)` pairs derived from the totals, e.g. instructions
    /// per cycle. Empty for an unknown mode.
    pub ratios: Vec<(String, String)>,
}

/// Readable names for the event mnemonics a custom set is likely to hold.
/// Anything else shows its mnemonic.
const EVENT_LABELS: [(&str, &str); 14] = [
    ("FIXED_CYCLES", "Cycles"),
    ("FIXED_INSTRUCTIONS", "Instructions"),
    ("INST_ALL", "Instructions"),
    ("CORE_ACTIVE_CYCLE", "Active cycles"),
    ("INST_BRANCH", "Branch instructions"),
    ("BRANCH_MISPRED_NONSPEC", "Branch mispredictions"),
    ("L1D_CACHE_MISS_LD", "L1 data misses, loads"),
    ("L1D_CACHE_MISS_ST", "L1 data misses, stores"),
    ("LD_UNIT_UOP", "Load uops"),
    ("ST_UNIT_UOP", "Store uops"),
    ("L1D_TLB_MISS", "L1 data TLB misses"),
    ("L2_TLB_MISS_DATA", "L2 TLB misses, data"),
    ("MMU_TABLE_WALK_DATA", "Page table walks, data"),
    ("L1D_CACHE_WRITEBACK", "L1 writebacks"),
];

impl CpuEvents {
    /// Build totals and ratios from the raw column sums. `mnemonics` are the
    /// column names the table of contents lists for a custom event set;
    /// empty for the bottleneck preset, whose columns are fixed.
    pub fn from_totals(mode: &str, mnemonics: &[String], totals: Vec<f64>) -> Self {
        let preset = mnemonics.is_empty() && mode.starts_with("bottleneck");
        let names: Vec<String> = (0..totals.len())
            .map(|i| {
                if preset && i < BOTTLENECK_EVENTS.len() {
                    BOTTLENECK_EVENTS[i].to_string()
                } else if let Some(m) = mnemonics.get(i) {
                    EVENT_LABELS
                        .iter()
                        .find(|(k, _)| k == m)
                        .map(|(_, v)| v.to_string())
                        .unwrap_or_else(|| m.clone())
                } else {
                    format!("pmc {i}")
                }
            })
            .collect();
        let mut ratios = Vec::new();
        if preset && totals.len() >= 8 {
            let (cyc, ins, ret, _act, map, bub, mis, stall) = (
                totals[0], totals[1], totals[2], totals[3], totals[4], totals[5], totals[6], totals[7],
            );
            if cyc > 0.0 {
                ratios.push(("Instructions per cycle".into(), format!("{:.2}", ins / cyc)));
                ratios.push(("Back-end stall".into(), format!("{:.1}% of cycles", stall / cyc * 100.0)));
                ratios.push(("Front-end bubbles".into(), format!("{:.2} slots per cycle", bub / cyc)));
            }
            if ins > 0.0 {
                ratios.push(("Branch mispredictions".into(), format!("{:.2} per 1k instructions", mis / ins * 1000.0)));
            }
            if map > 0.0 {
                ratios.push(("Wasted uops".into(), format!("{:.1}% of mapped", (map - ret).max(0.0) / map * 100.0)));
            }
        } else {
            // A custom set: every ratio whose inputs are present.
            let get = |m: &str| mnemonics.iter().position(|x| x == m).and_then(|i| totals.get(i)).copied();
            let sum = |a: &str, b: &str| match (get(a), get(b)) {
                (Some(x), Some(y)) => Some(x + y),
                (Some(x), None) | (None, Some(x)) => Some(x),
                _ => None,
            };
            let pct = |num: f64, den: f64| format!("{:.2}%", num / den * 100.0);
            let accesses = sum("LD_UNIT_UOP", "ST_UNIT_UOP").filter(|v| *v > 0.0);
            if let (Some(miss), Some(acc)) = (sum("L1D_CACHE_MISS_LD", "L1D_CACHE_MISS_ST"), accesses) {
                ratios.push(("L1 data miss rate".into(), pct(miss, acc)));
            }
            if let (Some(miss), Some(ld)) = (get("L1D_CACHE_MISS_LD"), get("LD_UNIT_UOP").filter(|v| *v > 0.0)) {
                ratios.push(("L1 load miss rate".into(), pct(miss, ld)));
            }
            if let (Some(miss), Some(acc)) = (get("L1D_TLB_MISS"), accesses) {
                ratios.push(("L1 data TLB miss rate".into(), pct(miss, acc)));
            }
            if let (Some(miss), Some(acc)) = (get("L2_TLB_MISS_DATA"), accesses) {
                ratios.push(("L2 TLB miss rate".into(), pct(miss, acc)));
            }
            if let (Some(mis), Some(br)) = (get("BRANCH_MISPRED_NONSPEC"), get("INST_BRANCH").filter(|v| *v > 0.0)) {
                ratios.push(("Branch mispredict rate".into(), pct(mis, br)));
            }
            if let (Some(ins), Some(cyc)) = (
                get("FIXED_INSTRUCTIONS").or_else(|| get("INST_ALL")),
                get("FIXED_CYCLES").filter(|v| *v > 0.0),
            ) {
                ratios.push(("Instructions per cycle".into(), format!("{:.2}", ins / cyc)));
            }
        }
        Self {
            mode: mode.to_string(),
            names,
            totals,
            ratios,
        }
    }
}

/// The `pmc-events` attribute of the CPU sample table: the event mnemonics
/// of a custom set, in column order. Empty for the bottleneck preset.
pub fn parse_pmc_names(toc: &str) -> Vec<String> {
    let Some(i) = toc.find("schema=\"kdebug-counters-with-time-sample\"") else {
        return Vec::new();
    };
    // The attribute may sit before or after the schema attribute in the tag.
    let start = toc[..i].rfind('<').unwrap_or(0);
    let end = toc[i..].find('>').map(|e| i + e).unwrap_or(toc.len());
    let tag = &toc[start..end];
    let Some(a) = tag.find("pmc-events=\"") else { return Vec::new() };
    let rest = &tag[a + "pmc-events=\"".len()..];
    let Some(e) = rest.find('"') else { return Vec::new() };
    unescape(&rest[..e])
        .split('"')
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string())
        .collect()
}

/// A count with a unit prefix: 12345678 → "12.3M".
pub fn format_count(v: f64) -> String {
    let a = v.abs();
    if a >= 1e12 {
        format!("{:.2}T", v / 1e12)
    } else if a >= 1e9 {
        format!("{:.2}G", v / 1e9)
    } else if a >= 1e6 {
        format!("{:.1}M", v / 1e6)
    } else if a >= 1e3 {
        format!("{:.1}k", v / 1e3)
    } else {
        format!("{v:.0}")
    }
}

/// The parsed summary of one `.trace` bundle.
#[derive(Debug, Clone, PartialEq)]
pub struct TraceAnalysis {
    pub path: PathBuf,
    /// The recording template's name, when the table of contents has one.
    pub template: Option<String>,
    /// Instrument names from the table of contents, in order of appearance.
    /// A stock template lists none; a user-saved template lists each one.
    pub instruments: Vec<String>,
    pub kind: TraceKind,
}

impl TraceAnalysis {
    /// One line for the header: template, then instruments.
    pub fn describe(&self) -> String {
        let mut parts: Vec<String> = Vec::new();
        if let Some(t) = &self.template {
            parts.push(t.clone());
        }
        parts.extend(self.instruments.iter().cloned());
        if parts.is_empty() {
            "unknown template".to_string()
        } else {
            parts.join(" · ")
        }
    }
}

/// The RUNTIME panel's TRACE section: which bundle, and its result once the
/// export lands (`None` while xctrace runs).
#[derive(Debug, Clone)]
pub struct TraceState {
    pub path: PathBuf,
    pub result: Option<Result<Arc<TraceAnalysis>, String>>,
}

/// True for an Instruments bundle: a directory whose name ends in `.trace`.
pub fn is_trace_bundle(path: &Path) -> bool {
    path.extension().and_then(|e| e.to_str()) == Some("trace") && path.is_dir()
}

/// Run the exports and parse them. Blocking: call from `spawn_blocking`.
pub fn analyze(path: &Path) -> Result<TraceAnalysis, String> {
    let toc = xctrace_export(path, &["--toc"])?;
    let instruments = parse_instruments(&toc);
    let template = parse_template_name(&toc);
    // The tables present decide the kind: a stock template lists no
    // instruments, so the names alone cannot.
    let has_buckets = has_table(&toc, "CounterMetricAggregatedForProcess");
    let has_samples = has_table(&toc, "kdebug-counters-with-time-sample");
    let kind = if has_buckets || has_samples {
        // The bottleneck preset writes both tables; a custom event set
        // writes only the samples. Either half alone is still a result.
        let mut cpu = if has_buckets {
            let xml = xctrace_export(path, &["--xpath", &table_xpath("CounterMetricAggregatedForProcess")])?;
            parse_cpu_bottleneck(&xml).unwrap_or_else(|_| CpuBottleneck::empty())
        } else {
            CpuBottleneck::empty()
        };
        if has_samples {
            let mode = parse_counting_mode(&toc).unwrap_or_else(|| "custom".to_string());
            let names = parse_pmc_names(&toc);
            let raw = xctrace_export(path, &["--xpath", &table_xpath("kdebug-counters-with-time-sample")])?;
            let totals = parse_pmc_totals(&raw);
            if !totals.is_empty() {
                cpu.events = Some(CpuEvents::from_totals(&mode, &names, totals));
            }
        }
        if cpu.windows.is_empty() && cpu.events.is_none() {
            return Err("no CPU counter samples in this trace".to_string());
        }
        TraceKind::CpuCounters(cpu)
    } else if has_table(&toc, "gpu-counter-info") {
        let xml = xctrace_export(path, &["--xpath", &table_xpath("gpu-counter-info")])?;
        TraceKind::GpuCounters {
            counters: parse_gpu_counter_names(&xml),
        }
    } else {
        TraceKind::Other
    };
    Ok(TraceAnalysis {
        path: path.to_path_buf(),
        template,
        instruments,
        kind,
    })
}

/// `<template-name>…</template-name>` from a `--toc` export.
pub fn parse_template_name(toc: &str) -> Option<String> {
    let start = toc.find("<template-name>")? + "<template-name>".len();
    let end = toc[start..].find("</template-name>")?;
    let name = unescape(toc[start..start + end].trim());
    (!name.is_empty()).then_some(name)
}

/// True when the table of contents declares a table with this schema.
pub fn has_table(toc: &str, schema: &str) -> bool {
    toc.contains(&format!("schema=\"{schema}\""))
}

/// The `counting-mode` attribute of the CPU sample table, e.g.
/// `bottleneck bottlenecks`.
pub fn parse_counting_mode(toc: &str) -> Option<String> {
    let i = toc.find("counting-mode=\"")? + "counting-mode=\"".len();
    let end = toc[i..].find('"')?;
    Some(toc[i..i + end].to_string())
}

/// The longest gap between two samples that still counts as adjacent. The
/// sampler fires every 1 ms for a running thread.
const PMC_ADJACENT_NS: u64 = 2_500_000;

/// Sum the raw counter deltas of the CPU sample table. Each row is one
/// thread's view of its core's cumulative counters at one sample. The
/// counters belong to the core, so a delta is that thread's work only
/// between adjacent samples on the same core: a core change, a gap longer
/// than [`PMC_ADJACENT_NS`], or a counter that went backwards resets the
/// baseline instead of adding a delta. Rows without counters (blocked
/// threads) are skipped.
pub fn parse_pmc_totals(xml: &str) -> Vec<f64> {
    // thread → (sample time, core, counters)
    let mut last: std::collections::HashMap<String, (u64, String, Vec<u64>)> = std::collections::HashMap::new();
    let mut totals: Vec<f64> = Vec::new();
    for cells in rows(xml) {
        let mut thread = String::new();
        let mut core = String::new();
        let mut time: u64 = 0;
        let mut counters: Option<Vec<u64>> = None;
        for (tag, text) in &cells {
            match tag.as_str() {
                "thread" => thread = text.clone(),
                "core" => core = text.clone(),
                "sample-time" => time = text.parse().unwrap_or(0),
                "pmc-events" => {
                    let v: Vec<u64> = text.split_whitespace().filter_map(|n| n.parse().ok()).collect();
                    if !v.is_empty() {
                        counters = Some(v);
                    }
                }
                _ => {}
            }
        }
        let Some(now) = counters else { continue };
        if totals.len() < now.len() {
            totals.resize(now.len(), 0.0);
        }
        if let Some((t0, core0, prev)) = last.get(&thread) {
            let adjacent = time >= *t0 && time - t0 <= PMC_ADJACENT_NS;
            let monotonic = prev.len() == now.len() && now.iter().zip(prev).all(|(n, p)| n >= p);
            if adjacent && *core0 == core && monotonic {
                for (i, (n, p)) in now.iter().zip(prev).enumerate() {
                    totals[i] += (n - p) as f64;
                }
            }
        }
        last.insert(thread, (time, core, now));
    }
    totals
}

fn table_xpath(schema: &str) -> String {
    format!("/trace-toc/run[@number='1']/data/table[@schema='{schema}']")
}

fn xctrace_export(path: &Path, args: &[&str]) -> Result<String, String> {
    let out = Command::new("xcrun")
        .arg("xctrace")
        .arg("export")
        .arg("--input")
        .arg(path)
        .args(args)
        .output()
        .map_err(|e| format!("could not run xcrun xctrace: {e}"))?;
    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr);
        let line = err.lines().find(|l| !l.trim().is_empty()).unwrap_or("export failed");
        return Err(format!("xctrace export: {line}"));
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// Instrument names from a `--toc` export, deduplicated in first-seen order.
pub fn parse_instruments(toc: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for (i, _) in toc.match_indices("<instrument name=\"") {
        let rest = &toc[i + "<instrument name=\"".len()..];
        if let Some(end) = rest.find('"') {
            let name = unescape(&rest[..end]);
            if !out.contains(&name) {
                out.push(name);
            }
        }
    }
    out
}

/// Every `<row>` of the export as a list of `(tag, id, text)` cells with
/// `ref` back-references resolved. xctrace emits a cell once with an `id`
/// and repeats it later as `<tag ref="id"/>`.
fn rows(xml: &str) -> Vec<Vec<(String, String)>> {
    let mut ids: std::collections::HashMap<String, String> = std::collections::HashMap::new();
    let mut out = Vec::new();
    let mut rest = xml;
    while let Some(start) = rest.find("<row>") {
        let body = &rest[start + 5..];
        let Some(end) = body.find("</row>") else { break };
        let row = &body[..end];
        rest = &body[end + 6..];
        let mut cells = Vec::new();
        let mut r = row;
        while let Some(lt) = r.find('<') {
            let tag_body = &r[lt + 1..];
            let Some(gt) = tag_body.find('>') else { break };
            let head = &tag_body[..gt];
            let self_closing = head.ends_with('/');
            let head = head.trim_end_matches('/');
            let tag = head.split_whitespace().next().unwrap_or("").to_string();
            if self_closing {
                let text = attr(head, "ref").and_then(|id| ids.get(&id).cloned()).unwrap_or_default();
                cells.push((tag, text));
                r = &tag_body[gt + 1..];
                continue;
            }
            let after = &tag_body[gt + 1..];
            let close = format!("</{tag}>");
            let Some(ce) = after.find(&close) else { break };
            let text = unescape(after[..ce].trim());
            if let Some(id) = attr(head, "id") {
                ids.insert(id, text.clone());
            }
            cells.push((tag, text));
            r = &after[ce + close.len()..];
        }
        out.push(cells);
    }
    out
}

fn attr(head: &str, name: &str) -> Option<String> {
    let key = format!("{name}=\"");
    let i = head.find(&key)?;
    let v = &head[i + key.len()..];
    let end = v.find('"')?;
    Some(v[..end].to_string())
}

fn unescape(s: &str) -> String {
    s.replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&apos;", "'")
        .replace("&amp;", "&")
}

/// The CPU Counters per-process table: each row holds a four-value
/// `uint64-array` of cycles per bucket in a 10 ms window.
pub fn parse_cpu_bottleneck(xml: &str) -> Result<CpuBottleneck, String> {
    let mut windows: Vec<[f32; 4]> = Vec::new();
    let mut window_s = 0.01;
    for cells in rows(xml) {
        let mut vals: Option<[f32; 4]> = None;
        for (tag, text) in &cells {
            if tag == "duration" {
                if let Ok(ns) = text.parse::<f64>() {
                    if ns > 0.0 {
                        window_s = (ns / 1e9) as f32;
                    }
                }
            }
            if tag == "uint64-array" {
                let nums: Vec<f32> = text.split_whitespace().filter_map(|n| n.parse().ok()).collect();
                if nums.len() == 4 {
                    vals = Some([nums[0], nums[1], nums[2], nums[3]]);
                }
            }
        }
        let Some(v) = vals else { continue };
        let sum: f32 = v.iter().sum();
        if sum <= 0.0 {
            continue;
        }
        windows.push([v[0] / sum, v[1] / sum, v[2] / sum, v[3] / sum]);
    }
    if windows.is_empty() {
        return Err("no CPU counter windows in this trace".to_string());
    }
    let n = windows.len() as f32;
    let mut means = [0.0f32; 4];
    for w in &windows {
        for i in 0..4 {
            means[i] += w[i] / n;
        }
    }
    Ok(CpuBottleneck {
        windows,
        means,
        window_s,
        events: None,
    })
}

/// Counter names from a `gpu-counter-info` export: the third cell of each
/// row is the name.
pub fn parse_gpu_counter_names(xml: &str) -> Vec<String> {
    rows(xml)
        .into_iter()
        .filter_map(|cells| cells.get(2).map(|(_, t)| t.clone()))
        .filter(|n| !n.is_empty())
        .collect()
}

impl JadeApp {
    /// Show a bundle in the RUNTIME panel and start its export. Opens the
    /// sidebar when it is closed, so the result has somewhere to land.
    pub fn open_trace(&mut self, path: PathBuf) {
        self.trace_gen += 1;
        let generation = self.trace_gen;
        self.trace = Some(TraceState {
            path: path.clone(),
            result: None,
        });
        if !self.runtime_visible || self.sidebar_closing {
            self.sidebar_anim_gen += 1;
            self.sidebar_closing = false;
            self.runtime_visible = true;
        }
        self.status_line(&format!("[jade] Reading {}", path.display()));
        let tx = self.app_tx.clone();
        self.runtime.spawn(async move {
            let result = tokio::task::spawn_blocking(move || analyze(&path))
                .await
                .unwrap_or_else(|e| Err(format!("trace analysis panicked: {e}")))
                .map(Arc::new);
            let _ = tx.send(AppEvent::Trace { generation, result });
        });
    }

    /// The `AppEvent::Trace` arm: keep the newest export only.
    pub fn on_trace_ready(&mut self, generation: u64, result: Result<Arc<TraceAnalysis>, String>) {
        if generation != self.trace_gen {
            return;
        }
        if self.trace.is_none() {
            return;
        }
        if let Err(e) = &result {
            self.push_toast(ToastKind::Error, format!("Trace: {e}"));
        }
        if let Some(state) = &mut self.trace {
            state.result = Some(result);
        }
    }

    /// Drop the TRACE section (the × in its header).
    pub fn clear_trace(&mut self) {
        self.trace = None;
        self.trace_gen += 1; // drop an export still in flight
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const TOC: &str = r#"<trace-toc><run number="1"><info><summary><template-name>CPU Counters</template-name></summary><target>
<instrument name="CPU Counters" id="1"/>
<instrument name="Time Profiler" id="2"/>
<instrument name="CPU Counters" id="3"/></target></info>
<data><table schema="CounterMetricAggregatedForProcess" swift-table="x"/></data></run></trace-toc>"#;

    const CPU_XML: &str = r#"<trace-query-result><node xpath="x"><schema name="CounterMetricAggregatedForProcess"></schema>
<row><start-time id="1" fmt="00:00.800.000">800000000</start-time><duration id="2" fmt="10.00 ms">10000000</duration><process id="3" fmt="bench">bench</process><uint64-array id="4" fmt="x">1000 3000 0 0</uint64-array></row>
<row><start-time id="5" fmt="00:00.810.000">810000000</start-time><duration ref="2"/><process ref="3"/><uint64-array id="6" fmt="x">0 0 0 0</uint64-array></row>
<row><start-time id="7" fmt="00:00.820.000">820000000</start-time><duration ref="2"/><process ref="3"/><uint64-array id="8" fmt="x">2000 2000 0 0</uint64-array></row>
<row><start-time id="9" fmt="00:00.830.000">830000000</start-time><duration ref="2"/><process ref="3"/><uint64-array ref="8"/></row>
</node></trace-query-result>"#;

    const GPU_XML: &str = r#"<node><row><uint32 id="1">0</uint32><uint32 id="2">0</uint32><string id="3">RT Unit Active</string><uint64 id="4">100</uint64></row>
<row><uint32 ref="1"/><uint32 id="5">1</uint32><string id="6">VS Occupancy</string><uint64 ref="4"/></row></node>"#;

    #[test]
    fn instruments_are_deduplicated_in_order() {
        assert_eq!(parse_instruments(TOC), vec!["CPU Counters", "Time Profiler"]);
    }

    #[test]
    fn template_name_and_tables_come_from_the_toc() {
        assert_eq!(parse_template_name(TOC).as_deref(), Some("CPU Counters"));
        assert!(has_table(TOC, "CounterMetricAggregatedForProcess"));
        assert!(!has_table(TOC, "gpu-counter-info"));
        assert_eq!(parse_template_name("<x/>"), None);
    }

    #[test]
    fn describe_joins_template_and_instruments() {
        let a = TraceAnalysis {
            path: PathBuf::from("x.trace"),
            template: Some("CPU Counters".into()),
            instruments: vec!["GPU".into()],
            kind: TraceKind::Other,
        };
        assert_eq!(a.describe(), "CPU Counters · GPU");
    }

    #[test]
    fn cpu_windows_skip_empty_rows_and_resolve_refs() {
        let cpu = parse_cpu_bottleneck(CPU_XML).unwrap();
        assert_eq!(cpu.windows.len(), 3);
        assert_eq!(cpu.windows[0], [0.25, 0.75, 0.0, 0.0]);
        assert_eq!(cpu.windows[2], [0.5, 0.5, 0.0, 0.0]);
        assert!((cpu.means[0] - (0.25 + 0.5 + 0.5) / 3.0).abs() < 1e-6);
        assert!((cpu.window_s - 0.01).abs() < 1e-6);
    }

    // Times are ns, 1 ms apart. t1 samples twice on core 7, then reappears
    // 3 ms later on core 2 with reset counters. t2 is blocked once (no
    // counters), then samples twice on core 3, then once on core 4 with a
    // huge jump that a core change must not count.
    const RAW_XML: &str = r#"<node>
<row><sample-time id="1">1000000</sample-time><thread id="2" fmt="t1">t1</thread><core id="20">7</core><thread-state id="3">Running</thread-state><pmc-events id="4" fmt="x">1000 500 600 1000 700 40 10 300 0 0 0 0</pmc-events></row>
<row><sample-time id="5">2000000</sample-time><thread ref="2"/><core ref="20"/><thread-state ref="3"/><pmc-events id="6" fmt="x">3000 1500 1800 3000 2100 120 30 900 0 0 0 0</pmc-events></row>
<row><sample-time id="7">2000000</sample-time><thread id="8" fmt="t2">t2</thread><core id="21">3</core><thread-state id="9">Blocked</thread-state><sentinel/></row>
<row><sample-time id="10">3000000</sample-time><thread ref="8"/><core ref="21"/><thread-state ref="3"/><pmc-events id="11" fmt="x">50 10 10 50 10 0 0 0 0 0 0 0</pmc-events></row>
<row><sample-time id="12">4000000</sample-time><thread ref="8"/><core ref="21"/><thread-state ref="3"/><pmc-events id="13" fmt="x">150 30 30 150 30 0 1 0 0 0 0 0</pmc-events></row>
<row><sample-time id="14">5000000</sample-time><thread ref="2"/><core id="22">2</core><thread-state ref="3"/><pmc-events id="15" fmt="x">10 1 1 10 1 0 0 0 0 0 0 0</pmc-events></row>
<row><sample-time id="16">5000000</sample-time><thread ref="8"/><core id="23">4</core><thread-state ref="3"/><pmc-events id="17" fmt="x">9000000000 30 30 150 30 0 1 0 0 0 0 0</pmc-events></row>
</node>"#;

    #[test]
    fn pmc_totals_sum_adjacent_same_core_deltas_only() {
        let t = parse_pmc_totals(RAW_XML);
        // t1: 3000-1000 on core 7; t2: 150-50 on core 3. The 3 ms gap with a
        // core change and the core-4 jump add nothing.
        assert_eq!(t[0], 2100.0);
        assert_eq!(t[1], 1020.0);
        assert_eq!(t[6], 21.0);
        assert_eq!(t.len(), 12);
        let ev = CpuEvents::from_totals("bottleneck bottlenecks", &[], t);
        assert_eq!(ev.names[1], "Instructions");
        assert_eq!(ev.names[8], "pmc 8");
        assert_eq!(ev.ratios[0], ("Instructions per cycle".to_string(), "0.49".to_string()));
        assert!(ev.ratios.iter().any(|(k, _)| k == "Branch mispredictions"));
        let unknown = CpuEvents::from_totals("custom", &[], vec![1.0, 2.0]);
        assert_eq!(unknown.names, vec!["pmc 0", "pmc 1"]);
        assert!(unknown.ratios.is_empty());
    }

    #[test]
    fn custom_event_sets_get_named_columns_and_miss_rates() {
        let toc = r#"<table sample-rate-micro-seconds="1000" pmc-events="&quot;L1D_CACHE_MISS_LD&quot; &quot;L1D_CACHE_MISS_ST&quot; &quot;LD_UNIT_UOP&quot; &quot;ST_UNIT_UOP&quot; &quot;INST_BRANCH&quot; &quot;BRANCH_MISPRED_NONSPEC&quot; &quot;WEIRD_EVENT&quot;" schema="kdebug-counters-with-time-sample" callstack="user"/>"#;
        let names = parse_pmc_names(toc);
        assert_eq!(names.len(), 7);
        assert_eq!(names[0], "L1D_CACHE_MISS_LD");
        assert_eq!(names[6], "WEIRD_EVENT");
        let ev = CpuEvents::from_totals("custom", &names, vec![10.0, 5.0, 80.0, 20.0, 50.0, 5.0, 3.0]);
        assert_eq!(ev.names[0], "L1 data misses, loads");
        assert_eq!(ev.names[6], "WEIRD_EVENT");
        let r: std::collections::HashMap<_, _> = ev.ratios.iter().cloned().collect();
        assert_eq!(r["L1 data miss rate"], "15.00%");
        assert_eq!(r["L1 load miss rate"], "12.50%");
        assert_eq!(r["Branch mispredict rate"], "10.00%");
        assert!(!r.contains_key("L2 TLB miss rate"));
        assert!(parse_pmc_names("<table schema=\"other\"/>").is_empty());
    }

    #[test]
    fn counts_get_unit_prefixes() {
        assert_eq!(format_count(950.0), "950");
        assert_eq!(format_count(12_345.0), "12.3k");
        assert_eq!(format_count(12_345_678.0), "12.3M");
        assert_eq!(format_count(2.5e9), "2.50G");
    }

    #[test]
    fn counting_mode_comes_from_the_table_attribute() {
        assert_eq!(
            parse_counting_mode(r#"<table counting-mode="bottleneck bottlenecks" schema="x"/>"#).as_deref(),
            Some("bottleneck bottlenecks")
        );
        assert_eq!(parse_counting_mode("<x/>"), None);
    }

    #[test]
    fn cpu_without_rows_is_an_error() {
        assert!(parse_cpu_bottleneck("<node></node>").is_err());
    }

    #[test]
    fn gpu_counter_names_come_from_the_third_cell() {
        assert_eq!(parse_gpu_counter_names(GPU_XML), vec!["RT Unit Active", "VS Occupancy"]);
    }

    /// End to end against a real bundle: `JADE_TRACE=path/to/x.trace cargo
    /// test -p jade analyze_real -- --ignored`. Needs Xcode's xctrace.
    #[test]
    #[ignore]
    fn analyze_real_trace() {
        let path = PathBuf::from(std::env::var("JADE_TRACE").expect("JADE_TRACE=…/x.trace"));
        let a = analyze(&path).unwrap();
        eprintln!("{}", a.describe());
        match a.kind {
            TraceKind::CpuCounters(cpu) => {
                if !cpu.windows.is_empty() {
                    assert!(cpu.windows.len() > 10);
                    let total: f32 = cpu.means.iter().sum();
                    assert!((total - 1.0).abs() < 1e-3, "means sum to {total}");
                }
                eprintln!("windows={} means={:?}", cpu.windows.len(), cpu.means);
                assert!(!cpu.windows.is_empty() || cpu.events.is_some());
                if let Some(ev) = &cpu.events {
                    eprintln!("mode={}", ev.mode);
                    for (n, t) in ev.names.iter().zip(&ev.totals) {
                        eprintln!("  {n}: {}", format_count(*t));
                    }
                    for (k, v) in &ev.ratios {
                        eprintln!("  {k}: {v}");
                    }
                }
            }
            other => eprintln!("kind={other:?}"),
        }
    }

    #[test]
    fn bundle_detection_needs_a_directory() {
        let dir = std::env::temp_dir().join(format!("jade-trace-test-{}.trace", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        assert!(is_trace_bundle(&dir));
        let file = dir.join("inner.trace");
        std::fs::write(&file, b"x").unwrap();
        assert!(!is_trace_bundle(&file));
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
