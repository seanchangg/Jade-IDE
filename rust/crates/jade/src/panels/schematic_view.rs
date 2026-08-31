//! The schematic view — the third bottom-panel tab in hardware mode.
//!
//! Two modes share one drawing pipeline. The RTL mode draws the
//! [`jade_hw::netlist::Netlist`] the engine extracts on every build; the
//! gate mode draws the Yosys-synthesized gate graph of the same module, so
//! a `*` becomes the real multiplier circuit. A chip in the eyebrow
//! switches between them.
//!
//! Both graphs render in real schematic vocabulary: the distinctive gate silhouettes of
//! MIL-STD-806 / IEEE 91 (the AND's true semicircle, the OR's three-arc
//! shield, the XOR's second back, the NOT's equilateral triangle and
//! bubble), trapezoid muxes, circled arithmetic, diamond comparators,
//! registers with the clock-edge notch, and port flags.
//!
//! Every gate keeps its textbook proportions. The AND arc's radius is half
//! the body height; the OR shield is three 60° arcs of one radius, so its
//! two long edges meet in a sharp point. The pin box is wider than the
//! silhouette, and the leftover width becomes the lead stubs.
//!
//! Everything is painted with `PathBuilder` inside one `canvas` (the training
//! charts' technique); text labels ride on top as positioned divs, because
//! GPUI paths carry no text.
//!
//! Wires are stroked orthogonal polylines with rounded bends; a bus (width
//! > 1) draws thicker and in the brand accent, with its slice annotation at
//! the target pin.
//!
//! The sheet is hoverable. A wire under the pointer repaints its whole net —
//! the driver, every sink, and every branch — in the focus color, on top of
//! everything else, so a route stays readable where wires overlap. A node
//! under the pointer repaints with every wire on it, and a tooltip lists its
//! inputs and outputs. The hit test runs in sheet space: the canvas records
//! its painted origin (Rc<Cell>, the wg3d scrubber's technique) and the
//! mouse listener subtracts it.

use std::cell::Cell;
use std::collections::HashMap;
use std::rc::Rc;

use gpui::{
    canvas, div, point, prelude::*, px, AnyElement, Context, MouseMoveEvent, PathBuilder,
    Pixels, Rgba,
};

use jade_hw::netlist::{Netlist, NodeKind};

use crate::app::JadeApp;
use crate::kumo::scale;
use crate::theme::Theme;

const MARGIN: f32 = 24.0;
const SLOT_W: f32 = 104.0;
const CHAN_W: f32 = 80.0;
const ROW_GAP: f32 = 24.0;
const WIRE: f32 = 1.5;
const BUS: f32 = 2.5;
const BEND_R: f32 = 6.0;
/// A quarter arc's Bézier control offset, as a fraction of the radius.
const KAPPA: f32 = 0.552_284_8;
/// sin(60°) = √3/2. An OR shield is this much longer than it is tall.
const SQ3_2: f32 = 0.866_025_4;
/// A 60° arc's Bézier control offset: 4/3 · tan(15°) · r.
const ARC60: f32 = 0.357_260_5;
/// The shortest lead stub between a pin and a gate silhouette.
const LEAD: f32 = 6.0;
/// The inversion bubble radius.
const BUBBLE: f32 = 4.0;
/// The gap between an XOR's second back and its shield.
const XOR_GAP: f32 = 6.0;

/// The drawable vocabulary a node maps onto.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Glyph {
    And,
    Or,
    Xor,
    Xnor,
    Not,
    Mux,
    /// Circle with an operator character (+ - * / %).
    Arith,
    /// Diamond with a comparison operator.
    Cmp,
    /// Parallelogram (shifts).
    Shift,
    /// Funnel (concat / replicate).
    Concat,
    Reg,
    InPort,
    OutPort,
    /// A named wire or an unrecognized op: plain rounded box.
    Box_,
    /// A literal: bare text, no outline.
    Const,
}

fn glyph_for(kind: NodeKind, label: &str) -> Glyph {
    match kind {
        NodeKind::Input => Glyph::InPort,
        NodeKind::Output => Glyph::OutPort,
        NodeKind::Reg => Glyph::Reg,
        NodeKind::Const => Glyph::Const,
        NodeKind::Wire => Glyph::Box_,
        NodeKind::Op => match label {
            "&" | "&&" => Glyph::And,
            "|" | "||" => Glyph::Or,
            "^" => Glyph::Xor,
            "^~" | "~^" => Glyph::Xnor,
            "~" | "!" => Glyph::Not,
            "MUX" => Glyph::Mux,
            "+" | "-" | "*" | "/" | "%" => Glyph::Arith,
            "==" | "!=" | "<" | ">" | "<=" | ">=" => Glyph::Cmp,
            "<<" | ">>" => Glyph::Shift,
            "{…}" | "{n{…}}" => Glyph::Concat,
            _ => Glyph::Box_,
        },
    }
}

fn glyph_size(g: Glyph) -> (f32, f32) {
    match g {
        // A gate's box spans pin to pin, not edge to edge of the silhouette:
        // it also holds the lead stubs, and the bubble of an inverting gate.
        Glyph::And | Glyph::Or | Glyph::Xor => (60.0, 40.0),
        Glyph::Xnor => (68.0, 40.0),
        Glyph::Not => (52.0, 32.0),
        Glyph::Mux => (40.0, 56.0),
        Glyph::Arith => (42.0, 42.0),
        Glyph::Cmp => (56.0, 40.0),
        Glyph::Shift => (54.0, 34.0),
        Glyph::Concat => (42.0, 52.0),
        Glyph::Reg => (92.0, 52.0),
        Glyph::InPort | Glyph::OutPort => (84.0, 30.0),
        Glyph::Box_ => (84.0, 32.0),
        Glyph::Const => (64.0, 20.0),
    }
}

/// True for the glyphs that draw a silhouette narrower than their pin box,
/// and therefore need lead stubs out to the pins.
fn is_gate(g: Glyph) -> bool {
    matches!(g, Glyph::And | Glyph::Or | Glyph::Xor | Glyph::Xnor | Glyph::Not)
}

