//! `WeightGrid3D` state + the pure data transforms behind it (feature inventory
//! §7.2), ported from `renderer/ml/weight-grid-3d.ts`.
//!
//! This module owns everything that does not touch GPUI: the independent
//! **64-frame ring buffer per buffer** (fed even while the overlay is hidden),
//! the buffer selector + live/freeze scrubber state machine, the sqrt-stride
//! subsample, the diverging colormap, and the conversion of a tensor frame into
//! a flat list of [`Bar`]s. The GPUI view (`render.rs`) is a pure projection of
//! this state; the camera lives in [`super::camera`].

use std::collections::{HashMap, VecDeque};

use gpui::FocusHandle;

use crate::training::TensorFrame;

use super::camera::{OrbitCamera, HEIGHT_SCALE};

/// Frames retained per buffer name — independent of the training view's 32
/// (weight-grid-3d.ts:31). The ring fills while hidden so a replay is available
/// the moment the overlay opens.
pub const RING: usize = 64;
/// Floor so near-zero cells still register a footprint (weight-grid-3d.ts:33).
pub const MIN_BAR: f32 = 0.02;
/// Instance cap; larger frames get sqrt-stride subsampled (256×256).
pub const MAX_CELLS: usize = 256 * 256;

/// One instanced bar: grid position `(x, z)`, signed world `height` (negative
/// bars extend below the y=0 plane), and its diverging-colormap `rgb`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Bar {
    pub x: f32,
    pub z: f32,
    pub height: f32,
    pub r: f32,
    pub g: f32,
    pub b: f32,
}

impl Bar {
    /// Centroid used for back-to-front depth sorting.
    pub fn centroid(&self) -> [f32; 3] {
        [self.x, self.height * 0.5, self.z]
    }
}

/// The result of turning one frame into bars: the (possibly subsampled) grid
/// dimensions, the bars, the frame max-abs (drives the value-axis label), and
/// the source dimensions (for index axis labels).
#[derive(Debug, Clone)]
pub struct BarGrid {
    pub rows: u32,
    pub cols: u32,
    pub bars: Vec<Bar>,
    pub max_abs: f32,
    pub src_rows: u32,
    pub src_cols: u32,
}

/// Diverging colormap centered at 0, normalized by `max_abs`
/// (weight-grid-3d.ts:880-897; identical to the 2D heatmap convention).
/// `t ≥ 0` → white→red, `t < 0` → white→blue.
/// The colormap's three stops. Zero sits on the charcoal, so a cell near
/// zero is quiet; magnitude moves toward a muted coral (positive) or a
/// periwinkle (negative). Both ends are the hues mixed back toward the ink,
/// so a full-magnitude sheet still reads as one drawing and not as a flag.
pub const MAP_MID: [f32; 3] = [0.239, 0.251, 0.278]; // #3D4047
pub const MAP_POS: [f32; 3] = [0.878, 0.478, 0.373]; // #E07A5F
pub const MAP_NEG: [f32; 3] = [0.420, 0.580, 0.902]; // #6B94E6

/// The diverging colormap for a value against the frame's max-abs.
pub fn diverging(v: f32, inv_max: f32) -> [f32; 3] {
    if inv_max == 0.0 || !v.is_finite() {
        return MAP_MID;
    }
    let t = (v * inv_max).clamp(-1.0, 1.0);
    // A gentle curve lifts the small values a little, so a sheet of mostly
    // small weights does not collapse into the midpoint.
    let k = t.abs().powf(0.75);
    let end = if t >= 0.0 { MAP_POS } else { MAP_NEG };
    [
        MAP_MID[0] + (end[0] - MAP_MID[0]) * k,
        MAP_MID[1] + (end[1] - MAP_MID[1]) * k,
        MAP_MID[2] + (end[2] - MAP_MID[2]) * k,
    ]
}

