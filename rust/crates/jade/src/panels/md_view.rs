//! Markdown preview panel (right-docked, resizable).
//!
//! The panel renders the active `.md` tab. It opens by itself when a Markdown
//! tab comes to the front and takes the right slot from the mode's side panel
//! (runtime sidebar / board) — see [`JadeApp::md_panel_active`]. ⌘⇧D hides
//! it. The buffer is the single source of truth, so an edit in the code view
//! shows here on the next frame.
//!
//! Rendered forms: headings 1–6, paragraphs, ordered/bulleted/task lists with
//! nesting, GFM tables with column alignment, blockquotes, fenced code,
//! rules, and the inline set — bold, italic, strikethrough, code spans, links
//! (click opens the browser), and image references.
//!
//! The two views stay in step on the caret: a caret move in the editor
//! scrolls the preview to the caret's block (`JadeApp::render` calls
//! [`child_index_for_row`]), and a preview click scrolls the editor to the
//! caret row (`JadeApp::md_preview_click`).
//!
//! The panel also supports edits in place ("preview edit"). A click on a block
//! moves the buffer caret into that block and turns the block into its raw
//! source lines. Keystrokes then flow through the normal editor path
//! ([`JadeApp::editor_key`] and the IME input handler), so undo, selection,
//! paste, and save all work, and the code view shows the same change live.
//! The raw block follows the caret; Escape returns the panel to the fully
//! rendered view.

use std::ops::Range;
use std::path::Path;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;

use gpui::{div, prelude::*, px, rgb, AnyElement, Context, Div, MouseButton};

use crate::app::JadeApp;
use crate::beautiful::markdown::{parse_inline, Style};
use crate::editor_view::{px_to_char_col, OpenTab};
use crate::kumo::{scale, Card, KumoTokens};
use crate::panels::code_view::CHAR_W;
use crate::theme::Theme;

/// Width clamp for the resize drag.
pub const MIN_W: f32 = 300.0;
pub const MAX_W: f32 = 720.0;
/// Default panel width.
pub const DEFAULT_W: f32 = 420.0;

/// Raw-source row height and font size (matches the editor's 13px mono, so
/// [`CHAR_W`] maps a click x to the correct column).
const RAW_LINE_H: f32 = 18.0;
const RAW_FONT_PX: f32 = 13.0;
/// Inner padding of the raw block (click x math subtracts it).
const RAW_PAD: f32 = 8.0;

/// True when `path` is a Markdown file.
pub fn is_markdown(path: &Path) -> bool {
    matches!(
        path.extension()
            .and_then(|e| e.to_str())
            .map(|e| e.to_ascii_lowercase())
            .as_deref(),
        Some("md" | "markdown" | "mdown" | "mkd")
    )
}

// ── Inline layer ──────────────────────────────────────────────────────────────

/// One inline run: a styled span plus an optional link target.
/// [`crate::beautiful::markdown::parse_inline`] supplies the emphasis; the
/// link layer here splits `[text](url)` / `![alt](url)` out first.
#[derive(Debug, Clone, PartialEq)]
pub struct MdSpan {
    pub text: String,
    pub style: Style,
    /// `Some(url)` for link text (and image references).
    pub link: Option<String>,
}

/// Parse inline markup, links included.
pub fn inline_md(src: &str) -> Vec<MdSpan> {
    let mut out = Vec::new();
    let mut plain = |seg: &str, out: &mut Vec<MdSpan>| {
        for sp in parse_inline(seg) {
            out.push(MdSpan {
                text: sp.text,
                style: sp.style,
                link: None,
            });
        }
    };
    let mut rest = src;
    while let Some((before, label, url, image, after)) = find_link(rest) {
        plain(before, &mut out);
        // An image reference renders as its alt text with a picture mark —
        // the panel does not load image files.
        let label = if image {
            format!("\u{1F5BC} {label}")
        } else {
            label.to_string()
        };
        for sp in parse_inline(&label) {
            out.push(MdSpan {
                text: sp.text,
                style: sp.style,
                link: Some(url.to_string()),
            });
        }
        rest = after;
    }
    plain(rest, &mut out);
    out
}

/// Find the first `[label](url)` (or `![alt](url)`) outside code spans:
/// `(before, label, url, is_image, after)`. All markers are ASCII, so the
/// byte offsets land on char boundaries.
fn find_link(s: &str) -> Option<(&str, &str, &str, bool, &str)> {
    let b = s.as_bytes();
    let mut i = 0;
    let mut in_code = false;
    while i < b.len() {
        match b[i] {
            b'`' => in_code = !in_code,
            b'[' if !in_code => {
                if let Some(close) = s[i + 1..].find(']').map(|j| i + 1 + j) {
                    if b.get(close + 1) == Some(&b'(') {
                        if let Some(end) = s[close + 2..].find(')').map(|j| close + 2 + j) {
                            let image = i > 0 && b[i - 1] == b'!';
                            let start = if image { i - 1 } else { i };
                            return Some((
                                &s[..start],
                                &s[i + 1..close],
                                &s[close + 2..end],
                                image,
                                &s[end + 1..],
                            ));
                        }
                    }
                }
            }
            _ => {}
        }
        i += 1;
    }
    None
}

