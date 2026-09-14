//! The Explain card (§4.14): a floating panel anchored to the selection,
//! streaming a prose explanation of it.
//!
//! Styled with [`crate::beautiful`], the port of Beautiful UI's agent-interface
//! primitives, rather than with [`crate::kumo`] — this is the one surface in
//! Jade where a model is visibly working, and that library has the vocabulary
//! for it: a pixel-grid loader, a shimmering status label, words resolving as
//! they arrive, and a status pill per terminal state.
//!
//! The editor is a `uniform_list` with a fixed row height ([`super::code_view`]
//! `LINE_H`), so nothing can occupy vertical space between two lines. The card
//! is therefore an absolutely positioned overlay, anchored with the same
//! [`super::code_view::popup_x`] / [`popup_y`](super::code_view::popup_y)
//! helpers the completion, hover, and signature popups use — which is what
//! makes it track horizontal scroll and folds for free.
//!
//! [`body`] is shared with [`super::explain_popout`], so the window and the
//! in-editor card cannot drift apart.

use gpui::{div, prelude::*, px, Context, MouseButton};

use crate::app::JadeApp;
use crate::beautiful::{
    self, chip, icon_button, pixel_loader, status_pill, stream, text, BeautifulTokens,
    Card, LoaderPattern,
};
use crate::explain::{ExplainCard, ExplainPhase};

/// Card width. Beautiful UI's agent components cap at `max-w-95` (380px); the
/// card runs a little wider because it holds code-adjacent prose rather than
/// chat, and a too-narrow measure makes an explanation of a long identifier
/// wrap badly.
pub const CARD_W: f32 = 420.0;
/// Prose pane cap. Past this the pane scrolls rather than the card growing —
/// an explanation must never cover the fragment it describes.
pub const PROSE_MAX_H: f32 = 240.0;
/// Breathing room between the card and the editor's edges.
const MARGIN: f32 = 8.0;
/// Gap between the selection and a card flipped above it.
const FLIP_GAP: f32 = 6.0;

/// The height assumed before the card has been measured once.
const MIN_H: f32 = 140.0;

/// Where to draw the card, given how tall it actually is.
///
/// Returns `(left, top, max_height)`.
///
/// Two requirements pull against each other here, and both are real:
///
///   - The card must **not flip** between above and below as prose streams in.
///     An earlier version chose a side from a guessed height, so the guess
///     drifted from reality and the card jumped mid-answer.
///   - The card must **move** as it fills, so a long explanation is not stuck
///     scrolling inside a box pinned to the top of the selection.
///
/// Both hold if position is a single clamp rather than a choice between sides.
/// The card wants to sit just under the selection and grows downward; once its
/// bottom would leave the editor it slides up by exactly the overflow, and it
/// stops when its top reaches the margin. There is no branch to flip, and the
/// movement is continuous with the text.
///
/// `measured_h` is the height from the previous frame (see
/// [`JadeApp::explain_card_h`](crate::app::JadeApp)), so the slide lags by one
/// frame — invisible at the repaint rate the card already runs at. Before the
/// first measurement it is `None` and [`MIN_H`] stands in.
///
/// A `view` of zero — before the first paint has measured anything — means "do
/// not clamp", rather than every branch failing and pinning the card to a
/// corner.
pub fn clamp_card(
    anchor: (f32, f32),
    width: f32,
    view: (f32, f32),
    measured_h: Option<f32>,
) -> (f32, f32, f32) {
    let (vw, vh) = view;
    if vw <= 1.0 || vh <= 1.0 {
        return (anchor.0.max(MARGIN), anchor.1.max(MARGIN), MIN_H);
    }

    // Horizontal: prefer the anchor, slide left to fit, never past the margin.
    let left = anchor.0.min(vw - width - MARGIN).max(MARGIN);

    // The card may grow to fill the editor; past that its body scrolls.
    let max_h = (vh - 2.0 * MARGIN).max(80.0);
    let h = measured_h.unwrap_or(MIN_H).clamp(0.0, max_h);

    // Sit under the selection, then slide up by however much would hang out
    // of the bottom. `max` wins the tie for a card taller than the viewport,
    // keeping its top visible and letting the body scroll.
    let top = anchor.1.min(vh - MARGIN - h).max(MARGIN);
    (left, top, max_h)
}