/// Where a gate's silhouette sits inside its pin box.
///
/// The silhouette keeps the textbook proportions, so its length follows from
/// the body height alone. The leftover width becomes a lead stub on each
/// side, and an inverting gate spends part of it on the bubble.
///
/// Returns the body's left edge, its length, and the bubble center.
fn gate_body(glyph: Glyph, x: f32, w: f32, h: f32) -> (f32, f32, Option<f32>) {
    if !is_gate(glyph) {
        return (x, w, None);
    }
    let len = match glyph {
        Glyph::And => h,
        Glyph::Xor | Glyph::Xnor => SQ3_2 * h + XOR_GAP,
        _ => SQ3_2 * h,
    };
    let inverts = matches!(glyph, Glyph::Not | Glyph::Xnor);
    let bubble_w = if inverts { BUBBLE * 2.0 } else { 0.0 };
    let bx = x + ((w - len - bubble_w) / 2.0).max(LEAD);
    let bubble = inverts.then_some(bx + len + BUBBLE);
    (bx, len, bubble)
}

/// The x where an input lead meets the silhouette at height `py`. A shield's
/// back is an arc, so every pin lands at its own depth.
fn attach_x(glyph: Glyph, bx: f32, y: f32, h: f32, py: f32) -> f32 {
    match glyph {
        Glyph::Or | Glyph::Xor | Glyph::Xnor => {
            let dy = (py - (y + h * 0.5)).abs().min(h * 0.5);
            bx - SQ3_2 * h + (h * h - dy * dy).sqrt()
        }
        _ => bx,
    }
}

/// What the pointer rests on. The indices point into the current
/// [`Netlist`]'s `nodes` / `edges`; a new netlist clears the hover.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SchematicHover {
    Node(usize),
    Edge(usize),
}

/// How far the pointer can sit from a wire and still take it, in px.
const HIT_TOL: f32 = 5.0;

/// The distance from a point to a segment.
fn seg_dist(x: f32, y: f32, a: (f32, f32), b: (f32, f32)) -> f32 {
    let (dx, dy) = (b.0 - a.0, b.1 - a.1);
    let len2 = dx * dx + dy * dy;
    let t = if len2 <= f32::EPSILON {
        0.0
    } else {
        (((x - a.0) * dx + (y - a.1) * dy) / len2).clamp(0.0, 1.0)
    };
    let (cx, cy) = (a.0 + t * dx, a.1 + t * dy);
    ((x - cx) * (x - cx) + (y - cy) * (y - cy)).sqrt()
}

/// What sits under the sheet-space point. A node box wins over a wire; among
/// wires the nearest segment within [`HIT_TOL`] wins.
fn hit_test(
    x: f32,
    y: f32,
    nodes: &[(usize, NodeBox)],
    wires: &[Vec<(f32, f32)>],
) -> Option<SchematicHover> {
    for &(id, b) in nodes {
        if x >= b.x && x <= b.x + b.w && y >= b.y && y <= b.y + b.h {
            return Some(SchematicHover::Node(id));
        }
    }
    let mut best: Option<(f32, usize)> = None;
    for (ei, pts) in wires.iter().enumerate() {
        for seg in pts.windows(2) {
            let d = seg_dist(x, y, seg[0], seg[1]);
            if d <= HIT_TOL && best.map_or(true, |(bd, _)| d < bd) {
                best = Some((d, ei));
            }
        }
    }
    best.map(|(_, ei)| SchematicHover::Edge(ei))
}

/// Computed geometry for one node.
#[derive(Clone, Copy)]
struct NodeBox {
    x: f32,
    y: f32,
    w: f32,
    h: f32,
    glyph: Glyph,
}

/// One painted primitive, resolved before the canvas closure runs.
enum Draw {
    /// A stroked polyline with rounded bends.
    Wire { pts: Vec<(f32, f32)>, width: f32, color: Rgba },
    /// A filled + outlined closed shape. `pins` holds the y of every input
    /// pin, so a gate can stub a lead out to each one.
    Shape {
        glyph: Glyph,
        x: f32,
        y: f32,
        w: f32,
        h: f32,
        pins: Vec<f32>,
        stroke: Rgba,
        fill: Rgba,
    },
}

