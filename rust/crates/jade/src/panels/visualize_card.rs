//! The Visualize card (§4.15): a floating panel anchored to the selection,
//! rendering and playing a Manim animation of it.
//!
//! Placement is [`super::explain_card::clamp_card`], reused wholesale — the
//! same anchor helpers, the same measured-height slide, the same off-screen
//! parking. The body is [`body`], shared with the pop-out window so the two
//! cannot drift apart.
//!
//! Shape (handoff §5):
//!
//! ```text
//! Card::new(t)                              w 520
//! ├── Card::bar      chip(language) · "L84–97" · stage pill · ⧉ · ×
//! ├── video area 488×274
//! │     Consent → the one-time opt-in
//! │     Requesting/Thinking/Writing → pixel_loader + shimmer
//! │     Rendering → loader + the last manim log line
//! │     Ready → gpui::surface(frame).object_fit(Contain)
//! │     Unsuitable/Failed → the reason
//! └── Card::footer   ▶/⏸ · scrub · "0:03 / 0:11" · retry
//! ```

use gpui::{div, prelude::*, px, Context, MouseButton};

use crate::app::JadeApp;
use crate::beautiful::{
    self, chip, icon_button, pixel_loader, radius, status_pill, stream, text, BeautifulTokens,
    Card, LoaderPattern,
};
use crate::video::Transport;
use crate::visualize::{format_time, VisualizeCard, VisualizeError, VisualizePhase};

/// Card width. Wider than Explain's 420: the video band is 16:9 and a
/// narrower card makes Manim's text illegible.
pub const CARD_W: f32 = 520.0;
/// The video band, 16:9 inside the padded card.
pub const VIDEO_W: f32 = 488.0;
pub const VIDEO_H: f32 = 274.0;

/// The card's contents, shared by the overlay and the pop-out window.
///
/// `cx` is `Some` only for the in-editor card, exactly as in
/// [`super::explain_card::body`]: the pop-out is a different view type, so it
/// renders the same content without the interactive controls.
pub fn body(
    app: &JadeApp,
    card: &VisualizeCard,
    t: &BeautifulTokens,
    now_ms: u64,
    cx: Option<&mut Context<JadeApp>>,
) -> impl IntoElement {
    let mut col = div().flex().flex_col().w_full().min_h(px(0.));

    // ── the video band ───────────────────────────────────────────────────
    let mut area = div()
        .id("visualize-area")
        .flex()
        .flex_col()
        .items_center()
        .justify_center()
        .w_full()
        .h(px(VIDEO_H))
        .gap(px(10.))
        .bg(t.inset)
        .overflow_hidden();

    match &card.phase {
        VisualizePhase::Consent => {
            area = area.child(consent_body(t));
        }
        VisualizePhase::Requesting | VisualizePhase::Thinking | VisualizePhase::Writing => {
            let label = match card.phase {
                VisualizePhase::Requesting => "Reading the selection",
                VisualizePhase::Thinking => "Thinking",
                _ => "Writing the scene",
            };
            area = area
                .child(pixel_loader(t, LoaderPattern::Drive, card.elapsed(now_ms)))
                .child(stream::shimmer_text(t, label, card.elapsed(now_ms)))
                .child(elapsed_timer(t, card.elapsed(now_ms)));
        }
        VisualizePhase::Rendering => {
            area = area
                .child(pixel_loader(t, LoaderPattern::Orbit, card.elapsed(now_ms)))
                .child(stream::shimmer_text(t, "Rendering", card.elapsed(now_ms)))
                .child(
                    div()
                        .max_w(px(VIDEO_W - 48.0))
                        .overflow_hidden()
                        .whitespace_nowrap()
                        .font_family(crate::fonts::mono_family())
                        .text_size(text::XS)
                        .text_color(t.ink_3)
                        .child(if card.render_line.is_empty() {
                            "Starting manim…".to_string()
                        } else {
                            card.render_line.clone()
                        }),
                )
                .child(elapsed_timer(t, card.elapsed(now_ms)));
        }
        VisualizePhase::Ready => {
            area = ready_area(app, area, t);
        }
        VisualizePhase::Unsuitable => {
            let reason = card
                .plan
                .as_ref()
                .map(|p| p.reason.clone())
                .filter(|r| !r.trim().is_empty())
                .unwrap_or_else(|| "This fragment has no data movement to show.".into());
            area = area
                .child(
                    div()
                        .text_size(text::LG)
                        .font_weight(gpui::FontWeight::MEDIUM)
                        .text_color(t.ink)
                        .child("Nothing to animate here"),
                )
                .child(
                    div()
                        .max_w(px(VIDEO_W - 64.0))
                        .text_size(text::BODY)
                        .text_color(t.ink_2)
                        .line_height(px(18.))
                        .child(reason),
                );
        }
        VisualizePhase::Failed(err) => {
            area = area.child(failure_body(t, err));
        }
    }
    col = col.child(Card::pad(t).w_full().child(area));

    // ── the transport footer ─────────────────────────────────────────────
    if card.phase == VisualizePhase::Ready {
        if let Some(cx) = cx {
            col = col.child(transport_footer(app, t, cx));
        } else if let Some(tr) = app.visualize_transport() {
            // The pop-out shows the times without the controls.
            col = col.child(
                Card::footer(t).w_full().child(time_label(t, &tr)),
            );
        }
    } else if let Some(cx) = cx {
        // Consent buttons / retry live in the footer band too.
        match &card.phase {
            VisualizePhase::Consent => col = col.child(consent_footer(t, cx)),
            VisualizePhase::Failed(e) if card.can_retry() => {
                let _ = e;
                col = col.child(
                    Card::footer(t).w_full().child(
                        icon_button("visualize-retry", "rotate-ccw", t).on_click(cx.listener(
                            |app: &mut JadeApp, _e, _w, cx| {
                                app.visualize_retry(cx);
                                cx.notify();
                            },
                        )),
                    ),
                );
            }
            _ => {}
        }
    }

    col
}

