//! Streaming text and the shimmer label — Beautiful UI's two "a model is
//! working" signals.
//!
//! ```text
//! @keyframes shimmer-text { 0% { background-position:150% } 100% { background-position:-50% } }
//! @keyframes stream-in    { 0% { opacity:0; filter:blur(4px) } 100% { opacity:1; filter:none } }
//! ```
//!
//! Both are ported with one deliberate substitution. The originals use a
//! clipped background gradient and a blur filter; GPUI has neither
//! `background-clip: text` nor a per-element blur, so the shimmer is a
//! per-word color ramp and the blur-resolve is an opacity ramp. The motion —
//! a highlight travelling left along the label, words settling in sequence — is
//! what carries the meaning, and that survives.

use gpui::{div, prelude::*, px, Div, Rgba, SharedString};

use super::text;
use super::tokens::BeautifulTokens;

/// `animation: shimmer-text 1.4s linear infinite`.
pub const SHIMMER_PERIOD_MS: u64 = 1400;

/// One word every 55ms (`WORD_MS`).
pub const WORD_MS: u64 = 55;

/// `stream-in 420ms` — how long one word takes to resolve.
pub const STREAM_IN_MS: u64 = 420;

/// Blend two colors. GPUI's `Rgba` has no lerp of its own.
fn mix(a: Rgba, b: Rgba, t: f32) -> Rgba {
    let t = t.clamp(0.0, 1.0);
    Rgba {
        r: a.r + (b.r - a.r) * t,
        g: a.g + (b.g - a.g) * t,
        b: a.b + (b.b - a.b) * t,
        a: a.a + (b.a - a.a) * t,
    }
}

/// The shimmer's brightness at a point along the label.
///
/// The source gradient is
/// `linear-gradient(90deg, var(--ink-3) 35%, var(--ink) 50%, var(--ink-3) 65%)`
/// over a `200%`-wide box scrolling from `150%` to `-50%`: a narrow bright band
/// travelling leftward through dim text. `pos` is 0.0 at the label's start and
/// 1.0 at its end; `phase` is the position in the cycle.
pub fn shimmer_weight(pos: f32, phase: f32) -> f32 {
    // The band's centre sweeps from beyond the right edge to beyond the left.
    let centre = 1.5 - 2.0 * phase.rem_euclid(1.0);
    // Half-width 0.15 == the 35%..65% stops over a 200% box.
    let d = ((pos - centre) / 0.15).abs();
    (1.0 - d).max(0.0)
}

/// A shimmering label — the "Thinking…" / "Explaining…" status text.
///
/// GPUI styles a whole text run at once, so the gradient is approximated by
/// splitting on words and coloring each by its position. At these label lengths
/// that is three or four segments, which is enough to read as a sweep.
pub fn shimmer_text(
    t: &BeautifulTokens,
    label: impl Into<SharedString>,
    elapsed_ms: u64,
) -> Div {
    let label: SharedString = label.into();
    let phase = (elapsed_ms % SHIMMER_PERIOD_MS) as f32 / SHIMMER_PERIOD_MS as f32;
    let words: Vec<&str> = label.split(' ').collect();
    let n = words.len().max(1) as f32;

    let mut row = div()
        .flex()
        .flex_row()
        .items_center()
        .text_size(text::LG)
        .font_weight(gpui::FontWeight::MEDIUM);
    for (i, w) in words.iter().enumerate() {
        let pos = (i as f32 + 0.5) / n;
        let color = mix(t.ink_3, t.ink, shimmer_weight(pos, phase));
        let s = if i + 1 == words.len() {
            w.to_string()
        } else {
            format!("{w} ")
        };
        row = row.child(div().text_color(color).child(s));
    }
    row
}

/// How far into its resolve a word is, given when streaming started.
///
/// Returns `1.0` for a word that has fully arrived and `0.0` for one that has
/// not started. Pure, so the staggering is testable without a clock.
pub fn word_progress(index: usize, elapsed_ms: u64) -> f32 {
    let start = index as u64 * WORD_MS;
    if elapsed_ms <= start {
        return 0.0;
    }
    let since = (elapsed_ms - start) as f32;
    (since / STREAM_IN_MS as f32).clamp(0.0, 1.0)
}

