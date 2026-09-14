//! The pixel-grid loader — Beautiful UI's "Loading State".
//!
//! A 3x3 grid of 4px cells with a 1.5px gap. Each cell pulses on its own
//! `pixel-on` cycle, and the per-cell delays are computed from its position so
//! a wavefront sweeps across the grid:
//!
//! ```text
//! @keyframes pixel-on { 0%,100% { opacity:.15 } 18%,42% { opacity:1 } 62% { opacity:.15 } }
//! ```
//!
//! The source's own note explains the choice of period: *"the 650ms cycle is
//! shorter than the sweep, so two fronts are always in flight."*

use gpui::{div, prelude::*, px, Div};

use super::tokens::BeautifulTokens;

/// Cell count per side.
const N: usize = 3;
/// `size-[4px]`.
const CELL: f32 = 4.0;
/// `gap-[1.5px]`.
const GAP: f32 = 1.5;

/// Which sweep the grid runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LoaderPattern {
    /// Square cells, a chevron wavefront driving right. The default.
    Drive,
    /// The same wavefront with round cells.
    Dots,
    /// A single front walking the ring, centre cell dark.
    Orbit,
}

/// Per-cell animation delay in ms, or `None` for a cell that never animates.
///
/// ```js
/// const chevron = Array.from({length: 9}, (_, i) => {
///   const r = Math.floor(i / 3), c = i % 3;
///   return (c + Math.abs(r - 1)) * 90;
/// });
/// const ORBIT_ORDER = [0,1,2,5,8,7,6,3];
/// ```
pub fn delays(pattern: LoaderPattern) -> [Option<u32>; N * N] {
    match pattern {
        LoaderPattern::Drive | LoaderPattern::Dots => {
            let mut out = [None; N * N];
            for (i, slot) in out.iter_mut().enumerate() {
                let (r, c) = (i / N, i % N);
                // Distance from the middle row plus the column index: a V that
                // opens rightward, which is what makes it read as a direction
                // rather than a shimmer.
                *slot = Some(((c + (r as isize - 1).unsigned_abs()) * 90) as u32);
            }
            out
        }
        LoaderPattern::Orbit => {
            const ORBIT_ORDER: [usize; 8] = [0, 1, 2, 5, 8, 7, 6, 3];
            let mut out = [None; N * N];
            for (k, &cell) in ORBIT_ORDER.iter().enumerate() {
                out[cell] = Some((k * 110) as u32);
            }
            out // index 4, the centre, stays None
        }
    }
}

/// The cycle length for a pattern.
pub fn duration_ms(pattern: LoaderPattern) -> u32 {
    match pattern {
        LoaderPattern::Drive | LoaderPattern::Dots => 650,
        LoaderPattern::Orbit => 950,
    }
}

/// `pixel-on` sampled at `t` within the cycle (`0.0..=1.0`).
///
/// Held as a pure function so the shape is testable: the fully-lit plateau
/// between 18% and 42% is what gives the sweep a visible leading edge, and it
/// is easy to lose by "simplifying" this to a sine.
pub fn pixel_opacity(t: f32) -> f32 {
    const DIM: f32 = 0.15;
    let t = t.rem_euclid(1.0);
    match t {
        t if t < 0.18 => DIM + (1.0 - DIM) * (t / 0.18),
        t if t < 0.42 => 1.0,
        t if t < 0.62 => 1.0 - (1.0 - DIM) * ((t - 0.42) / 0.20),
        _ => DIM,
    }
}

/// Render the grid for a given elapsed time.
///
/// The caller drives `elapsed_ms` from its own repaint loop rather than the
/// element owning a timer: the Explain card already repaints on every streamed
/// delta, so a second animation clock would fight it.
pub fn pixel_loader(t: &BeautifulTokens, pattern: LoaderPattern, elapsed_ms: u64) -> Div {
    let dur = duration_ms(pattern) as f32;
    let ds = delays(pattern);
    let round = matches!(pattern, LoaderPattern::Dots);

    let mut grid = div().flex().flex_col().flex_none().gap(px(GAP));
    for r in 0..N {
        let mut row = div().flex().flex_row().gap(px(GAP));
        for c in 0..N {
            let i = r * N + c;
            let mut cell = div().w(px(CELL)).h(px(CELL)).flex_none().bg(t.ink);
            cell = if round {
                cell.rounded_full()
            } else {
                cell
            };
            let alpha = match ds[i] {
                // A cell with no delay sits below the dim level so the ring
                // reads as a ring rather than a full grid.
                None => 0.07,
                Some(delay) => {
                    let phase = (elapsed_ms as f32 - delay as f32) / dur;
                    pixel_opacity(phase)
                }
            };
            row = row.child(cell.opacity(alpha));
        }
        grid = grid.child(row);
    }
    grid
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chevron_delays_match_the_source_formula() {
        // (c + |r - 1|) * 90 over a row-major 3x3.
        let d = delays(LoaderPattern::Drive);
        let got: Vec<u32> = d.iter().map(|x| x.unwrap()).collect();
        assert_eq!(got, vec![90, 180, 270, 0, 90, 180, 90, 180, 270]);
    }

    #[test]
    fn dots_share_the_chevron_wavefront() {
        assert_eq!(delays(LoaderPattern::Drive), delays(LoaderPattern::Dots));
        assert_eq!(duration_ms(LoaderPattern::Dots), 650);
    }

    /// The orbit walks the ring and leaves the centre dark.
    #[test]
    fn orbit_skips_the_centre_and_walks_the_ring() {
        let d = delays(LoaderPattern::Orbit);
        assert_eq!(d[4], None, "the centre cell must not animate");
        assert_eq!(d[0], Some(0));
        assert_eq!(d[1], Some(110));
        assert_eq!(d[2], Some(220));
        assert_eq!(d[5], Some(330));
        assert_eq!(d[8], Some(440));
        assert_eq!(d[3], Some(770), "the ring closes back up the left edge");
        assert_eq!(d.iter().filter(|x| x.is_some()).count(), 8);
    }

    /// The plateau is the whole point: without it there is no leading edge and
    /// the grid reads as a generic shimmer.
    #[test]
    fn pixel_opacity_has_a_lit_plateau() {
        assert!((pixel_opacity(0.0) - 0.15).abs() < 1e-3);
        assert!((pixel_opacity(0.18) - 1.0).abs() < 1e-3);
        assert!((pixel_opacity(0.30) - 1.0).abs() < 1e-3);
        assert!((pixel_opacity(0.42) - 1.0).abs() < 1e-3);
        assert!((pixel_opacity(0.62) - 0.15).abs() < 1e-3);
        assert!((pixel_opacity(0.99) - 0.15).abs() < 1e-3);
    }

    #[test]
    fn pixel_opacity_stays_in_range_and_wraps() {
        for i in -200..400 {
            let v = pixel_opacity(i as f32 / 100.0);
            assert!((0.0..=1.0).contains(&v), "out of range at {i}: {v}");
        }
        // A negative phase (a cell whose delay has not elapsed) must wrap, not
        // clamp to dark — otherwise the grid starts with a visible hitch.
        assert!((pixel_opacity(-0.5) - pixel_opacity(0.5)).abs() < 1e-5);
    }
}