/// Subsampled output dimensions + stride for a `rows × cols` frame under an
/// arbitrary cell budget (weight-grid-3d.ts:824-832 generalized — the TS only
/// had the MAX_CELLS budget; the CPU painter also uses a smaller interactive
/// budget so sort+paint stays comfortable).
pub fn subsample_dims_capped(rows: u32, cols: u32, max_cells: usize) -> (u32, u32, u32) {
    let rows = rows.max(1);
    let cols = cols.max(1);
    let max_cells = max_cells.max(1);
    let n = rows as usize * cols as usize;
    if n <= max_cells {
        return (rows, cols, 1);
    }
    let stride = ((n as f64 / max_cells as f64).sqrt()).ceil() as u32;
    let stride = stride.max(1);
    (
        (rows as f64 / stride as f64).ceil() as u32,
        (cols as f64 / stride as f64).ceil() as u32,
        stride,
    )
}

/// The §7.2 instance-cap subsample (256×256 budget).
pub fn subsample_dims(rows: u32, cols: u32) -> (u32, u32, u32) {
    subsample_dims_capped(rows, cols, MAX_CELLS)
}

/// Build the bar list for a frame (weight-grid-3d.ts `applyFrame`) with the
/// §7.2 MAX_CELLS budget. Returns `None` if the frame data is short of its
/// declared shape (the TS early-out).
pub fn build_bars(frame: &TensorFrame) -> Option<BarGrid> {
    build_bars_capped(frame, MAX_CELLS)
}

/// [`build_bars`] under an explicit cell budget — the interactive painter caps
/// the default display at 64×64 bars (resolution select raises it).
pub fn build_bars_capped(frame: &TensorFrame, max_cells: usize) -> Option<BarGrid> {
    let src_rows = frame.rows.max(1);
    let src_cols = frame.cols.max(1);
    if (frame.data.len() as u32) < src_rows * src_cols {
        return None;
    }
    let (rows, cols, stride) = subsample_dims_capped(src_rows, src_cols, max_cells);
    let src_cols_i = src_cols as usize;

    // Normalize by max absolute value across sampled cells (guard div-by-zero).
    let mut max_abs = 0.0f32;
    for r in 0..rows {
        let sr = (r * stride) as usize;
        for c in 0..cols {
            let idx = sr * src_cols_i + (c * stride) as usize;
            if let Some(&v) = frame.data.get(idx) {
                let a = v.abs();
                if a.is_finite() && a > max_abs {
                    max_abs = a;
                }
            }
        }
    }
    let inv_max = if max_abs > 0.0 { 1.0 / max_abs } else { 0.0 };

    let mut bars = Vec::with_capacity(rows as usize * cols as usize);
    for r in 0..rows {
        let sr = (r * stride) as usize;
        for c in 0..cols {
            let idx = sr * src_cols_i + (c * stride) as usize;
            let v = frame.data.get(idx).copied().unwrap_or(0.0);
            let finite = v.is_finite();
            let norm_abs = if finite { v.abs() * inv_max } else { 0.0 };
            let mag = (norm_abs * HEIGHT_SCALE).max(MIN_BAR);
            let height = if finite && v < 0.0 { -mag } else { mag };
            let [cr, cg, cb] = diverging(v, inv_max);
            bars.push(Bar {
                x: c as f32 - cols as f32 / 2.0,
                z: r as f32 - rows as f32 / 2.0,
                height,
                r: cr,
                g: cg,
                b: cb,
            });
        }
    }

    Some(BarGrid {
        rows,
        cols,
        bars,
        max_abs,
        src_rows: frame.src_rows.unwrap_or(src_rows),
        src_cols: frame.src_cols.unwrap_or(src_cols),
    })
}

/// The overlay's non-GPUI state.
pub struct WeightGrid3D {
    /// Per-buffer ring of decoded frames, in insertion order (`order`).
    buffers: HashMap<String, VecDeque<TensorFrame>>,
    order: Vec<String>,

    selected: Option<String>,
    index: usize,
    /// Auto-advance to the newest frame while true (weight-grid-3d.ts `live`).
    live: bool,

    pub visible: bool,
    pub camera: OrbitCamera,
    /// The grid shape the camera was last framed for (avoids re-framing every
    /// live frame when the shape is stable).
    framed_shape: Option<(u32, u32)>,

