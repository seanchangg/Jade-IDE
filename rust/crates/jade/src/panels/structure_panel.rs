//! STRUCTURE panel (feature inventory §5.5).
//!
//! The left sidebar switches between FILES (the tree) and STRUCTURE (this) via a
//! tab switcher at the top ([`tab_switcher`]). STRUCTURE renders the active tab's
//! tree-sitter outline ([`crate::structure`]) as a mermaid-style tree: nested
//! symbols sit inside a group container with a left border (the vertical
//! connector), each row is a kind-colored dot + name, and clicking a row reveals
//! its line in the code viewer (`JadeApp::reveal_line`). The symbols themselves
//! come from [`crate::structure::parse_symbols`], computed once per open tab.

use gpui::{div, prelude::*, px, rgb, Context, SharedString};

use crate::app::{JadeApp, SidebarTab};
use crate::kumo::{scale, Empty, Size as KumoSize};
use crate::structure::{kind_color, Symbol, SymbolKind};
use crate::theme::Theme;

/// FILES | STRUCTURE switcher shown at the top of the left sidebar. Clicking a
/// tab flips `JadeApp::sidebar_tab`.
pub fn tab_switcher(app: &JadeApp, cx: &mut Context<JadeApp>, theme: &Theme) -> impl IntoElement {
    // The old `.sidebar-tabs`: two equal cells across the top of the panel,
    // a 2px brand rule under the active one, a hairline under the row.
    let t = &theme.kumo;
    let tab = |id: &'static str, label: &'static str, which: SidebarTab, active: bool| {
        let ink = if active { t.text_default } else { t.text_subtle };
        let hover_ink = t.text_default;
        let mut cell = div()
            .id(id)
            .relative()
            .flex()
            .flex_1()
            .items_center()
            .justify_center()
            .h_full()
            .text_size(scale::TEXT_XS)
            .font_weight(gpui::FontWeight::MEDIUM)
            .text_color(ink)
            .cursor_pointer()
            .on_click(cx.listener(move |a: &mut JadeApp, _e, _w, cx| {
                a.set_sidebar_tab(which);
                cx.notify();
            }))
            .child(label);
        if active {
            cell = cell.child(
                div()
                    .absolute()
                    .bottom_0()
                    .left_0()
                    .right_0()
                    .h(px(2.))
                    .bg(t.brand),
            );
        } else {
            cell = cell.hover(move |s| s.text_color(hover_ink));
        }
        cell
    };

    div()
        .flex()
        .flex_row()
        .flex_none()
        .h(px(28.))
        .w_full()
        .border_b_1()
        .border_color(t.hairline)
        .child(tab(
            "sb-files",
            "Files",
            SidebarTab::Files,
            app.sidebar_tab == SidebarTab::Files,
        ))
        .child(tab(
            "sb-structure",
            "Structure",
            SidebarTab::Structure,
            app.sidebar_tab == SidebarTab::Structure,
        ))
}

/// Render the outline for the active tab (or a muted placeholder).
pub fn render(app: &JadeApp, cx: &mut Context<JadeApp>) -> impl IntoElement {
    let theme = app.theme.clone();
    let symbols = app.active_symbols();

    let mut list = div().flex().flex_col().w_full();
    if symbols.is_empty() {
        // A Kumo Empty at `size="sm"` — the outline has nothing to show, so it
        // says so in the panel rather than leaving a blank column.
        let (title, body) = if app.editor.active_tab().is_some() {
            ("No symbols", "This file has no top-level declarations.")
        } else {
            ("No file open", "Open a file to see its outline.")
        };
        list = list.child(
            Empty::new(title)
                .body(body)
                .icon("list-tree")
                .size(KumoSize::Sm)
                .render(&theme.kumo),
        );
    } else {
        for sym in symbols {
            list = list.child(node(sym, &theme, cx));
        }
    }

    div()
        .id("structure-scroll")
        .flex_1()
        .px(scale::SPACE_1)
        .overflow_y_scroll()
        .child(list)
}

/// One symbol row plus (indented, connector-bordered) its children.
fn node(sym: &Symbol, theme: &Theme, cx: &mut Context<JadeApp>) -> gpui::AnyElement {
    let line = sym.line;
    let dot_color = kind_color(sym.kind, theme);

    // The clickable pill: dot + name (+ a muted access tag for members/methods).
    let mut pill = div()
        .id(SharedString::from(format!("sym-{}-{}", sym.name, sym.line)))
        .flex()
        .flex_row()
        .items_center()
        .gap(scale::SPACE_1_5)
        .h(px(22.))
        .px(scale::SPACE_2)
        .text_xs()
        .cursor_pointer()
        .hover(|s| s.bg(theme.kumo.fill_hover))
        .on_click(cx.listener(move |a: &mut JadeApp, _e, _w, cx| {
            a.reveal_line(line);
            cx.notify();
        }))
        .child(
            // Kind-colored dot.
            div()
                .w(px(7.))
                .h(px(7.))
                .flex_none()
                .rounded_full()
                .bg(rgb(dot_color)),
        )
        .child(div().text_color(rgb(theme.text)).child(sym.name.clone()));

    // Public is the default reading of a member, so only the other two
    // access levels earn a tag.
    if let Some(access) = sym.access.filter(|a| a.label() != "public") {
        pill = pill.child(
            div()
                .text_color(rgb(theme.muted))
                .child(SharedString::from(access.label())),
        );
    }
    // A subtle kind label keeps free functions vs types legible.
    pill = pill.child(
        div()
            .text_color(rgb(theme.muted))
            .child(SharedString::from(kind_label(sym.kind))),
    );

    let mut container = div().flex().flex_col().child(pill);

    if !sym.children.is_empty() {
        // Children group: left border == the mermaid vertical connector.
        let mut group = div()
            .flex()
            .flex_col()
            .ml(px(11.))
            .pl(px(4.))
            .border_l_1()
            .border_color(theme.kumo.hairline);
        for child in &sym.children {
            group = group.child(node(child, theme, cx));
        }
        container = container.child(group);
    }

    container.into_any_element()
}

fn kind_label(kind: SymbolKind) -> &'static str {
    match kind {
        SymbolKind::Namespace => "namespace",
        SymbolKind::Class => "class",
        SymbolKind::Struct => "struct",
        SymbolKind::Function => "fn",
        SymbolKind::Method => "method",
        SymbolKind::Member => "member",
        SymbolKind::Enum => "enum",
    }
}