/// The plain text of a span list, markup removed (tests + assertions).
pub fn plain_md(spans: &[MdSpan]) -> String {
    spans.iter().map(|s| s.text.as_str()).collect()
}

// ── Block parser ──────────────────────────────────────────────────────────────

/// A table column's alignment, from the delimiter row (`:---`, `:---:`, `---:`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Align {
    Left,
    Center,
    Right,
}

/// A block-level element with its source rows (`rows` is 0-based, end
/// exclusive). The rows let a preview click map back to buffer positions.
#[derive(Debug, Clone, PartialEq)]
pub enum MdBlock {
    Heading {
        level: usize,
        spans: Vec<MdSpan>,
        rows: Range<usize>,
    },
    Paragraph {
        spans: Vec<MdSpan>,
        rows: Range<usize>,
    },
    Item {
        spans: Vec<MdSpan>,
        ordered: bool,
        number: usize,
        /// Task-list state: `Some(done)` for `- [ ]` / `- [x]` items.
        task: Option<bool>,
        /// Nesting depth from the leading indent (2 columns per level).
        depth: usize,
        rows: Range<usize>,
    },
    Code {
        lang: String,
        text: String,
        rows: Range<usize>,
    },
    Quote {
        spans: Vec<MdSpan>,
        rows: Range<usize>,
    },
    Table {
        header: Vec<Vec<MdSpan>>,
        aligns: Vec<Align>,
        body: Vec<Vec<Vec<MdSpan>>>,
        rows: Range<usize>,
    },
    Rule {
        rows: Range<usize>,
    },
}

impl MdBlock {
    /// The source rows this block covers.
    pub fn rows(&self) -> Range<usize> {
        match self {
            MdBlock::Heading { rows, .. }
            | MdBlock::Paragraph { rows, .. }
            | MdBlock::Item { rows, .. }
            | MdBlock::Code { rows, .. }
            | MdBlock::Quote { rows, .. }
            | MdBlock::Table { rows, .. }
            | MdBlock::Rule { rows } => rows.clone(),
        }
    }
}

/// `### Title` → `(3, "Title")`. The hash marks need a following space (or
/// end of line), so prose such as `#include` stays a paragraph.
fn heading(line: &str) -> Option<(usize, &str)> {
    let n = line.len() - line.trim_start_matches('#').len();
    if !(1..=6).contains(&n) {
        return None;
    }
    let rest = &line[n..];
    if rest.is_empty() {
        return Some((n, ""));
    }
    rest.strip_prefix(' ').map(|r| (n, r.trim_start()))
}

/// A thematic break: three or more of the same `-` / `*` / `_`, spaces allowed.
fn is_rule(line: &str) -> bool {
    let s: Vec<char> = line.chars().filter(|c| *c != ' ').collect();
    s.len() >= 3 && s.iter().all(|c| *c == s[0]) && matches!(s[0], '-' | '*' | '_')
}

/// `- item`, `* item`, `1. item` → the body, plus whether it was numbered.
fn list_item(line: &str) -> Option<(&str, bool, usize)> {
    for m in ["- ", "* ", "+ "] {
        if let Some(rest) = line.strip_prefix(m) {
            return Some((rest, false, 0));
        }
    }
    let digits = line.len() - line.trim_start_matches(|c: char| c.is_ascii_digit()).len();
    if digits > 0 {
        let rest = &line[digits..];
        if let Some(body) = rest.strip_prefix(". ").or_else(|| rest.strip_prefix(") ")) {
            let n = line[..digits].parse().unwrap_or(0);
            return Some((body, true, n));
        }
    }
    None
}

/// `[ ] body` / `[x] body` → `(Some(done), body)`; else the input unchanged.
fn task_item(body: &str) -> (Option<bool>, &str) {
    if let Some(rest) = body.strip_prefix("[ ] ") {
        return (Some(false), rest);
    }
    for m in ["[x] ", "[X] "] {
        if let Some(rest) = body.strip_prefix(m) {
            return (Some(true), rest);
        }
    }
    (None, body)
}

/// Leading indent in columns (a tab counts 4).
fn indent_cols(line: &str) -> usize {
    let mut cols = 0;
    for c in line.chars() {
        match c {
            ' ' => cols += 1,
            '\t' => cols += 4,
            _ => break,
        }
    }
    cols
}

