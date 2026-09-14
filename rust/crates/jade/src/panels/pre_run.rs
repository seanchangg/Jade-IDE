//! Pre-run tracking panel: Run/Debug open this overlay first so the user
//! picks which timers and buffers to track before the program launches.
//!
//! Timers default to untracked (the probe doesn't even emit samples until
//! asked — see `jade_probe.mm`'s timer gating), so this panel is where a
//! run's telemetry cost is decided. The list comes from the registry; when
//! it's empty a short discovery run (`JadeApp::start_discovery`) launches the
//! built executable for a few seconds with everything untracked, harvesting
//! decls to fill the list live.
//!
//! Checkbox toggles route through `JadeApp::toggle_enabled`, which persists
//! the choice (prefs) and sends `track` — on the next run the probe's decl →
//! pref → track handshake re-arms tracking automatically.

use gpui::{div, prelude::*, px, rgb, Context, FocusHandle, KeyDownEvent};

use jade_telemetry::Kind;

use crate::app::{JadeApp, PreRunLaunch};
use crate::theme::Theme;

/// True when a keystroke should type a character into the bundle-name buffer
/// (same rule as quick_open: printable `key_char`, no command chord).
fn is_printable(ev: &KeyDownEvent) -> bool {
    let m = &ev.keystroke.modifiers;
    ev.keystroke.key_char.is_some() && !m.platform && !m.control && !m.alt && !m.function
}

/// One checkbox row. `detail` is the dim right-side annotation (buffer shape
/// or bundle membership); `stage` adds the bundle-staging toggle (timers);
/// `seeded` dims the name — remembered from a previous session, not yet
/// declared by the probe this session.
fn item_row(
    kind: Kind,
    name: &str,
    enabled: bool,
    seeded: bool,
    detail: Option<String>,
    staged: Option<bool>,
    theme: &Theme,
    cx: &mut Context<JadeApp>,
) -> impl IntoElement {
    let toggle_name = name.to_string();
    let mut row = div()
        .id(gpui::ElementId::Name(
            format!("prerun-{}", crate::registry::key_of(kind, name)).into(),
        ))
        .flex()
        .flex_row()
        .items_center()
        .gap_2()
        .px(px(10.))
        .py(px(3.))
        .text_xs()
        .cursor_pointer()
        .hover(|s| s.bg(theme.kumo.tint))
        .on_click(cx.listener(move |app, _e, _w, cx| {
            app.toggle_enabled(kind, &toggle_name);
            cx.notify();
        }))
        .child(
            div()
                .w(px(14.))
                .text_color(rgb(if enabled { theme.accent } else { theme.muted }))
                .child(if enabled { "☑" } else { "☐" }),
        )
        .child(
            div()
                .flex_1()
                .overflow_hidden()
                .text_color(rgb(if seeded { theme.muted } else { theme.text }))
                .child(name.to_string()),
        );
    if let Some(d) = detail {
        row = row.child(div().text_color(rgb(theme.muted)).child(d));
    }
    if let Some(is_staged) = staged {
        let stage_name = name.to_string();
        row = row.child(
            div()
                .id(gpui::ElementId::Name(format!("prerun-stage-{name}").into()))
                .px_1()
                
                .text_color(rgb(if is_staged { theme.periwinkle } else { theme.muted }))
                .hover(|s| s.text_color(rgb(theme.periwinkle)))
                .on_click(cx.listener(move |app, _e, _w, cx| {
                    cx.stop_propagation(); // don't also toggle the checkbox
                    app.toggle_group_staging(&stage_name);
                    cx.notify();
                }))
                .child(if is_staged { "▣" } else { "◧" }),
        );
    }
    row
}

/// The TIMERS section: raw timers with a per-row staging toggle (◧). Bundles
/// and the staging name box live in the side card ([`bundles_card`]).
fn timers_section(app: &JadeApp, theme: &Theme, cx: &mut Context<JadeApp>) -> gpui::AnyElement {
    let timers: Vec<(String, bool, bool)> = app
        .registry
        .items_of_kind(Kind::Timer)
        .iter()
        .filter(|i| app.timer_groups.get(&i.name).is_none()) // bundles render in the card
        .map(|i| (i.name.clone(), i.enabled, i.seeded))
        .collect();

    let mut col = div().flex().flex_col().child(
        div()
            .px(px(10.))
            .pt(px(8.))
            .pb(px(2.))
            .text_size(px(10.))
            .font_weight(gpui::FontWeight::MEDIUM)
            .text_color(rgb(theme.muted))
            .child(format!("TIMERS ({})", timers.len())),
    );

    if timers.is_empty() {
        col = col.child(
            div()
                .px(px(10.))
                .py(px(2.))
                .text_xs()
                .text_color(rgb(theme.muted))
                .child("none discovered"),
        );
    }
    for (name, enabled, seeded) in timers {
        let in_groups = app.timer_groups.groups_of(&name);
        let detail = (!in_groups.is_empty()).then(|| format!("∈ {}", in_groups.join(", ")));
        let staged = app.group_staging.iter().any(|n| n == &name);
        col = col.child(item_row(
            Kind::Timer,
            &name,
            enabled,
            seeded,
            detail,
            Some(staged),
            theme,
            cx,
        ));
    }
    col.into_any_element()
}

