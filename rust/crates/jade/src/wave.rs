//! Wave-panel state and actions (hardware mode).
//!
//! The panel replaces the board drawer. It runs the project's testbench
//! with Icarus Verilog (`jade_hw::testbench`), loads the FST/VCD dump it
//! writes (`jade_hw::wave`), and draws the signals. The render model lives
//! in [`WaveState`] inside `HwState`; the view transform is
//! `x = (t - t0) * ppu` in file time units.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use jade_hw::testbench::{self, TbOutcome};
use jade_hw::wave::WaveFile;

use crate::app::{AppEvent, BottomView, JadeApp, ToastKind};
use crate::output::push_output;

/// The names column width in the panel.
pub const NAME_W: f32 = 148.0;
/// The value-at-cursor column width.
pub const VAL_W: f32 = 72.0;
/// The card's horizontal padding (both sides together) plus the grab strip.
pub const CHROME_W: f32 = 24.0;
/// The narrowest and widest the drawer goes.
pub const MIN_W: f32 = 380.0;
pub const MAX_W: f32 = 1400.0;

/// Background work reports back through `AppEvent::Wave`.
#[derive(Debug)]
pub enum WaveEvent {
    /// A testbench run ended.
    TbDone(TbOutcome),
    /// A dump finished loading (or failed). `generation` drops stale loads.
    Loaded {
        generation: u64,
        result: Result<Arc<WaveFile>, String>,
    },
}

/// Render model for the wave panel.
#[derive(Debug, Clone)]
pub struct WaveState {
    pub file: Option<Arc<WaveFile>>,
    /// The last failure, shown in the status row.
    pub error: Option<String>,
    pub loading: bool,
    pub tb_running: bool,
    /// The testbench of the last run; a save re-runs it.
    pub tb_path: Option<PathBuf>,
    /// Signal indices into `file.signals`, in display order.
    pub rows: Vec<usize>,
    /// File time at the left edge of the wave canvas.
    pub t0: f64,
    /// Pixels per file time unit.
    pub ppu: f64,
    pub cursor: Option<u64>,
    /// File time under the pointer, for the ruler readout.
    pub hover_t: Option<f64>,
    /// A pan in progress: `(mouse x at start, t0 at start)`.
    pub drag: Option<(f32, f64)>,
    /// The signal picker replaces the rows while open.
    pub picker: bool,
    /// Bumped per load request; a result with an older value is dropped.
    pub generation: u64,
}

impl Default for WaveState {
    fn default() -> Self {
        WaveState {
            file: None,
            error: None,
            loading: false,
            tb_running: false,
            tb_path: None,
            rows: Vec::new(),
            t0: 0.0,
            ppu: 1.0,
            cursor: None,
            hover_t: None,
            drag: None,
            picker: false,
            generation: 0,
        }
    }
}

impl WaveState {
    /// The file time at canvas x.
    pub fn time_at(&self, x: f32) -> f64 {
        self.t0 + x as f64 / self.ppu
    }

    /// The canvas x of a file time.
    pub fn x_of(&self, t: f64) -> f32 {
        ((t - self.t0) * self.ppu) as f32
    }

    fn end_time(&self) -> f64 {
        self.file.as_ref().map(|f| f.end_time as f64).unwrap_or(0.0).max(1.0)
    }

    /// Keep the view on the dump: a small overshoot on both ends is allowed.
    fn clamp(&mut self, canvas_w: f32) {
        let end = self.end_time();
        let span = canvas_w as f64 / self.ppu;
        let slack = span * 0.05;
        let max_t0 = (end - span + slack).max(-slack);
        self.t0 = self.t0.clamp(-slack, max_t0);
    }

    /// Show the whole dump across `canvas_w` pixels.
    pub fn fit(&mut self, canvas_w: f32) {
        let w = canvas_w.max(40.0) as f64;
        self.ppu = (w * 0.98) / self.end_time();
        self.t0 = -(w * 0.01) / self.ppu;
    }