/// Split a `| a | b |` row into trimmed cells, or `None` without a pipe.
fn split_row(line: &str) -> Option<Vec<String>> {
    let t = line.trim();
    if !t.contains('|') {
        return None;
    }
    let t = t.strip_prefix('|').unwrap_or(t);
    let t = t.strip_suffix('|').unwrap_or(t);
    Some(t.split('|').map(|c| c.trim().to_string()).collect())
}

/// A table delimiter row (`| --- | :--: |`) → per-column alignment.
fn table_delim(line: &str) -> Option<Vec<Align>> {
    let cells = split_row(line)?;
    let mut aligns = Vec::with_capacity(cells.len());
    for c in &cells {
        let body = c.trim_start_matches(':').trim_end_matches(':');
        if body.is_empty() || !body.chars().all(|ch| ch == '-') {
            return None;
        }
        aligns.push(match (c.starts_with(':'), c.ends_with(':')) {
            (true, true) => Align::Center,
            (false, true) => Align::Right,
            _ => Align::Left,
        });
    }
    Some(aligns)
}

/// True when `line` starts a non-paragraph block (a paragraph stops here).
/// A `|` start counts, so a table can follow a paragraph without a blank line.
fn breaks_paragraph(t: &str) -> bool {
    t.is_empty()
        || t.starts_with("```")
        || t.starts_with('>')
        || t.starts_with('|')
        || is_rule(t)
        || heading(t).is_some()
        || list_item(t).is_some()
}

/// Split `text` into blocks with source rows.
pub fn parse_blocks(text: &str) -> Vec<MdBlock> {
    let lines: Vec<&str> = text.split('\n').collect();
    let mut out = Vec::new();
    let mut i = 0;
    while i < lines.len() {
        let raw = lines[i];
        let t = raw.trim_start();
        if t.is_empty() {
            i += 1;
            continue;
        }
        if let Some(rest) = t.strip_prefix("```") {
            // A fence runs to the closing marker, or to the end of the file.
            let lang = rest.trim().to_string();
            let start = i;
            i += 1;
            let mut body = Vec::new();
            while i < lines.len() && !lines[i].trim_start().starts_with("```") {
                body.push(lines[i]);
                i += 1;
            }
            if i < lines.len() {
                i += 1; // closing fence
            }
            out.push(MdBlock::Code {
                lang,
                text: body.join("\n"),
                rows: start..i,
            });
            continue;
        }
        if is_rule(t) {
            out.push(MdBlock::Rule { rows: i..i + 1 });
            i += 1;
            continue;
        }
        if let Some((level, rest)) = heading(t) {
            out.push(MdBlock::Heading {
                level,
                spans: inline_md(rest),
                rows: i..i + 1,
            });
            i += 1;
            continue;
        }
        if t.starts_with('>') {
            // Adjacent quote lines merge into one block.
            let start = i;
            let mut buf = Vec::new();
            while i < lines.len() {
                let q = lines[i].trim_start();
                let Some(rest) = q.strip_prefix('>') else { break };
                buf.push(rest.trim_start().to_string());
                i += 1;
            }
            out.push(MdBlock::Quote {
                spans: inline_md(buf.join(" ").trim()),
                rows: start..i,
            });
            continue;
        }
        // A GFM table: a header row whose next line is the delimiter row.
        if let Some(cells) = split_row(t) {
            if let Some(aligns) = lines.get(i + 1).and_then(|l| table_delim(l)) {
                let start = i;
                let header: Vec<Vec<MdSpan>> = cells.iter().map(|c| inline_md(c)).collect();
                i += 2;
                let mut body = Vec::new();
                while i < lines.len() {
                    let Some(rc) = split_row(lines[i]) else { break };
                    body.push(rc.iter().map(|c| inline_md(c)).collect());
                    i += 1;
                }
                out.push(MdBlock::Table {
                    header,
                    aligns,
                    body,
                    rows: start..i,
                });
                continue;
            }
        }
        if let Some((rest, ordered, number)) = list_item(t) {
            let (task, rest) = task_item(rest);
            out.push(MdBlock::Item {
                spans: inline_md(rest),
                ordered,
                number,
                task,
                depth: indent_cols(raw) / 2,
                rows: i..i + 1,
            });
            i += 1;
            continue;
        }
        // Paragraph: adjacent plain lines merge.
        let start = i;
        let mut buf = vec![t];
        i += 1;
        while i < lines.len() {
            let n = lines[i].trim_start();
            if breaks_paragraph(n) {
                break;
            }
            buf.push(n);
            i += 1;
        }
        out.push(MdBlock::Paragraph {
            spans: inline_md(&buf.join(" ")),
            rows: start..i,
        });
    }
    out
}