/// Keep an off-screen anchor inside the editor, so scrolling never hides the
/// card. It parks against the edge it left by, and comes back to the code when
/// its row scrolls into view again. Shared with the Visualize card.
pub fn park_offscreen_anchor(anchor_y: f32, view_h: f32) -> f32 {
    if view_h <= 1.0 {
        return anchor_y.max(MARGIN);
    }
    anchor_y.clamp(MARGIN, (view_h - MIN_H - MARGIN).max(MARGIN))
}

/// The card's contents, shared by the overlay and the pop-out window.
///
/// `cx` is `Some` only for the in-editor card: the pop-out renders a different
/// view type and gpui listeners bind to the concrete view, so the window shows
/// the same content without the buttons.
pub fn body(
    card: &ExplainCard,
    t: &BeautifulTokens,
    now_ms: u64,
    cx: Option<&mut Context<JadeApp>>,
) -> impl IntoElement {
    let mut col = Card::pad(t)
        .id("explain-body")
        .flex()
        .flex_col()
        .w_full()
        .min_h(px(0.))
        .gap(px(8.))
        .overflow_y_scroll();

    match &card.phase {
        // Nothing has arrived: the loader carries the wait.
        ExplainPhase::Requesting | ExplainPhase::Thinking if card.prose.is_empty() => {
            let label = if card.phase == ExplainPhase::Thinking {
                "Thinking"
            } else {
                "Reading the selection"
            };
            col = col.child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    // `gap-2.5` on the loader row.
                    .gap(px(10.))
                    .child(pixel_loader(t, LoaderPattern::Drive, card.elapsed(now_ms)))
                    .child(stream::shimmer_text(t, label, card.elapsed(now_ms)))
                    .child(div().flex_1())
                    .child(elapsed_timer(t, card.elapsed(now_ms))),
            );
        }
        ExplainPhase::Failed(err) => {
            col = col.child(failure_body(t, err));
        }
        _ => {}
    }

    // ── prose ────────────────────────────────────────────────────────────
    if !card.prose.is_empty() {
        let streaming = matches!(card.phase, ExplainPhase::Streaming);
        // No height cap here: the CARD grows and slides up as prose arrives
        // (see `clamp_card`), and only once it fills the editor does its body
        // scroll. Capping the pane instead would make a short explanation
        // scroll inside a half-empty card.
        let mut prose = div()
            .id("explain-prose")
            .w_full()
            .child(stream::streaming_body(
                t,
                &card.prose,
                card.stream_elapsed(now_ms),
                !streaming,
            ));
        if streaming {
            prose = prose.child(stream::stream_caret(t));
        }
        col = col.child(prose);
    }

    // ── actions ──────────────────────────────────────────────────────────
    //
    // The action bar is present but inert until the answer settles, exactly as
    // in the source (`opacity: +!!done`, `pointerEvents: none`) — the row does
    // not appear and shove the prose upward the moment streaming ends.
    if let Some(cx) = cx {
        let done = matches!(card.phase, ExplainPhase::Done | ExplainPhase::Failed(_));
        let mut bar = div()
            .flex()
            .flex_row()
            .items_center()
            .gap(px(2.))
            .opacity(if done { 1.0 } else { 0.0 });
        if card.can_retry() {
            bar = bar.child(
                icon_button("explain-retry", "rotate-ccw", t).on_click(cx.listener(
                    |app: &mut JadeApp, _e, _w, cx| {
                        app.explain_retry(cx);
                        cx.notify();
                    },
                )),
            );
        }
        if done {
            col = col.child(bar);
        }
    }

    col
}