    /// Zoom by `factor` around canvas x.
    pub fn zoom(&mut self, factor: f64, anchor_x: f32, canvas_w: f32) {
        let end = self.end_time();
        let min_ppu = (canvas_w.max(40.0) as f64 * 0.5) / end;
        let max_ppu = 400.0;
        let anchor_t = self.time_at(anchor_x);
        self.ppu = (self.ppu * factor).clamp(min_ppu, max_ppu);
        self.t0 = anchor_t - anchor_x as f64 / self.ppu;
        self.clamp(canvas_w);
    }

    /// Pan by `dx` pixels (positive = later times come into view).
    pub fn pan(&mut self, dx: f32, canvas_w: f32) {
        self.t0 += dx as f64 / self.ppu;
        self.clamp(canvas_w);
    }

    /// Place the cursor at canvas x, clamped to the dump.
    pub fn set_cursor_x(&mut self, x: f32) {
        let end = self.end_time();
        let t = self.time_at(x).clamp(0.0, end);
        self.cursor = Some(t.round() as u64);
    }

    /// The time the value column reads at: the cursor, else the dump's end.
    pub fn read_time(&self) -> u64 {
        self.cursor
            .unwrap_or_else(|| self.file.as_ref().map(|f| f.end_time).unwrap_or(0))
    }
}

impl JadeApp {
    /// The wave canvas width, from the drawer width and the fixed columns.
    pub fn wave_canvas_w(&self) -> f32 {
        (self.wave_width - CHROME_W - NAME_W - VAL_W).max(40.0)
    }

    /// The testbench a run would use: the active tab's, else the first.
    pub fn wave_testbench(&self) -> Option<PathBuf> {
        let active = self.editor.active_tab().map(|t| t.path.clone());
        testbench::pick_testbench(&self.workspace_root, active.as_deref())
    }

    /// "Run testbench": compile and run it, stream its output to the OUTPUT
    /// pane, then load the dump. `explicit` = the user clicked, so the
    /// Output tab comes to the front for the `$display` lines.
    pub fn hw_run_testbench(&mut self, explicit: bool) {
        let Some(hw) = &mut self.hw else { return };
        if hw.wave.tb_running {
            return;
        }
        let tb = match self.wave_testbench() {
            Some(tb) => tb,
            None => {
                let msg = "No testbench found. Add a tb_<name>.v with $dumpfile and $dumpvars.";
                if let Some(hw) = &mut self.hw {
                    hw.wave.error = Some(msg.to_string());
                }
                self.push_toast(ToastKind::Error, msg);
                return;
            }
        };
        let Some(hw) = &mut self.hw else { return };
        hw.wave.tb_running = true;
        hw.wave.tb_path = Some(tb.clone());
        hw.wave.error = None;
        let tb_name = tb.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        push_output(&mut self.output, &format!("[jade] Running testbench {tb_name}"));
        self.status_line(&format!("[jade] Running {tb_name}..."));
        if explicit {
            self.output_visible = true;
            self.bottom_closing = false;
            self.set_bottom_view(BottomView::Output);
        }

        let tx = self.app_tx.clone();
        // The project directory is the directory of the testbench: the design
        // sources and the `sim/` dump directory are next to it, as with the
        // Makefile run from that directory.
        let root = tb
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_else(|| self.workspace_root.clone());
        let (line_tx, mut line_rx) = tokio::sync::mpsc::unbounded_channel::<String>();
        let ftx = tx.clone();
        self.runtime.spawn(async move {
            while let Some(line) = line_rx.recv().await {
                let _ = ftx.send(AppEvent::BuildOutput(line));
            }
        });
        self.runtime.spawn(async move {
            let outcome = testbench::run_testbench(root, tb, line_tx).await;
            let _ = tx.send(AppEvent::Wave(WaveEvent::TbDone(outcome)));
        });
    }