/// The index of the preview's scroll child that covers `row`. Blocks render
/// in order; in edit mode a synthetic raw region for a blank row is inserted
/// between blocks (see [`panel`]). Drives the caret → preview scroll sync.
pub fn child_index_for_row(blocks: &[MdBlock], row: usize, edit: bool) -> usize {
    if let Some(i) = blocks.iter().position(|b| b.rows().contains(&row)) {
        return i;
    }
    if edit {
        blocks
            .iter()
            .position(|b| b.rows().start > row)
            .unwrap_or(blocks.len())
    } else {
        blocks
            .iter()
            .rposition(|b| b.rows().start <= row)
            .unwrap_or(0)
    }
}

// ── Panel ─────────────────────────────────────────────────────────────────────

/// The whole panel: header, rendered blocks, and the left-edge resize handle.
/// Rendered only while [`JadeApp::md_panel_active`] holds.
pub fn panel(app: &JadeApp, cx: &mut Context<JadeApp>, theme: &Theme) -> AnyElement {
    if !app.md_visible {
        return div().into_any_element();
    }
    let Some(tab) = app.editor.active_tab() else {
        return div().into_any_element();
    };
    if !is_markdown(&tab.path) {
        return div().into_any_element();
    }

    let t = &theme.kumo;
    let text = tab.buffer.to_string();
    let blocks = parse_blocks(&text);
    let caret = tab.caret_point();
    let w = app.md_width;

    // Preview-edit mode: the block under the caret shows its raw source. A
    // caret on a blank row between blocks gets a synthetic one-row region.
    let active: Option<Range<usize>> = if app.md_edit {
        Some(
            blocks
                .iter()
                .find(|b| b.rows().contains(&caret.row))
                .map(|b| b.rows())
                .unwrap_or(caret.row..caret.row + 1),
        )
    } else {
        None
    };

    // The scroll container's direct children are the blocks, in order —
    // `child_index_for_row` + `ScrollHandle::scroll_to_item` depend on that.
    let mut body = div()
        .id("md-scroll")
        .flex_1()
        .min_h(px(0.))
        .flex()
        .flex_col()
        .gap(px(10.))
        .overflow_y_scroll()
        .track_scroll(&app.md_scroll);
    let mut raw_done = false;
    for b in &blocks {
        if let Some(a) = &active {
            // A synthetic blank-row region sits between blocks: emit it in
            // source order, before the first block that follows it.
            if !raw_done && a.end <= b.rows().start {
                body = body.child(raw_region(app, tab, a.clone(), &caret, theme, cx));
                raw_done = true;
            }
            if !raw_done && b.rows().contains(&a.start) {
                body = body.child(raw_region(app, tab, a.clone(), &caret, theme, cx));
                raw_done = true;
                continue;
            }
        }
        body = body.child(render_block(b, theme, cx));
    }
    if let Some(a) = &active {
        if !raw_done {
            body = body.child(raw_region(app, tab, a.clone(), &caret, theme, cx));
        }
    }
    if blocks.is_empty() && active.is_none() {
        body = body.child(
            div()
                .id("md-empty")
                .text_size(px(12.))
                .text_color(t.text_placeholder)
                .cursor(gpui::CursorStyle::IBeam)
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(|app, _ev: &gpui::MouseDownEvent, _w, cx| {
                        app.md_preview_click(0, 0, cx);
                    }),
                )
                .child("Empty file — click to write"),
        );
    }

    let hint = if app.md_edit {
        "esc — done editing"
    } else {
        "click a block to edit"
    };
    let header = div()
        .flex()
        .flex_none()
        .flex_row()
        .items_center()
        .gap(px(6.))
        .pb(px(6.))
        .border_b_1()
        .border_color(t.hairline)
        .child(
            div()
                .text_size(scale::TEXT_XS)
                .font_weight(gpui::FontWeight::SEMIBOLD)
                .text_color(t.text_strong)
                .child(tab.name.clone()),
        )
        .child(div().flex_1())
        .child(
            div()
                .text_size(px(10.5))
                .text_color(t.text_subtle)
                .child(hint),
        );

    let card = Card::new(t)
        .id("markdown-panel-card")
        .flex()
        .flex_none()
        .flex_col()
        .gap(scale::SPACE_3)
        .w(px(w))
        .h_full()
        .min_h(px(0.))
        .p(scale::SPACE_3)
        .bg(t.elevated)
        .child(header)
        .child(body);

    div()
        .debug_selector(|| "markdown-panel".into())
        .flex()
        .flex_row()
        .flex_none()
        .h_full()
        .child(resize_handle(cx, theme))
        .child(card)
        .into_any_element()
}