/// Stack the nodes of each column and give every node a box. Two barycenter
/// sweeps order each column's rows by where their neighbors sit, so wires
/// run mostly straight and crossings stay rare.
fn layout(nl: &Netlist) -> (Vec<NodeBox>, f32, f32) {
    let columns = nl.columns() as usize;
    let mut by_col: Vec<Vec<usize>> = vec![Vec::new(); columns.max(1)];
    let connected: Vec<bool> = nl
        .nodes
        .iter()
        .map(|n| nl.edges.iter().any(|e| e.from == n.id || e.to == n.id))
        .collect();
    // Constants sit beside their consumer, not in the column stack.
    for n in &nl.nodes {
        if n.kind != NodeKind::Const {
            by_col[n.layer as usize].push(n.id);
        }
    }
    // Unconnected ports sink below the live logic.
    for ids in by_col.iter_mut() {
        ids.sort_by_key(|&id| (!connected[id] as u8, id));
    }
    let mut boxes: Vec<NodeBox> = nl
        .nodes
        .iter()
        .map(|n| {
            let glyph = glyph_for(n.kind, &n.label);
            let (w, h) = glyph_size(glyph);
            NodeBox { x: 0.0, y: 0.0, w, h, glyph }
        })
        .collect();
    let restack = |ids: &[usize], col: usize, boxes: &mut Vec<NodeBox>| {
        let slot_x = MARGIN + col as f32 * (SLOT_W + CHAN_W);
        let mut y = MARGIN;
        for &id in ids {
            let b = &mut boxes[id];
            // Center the shape inside its column slot.
            b.x = slot_x + (SLOT_W - b.w) / 2.0;
            b.y = y;
            y += b.h + ROW_GAP;
        }
    };
    for (col, ids) in by_col.iter().enumerate() {
        restack(ids, col, &mut boxes);
    }
    // Left-to-right sweep: sort by the mean center of the incoming sources;
    // then right-to-left by the outgoing targets.
    let center = |b: &NodeBox| b.y + b.h / 2.0;
    for col in 1..columns {
        let mut keyed: Vec<(f32, usize)> = by_col[col]
            .iter()
            .map(|&id| {
                let srcs: Vec<f32> = nl
                    .edges
                    .iter()
                    .filter(|e| e.to == id)
                    .map(|e| center(&boxes[e.from]))
                    .collect();
                let key = if srcs.is_empty() {
                    center(&boxes[id])
                } else {
                    srcs.iter().sum::<f32>() / srcs.len() as f32
                };
                (key, id)
            })
            .collect();
        keyed.sort_by(|a, b| a.0.total_cmp(&b.0));
        by_col[col] = keyed.into_iter().map(|(_, id)| id).collect();
        restack(&by_col[col], col, &mut boxes);
    }
    for col in (0..columns.saturating_sub(1)).rev() {
        let mut keyed: Vec<(f32, usize)> = by_col[col]
            .iter()
            .map(|&id| {
                let dsts: Vec<f32> = nl
                    .edges
                    .iter()
                    .filter(|e| e.from == id)
                    .map(|e| center(&boxes[e.to]))
                    .collect();
                let key = if dsts.is_empty() {
                    center(&boxes[id])
                } else {
                    dsts.iter().sum::<f32>() / dsts.len() as f32
                };
                (key, id)
            })
            .collect();
        keyed.sort_by(|a, b| a.0.total_cmp(&b.0));
        by_col[col] = keyed.into_iter().map(|(_, id)| id).collect();
        restack(&by_col[col], col, &mut boxes);
    }
    // Center each column vertically against the tallest one, so the sheet
    // reads balanced instead of hanging from the top edge.
    let mut col_span: Vec<(f32, f32)> = vec![(f32::MAX, 0.0); columns.max(1)];
    for n in &nl.nodes {
        if n.kind == NodeKind::Const {
            continue;
        }
        let b = boxes[n.id];
        let span = &mut col_span[n.layer as usize];
        span.0 = span.0.min(b.y);
        span.1 = span.1.max(b.y + b.h);
    }
    let tallest = col_span
        .iter()
        .filter(|(top, _)| *top < f32::MAX)
        .map(|(top, bot)| bot - top)
        .fold(0.0f32, f32::max);
    for n in &nl.nodes {
        if n.kind == NodeKind::Const {
            continue;
        }
        let (top, bot) = col_span[n.layer as usize];
        if top < f32::MAX {
            boxes[n.id].y += (tallest - (bot - top)) / 2.0;
        }
    }

    // Constants: directly left of the pin they feed, clear of the stack.
    for n in &nl.nodes {
        if n.kind != NodeKind::Const {
            continue;
        }
        if let Some(e) = nl.edges.iter().find(|e| e.from == n.id) {
            let to = boxes[e.to];
            let pin_n = nl
                .edges
                .iter()
                .filter(|x| x.to == e.to)
                .map(|x| x.to_pin + 1)
                .max()
                .unwrap_or(1) as f32;
            let ty = to.y + to.h * (e.to_pin as f32 + 1.0) / (pin_n + 1.0);
            let b = &mut boxes[n.id];
            b.x = to.x - b.w - 16.0;
            b.y = ty - b.h / 2.0;
        }
    }
    let max_bottom = boxes.iter().map(|b| b.y + b.h).fold(0.0f32, f32::max);
    let total_w = MARGIN * 2.0 + columns as f32 * (SLOT_W + CHAN_W) - CHAN_W;
    (boxes, total_w, max_bottom + MARGIN)
}

// ── Path painting ───────────────────────────────────────────────────────────

fn p(x: f32, y: f32, ox: f32, oy: f32) -> gpui::Point<Pixels> {
    point(px(x + ox), px(y + oy))
}

/// Stroke a polyline with rounded bends.
fn paint_wire(
    window: &mut gpui::Window,
    pts: &[(f32, f32)],
    ox: f32,
    oy: f32,
    width: f32,
    color: Rgba,
) {
    if pts.len() < 2 {
        return;
    }
    let mut b = PathBuilder::stroke(px(width));
    b.move_to(p(pts[0].0, pts[0].1, ox, oy));
    for i in 1..pts.len() {
        let (cx, cy) = pts[i];
        if i + 1 < pts.len() {
            // Shorten into the corner, then curve through it.
            let (nx, ny) = pts[i + 1];
            let (pxx, pyy) = pts[i - 1];
            let din = ((cx - pxx).abs() + (cy - pyy).abs()).min(BEND_R);
            let dout = ((nx - cx).abs() + (ny - cy).abs()).min(BEND_R);
            let ix = cx - (cx - pxx).signum() * if cy == pyy { din } else { 0.0 };
            let iy = cy - (cy - pyy).signum() * if cx == pxx { din } else { 0.0 };
            let out_x = cx + (nx - cx).signum() * if ny == cy { dout } else { 0.0 };
            let out_y = cy + (ny - cy).signum() * if nx == cx { dout } else { 0.0 };
            b.line_to(p(ix, iy, ox, oy));
            b.curve_to(p(out_x, out_y, ox, oy), p(cx, cy, ox, oy));
        } else {
            b.line_to(p(cx, cy, ox, oy));
        }
    }
    if let Ok(path) = b.build() {
        window.paint_path(path, color);
    }
}

/// The AND silhouette: a rectangle that a true semicircle of radius h/2
/// closes on the right.
fn and_body(b: &mut PathBuilder, bx: f32, y: f32, len: f32, h: f32, ox: f32, oy: f32) {
    let r = h * 0.5;
    let (ex, mid, k) = (bx + len, y + r, r * KAPPA);
    b.move_to(p(bx, y, ox, oy));
    b.line_to(p(ex - r, y, ox, oy));
    b.cubic_bezier_to(p(ex, mid, ox, oy), p(ex - r + k, y, ox, oy), p(ex, mid - k, ox, oy));
    b.cubic_bezier_to(
        p(ex - r, y + h, ox, oy),
        p(ex, mid + k, ox, oy),
        p(ex - r + k, y + h, ox, oy),
    );
    b.line_to(p(bx, y + h, ox, oy));
    b.line_to(p(bx, y, ox, oy));
}

/// The OR silhouette: three 60° arcs of radius `h`. Each long edge turns
/// about the opposite back corner, so the two meet in a sharp 60° point at
/// `bx + √3/2·h`; the back turns about a center off to the left and bulges
/// into the body by 0.134·h.
fn shield(b: &mut PathBuilder, bx: f32, y: f32, h: f32, ox: f32, oy: f32) {
    let mid = y + h * 0.5;
    let tip = bx + SQ3_2 * h;
    let k = ARC60 * h;
    let (kx, ky) = (k * 0.5, k * SQ3_2);
    b.move_to(p(bx, y, ox, oy));
    b.cubic_bezier_to(p(tip, mid, ox, oy), p(bx + k, y, ox, oy), p(tip - kx, mid - ky, ox, oy));
    b.cubic_bezier_to(
        p(bx, y + h, ox, oy),
        p(tip - kx, mid + ky, ox, oy),
        p(bx + k, y + h, ox, oy),
    );
    back_arc(b, bx, y, h, ox, oy);
}