    /// Lazily created in the view (needs a `Context`); drives Esc + focus.
    pub focus: Option<FocusHandle>,
    /// True while the damping repaint loop is running (idle-stopping contract).
    pub anim_running: bool,

    /// Last pointer position while a drag is active (window-space pixels).
    drag_last: Option<(f32, f32)>,
    /// True when the active drag is a pan (Shift/right button) vs an orbit.
    drag_pan: bool,
    /// Interactive display resolution (bars per dimension). Defaults to 64 so
    /// the CPU sort+paint stays comfortable; the ≤64/≤128/≤256 select raises it
    /// (also forwarding to the registry's streaming maxDim as in the TS).
    view_max_dim: u32,

    /// Memoized bars for the current (buffer, frame, resolution): camera motion
    /// repaints every 16 ms and must not re-subsample the tensor each time.
    /// The key's `step` distinguishes ring turnover in live mode (a full ring
    /// keeps the same len/index while frames keep changing).
    bars_cache: Option<(BarsKey, std::sync::Arc<BarGrid>)>,
    /// Bumped on every cache rebuild; the GPU renderer re-uploads instance
    /// data only when this changes (the TS `bufferSubData`-per-new-frame).
    bars_generation: u64,
}

/// Cache key for [`WeightGrid3D::current_bars_cached`]:
/// (buffer, frame index, ring len, view resolution, frame step).
type BarsKey = (String, usize, usize, u32, i64);

impl Default for WeightGrid3D {
    fn default() -> Self {
        WeightGrid3D {
            buffers: HashMap::new(),
            order: Vec::new(),
            selected: None,
            index: 0,
            live: true,
            visible: false,
            camera: OrbitCamera::new(),
            framed_shape: None,
            focus: None,
            anim_running: false,
            drag_last: None,
            drag_pan: false,
            view_max_dim: 64,
            bars_cache: None,
            bars_generation: 0,
        }
    }
}

impl WeightGrid3D {
    pub fn new() -> Self {
        Self::default()
    }

    // ── Data ingestion (fed from the app's telemetry apply path) ──────────────

    /// Ingest a tensor frame. Pushes to the buffer's ring (capped at [`RING`]),
    /// auto-selecting the first buffer ever seen. When live + selected, tracks
    /// the newest frame. Runs even while hidden (weight-grid-3d.ts:404-425).
    pub fn on_frame(&mut self, name: &str, frame: TensorFrame) {
        let ring = match self.buffers.get_mut(name) {
            Some(r) => r,
            None => {
                self.order.push(name.to_string());
                self.buffers.entry(name.to_string()).or_default()
            }
        };
        ring.push_back(frame);
        while ring.len() > RING {
            ring.pop_front();
        }

        if self.selected.is_none() {
            self.select_buffer(name);
            return;
        }
        if self.selected.as_deref() == Some(name) && self.live {
            self.index = self.frames_len().saturating_sub(1);
            self.refresh_frame_shape();
        }
    }

    // ── Selection + scrubber ──────────────────────────────────────────────────

    pub fn buffer_names(&self) -> &[String] {
        &self.order
    }

    pub fn selected(&self) -> Option<&str> {
        self.selected.as_deref()
    }

    /// Focus a buffer: reset to live at its newest frame (weight-grid-3d.ts:440-451).
    pub fn select_buffer(&mut self, name: &str) {
        self.selected = Some(name.to_string());
        self.live = true;
        self.index = self.frames_len().saturating_sub(1);
        self.framed_shape = None; // force a re-frame for the new buffer
        self.refresh_frame_shape();
    }

    /// Scrubber drag → frame `idx`. Live only while parked at the newest frame;
    /// dragging back freezes (weight-grid-3d.ts:496-504).
    pub fn scrub(&mut self, idx: usize) {
        let len = self.frames_len();
        if len == 0 {
            return;
        }
        let idx = idx.min(len - 1);
        self.index = idx;
        self.live = idx >= len - 1;
        self.refresh_frame_shape();
    }

    pub fn index(&self) -> usize {
        self.index
    }

    pub fn is_live(&self) -> bool {
        self.live
    }