/// The 6px left-edge grab strip (drag to resize; completes at the app root).
fn resize_handle(cx: &mut Context<JadeApp>, theme: &Theme) -> impl IntoElement {
    div()
        .id("md-resize")
        .w(px(6.))
        .h_full()
        .flex_none()
        .cursor(gpui::CursorStyle::ResizeLeftRight)
        .hover(|s| s.bg(theme.kumo.tint))
        .on_mouse_down(
            MouseButton::Left,
            cx.listener(|app: &mut JadeApp, ev: &gpui::MouseDownEvent, _w, cx| {
                app.md_resize = Some((f32::from(ev.position.x), app.md_width));
                cx.notify();
            }),
        )
}

// ── Rendered blocks ───────────────────────────────────────────────────────────

/// One rendered block. A click moves the caret to the block's first row and
/// enters preview-edit mode.
fn render_block(b: &MdBlock, theme: &Theme, cx: &mut Context<JadeApp>) -> AnyElement {
    let t = &theme.kumo;
    let start = b.rows().start;
    let inner = match b {
        MdBlock::Heading { level, spans, .. } => {
            let (size, weight) = match level {
                1 => (19.0, gpui::FontWeight::BOLD),
                2 => (16.0, gpui::FontWeight::SEMIBOLD),
                3 => (14.0, gpui::FontWeight::SEMIBOLD),
                _ => (13.0, gpui::FontWeight::SEMIBOLD),
            };
            let mut h = span_row(spans, size, t, cx)
                .font_weight(weight)
                .text_color(t.text_strong);
            if *level <= 2 {
                h = h.pb(px(4.)).border_b_1().border_color(t.hairline);
            }
            h
        }
        MdBlock::Paragraph { spans, .. } => span_row(spans, 12.5, t, cx),
        MdBlock::Item {
            spans,
            ordered,
            number,
            task,
            depth,
            ..
        } => {
            let (marker, marker_color) = match task {
                Some(true) => ("\u{2611}".to_string(), t.brand),
                Some(false) => ("\u{2610}".to_string(), t.text_subtle),
                None if *ordered => (format!("{number}."), t.text_subtle),
                None => ("\u{2022}".to_string(), t.text_subtle),
            };
            let mut row = span_row(spans, 12.5, t, cx).flex_1();
            // A finished task reads as done: struck and faded.
            if *task == Some(true) {
                row = row.line_through().text_color(t.text_subtle);
            }
            div()
                .flex()
                .flex_row()
                .items_start()
                .gap(px(6.))
                .pl(px(*depth as f32 * 14.0))
                .child(
                    div()
                        .flex_none()
                        .min_w(px(14.))
                        .text_size(px(12.5))
                        .text_color(marker_color)
                        .child(marker),
                )
                .child(row)
        }
        MdBlock::Code { text, .. } => div()
            .w_full()
            .rounded(scale::RADIUS_MD)
            .bg(t.recessed)
            .border_1()
            .border_color(t.hairline)
            .p(px(8.))
            .text_size(px(11.5))
            .line_height(px(16.))
            .text_color(t.text_default)
            .child(text.clone()),
        MdBlock::Quote { spans, .. } => div()
            .border_l_2()
            .border_color(t.line)
            .pl(px(8.))
            .child(span_row(spans, 12.5, t, cx).text_color(t.text_subtle)),
        MdBlock::Table {
            header,
            aligns,
            body,
            ..
        } => table_el(header, aligns, body, t, cx),
        MdBlock::Rule { .. } => div()
            .py(px(4.))
            .child(div().h(px(1.)).w_full().bg(t.hairline)),
    };
    div()
        .id(("md-block", start))
        .debug_selector(move || format!("md-block-{start}"))
        .w_full()
        .flex_none()
        .cursor(gpui::CursorStyle::IBeam)
        .on_mouse_down(
            MouseButton::Left,
            cx.listener(move |app, _ev: &gpui::MouseDownEvent, _w, cx| {
                app.md_preview_click(start, 0, cx);
            }),
        )
        .child(inner)
        .into_any_element()
}

/// A GFM table: equal-width columns, header on the recessed fill, hairline
/// row rules, per-column alignment from the delimiter row.
fn table_el(
    header: &[Vec<MdSpan>],
    aligns: &[Align],
    body: &[Vec<Vec<MdSpan>>],
    t: &KumoTokens,
    cx: &mut Context<JadeApp>,
) -> Div {
    let ncols = header.len().max(1);
    let empty: Vec<MdSpan> = Vec::new();
    let cell = |spans: &[MdSpan], c: usize, cx: &mut Context<JadeApp>| {
        let mut s = span_row(spans, 12.0, t, cx);
        match aligns.get(c) {
            Some(Align::Center) => s = s.justify_center(),
            Some(Align::Right) => s = s.justify_end(),
            _ => {}
        }
        div().flex_1().min_w(px(0.)).px(px(8.)).py(px(5.)).child(s)
    };

    let mut head = div()
        .flex()
        .flex_row()
        .bg(t.recessed)
        .border_b_1()
        .border_color(t.hairline)
        .font_weight(gpui::FontWeight::SEMIBOLD)
        .text_color(t.text_strong);
    for c in 0..ncols {
        head = head.child(cell(header.get(c).unwrap_or(&empty), c, cx));
    }

    let mut tbl = div()
        .w_full()
        .flex()
        .flex_col()
        .rounded(scale::RADIUS_MD)
        .border_1()
        .border_color(t.hairline)
        .overflow_hidden()
        .child(head);
    for (i, row) in body.iter().enumerate() {
        let mut r = div().flex().flex_row();
        if i + 1 < body.len() {
            r = r.border_b_1().border_color(t.hairline);
        }
        for c in 0..ncols {
            r = r.child(cell(row.get(c).unwrap_or(&empty), c, cx));
        }
        tbl = tbl.child(r);
    }
    tbl
}