/// Streamed prose: Markdown, rendered, with the most recent words still
/// resolving out of nothing.
///
/// `settled` text is drawn as whole styled runs; only the tail animates,
/// because re-animating the whole body on every delta would make a long
/// explanation flicker. `elapsed_ms` is measured from when text began arriving.
pub fn streaming_body(
    t: &BeautifulTokens,
    body: &str,
    elapsed_ms: u64,
    done: bool,
) -> Div {
    use super::markdown::{self, Block};

    let mut col = div().flex().flex_col().w_full().gap(px(8.));

    // How many words at the end are still resolving. Everything before that is
    // drawn as whole runs, which keeps the paragraph from re-wrapping per frame.
    let total_words = body.split_whitespace().count();
    let animating = if done {
        0
    } else {
        ((STREAM_IN_MS / WORD_MS) as usize + 1).min(total_words)
    };
    let first_animated = total_words.saturating_sub(animating);
    // Counts words as the blocks are walked, so a word's index — and therefore
    // its progress — is stable no matter which block it landed in.
    let mut seen = 0usize;

    for block in markdown::parse(body) {
        match block {
            Block::Code(text) => {
                col = col.child(code_block(t, &text));
                seen += text.split_whitespace().count();
            }
            Block::Heading(spans) => {
                col = col.child(
                    span_row(t, &spans, first_animated, elapsed_ms, &mut seen)
                        .font_weight(gpui::FontWeight::SEMIBOLD)
                        .text_color(t.ink),
                );
            }
            Block::Paragraph(spans) => {
                col = col.child(span_row(t, &spans, first_animated, elapsed_ms, &mut seen));
            }
            Block::Item {
                spans,
                ordered,
                number,
            } => {
                let marker = if ordered {
                    format!("{number}.")
                } else {
                    "\u{2022}".to_string()
                };
                col = col.child(
                    div()
                        .flex()
                        .flex_row()
                        .items_start()
                        .gap(px(6.))
                        .child(
                            div()
                                .flex_none()
                                .min_w(px(14.))
                                .text_color(t.ink_3)
                                .child(marker),
                        )
                        .child(
                            span_row(t, &spans, first_animated, elapsed_ms, &mut seen).flex_1(),
                        ),
                );
            }
        }
    }
    col
}

/// One word of a paragraph, ready to draw.
#[derive(Debug, Clone, PartialEq)]
pub struct Piece {
    /// The text, including its trailing space when it had one.
    pub text: String,
    pub style: super::markdown::Style,
    /// 1.0 once the word has fully resolved.
    pub opacity: f32,
}

/// Split a paragraph's styled runs into per-word pieces.
///
/// **Every word gets its own piece, in every state.** It is tempting to emit a
/// settled run as one element and only split the animating tail — but a flex
/// row lays each child out as one item, and an item does not wrap inside
/// itself. A whole paragraph in one child therefore runs off the side of the
/// card, while the same text mid-stream wraps correctly because it happens to
/// be split. Keeping the segmentation identical in both states is what makes
/// the settled card look like the streaming one.
///
/// `seen` counts words across the whole body, so a word's animation progress
/// does not depend on which block it landed in.
pub fn pieces(
    spans: &[super::markdown::Span],
    first_animated: usize,
    elapsed_ms: u64,
    seen: &mut usize,
) -> Vec<Piece> {
    let mut out = Vec::new();
    for sp in spans {
        for w in sp.text.split_inclusive(' ') {
            let is_word = !w.trim().is_empty();
            let idx = *seen;
            if is_word {
                *seen += 1;
            }
            let opacity = if !is_word || idx < first_animated {
                1.0
            } else {
                word_progress(idx.saturating_sub(first_animated), elapsed_ms).max(0.05)
            };
            out.push(Piece {
                text: w.to_string(),
                style: sp.style,
                opacity,
            });
        }
    }
    out
}

