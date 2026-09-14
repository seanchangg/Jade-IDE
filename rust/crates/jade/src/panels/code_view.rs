//! Read-only code viewer (deliverable §5).
//!
//! A virtualized line list (`uniform_list`) so a 5k-line file renders only the
//! visible rows, never 5k divs/frame. Each row is a fixed-height (20px) line: a
//! right-aligned gutter line number (#4B4E56) plus the code, rendered as a single
//! [`StyledText`] whose per-token color runs come from the tab's precomputed
//! highlight spans (`with_highlights`; gaps fall back to the default text color).
//! Menlo 13px, line-height 20, padding-top 16, horizontal overflow clipped
//! (no wrap).

use std::collections::HashMap;

use gpui::{
    canvas, div, point, prelude::*, px, rgb, rgba, size, uniform_list, Bounds, ClickEvent, Context,
    ElementInputHandler, Entity, FocusHandle, HighlightStyle, KeyDownEvent,
    ListHorizontalSizingBehavior, MouseButton, MouseDownEvent, MouseMoveEvent, Rgba,
    UnderlineStyle,
};
use jade_buffer::Point;

use crate::app::{CompletionState, HoverState, JadeApp, SignatureState};
use crate::panes::{TabDrag, TabDragPreview};
use crate::decorations::flow::GlyphKind;
use crate::decorations::{self, RuntimeAlloc};
use crate::editor_view::{self, CellStyle};
use crate::kumo::scale;
use crate::theme::Theme;

/// Editor metrics (§4.1 "editor look").
const FONT_PX: f32 = 13.0;
pub const LINE_H: f32 = 20.0;
pub const PAD_TOP: f32 = 16.0;
const GUTTER_W: f32 = 52.0;
/// Width of the flow glyph margin column (shown only when Flow is toggled on).
const GLYPH_W: f32 = 16.0;
/// Width of the fold-chevron column between the gutter and the code.
const FOLD_W: f32 = 12.0;
/// Menlo's char advance at [`FONT_PX`] (Menlo is monospace, ~0.602 em → 7.83px
/// at 13px). Used for caret/selection/popup placement and click→column mapping;
/// the two share this constant so hit-testing stays self-consistent. Wide glyphs
/// (CJK/emoji) count as one column — an accepted, documented approximation.
pub const CHAR_W: f32 = 7.83;
/// End-of-line annotation text size (§4.5: 11px emerald @0.7 opacity).
const ANN_PX: f32 = 11.0;

/// A packed `0xRRGGBB` with an alpha fraction → `gpui::Rgba`.
fn rgba_a(rgb: u32, alpha: f32) -> Rgba {
    let a = (alpha.clamp(0.0, 1.0) * 255.0).round() as u32;
    rgba((rgb << 8) | (a & 0xff))
}

/// Base color for a flow glyph kind, resolved from the active [`Theme`] (§4.8
/// FLOW_COLORS map onto accent/periwinkle/amber/red so the light theme applies).
fn flow_color(kind: GlyphKind, theme: &Theme) -> u32 {
    match kind {
        GlyphKind::Sequential => theme.accent,
        GlyphKind::Call | GlyphKind::Loop => theme.periwinkle,
        GlyphKind::Return | GlyphKind::Branch => theme.amber,
        GlyphKind::Error => theme.red,
    }
}

/// Whole-line (bg alpha, left-border alpha) for a flow kind (§4.8 CSS `:87-110`).
fn flow_tint(kind: GlyphKind) -> (f32, f32) {
    match kind {
        GlyphKind::Call => (0.08, 0.375), // rgba .08 / border @60
        GlyphKind::Error => (0.12, 0.5),  // rgba .12 / border @80
        _ => (0.06, 0.25),                // rgba .06 / border @40
    }
}

