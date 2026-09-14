//! File-tree panel (feature inventory §5.1, deliverable §2).
//!
//! Header "FILES" + a minimize placeholder; one row per visible tree node with a
//! leading lucide icon (folder/folder-open for directories, one glyph per
//! `FileKind` for files — see `kind_glyph` and `crate::assets`), indentation
//! `12 + depth*16` px, source files accent-colored / headers accent2, and the
//! active file's row highlighted.
//! Clicking a directory toggles expansion (lazy-loading its children); clicking a
//! file opens it in the editor.
//!
//! Rows are plain divs (not virtualized): the visible set is bounded by what the
//! user has expanded, and per-row click handlers use `cx.listener`. The heavy
//! virtualization lives in the code viewer (5k-line files), not here.

use gpui::{div, prelude::*, px, rgb, Context};

use crate::app::JadeApp;
use crate::kumo::{scale, Size as KumoSize, Text as KumoText, TextTone};
use crate::theme::Theme;
use crate::workspace_tree::{FileKind, Row};

pub fn render(app: &JadeApp, cx: &mut Context<JadeApp>) -> impl IntoElement {
    let theme = app.theme.clone();

    let mut list = div().flex().flex_col().w_full();

    match &app.tree {
        Some(tree) => {
            let active = app.active_file.clone();
            let selected = app.tree_selection.clone();
            for row in tree.visible_rows() {
                list = list.child(tree_row(
                    row,
                    active.as_deref(),
                    selected.as_deref(),
                    &theme,
                    cx,
                ));
            }
        }
        None => {
            list = list.child(
                div().px(scale::SPACE_2).py(scale::SPACE_1_5).child(
                    KumoText::new("Open a folder to get started")
                        .tone(TextTone::Secondary)
                        .size(KumoSize::Xs)
                        .render(&theme.kumo),
                ),
            );
        }
    }

    div()
        .flex()
        .flex_col()
        .child(div().id("file-tree-scroll").flex_1().overflow_y_scroll().child(list))
}

fn tree_row(
    row: Row,
    active: Option<&std::path::Path>,
    selected: Option<&std::path::Path>,
    theme: &Theme,
    cx: &mut Context<JadeApp>,
) -> impl IntoElement {
    let indent = 8.0 + row.depth as f32 * 14.0;
    let is_active = active == Some(row.path.as_path());
    // The selected row is the one a new terminal opens in (§5.2). A directory
    // shows it; for a file the active-tab highlight already says the same thing.
    let is_selected = row.is_dir && selected == Some(row.path.as_path());
    let path = row.path.clone();

    // Leading icon: a folder (open when expanded) for dirs; for files, one glyph
    // per file kind (see `kind_glyph`). The color inherits into the icon via
    // text_color.
    let (icon_name, glyph_color) = if row.is_dir {
        let name = if row.expanded { "folder-open" } else { "folder" };
        (name, theme.muted)
    } else {
        kind_glyph(row.kind, theme)
    };

    // Label color (`.file-tree-name`): the default ink, brand on the active
    // row. The icon alone carries the file kind.
    let label_color = if is_active { theme.accent } else { theme.text };

    let mut el = div()
        .id(("tree-row", row_id(&row.path)))
        .flex()
        .flex_row()
        .items_center()
        .gap(scale::SPACE_1_5)
        .h(px(24.))
        .pl(px(indent))
        .pr(scale::SPACE_2)
        .text_size(scale::TEXT_XS)
        .cursor_pointer()
        // Hover affordance — Kumo's `hover:bg-kumo-fill-hover`.
        .hover(|s| s.bg(theme.kumo.fill_hover));

    // The active row takes `.active-line`, the recessed step; a selected
    // directory the same.
    if is_active || is_selected {
        el = el.bg(theme.kumo.recessed);
    }

    let is_dir = row.is_dir;
    el.on_click(cx.listener(move |app, _ev, _win, cx| {
        // Every click sets the selection, so the next terminal opens here.
        app.select_tree_path(path.clone());
        if is_dir {
            app.toggle_dir(path.clone());
        } else {
            app.open_file(path.clone());
            app.schedule_ui_save(cx); // openTabs / activeTabIndex (§1.2)
        }
        cx.notify();
    }))
    .child(
        div()
            .w(px(14.))
            .flex_none()
            .flex()
            .items_center()
            .text_color(rgb(glyph_color))
            .child(crate::assets::ui_icon(icon_name, 14., glyph_color)),
    )
    .child(div().text_color(rgb(label_color)).child(row.name))
}

/// The icon name and tint for one file kind. Every name here must also appear in
/// `assets::ICONS` (the `every_ui_icon_resolves` test guards that).
///
/// Color follows the old `.file-tree-icon` rules: the source kinds take the
/// brand, headers the type color, and everything else the subtle ink — so a
/// tree reads as one column of glyphs with the compilable files picked out.
fn kind_glyph(kind: FileKind, theme: &Theme) -> (&'static str, u32) {
    match kind {
        FileKind::Source => ("file-code", theme.accent),
        FileKind::Header => ("code", theme.blue_gray),
        FileKind::Hdl => ("cpu", theme.accent),
        FileKind::Shader => ("cpu", theme.accent),
        FileKind::Script => ("file-code", theme.muted),
        FileKind::Shell => ("file-terminal", theme.muted),
        FileKind::Build => ("hammer", theme.muted),
        FileKind::Config => ("settings", theme.muted),
        FileKind::Data => ("braces", theme.muted),
        FileKind::Table => ("table", theme.muted),
        FileKind::Model => ("box", theme.muted),
        FileKind::Doc => ("file-text", theme.muted),
        FileKind::Image => ("image", theme.muted),
        FileKind::Archive => ("package", theme.muted),
        FileKind::Lock => ("lock", theme.muted),
        FileKind::Other => ("file", theme.muted),
    }
}

/// Stable-ish element id from a path (hash of the string form).
fn row_id(path: &std::path::Path) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    path.hash(&mut h);
    h.finish()
}