/// The BUNDLES side card: existing bundles (toggle / dissolve) plus the
/// staging name box while timers are staged. `None` when there's nothing to
/// show — the card only appears once bundling is in play.
fn bundles_card(
    app: &JadeApp,
    theme: &Theme,
    cx: &mut Context<JadeApp>,
) -> Option<gpui::AnyElement> {
    let staging = !app.group_staging.is_empty();
    if app.timer_groups.is_empty() && !staging {
        return None;
    }

    // "(0)" reads like an error while the first bundle is still being staged;
    // show the count only once bundles exist.
    let n_bundles = app.timer_groups.defs().len();
    let header_text = if n_bundles > 0 {
        format!("BUNDLES ({n_bundles})")
    } else {
        "BUNDLES".to_string()
    };
    let mut col = div().flex().flex_col().pb(px(6.)).child(
        div()
            .px(px(10.))
            .pt(px(8.))
            .pb(px(2.))
            .text_size(px(10.))
            .font_weight(gpui::FontWeight::MEDIUM)
            .text_color(rgb(theme.muted))
            .child(header_text),
    );

    // Bundle rows: ☑ Forward (Σ 5 timers) ✕
    for def in app.timer_groups.defs() {
        let gname = def.name.clone();
        let enabled = app.registry.is_enabled(Kind::Timer, &gname);
        let n = def.members.len();
        let toggle = gname.clone();
        let dissolve = gname.clone();
        col = col.child(
            div()
                .id(gpui::ElementId::Name(format!("prerun-group-{gname}").into()))
                .flex()
                .flex_row()
                .items_center()
                .gap_2()
                .px(px(10.))
                .py(px(3.))
                .text_xs()
                .cursor_pointer()
                .hover(|s| s.bg(theme.kumo.tint))
                .on_click(cx.listener(move |app, _e, _w, cx| {
                    app.toggle_timer_group(&toggle);
                    cx.notify();
                }))
                .child(
                    div()
                        .w(px(14.))
                        .text_color(rgb(if enabled { theme.accent } else { theme.muted }))
                        .child(if enabled { "☑" } else { "☐" }),
                )
                .child(
                    div()
                        .flex_1()
                        .overflow_hidden()
                        .text_color(rgb(theme.periwinkle))
                        .child(format!("{gname} (Σ {n})")),
                )
                .child(
                    div()
                        .id(gpui::ElementId::Name(
                            format!("prerun-group-del-{gname}").into(),
                        ))
                        .px_1()
                        
                        .text_color(rgb(theme.muted))
                        .hover(|s| s.text_color(rgb(theme.red)))
                        .on_click(cx.listener(move |app, _e, _w, cx| {
                            cx.stop_propagation();
                            app.dissolve_timer_group(&dissolve);
                            cx.notify();
                        }))
                        .child("✕"),
                ),
        );
    }

    // Staging name box: appears while ◧-staged timers await a bundle name.
    if staging {
        col = col.child(
            div()
                .flex()
                .flex_col()
                .gap_1()
                .mx(px(8.))
                .mt(px(6.))
                .px(px(8.))
                .py(px(6.))
                
                .bg(rgb(theme.bg))
                .text_xs()
                // Title row: NEW BUNDLE ……… "7 timers"
                .child(
                    div()
                        .flex()
                        .flex_row()
                        .items_center()
                        .justify_between()
                        .child(
                            div()
                                .text_color(rgb(theme.periwinkle))
                                .child("NEW BUNDLE"),
                        )
                        .child(div().text_color(rgb(theme.muted)).child(format!(
                            "{} timer{}",
                            app.group_staging.len(),
                            if app.group_staging.len() == 1 { "" } else { "s" }
                        ))),
                )
                // Name input, framed like a real field.
                .child(
                    div()
                        .px(px(6.))
                        .py(px(3.))
                        
                        .bg(rgb(theme.panel))
                        .border_1()
                        .border_color(rgb(theme.border))
                        .child(crate::panels::input_line(
                            &app.group_name_input,
                            "name…",
                            true,
                            true,
                            theme.periwinkle,
                            theme,
                            app.group_name_input.len(),
                        )),
                )
                .child(
                    div()
                        .text_size(px(10.))
                        .text_color(rgb(theme.muted))
                        .child("↵ create · esc clear"),
                ),
        );
    }

    Some(
        crate::kumo::Surface::overlay(&theme.kumo)
            .w(px(230.))
            .max_h(px(440.))
            .flex()
            .flex_col()
            .overflow_hidden()
            .occlude() // same backdrop-fallthrough guard as the main panel
            .child(col)
            .into_any_element(),
    )
}