/// Render the editable editor for the active tab, or a centered placeholder when
/// none is open. The surface is focusable (keys → [`JadeApp::editor_key`], text →
/// IME `replace_text_in_range`), mouse-selectable, and overlays the autocomplete
/// + hover popups.
pub fn render(app: &JadeApp, cx: &mut Context<JadeApp>) -> gpui::AnyElement {
    let theme = app.theme.clone();

    let Some(tab) = app.editor.active_tab() else {
        return div()
            .flex()
            .flex_1()
            .size_full()
            .items_center()
            .justify_center()
            .child(
                div()
                    .text_color(rgb(theme.muted))
                    .child("No file open — pick one from the tree"),
            )
            .into_any_element();
    };

    let visible_count = tab.visible_rows().len();
    let default_color = theme.text;
    let flow_visible_now = app.flow_visible;

    // The focus handle backing keyboard + IME input (created lazily in the shell).
    let handle = app
        .editor_focus
        .clone()
        .unwrap_or_else(|| cx.focus_handle());

    let list = uniform_list(
        "code-lines",
        visible_count,
        cx.processor(move |this, range: std::ops::Range<usize>, window, cx| {
            let mut rows = Vec::with_capacity(range.len());

            let flow_visible = this.flow_visible;
            let error_line = this.error_line;
            let exec_line = this.exec_pointer_line();
            let has_run = !this.last_executed.is_empty();
            let theme = this.theme.clone();
            // Ghost text (§4.11): shown on the caret row only. Multi-line ghosts
            // render their first line inline with a `⏎…` continuation marker; Tab
            // still inserts the full suggestion.
            let ghost_render: Option<(usize, String)> = this.ghost.as_ref().map(|g| {
                let first = g.text.lines().next().unwrap_or("").to_string();
                let shown = if g.text.contains('\n') {
                    format!("{first} ⏎…")
                } else {
                    first
                };
                (g.anchor.0, shown)
            });
            let focused = this
                .editor_focus
                .as_ref()
                .map(|f| f.is_focused(window))
                .unwrap_or(false);
            let row_handle = this.editor_focus.clone();
            let char_w = this.char_w();
            // Find matches (whole-buffer byte ranges) captured before the tab
            // borrow, so every occurrence — not just the current one — is washed.
            let find_matches: Vec<std::ops::Range<usize>> = this
                .find
                .as_ref()
                .map(|f| f.matches.clone())
                .unwrap_or_default();

            let Some(tab) = this.editor.active_tab() else {
                return rows;
            };

            let runtime = runtime_allocs_for(this, tab);
            let Some(tab) = this.editor.active_tab() else {
                return rows;
            };

            // Uniform code-cell width (§ horizontal scroll): the longest line plus
            // a trailing margin, so every row is at least this wide. Rows wider
            // than the viewport make the `Unconstrained` list scroll sideways; the
            // uniform min-width also keeps the whole line band clickable past EOL.
            let code_w = tab.max_cols as f32 * char_w + 48.0;

            // Caret + selection (buffer byte coordinates).
            let sel = tab.buffer.selection();
            let caret_point = tab.caret_point();

            // Display → buffer row map (folds hide interior rows).
            let visible = tab.visible_rows();

            for di in range {
                let Some(&i) = visible.get(di) else { break };
                let line_no = i + 1;
                let text = tab.line(i);
                let line_bytes = text.len();
                let line_start = tab.buffer.point_to_offset(Point::new(i, 0));
                let line_end = line_start + line_bytes;

                // Style layers → flattened runs (syntax · selection · diagnostics
                // · IME marked), see editor_view::merge_line_styles.
                let syntax = tab.highlights.get(i).map(Vec::as_slice).unwrap_or(&[]);
                let selection = editor_view::selection_on_line(sel, line_start, line_end);
                let underlines = editor_view::diagnostic_underlines_for_row(
                    &tab.diagnostics,
                    i,
                    &tab.buffer,
                    theme.red,
                    theme.amber,
                    theme.periwinkle,
                );
                let marked = tab.marked.as_ref().and_then(|m| {
                    editor_view::selection_on_line(
                        jade_buffer::Selection::new(m.start, m.end),
                        line_start,
                        line_end,
                    )
                });
                let line_matches =
                    editor_view::find_matches_on_line(&find_matches, line_start, line_end);
                let cells = editor_view::merge_line_styles(
                    line_bytes,
                    syntax,
                    selection,
                    &underlines,
                    marked,
                    &line_matches,
                );
                // Hard tabs expand to spaces before shaping (CoreText would lay
                // a raw tab out on its own 28px stops, off the char grid the
                // caret uses), so the highlight runs move onto the expanded text.
                let display = editor_view::DisplayLine::new(text);
                let highlights = cells
                    .into_iter()
                    .map(|(r, s)| (display.map_range(r), cell_to_style(&s, &theme)))
                    .collect::<Vec<_>>();
                let styled =
                    gpui::StyledText::new(display.text.clone()).with_highlights(highlights);

                // Merged end-of-line annotation (§4.5).
                let size = tab.sizes.get(&line_no).map(String::as_str);
                let exec = this.last_executed.get(&(line_no as u32)).and_then(|&c| {
                    let prev = this
                        .prev_executed
                        .get(&(line_no as u32))
                        .copied()
                        .unwrap_or(0);
                    decorations::exec_annotations::annotate_line(c, prev)
                });
                let rt: Option<RuntimeAlloc> = runtime.get(&(line_no as u32)).copied();
                let annotation = decorations::merge_annotation(size, exec.as_deref(), rt.as_ref());

                // Flow glyph + whole-line tint (§4.8).
                let mut row_bg: Option<Rgba> = None;
                let mut border_col = rgba_a(0, 0.0);
                let mut glyph: Option<(char, Rgba)> = None;
                let mut nav_target: Option<usize> = None;

                if flow_visible {
                    let executed = has_run && this.last_executed.contains_key(&(line_no as u32));
                    if let Some(fg) = tab.flow.glyphs.get(&line_no) {
                        nav_target = fg.targets.first().copied();
                        let color = flow_color(fg.kind, &theme);
                        let (ba, bo) = flow_tint(fg.kind);
                        let mut gcolor = rgba_a(color, 1.0);
                        if has_run {
                            if executed {
                                row_bg = Some(rgba_a(theme.accent, 0.08));
                                border_col = rgba_a(color, bo);
                            } else {
                                gcolor = rgba_a(color, 0.25);
                            }
                        } else {
                            row_bg = Some(rgba_a(color, ba));
                            border_col = rgba_a(color, bo);
                        }
                        glyph = Some((fg.kind.glyph(), gcolor));
                    } else if executed {
                        row_bg = Some(rgba_a(theme.accent, 0.04));
                    }
                    if error_line == Some(line_no as u32) {
                        row_bg = Some(rgba_a(theme.red, 0.12));
                        border_col = rgba_a(theme.red, 0.5);
                        glyph = Some((GlyphKind::Error.glyph(), rgba_a(theme.red, 1.0)));
                        nav_target = None;
                    }
                }

                // Gutter diagnostic dot (max severity starting on this row).
                let diag_dot = diag_dot_color(&tab.diagnostics, i, &theme);
                // Breakpoint on this line (§4.6) — takes visual precedence:
                // the whole row gets a red wash + left border.
                let has_bp = this.is_breakpoint(line_no as u32);
                if has_bp {
                    row_bg = Some(rgba_a(theme.red, 0.10));
                    border_col = rgba_a(theme.red, 0.45);
                }
                // Execution pointer (§5.8): the line the debugger is paused on
                // gets an amber wash + border and a gutter arrow, over any
                // breakpoint tint (you want to see where you ARE first).
                let is_exec = exec_line == Some(line_no as u32);
                if is_exec {
                    row_bg = Some(rgba_a(theme.amber, 0.14));
                    border_col = rgba_a(theme.amber, 0.7);
                }

                // Row width comes from the fixed gutter/fold columns plus the
                // `code_w`-wide code cell below (uniform across rows), so every
                // line band is clickable past EOL AND long lines extend the row so
                // the `Unconstrained` list can scroll horizontally. (No `.w_full()`
                // — that would re-clamp rows to the viewport and kill h-scroll.)
                let mut row = div().flex().flex_row().h(px(LINE_H)).items_center();

                if flow_visible {
                    let mut cell = div()
                        .id(("flow-glyph", i))
                        .w(px(GLYPH_W))
                        .flex_none()
                        .flex()
                        .items_center()
                        .justify_center()
                        .text_size(px(12.));
                    if let Some((ch, col)) = glyph {
                        cell = cell.text_color(col).child(ch.to_string());
                    }
                    if let Some(target) = nav_target {
                        cell = cell.cursor_pointer().on_click(cx.listener(
                            move |app: &mut JadeApp, ev: &ClickEvent, _win, cx| {
                                let m = ev.modifiers();
                                if m.platform || m.control {
                                    app.flow_goto(target);
                                    cx.notify();
                                }
                            },
                        ));
                    }
                    row = row.child(cell);
                }

                // (Structure-based whole-block row tints removed — deprecated.)
                row = row.border_l_2().border_color(border_col);
                if let Some(bg) = row_bg {
                    row = row.bg(bg);
                }

                // Gutter: diagnostic dot + right-aligned line number.
                let mut gutter = div()
                    .w(px(GUTTER_W))
                    .flex_none()
                    .flex()
                    .flex_row()
                    .items_center()
                    .pr(px(8.));
                // The 8px glyph margin: click toggles a breakpoint (§4.6); it
                // shows a red breakpoint dot (precedence) or the diagnostic dot.
                let dot: gpui::AnyElement = if is_exec {
                    // Where the paused program is: an amber ▶ in the margin.
                    div()
                        .text_size(px(9.))
                        .text_color(rgb(theme.amber))
                        .child("▶")
                        .into_any_element()
                } else if has_bp {
                    div()
                        .w(px(6.))
                        .h(px(6.))
                        .rounded_full()
                        .bg(rgb(theme.red))
                        .into_any_element()
                } else {
                    match diag_dot {
                        Some(c) => div()
                            .w(px(5.))
                            .h(px(5.))
                            .rounded_full()
                            .bg(rgb(c))
                            .into_any_element(),
                        None => div().into_any_element(),
                    }
                };
                gutter = gutter.child(
                    div()
                        .id(("bp-margin", i))
                        .w(px(8.))
                        .ml(px(5.)) // breathing room off the row's left border
                        .flex_none()
                        .flex()
                        .items_center()
                        .justify_center()
                        .cursor_pointer()
                        .on_mouse_down(
                            MouseButton::Left,
                            cx.listener(move |app: &mut JadeApp, _ev: &MouseDownEvent, _w, cx| {
                                app.toggle_breakpoint(line_no as u32);
                                app.save_ui_state();
                                cx.notify();
                            }),
                        )
                        .child(dot),
                );
                // Line-number gutter: click toggles a breakpoint on this line
                // (§4.6 — same action as the dot margin, bigger target).
                gutter = gutter.child(
                    div()
                        .id(("gutter-line", i))
                        .flex_1()
                        .text_right()
                        .text_color(rgb(theme.muted))
                        .cursor_pointer()
                        .on_mouse_down(
                            MouseButton::Left,
                            cx.listener(move |app: &mut JadeApp, _ev: &MouseDownEvent, _w, cx| {
                                app.toggle_breakpoint(line_no as u32);
                                app.save_ui_state();
                                cx.notify();
                            }),
                        )
                        .child(line_no.to_string()),
                );
                row = row.child(gutter);

                // Fold chevron (§ hierarchical collapse): down = foldable +
                // open, right = folded; click toggles. Empty for non-blocks.
                let foldable = tab.fold_map.contains_key(&i);
                let folded = foldable && tab.folds.contains(&i);
                let mut fold_cell = div()
                    .id(("fold", i))
                    .w(px(FOLD_W))
                    .flex_none()
                    .flex()
                    .items_center()
                    .justify_center();
                if foldable {
                    let chev = if folded {
                        "chevron-right"
                    } else {
                        "chevron-down"
                    };
                    let ccol = if folded { theme.accent } else { theme.muted };
                    fold_cell = fold_cell
                        .cursor_pointer()
                        .child(crate::assets::ui_icon(chev, 11., ccol))
                        .on_mouse_down(
                            MouseButton::Left,
                            cx.listener(move |app: &mut JadeApp, _ev: &MouseDownEvent, _w, cx| {
                                app.toggle_fold(i);
                                cx.notify();
                            }),
                        );
                }
                row = row.child(fold_cell);

                // Code cell: relative so the caret bar can be absolutely placed.
                // Stateful (`.id`) from the start so the mouse handlers below keep
                // the element type consistent.
                // `min_w(code_w)` (not `flex_1`) + no `overflow_hidden`: the cell is
                // at least the longest-line width so short lines stay clickable to
                // the right, and long lines / EOL annotations extend it past the
                // viewport for horizontal scrolling instead of being clipped.
                let mut cell = div()
                    .id(("code-cell", i))
                    .debug_selector(|| format!("code-cell-{i}"))
                    .flex_none()
                    .min_w(px(code_w))
                    .relative()
                    .flex()
                    .flex_row()
                    .items_center();

                // Caret bar (2px accent) on the caret row — blinks at the
                // app-driven 530ms phase while focused; steady dim marker when
                // the editor doesn't own keyboard focus (so it stays findable).
                if caret_point.row == i && (!focused || this.caret_blink_show) {
                    cell = cell.child(
                        div()
                            .absolute()
                            .top_0()
                            .left(px(editor_view::col_to_px(
                                display.display_col(caret_point.col),
                                char_w,
                            )))
                            .w(px(2.))
                            .h(px(LINE_H))
                            .bg(rgba_a(theme.accent, if focused { 1.0 } else { 0.35 }))
                            .debug_selector(|| "editor-caret".into()),
                    );
                }

                cell = cell.child(
                    div()
                        .whitespace_nowrap()
                        .flex_none()
                        .text_color(rgb(default_color))
                        .child(styled),
                );
                if folded {
                    cell = cell.child(
                        div()
                            .flex_none()
                            .px_1()
                            .text_color(rgba_a(theme.muted, 0.7))
                            .child("…"),
                    );
                }
                // Ghost text run (§4.11): faded gray at the caret row, appended
                // right after the line text (before the EOL annotation). 65%
                // muted ≈ the CLion/VS Code inline-hint contrast (~3.5:1); the
                // old 40% landed near 1.7:1 — invisible with smoothing off.
                if focused {
                    if let Some((grow, gtext)) = &ghost_render {
                        if *grow == i {
                            cell = cell.child(
                                div()
                                    .flex_none()
                                    .whitespace_nowrap()
                                    .text_color(rgba_a(theme.muted, 0.65))
                                    .debug_selector(|| "ghost-run".into())
                                    .child(gtext.clone()),
                            );
                        }
                    }
                }
                if let Some(ann) = annotation {
                    cell = cell.child(
                        div()
                            .flex_none()
                            .whitespace_nowrap()
                            .text_size(px(ANN_PX))
                            .text_color(rgba_a(theme.accent, 0.7))
                            .child(ann),
                    );
                }

                // Mouse: click sets caret / word / line; drag selects; ⌘-click →
                // definition; plain move → hover dwell.
                let mh = row_handle.clone();
                cell = cell
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(move |app: &mut JadeApp, ev: &MouseDownEvent, window, cx| {
                            if let Some(h) = &mh {
                                window.focus(h, cx);
                            }
                            let x = f32::from(ev.position.x);
                            if ev.modifiers.platform || ev.modifiers.control {
                                app.editor_goto_definition(i, x);
                            } else {
                                app.editor_mouse_down(i, x, ev.modifiers.shift, ev.click_count);
                            }
                            cx.notify();
                        }),
                    )
                    .on_mouse_move(cx.listener(
                        move |app: &mut JadeApp, ev: &MouseMoveEvent, _w, cx| {
                            let x = f32::from(ev.position.x);
                            if ev.pressed_button == Some(MouseButton::Left) {
                                app.editor_mouse_drag(i, x);
                                cx.notify();
                            } else {
                                app.editor_hover_move(i, x);
                            }
                        },
                    ));

                row = row.child(cell);
                rows.push(row);
            }
            rows
        }),
    )
    .track_scroll(&app.code_scroll)
    // Let rows exceed the viewport width so long lines scroll horizontally
    // (trackpad / shift-wheel), instead of being clipped at the right edge.
    .with_horizontal_sizing_behavior(ListHorizontalSizingBehavior::Unconstrained)
    .flex_1()
    .size_full()
    .pt(px(PAD_TOP))
    .px(px(8.))
    .text_size(px(FONT_PX))
    .line_height(px(LINE_H))
    .font_family(crate::fonts::mono_family());

    let entity = cx.entity();
    let mut container = div()
        .relative()
        .flex()
        .flex_1()
        .size_full()
        .track_focus(&handle)
        .on_key_down(cx.listener(|app: &mut JadeApp, ev: &KeyDownEvent, _w, cx| {
            if app.editor_key(&ev.keystroke, cx) {
                cx.stop_propagation();
                cx.notify();
            }
        }))
        .on_mouse_up(
            MouseButton::Left,
            cx.listener(|app: &mut JadeApp, _ev, _w, cx| {
                app.editor_mouse_up();
                cx.notify();
            }),
        )
        .child(list)
        .child(ime_geometry_canvas(
            flow_visible_now,
            app.editor_text_left.clone(),
            app.editor_rows.clone(),
            app.editor_char_w.clone(),
            (app.editor_w.clone(), app.editor_h.clone()),
            handle.clone(),
            entity,
        ));

    // Find / replace bar (⌘F / Ctrl+F): anchored top-right of the editor, over
    // the code. Its own focus handle captures keystrokes (see `app.find_key`).
    if app.find.is_some() {
        let focus = app.find_focus.clone().unwrap_or_else(|| cx.focus_handle());
        container = container.child(find_bar(app, focus, cx));
    }

    // Sync-suggestion banner (§4.13): floats bottom-center over the code.
    if app.sync_suggestion.is_some() {
        container = container.child(sync_banner(app, cx));
    }

    let scroll_top = app.editor_scroll_top();
    if let Some(popup) = completion_popup(app, flow_visible_now, scroll_top, cx) {
        container = container.child(popup);
    }
    if let Some(popup) = hover_popup(app, flow_visible_now, scroll_top) {
        container = container.child(popup);
    }
    if let Some(popup) = signature_popup(app, flow_visible_now, scroll_top) {
        container = container.child(popup);
    }
    // The Explain card (§4.14) paints above the other popups: it is the one
    // the user summoned deliberately, so it wins any overlap.
    if let Some(card) = super::explain_card::render(app, flow_visible_now, scroll_top, cx) {
        container = container.child(card);
    }
    // The Visualize card (§4.15) paints last: opened last wins any overlap
    // with an Explain card, and both stay usable.
    if let Some(card) = super::visualize_card::render(app, flow_visible_now, scroll_top, cx) {
        container = container.child(card);
    }
    container.into_any_element()
}