    pub fn frames_len(&self) -> usize {
        self.selected
            .as_ref()
            .and_then(|s| self.buffers.get(s))
            .map(|r| r.len())
            .unwrap_or(0)
    }

    /// The frame currently displayed (selected buffer at `index`).
    pub fn current_frame(&self) -> Option<&TensorFrame> {
        let ring = self.buffers.get(self.selected.as_ref()?)?;
        let idx = self.index.min(ring.len().saturating_sub(1));
        ring.get(idx)
    }

    /// Bars for the current frame (or `None` when there's nothing to draw),
    /// under the interactive display budget (`view_max_dim`²).
    pub fn current_bars(&self) -> Option<BarGrid> {
        build_bars_capped(self.current_frame()?, self.view_cells())
    }

    /// Memoized [`current_bars`](Self::current_bars): rebuilds only when the
    /// (buffer, frame, resolution) key changes, so per-repaint cost during
    /// camera motion is a hash-key compare instead of a full re-subsample.
    /// Returns the bars plus the cache generation the GPU uploader keys on.
    pub fn current_bars_cached(&mut self) -> Option<(std::sync::Arc<BarGrid>, u64)> {
        let name = self.selected.clone()?;
        let frame_step = self.current_frame()?.step;
        let key: BarsKey = (
            name,
            self.index,
            self.frames_len(),
            self.view_max_dim,
            frame_step,
        );
        if let Some((k, bars)) = &self.bars_cache {
            if *k == key {
                return Some((bars.clone(), self.bars_generation));
            }
        }
        let bars = std::sync::Arc::new(build_bars_capped(
            self.current_frame()?,
            self.view_cells(),
        )?);
        self.bars_generation = self.bars_generation.wrapping_add(1);
        self.bars_cache = Some((key, bars.clone()));
        Some((bars, self.bars_generation))
    }

    /// Interactive display resolution (bars per dimension).
    pub fn view_max_dim(&self) -> u32 {
        self.view_max_dim
    }

    /// Set the interactive display resolution (clamped to 16..=256).
    pub fn set_view_max_dim(&mut self, dim: u32) {
        self.view_max_dim = dim.clamp(16, 256);
        self.framed_shape = None; // shape may change → allow a re-frame
        self.refresh_frame_shape();
    }

    fn view_cells(&self) -> usize {
        ((self.view_max_dim as usize) * (self.view_max_dim as usize)).min(MAX_CELLS)
    }

    /// Re-frame the camera if the displayed (subsampled) shape changed.
    fn refresh_frame_shape(&mut self) {
        if let Some(frame) = self.current_frame() {
            let (rows, cols, _) = subsample_dims_capped(frame.rows, frame.cols, self.view_cells());
            if self.framed_shape != Some((rows, cols)) {
                self.framed_shape = Some((rows, cols));
                self.camera.frame(rows, cols);
            }
        }
    }

    // ── Open/close ────────────────────────────────────────────────────────────

    /// Show the overlay, optionally focusing a buffer (weight-grid-3d.ts:334-352).
    pub fn open(&mut self, buffer: Option<&str>) {
        self.visible = true;
        if let Some(name) = buffer {
            self.select_buffer(name);
        } else if self.selected.is_none() {
            if let Some(first) = self.order.first().cloned() {
                self.select_buffer(&first);
            }
        } else {
            self.refresh_frame_shape();
        }
    }

    /// Hide the overlay (keeps ring buffers + selection). Stops the loop via the
    /// caller checking `visible` (weight-grid-3d.ts:355-362).
    pub fn close(&mut self) {
        self.visible = false;
    }

    // ── Pointer / wheel input (orbit / pan / dolly) ───────────────────────────

    /// Begin a drag. `pan` selects pan mode (Shift held or right button), else
    /// orbit (weight-grid-gl.ts:201-208).
    pub fn pointer_down(&mut self, x: f32, y: f32, pan: bool) {
        self.drag_last = Some((x, y));
        self.drag_pan = pan;
    }