fn section(
    title: &str,
    kind: Kind,
    app: &JadeApp,
    theme: &Theme,
    cx: &mut Context<JadeApp>,
) -> gpui::AnyElement {
    // Buffers get a type-to-filter search (buffer lists run long); the query
    // is edited by the panel's key routing (`JadeApp::pre_run_key`).
    let query = (kind == Kind::Buffer && !app.buffer_search.is_empty())
        .then(|| app.buffer_search.to_lowercase());
    let all = app.registry.items_of_kind(kind);
    let total = all.len();
    let items: Vec<(String, bool, bool, Option<String>)> = all
        .iter()
        .filter(|i| {
            query
                .as_deref()
                .is_none_or(|q| i.name.to_lowercase().contains(q))
        })
        .map(|i| {
            let detail = match (i.meta_rows.or(i.last_rows), i.meta_cols.or(i.last_cols)) {
                (Some(r), Some(c)) => Some(format!("{r}×{c}")),
                _ => None,
            };
            (i.name.clone(), i.enabled, i.seeded, detail)
        })
        .collect();

    let count = if query.is_some() {
        format!("{title} ({}/{total})", items.len())
    } else {
        format!("{title} ({total})")
    };
    let mut col = div().flex().flex_col().child(
        div()
            .px(px(10.))
            .pt(px(8.))
            .pb(px(2.))
            .text_size(px(10.))
            .font_weight(gpui::FontWeight::MEDIUM)
            .text_color(rgb(theme.muted))
            .child(count),
    );

    // Search row (buffers only). The caret hides while the staging editor
    // owns the keyboard, signalling where keystrokes will land.
    if kind == Kind::Buffer {
        let typing_here = app.group_staging.is_empty();
        col = col.child(
            div()
                .flex()
                .flex_row()
                .items_center()
                .gap_1()
                .mx(px(10.))
                .my(px(2.))
                .px(px(6.))
                .py(px(2.))
                
                .bg(rgb(theme.bg))
                .border_1()
                .border_color(rgb(theme.border))
                .text_xs()
                .child(div().text_color(rgb(theme.muted)).child("⌕"))
                .child(crate::panels::input_line(
                    &app.buffer_search,
                    "type to filter…",
                    typing_here,
                    true,
                    theme.periwinkle,
                    theme,
                    app.buffer_search.len(),
                )),
        );
    }

    if items.is_empty() {
        col = col.child(
            div()
                .px(px(10.))
                .py(px(2.))
                .text_xs()
                .text_color(rgb(theme.muted))
                .child(if query.is_some() {
                    "no matches"
                } else {
                    "none discovered"
                }),
        );
    }
    for (name, enabled, seeded, detail) in items {
        col = col.child(item_row(kind, &name, enabled, seeded, detail, None, theme, cx));
    }
    col.into_any_element()
}