/// A background split pane: the active tab of pane `idx` as a read-only,
/// syntax-colored line list (no caret, selection, or popups). Its own scroll
/// handle lives on the pane. A click gives the pane the keyboard and puts the
/// caret on the clicked row.
pub fn render_background(
    app: &JadeApp,
    idx: usize,
    cx: &mut Context<JadeApp>,
) -> gpui::AnyElement {
    let theme = app.theme.clone();
    let focus_click = cx.listener(move |app: &mut JadeApp, _ev: &MouseDownEvent, _w, cx| {
        app.focus_pane(idx);
        cx.notify();
    });
    let Some(pane) = app.panes.get(idx) else {
        return div().into_any_element();
    };
    let Some(tab) = pane.active_tab() else {
        return div()
            .id(("pane-bg", idx))
            .flex()
            .flex_1()
            .size_full()
            .items_center()
            .justify_center()
            .on_mouse_down(MouseButton::Left, focus_click)
            .child(
                div()
                    .text_color(rgb(theme.muted))
                    .child("No file open — pick one from the tree"),
            )
            .into_any_element();
    };

    let visible_count = tab.visible_rows().len();
    let default_color = theme.text;
    let list = uniform_list(
        ("code-lines-bg", idx),
        visible_count,
        cx.processor(move |this, range: std::ops::Range<usize>, _window, cx| {
            let mut rows = Vec::with_capacity(range.len());
            let theme = this.theme.clone();
            let char_w = this.char_w();
            let Some(tab) = this.panes.get(idx).and_then(|p| p.active_tab()) else {
                return rows;
            };
            let code_w = tab.max_cols as f32 * char_w + 48.0;
            let visible = tab.visible_rows();
            for di in range {
                let Some(&i) = visible.get(di) else { break };
                let text = tab.line(i);
                let syntax = tab.highlights.get(i).map(Vec::as_slice).unwrap_or(&[]);
                let cells =
                    editor_view::merge_line_styles(text.len(), syntax, None, &[], None, &[]);
                let display = editor_view::DisplayLine::new(text);
                let highlights = cells
                    .into_iter()
                    .map(|(r, s)| (display.map_range(r), cell_to_style(&s, &theme)))
                    .collect::<Vec<_>>();
                let styled =
                    gpui::StyledText::new(display.text.clone()).with_highlights(highlights);
                let folded = tab.fold_map.contains_key(&i) && tab.folds.contains(&i);

                let mut cell = div()
                    .id(("bg-code-cell", i))
                    .flex_none()
                    .min_w(px(code_w))
                    .flex()
                    .flex_row()
                    .items_center()
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(move |app: &mut JadeApp, _ev: &MouseDownEvent, _w, cx| {
                            app.focus_pane(idx);
                            if let Some(tab) = app.editor.active_tab_mut() {
                                let row = i.min(tab.line_count().saturating_sub(1));
                                let off = tab.buffer.point_to_offset(Point::new(row, 0));
                                tab.buffer.set_caret(off);
                            }
                            cx.notify();
                        }),
                    )
                    .child(
                        div()
                            .whitespace_nowrap()
                            .flex_none()
                            .text_color(rgb(default_color))
                            .child(styled),
                    );
                if folded {
                    cell = cell.child(
                        div()
                            .flex_none()
                            .px_1()
                            .text_color(rgba_a(theme.muted, 0.7))
                            .child("…"),
                    );
                }
                let row = div()
                    .flex()
                    .flex_row()
                    .h(px(LINE_H))
                    .items_center()
                    .border_l_2()
                    .border_color(rgba_a(0, 0.0))
                    .child(
                        div()
                            .w(px(GUTTER_W))
                            .flex_none()
                            .pr(px(8.))
                            .text_right()
                            .text_color(rgb(theme.muted))
                            .child((i + 1).to_string()),
                    )
                    .child(div().w(px(FOLD_W)).flex_none())
                    .child(cell);
                rows.push(row);
            }
            rows
        }),
    )
    .track_scroll(&pane.scroll)
    .with_horizontal_sizing_behavior(ListHorizontalSizingBehavior::Unconstrained)
    .flex_1()
    .size_full()
    .pt(px(PAD_TOP))
    .px(px(8.))
    .text_size(px(FONT_PX))
    .line_height(px(LINE_H))
    .font_family(crate::fonts::mono_family());

    div()
        .id(("pane-bg", idx))
        .relative()
        .flex()
        .flex_1()
        .size_full()
        .on_mouse_down(MouseButton::Left, focus_click)
        .child(list)
        .into_any_element()
}