/// A shield's back on its own: the XOR draws a second one, one gap further
/// left. The curve runs bottom to top, to close the shield's outline.
fn back_arc(b: &mut PathBuilder, bx: f32, y: f32, h: f32, ox: f32, oy: f32) {
    let k = ARC60 * h;
    let (kx, ky) = (k * 0.5, k * SQ3_2);
    b.cubic_bezier_to(
        p(bx, y, ox, oy),
        p(bx + kx, y + h - ky, ox, oy),
        p(bx + kx, y + ky, ox, oy),
    );
}

/// A circle from four quarter arcs: the inversion bubble, and the body of
/// the arithmetic glyph.
fn circle(b: &mut PathBuilder, cx: f32, cy: f32, r: f32, ox: f32, oy: f32) {
    let k = r * KAPPA;
    b.move_to(p(cx, cy - r, ox, oy));
    b.cubic_bezier_to(p(cx + r, cy, ox, oy), p(cx + k, cy - r, ox, oy), p(cx + r, cy - k, ox, oy));
    b.cubic_bezier_to(p(cx, cy + r, ox, oy), p(cx + r, cy + k, ox, oy), p(cx + k, cy + r, ox, oy));
    b.cubic_bezier_to(p(cx - r, cy, ox, oy), p(cx - k, cy + r, ox, oy), p(cx - r, cy + k, ox, oy));
    b.cubic_bezier_to(p(cx, cy - r, ox, oy), p(cx - r, cy - k, ox, oy), p(cx - k, cy - r, ox, oy));
}

/// Build the closed outline of a glyph as a fill.
fn shape_path(glyph: Glyph, x: f32, y: f32, w: f32, h: f32, ox: f32, oy: f32) -> PathBuilder {
    let mut b = PathBuilder::fill();
    replay_shape(&mut b, glyph, x, y, w, h, ox, oy);
    b
}

/// Fill + outline one glyph, plus its lead stubs and its decorations (the
/// inversion bubble, the XOR's second back, the register's clock notch).
#[allow(clippy::too_many_arguments)]
fn paint_shape(
    window: &mut gpui::Window,
    glyph: Glyph,
    x: f32,
    y: f32,
    w: f32,
    h: f32,
    pins: &[f32],
    ox: f32,
    oy: f32,
    stroke: Rgba,
    fill: Rgba,
) {
    if glyph == Glyph::Const {
        return; // literals are text only
    }
    let (bx, len, bubble) = gate_body(glyph, x, w, h);
    let mid = y + h * 0.5;
    if let Ok(path) = shape_path(glyph, x, y, w, h, ox, oy).build() {
        window.paint_path(path, fill);
    }
    // Outline: the same geometry through a stroke-style builder.
    if let Ok(path) = stroked(glyph, x, y, w, h, ox, oy).build() {
        window.paint_path(path, stroke);
    }

    if matches!(glyph, Glyph::Xor | Glyph::Xnor) {
        // The second back, one gap left of the shield's own.
        let mut b = PathBuilder::stroke(px(1.5));
        b.move_to(p(bx, y + h, ox, oy));
        back_arc(&mut b, bx, y, h, ox, oy);
        if let Ok(path) = b.build() {
            window.paint_path(path, stroke);
        }
    }
    if let Some(cx) = bubble {
        // The inversion bubble, tangent to the tip.
        let mut f = PathBuilder::fill();
        circle(&mut f, cx, mid, BUBBLE, ox, oy);
        if let Ok(path) = f.build() {
            window.paint_path(path, fill);
        }
        let mut b = PathBuilder::stroke(px(1.5));
        circle(&mut b, cx, mid, BUBBLE, ox, oy);
        if let Ok(path) = b.build() {
            window.paint_path(path, stroke);
        }
    }
    if is_gate(glyph) {
        // Lead stubs: each input pin out to the silhouette, and the output
        // pin back to the tip.
        let mut b = PathBuilder::stroke(px(WIRE));
        let mut any = false;
        for &py in pins {
            let ax = attach_x(glyph, bx, y, h, py);
            if ax > x + 0.5 {
                b.move_to(p(x, py, ox, oy));
                b.line_to(p(ax, py, ox, oy));
                any = true;
            }
        }
        let out = bubble.map_or(bx + len, |cx| cx + BUBBLE);
        if x + w > out + 0.5 {
            b.move_to(p(out, mid, ox, oy));
            b.line_to(p(x + w, mid, ox, oy));
            any = true;
        }
        if any {
            if let Ok(path) = b.build() {
                window.paint_path(path, stroke);
            }
        }
    }
    if glyph == Glyph::Reg {
        // Clock-edge notch on the lower left edge.
        let ny = y + h - 12.0;
        let mut b = PathBuilder::stroke(px(1.5));
        b.move_to(p(x, ny - 5.0, ox, oy));
        b.line_to(p(x + 8.0, ny, ox, oy));
        b.line_to(p(x, ny + 5.0, ox, oy));
        if let Ok(path) = b.build() {
            window.paint_path(path, stroke);
        }
    }
}

/// The stroked twin of [`shape_path`]: PathBuilder fixes fill/stroke at
/// construction, so the outline replays the same geometry into a stroke
/// builder.
fn stroked(glyph: Glyph, x: f32, y: f32, w: f32, h: f32, ox: f32, oy: f32) -> PathBuilder {
    let mut b = PathBuilder::stroke(px(1.5));
    replay_shape(&mut b, glyph, x, y, w, h, ox, oy);
    b
}