/// The playing (or paused) clip. Falls back to a text line when playback
/// degraded or the platform has no decoder.
fn ready_area(app: &JadeApp, area: gpui::Stateful<gpui::Div>, t: &BeautifulTokens) -> gpui::Stateful<gpui::Div> {
    if let Some(reason) = app.visualize_player.as_ref().and_then(|p| p.degraded()) {
        return area.child(
            div()
                .text_size(text::BODY)
                .text_color(t.ink_2)
                .child(format!("Playback stopped: {reason}")),
        );
    }
    #[cfg(target_os = "macos")]
    {
        if let Some(frame) = app.visualize_player.as_ref().and_then(|p| p.frame()) {
            // Contain, not Fill — a 16:9 clip must not stretch (§4.15 step 5).
            return area.child(
                gpui::surface(frame)
                    .object_fit(gpui::ObjectFit::Contain)
                    .w(px(VIDEO_W))
                    .h(px(VIDEO_H)),
            );
        }
        // Ready but no frame yet: the first pump lands within a frame or two.
        area.child(
            div()
                .text_size(text::BODY)
                .text_color(t.ink_3)
                .child("Starting playback…"),
        )
    }
    #[cfg(not(target_os = "macos"))]
    {
        area.child(
            div()
                .text_size(text::BODY)
                .text_color(t.ink_2)
                .child("The clip is rendered; playback needs macOS."),
        )
    }
}