/// Drop zones over pane `idx`'s body, shown only while a tab drag is live:
/// the left part moves the tab into this pane, the right part opens a new
/// pane to the right with it. Returns `None` when nothing is dragged.
pub fn drop_zones(
    app: &JadeApp,
    idx: usize,
    cx: &mut Context<JadeApp>,
    theme: &Theme,
) -> Option<gpui::AnyElement> {
    if !cx.has_active_drag() {
        return None;
    }
    let t = &theme.kumo;
    let tint = t.tint;
    let brand = t.brand;
    let zone = |id: &'static str, label: &'static str| {
        div()
            .id((id, idx))
            .flex()
            .items_center()
            .justify_center()
            .h_full()
            .text_size(scale::TEXT_XS)
            .text_color(t.text_subtle)
            .drag_over::<TabDrag>(move |st, _, _, _| {
                st.bg(tint).border_1().border_color(brand)
            })
            .child(label)
    };
    let mut zones = div()
        .absolute()
        .top(px(32.)) // below the tab strip
        .left_0()
        .right_0()
        .bottom_0()
        .flex()
        .flex_row();
    if app.split_mode() {
        zones = zones.child(
            zone("pane-drop-move", "Move here")
                .flex_1()
                .on_drop(cx.listener(move |app, d: &TabDrag, _win, cx| {
                    app.move_tab(d.pane, d.index, idx, None);
                    app.schedule_ui_save(cx);
                    cx.notify();
                })),
        );
    } else {
        zones = zones.child(div().flex_1());
    }
    if app.pane_count() < crate::panes::MAX_PANES {
        zones = zones.child(
            zone("pane-drop-split", "Split right")
                .w(gpui::relative(0.35))
                .on_drop(cx.listener(move |app, d: &TabDrag, _win, cx| {
                    app.split_with_tab(d.pane, d.index, idx);
                    app.schedule_ui_save(cx);
                    cx.notify();
                })),
        );
    }
    Some(zones.into_any_element())
}

/// Wrap `body` with the editor's keyboard plumbing: the focus handle, the
/// key path ([`JadeApp::editor_key`]), and the IME input handler. The
/// focused pane's formatted Markdown view uses it so preview edits still
/// type into the buffer while the code list is off screen.
pub fn focus_shell(
    app: &JadeApp,
    cx: &mut Context<JadeApp>,
    body: gpui::AnyElement,
) -> gpui::AnyElement {
    let handle = app
        .editor_focus
        .clone()
        .unwrap_or_else(|| cx.focus_handle());
    let entity = cx.entity();
    div()
        .relative()
        .flex()
        .flex_1()
        .size_full()
        .min_h(px(0.))
        .track_focus(&handle)
        .on_key_down(cx.listener(|app: &mut JadeApp, ev: &KeyDownEvent, _w, cx| {
            if app.editor_key(&ev.keystroke, cx) {
                cx.stop_propagation();
                cx.notify();
            }
        }))
        .child(body)
        .child(ime_geometry_canvas(
            app.flow_visible,
            app.editor_text_left.clone(),
            app.editor_rows.clone(),
            app.editor_char_w.clone(),
            (app.editor_w.clone(), app.editor_h.clone()),
            handle,
            entity,
        ))
        .into_any_element()
}

/// True when a keystroke should type a character into a find field: a printable
/// `key_char` with no command/control/alt/function modifier.
fn find_is_printable(ev: &KeyDownEvent) -> bool {
    let m = &ev.keystroke.modifiers;
    ev.keystroke.key_char.is_some() && !m.platform && !m.control && !m.alt && !m.function
}

/// A small pill button for the find bar (icon/label + hover + click).
fn find_btn(
    id: &'static str,
    label: impl Into<String>,
    active: bool,
    theme: &Theme,
    cx: &mut Context<JadeApp>,
    on_click: impl Fn(&mut JadeApp, &mut Context<JadeApp>) + 'static,
) -> gpui::Stateful<gpui::Div> {
    // Kumo Button geometry at `size="sm"` (`h-6.5 rounded-md px-2 text-xs`),
    // ghost at rest and brand-tinted while its toggle is on.
    let t = &theme.kumo;
    let hover_bg = t.tint;
    let mut b = div()
        .id(id)
        .flex()
        .items_center()
        .justify_center()
        .h(scale::H_6_5)
        .min_w(px(24.))
        .px(scale::SPACE_2)
        .text_size(scale::TEXT_XS)
        .font_weight(gpui::FontWeight::MEDIUM)
        .cursor_pointer()
        .text_color(if active { t.brand } else { t.text_subtle })
        .hover(move |st| st.bg(hover_bg))
        .on_mouse_down(
            MouseButton::Left,
            cx.listener(move |app: &mut JadeApp, _ev: &MouseDownEvent, _w, cx| {
                on_click(app, cx);
                cx.notify();
            }),
        )
        .child(label.into());
    if active {
        b = b.bg(t.success_tint);
    }
    b
}

/// One clickable text field of the find bar: the captured query/replace buffer,
/// with a blinking caret at `cursor` when it's the focused field. Clicking it
/// makes it the active field, puts the caret at the clicked char column
/// (mapped through the same measured-advance scheme as editor clicks), and
/// pulls keyboard focus back to the bar.
fn find_field(
    id: &'static str,
    field: crate::find::FindField,
    value: &str,
    placeholder: &str,
    focused: bool,
    caret_on: bool,
    cursor: usize,
    app: &JadeApp,
    theme: &Theme,
    cx: &mut Context<JadeApp>,
) -> gpui::Stateful<gpui::Div> {
    use std::sync::atomic::Ordering;
    // Shared fake-input line: caret before the placeholder when empty, at the
    // cursor while typing; blinks via `caret_on` (530ms editor phase).
    let inner = crate::panels::input_line(
        value,
        placeholder,
        focused,
        caret_on,
        theme.accent,
        theme,
        cursor,
    )
    .text_xs();

    // Zero-width canvas as the FIRST flex child: its painted origin is exactly
    // where the text starts (past padding), and the inherited text style gives
    // the bar's real font/size — both captured for click→column mapping.
    let text_left = app.find_field_left[field as usize].clone();
    let char_w_store = app.find_char_w.clone();
    let geom = canvas(
        move |_bounds, _window, _cx| {},
        move |bounds: Bounds<gpui::Pixels>, _, window, _cx| {
            text_left.store(f32::from(bounds.origin.x).to_bits(), Ordering::Relaxed);
            let style = window.text_style();
            let font_id = window.text_system().resolve_font(&style.font());
            let size = style.font_size.to_pixels(window.rem_size());
            if let Ok(adv) = window.text_system().advance(font_id, size, 'M') {
                let w = f32::from(adv.width);
                if w > 0.0 {
                    char_w_store.store(w.to_bits(), Ordering::Relaxed);
                }
            }
        },
    )
    .w(px(0.))
    .h(px(16.))
    .flex_none();

    let text_left = app.find_field_left[field as usize].clone();
    let char_w_store = app.find_char_w.clone();
    let max_col = value.chars().count();
    div()
        .id(id)
        .debug_selector(move || id.to_string())
        .flex()
        .flex_row()
        .items_center()
        .h(px(22.))
        .min_w(px(200.))
        .px(px(6.))
        .bg(rgb(theme.bg))
        .border_1()
        .border_color(rgb(if focused { theme.accent } else { theme.border }))
        .cursor_text()
        .on_mouse_down(
            MouseButton::Left,
            cx.listener(move |app: &mut JadeApp, ev: &MouseDownEvent, _w, cx| {
                let left = f32::from_bits(text_left.load(Ordering::Relaxed));
                let cw = f32::from_bits(char_w_store.load(Ordering::Relaxed));
                let col = editor_view::px_to_col(f32::from(ev.position.x) - left, cw, max_col);
                app.find_click_field(field, col);
                cx.notify();
            }),
        )
        .child(geom)
        .child(inner)
}

/// The cross-file sync banner (§4.13): the pending suggestion's message plus
/// an apply pill (⌘⏎) and a dismiss pill (Esc). Floats near the bottom of the
/// editor so it never hides the caret line.
fn sync_banner(app: &JadeApp, cx: &mut Context<JadeApp>) -> gpui::AnyElement {
    let theme = app.theme.clone();
    let Some(sug) = app.sync_suggestion.as_ref() else {
        return div().into_any_element();
    };
    div()
        .id("sync-banner")
        .debug_selector(|| "sync-banner".to_string())
        .absolute()
        .bottom(px(14.))
        .left(px(24.))
        .flex()
        .flex_row()
        .items_center()
        .gap(px(8.))
        .h(px(28.))
        .px(px(10.))
        .bg(rgb(theme.panel))
        .border_1()
        .border_color(rgba_a(theme.accent, 0.6))
        .shadow_md()
        .text_xs()
        .text_color(rgb(theme.text))
        .child(sug.message())
        .when(
            !matches!(sug, crate::sync::SyncSuggestion::SimilarLines { .. })
                && app.sync_scope_is_narrow(),
            |el| {
                let dir = app
                    .sync_scope
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_default();
                el.child(
                    div()
                        .text_color(rgb(theme.muted))
                        .child(format!("in {dir}/")),
                )
            },
        )
        .child(find_btn(
            "sync-apply",
            "apply ⌘⏎",
            true,
            &theme,
            cx,
            |app, cx| {
                app.sync_apply(cx);
            },
        ))
        .child(find_btn(
            "sync-dismiss",
            "esc",
            false,
            &theme,
            cx,
            |app, _cx| {
                app.sync_suggestion = None;
            },
        ))
        .into_any_element()
}