/// `font-mono text-[12px] text-ink-3 tabular-nums`, `"0.0s"` then `"1m 4.2s"`.
fn elapsed_timer(t: &BeautifulTokens, elapsed_ms: u64) -> impl IntoElement {
    let tenths = elapsed_ms / 100;
    let label = if tenths < 600 {
        format!("{}.{}s", tenths / 10, tenths % 10)
    } else {
        let s = tenths / 10;
        format!("{}m {}.{}s", s / 60, s % 60, tenths % 10)
    };
    div()
        .flex_none()
        .font_family(crate::fonts::mono_family())
        .text_size(text::SM)
        .text_color(t.ink_3)
        .child(label)
}

/// A failure, told as a sentence rather than a status code.
fn failure_body(t: &BeautifulTokens, err: &jade_ai::ChatError) -> impl IntoElement {
    let mut col = div()
        .flex()
        .flex_col()
        .gap(px(6.))
        .child(
            div()
                .text_size(text::LG)
                .font_weight(gpui::FontWeight::MEDIUM)
                .text_color(t.ink)
                .child(err.headline()),
        );
    if let Some(detail) = err.detail() {
        col = col.child(
            div()
                .text_size(text::BODY)
                .text_color(t.ink_2)
                .line_height(px(18.))
                .child(detail),
        );
    }
    col
}

/// The header rail: what was explained, how it is going, and the controls.
fn header(
    card: &ExplainCard,
    t: &BeautifulTokens,
    now_ms: u64,
    cx: &mut Context<JadeApp>,
) -> impl IntoElement {
    let range = if card.start_row == card.end_row {
        format!("L{}", card.start_row + 1)
    } else {
        format!("L{}\u{2013}{}", card.start_row + 1, card.end_row + 1)
    };

    let mut rail = Card::bar(t)
        .w_full()
        .gap(px(8.))
        .child(chip(t, card.language))
        .child(
            div()
                .font_family(crate::fonts::mono_family())
                .text_size(text::CHIP)
                .text_color(t.ink_3)
                .child(range),
        );

    // Status: a pill only for a terminal state. While work is in flight the
    // loader in the body already says so, and two live indicators compete.
    match &card.phase {
        ExplainPhase::Done => {
            rail = rail.child(status_pill("Explained", t.green, t.green_tint));
        }
        ExplainPhase::Failed(jade_ai::ChatError::LocalStarting) => {
            rail = rail.child(status_pill("Starting", t.orange, t.orange_tint));
        }
        ExplainPhase::Failed(_) => {
            rail = rail.child(status_pill("Failed", t.red, t.red_tint));
        }
        _ => {
            rail = rail.child(stream::shimmer_text(t, "Working", card.elapsed(now_ms)));
        }
    }

    // The buffer moved under the explanation, so it may no longer be true.
    if card.stale {
        rail = rail.child(status_pill("Edited", t.orange, t.orange_tint));
    }

    rail.child(div().flex_1())
        .child(
            icon_button("explain-popout", "external-link", t).on_click(cx.listener(
                |app: &mut JadeApp, _e, _w, cx| {
                    app.open_explain_popout(cx);
                },
            )),
        )
        .child(
            icon_button("explain-close", "x", t).on_click(cx.listener(
                |app: &mut JadeApp, _e, _w, cx| {
                    app.close_explain(cx);
                    cx.notify();
                },
            )),
        )
}

/// A zero-size underlay that records the card's painted height.
fn height_probe(store: std::sync::Arc<std::sync::atomic::AtomicU32>) -> impl IntoElement {
    gpui::canvas(
        move |bounds, _window, _cx| {
            store.store(
                f32::from(bounds.size.height).to_bits(),
                std::sync::atomic::Ordering::Relaxed,
            );
        },
        |_, _, _, _| {},
    )
    .absolute()
    .top_0()
    .left_0()
    .size_full()
}