/// ▶/⏸ · scrub · time · retry.
fn transport_footer(
    app: &JadeApp,
    t: &BeautifulTokens,
    cx: &mut Context<JadeApp>,
) -> impl IntoElement {
    let tr = app.visualize_transport().unwrap_or(Transport {
        playing: false,
        duration: 0.0,
        position: 0.0,
    });
    let play_icon = if tr.playing { "pause" } else { "play" };

    // The scrub track: a probe canvas records its bounds so a click maps to a
    // fraction (the same trick the card uses for its height).
    let bx = app.visualize_scrub_bounds[0].clone();
    let bw = app.visualize_scrub_bounds[1].clone();
    let probe = gpui::canvas(
        move |bounds, _window, _cx| {
            bx.store(f32::from(bounds.origin.x).to_bits(), std::sync::atomic::Ordering::Relaxed);
            bw.store(f32::from(bounds.size.width).to_bits(), std::sync::atomic::Ordering::Relaxed);
        },
        |_, _, _, _| {},
    )
    .absolute()
    .top_0()
    .left_0()
    .size_full();

    let frac = tr.fraction() as f32;
    let track = div()
        .id("visualize-scrub")
        .relative()
        .flex_1()
        .h(px(16.))
        .flex()
        .items_center()
        .cursor_pointer()
        .child(probe)
        .child(
            div()
                .w_full()
                .h(px(4.))
                .rounded(radius::PILL)
                .bg(t.field)
                .child(
                    div()
                        .h_full()
                        .w(gpui::relative(frac))
                        .rounded(radius::PILL)
                        .bg(t.accent),
                ),
        )
        .on_mouse_down(
            MouseButton::Left,
            cx.listener(|app: &mut JadeApp, ev: &gpui::MouseDownEvent, _w, cx| {
                use std::sync::atomic::Ordering;
                app.visualize_scrubbing = true;
                let x0 = f32::from_bits(app.visualize_scrub_bounds[0].load(Ordering::Relaxed));
                let w = f32::from_bits(app.visualize_scrub_bounds[1].load(Ordering::Relaxed));
                if w > 1.0 {
                    let frac = (f32::from(ev.position.x) - x0) / w;
                    app.visualize_seek_fraction(frac);
                }
                cx.stop_propagation();
                cx.notify();
            }),
        );

    let replay_hint = tr.finished();
    let mut footer = Card::footer(t)
        .w_full()
        .gap(px(8.))
        .child(
            icon_button("visualize-play", play_icon, t).on_click(cx.listener(
                |app: &mut JadeApp, _e, _w, cx| {
                    app.visualize_toggle_play();
                    cx.notify();
                },
            )),
        )
        .child(track)
        .child(time_label(t, &tr));
    if replay_hint {
        footer = footer.child(
            div()
                .text_size(text::XS)
                .text_color(t.ink_3)
                .child("Replay"),
        );
    }
    footer.child(
        icon_button("visualize-again", "rotate-ccw", t).on_click(cx.listener(
            |app: &mut JadeApp, _e, _w, cx| {
                app.visualize_retry(cx);
                cx.notify();
            },
        )),
    )
}

/// `0:03 / 0:11`, monospace so the digits do not shove the scrub bar.
fn time_label(t: &BeautifulTokens, tr: &Transport) -> impl IntoElement {
    div()
        .flex_none()
        .font_family(crate::fonts::mono_family())
        .text_size(text::SM)
        .text_color(t.ink_3)
        .child(format!(
            "{} / {}",
            format_time(tr.position),
            format_time(tr.duration)
        ))
}

/// The one-time consent copy. Plain about what this runs and where.
fn consent_body(t: &BeautifulTokens) -> impl IntoElement {
    div()
        .flex()
        .flex_col()
        .items_center()
        .gap(px(8.))
        .max_w(px(VIDEO_W - 48.0))
        .child(
            div()
                .text_size(text::LG)
                .font_weight(gpui::FontWeight::MEDIUM)
                .text_color(t.ink)
                .child("Run generated Python on this machine?"),
        )
        .child(
            div()
                .text_size(text::BODY)
                .text_color(t.ink_2)
                .line_height(px(18.))
                .child(
                    "Visualize asks a model to write a Manim scene for the selection, \
                     then renders it locally. The render runs in a sandbox: no network, \
                     no access to your files outside its own work directory. \
                     Explain (⌘⇧E) works without this.",
                ),
        )
}

/// Enable / Not now.
fn consent_footer(t: &BeautifulTokens, cx: &mut Context<JadeApp>) -> impl IntoElement {
    let accept = div()
        .id("visualize-consent-accept")
        .flex()
        .items_center()
        .h(px(26.))
        .px(px(10.))
        .rounded(radius::CONTROL)
        .bg(t.accent_tint)
        .text_size(text::BODY)
        .font_weight(gpui::FontWeight::MEDIUM)
        .text_color(t.accent)
        .cursor_pointer()
        .child("Enable and visualize")
        .on_click(cx.listener(|app: &mut JadeApp, _e, _w, cx| {
            app.visualize_consent_accept(cx);
            cx.notify();
        }));
    let decline = div()
        .id("visualize-consent-decline")
        .flex()
        .items_center()
        .h(px(26.))
        .px(px(10.))
        .rounded(radius::CONTROL)
        .text_size(text::BODY)
        .text_color(t.ink_3)
        .cursor_pointer()
        .hover(|s| s.bg(gpui::rgba(0xffffff10)))
        .child("Not now")
        .on_click(cx.listener(|app: &mut JadeApp, _e, _w, cx| {
            app.close_visualize(cx);
            cx.notify();
        }));
    Card::footer(t).w_full().gap(px(8.)).child(accept).child(decline)
}