    /// Continue a drag; returns `true` if a motion delta was applied (so the
    /// caller can wake the repaint loop). `h` is the viewport height.
    pub fn pointer_move(&mut self, x: f32, y: f32, h: f32) -> bool {
        let Some((lx, ly)) = self.drag_last else {
            return false;
        };
        let dx = x - lx;
        let dy = y - ly;
        self.drag_last = Some((x, y));
        if self.drag_pan {
            self.camera.pan(dx, dy, h);
        } else {
            self.camera.orbit(dx, dy, h);
        }
        true
    }

    pub fn pointer_up(&mut self) {
        self.drag_last = None;
    }

    pub fn is_dragging(&self) -> bool {
        self.drag_last.is_some()
    }

    /// Wheel dolly (weight-grid-gl.ts:247-252).
    pub fn wheel(&mut self, delta_y: f32) {
        self.camera.dolly(delta_y);
    }

    // ── Readout (toolbar) ─────────────────────────────────────────────────────

    /// The toolbar readout string: `name dims step N [min…max] [i/count] • live`
    /// (weight-grid-3d.ts:518-546).
    pub fn readout(&self) -> String {
        let Some(sel) = &self.selected else {
            return "no data".to_string();
        };
        let len = self.frames_len();
        if len == 0 {
            return format!("{sel}  (no frames)");
        }
        let Some(frame) = self.current_frame() else {
            return format!("{sel}  (no frames)");
        };
        let n = frame.rows as usize * frame.cols as usize;
        let mut mn = f32::INFINITY;
        let mut mx = f32::NEG_INFINITY;
        for &v in frame.data.iter().take(n) {
            if v < mn {
                mn = v;
            }
            if v > mx {
                mx = v;
            }
        }
        let src_r = frame.src_rows.unwrap_or(frame.rows);
        let src_c = frame.src_cols.unwrap_or(frame.cols);
        let dims = if src_r != frame.rows || src_c != frame.cols {
            format!("{}×{} (shown {}×{})", src_r, src_c, frame.rows, frame.cols)
        } else {
            format!("{}×{}", frame.rows, frame.cols)
        };
        let range = if mn.is_finite() {
            format!("[{}…{}]  ", fmt_axis(mn), fmt_axis(mx))
        } else {
            String::new()
        };
        format!(
            "{}  {}  step {}  {}[{}/{}]{}",
            sel,
            dims,
            frame.step,
            range,
            self.index + 1,
            len,
            if self.live { " • live" } else { "" }
        )
    }
}