/// One paragraph, as a wrapping row of per-word elements.
fn span_row(
    t: &BeautifulTokens,
    spans: &[super::markdown::Span],
    first_animated: usize,
    elapsed_ms: u64,
    seen: &mut usize,
) -> Div {
    let mut row = div()
        .flex()
        .flex_row()
        .flex_wrap()
        // Without this the row keeps its widest child's intrinsic width and
        // pushes past the card instead of wrapping.
        .w_full()
        .min_w(px(0.))
        .text_size(text::LG)
        .text_color(t.ink)
        // `leading-relaxed`.
        .line_height(px(19.5));

    for p in pieces(spans, first_animated, elapsed_ms, seen) {
        row = row.child(styled(t, &p.text, p.style).opacity(p.opacity));
    }
    row
}

/// A run of text wearing its Markdown style.
fn styled(t: &BeautifulTokens, text: &str, style: super::markdown::Style) -> Div {
    let mut d = div().child(text.to_string());
    if style.code {
        // An inline code span: monospace on the chip fill, so an identifier is
        // distinguishable from the prose around it at a glance.
        d = d
            .font_family(crate::fonts::mono_family())
            .text_size(text::CHIP)
            .text_color(t.ink)
            .bg(t.field)
            
            .px(px(3.));
    }
    if style.bold {
        d = d.font_weight(gpui::FontWeight::SEMIBOLD);
    }
    if style.italic {
        d = d.italic();
    }
    if style.strike {
        d = d.line_through();
    }
    d
}

/// A fenced code block: the inset well, monospace, no inline markup.
fn code_block(t: &BeautifulTokens, code: &str) -> Div {
    div()
        .w_full()
        
        .bg(t.inset)
        .border_1()
        .border_color(t.line)
        .p(px(8.))
        .font_family(crate::fonts::mono_family())
        .text_size(text::CHIP)
        .text_color(t.ink_2)
        .line_height(px(17.))
        .child(code.to_string())
}