/// The find / replace bar. A captured-keystroke buffer (like Quick Open): the
/// panel takes focus and forwards keystrokes to `JadeApp::find_key`. Anchored
/// top-right so it never hides the caret line.
fn find_bar(app: &JadeApp, focus: FocusHandle, cx: &mut Context<JadeApp>) -> gpui::AnyElement {
    use crate::find::FindField;
    let theme = app.theme.clone();
    let Some(state) = app.find.as_ref() else {
        return div().into_any_element();
    };
    let replace_visible = state.replace_visible;
    let find_focused = state.field == FindField::Find;
    // The caret blinks only while the bar owns focus (so it goes dim when you
    // click into the editor to keep editing) — driven by the shared 530ms phase.
    let blink = app.caret_blink_show && app.find_bar_focused;
    let count = state.count();
    let count_label = if state.query.is_empty() {
        String::new()
    } else if count == 0 {
        "No results".to_string()
    } else {
        format!("{} of {}", state.ordinal(), count)
    };

    // Left chevron: the dropdown toggle that shows / hides the replace row.
    let chevron = if replace_visible {
        "chevron-down"
    } else {
        "chevron-right"
    };
    let chevron_btn = div()
        .id("find-chevron")
        .flex()
        .items_center()
        .justify_center()
        .w(px(16.))
        .h(scale::H_6_5)
        .flex_none()
        .cursor_pointer()
        .on_mouse_down(
            MouseButton::Left,
            cx.listener(|app: &mut JadeApp, _ev: &MouseDownEvent, _w, cx| {
                if let Some(s) = app.find.as_mut() {
                    if s.replace_visible {
                        s.replace_visible = false;
                        s.field = FindField::Find;
                    } else {
                        s.replace_visible = true;
                    }
                }
                app.pending_find_focus = true;
                cx.notify();
            }),
        )
        .child(crate::kumo::icon(chevron, 12., theme.kumo.text_subtle));

    // Find row: [chevron] [ query field ] [k of N] [Aa] [‹] [›] [×]
    let find_row = div()
        .flex()
        .flex_row()
        .items_center()
        .gap(px(4.))
        .child(chevron_btn)
        .child(find_field(
            "find-query-field",
            FindField::Find,
            &state.query,
            "Find",
            find_focused,
            blink,
            if find_focused {
                state.cursor
            } else {
                state.query.len()
            },
            app,
            &theme,
            cx,
        ))
        .child(
            div()
                .min_w(px(62.))
                .font_family(crate::fonts::mono_family()) // the count must not reflow the row
                .text_size(scale::TEXT_XS)
                .text_color(theme.kumo.text_subtle)
                .child(count_label),
        )
        .child(find_btn(
            "find-case",
            "Aa",
            state.case_sensitive,
            &theme,
            cx,
            |app, _cx| {
                app.find_toggle_case();
            },
        ))
        .child(find_btn(
            "find-prev",
            "‹",
            false,
            &theme,
            cx,
            |app, _cx| {
                app.find_prev();
            },
        ))
        .child(find_btn(
            "find-next",
            "›",
            false,
            &theme,
            cx,
            |app, _cx| {
                app.find_next();
            },
        ))
        .child(find_btn(
            "find-close",
            "×",
            false,
            &theme,
            cx,
            |app, _cx| {
                app.close_find();
            },
        ));

    let mut bar = div().flex().flex_col().gap(px(4.)).child(find_row);

    // Replace row (the dropdown): [ replace field ] [Replace] [All]. Indented to
    // line up under the query field, past the chevron column.
    if replace_visible {
        let replace_row = div()
            .flex()
            .flex_row()
            .items_center()
            .gap(px(4.))
            .child(div().w(px(16.)).flex_none()) // align under the query field
            .child(find_field(
                "find-replace-field",
                FindField::Replace,
                &state.replace,
                "Replace",
                !find_focused,
                blink,
                if find_focused {
                    state.replace.len()
                } else {
                    state.cursor
                },
                app,
                &theme,
                cx,
            ))
            .child(find_btn(
                "find-replace-one",
                "Replace",
                false,
                &theme,
                cx,
                |app, cx| {
                    app.find_replace_current(cx);
                },
            ))
            .child(find_btn(
                "find-replace-all",
                "All",
                false,
                &theme,
                cx,
                |app, cx| {
                    app.find_replace_all(cx);
                },
            ));
        bar = bar.child(replace_row);
    }

    // A Kumo overlay surface floated over the editor: `bg-kumo-overlay`,
    // `rounded-lg`, the standard ring, and a drop shadow.
    crate::kumo::Surface::overlay(&theme.kumo)
        .absolute()
        .top(scale::SPACE_2)
        .right(scale::SPACE_4)
        .p(scale::SPACE_1_5)
        // Absorb mouse events so a click on the bar never falls through to the
        // editor rows painted underneath (which would move the caret / focus).
        .occlude()
        .track_focus(&focus)
        .on_key_down(cx.listener(|app: &mut JadeApp, ev: &KeyDownEvent, _w, cx| {
            let ks = &ev.keystroke;
            app.find_key(
                ks.key.as_str(),
                ks.key_char.clone(),
                find_is_printable(ev),
                ks.modifiers.shift,
                ks.modifiers.platform,
                cx,
            );
            // Consume every key so it doesn't bubble to the editor / global
            // shortcuts while the bar owns input.
            cx.stop_propagation();
            cx.notify();
        }))
        .child(bar)
        .into_any_element()
}

/// Convert a flattened [`CellStyle`] into a gpui [`HighlightStyle`]: syntax color,
/// a 15%-alpha accent selection wash (§4.2), and a 1px diagnostic/IME underline
/// (a flat line — wavy squiggles are deferred; documented).
fn cell_to_style(cell: &CellStyle, theme: &Theme) -> HighlightStyle {
    let underline = cell
        .underline
        .map(|c| UnderlineStyle {
            thickness: px(1.0),
            color: Some(rgb(c).into()),
            wavy: false,
        })
        .or_else(|| {
            cell.marked.then(|| UnderlineStyle {
                thickness: px(1.0),
                color: Some(rgb(theme.text).into()),
                wavy: false,
            })
        });
    // Background wash precedence: the *current* find match (both selected and a
    // match) reads strongest, other matches get an amber wash so every occurrence
    // is visible, and a plain text selection keeps its subtle 15% accent.
    let background_color = if cell.selected && cell.find_match {
        Some(gpui::Hsla::from(rgba_a(theme.accent, 0.34)))
    } else if cell.find_match {
        Some(gpui::Hsla::from(rgba_a(theme.amber, 0.30)))
    } else if cell.selected {
        Some(gpui::Hsla::from(rgba_a(theme.accent, 0.15)))
    } else {
        None
    };
    HighlightStyle {
        color: cell.color.map(|c| rgb(c).into()),
        background_color,
        underline,
        ..Default::default()
    }
}

/// The gutter dot color for the highest-severity diagnostic **starting** on row
/// `row` (0-based), or `None`.
fn diag_dot_color(diagnostics: &[jade_lsp::Diagnostic], row: usize, theme: &Theme) -> Option<u32> {
    use jade_lsp::DiagnosticSeverity as S;
    // Rank: lower is more severe (Error most). Track the most-severe on the row.
    let rank = |s: S| match s {
        S::ERROR => 0,
        S::WARNING => 1,
        S::INFORMATION => 2,
        _ => 3,
    };
    let mut best: Option<S> = None;
    for d in diagnostics {
        if d.range.start.line as usize == row {
            let sev = d.severity.unwrap_or(S::INFORMATION);
            best = Some(match best {
                Some(b) if rank(b) <= rank(sev) => b,
                _ => sev,
            });
        }
    }
    best.map(|s| editor_view::severity_color(Some(s), theme.red, theme.amber, theme.periwinkle))
}