    /// Re-run the last testbench after a source save, quietly.
    pub fn hw_rerun_testbench(&mut self) {
        let has_tb = self.hw.as_ref().is_some_and(|h| h.wave.tb_path.is_some() && !h.wave.tb_running);
        if has_tb {
            self.hw_run_testbench(false);
        }
    }

    /// Load a dump off the main thread.
    pub fn hw_load_wave(&mut self, path: PathBuf) {
        let Some(hw) = &mut self.hw else { return };
        hw.wave.generation += 1;
        hw.wave.loading = true;
        let generation = hw.wave.generation;
        let tx = self.app_tx.clone();
        self.runtime.spawn(async move {
            let result = tokio::task::spawn_blocking(move || WaveFile::load(&path))
                .await
                .unwrap_or_else(|e| Err(format!("wave load panicked: {e}")))
                .map(Arc::new);
            let _ = tx.send(AppEvent::Wave(WaveEvent::Loaded { generation, result }));
        });
    }

    /// Load the newest dump already in `sim/`, so the panel opens with the
    /// last run instead of an empty sheet.
    pub fn hw_load_existing_wave(&mut self) {
        let sim = self.workspace_root.join("sim");
        if let Some(dump) = testbench::newest_dump(&sim, None) {
            self.hw_load_wave(dump);
        }
    }

    /// Apply one background result (the `AppEvent::Wave` arm).
    pub fn on_wave_event(&mut self, ev: WaveEvent) {
        match ev {
            WaveEvent::TbDone(outcome) => {
                if let Some(hw) = &mut self.hw {
                    hw.wave.tb_running = false;
                }
                self.on_hw_diagnostics(outcome.errors.clone());
                if outcome.ok {
                    self.push_toast(ToastKind::Success, "Testbench ran");
                    self.status_line("[jade] Testbench ran");
                    if let Some(dump) = outcome.dump {
                        self.hw_load_wave(dump);
                    }
                } else {
                    let why = outcome.failure.unwrap_or_else(|| "testbench failed".to_string());
                    if let Some(hw) = &mut self.hw {
                        hw.wave.error = Some(why.clone());
                    }
                    self.push_toast(ToastKind::Error, format!("Testbench: {why}"));
                    self.status_line(&format!("[jade] Testbench failed: {why}"));
                    self.output_visible = true;
                }
            }
            WaveEvent::Loaded { generation, result } => {
                let canvas_w = self.wave_canvas_w();
                let Some(hw) = &mut self.hw else { return };
                if generation != hw.wave.generation {
                    return; // a newer load is in flight
                }
                hw.wave.loading = false;
                match result {
                    Ok(file) => {
                        // Keep the visible rows across a re-run by name.
                        let old_names: Vec<String> = match &hw.wave.file {
                            Some(old) => hw
                                .wave
                                .rows
                                .iter()
                                .filter_map(|&i| old.signals.get(i).map(|s| s.full_name()))
                                .collect(),
                            None => Vec::new(),
                        };
                        // The old rows survive only when every one of them
                        // is in the new dump; a changed signal set (a new
                        // testbench) starts from the testbench scope again.
                        let matched: Vec<usize> =
                            old_names.iter().filter_map(|n| file.find(n)).collect();
                        let rows = if !matched.is_empty() && matched.len() == old_names.len() {
                            matched
                        } else {
                            file.default_rows()
                        };
                        let keep_view = hw
                            .wave
                            .file
                            .as_ref()
                            .is_some_and(|old| old.path == file.path && old.end_time == file.end_time);
                        hw.wave.rows = rows;
                        hw.wave.error = None;
                        if let Some(c) = hw.wave.cursor {
                            if c > file.end_time {
                                hw.wave.cursor = None;
                            }
                        }
                        hw.wave.file = Some(file);
                        if !keep_view {
                            hw.wave.fit(canvas_w);
                        }
                    }
                    Err(e) => {
                        hw.wave.error = Some(e.clone());
                        self.push_toast(ToastKind::Error, format!("Waveform: {e}"));
                    }
                }
            }
        }
    }