/// Render the card over the editor, or `None` when there is nothing to show.
pub fn render(
    app: &JadeApp,
    flow_visible: bool,
    scroll_top: usize,
    cx: &mut Context<JadeApp>,
) -> Option<gpui::AnyElement> {
    if !app.explain_visible() {
        return None;
    }
    let card = app.explain.as_ref()?;
    let t = beautiful::dark();
    let now = app.now_ms();

    let display_row = app
        .editor
        .active_tab()
        .map(|tab| tab.display_row(card.end_row))
        .unwrap_or(card.end_row);

    let anchor_x = super::code_view::popup_x(0, flow_visible, app.char_w(), app.editor_h_scroll());
    let anchor_y = super::code_view::popup_y(display_row, scroll_top);

    // Scrolling the anchor off-screen must not take the card with it: the user
    // asked for this explanation, and an in-flight request is worth money. The
    // card parks against the edge instead, and rejoins its row on the way back.
    let (_, view_h) = app.editor_size();
    let anchor_y = park_offscreen_anchor(anchor_y, view_h);
    let measured = app.explain_card_height();
    let (left, top, max_h) =
        clamp_card((anchor_x, anchor_y), CARD_W, app.editor_size(), measured);

    let mut el = Card::new(&t)
        .id("explain-card")
        // Lets the interaction tests assert the card is actually painted,
        // rather than only that the state field is set.
        .debug_selector(|| "explain-card".into())
        .absolute()
        .left(px(left))
        .top(px(top))
        .w(px(CARD_W))
        .max_h(px(max_h))
        
        // Clicks belong to the card, not to the buffer underneath it.
        .occlude()
        .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
        // Publishes the drawn height so the next frame can slide the card up
        // as it fills. Layout is only known at paint, so there is nowhere
        // earlier to read it — the same trick `ime_geometry_canvas` uses.
        .child(height_probe(app.explain_card_h.clone()))
        .child(header(card, &t, now, cx));

    if card.popped_out {
        el = el.child(
            Card::pad(&t).w_full().child(
                div()
                    .text_size(text::BODY)
                    .text_color(t.ink_3)
                    .child("Showing in a separate window"),
            ),
        );
    } else {
        el = el.child(body(card, &t, now, Some(cx)));
    }

    Some(el.into_any_element())
}

#[cfg(test)]
mod tests {
    use super::*;

    const W: f32 = CARD_W;
    const VIEW: (f32, f32) = (1200.0, 800.0);

    #[test]
    fn sits_under_the_selection_when_it_fits() {
        let (l, t, h) = clamp_card((60.0, 100.0), W, VIEW, Some(200.0));
        assert_eq!((l, t), (60.0, 100.0));
        assert_eq!(h, 800.0 - 2.0 * MARGIN, "may grow to fill the editor");
    }

    /// The card must slide up as prose arrives, rather than hanging out of the
    /// bottom of the editor or scrolling inside a pinned box.
    #[test]
    fn it_slides_up_as_it_fills() {
        let anchor = (60.0, 600.0);
        let short = clamp_card(anchor, W, VIEW, Some(120.0)).1;
        let tall = clamp_card(anchor, W, VIEW, Some(400.0)).1;
        assert_eq!(short, 600.0, "a short card stays at the anchor");
        assert!(tall < short, "a taller card must move up: {tall} vs {short}");
        assert_eq!(tall, 800.0 - MARGIN - 400.0);
    }

    /// The slide is continuous — no jump as the height crosses the edge.
    #[test]
    fn the_slide_is_monotonic_in_height() {
        let mut last = f32::INFINITY;
        for h in (60..=700).step_by(20) {
            let top = clamp_card((60.0, 600.0), W, VIEW, Some(h as f32)).1;
            assert!(top <= last + 0.001, "top jumped upward at h={h}");
            last = top;
        }
    }

    /// The original bug: placement must never flip between two sides, whatever
    /// the height. It only ever slides.
    #[test]
    fn placement_never_flips_sides() {
        let anchor = (60.0, 300.0);
        for h in [50.0, 200.0, 480.0, 900.0] {
            let top = clamp_card(anchor, W, VIEW, Some(h)).1;
            assert!(top <= anchor.1, "must not jump below the anchor");
            assert!(top >= MARGIN);
        }
    }