/// A paragraph as a wrapping row of per-word elements. A flex row lays each
/// child out as one item, and an item does not wrap inside itself — so every
/// word gets its own element (same reason as `beautiful::stream::pieces`).
fn span_row(spans: &[MdSpan], size: f32, t: &KumoTokens, cx: &mut Context<JadeApp>) -> Div {
    let mut row = div()
        .flex()
        .flex_row()
        .flex_wrap()
        .w_full()
        .min_w(px(0.))
        .text_size(px(size))
        .line_height(px(size * 1.5))
        .text_color(t.text_default);
    for sp in spans {
        for w in sp.text.split_inclusive(' ') {
            row = row.child(word(w, sp.style, sp.link.as_deref(), t, cx));
        }
    }
    row
}

/// A run of text wearing its inline style. A link word opens its URL in the
/// browser; the click does not bubble to the block (no edit-mode entry).
fn word(
    text: &str,
    style: Style,
    link: Option<&str>,
    t: &KumoTokens,
    cx: &mut Context<JadeApp>,
) -> AnyElement {
    let mut d = div().child(text.to_string());
    if style.code {
        d = d
            .text_size(px(11.5))
            .text_color(t.text_strong)
            .bg(t.recessed)
            .rounded(px(4.))
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
    if let Some(url) = link {
        // `[x](url "title")`: the target is the first token in the parens.
        let target = url
            .split_whitespace()
            .next()
            .unwrap_or_default()
            .to_string();
        d = d
            .text_color(t.text_link)
            .underline()
            .cursor(gpui::CursorStyle::PointingHand)
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(move |_app, _ev: &gpui::MouseDownEvent, _w, cx| {
                    cx.stop_propagation();
                    if !target.is_empty() {
                        cx.open_url(&target);
                    }
                }),
            );
    }
    d.into_any_element()
}

// ── The raw (in-edit) block ───────────────────────────────────────────────────

/// The active block as raw source lines with the buffer caret. Clicks map to
/// exact columns via the origin captured at paint.
fn raw_region(
    app: &JadeApp,
    tab: &OpenTab,
    rows: Range<usize>,
    caret: &jade_buffer::Point,
    theme: &Theme,
    cx: &mut Context<JadeApp>,
) -> AnyElement {
    let t = &theme.kumo;
    let start = rows.start;
    // Layout is only known at paint, so a canvas stores the region's x origin
    // for the click → column math (same trick as the code view's overlay).
    let store = app.md_raw_x.clone();
    let cap = gpui::canvas(
        move |_b, _w, _cx| {},
        move |b: gpui::Bounds<gpui::Pixels>, _, _w, _cx| {
            store.store(f32::from(b.origin.x).to_bits(), Ordering::Relaxed);
        },
    )
    .absolute()
    .top_0()
    .left_0()
    .size_full();

    let mut col = div()
        .debug_selector(move || format!("md-raw-{start}"))
        .relative()
        .flex()
        .flex_none()
        .flex_col()
        .w_full()
        .rounded(scale::RADIUS_MD)
        .bg(t.recessed)
        .border_1()
        .border_color(t.focus)
        .p(px(RAW_PAD))
        .child(cap);
    let end = rows.end.min(tab.line_count());
    for row in start..end {
        let line = tab.line(row);
        let caret_col = (row == caret.row).then_some(caret.col);
        col = col.child(raw_line(
            row,
            line,
            caret_col,
            app.caret_blink_show,
            app.md_raw_x.clone(),
            theme,
            cx,
        ));
    }
    col.into_any_element()
}