    /// "Surfer": open the current dump in the external viewer.
    pub fn hw_open_surfer(&mut self) {
        let dump = self.hw.as_ref().and_then(|h| h.wave.file.as_ref().map(|f| f.path.clone()));
        let Some(dump) = dump else {
            self.push_toast(ToastKind::Error, "No waveform to open. Run a testbench first.");
            return;
        };
        match testbench::open_in_surfer(&dump) {
            Ok(()) => self.status_line(&format!("[jade] Opened {} in Surfer", dump.display())),
            Err(hint) => {
                if let Some(hw) = &mut self.hw {
                    hw.wave.error = Some(hint.clone());
                }
                self.push_toast(ToastKind::Error, hint);
            }
        }
    }

    /// Show or hide one signal row.
    pub fn wave_toggle_row(&mut self, sig: usize) {
        let Some(hw) = &mut self.hw else { return };
        if let Some(pos) = hw.wave.rows.iter().position(|&r| r == sig) {
            hw.wave.rows.remove(pos);
        } else {
            hw.wave.rows.push(sig);
        }
    }

    /// Show every signal of a scope, or hide them all when all are shown.
    pub fn wave_toggle_scope(&mut self, scope: usize) {
        let Some(hw) = &mut self.hw else { return };
        let Some(file) = &hw.wave.file else { return };
        let Some(sc) = file.scopes.get(scope) else { return };
        let sigs = sc.signals.clone();
        let all_shown = sigs.iter().all(|s| hw.wave.rows.contains(s));
        if all_shown {
            hw.wave.rows.retain(|r| !sigs.contains(r));
        } else {
            for s in sigs {
                if !hw.wave.rows.contains(&s) {
                    hw.wave.rows.push(s);
                }
            }
        }
    }

    pub fn wave_fit(&mut self) {
        let w = self.wave_canvas_w();
        if let Some(hw) = &mut self.hw {
            hw.wave.fit(w);
        }
    }

    /// Zoom around the canvas center (toolbar and keyboard zoom).
    pub fn wave_zoom_center(&mut self, factor: f64) {
        let w = self.wave_canvas_w();
        if let Some(hw) = &mut self.hw {
            hw.wave.zoom(factor, w / 2.0, w);
        }
    }

    /// Move the cursor to the previous (`-1`) or next (`+1`) change on any
    /// visible row; with no cursor, start from the left edge.
    pub fn wave_step_cursor(&mut self, dir: i32) {
        let Some(hw) = &mut self.hw else { return };
        let Some(file) = hw.wave.file.clone() else { return };
        let from = hw.wave.cursor.unwrap_or(hw.wave.t0.max(0.0) as u64);
        let mut best: Option<u64> = None;
        for &r in &hw.wave.rows {
            let Some(sig) = file.signals.get(r) else { continue };
            let cand = if dir > 0 {
                let i = sig.changes.partition_point(|c| c.time <= from);
                sig.changes.get(i).map(|c| c.time)
            } else {
                let i = sig.changes.partition_point(|c| c.time < from);
                i.checked_sub(1).map(|i| sig.changes[i].time)
            };
            if let Some(t) = cand {
                best = Some(match best {
                    Some(b) if dir > 0 => b.min(t),
                    Some(b) => b.max(t),
                    None => t,
                });
            }
        }
        if let Some(t) = best {
            hw.wave.cursor = Some(t);
            // Keep the cursor in view.
            let w = (self.wave_width - CHROME_W - NAME_W - VAL_W).max(40.0);
            let x = hw.wave.x_of(t as f64);
            if x < 0.0 || x > w {
                hw.wave.t0 = t as f64 - (w as f64 / 2.0) / hw.wave.ppu;
                hw.wave.clamp(w);
            }
        }
    }
}