fn replay_shape(b: &mut PathBuilder, glyph: Glyph, x: f32, y: f32, w: f32, h: f32, ox: f32, oy: f32) {
    let (bx, len, _) = gate_body(glyph, x, w, h);
    match glyph {
        Glyph::And => and_body(b, bx, y, len, h, ox, oy),
        Glyph::Or => shield(b, bx, y, h, ox, oy),
        // The second back stands clear on the left, so the shield starts one
        // gap in and the body still ends at `bx + len`.
        Glyph::Xor | Glyph::Xnor => shield(b, bx + XOR_GAP, y, h, ox, oy),
        Glyph::Not => {
            // An equilateral triangle: the tip sits √3/2·h from the back.
            b.move_to(p(bx, y, ox, oy));
            b.line_to(p(bx + len, y + h * 0.5, ox, oy));
            b.line_to(p(bx, y + h, ox, oy));
            b.line_to(p(bx, y, ox, oy));
        }
        Glyph::Mux => {
            b.move_to(p(x, y, ox, oy));
            b.line_to(p(x + w, y + h * 0.22, ox, oy));
            b.line_to(p(x + w, y + h * 0.78, ox, oy));
            b.line_to(p(x, y + h, ox, oy));
            b.line_to(p(x, y, ox, oy));
        }
        Glyph::Arith => circle(b, x + w / 2.0, y + h / 2.0, w.min(h) / 2.0, ox, oy),
        Glyph::Cmp => {
            let (cx, cy) = (x + w / 2.0, y + h / 2.0);
            b.move_to(p(cx, y, ox, oy));
            b.line_to(p(x + w, cy, ox, oy));
            b.line_to(p(cx, y + h, ox, oy));
            b.line_to(p(x, cy, ox, oy));
            b.line_to(p(cx, y, ox, oy));
        }
        Glyph::Shift => {
            let s = w * 0.18;
            b.move_to(p(x + s, y, ox, oy));
            b.line_to(p(x + w, y, ox, oy));
            b.line_to(p(x + w - s, y + h, ox, oy));
            b.line_to(p(x, y + h, ox, oy));
            b.line_to(p(x + s, y, ox, oy));
        }
        Glyph::Concat => {
            b.move_to(p(x, y, ox, oy));
            b.line_to(p(x + w * 0.55, y, ox, oy));
            b.line_to(p(x + w, y + h * 0.35, ox, oy));
            b.line_to(p(x + w, y + h * 0.65, ox, oy));
            b.line_to(p(x + w * 0.55, y + h, ox, oy));
            b.line_to(p(x, y + h, ox, oy));
            b.line_to(p(x, y, ox, oy));
        }
        Glyph::Reg | Glyph::Box_ | Glyph::Const => {
            b.move_to(p(x, y, ox, oy));
            b.line_to(p(x + w, y, ox, oy));
            b.line_to(p(x + w, y + h, ox, oy));
            b.line_to(p(x, y + h, ox, oy));
            b.line_to(p(x, y, ox, oy));
        }
        Glyph::InPort => {
            let tip = 10.0;
            b.move_to(p(x, y, ox, oy));
            b.line_to(p(x + w - tip, y, ox, oy));
            b.line_to(p(x + w, y + h / 2.0, ox, oy));
            b.line_to(p(x + w - tip, y + h, ox, oy));
            b.line_to(p(x, y + h, ox, oy));
            b.line_to(p(x, y, ox, oy));
        }
        Glyph::OutPort => {
            let tip = 10.0;
            b.move_to(p(x + tip, y, ox, oy));
            b.line_to(p(x + w, y, ox, oy));
            b.line_to(p(x + w, y + h, ox, oy));
            b.line_to(p(x + tip, y + h, ox, oy));
            b.line_to(p(x, y + h / 2.0, ox, oy));
            b.line_to(p(x + tip, y, ox, oy));
        }
    }
}

// ── The view ────────────────────────────────────────────────────────────────

/// The floating eyebrow chip: the module name plus the RTL/gates switch.
fn mode_eyebrow(
    t: &crate::kumo::tokens::KumoTokens,
    module: Option<String>,
    gates_mode: bool,
    cx: &mut Context<JadeApp>,
) -> AnyElement {
    let seg = |id: &'static str, label: &'static str, want: bool| {
        let active = gates_mode == want;
        let mut d = div()
            .id(id)
            .h(px(14.))
            .px(px(5.))
            .flex()
            .items_center()
            .rounded(px(3.))
            .cursor_pointer()
            .text_color(if active { t.text_default } else { t.text_subtle })
            .child(label);
        if active {
            d = d.bg(t.tint);
        }
        d.on_click(cx.listener(move |a: &mut JadeApp, _e, _w, cx| {
            if a.hw.as_ref().is_some_and(|h| h.schematic_gates != want) {
                a.hw_toggle_schematic_gates();
                cx.notify();
            }
        }))
    };
    let rtl = seg("schematic-mode-rtl", "RTL", false);
    let gates = seg("schematic-mode-gates", "gates", true);
    let mut chip = div()
        .absolute()
        .left(px(12.))
        .top(px(10.))
        .h(px(20.))
        .px(px(6.))
        .flex()
        .items_center()
        .gap(px(6.))
        .rounded(px(4.))
        .bg(t.elevated)
        .border_1()
        .border_color(t.hairline)
        .text_size(px(10.0))
        .font_family(crate::fonts::mono_family());
    if let Some(m) = module {
        chip = chip
            .child(div().text_color(t.text_subtle).child("module"))
            .child(div().text_color(t.text_default).child(m))
            .child(div().w(px(1.)).h(px(12.)).bg(t.hairline));
    }
    chip.child(rtl).child(gates).into_any_element()
}