/// The caret shown while text is still arriving.
///
/// ```text
/// ml-0.5 inline-block h-3 w-0.5 translate-y-0.5 rounded-full bg-ink
/// ```
pub fn stream_caret(t: &BeautifulTokens) -> Div {
    div()
        .w(px(2.))
        .h(px(12.))
        .ml(px(2.))
        .flex_none()
        .rounded_full()
        .bg(t.ink)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The band must actually travel, and leftward: the gradient scrolls from
    /// 150% to -50%, so a fixed point brightens later the further right it is.
    #[test]
    fn the_shimmer_band_sweeps_leftward() {
        let peak = |pos: f32| {
            (0..1000)
                .map(|i| (i as f32 / 1000.0, shimmer_weight(pos, i as f32 / 1000.0)))
                .max_by(|a, b| a.1.partial_cmp(&b.1).unwrap())
                .unwrap()
                .0
        };
        let right = peak(0.9);
        let left = peak(0.1);
        assert!(
            right < left,
            "the right of the label must light first: {right} then {left}"
        );
    }

    #[test]
    fn shimmer_weight_stays_in_range() {
        for p in 0..100 {
            for ph in 0..100 {
                let w = shimmer_weight(p as f32 / 100.0, ph as f32 / 100.0);
                assert!((0.0..=1.0).contains(&w), "{w}");
            }
        }
    }

    /// Every point must be fully lit at some phase, or part of the label never
    /// brightens and the sweep looks broken.
    #[test]
    fn every_position_reaches_full_brightness() {
        for p in 0..=10 {
            let pos = p as f32 / 10.0;
            let best = (0..2000)
                .map(|i| shimmer_weight(pos, i as f32 / 2000.0))
                .fold(0.0f32, f32::max);
            assert!(best > 0.97, "position {pos} peaks at only {best}");
        }
    }

    #[test]
    fn words_resolve_in_order() {
        // At 300ms, word 0 (start 0) is further along than word 4 (start 220).
        assert!(word_progress(0, 300) > word_progress(4, 300));
        assert_eq!(word_progress(0, 0), 0.0);
        assert_eq!(word_progress(10, 100), 0.0, "not started yet");
    }

    #[test]
    fn a_word_fully_resolves_and_stays_resolved() {
        assert_eq!(word_progress(0, STREAM_IN_MS), 1.0);
        assert_eq!(word_progress(0, STREAM_IN_MS * 10), 1.0);
        assert_eq!(word_progress(3, 3 * WORD_MS + STREAM_IN_MS), 1.0);
    }

    #[test]
    fn mix_interpolates_both_ends() {
        let a = Rgba { r: 0., g: 0., b: 0., a: 1. };
        let b = Rgba { r: 1., g: 1., b: 1., a: 1. };
        assert_eq!(mix(a, b, 0.0).r, 0.0);
        assert_eq!(mix(a, b, 1.0).r, 1.0);
        assert!((mix(a, b, 0.5).r - 0.5).abs() < 1e-6);
        // Out of range must clamp, not overshoot into an invalid color.
        assert_eq!(mix(a, b, 2.0).r, 1.0);
        assert_eq!(mix(a, b, -1.0).r, 0.0);
    }

    // ---- word segmentation ------------------------------------------------

    use super::super::markdown;

    fn spans(src: &str) -> Vec<markdown::Span> {
        markdown::parse_inline(src)
    }

    fn texts(p: &[Piece]) -> Vec<String> {
        p.iter().map(|x| x.text.clone()).collect()
    }

    /// The bug: a settled paragraph used to collapse into one element, and a
    /// single flex item cannot wrap inside itself, so it ran off the card. The
    /// segmentation must be identical whether or not the text is still
    /// arriving — only the opacities may differ.
    #[test]
    fn segmentation_is_identical_streaming_and_settled() {
        let sp = spans("The loop walks `verts` and scales each `v.pos` by `scale` now.");
        let total = "The loop walks verts and scales each v.pos by scale now."
            .split_whitespace()
            .count();

        let mut a_seen = 0;
        let streaming = pieces(&sp, total.saturating_sub(8), 100, &mut a_seen);
        let mut b_seen = 0;
        let settled = pieces(&sp, total, 99_999, &mut b_seen);

        assert_eq!(texts(&streaming), texts(&settled));
        assert_eq!(a_seen, b_seen);
        assert!(streaming.len() > 8, "must be split per word, not per run");
    }

    /// Every word is its own piece, so the row has something to wrap at.
    #[test]
    fn a_settled_paragraph_is_still_split_per_word() {
        let sp = spans("one two three four five");
        let mut seen = 0;
        let p = pieces(&sp, 5, 10_000, &mut seen);
        assert_eq!(texts(&p), vec!["one ", "two ", "three ", "four ", "five"]);
        assert!(p.iter().all(|x| x.opacity == 1.0));
    }

    /// The spaces travel with their words, so joining the pieces reproduces
    /// the paragraph exactly — no gap property is doing the spacing.
    #[test]
    fn pieces_rejoin_into_the_original_text() {
        let src = "the `verts` container holds every vertex";
        let sp = spans(src);
        let mut seen = 0;
        let p = pieces(&sp, 99, 0, &mut seen);
        assert_eq!(texts(&p).concat(), src.replace('`', ""));
    }

    /// Style survives the split: a two-word code span yields two code pieces.
    #[test]
    fn style_is_carried_onto_every_word_of_a_run() {
        let sp = spans("call `read frame` now");
        let mut seen = 0;
        let p = pieces(&sp, 99, 0, &mut seen);
        let coded: Vec<String> = p
            .iter()
            .filter(|x| x.style.code)
            .map(|x| x.text.clone())
            .collect();
        assert_eq!(coded, vec!["read ", "frame"]);
    }

    #[test]
    fn only_the_tail_is_faded_while_streaming() {
        let sp = spans("alpha beta gamma delta");
        let mut seen = 0;
        let p = pieces(&sp, 2, 0, &mut seen);
        assert_eq!(p[0].opacity, 1.0);
        assert_eq!(p[1].opacity, 1.0);
        assert!(p[2].opacity < 1.0, "the tail must still be resolving");
    }

    /// The counter advances once per word across blocks, so progress does not
    /// restart in each paragraph.
    #[test]
    fn the_word_counter_carries_across_calls() {
        let mut seen = 0;
        pieces(&spans("one two"), 99, 0, &mut seen);
        assert_eq!(seen, 2);
        pieces(&spans("three"), 99, 0, &mut seen);
        assert_eq!(seen, 3);
    }
}