/// The IME/geometry underlay: captures the code-text left edge + visible row
/// count for mouse mapping and follow-scroll, and registers the IME input
/// handler for the editor focus each frame (`window.handle_input`).
fn ime_geometry_canvas(
    flow_visible: bool,
    text_left: std::sync::Arc<std::sync::atomic::AtomicU32>,
    rows_store: std::sync::Arc<std::sync::atomic::AtomicU32>,
    char_w_store: std::sync::Arc<std::sync::atomic::AtomicU32>,
    size_store: (
        std::sync::Arc<std::sync::atomic::AtomicU32>,
        std::sync::Arc<std::sync::atomic::AtomicU32>,
    ),
    handle: FocusHandle,
    entity: Entity<JadeApp>,
) -> impl IntoElement {
    use std::sync::atomic::Ordering;
    canvas(
        move |_bounds, _window, _cx| {},
        move |bounds: Bounds<gpui::Pixels>, _, window, cx| {
            let glyph = if flow_visible { GLYPH_W } else { 0.0 };
            let left = f32::from(bounds.origin.x) + 8.0 + GUTTER_W + FOLD_W + glyph;
            text_left.store(left.to_bits(), Ordering::Relaxed);
            // Measure the mono font's real advance so caret placement and
            // click→column mapping stay correct if the bundled font changes.
            let font = gpui::font(crate::fonts::mono_family());
            let font_id = window.text_system().resolve_font(&font);
            if let Ok(adv) = window.text_system().advance(font_id, px(FONT_PX), 'M') {
                let w = f32::from(adv.width);
                if w > 0.0 {
                    char_w_store.store(w.to_bits(), Ordering::Relaxed);
                }
            }
            let h = f32::from(bounds.size.height);
            rows_store.store(((h / LINE_H).floor() as u32).max(1), Ordering::Relaxed);
            // The editor's own box, so a floating card can clamp itself to it
            // (`panels::explain_card::clamp_card`). Same capture-per-frame
            // trick as `text_left` above — layout is only known at paint.
            size_store
                .0
                .store(f32::from(bounds.size.width).to_bits(), Ordering::Relaxed);
            size_store.1.store(h.to_bits(), Ordering::Relaxed);
            let region = Bounds::new(
                point(px(left), bounds.origin.y + px(PAD_TOP)),
                size(bounds.size.width, bounds.size.height),
            );
            window.handle_input(
                &handle,
                ElementInputHandler::new(region, entity.clone()),
                cx,
            );
        },
    )
    .absolute()
    .top_0()
    .left_0()
    .size_full()
}

/// The x pixel of the code column `col`, including the gutter + optional flow
/// margin, for anchoring the floating popups inside the editor container. The
/// code text scrolls horizontally under the fixed gutter, so `h_scroll` (the
/// list's horizontal offset, ≤ 0) is folded in to keep popups under the glyph.
pub fn popup_x(col: usize, flow_visible: bool, char_w: f32, h_scroll: f32) -> f32 {
    let glyph = if flow_visible { GLYPH_W } else { 0.0 };
    8.0 + GUTTER_W + FOLD_W + glyph + editor_view::col_to_px(col, char_w) + h_scroll
}

/// The display column of char column `col` on buffer row `row` of the active tab
/// — what the popups anchor at, so a tab-indented row puts them under the glyph
/// (see [`editor_view::DisplayLine`]).
fn display_col(app: &JadeApp, row: usize, col: usize) -> usize {
    app.editor
        .active_tab()
        .map(|t| editor_view::DisplayLine::new(t.line(row)).display_col(col))
        .unwrap_or(col)
}

/// The y pixel of the TOP of `row` itself, given the current scroll top —
/// what a popup flipped ABOVE an anchor measures back from.
pub fn row_top(row: usize, scroll_top: usize) -> f32 {
    let rel = row.saturating_sub(scroll_top);
    PAD_TOP + rel as f32 * LINE_H
}

/// The y pixel (top) of the row below `row`, given the current scroll top.
pub fn popup_y(row: usize, scroll_top: usize) -> f32 {
    let rel = row.saturating_sub(scroll_top);
    PAD_TOP + (rel as f32 + 1.0) * LINE_H
}

/// The autocomplete popup (max ~8 visible), anchored under the caret.
fn completion_popup(
    app: &JadeApp,
    flow_visible: bool,
    scroll_top: usize,
    cx: &mut Context<JadeApp>,
) -> Option<gpui::AnyElement> {
    let c: &CompletionState = app.completion.as_ref()?;
    if c.filtered.is_empty() {
        return None;
    }
    let theme = app.theme.clone();
    let (row, col) = c.anchor;
    let col = display_col(app, row, col);
    let row = app
        .editor
        .active_tab()
        .map(|t| t.display_row(row))
        .unwrap_or(row);
    let left = popup_x(col, flow_visible, app.char_w(), app.editor_h_scroll());
    let top = popup_y(row, scroll_top);

    // Window of up to 8 items centered on the selection.
    let visible = 8usize;
    let start = c
        .selected
        .saturating_sub(visible - 1)
        .min(c.filtered.len().saturating_sub(visible));
    let mut list = div().flex().flex_col().py_1();
    for slot in start..(start + visible).min(c.filtered.len()) {
        let item_ix = c.filtered[slot];
        let item = &c.items[item_ix];
        let is_sel = slot == c.selected;
        let label = item.label.clone();
        let detail = item.detail.clone();
        let mut rowdiv = div()
            .id(("cmp", slot))
            .flex()
            .flex_row()
            .items_center()
            .justify_between()
            .h(px(20.))
            .px(px(8.))
            .gap_2()
            .text_xs()
            .cursor_pointer()
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(move |app: &mut JadeApp, _ev, _w, cx| {
                    if let Some(c) = &mut app.completion {
                        c.selected = slot;
                    }
                    app.completion_accept(cx);
                    cx.notify();
                }),
            )
            .child(div().text_color(rgb(theme.text)).child(label));
        if let Some(d) = detail {
            rowdiv = rowdiv.child(div().text_color(rgb(theme.muted)).child(d));
        }
        if is_sel {
            rowdiv = rowdiv.bg(rgb(theme.border));
        }
        list = list.child(rowdiv);
    }

    Some(
        div()
            .absolute()
            .left(px(left))
            .top(px(top))
            .min_w(px(220.))
            .max_w(px(420.))
            .max_h(px(8.0 * 20.0 + 8.0))
            .flex()
            .flex_col()
            .bg(rgb(theme.panel))
            .border_1()
            .border_color(rgb(theme.border))
            .overflow_hidden()
            .child(list)
            .into_any_element(),
    )
}

/// The signature-help hint: the active call's signature shown one line above the
/// caret, with the parameter currently being filled in emphasized (accent+bold).
fn signature_popup(
    app: &JadeApp,
    flow_visible: bool,
    scroll_top: usize,
) -> Option<gpui::AnyElement> {
    let s: &SignatureState = app.signature.as_ref()?;
    if s.label.is_empty() {
        return None;
    }
    let theme = app.theme.clone();
    let (row, col) = s.anchor;
    let col = display_col(app, row, col);
    let row = app
        .editor
        .active_tab()
        .map(|t| t.display_row(row))
        .unwrap_or(row);
    let left = popup_x(col, flow_visible, app.char_w(), app.editor_h_scroll());
    let rel = row.saturating_sub(scroll_top);
    // Sit the hint fully ABOVE the caret line: its bottom edge lands SIG_GAP px
    // above the line's top so it never overlaps the code you're typing (the box
    // is ~24px, taller than one line, so a plain "-LINE_H" left ~6px of overlap).
    // When there isn't room above (caret near the viewport top), drop it BELOW
    // the caret line instead — same fallback the completion popup uses.
    const SIG_H: f32 = 24.0;
    const SIG_GAP: f32 = 4.0;
    let caret_top = PAD_TOP + rel as f32 * LINE_H;
    let above = caret_top - SIG_H - SIG_GAP;
    let top = if above >= 2.0 {
        above
    } else {
        caret_top + LINE_H + SIG_GAP // below the caret line
    };

    // Split the label into before · active-param · after so the argument being
    // filled in stands out.
    let mut hint = div()
        .flex()
        .flex_row()
        .items_center()
        .text_xs()
        .whitespace_nowrap();
    match &s.active_param {
        Some(r) if r.end <= s.label.len() && r.start <= r.end => {
            hint = hint
                .child(
                    div()
                        .text_color(rgb(theme.muted))
                        .child(s.label[..r.start].to_string()),
                )
                .child(
                    div()
                        .text_color(rgb(theme.accent))
                        .font_weight(gpui::FontWeight::BOLD)
                        .child(s.label[r.start..r.end].to_string()),
                )
                .child(
                    div()
                        .text_color(rgb(theme.muted))
                        .child(s.label[r.end..].to_string()),
                );
        }
        _ => {
            hint = hint.child(div().text_color(rgb(theme.text)).child(s.label.clone()));
        }
    }

    Some(
        div()
            .absolute()
            .left(px(left))
            .top(px(top))
            .debug_selector(|| "signature-hint".into())
            .max_w(px(560.))
            .px(px(8.))
            .py(px(3.))
            .bg(rgb(theme.panel))
            .border_1()
            .border_color(rgb(theme.border))
            .overflow_hidden()
            .child(hint)
            .into_any_element(),
    )
}

/// The floating hover panel (plain text; markdown rendering deferred).
/// One block of a hover card. clangd sends Markdown; the card shows the few
/// forms clangd actually uses and never the raw fences or hashes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HoverBlock {
    /// A `#` heading, with the hashes and any backticks stripped.
    Heading(String),
    /// The lines inside a ``` fence, verbatim.
    Code(Vec<String>),
    /// A horizontal rule (`---`).
    Rule,
    /// A run of plain lines, joined with spaces.
    Text(String),
}