pub fn render(
    app: &JadeApp,
    theme: &Theme,
    cx: &mut Context<JadeApp>,
    avail_w: f32,
    avail_h: f32,
) -> AnyElement {
    let t = theme.kumo.clone();
    let gates_mode = app.hw.as_ref().map(|h| h.schematic_gates).unwrap_or(false);
    let nl_opt = app.hw.as_ref().and_then(|h| {
        if gates_mode {
            h.gate_netlist.clone()
        } else {
            h.netlist.clone()
        }
    });
    let Some(nl) = nl_opt else {
        // The empty sheet keeps the mode switch, so the user can leave a
        // mode that has nothing to show yet.
        let msg = if let Some(hint) = app
            .hw
            .as_ref()
            .filter(|_| gates_mode)
            .and_then(|h| h.synth_missing.clone())
        {
            format!("The gate view needs yosys. Install it with: {hint}")
        } else if gates_mode {
            if app.hw.as_ref().map(|h| h.synthesizing).unwrap_or(false) {
                "Yosys synthesizes the gate netlist…".to_string()
            } else {
                "The gate view appears after the first synthesis.".to_string()
            }
        } else {
            "The schematic appears after the first successful build.".to_string()
        };
        let eyebrow = mode_eyebrow(&t, None, gates_mode, cx);
        return div()
            .relative()
            .flex_1()
            .min_h(px(0.))
            .child(
                div()
                    .size_full()
                    .flex()
                    .items_center()
                    .justify_center()
                    .text_size(scale::TEXT_SM)
                    .text_color(t.text_subtle)
                    .child(msg),
            )
            .child(eyebrow)
            .into_any_element();
    };

    let (mut boxes, total_w, body_h) = layout(&nl);
    // Center the sheet inside the visible panel; a diagram larger than the
    // panel keeps its margins and scrolls.
    let pad_x = ((avail_w - total_w) / 2.0).max(0.0);
    let pad_y = ((avail_h - body_h) / 2.0).max(0.0);
    for b in boxes.iter_mut() {
        b.x += pad_x;
        b.y += pad_y;
    }
    let sheet_w = total_w + pad_x * 2.0;

    // Input pin count per node, for pin spacing on the left edge.
    let mut pins: HashMap<usize, u32> = HashMap::new();
    for e in &nl.edges {
        let pin = pins.entry(e.to).or_insert(0);
        *pin = (*pin).max(e.to_pin + 1);
    }

    // ── Resolve every primitive up front ──
    let mut draws: Vec<Draw> = Vec::new();
    let mut labels: Vec<AnyElement> = Vec::new();
    let wire_color = t.text_subtle;
    let bus_color = t.brand;
    let mut channel_use: HashMap<u32, f32> = HashMap::new();
    let mut feedback_lanes = 0usize;
    let mut max_wire_y = 0.0f32;
    // Every edge's polyline, by edge index, kept for the hover hit test and
    // for the highlight repaint.
    let mut wire_pts: Vec<Vec<(f32, f32)>> = Vec::with_capacity(nl.edges.len());

    // One shape primitive for a node, so the hover repaint uses the same
    // geometry as the base pass and only the stroke color changes.
    let shape_for = {
        let boxes = &boxes;
        let pins = &pins;
        move |id: usize, stroke: Rgba, fill: Rgba| -> Draw {
            let b = boxes[id];
            let n_pins = pins.get(&id).copied().unwrap_or(0);
            Draw::Shape {
                glyph: b.glyph,
                x: b.x,
                y: b.y,
                w: b.w,
                h: b.h,
                pins: (0..n_pins)
                    .map(|i| b.y + b.h * (i as f32 + 1.0) / (n_pins as f32 + 1.0))
                    .collect(),
                stroke,
                fill,
            }
        }
    };

    for e in &nl.edges {
        let from = boxes[e.from];
        let to = boxes[e.to];
        let n_pins = pins.get(&e.to).copied().unwrap_or(1).max(1) as f32;
        let sy = from.y + from.h / 2.0;
        let ty = to.y + to.h * (e.to_pin as f32 + 1.0) / (n_pins + 1.0);
        let sx = from.x + from.w;
        let tx = to.x;
        let (width, color) = if e.width > 1 {
            (BUS, bus_color)
        } else {
            (WIRE, wire_color)
        };
        if sx <= tx {
            // Forward: across the channel, staggered per channel.
            let col = nl.nodes[e.to].layer;
            let slot = channel_use.entry(col).or_insert(0.0);
            let midx = tx - CHAN_W * 0.45 - *slot;
            *slot += 7.0;
            let pts = vec![(sx, sy), (midx, sy), (midx, ty), (tx, ty)];
            wire_pts.push(pts.clone());
            draws.push(Draw::Wire { pts, width, color });
        } else {
            // Feedback into a register: loop below every box in the spanned
            // columns, so the wire never crosses another symbol.
            feedback_lanes += 1;
            let lane = feedback_lanes as f32;
            let (lo, hi) = (
                nl.nodes[e.to].layer.min(nl.nodes[e.from].layer),
                nl.nodes[e.to].layer.max(nl.nodes[e.from].layer),
            );
            let clear = nl
                .nodes
                .iter()
                .filter(|n2| n2.layer >= lo && n2.layer <= hi)
                .map(|n2| boxes[n2.id].y + boxes[n2.id].h)
                .fold(0.0f32, f32::max);
            let lane_y = clear + 14.0 + lane * 8.0;
            max_wire_y = max_wire_y.max(lane_y);
            let outx = sx + 8.0 + lane * 5.0;
            let inx = (tx - 12.0 - lane * 5.0).max(4.0);
            let pts = vec![
                (sx, sy),
                (outx, sy),
                (outx, lane_y),
                (inx, lane_y),
                (inx, ty),
                (tx, ty),
            ];
            wire_pts.push(pts.clone());
            draws.push(Draw::Wire { pts, width, color });
        }
        // Slice annotation: a chip ON the wire, haloed by the panel surface
        // so the stroke never runs through the text.
        if let Some(note) = e.label.clone() {
            labels.push(
                div()
                    .absolute()
                    .left(px((tx - CHAN_W * 0.72).max(2.0)))
                    .top(px(ty - 8.0))
                    .h(px(16.0))
                    .px(px(4.0))
                    .flex()
                    .items_center()
                    .rounded(px(3.0))
                    .bg(t.elevated)
                    .text_size(px(9.0))
                    .font_family(crate::fonts::mono_family())
                    .text_color(t.text_subtle)
                    .child(note)
                    .into_any_element(),
            );
        }
    }

    for n in &nl.nodes {
        let b = boxes[n.id];
        let (stroke, ink) = match n.kind {
            NodeKind::Input => (t.success, t.success),
            NodeKind::Output => (t.brand, t.brand),
            NodeKind::Reg => (t.warning, t.text_default),
            NodeKind::Const => (t.hairline, t.text_subtle),
            _ => (t.text_subtle, t.text_default),
        };
        draws.push(shape_for(n.id, stroke, t.elevated));

        // ── Text overlay per glyph ──
        let mono = crate::fonts::mono_family();
        match b.glyph {
            Glyph::InPort | Glyph::OutPort | Glyph::Box_ => {
                let tag = if n.width > 1 { format!("  {}b", n.width) } else { String::new() };
                labels.push(
                    div()
                        .absolute()
                        .left(px(b.x))
                        .top(px(b.y))
                        .w(px(b.w))
                        .h(px(b.h))
                        .flex()
                        .items_center()
                        .justify_center()
                        .text_size(px(11.0))
                        .font_family(mono.clone())
                        .text_color(ink)
                        .child(format!("{}{}", n.label, tag))
                        .into_any_element(),
                );
            }
            Glyph::Reg => {
                labels.push(
                    div()
                        .absolute()
                        .left(px(b.x))
                        .top(px(b.y + 6.0))
                        .w(px(b.w))
                        .flex()
                        .flex_col()
                        .items_center()
                        .text_size(px(11.0))
                        .font_family(mono.clone())
                        .text_color(ink)
                        .child(n.label.clone())
                        .child(
                            div()
                                .text_size(px(8.5))
                                .text_color(t.warning)
                                .child(format!(
                                    "{}b · {}",
                                    n.width,
                                    n.clock.clone().unwrap_or_default()
                                )),
                        )
                        .into_any_element(),
                );
            }
            Glyph::Const => {
                labels.push(
                    div()
                        .absolute()
                        .left(px(b.x))
                        .top(px(b.y))
                        .w(px(b.w))
                        .h(px(b.h))
                        .flex()
                        .items_center()
                        .justify_end()
                        .child(
                            div()
                                .h(px(16.0))
                                .px(px(4.0))
                                .flex()
                                .items_center()
                                .rounded(px(3.0))
                                .bg(t.elevated)
                                .text_size(px(10.0))
                                .font_family(mono.clone())
                                .text_color(t.text_subtle)
                                .child(n.label.clone()),
                        )
                        .into_any_element(),
                );
            }
            Glyph::Arith | Glyph::Cmp | Glyph::Shift => {
                labels.push(
                    div()
                        .absolute()
                        .left(px(b.x))
                        .top(px(b.y))
                        .w(px(b.w))
                        .h(px(b.h))
                        .flex()
                        .items_center()
                        .justify_center()
                        .text_size(px(13.0))
                        .font_family(mono.clone())
                        .text_color(ink)
                        .child(n.label.clone())
                        .into_any_element(),
                );
            }
            Glyph::Mux => {
                // Pin marks: select on top, then the two data legs.
                for (pin, mark) in [(0u32, "s"), (1, "1"), (2, "0")] {
                    let n_pins = pins.get(&n.id).copied().unwrap_or(3).max(1) as f32;
                    let py2 = b.y + b.h * (pin as f32 + 1.0) / (n_pins + 1.0);
                    labels.push(
                        div()
                            .absolute()
                            .left(px(b.x + 5.0))
                            .top(px(py2 - 7.0))
                            .text_size(px(9.0))
                            .font_family(mono.clone())
                            .text_color(t.text_subtle)
                            .child(mark.to_string())
                            .into_any_element(),
                    );
                }
            }
            Glyph::And | Glyph::Or | Glyph::Xor | Glyph::Xnor | Glyph::Not | Glyph::Concat => {
                // The silhouette carries the meaning; the funnel adds a tag.
                if n.label == "{…}" || n.label == "{n{…}}" {
                    let (bx, len, _) = gate_body(b.glyph, b.x, b.w, b.h);
                    labels.push(
                        div()
                            .absolute()
                            .left(px(bx + len * 0.16))
                            .top(px(b.y + b.h / 2.0 - 7.0))
                            .w(px(len * 0.5))
                            .flex()
                            .justify_center()
                            .text_size(px(10.0))
                            .font_family(mono.clone())
                            .text_color(ink)
                            .child("{}")
                            .into_any_element(),
                    );
                }
            }
        }
    }

    // ── Hover: repaint the pointed-at net or node on top, in focus color ──
    let hover = app
        .hw
        .as_ref()
        .and_then(|h| h.schematic_hover)
        .filter(|h| match h {
            SchematicHover::Node(id) => *id < nl.nodes.len(),
            SchematicHover::Edge(ei) => *ei < nl.edges.len(),
        });
    let halo = {
        let mut c = t.focus;
        c.a = 0.20;
        c
    };
    let wire_w = |ei: usize| if nl.edges[ei].width > 1 { BUS } else { WIRE };
    let mono = crate::fonts::mono_family();
    let tooltip = |x: f32, y: f32| {
        div()
            .absolute()
            .left(px(x))
            .top(px(y))
            .flex()
            .flex_col()
            .gap(px(2.0))
            .px(px(8.0))
            .py(px(6.0))
            .rounded(px(4.0))
            .bg(t.elevated)
            .border_1()
            .border_color(t.focus)
            .text_size(px(10.0))
            .font_family(mono.clone())
    };
    match hover {
        Some(SchematicHover::Edge(ei)) => {
            // The full route: every edge the hovered wire's driver feeds.
            // Halo first, then the crisp line, both above every base draw,
            // so the net stays readable where wires overlap.
            let driver = nl.edges[ei].from;
            let net: Vec<usize> = (0..nl.edges.len())
                .filter(|&i| nl.edges[i].from == driver)
                .collect();
            draws.push(shape_for(driver, t.focus, t.elevated));
            for &i in &net {
                draws.push(shape_for(nl.edges[i].to, t.focus, t.elevated));
            }
            for &i in &net {
                draws.push(Draw::Wire {
                    pts: wire_pts[i].clone(),
                    width: wire_w(i) + 5.0,
                    color: halo,
                });
            }
            for &i in &net {
                draws.push(Draw::Wire {
                    pts: wire_pts[i].clone(),
                    width: wire_w(i) + 1.0,
                    color: t.focus,
                });
            }
            // The route chip, at the hovered wire's middle segment.
            let e = &nl.edges[ei];
            let pts = &wire_pts[ei];
            let k = (pts.len() - 1) / 2;
            let (a, b2) = (pts[k], pts[k + 1]);
            let (mx, my) = ((a.0 + b2.0) / 2.0, (a.1 + b2.1) / 2.0);
            let mut meta = format!("{}b", e.width);
            if let Some(s) = &e.label {
                meta = format!("{s} · {meta}");
            }
            if net.len() > 1 {
                meta.push_str(&format!(" · {} sinks", net.len()));
            }
            labels.push(
                tooltip((mx + 10.0).min(sheet_w - 150.0).max(2.0), (my - 34.0).max(2.0))
                    .child(
                        div()
                            .flex()
                            .gap(px(6.0))
                            .child(div().text_color(t.text_default).child(format!(
                                "{} → {}",
                                nl.nodes[e.from].label, nl.nodes[e.to].label
                            )))
                            .child(div().text_color(t.text_subtle).child(meta)),
                    )
                    .into_any_element(),
            );
        }
        Some(SchematicHover::Node(id)) => {
            for (i, e) in nl.edges.iter().enumerate() {
                if e.from == id || e.to == id {
                    draws.push(Draw::Wire {
                        pts: wire_pts[i].clone(),
                        width: wire_w(i) + 5.0,
                        color: halo,
                    });
                }
            }
            for (i, e) in nl.edges.iter().enumerate() {
                if e.from == id || e.to == id {
                    draws.push(Draw::Wire {
                        pts: wire_pts[i].clone(),
                        width: wire_w(i) + 1.0,
                        color: t.focus,
                    });
                }
            }
            draws.push(shape_for(id, t.focus, t.elevated));
            // The card: what the node is, then every input and output.
            let n = &nl.nodes[id];
            let b = boxes[id];
            let kind_word = match n.kind {
                NodeKind::Input => "input",
                NodeKind::Output => "output",
                NodeKind::Reg => "register",
                NodeKind::Const => "constant",
                NodeKind::Wire => "net",
                NodeKind::Op => "operator",
            };
            let mut meta = format!("{kind_word} · {}b", n.width);
            if let Some(c) = &n.clock {
                meta.push_str(&format!(" · {c}"));
            }
            let pin_name = |pin: u32| -> String {
                if b.glyph == Glyph::Mux {
                    match pin {
                        0 => "s".into(),
                        1 => "1".into(),
                        2 => "0".into(),
                        p => p.to_string(),
                    }
                } else {
                    pin.to_string()
                }
            };
            let mut ins: Vec<(u32, String)> = nl
                .edges
                .iter()
                .filter(|e| e.to == id)
                .map(|e| {
                    let mut s = nl.nodes[e.from].label.clone();
                    if let Some(slice) = &e.label {
                        s.push_str(&format!(" {slice}"));
                    }
                    if e.width > 1 {
                        s.push_str(&format!(" · {}b", e.width));
                    }
                    (e.to_pin, s)
                })
                .collect();
            ins.sort_by_key(|(p, _)| *p);
            let outs: Vec<String> = nl
                .edges
                .iter()
                .filter(|e| e.from == id)
                .map(|e| nl.nodes[e.to].label.clone())
                .collect();
            let card_x = if b.x + b.w + 172.0 <= sheet_w {
                b.x + b.w + 12.0
            } else {
                (b.x - 172.0).max(2.0)
            };
            let mut card = tooltip(card_x, b.y.max(2.0)).child(
                div()
                    .flex()
                    .gap(px(6.0))
                    .child(div().text_color(t.text_default).child(n.label.clone()))
                    .child(div().text_color(t.text_subtle).child(meta)),
            );
            if !ins.is_empty() {
                card = card
                    .child(div().text_size(px(8.5)).text_color(t.text_subtle).child("inputs"));
                for (pin, s) in ins {
                    card = card.child(
                        div()
                            .flex()
                            .gap(px(6.0))
                            .child(div().text_color(t.text_subtle).child(pin_name(pin)))
                            .child(div().text_color(t.text_default).child(s)),
                    );
                }
            }
            if !outs.is_empty() {
                card = card
                    .child(div().text_size(px(8.5)).text_color(t.text_subtle).child("outputs"));
                for s in outs {
                    card = card.child(div().text_color(t.text_default).child(format!("→ {s}")));
                }
            }
            labels.push(card.into_any_element());
        }
        None => {}
    }

    let sheet_h = (body_h + pad_y * 2.0).max(max_wire_y + 16.0 + MARGIN);

    // ── One canvas paints the grid paper, every wire, and every silhouette ──
    let grid_color = {
        let mut c = t.text_subtle;
        c.a = 0.13;
        c
    };
    let (grid_w, grid_h) = (sheet_w, sheet_h);
    // The canvas records its painted origin so the mouse listener below can
    // map window space → sheet space (the wg3d scrubber's technique).
    let origin: Rc<Cell<(f32, f32)>> = Rc::new(Cell::new((0.0, 0.0)));
    let o_paint = origin.clone();
    let o_move = origin;
    let hit_nodes: Vec<(usize, NodeBox)> =
        nl.nodes.iter().map(|n| (n.id, boxes[n.id])).collect();
    let hit_wires: Vec<Vec<(f32, f32)>> = wire_pts;
    let paint = canvas(
        |_, _, _| (),
        move |bounds: gpui::Bounds<Pixels>, _, window, _| {
            let ox = f32::from(bounds.origin.x);
            let oy = f32::from(bounds.origin.y);
            o_paint.set((ox, oy));
            // Grid paper: a quiet 24px dot lattice, the schematic's ground.
            let mut gy = 12.0;
            while gy < grid_h {
                let mut gx = 12.0;
                while gx < grid_w {
                    window.paint_quad(gpui::fill(
                        gpui::Bounds {
                            origin: point(px(gx + ox), px(gy + oy)),
                            size: gpui::size(px(1.5), px(1.5)),
                        },
                        grid_color,
                    ));
                    gx += 24.0;
                }
                gy += 24.0;
            }
            for d in &draws {
                match d {
                    Draw::Wire { pts, width, color } => {
                        paint_wire(window, pts, ox, oy, *width, *color)
                    }
                    Draw::Shape { glyph, x, y, w, h, pins, stroke, fill } => {
                        paint_shape(window, *glyph, *x, *y, *w, *h, pins, ox, oy, *stroke, *fill)
                    }
                }
            }
        },
    )
    .absolute()
    .left(px(0.))
    .top(px(0.))
    .w(px(sheet_w))
    .h(px(sheet_h));

    let mut sheet = div()
        .id("schematic-sheet")
        .relative()
        .w(px(sheet_w))
        .h(px(sheet_h))
        .flex_none()
        .on_mouse_move(cx.listener(move |app: &mut JadeApp, ev: &MouseMoveEvent, _w, cx| {
            let (ox, oy) = o_move.get();
            let x = f32::from(ev.position.x) - ox;
            let y = f32::from(ev.position.y) - oy;
            let next = hit_test(x, y, &hit_nodes, &hit_wires);
            if let Some(hw) = &mut app.hw {
                if hw.schematic_hover != next {
                    hw.schematic_hover = next;
                    cx.notify();
                }
            }
        }))
        .on_hover(cx.listener(|app: &mut JadeApp, hovered: &bool, _w, cx| {
            if !hovered {
                if let Some(hw) = &mut app.hw {
                    if hw.schematic_hover.is_some() {
                        hw.schematic_hover = None;
                        cx.notify();
                    }
                }
            }
        }))
        .child(paint);
    for l in labels {
        sheet = sheet.child(l);
    }

    // The module eyebrow floats over the sheet, pinned to the panel corner.
    let eyebrow = mode_eyebrow(&t, Some(nl.top.clone()), gates_mode, cx);

    div()
        .relative()
        .flex_1()
        .min_h(px(0.))
        .child(
            div()
                .id("schematic-body")
                .debug_selector(|| "schematic-body".into())
                .size_full()
                .overflow_scroll()
                .child(sheet),
        )
        .child(eyebrow)
        .into_any_element()
}