/// Compact numeric formatting for axis / readout labels (weight-grid-3d.ts:997).
pub fn fmt_axis(v: f32) -> String {
    if !v.is_finite() {
        return "?".to_string();
    }
    let a = v.abs();
    if a != 0.0 && (a >= 10000.0 || a < 0.001) {
        format!("{:.1e}", v)
    } else {
        let r = (v * 1000.0).round() / 1000.0;
        // Trim to match the TS String(Math.round(v*1000)/1000) look.
        format!("{}", r)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frame(step: i64, rows: u32, cols: u32, data: Vec<f32>) -> TensorFrame {
        TensorFrame {
            step,
            rows,
            cols,
            src_rows: None,
            src_cols: None,
            data,
        }
    }

    #[test]
    fn ring_caps_at_64() {
        let mut g = WeightGrid3D::new();
        for i in 0..70 {
            g.on_frame("W", frame(i, 2, 2, vec![i as f32; 4]));
        }
        assert_eq!(g.frames_len(), RING);
        // Newest retained frame is the 70th (step 69); oldest is step 6 (70-64).
        assert_eq!(g.current_frame().unwrap().step, 69);
    }

    #[test]
    fn subsample_stride_matches_ts() {
        // 512×512 = 262144 > 65536 → stride 2 → 256×256.
        assert_eq!(subsample_dims(512, 512), (256, 256, 2));
        // 300×300 = 90000 → stride 2 → 150×150.
        assert_eq!(subsample_dims(300, 300), (150, 150, 2));
        // Within cap: untouched.
        assert_eq!(subsample_dims(64, 64), (64, 64, 1));
        assert_eq!(subsample_dims(256, 256), (256, 256, 1));
    }

    #[test]
    fn colormap_diverges() {
        // Positive → the warm end; negative → the cool end.
        assert_eq!(diverging(1.0, 1.0), MAP_POS);
        assert_eq!(diverging(-1.0, 1.0), MAP_NEG);
        // Zero / no scale → the charcoal midpoint.
        assert_eq!(diverging(0.0, 1.0), MAP_MID);
        assert_eq!(diverging(5.0, 0.0), MAP_MID);
        // Half magnitude lands strictly between the midpoint and the warm
        // end on every channel.
        let half = diverging(0.5, 1.0);
        for c in 0..3 {
            let (lo, hi) = (MAP_MID[c].min(MAP_POS[c]), MAP_MID[c].max(MAP_POS[c]));
            assert!(half[c] > lo && half[c] < hi, "channel {c}: {}", half[c]);
        }
    }

    #[test]
    fn bars_sign_and_min_floor() {
        // Mixed frame: max-abs = 4. A +4 cell → full red up; a -2 cell → blue down.
        let f = frame(0, 1, 3, vec![4.0, -2.0, 0.0]);
        let g = build_bars(&f).unwrap();
        assert_eq!(g.bars.len(), 3);
        assert!((g.max_abs - 4.0).abs() < 1e-6);
        // +4 → height +HEIGHT_SCALE, the warm end.
        assert!((g.bars[0].height - HEIGHT_SCALE).abs() < 1e-4);
        assert_eq!([g.bars[0].r, g.bars[0].g, g.bars[0].b], MAP_POS);
        // -2 → height -(0.5*HEIGHT_SCALE), extends below plane, blue.
        assert!((g.bars[1].height + HEIGHT_SCALE * 0.5).abs() < 1e-4);
        assert!(g.bars[1].height < 0.0);
        // 0 → floored to MIN_BAR, the midpoint.
        assert!((g.bars[2].height - MIN_BAR).abs() < 1e-6);
        assert_eq!([g.bars[2].r, g.bars[2].g, g.bars[2].b], MAP_MID);
    }

    #[test]
    fn live_freeze_scrubber_rule() {
        let mut g = WeightGrid3D::new();
        for i in 0..5 {
            g.on_frame("W", frame(i, 2, 2, vec![i as f32; 4]));
        }
        // Live: parked at newest (index 4).
        assert!(g.is_live());
        assert_eq!(g.index(), 4);
        // Drag back to an older frame → freeze.
        g.scrub(2);
        assert!(!g.is_live());
        assert_eq!(g.index(), 2);
        // A new frame arrives while frozen: index must NOT move.
        g.on_frame("W", frame(5, 2, 2, vec![5.0; 4]));
        assert_eq!(g.index(), 2);
        assert!(!g.is_live());
        // Scrub back to the newest → resume live.
        let last = g.frames_len() - 1;
        g.scrub(last);
        assert!(g.is_live());
        // Switching buffers resets to live (weight-grid-3d.ts:448).
        g.on_frame("B", frame(0, 2, 2, vec![0.0; 4]));
        g.scrub(0);
        g.select_buffer("W");
        assert!(g.is_live());
    }

    #[test]
    fn interactive_view_cap_defaults_to_64() {
        let mut g = WeightGrid3D::new();
        assert_eq!(g.view_max_dim(), 64);
        // A 128×128 frame displays as 64×64 (stride 2) at the default budget…
        g.on_frame("W", frame(0, 128, 128, vec![1.0; 128 * 128]));
        let bars = g.current_bars().unwrap();
        assert_eq!((bars.rows, bars.cols), (64, 64));
        assert_eq!(bars.bars.len(), 64 * 64);
        // …and at full detail once the resolution select raises the budget.
        g.set_view_max_dim(128);
        let bars = g.current_bars().unwrap();
        assert_eq!((bars.rows, bars.cols), (128, 128));
        // The pure §7.2 path still uses the 256×256 instance cap.
        assert_eq!(subsample_dims(512, 512), (256, 256, 2));
    }

    #[test]
    fn short_frame_is_skipped() {
        // data shorter than rows*cols → None (matches the TS guard).
        let f = frame(0, 4, 4, vec![1.0; 3]);
        assert!(build_bars(&f).is_none());
    }
}