/// Split clangd's hover Markdown into [`HoverBlock`]s.
pub fn hover_blocks(text: &str) -> Vec<HoverBlock> {
    let mut out = Vec::new();
    let mut code: Option<Vec<String>> = None;
    let mut para: Vec<String> = Vec::new();
    let flush_para = |para: &mut Vec<String>, out: &mut Vec<HoverBlock>| {
        if !para.is_empty() {
            out.push(HoverBlock::Text(para.join(" ")));
            para.clear();
        }
    };
    for line in text.lines() {
        let trimmed = line.trim();
        if let Some(buf) = code.as_mut() {
            if trimmed.starts_with("```") {
                out.push(HoverBlock::Code(std::mem::take(buf)));
                code = None;
            } else {
                buf.push(line.to_string());
            }
            continue;
        }
        if trimmed.starts_with("```") {
            flush_para(&mut para, &mut out);
            code = Some(Vec::new());
        } else if trimmed == "---" || trimmed == "***" {
            flush_para(&mut para, &mut out);
            out.push(HoverBlock::Rule);
        } else if let Some(rest) = trimmed.strip_prefix('#') {
            flush_para(&mut para, &mut out);
            let title = rest.trim_start_matches('#').trim().replace('`', "");
            out.push(HoverBlock::Heading(title));
        } else if trimmed.is_empty() {
            flush_para(&mut para, &mut out);
        } else {
            para.push(trimmed.replace('`', ""));
        }
    }
    if let Some(buf) = code {
        out.push(HoverBlock::Code(buf));
    }
    flush_para(&mut para, &mut out);
    // A rule that only separates a heading from a code block adds nothing.
    out.retain(|b| *b != HoverBlock::Rule);
    out
}

fn hover_popup(app: &JadeApp, flow_visible: bool, scroll_top: usize) -> Option<gpui::AnyElement> {
    let h: &HoverState = app.hover.as_ref()?;
    let theme = app.theme.clone();
    let t = &theme.kumo;
    let col = display_col(app, h.row, h.col);
    let row = app
        .editor
        .active_tab()
        .map(|t| t.display_row(h.row))
        .unwrap_or(h.row);
    let left = popup_x(col, flow_visible, app.char_w(), app.editor_h_scroll());
    let top = popup_y(row, scroll_top);

    let mut body = div().flex().flex_col().gap(scale::SPACE_1_5);
    // Diagnostics first: a severity word in the squiggle's color, then the
    // message in the default ink.
    for (sev, message) in &h.diagnostics {
        use jade_lsp::DiagnosticSeverity as S;
        let label = match *sev {
            Some(S::ERROR) => "error",
            Some(S::WARNING) => "warning",
            Some(S::HINT) => "hint",
            _ => "note",
        };
        let color = editor_view::severity_color(*sev, theme.red, theme.amber, theme.periwinkle);
        body = body.child(
            div()
                .flex()
                .flex_row()
                .items_start()
                .gap(scale::SPACE_2)
                .text_size(scale::TEXT_XS)
                .child(
                    div()
                        .flex_none()
                        .font_weight(gpui::FontWeight::SEMIBOLD)
                        .text_color(rgb(color))
                        .child(label),
                )
                .child(
                    div()
                        .whitespace_normal()
                        .text_color(t.text_default)
                        .child(message.clone()),
                ),
        );
    }
    if !h.diagnostics.is_empty() && !h.text.trim().is_empty() {
        body = body.child(crate::kumo::separator_h(t));
    }
    for block in hover_blocks(&h.text) {
        body = match block {
            HoverBlock::Heading(s) => body.child(
                div()
                    .text_size(scale::TEXT_XS)
                    .font_weight(gpui::FontWeight::SEMIBOLD)
                    .text_color(t.text_default)
                    .child(s),
            ),
            HoverBlock::Code(lines) => {
                let mut code = div()
                    .flex()
                    .flex_col()
                    .px(scale::SPACE_2)
                    .py(scale::SPACE_1)
                    .bg(t.canvas)
                    .font_family(crate::fonts::mono_family())
                    .text_size(px(11.))
                    .text_color(t.text_default);
                for l in lines {
                    code = code.child(div().whitespace_nowrap().child(l));
                }
                body.child(code)
            }
            HoverBlock::Rule => body.child(crate::kumo::separator_h(t)),
            HoverBlock::Text(s) => body.child(
                div()
                    .text_size(scale::TEXT_XS)
                    .text_color(t.text_subtle)
                    .whitespace_normal()
                    .child(s),
            ),
        };
    }

    Some(
        crate::kumo::Surface::overlay(t)
            .absolute()
            .left(px(left))
            .top(px(top))
            .max_w(px(640.))
            .max_h(px(240.))
            .p(scale::SPACE_2)
            .overflow_hidden()
            .child(body)
            .into_any_element(),
    )
}

#[cfg(test)]
mod hover_block_tests {
    use super::{hover_blocks, HoverBlock};

    #[test]
    fn clangd_include_hover_splits_into_heading_and_code() {
        let md = "### `mps_gemm.h`  \n\n---\n```\n/Users/x/mps_gemm.h\n```";
        assert_eq!(
            hover_blocks(md),
            vec![
                HoverBlock::Heading("mps_gemm.h".into()),
                HoverBlock::Code(vec!["/Users/x/mps_gemm.h".into()]),
            ]
        );
    }

    #[test]
    fn paragraphs_join_and_fences_stay_verbatim() {
        let md = "### `int x`\n\nType: `int`\nSize: 4 bytes\n\n```cpp\n  int x = 1;\n```";
        assert_eq!(
            hover_blocks(md),
            vec![
                HoverBlock::Heading("int x".into()),
                HoverBlock::Text("Type: int Size: 4 bytes".into()),
                HoverBlock::Code(vec!["  int x = 1;".into()]),
            ]
        );
    }
}

/// Build the `line → RuntimeAlloc` map for the active tab's file from the
/// memory-bar per-`file:line` tracker (§4.5 system 3). Keys are matched by full
/// path or basename, since the alloc event's `file` field form isn't fixed.
fn runtime_allocs_for(
    app: &JadeApp,
    tab: &crate::editor_view::OpenTab,
) -> HashMap<u32, RuntimeAlloc> {
    let mut out = HashMap::new();
    if app.mem.per_line.is_empty() {
        return out;
    }
    let tab_disp = tab.path.to_string_lossy();
    let tab_base = tab
        .path
        .file_name()
        .and_then(|n| n.to_str())
        .map(str::to_string);
    for (key, la) in &app.mem.per_line {
        let Some((file, lnum)) = key.rsplit_once(':') else {
            continue;
        };
        let Ok(line) = lnum.parse::<u32>() else {
            continue;
        };
        let base = std::path::Path::new(file)
            .file_name()
            .and_then(|n| n.to_str());
        let matches =
            file == tab_disp || (tab_base.as_deref().is_some() && tab_base.as_deref() == base);
        if matches {
            out.insert(
                line,
                RuntimeAlloc {
                    calls: la.calls,
                    bytes: la.bytes,
                    leaked: la.leaked,
                },
            );
        }
    }
    out
}

/// The tab strip above the viewer (deliverable §3): one chip per open tab with a
/// an `x` close icon, the active tab underlined. Middle-click also closes (GPUI exposes
/// the mouse button on the down event).
pub fn pane_strip(
    app: &JadeApp,
    pane: usize,
    cx: &mut Context<JadeApp>,
    theme: &Theme,
) -> impl IntoElement {
    let focused = pane == app.focused_pane();
    // The focused slot's editor is the live one; a background slot owns its own.
    let editor = app
        .panes
        .get(pane)
        .and_then(|p| p.editor.as_ref())
        .unwrap_or(&app.editor);
    // Kumo Tabs at `appearance="underline"`: flat triggers on the strip with a
    // 2px brand bar under the active one, and a hairline along the whole list.
    let tint = theme.kumo.tint;
    let mut strip = div()
        .id(("tab-strip", pane))
        .flex()
        .flex_row()
        .items_stretch()
        .h(px(32.))
        .w_full()
        .bg(theme.kumo.elevated)
        .border_b_1()
        .border_color(theme.kumo.hairline)
        .overflow_x_hidden()
        // A tab dropped on the strip's free space lands at the end.
        .drag_over::<TabDrag>(move |st, _, _, _| st.bg(tint))
        .on_drop(cx.listener(move |app, d: &TabDrag, _win, cx| {
            app.move_tab(d.pane, d.index, pane, None);
            app.schedule_ui_save(cx);
            cx.notify();
        }));

    for (i, tab) in editor.tabs.iter().enumerate() {
        let active = editor.active == Some(i);
        strip = strip.child(tab_chip(
            pane,
            i,
            &tab.name,
            active,
            focused,
            tab.buffer.is_dirty(),
            tab.preview,
            theme,
            cx,
        ));
    }
    // Right slot: the pane controls, then the XP bar (§4.10) on the last
    // pane. A flex spacer pushes both to the edge.
    strip = strip
        .child(div().flex_1())
        .child(pane_controls(app, pane, cx, theme));
    if pane + 1 == app.pane_count() {
        strip = strip.child(xp_bar(app, theme));
    }
    strip
}