/// One raw source line. The caret splits the line so the bar sits mid-text.
fn raw_line(
    row: usize,
    line: String,
    caret_col: Option<usize>,
    blink: bool,
    origin: Arc<AtomicU32>,
    theme: &Theme,
    cx: &mut Context<JadeApp>,
) -> gpui::Stateful<Div> {
    let t = &theme.kumo;
    let click_line = line.clone();
    let run = |s: &str| {
        div()
            .flex_none()
            .whitespace_nowrap()
            .child(s.to_string())
    };
    let mut el = div()
        .id(("md-raw-line", row))
        .h(px(RAW_LINE_H))
        .flex()
        .flex_row()
        .items_center()
        .text_size(px(RAW_FONT_PX))
        .text_color(t.text_default)
        .whitespace_nowrap()
        .cursor(gpui::CursorStyle::IBeam)
        .on_mouse_down(
            MouseButton::Left,
            cx.listener(move |app, ev: &gpui::MouseDownEvent, _w, cx| {
                let x0 = f32::from_bits(origin.load(Ordering::Relaxed));
                let x_rel = f32::from(ev.position.x) - x0 - RAW_PAD;
                let col = px_to_char_col(&click_line, x_rel, CHAR_W);
                app.md_preview_click(row, col, cx);
            }),
        );
    match caret_col {
        None => {
            el = el.child(run(&line));
        }
        Some(c) => {
            let at = line
                .char_indices()
                .nth(c)
                .map(|(i, _)| i)
                .unwrap_or(line.len());
            let (before, after) = line.split_at(at);
            if !before.is_empty() {
                el = el.child(run(before));
            }
            let mut bar = div().w(px(1.5)).h(px(14.)).flex_none();
            if blink {
                bar = bar.bg(rgb(theme.accent));
            }
            el = el.child(bar);
            if !after.is_empty() {
                el = el.child(run(after));
            }
        }
    }
    el
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn spans_text(b: &MdBlock) -> String {
        match b {
            MdBlock::Heading { spans, .. }
            | MdBlock::Paragraph { spans, .. }
            | MdBlock::Item { spans, .. }
            | MdBlock::Quote { spans, .. } => plain_md(spans),
            MdBlock::Code { text, .. } => text.clone(),
            MdBlock::Table { header, .. } => header
                .iter()
                .map(|c| plain_md(c))
                .collect::<Vec<_>>()
                .join("|"),
            MdBlock::Rule { .. } => String::new(),
        }
    }

    #[test]
    fn markdown_extensions_match() {
        assert!(is_markdown(&PathBuf::from("/a/README.md")));
        assert!(is_markdown(&PathBuf::from("/a/notes.MARKDOWN")));
        assert!(!is_markdown(&PathBuf::from("/a/main.cpp")));
        assert!(!is_markdown(&PathBuf::from("/a/README")));
    }

    #[test]
    fn blocks_carry_source_rows() {
        let src = "# Title\n\nOne line\nsame para\n\n- item\n";
        let b = parse_blocks(src);
        assert_eq!(b.len(), 3);
        assert!(matches!(&b[0], MdBlock::Heading { level: 1, .. }));
        assert_eq!(b[0].rows(), 0..1);
        assert_eq!(b[1].rows(), 2..4);
        assert_eq!(spans_text(&b[1]), "One line same para");
        assert!(matches!(&b[2], MdBlock::Item { ordered: false, .. }));
        assert_eq!(b[2].rows(), 5..6);
    }

    #[test]
    fn heading_needs_a_space() {
        let b = parse_blocks("#include <vector>\n");
        assert!(matches!(&b[0], MdBlock::Paragraph { .. }));
        let b = parse_blocks("## Two\n");
        assert!(matches!(&b[0], MdBlock::Heading { level: 2, .. }));
    }

    #[test]
    fn fence_keeps_lang_and_rows() {
        let b = parse_blocks("```rust\nlet x = 1;\nlet y = 2;\n```\nafter\n");
        match &b[0] {
            MdBlock::Code { lang, text, rows } => {
                assert_eq!(lang, "rust");
                assert_eq!(text, "let x = 1;\nlet y = 2;");
                assert_eq!(*rows, 0..4);
            }
            other => panic!("{other:?}"),
        }
        assert_eq!(b[1].rows(), 4..5);
    }

    #[test]
    fn unclosed_fence_runs_to_the_end() {
        let b = parse_blocks("```\nint x;");
        match &b[0] {
            MdBlock::Code { text, rows, .. } => {
                assert_eq!(text, "int x;");
                assert_eq!(*rows, 0..2);
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn quote_lines_merge() {
        let b = parse_blocks("> first\n> second\nplain\n");
        assert_eq!(b.len(), 2);
        match &b[0] {
            MdBlock::Quote { rows, .. } => assert_eq!(*rows, 0..2),
            other => panic!("{other:?}"),
        }
        assert_eq!(spans_text(&b[0]), "first second");
    }

    #[test]
    fn rule_is_not_a_list() {
        let b = parse_blocks("---\n- item\n***\n");
        assert!(matches!(&b[0], MdBlock::Rule { .. }));
        assert!(matches!(&b[1], MdBlock::Item { .. }));
        assert!(matches!(&b[2], MdBlock::Rule { .. }));
    }

    #[test]
    fn ordered_items_and_depth() {
        let b = parse_blocks("1. first\n  - nested\n");
        match &b[0] {
            MdBlock::Item {
                ordered, number, depth, ..
            } => {
                assert!(*ordered);
                assert_eq!(*number, 1);
                assert_eq!(*depth, 0);
            }
            other => panic!("{other:?}"),
        }
        match &b[1] {
            MdBlock::Item { depth, .. } => assert_eq!(*depth, 1),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn a_decimal_is_not_a_list() {
        let b = parse_blocks("3.14 is pi\n");
        assert!(matches!(&b[0], MdBlock::Paragraph { .. }));
    }

    #[test]
    fn task_items_carry_their_state() {
        let b = parse_blocks("- [ ] open\n- [x] done\n- plain\n");
        match &b[0] {
            MdBlock::Item { task, spans, .. } => {
                assert_eq!(*task, Some(false));
                assert_eq!(plain_md(spans), "open");
            }
            other => panic!("{other:?}"),
        }
        assert!(matches!(&b[1], MdBlock::Item { task: Some(true), .. }));
        assert!(matches!(&b[2], MdBlock::Item { task: None, .. }));
    }

    #[test]
    fn table_parses_header_alignment_body_and_rows() {
        let src = "| Name | **Size** | Cost |\n| :--- | :---: | ---: |\n| a | 1 | 2 |\n| b | 3 | 4 |\nafter\n";
        let b = parse_blocks(src);
        match &b[0] {
            MdBlock::Table {
                header,
                aligns,
                body,
                rows,
            } => {
                assert_eq!(header.len(), 3);
                assert_eq!(plain_md(&header[1]), "Size");
                assert!(header[1].iter().any(|s| s.style.bold));
                assert_eq!(aligns, &[Align::Left, Align::Center, Align::Right]);
                assert_eq!(body.len(), 2);
                assert_eq!(plain_md(&body[1][2]), "4");
                assert_eq!(*rows, 0..4);
            }
            other => panic!("{other:?}"),
        }
        assert!(matches!(&b[1], MdBlock::Paragraph { .. }));
    }

    #[test]
    fn a_pipe_line_without_a_delimiter_is_prose() {
        let b = parse_blocks("a | b\nplain\n");
        assert_eq!(b.len(), 1);
        assert!(matches!(&b[0], MdBlock::Paragraph { .. }));
    }

    #[test]
    fn a_table_interrupts_a_paragraph() {
        let b = parse_blocks("prose\n| a | b |\n| - | - |\n| 1 | 2 |\n");
        assert_eq!(b.len(), 2);
        assert!(matches!(&b[0], MdBlock::Paragraph { .. }));
        assert!(matches!(&b[1], MdBlock::Table { .. }));
    }

    #[test]
    fn links_and_images_split_out() {
        let s = inline_md("see [the *docs*](https://x.dev \"t\") now");
        assert_eq!(plain_md(&s), "see the docs now");
        let linked: Vec<_> = s.iter().filter(|p| p.link.is_some()).collect();
        assert_eq!(plain_md(&linked.iter().map(|p| (*p).clone()).collect::<Vec<_>>()), "the docs");
        assert!(linked.iter().all(|p| p.link.as_deref() == Some("https://x.dev \"t\"")));
        assert!(linked.iter().any(|p| p.style.italic));

        let s = inline_md("![chart](img.png)");
        assert!(s[0].link.is_some());
        assert!(plain_md(&s).contains("chart"));
    }

    #[test]
    fn a_bracket_in_a_code_span_is_not_a_link() {
        let s = inline_md("`a[0](x)` stays code");
        assert!(s.iter().all(|p| p.link.is_none()));
        assert_eq!(plain_md(&s), "a[0](x) stays code");
    }

    #[test]
    fn strike_reaches_the_preview_spans() {
        let s = inline_md("keep ~~drop~~");
        assert!(s.iter().any(|p| p.style.strike && p.text == "drop"));
    }

    #[test]
    fn child_index_follows_the_caret() {
        let blocks = parse_blocks("# T\n\npara\n\n- item\n");
        // rows: heading 0..1, paragraph 2..3, item 4..5.
        assert_eq!(child_index_for_row(&blocks, 0, false), 0);
        assert_eq!(child_index_for_row(&blocks, 2, false), 1);
        assert_eq!(child_index_for_row(&blocks, 4, false), 2);
        // A blank row maps to the nearest block before it…
        assert_eq!(child_index_for_row(&blocks, 3, false), 1);
        // …and in edit mode to the synthetic raw region's insert position.
        assert_eq!(child_index_for_row(&blocks, 3, true), 2);
        assert_eq!(child_index_for_row(&blocks, 99, true), 3);
    }
}