    /// A card taller than the editor keeps its top visible and scrolls its
    /// body, rather than sliding its header off the screen.
    #[test]
    fn an_overlong_card_pins_to_the_top_and_scrolls() {
        let (_, top, h) = clamp_card((60.0, 400.0), W, VIEW, Some(5000.0));
        assert_eq!(top, MARGIN);
        assert_eq!(h, 800.0 - 2.0 * MARGIN);
    }

    /// Before the first paint the editor box is (0,0), which used to fail every
    /// branch and pin the card to the corner.
    #[test]
    fn an_unmeasured_viewport_uses_the_anchor_rather_than_the_corner() {
        let (l, t, h) = clamp_card((240.0, 190.0), W, (0.0, 0.0), None);
        assert_eq!((l, t), (240.0, 190.0));
        assert!(h >= MIN_H);
    }

    /// An unmeasured card assumes MIN_H rather than zero, so its first frame
    /// is not placed as though it were empty.
    #[test]
    fn an_unmeasured_card_assumes_a_minimum_height() {
        let a = clamp_card((60.0, 700.0), W, VIEW, None).1;
        let b = clamp_card((60.0, 700.0), W, VIEW, Some(MIN_H)).1;
        assert_eq!(a, b);
    }

    #[test]
    fn slides_left_rather_than_overflowing_the_right_edge() {
        let (l, _, _) = clamp_card((1100.0, 100.0), W, VIEW, Some(200.0));
        assert_eq!(l, 1200.0 - W - MARGIN);
    }

    /// A pane narrower than the card clamps to the left margin rather than
    /// sliding off the left edge to honor the right one.
    #[test]
    fn never_slides_past_the_left_margin() {
        let (l, _, _) = clamp_card((900.0, 100.0), W, (300.0, 800.0), Some(200.0));
        assert_eq!(l, MARGIN);
    }

    #[test]
    fn a_degenerate_viewport_still_yields_a_usable_box() {
        let (l, top, h) = clamp_card((0.0, 0.0), W, (40.0, 40.0), Some(200.0));
        assert!(h >= 80.0, "{h}");
        assert!(top >= 0.0 && l.is_finite());
    }

    // ---- parking an off-screen anchor ---------------------------------

    #[test]
    fn an_anchor_scrolled_above_the_view_parks_at_the_top() {
        assert_eq!(park_offscreen_anchor(-4000.0, 800.0), MARGIN);
    }

    #[test]
    fn an_anchor_scrolled_below_the_view_parks_at_the_bottom() {
        let y = park_offscreen_anchor(9000.0, 800.0);
        assert_eq!(y, 800.0 - MIN_H - MARGIN);
        assert!(y > MARGIN);
    }

    #[test]
    fn an_onscreen_anchor_is_untouched() {
        assert_eq!(park_offscreen_anchor(300.0, 800.0), 300.0);
    }

    #[test]
    fn parking_is_safe_in_an_unmeasured_or_tiny_viewport() {
        assert_eq!(park_offscreen_anchor(-50.0, 0.0), MARGIN);
        let y = park_offscreen_anchor(500.0, 60.0);
        assert!(y >= MARGIN && y.is_finite());
    }

    /// Whatever the scroll position and however tall the card, it lands inside
    /// the editor.
    #[test]
    fn the_card_is_always_inside_the_viewport() {
        for anchor_y in [-5000.0, -1.0, 0.0, 12.0, 399.0, 780.0, 5000.0] {
            for h in [40.0, 200.0, 700.0, 2000.0] {
                let y = park_offscreen_anchor(anchor_y, VIEW.1);
                let (l, top, max_h) = clamp_card((60.0, y), W, VIEW, Some(h));
                assert!(top >= 0.0, "top {top} for anchor {anchor_y} h {h}");
                assert!(
                    top + h.min(max_h) <= VIEW.1 + 0.001,
                    "bottom {} overflows for anchor {anchor_y} h {h}",
                    top + h.min(max_h)
                );
                assert!(l >= 0.0 && l + W <= VIEW.0);
            }
        }
    }
}