/// A failure, told as a sentence rather than a status code.
fn failure_body(t: &BeautifulTokens, err: &VisualizeError) -> impl IntoElement {
    let mut col = div()
        .flex()
        .flex_col()
        .items_center()
        .gap(px(6.))
        .max_w(px(VIDEO_W - 48.0))
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
                .overflow_hidden()
                .max_h(px(80.))
                .child(detail),
        );
    }
    col
}

/// `font-mono text-[12px] text-ink-3 tabular-nums` elapsed readout.
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

/// The header rail: what is visualized, how it is going, and the controls.
fn header(
    card: &VisualizeCard,
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

    // The clip's title once one exists — the model names what it drew.
    if let Some(title) = card
        .plan
        .as_ref()
        .map(|p| p.title.clone())
        .filter(|s| !s.trim().is_empty())
    {
        rail = rail.child(
            div()
                .text_size(text::CHIP)
                .text_color(t.ink_2)
                .overflow_hidden()
                .whitespace_nowrap()
                .max_w(px(180.))
                .child(title),
        );
    }

    match &card.phase {
        VisualizePhase::Ready => {
            rail = rail.child(status_pill("Ready", t.green, t.green_tint));
        }
        VisualizePhase::Unsuitable => {
            rail = rail.child(status_pill("Skipped", t.orange, t.orange_tint));
        }
        VisualizePhase::Failed(VisualizeError::Chat(jade_ai::ChatError::LocalStarting)) => {
            rail = rail.child(status_pill("Starting", t.orange, t.orange_tint));
        }
        VisualizePhase::Failed(_) => {
            rail = rail.child(status_pill("Failed", t.red, t.red_tint));
        }
        VisualizePhase::Consent => {}
        _ => {
            rail = rail.child(stream::shimmer_text(t, "Working", card.elapsed(now_ms)));
        }
    }

    if card.stale {
        rail = rail.child(status_pill("Edited", t.orange, t.orange_tint));
    }

    rail.child(div().flex_1())
        .child(
            icon_button("visualize-popout", "external-link", t).on_click(cx.listener(
                |app: &mut JadeApp, _e, _w, cx| {
                    app.open_visualize_popout(cx);
                },
            )),
        )
        .child(
            icon_button("visualize-close", "x", t).on_click(cx.listener(
                |app: &mut JadeApp, _e, _w, cx| {
                    app.close_visualize(cx);
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
    if !app.visualize_visible() {
        return None;
    }
    let card = app.visualize.as_ref()?;
    let t = beautiful::dark();
    let now = app.now_ms();

    let display_row = app
        .editor
        .active_tab()
        .map(|tab| tab.display_row(card.end_row))
        .unwrap_or(card.end_row);

    let anchor_x = super::code_view::popup_x(0, flow_visible, app.char_w(), app.editor_h_scroll());
    let anchor_y = super::code_view::popup_y(display_row, scroll_top);
    let (_, view_h) = app.editor_size();
    let anchor_y = super::explain_card::park_offscreen_anchor(anchor_y, view_h);
    let measured = app.visualize_card_height();
    let (left, top, max_h) =
        super::explain_card::clamp_card((anchor_x, anchor_y), CARD_W, app.editor_size(), measured);

    let mut el = Card::new(&t)
        .id("visualize-card")
        .debug_selector(|| "visualize-card".into())
        .absolute()
        .left(px(left))
        .top(px(top))
        .w(px(CARD_W))
        .max_h(px(max_h))
        .rounded(radius::CARD)
        .occlude()
        .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
        .child(height_probe(app.visualize_card_h.clone()))
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
        el = el.child(body(app, card, &t, now, Some(cx)));
    }

    Some(el.into_any_element())
}