/// The pane's control cluster in the strip's right slot: the formatted/raw
/// toggle for a Markdown tab (split mode only), the split-right button, and
/// the close-pane button (split mode only).
fn pane_controls(
    app: &JadeApp,
    pane: usize,
    cx: &mut Context<JadeApp>,
    theme: &Theme,
) -> impl IntoElement {
    let t = &theme.kumo;
    let split = app.split_mode();
    let mut row = div()
        .flex()
        .flex_row()
        .items_stretch()
        .h_full()
        .px(scale::SPACE_1)
        .border_l_1()
        .border_color(t.hairline);

    let md_tab = app
        .pane_active_tab(pane)
        .is_some_and(|tab| super::md_view::is_markdown(&tab.path));
    if split && md_tab {
        let rendered = app.panes.get(pane).is_some_and(|p| p.md_rendered);
        row = row
            .child(view_toggle(pane, "Formatted", true, rendered, theme, cx))
            .child(view_toggle(pane, "Raw", false, !rendered, theme, cx));
    }
    if app.pane_count() < crate::panes::MAX_PANES {
        row = row.child(
            strip_icon_button(("pane-split", pane), "columns-2", theme, cx, |app| {
                app.split_pane();
            }),
        );
    }
    if split {
        row = row.child(
            strip_icon_button(("pane-close", pane), "x", theme, cx, move |app| {
                app.close_pane(pane);
            }),
        );
    }
    row
}

/// One side of the formatted/raw toggle: an underline trigger in the same
/// dress as a tab chip, at the strip's text size.
fn view_toggle(
    pane: usize,
    label: &'static str,
    rendered: bool,
    on: bool,
    theme: &Theme,
    cx: &mut Context<JadeApp>,
) -> impl IntoElement {
    let t = &theme.kumo;
    let hover_bg = t.tint;
    let mut b = div()
        .id(("pane-md-view", pane * 2 + usize::from(rendered)))
        .relative()
        .flex()
        .items_center()
        .h_full()
        .px(scale::SPACE_2)
        .text_size(scale::TEXT_XS)
        .cursor_pointer()
        .text_color(if on { t.text_default } else { t.text_subtle })
        .on_click(cx.listener(move |app, _ev, _win, cx| {
            app.set_pane_md(pane, rendered);
            app.schedule_ui_save(cx);
            cx.notify();
        }))
        .child(label);
    if on {
        b = b.font_weight(gpui::FontWeight::MEDIUM).child(
            div()
                .absolute()
                .bottom_0()
                .left_0()
                .right_0()
                .h(px(2.))
                .bg(t.brand),
        );
    } else {
        b = b.hover(move |st| st.bg(hover_bg));
    }
    b
}

/// A square icon button for the strip's right slot.
fn strip_icon_button(
    id: (&'static str, usize),
    icon: &'static str,
    theme: &Theme,
    cx: &mut Context<JadeApp>,
    on_click: impl Fn(&mut JadeApp) + 'static,
) -> impl IntoElement {
    let t = &theme.kumo;
    let hover_bg = t.tint;
    div()
        .id(id)
        .flex()
        .items_center()
        .justify_center()
        .w(px(26.))
        .h_full()
        .cursor_pointer()
        .hover(move |st| st.bg(hover_bg))
        .on_click(cx.listener(move |app, _ev, _win, cx| {
            on_click(app);
            app.schedule_ui_save(cx);
            cx.notify();
        }))
        .child(crate::kumo::icon(icon, 13., t.text_subtle))
}

/// The XP bar (§4.10): `L{n}` label · progress track · `×{streak}` badge (shown
/// only when the streak is live). Mounted in the tab-bar right slot.
fn xp_bar(app: &JadeApp, theme: &Theme) -> impl IntoElement {
    let info = crate::xp::level_from_xp(app.xp.total());
    let streak = app.xp.effective_streak(app.now_ms());
    let pct = if info.needed > 0 {
        (info.progress as f32 / info.needed as f32 * 100.0).clamp(0.0, 100.0)
    } else {
        0.0
    };

    let t = &theme.kumo;
    // The old `.xp-bar`: a brand `L{n}` label, a 90×4 track, the count in
    // mono, and the streak in amber only while it is live. No fills.
    let mut bar = div()
        .id("xp-bar")
        .flex()
        .flex_row()
        .items_center()
        .gap(scale::SPACE_2)
        .px(scale::SPACE_3)
        .border_l_1()
        .border_color(t.hairline)
        .text_size(px(11.))
        .child(
            div()
                .text_color(t.brand)
                .font_weight(gpui::FontWeight::SEMIBOLD)
                .child(format!("L{}", info.level)),
        )
        .child(
            div()
                .w(px(90.))
                .h(px(4.))
                .overflow_hidden()
                .bg(t.fill)
                .child(div().h_full().w(gpui::relative(pct / 100.0)).bg(t.brand)),
        )
        .child(
            div()
                .font_family(crate::fonts::mono_family())
                .text_color(t.text_subtle)
                .child(format!("{}/{}", info.progress, info.needed)),
        );
    if streak > 1 {
        bar = bar.child(
            div()
                .font_family(crate::fonts::mono_family())
                .font_weight(gpui::FontWeight::SEMIBOLD)
                .text_color(t.text_warning)
                .child(format!("×{streak}")),
        );
    }
    bar
}

fn tab_chip(
    pane: usize,
    index: usize,
    name: &str,
    active: bool,
    focused: bool,
    dirty: bool,
    preview: bool,
    theme: &Theme,
    cx: &mut Context<JadeApp>,
) -> impl IntoElement {
    let t = &theme.kumo;
    // One Kumo underline trigger: `text-kumo-subtle` at rest,
    // `aria-selected:text-kumo-default` with the brand bar when active.
    let fg = if active {
        t.text_default
    } else {
        t.text_subtle
    };
    let hover_bg = t.tint;
    let group = gpui::SharedString::from(format!("tab-group-{pane}-{index}"));
    // Element ids are per window: keep the chips of different panes apart.
    let slot = pane * 4096 + index;
    let drag = TabDrag {
        pane,
        index,
        name: name.to_string(),
    };
    let (ghost_bg, ghost_fg, ghost_line) = (t.elevated, t.text_default, t.hairline);

    let mut chip = div()
        .id(("tab", slot))
        // Drag the chip to another pane (or its right edge to split); the
        // ghost is the tab name. A drop on a chip inserts before it.
        .on_drag(drag, move |d, _offset, _win, cx| {
            cx.new(|_| TabDragPreview {
                name: d.name.clone(),
                bg: ghost_bg,
                fg: ghost_fg,
                line: ghost_line,
            })
        })
        .drag_over::<TabDrag>(move |st, _, _, _| st.bg(hover_bg))
        .on_drop(cx.listener(move |app, d: &TabDrag, _win, cx| {
            app.move_tab(d.pane, d.index, pane, Some(index));
            app.schedule_ui_save(cx);
            cx.notify();
        }))
        .group(group.clone())
        .relative()
        .flex()
        .flex_row()
        .items_center()
        .gap(scale::SPACE_1_5)
        .h_full()
        .pl(scale::SPACE_3)
        .pr(scale::SPACE_2)
        .text_size(scale::TEXT_XS)
        .cursor_pointer()
        .text_color(fg)
        // Middle-click closes (mouse-down carries the button). A chip of a
        // background pane first gives that pane the keyboard.
        .on_mouse_down(
            gpui::MouseButton::Middle,
            cx.listener(move |app, _ev, _win, cx| {
                app.focus_pane(pane);
                app.close_tab(index);
                app.schedule_ui_save(cx);
                cx.notify();
            }),
        )
        // Left-click switches to this tab.
        .on_click(cx.listener(move |app, _ev, _win, cx| {
            app.focus_pane(pane);
            app.switch_tab(index);
            app.schedule_ui_save(cx);
            cx.notify();
        }))
        .child({
            // A preview ("temp") tab shows its name in italic, VS Code-style.
            let mut label = div().child(name.to_string());
            if preview {
                label = label.italic();
            }
            label
        });

    if active {
        // The old `.tab-active`: the editor canvas behind the label, so the
        // tab reads as part of the page below it, plus the 2px brand rule.
        // A background pane's active tab keeps the rule in the line ink, so
        // the pane with the keyboard reads at a glance.
        chip = chip
            .bg(t.canvas)
            .font_weight(gpui::FontWeight::MEDIUM)
            .child(
                div()
                    .absolute()
                    .bottom_0()
                    .left_0()
                    .right_0()
                    .h(px(2.))
                    .bg(if focused { t.brand } else { t.line }),
            );
    } else {
        chip = chip.hover(move |st| st.bg(hover_bg));
    }

    // Dirty dot ● from buffer.is_dirty() (§3); silent-close semantics preserved.
    if dirty {
        chip = chip.child(div().size(px(6.)).rounded_full().bg(t.brand));
    }

    // Close button: hidden until the tab is hovered (`.tab:hover .tab-close`),
    // but always there on the active tab. It keeps its box either way so the
    // label does not shift when it appears.
    let mut close = div()
        .id(("tab-close", slot))
        .flex()
        .items_center()
        .justify_center()
        .size(px(16.))
        .cursor_pointer()
        .hover(move |s| s.bg(hover_bg))
        .on_click(cx.listener(move |app, _ev, _win, cx| {
            cx.stop_propagation();
            app.focus_pane(pane);
            app.close_tab(index);
            app.schedule_ui_save(cx);
            cx.notify();
        }))
        .child(crate::kumo::icon("x", 11., t.text_subtle));
    if !active {
        close = close.invisible().group_hover(group, |s| s.visible());
    }
    chip.child(close)
}
