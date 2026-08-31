//! The Explain card in its own window.
//!
//! A pure projection of `JadeApp::explain`, exactly like
//! [`crate::panels::metric_popout`]: no state moves here, so the in-editor card
//! and this window always agree and closing either one loses nothing.

use gpui::{div, prelude::*, px, Context, Entity, Window};

use crate::app::JadeApp;
use crate::beautiful::{self, text};

/// Root view of the Explain pop-out window.
pub struct ExplainPopout {
    app: Entity<JadeApp>,
    /// The card generation this window was opened for. When the app moves on
    /// to a different explanation the window says so rather than silently
    /// swapping its contents underneath the reader.
    generation: u64,
}

impl ExplainPopout {
    pub fn new(app: Entity<JadeApp>, generation: u64, cx: &mut Context<Self>) -> Self {
        // The app notifies on every applied delta; piggyback on that so the
        // prose streams here too.
        cx.observe(&app, |_, _, cx| cx.notify()).detach();
        Self { app, generation }
    }
}

impl Render for ExplainPopout {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let app = self.app.read(cx);
        let t = beautiful::dark();
        let card = app.explain.as_ref().filter(|c| c.generation == self.generation);

        let body = match card {
            Some(c) => super::explain_card::body(c, &t, app.now_ms(), None).into_any_element(),
            None => div()
                .flex()
                .flex_col()
                .items_center()
                .justify_center()
                .size_full()
                .gap(px(6.))
                .child(
                    div()
                        .text_size(text::LG)
                        .text_color(t.ink_2)
                        .child("This explanation was closed"),
                )
                .child(
                    div()
                        .text_size(text::BODY)
                        .text_color(t.ink_3)
                        .child("Press ⌘⇧E in the editor to explain another selection."),
                )
                .into_any_element(),
        };

        div()
            .bg(t.page)
            .text_color(t.ink)
            .size_full()
            .flex()
            .flex_col()
            .p(px(16.))
            .font_family(crate::fonts::mono_family())
            .font_features(crate::fonts::code_features())
            .text_sm()
            .child(div().flex_1().min_h(px(0.)).child(body))
    }
}