/// Build the overlay. `focus` is pre-created and focused by the app so
/// Esc/Enter land here.
pub fn overlay(app: &JadeApp, focus: FocusHandle, cx: &mut Context<JadeApp>) -> gpui::AnyElement {
    let theme = app.theme.clone();
    let panel_state = app.pre_run.expect("overlay rendered only while open");
    let discovering = panel_state.discovering;
    let launch_label = match panel_state.launch {
        PreRunLaunch::Run => "Run",
        PreRunLaunch::Debug => "Debug",
    };

    // Header: title + discovery status / rescan.
    let status: gpui::AnyElement = if discovering {
        // Past the normal window with no telemetry flowing yet → the app is
        // still starting (e.g. Metal is cold-compiling a heavy pipeline).
        let label = match (app.discovery_warm_at, app.discovery_started) {
            (None, Some(start))
                if start.elapsed().as_secs() > crate::app::DISCOVERY_SECS =>
            {
                format!("scanning… app still starting ({}s)", start.elapsed().as_secs())
            }
            _ => "scanning…".to_string(),
        };
        div()
            .text_xs()
            .text_color(rgb(theme.amber))
            .child(label)
            .into_any_element()
    } else if app.can_run() {
        crate::kumo::Button::new("prerun-rescan", "Rescan")
            .variant(crate::kumo::ButtonVariant::Ghost)
            .size(crate::kumo::Size::Sm)
            .icon("refresh-cw")
            .render(&theme.kumo)
            .on_click(cx.listener(|app, _e, _w, cx| {
                app.start_discovery();
                cx.notify();
            }))
            .into_any_element()
    } else {
        div()
            .text_xs()
            .text_color(rgb(theme.muted))
            .child("build once (⌘B) to enable discovery")
            .into_any_element()
    };

    let header = div()
        .flex()
        .flex_row()
        .items_center()
        .justify_between()
        .h(px(34.))
        .pl(px(10.))
        .pr(px(6.))
        .border_b_1()
        .border_color(theme.kumo.hairline)
        .child(
            div()
                .text_xs()
                .font_weight(gpui::FontWeight::SEMIBOLD)
                .text_color(rgb(theme.text))
                .child("Track telemetry"),
        )
        .child(status);

    // Footer: Cancel / Run.
    let footer = div()
        .flex()
        .flex_row()
        .items_center()
        .justify_between()
        .h(px(40.))
        .pl(px(10.))
        .pr(px(6.))
        .border_t_1()
        .border_color(theme.kumo.hairline)
        .child(
            div()
                .text_xs()
                .text_color(rgb(theme.muted))
                .child("esc to cancel · enter to launch"),
        )
        .child(
            div()
                .flex()
                .flex_row()
                .gap(px(4.))
                .child(
                    crate::kumo::Button::new("prerun-cancel", "Cancel")
                        .variant(crate::kumo::ButtonVariant::Ghost)
                        .size(crate::kumo::Size::Sm)
                        .render(&theme.kumo)
                        .on_click(cx.listener(|app, _e, _w, cx| {
                            app.cancel_pre_run();
                            cx.notify();
                        })),
                )
                .child(
                    crate::kumo::Button::new("prerun-launch", launch_label)
                        .variant(crate::kumo::ButtonVariant::Tinted)
                        .ink(theme.kumo.text_link)
                        .icon("play")
                        .size(crate::kumo::Size::Sm)
                        .render(&theme.kumo)
                        .on_click(cx.listener(|app, _e, _w, cx| {
                            app.confirm_pre_run();
                            cx.notify();
                        })),
                ),
        );

    let body = div()
        .id("prerun-body")
        .flex_1()
        .overflow_y_scroll()
        .child(timers_section(app, &theme, cx))
        .child(section("BUFFERS", Kind::Buffer, app, &theme, cx));

    let panel = crate::kumo::Surface::overlay(&theme.kumo)
        .w(px(460.))
        .max_h(px(440.))
        .flex()
        .flex_col()
        .overflow_hidden()
        // Clicks inside the panel must not fall through to the full-window
        // backdrop underneath (its handler cancels the panel — without this,
        // every checkbox click also closed the popup).
        .occlude()
        .child(header)
        .child(body)
        .child(footer);

    // Panel + optional BUNDLES side card, laid out as a row so the card sits
    // next to the panel instead of pushing the lists down.
    let mut cluster = div()
        .flex()
        .flex_row()
        .items_start()
        .gap_2()
        .child(panel);
    if let Some(card) = bundles_card(app, &theme, cx) {
        cluster = cluster.child(card);
    }

    // Full-area overlay: dim backdrop (click cancels), cluster centered.
    div()
        .absolute()
        .top_0()
        .left_0()
        .size_full()
        .flex()
        .flex_col()
        .items_center()
        .pt(px(120.))
        .bg(gpui::rgba(0x00000066))
        .track_focus(&focus)
        // Swallow wheel events at the overlay root: the panel body (deeper, so
        // it handles first) scrolls its own list, and whatever is left must
        // not fall through to the editor's scroll container underneath.
        .on_scroll_wheel(cx.listener(|_app: &mut JadeApp, _ev, _w, cx| {
            cx.stop_propagation();
        }))
        .on_key_down(cx.listener(|app: &mut JadeApp, ev: &KeyDownEvent, _w, cx| {
            // The bundle-staging editor eats keys first (name typing).
            if app.pre_run_key(
                &ev.keystroke.key.clone(),
                ev.keystroke.key_char.as_deref(),
                is_printable(ev),
            ) {
                cx.notify();
                return;
            }
            match ev.keystroke.key.as_str() {
                "escape" => app.cancel_pre_run(),
                "enter" => app.confirm_pre_run(),
                _ => return,
            }
            cx.notify();
        }))
        .child(
            div()
                .id("prerun-backdrop")
                .absolute()
                .top_0()
                .left_0()
                .size_full()
                .on_click(cx.listener(|app: &mut JadeApp, _e, _w, cx| {
                    app.cancel_pre_run();
                    cx.notify();
                })),
        )
        .child(cluster)
        .into_any_element()
}
