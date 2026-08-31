//! TERMINAL panel (feature inventory §5.2, Phase-4 wave 2).
//!
//! Renders a [`jade_term::GridSnapshot`] as monospace rows and forwards
//! keystrokes to the PTY. The color mapping (`Named`/`Indexed` ANSI → the
//! hardcoded jade-dark palette, `Default` → theme fg/bg, inverse-video swap)
//! and the key→bytes encoding are factored into pure, unit-tested functions;
//! the renderer is a thin projection over them.
//!
//! # Many terminals, one panel
//! `JadeApp` keeps a [`TermSession`] per open shell and renders the active one.
//! The header's tab strip (`app::bottom_panel`) switches between them, and this
//! module claims the ⌘T / ⌘W / ⌘⌥←→ / ⌘1…⌘9 chords while the grid holds focus.
//!
//! # jade-term contract (see `crates/jade-term`)
//! - on `TermEvent::Damaged` → `snapshot(id)` → repaint (cached in `JadeApp`);
//! - on `TermEvent::Exited` → a dim `[exited <code>]` line;
//! - `resize(cols, rows)` on panel resize (computed from pixel bounds here);
//! - `write(bytes)` for keystrokes (this module's [`key_to_bytes`]).
//!
//! # Why `[jade]` status lines are NOT echoed here
//! The TS `writeOutput` wrote to the xterm.js *display buffer*. `jade-term`
//! exposes only `write()`, which feeds the **PTY** (i.e. the shell's stdin), so
//! injecting `[jade] …` text there would run it as a shell command, not print
//! it. There is no display-inject API. So build/run status lines stay in the
//! plain output scrollback, kept as the bottom panel's OUTPUT view toggle
//! (`app.rs`), and the TERMINAL view is a real interactive shell.

use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::Arc;

use jade_term::{Cell, Color, GridSnapshot, TermId, TermManager};
use gpui::{
    canvas, div, prelude::*, px, rgb, Bounds, ClipboardItem, Context, Div, FocusHandle, FontWeight,
    MouseButton, MouseDownEvent, MouseMoveEvent, MouseUpEvent, Pixels, ScrollDelta,
    ScrollWheelEvent,
};

use crate::app::JadeApp;

/// Terminal font size; cell width is measured from the real glyph advance at
/// this size (see [`render`]), so the PTY's cols always match what fits.
pub const FONT_SIZE: f32 = 12.0;
/// Fallback cell width if the text system can't resolve the mono font
/// (Menlo/JetBrains Mono 'M' advance at 12px ≈ 7.2px).
pub const CELL_W: f32 = 7.2;
/// Cell height == the per-row line height.
pub const CELL_H: f32 = 20.0;

/// The 16 ANSI colors, ported from `TERMINAL_THEME_DARK`
/// (`src/renderer/panels/terminal-panel.ts:13-46`). Index 0..=7 normal, 8..=15
/// bright; the jade-dark chrome reuses editor hues so program output matches.
pub const ANSI_DARK: [u32; 16] = [
    0x2B2D30, // 0 black
    0xCF6B6B, // 1 red
    0x56B389, // 2 green
    0xD4A76A, // 3 yellow
    0x8DB2FF, // 4 blue
    0x9BB5CF, // 5 magenta
    0x7A9484, // 6 cyan
    0xDFE1E5, // 7 white
    0x6F737A, // 8 bright black
    0xCF6B6B, // 9 bright red
    0x56B389, // 10 bright green
    0xD4A76A, // 11 bright yellow
    0x8DB2FF, // 12 bright blue
    0x9BB5CF, // 13 bright magenta
    0x7A9484, // 14 bright cyan
    0xDFE1E5, // 15 bright white
];

/// The 16 ANSI colors for jade-light, ported from `TERMINAL_THEME_LIGHT`
/// (`src/renderer/panels/terminal-panel.ts:28-40`): the same hues darkened for
/// contrast on the cream background. Swapped in on the theme toggle (§5.2 — a
/// canvas terminal can't read CSS vars, so the palette is chosen in code).
pub const ANSI_LIGHT: [u32; 16] = [
    0x1F2328, // 0 black
    0xB3403F, // 1 red
    0x2E7D5B, // 2 green
    0x9A6700, // 3 yellow
    0x2F5FD0, // 4 blue
    0x6E5494, // 5 magenta
    0x3E6D5D, // 6 cyan
    0xE8EAED, // 7 white
    0x6F737A, // 8 bright black
    0xB3403F, // 9 bright red
    0x2E7D5B, // 10 bright green
    0x9A6700, // 11 bright yellow
    0x2F5FD0, // 12 bright blue
    0x6E5494, // 13 bright magenta
    0x3E6D5D, // 14 bright cyan
    0xF5F6F8, // 15 bright white
];

/// jade-dark terminal default fg / bg (the palette's `foreground`/`background`).
pub const TERM_DEFAULT_FG: u32 = 0xDFE1E5;
pub const TERM_DEFAULT_BG: u32 = 0x1E1F22;

/// The active 16-color ANSI palette for a theme (`is_light` selects the light
/// port). The `Color::Default` fg/bg still come from the `Theme` surfaces.
pub fn ansi_palette(is_light: bool) -> &'static [u32; 16] {
    if is_light {
        &ANSI_LIGHT
    } else {
        &ANSI_DARK
    }
}

/// Resolve one raw [`Color`] into an `0xRRGGBB` value against a specific ANSI
/// `palette`. `Named`/`Indexed` map through it (256-color cube/greyscale computed
/// for 16..=255), `Rgb` passes through, `Default` yields the theme default.
pub fn resolve_color_pal(c: Color, default: u32, palette: &[u32; 16]) -> u32 {
    match c {
        Color::Named(i) => palette.get(i as usize).copied().unwrap_or(default),
        Color::Indexed(i) => indexed_rgb(i, default, palette),
        Color::Rgb(r, g, b) => rgb_u32(r, g, b),
        Color::Default => default,
    }
}

/// [`resolve_color_pal`] against the jade-dark palette (back-compat + tests).
pub fn resolve_color(c: Color, default: u32) -> u32 {
    resolve_color_pal(c, default, &ANSI_DARK)
}

fn rgb_u32(r: u8, g: u8, b: u8) -> u32 {
    ((r as u32) << 16) | ((g as u32) << 8) | b as u32
}

/// xterm 256-color resolution: 0..=15 the ANSI palette, 16..=231 the 6×6×6 RGB
/// cube, 232..=255 the 24-step greyscale ramp.
fn indexed_rgb(i: u8, default: u32, palette: &[u32; 16]) -> u32 {
    match i {
        0..=15 => palette[i as usize],
        16..=231 => {
            let n = i - 16;
            let r = n / 36;
            let g = (n % 36) / 6;
            let b = n % 6;
            let step = |v: u8| if v == 0 { 0 } else { 55 + v * 40 };
            rgb_u32(step(r), step(g), step(b))
        }
        232..=255 => {
            let v = 8 + (i - 232) * 10;
            rgb_u32(v, v, v)
        }
        // exhaustive but keep the default arm for clarity
        #[allow(unreachable_patterns)]
        _ => default,
    }
}

/// Resolve a cell's (fg, bg) against a specific ANSI `palette`, honoring inverse
/// video by swapping. `def_fg`/`def_bg` are the theme defaults for `Color::Default`.
pub fn cell_colors_pal(cell: &Cell, def_fg: u32, def_bg: u32, palette: &[u32; 16]) -> (u32, u32) {
    let fg = resolve_color_pal(cell.fg, def_fg, palette);
    let bg = resolve_color_pal(cell.bg, def_bg, palette);
    if cell.flags.inverse {
        (bg, fg)
    } else {
        (fg, bg)
    }
}

/// [`cell_colors_pal`] against the jade-dark palette (back-compat + tests).
pub fn cell_colors(cell: &Cell, def_fg: u32, def_bg: u32) -> (u32, u32) {
    cell_colors_pal(cell, def_fg, def_bg, &ANSI_DARK)
}

/// Modifier state for [`key_to_bytes`] (a testable subset of `gpui::Modifiers`).
#[derive(Clone, Copy, Default, Debug)]
pub struct KeyMods {
    pub control: bool,
    pub alt: bool,
    pub shift: bool,
    /// cmd (macOS) — reserved for app shortcuts, never sent to the PTY.
    pub platform: bool,
}

/// Encode a GPUI keystroke into the bytes to `write()` to the PTY. Returns
/// `None` for keys that shouldn't reach the shell (cmd-chords, unmapped keys).
///
/// - Enter → `\r`; Shift/Option+Enter → `ESC \r` (Ink/Claude Code reads that
///   as "insert newline" instead of "submit").
/// - Backspace → `0x7f`, Tab → `\t`, Escape → `0x1b`, Space → ` `.
/// - Shift+Tab → `ESC [ Z` (backtab — Claude Code cycles modes with it).
/// - Option+Backspace → `ESC 0x7f` (delete word); Ctrl+Space → NUL.
/// - Arrows/Home/End → CSI (`ESC [ A…`), or SS3 (`ESC O A…`) when the app
///   switched on DECCKM (`app_cursor`, e.g. full-screen TUIs). With
///   Shift/Option/Ctrl held they use the xterm modified form
///   (`ESC [ 1 ; m A…`, m = 1 + shift·1 + alt·2 + ctrl·4).
/// - Delete → `ESC [ 3~` (`ESC [ 3 ; m ~` modified); PageUp/PageDown →
///   `ESC [ 5~`/`ESC [ 6~`.
/// - Ctrl+letter → the C0 control code (`Ctrl+C` → `0x03`).
/// - Otherwise the printable `key_char` (which already reflects Shift), else a
///   single-character `key`.
pub fn key_to_bytes(
    key: &str,
    key_char: Option<&str>,
    mods: KeyMods,
    app_cursor: bool,
) -> Option<Vec<u8>> {
    // cmd-chords (copy/paste/etc.) are app-level, not shell input.
    if mods.platform {
        return None;
    }

    // Ctrl+letter → C0 control byte (Ctrl+C=3, Ctrl+D=4, Ctrl+Z=26, …).
    if mods.control && key.chars().count() == 1 {
        let ch = key.chars().next().unwrap();
        if ch.is_ascii_alphabetic() {
            return Some(vec![(ch.to_ascii_lowercase() as u8) & 0x1f]);
        }
    }

    // DECCKM: cursor keys switch from CSI to SS3 encoding.
    let pre = if app_cursor { b'O' } else { b'[' };
    // xterm modifier parameter: 1 + shift·1 + alt·2 + ctrl·4 (1 = no modifier).
    let modc = 1 + (mods.shift as u8) + ((mods.alt as u8) << 1) + ((mods.control as u8) << 2);
    match key {
        "enter" if mods.shift || mods.alt => return Some(b"\x1b\r".to_vec()),
        "enter" => return Some(vec![b'\r']),
        "backspace" if mods.alt => return Some(vec![0x1b, 0x7f]),
        "backspace" => return Some(vec![0x7f]),
        "tab" if mods.shift => return Some(b"\x1b[Z".to_vec()),
        "tab" => return Some(vec![b'\t']),
        "escape" => return Some(vec![0x1b]),
        "space" if mods.control => return Some(vec![0x00]),
        "space" => return Some(vec![b' ']),
        "up" | "down" | "right" | "left" | "home" | "end" => {
            let ch = match key {
                "up" => b'A',
                "down" => b'B',
                "right" => b'C',
                "left" => b'D',
                "home" => b'H',
                _ => b'F',
            };
            // Modified cursor keys always use the CSI form, even under DECCKM.
            return Some(if modc > 1 {
                format!("\x1b[1;{}{}", modc, ch as char).into_bytes()
            } else {
                vec![0x1b, pre, ch]
            });
        }
        "delete" if modc > 1 => return Some(format!("\x1b[3;{modc}~").into_bytes()),
        "delete" => return Some(b"\x1b[3~".to_vec()),
        "pageup" => return Some(b"\x1b[5~".to_vec()),
        "pagedown" => return Some(b"\x1b[6~".to_vec()),
        _ => {}
    }

    // Printable input (no control chord). key_char already accounts for Shift.
    if !mods.control {
        if let Some(kc) = key_char {
            if !kc.is_empty() {
                return Some(kc.as_bytes().to_vec());
            }
        }
        if key.chars().count() == 1 {
            return Some(key.as_bytes().to_vec());
        }
    }
    None
}

/// Encode clipboard text for the PTY (⌘V). Newlines become `\r` (what a
/// terminal keyboard sends); in bracketed-paste mode the text is wrapped in
/// `ESC [ 200~` … `ESC [ 201~` with any embedded `ESC` stripped so the paste
/// can't jade the closing marker (or any other control sequence).
pub fn paste_bytes(text: &str, bracketed: bool) -> Vec<u8> {
    let normalized = text.replace("\r\n", "\r").replace('\n', "\r");
    if bracketed {
        let clean: String = normalized.chars().filter(|&c| c != '\x1b').collect();
        let mut out = b"\x1b[200~".to_vec();
        out.extend_from_slice(clean.as_bytes());
        out.extend_from_slice(b"\x1b[201~");
        out
    } else {
        normalized.into_bytes()
    }
}

/// A mouse selection over the terminal grid, in logical (row, col) cells of the
/// combined `scrollback ++ viewport` buffer (row 0 = oldest scrollback line).
/// `anchor` is where the drag started, `head` where it currently is; either may
/// come first in the buffer. Both endpoints are inclusive.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TermSelection {
    pub anchor: (usize, usize),
    pub head: (usize, usize),
}

impl TermSelection {
    /// (start, end) in buffer order, both inclusive.
    pub fn ordered(&self) -> ((usize, usize), (usize, usize)) {
        if self.anchor <= self.head {
            (self.anchor, self.head)
        } else {
            (self.head, self.anchor)
        }
    }

    /// The inclusive column range this selection covers on logical `row` (a row
    /// of `cols` cells), or `None` when the row is outside the selection.
    /// Interior rows of a multi-row selection are covered edge to edge.
    pub fn row_range(&self, row: usize, cols: usize) -> Option<(usize, usize)> {
        let (a, b) = self.ordered();
        if row < a.0 || row > b.0 || cols == 0 {
            return None;
        }
        let c0 = if row == a.0 { a.1 } else { 0 };
        let c1 = if row == b.0 { b.1.min(cols - 1) } else { cols - 1 };
        (c0 <= c1).then_some((c0, c1))
    }
}

/// One open terminal: its PTY id plus the view state that belongs to that
/// terminal alone. The panel renders the active session; the others keep their
/// snapshot, scrollback offset, and selection while they wait in the tab strip.
pub struct TermSession {
    /// PTY id handed out by [`TermManager`].
    pub id: TermId,
    /// Tab label — the base name of the directory the shell started in.
    pub title: String,
    /// This terminal's slot in the theme's `series` palette, which colors its
    /// segment of the header bar. Assigned by [`next_color`] and never changes,
    /// so a terminal keeps its color when its neighbors close.
    pub color: usize,
    /// Latest grid snapshot (refreshed on `Damaged` while this session is the
    /// active one; rendered by [`render`]).
    pub snapshot: Option<GridSnapshot>,
    /// Set once the child exits — renders the dim `[exited <code>]` line. The
    /// tab stays until the user closes it.
    pub exited: bool,
    pub exit_code: Option<i32>,
    /// Rows scrolled up into scrollback (0 = pinned to the live bottom).
    /// Scroll-wheel-up increases it; typing / new snapshots pin back.
    pub scroll_back: usize,
    /// Mouse selection over the grid, in logical (row, col) cells of the
    /// combined `scrollback ++ viewport` buffer. ⌘C copies it as text.
    pub sel: Option<TermSelection>,
    /// True while the left button is down extending `sel`.
    pub sel_dragging: bool,
    /// The shell wrote while this terminal was in the background. The tab marks
    /// it until the user switches to it.
    pub activity: bool,
}

impl TermSession {
    /// A fresh session for the terminal `id`, before its first snapshot.
    pub fn new(id: TermId, title: String, color: usize) -> Self {
        Self {
            id,
            title,
            color,
            snapshot: None,
            exited: false,
            exit_code: None,
            scroll_back: 0,
            sel: None,
            sel_dragging: false,
            activity: false,
        }
    }
}

/// The palette slot a new terminal takes, given the slots the open terminals
/// already hold and a palette of `len` colors: the lowest free slot, so the bar
/// stays as colorful as it can and a re-opened slot is re-used. Past `len` open
/// terminals the colors have to repeat, and it wraps.
pub fn next_color(used: &[usize], len: usize) -> usize {
    if len == 0 {
        return 0;
    }
    (0..len)
        .find(|slot| !used.contains(slot))
        .unwrap_or(used.len() % len)
}

/// The tab `delta` steps along a strip of `len` tabs, wrapping at both ends
/// (⌘⌥← / ⌘⌥→). `None` when there is nothing to move to.
pub fn wrap_index(current: usize, len: usize, delta: i32) -> Option<usize> {
    if len < 2 {
        return None;
    }
    Some((current as i32 + delta).rem_euclid(len as i32) as usize)
}

/// The tab ⌘`nth` selects out of `len` open tabs, counting from 1. As on macOS,
/// 9 means the last tab however many are open. `None` for a tab that is not
/// open.
pub fn nth_index(nth: usize, len: usize) -> Option<usize> {
    if len == 0 || nth == 0 {
        return None;
    }
    if nth == 9 {
        return Some(len - 1);
    }
    (nth <= len).then_some(nth - 1)
}

/// Extract the selected cells as plain text: one line per logical row, each
/// trimmed of trailing whitespace (grid rows are space-padded to `cols`), joined
/// with `\n`.
pub fn selection_text(snap: &GridSnapshot, sel: &TermSelection) -> String {
    let n = snap.scrollback.len();
    let (a, b) = sel.ordered();
    let mut lines = Vec::new();
    for row in a.0..=b.0 {
        let cells = if row < n {
            snap.scrollback.get(row)
        } else {
            snap.cells.get(row - n)
        };
        let Some(cells) = cells else { break };
        let Some((c0, c1)) = sel.row_range(row, cells.len()) else { continue };
        let line: String = cells[c0..=c1].iter().map(|c| c.ch).collect();
        lines.push(line.trim_end().to_string());
    }
    lines.join("\n")
}

/// Map a mouse position (already relative to the grid origin) to the logical
/// (row, col) cell under it, clamped to the `rows`×`cols` window starting at
/// logical row `top`.
pub fn hit_cell(x: f32, y: f32, cell_w: f32, top: usize, rows: usize, cols: usize) -> (usize, usize) {
    let col = ((x / cell_w).floor().max(0.0) as usize).min(cols.saturating_sub(1));
    let disp = ((y / CELL_H).floor().max(0.0) as usize).min(rows.saturating_sub(1));
    (top + disp, col)
}

/// Selection wash strength: the selected cells' bg is blended 25% toward the
/// theme accent (the editor uses a 15% accent wash; the terminal needs a bit
/// more to read over colored program output).
const SEL_ALPHA: f32 = 0.25;

/// Blend `over` onto `base` (both `0xRRGGBB`) at alpha `a`, returning opaque
/// `0xRRGGBB` (runs render solid colors, so the wash is pre-composited).
pub fn blend(base: u32, over: u32, a: f32) -> u32 {
    let ch = |shift: u32| {
        let b = ((base >> shift) & 0xff) as f32;
        let o = ((over >> shift) & 0xff) as f32;
        (b + (o - b) * a) as u32 & 0xff
    };
    (ch(16) << 16) | (ch(8) << 8) | ch(0)
}

/// A maximal run of adjacent cells sharing fg/bg/weight — rendered as one span
/// so a row is a handful of divs, not one-per-column.
struct Run {
    text: String,
    fg: u32,
    bg: u32,
    bold: bool,
}

fn row_runs(
    cells: &[Cell],
    def_fg: u32,
    def_bg: u32,
    palette: &[u32; 16],
    sel: Option<(usize, usize)>,
    sel_tint: u32,
) -> Vec<Run> {
    let mut runs: Vec<Run> = Vec::new();
    for (i, cell) in cells.iter().enumerate() {
        let (fg, mut bg) = cell_colors_pal(cell, def_fg, def_bg, palette);
        if sel.is_some_and(|(c0, c1)| i >= c0 && i <= c1) {
            bg = blend(bg, sel_tint, SEL_ALPHA);
        }
        let bold = cell.flags.bold;
        match runs.last_mut() {
            Some(r) if r.fg == fg && r.bg == bg && r.bold == bold => r.text.push(cell.ch),
            _ => runs.push(Run {
                text: cell.ch.to_string(),
                fg,
                bg,
                bold,
            }),
        }
    }
    runs
}

/// Render the active terminal's body: a resize-measuring canvas underlay plus
/// the grid rows. Focusable so `on_key_down` reaches [`key_to_bytes`] →
/// `write()`. Renders empty until the first [`TermSession`] opens.
pub fn render(app: &JadeApp, handle: FocusHandle, cx: &mut Context<JadeApp>) -> impl IntoElement {
    let theme = app.theme.clone();
    let def_fg = theme.text;
    let def_bg = theme.bg;
    let palette = ansi_palette(theme.is_light);

    // Measure the mono font's real advance so the cols reported to the PTY
    // match what the rows actually render at (a hardcoded width drifts per
    // font, and TUIs right-align/rule against the reported width).
    let font = gpui::font(crate::fonts::mono_family());
    let font_id = cx.text_system().resolve_font(&font);
    let cell_w = cx
        .text_system()
        .advance(font_id, px(FONT_SIZE), 'M')
        .map(|a| f32::from(a.width))
        .unwrap_or(CELL_W);

    let body = div()
        .id("terminal-body")
        .relative()
        .flex_1()
        .w_full()
        .overflow_hidden()
        .bg(rgb(theme.bg))
        .track_focus(&handle)
        .font_family(crate::fonts::mono_family())
        .text_size(px(FONT_SIZE))
        .line_height(px(CELL_H));

    // Focus on click so keystrokes flow to this panel, and start a mouse
    // selection at the cell under the cursor (drag extends it, ⌘C copies).
    let focus = handle.clone();
    let origin_down = app.term_origin.clone();
    let body = body.cursor_text().on_mouse_down(
        MouseButton::Left,
        cx.listener(move |app: &mut JadeApp, ev: &MouseDownEvent, window, cx| {
            window.focus(&focus, cx);
            let cell = cell_at(app, &origin_down, cell_w, ev.position);
            if let (Some(cell), Some(session)) = (cell, app.active_term_mut()) {
                session.sel = Some(TermSelection { anchor: cell, head: cell });
                session.sel_dragging = true;
                cx.notify();
            }
        }),
    );

    // Drag extends the selection head; releasing the button ends the drag, and
    // a plain click (no movement past the start cell) leaves no selection.
    let origin_move = app.term_origin.clone();
    let body = body.on_mouse_move(cx.listener(
        move |app: &mut JadeApp, ev: &MouseMoveEvent, _w, cx| {
            if !app.active_term().is_some_and(|s| s.sel_dragging) {
                return;
            }
            if ev.pressed_button != Some(MouseButton::Left) {
                if let Some(session) = app.active_term_mut() {
                    session.sel_dragging = false;
                }
                return;
            }
            let cell = cell_at(app, &origin_move, cell_w, ev.position);
            let Some(session) = app.active_term_mut() else { return };
            if let (Some(cell), Some(sel)) = (cell, session.sel.as_mut()) {
                if sel.head != cell {
                    sel.head = cell;
                    cx.notify();
                }
            }
        },
    ));
    let body = body.on_mouse_up(
        MouseButton::Left,
        cx.listener(|app: &mut JadeApp, _ev: &MouseUpEvent, _w, cx| {
            let Some(session) = app.active_term_mut() else { return };
            if session.sel_dragging {
                session.sel_dragging = false;
                if session.sel.is_some_and(|s| s.anchor == s.head) {
                    session.sel = None;
                }
                cx.notify();
            }
        }),
    );

    // Keystrokes → PTY bytes. The terminal modes ride along from the latest
    // snapshot: DECCKM flips the arrow-key encoding, bracketed paste wraps ⌘V
    // payloads. Any keystroke also pins the view back to the live bottom (like
    // every terminal: typing jumps you out of scrollback).
    let body = body.on_key_down(cx.listener(|app: &mut JadeApp, ev: &gpui::KeyDownEvent, _window, cx| {
        let Some(id) = app.active_term().map(|s| s.id) else { return };
        let ks = &ev.keystroke;
        let mods = KeyMods {
            control: ks.modifiers.control,
            alt: ks.modifiers.alt,
            shift: ks.modifiers.shift,
            platform: ks.modifiers.platform,
        };
        let snap = app.active_term().and_then(|s| s.snapshot.as_ref());
        let bracketed = snap.is_some_and(|s| s.bracketed_paste);
        let app_cursor = snap.is_some_and(|s| s.app_cursor);
        // Terminal-tab chords. Safe to claim here: `key_to_bytes` drops every
        // cmd chord, so none of these ever reached the shell.
        if mods.platform && !mods.control {
            match ks.key.as_str() {
                "t" => {
                    app.action_new_terminal();
                    cx.notify();
                    return;
                }
                "w" => {
                    app.action_close_terminal(id, cx);
                    cx.notify();
                    return;
                }
                "left" if mods.alt => {
                    app.cycle_terminal(-1);
                    cx.notify();
                    return;
                }
                "right" if mods.alt => {
                    app.cycle_terminal(1);
                    cx.notify();
                    return;
                }
                // ⌘1…⌘8 jump to that tab, ⌘9 to the last one (macOS tab order).
                d if !mods.alt && matches!(d, "1" | "2" | "3" | "4" | "5" | "6" | "7" | "8" | "9") => {
                    app.select_terminal_at(d.parse::<usize>().unwrap_or(1));
                    cx.notify();
                    return;
                }
                _ => {}
            }
        }
        // ⌘C copies the mouse selection (Ctrl+C stays the shell interrupt).
        if mods.platform && ks.key == "c" {
            let session = app.active_term();
            let sel = session.and_then(|s| s.sel);
            if let (Some(sel), Some(snap)) = (sel, session.and_then(|s| s.snapshot.as_ref())) {
                let text = selection_text(snap, &sel);
                if !text.is_empty() {
                    cx.write_to_clipboard(ClipboardItem::new_string(text));
                }
            }
            return;
        }
        // ⌘V pastes into the shell (Claude Code and any REPL need this).
        if mods.platform && ks.key == "v" {
            if let Some(text) = cx.read_from_clipboard().and_then(|i| i.text()) {
                app.term.write(id, &paste_bytes(&text, bracketed));
                app.pin_terminal_to_bottom();
                cx.notify();
            }
            return;
        }
        if let Some(bytes) = key_to_bytes(&ks.key, ks.key_char.as_deref(), mods, app_cursor) {
            app.term.write(id, &bytes);
            app.pin_terminal_to_bottom();
            cx.notify();
        }
    }));

    // Scroll wheel walks up/down through the scrollback window. One wheel line
    // is one grid row; clamped to the available scrollback at render time.
    let scroll_max = app
        .active_term()
        .and_then(|s| s.snapshot.as_ref())
        .map(|s| s.scrollback.len())
        .unwrap_or(0);
    let body = body.on_scroll_wheel(cx.listener(move |app: &mut JadeApp, ev: &ScrollWheelEvent, _w, cx| {
        let dy = match ev.delta {
            ScrollDelta::Lines(p) => p.y,
            ScrollDelta::Pixels(p) => f32::from(p.y) / CELL_H,
        };
        // Wheel-up (positive dy) reveals older lines; wheel-down returns to live.
        let rows = dy.round() as i64;
        let Some(session) = app.active_term_mut() else { return };
        let next = (session.scroll_back as i64 + rows).clamp(0, scroll_max as i64);
        if next != session.scroll_back as i64 {
            session.scroll_back = next as usize;
            cx.notify();
        }
    }));

    // Resize underlay: derive cols×rows from bounds, resize only on change.
    let resize = resize_canvas(
        app.term.clone(),
        app.active_term().map(|s| s.id),
        app.term_last_size.clone(),
        app.term_origin.clone(),
        cell_w,
    );

    let mut grid = div().absolute().top_0().left_0().flex().flex_col();
    let mut cursor_block = None;
    let session = app.active_term();
    if let Some(snap) = session.and_then(|s| s.snapshot.as_ref()) {
        // Window `rows` display lines out of `scrollback ++ cells`, offset up
        // from the live bottom by `term_scroll_back` (clamped). At offset 0 the
        // window is exactly the viewport; scrolling up reveals scrollback.
        let n = snap.scrollback.len();
        let rows = snap.rows;
        let back = session.map(|s| s.scroll_back).unwrap_or(0).min(n);
        // Logical index of the first displayed row (0 = oldest scrollback line).
        let top = n.saturating_sub(back);
        grid = render_window(
            grid,
            snap,
            top,
            rows,
            def_fg,
            def_bg,
            palette,
            session.and_then(|s| s.sel),
            theme.accent,
        );
        // Block cursor: fg-colored cell with the covered glyph in bg color.
        // Ink/Claude Code position the real cursor in the input box; without
        // this there is no visible caret at all. Its display row shifts down by
        // the scrollback offset, and it hides once scrolled off the top.
        if snap.cursor.visible && !session.is_some_and(|s| s.exited) {
            let cur = snap.cursor;
            let disp_row = cur.line as isize + back as isize;
            if disp_row >= 0 && (disp_row as usize) < rows {
                let ch = snap
                    .cells
                    .get(cur.line)
                    .and_then(|row| row.get(cur.col))
                    .map(|c| c.ch)
                    .unwrap_or(' ');
                cursor_block = Some(
                    div()
                        .absolute()
                        .left(px(cur.col as f32 * cell_w))
                        .top(px(disp_row as f32 * CELL_H))
                        .w(px(cell_w))
                        .h(px(CELL_H))
                        .bg(rgb(def_fg))
                        .text_color(rgb(def_bg))
                        .child(ch.to_string()),
                );
            }
        }
    }
    if session.is_some_and(|s| s.exited) {
        let code = session
            .and_then(|s| s.exit_code)
            .map(|c| c.to_string())
            .unwrap_or_else(|| "signal".to_string());
        grid = grid.child(
            div()
                .text_color(rgb(theme.muted))
                .child(format!("[exited {code}]")),
        );
    }

    body.child(resize).child(grid).children(cursor_block)
}

/// Map a window-space mouse position to the logical cell under it, using the
/// grid origin the resize canvas recorded (`term_origin`) and the same window
/// placement math as the renderer. `None` before the first snapshot.
fn cell_at(
    app: &JadeApp,
    origin: &Arc<AtomicU64>,
    cell_w: f32,
    pos: gpui::Point<Pixels>,
) -> Option<(usize, usize)> {
    let session = app.active_term()?;
    let snap = session.snapshot.as_ref()?;
    let packed = origin.load(Ordering::Relaxed);
    let ox = f32::from_bits((packed >> 32) as u32);
    let oy = f32::from_bits(packed as u32);
    let n = snap.scrollback.len();
    let top = n - session.scroll_back.min(n);
    Some(hit_cell(
        f32::from(pos.x) - ox,
        f32::from(pos.y) - oy,
        cell_w,
        top,
        snap.rows,
        snap.cols,
    ))
}

/// Render `count` display rows starting at logical index `top` of the combined
/// `scrollback ++ cells` buffer (0 = oldest scrollback line). Rows past the end
/// of the viewport render blank (the buffer never has fewer than the viewport).
/// Cells inside `sel` get their bg washed toward `sel_tint` (see [`SEL_ALPHA`]).
#[allow(clippy::too_many_arguments)]
fn render_window(
    mut grid: Div,
    snap: &GridSnapshot,
    top: usize,
    count: usize,
    def_fg: u32,
    def_bg: u32,
    palette: &[u32; 16],
    sel: Option<TermSelection>,
    sel_tint: u32,
) -> Div {
    let n = snap.scrollback.len();
    for i in 0..count {
        let idx = top + i;
        let row = if idx < n {
            snap.scrollback.get(idx)
        } else {
            snap.cells.get(idx - n)
        };
        let mut line = div().flex().flex_row().h(px(CELL_H)).items_center();
        if let Some(row) = row {
            let row_sel = sel.and_then(|s| s.row_range(idx, row.len()));
            for r in row_runs(row, def_fg, def_bg, palette, row_sel, sel_tint) {
                let mut span = div().text_color(rgb(r.fg)).child(r.text);
                if r.bg != def_bg {
                    span = span.bg(rgb(r.bg));
                }
                if r.bold {
                    span = span.font_weight(FontWeight::BOLD);
                }
                line = line.child(span);
            }
        }
        grid = grid.child(line);
    }
    grid
}

fn resize_canvas(
    manager: Arc<TermManager>,
    id: Option<TermId>,
    last: Arc<AtomicU32>,
    origin: Arc<AtomicU64>,
    cell_w: f32,
) -> impl IntoElement {
    canvas(
        move |_, _, _| {},
        move |bounds: Bounds<Pixels>, _, _window, _| {
            // Record the grid origin in window px so the mouse-selection
            // listeners can map event positions to cells (the canvas fills the
            // body, and the grid is painted from its top-left).
            let ox = f32::from(bounds.origin.x).to_bits() as u64;
            let oy = f32::from(bounds.origin.y).to_bits() as u64;
            origin.store((ox << 32) | oy, Ordering::Relaxed);
            let Some(id) = id else { return };
            let w = f32::from(bounds.size.width);
            let h = f32::from(bounds.size.height);
            if w <= 0.0 || h <= 0.0 {
                return;
            }
            let cols = ((w / cell_w).floor() as u16).max(2);
            let rows = ((h / CELL_H).floor() as u16).max(2);
            let packed = ((cols as u32) << 16) | rows as u32;
            // Resize only when the derived geometry actually changed, so the
            // Damaged→snapshot→repaint loop this triggers can't run away.
            if last.swap(packed, Ordering::Relaxed) != packed {
                manager.resize(id, cols, rows);
            }
        },
    )
    .absolute()
    .size_full()
}

#[cfg(test)]
mod tests {
    use super::*;
    use jade_term::CellFlags;

    fn cell(ch: char, fg: Color, bg: Color, inverse: bool) -> Cell {
        Cell {
            ch,
            fg,
            bg,
            flags: CellFlags {
                inverse,
                ..Default::default()
            },
        }
    }

    #[test]
    fn named_and_default_colors() {
        assert_eq!(resolve_color(Color::Named(1), TERM_DEFAULT_FG), 0xCF6B6B); // red
        assert_eq!(resolve_color(Color::Named(2), TERM_DEFAULT_FG), 0x56B389); // green
        assert_eq!(resolve_color(Color::Named(4), TERM_DEFAULT_FG), 0x8DB2FF); // blue
        // Default resolves to whatever theme default is supplied.
        assert_eq!(resolve_color(Color::Default, TERM_DEFAULT_FG), TERM_DEFAULT_FG);
        assert_eq!(resolve_color(Color::Default, 0x123456), 0x123456);
    }

    #[test]
    fn indexed_and_rgb_colors() {
        // Low indices alias the ANSI palette.
        assert_eq!(resolve_color(Color::Indexed(1), TERM_DEFAULT_FG), 0xCF6B6B);
        // Cube corner 16 = (0,0,0); 231 = (255,255,255).
        assert_eq!(resolve_color(Color::Indexed(16), TERM_DEFAULT_FG), 0x000000);
        assert_eq!(resolve_color(Color::Indexed(231), TERM_DEFAULT_FG), 0xFFFFFF);
        // Greyscale ramp start.
        assert_eq!(resolve_color(Color::Indexed(232), TERM_DEFAULT_FG), 0x080808);
        // Truecolor passes through.
        assert_eq!(resolve_color(Color::Rgb(0x12, 0x34, 0x56), 0), 0x123456);
    }

    #[test]
    fn inverse_swaps_fg_bg() {
        let c = cell('x', Color::Named(1), Color::Default, false);
        assert_eq!(cell_colors(&c, TERM_DEFAULT_FG, TERM_DEFAULT_BG), (0xCF6B6B, TERM_DEFAULT_BG));
        let c = cell('x', Color::Named(1), Color::Default, true);
        assert_eq!(cell_colors(&c, TERM_DEFAULT_FG, TERM_DEFAULT_BG), (TERM_DEFAULT_BG, 0xCF6B6B));
    }

    #[test]
    fn key_encoding_specials() {
        let m = KeyMods::default();
        assert_eq!(key_to_bytes("enter", Some("\n"), m, false), Some(vec![b'\r']));
        assert_eq!(key_to_bytes("backspace", None, m, false), Some(vec![0x7f]));
        assert_eq!(key_to_bytes("tab", Some("\t"), m, false), Some(vec![b'\t']));
        assert_eq!(key_to_bytes("escape", None, m, false), Some(vec![0x1b]));
        assert_eq!(key_to_bytes("space", Some(" "), m, false), Some(vec![b' ']));
        assert_eq!(key_to_bytes("delete", None, m, false), Some(b"\x1b[3~".to_vec()));
        assert_eq!(key_to_bytes("pageup", None, m, false), Some(b"\x1b[5~".to_vec()));
        assert_eq!(key_to_bytes("pagedown", None, m, false), Some(b"\x1b[6~".to_vec()));
    }

    #[test]
    fn key_encoding_modified_specials() {
        let shift = KeyMods { shift: true, ..Default::default() };
        let alt = KeyMods { alt: true, ..Default::default() };
        let ctrl = KeyMods { control: true, ..Default::default() };
        // Shift+Tab is backtab (CSI Z) — Claude Code cycles its modes with it.
        assert_eq!(key_to_bytes("tab", None, shift, false), Some(b"\x1b[Z".to_vec()));
        // Option+Backspace deletes the previous word (ESC DEL).
        assert_eq!(key_to_bytes("backspace", None, alt, false), Some(vec![0x1b, 0x7f]));
        // Ctrl+Space is NUL (readline set-mark, tmux default prefix alt).
        assert_eq!(key_to_bytes("space", None, ctrl, false), Some(vec![0x00]));
        // Modified arrows use the xterm form, even under DECCKM.
        assert_eq!(key_to_bytes("up", None, shift, false), Some(b"\x1b[1;2A".to_vec()));
        assert_eq!(key_to_bytes("left", None, alt, true), Some(b"\x1b[1;3D".to_vec()));
        assert_eq!(key_to_bytes("right", None, ctrl, false), Some(b"\x1b[1;5C".to_vec()));
        let ctrl_shift = KeyMods { control: true, shift: true, ..Default::default() };
        assert_eq!(key_to_bytes("end", None, ctrl_shift, false), Some(b"\x1b[1;6F".to_vec()));
        // Modified forward-delete.
        assert_eq!(key_to_bytes("delete", None, shift, false), Some(b"\x1b[3;2~".to_vec()));
    }

    #[test]
    fn shift_or_alt_enter_is_newline() {
        // Ink/Claude Code treat ESC CR as "insert newline, don't submit".
        let shift = KeyMods { shift: true, ..Default::default() };
        let alt = KeyMods { alt: true, ..Default::default() };
        assert_eq!(key_to_bytes("enter", Some("\n"), shift, false), Some(b"\x1b\r".to_vec()));
        assert_eq!(key_to_bytes("enter", Some("\n"), alt, false), Some(b"\x1b\r".to_vec()));
    }

    #[test]
    fn key_encoding_arrows() {
        let m = KeyMods::default();
        assert_eq!(key_to_bytes("up", None, m, false), Some(b"\x1b[A".to_vec()));
        assert_eq!(key_to_bytes("down", None, m, false), Some(b"\x1b[B".to_vec()));
        assert_eq!(key_to_bytes("right", None, m, false), Some(b"\x1b[C".to_vec()));
        assert_eq!(key_to_bytes("left", None, m, false), Some(b"\x1b[D".to_vec()));
        // DECCKM (application cursor keys) switches to SS3.
        assert_eq!(key_to_bytes("up", None, m, true), Some(b"\x1bOA".to_vec()));
        assert_eq!(key_to_bytes("down", None, m, true), Some(b"\x1bOB".to_vec()));
        assert_eq!(key_to_bytes("home", None, m, true), Some(b"\x1bOH".to_vec()));
        assert_eq!(key_to_bytes("end", None, m, true), Some(b"\x1bOF".to_vec()));
    }

    #[test]
    fn key_encoding_ctrl_and_printable() {
        let ctrl = KeyMods {
            control: true,
            ..Default::default()
        };
        assert_eq!(key_to_bytes("c", Some("c"), ctrl, false), Some(vec![0x03])); // Ctrl+C
        assert_eq!(key_to_bytes("d", Some("d"), ctrl, false), Some(vec![0x04])); // Ctrl+D

        let m = KeyMods::default();
        assert_eq!(key_to_bytes("a", Some("a"), m, false), Some(b"a".to_vec()));
        assert_eq!(key_to_bytes("!", Some("!"), m, false), Some(b"!".to_vec())); // shifted char

        // cmd-chords are not shell input.
        let cmd = KeyMods {
            platform: true,
            ..Default::default()
        };
        assert_eq!(key_to_bytes("c", Some("c"), cmd, false), None);
    }

    #[test]
    fn paste_encoding() {
        // Plain paste: newlines become CR, like a terminal keyboard.
        assert_eq!(paste_bytes("ab\ncd\r\nef", false), b"ab\rcd\ref".to_vec());
        // Bracketed paste wraps and strips embedded ESC (marker forgery).
        assert_eq!(
            paste_bytes("hi\x1b[201~there\n!", true),
            b"\x1b[200~hi[201~there\r!\x1b[201~".to_vec()
        );
    }

    #[test]
    fn row_runs_group_adjacent() {
        let cells = vec![
            cell('a', Color::Named(1), Color::Default, false),
            cell('b', Color::Named(1), Color::Default, false),
            cell('c', Color::Named(2), Color::Default, false),
        ];
        let runs = row_runs(&cells, TERM_DEFAULT_FG, TERM_DEFAULT_BG, &ANSI_DARK, None, 0);
        assert_eq!(runs.len(), 2);
        assert_eq!(runs[0].text, "ab");
        assert_eq!(runs[1].text, "c");
    }

    #[test]
    fn selection_splits_runs_with_washed_bg() {
        let cells = vec![
            cell('a', Color::Default, Color::Default, false),
            cell('b', Color::Default, Color::Default, false),
            cell('c', Color::Default, Color::Default, false),
        ];
        let tint = 0x56B389;
        let runs = row_runs(
            &cells,
            TERM_DEFAULT_FG,
            TERM_DEFAULT_BG,
            &ANSI_DARK,
            Some((1, 1)),
            tint,
        );
        // 'a' | selected 'b' | 'c' — the selected run's bg is the blend.
        assert_eq!(runs.len(), 3);
        assert_eq!(runs[0].bg, TERM_DEFAULT_BG);
        assert_eq!(runs[1].bg, blend(TERM_DEFAULT_BG, tint, SEL_ALPHA));
        assert_ne!(runs[1].bg, TERM_DEFAULT_BG);
        assert_eq!(runs[2].bg, TERM_DEFAULT_BG);
    }

    fn text_row(s: &str, cols: usize) -> Vec<Cell> {
        let mut row: Vec<Cell> = s
            .chars()
            .map(|ch| cell(ch, Color::Default, Color::Default, false))
            .collect();
        row.resize(cols, cell(' ', Color::Default, Color::Default, false));
        row
    }

    fn snap_with(scrollback: &[&str], viewport: &[&str], cols: usize) -> GridSnapshot {
        GridSnapshot {
            cols,
            rows: viewport.len(),
            cursor: jade_term::Cursor { col: 0, line: 0, visible: false },
            cells: viewport.iter().map(|s| text_row(s, cols)).collect(),
            scrollback: scrollback.iter().map(|s| text_row(s, cols)).collect(),
            app_cursor: false,
            bracketed_paste: false,
        }
    }

    #[test]
    fn selection_row_range() {
        // Backwards drag (head before anchor) normalizes.
        let sel = TermSelection { anchor: (2, 3), head: (0, 5) };
        assert_eq!(sel.row_range(0, 10), Some((5, 9))); // first row: head col → edge
        assert_eq!(sel.row_range(1, 10), Some((0, 9))); // interior: full width
        assert_eq!(sel.row_range(2, 10), Some((0, 3))); // last row: edge → anchor col
        assert_eq!(sel.row_range(3, 10), None); // outside
        // Single-row selection.
        let sel = TermSelection { anchor: (1, 4), head: (1, 2) };
        assert_eq!(sel.row_range(1, 10), Some((2, 4)));
    }

    #[test]
    fn selection_text_spans_scrollback_and_viewport() {
        let snap = snap_with(&["old line"], &["hello world", "second"], 12);
        // From scrollback col 4 through viewport row 0 col 4 (inclusive).
        let sel = TermSelection { anchor: (0, 4), head: (1, 4) };
        assert_eq!(selection_text(&snap, &sel), "line\nhello");
        // Trailing pad cells are trimmed on full-width interior rows.
        let sel = TermSelection { anchor: (0, 0), head: (2, 5) };
        assert_eq!(selection_text(&snap, &sel), "old line\nhello world\nsecond");
        // Backwards drag yields the same text.
        let rev = TermSelection { anchor: (2, 5), head: (0, 0) };
        assert_eq!(selection_text(&snap, &rev), "old line\nhello world\nsecond");
    }

    #[test]
    fn hit_cell_maps_and_clamps() {
        // 8px cells, 20px rows, window starting at logical row 10, 24×80 grid.
        assert_eq!(hit_cell(0.0, 0.0, 8.0, 10, 24, 80), (10, 0));
        assert_eq!(hit_cell(17.0, 45.0, 8.0, 10, 24, 80), (12, 2));
        // Negative (above/left of the grid) clamps to the window start.
        assert_eq!(hit_cell(-5.0, -5.0, 8.0, 10, 24, 80), (10, 0));
        // Past the right/bottom edge clamps to the last cell.
        assert_eq!(hit_cell(10_000.0, 10_000.0, 8.0, 10, 24, 80), (33, 79));
    }

    #[test]
    fn light_ansi_mapping() {
        let light = ansi_palette(true);
        assert_eq!(light, &ANSI_LIGHT);
        // Named colors resolve through the light palette (darkened hues).
        assert_eq!(resolve_color_pal(Color::Named(1), 0, light), 0xB3403F); // red
        assert_eq!(resolve_color_pal(Color::Named(2), 0, light), 0x2E7D5B); // green
        assert_eq!(resolve_color_pal(Color::Named(4), 0, light), 0x2F5FD0); // blue
        // ANSI black (index 0) is the dark charcoal fg on light, not near-black bg.
        assert_eq!(resolve_color_pal(Color::Named(0), 0, light), 0x1F2328);
        // Indexed 0..=15 also route through the light palette.
        assert_eq!(resolve_color_pal(Color::Indexed(3), 0, light), 0x9A6700); // yellow
        // Dark palette still selected when not light.
        assert_eq!(ansi_palette(false), &ANSI_DARK);
        assert_eq!(resolve_color_pal(Color::Named(1), 0, &ANSI_DARK), 0xCF6B6B);
        // Default falls back to the supplied theme default regardless of palette.
        assert_eq!(resolve_color_pal(Color::Default, 0x123456, light), 0x123456);
    }

    #[test]
    fn tab_cycling_wraps_both_ways() {
        // One terminal (or none) has nowhere to go — ⌘⌥←→ must not move.
        assert_eq!(wrap_index(0, 0, 1), None);
        assert_eq!(wrap_index(0, 1, 1), None);
        assert_eq!(wrap_index(0, 1, -1), None);
        // Forward, then over the end and back to the first tab.
        assert_eq!(wrap_index(0, 3, 1), Some(1));
        assert_eq!(wrap_index(1, 3, 1), Some(2));
        assert_eq!(wrap_index(2, 3, 1), Some(0));
        // Backward, then off the front and around to the last tab.
        assert_eq!(wrap_index(2, 3, -1), Some(1));
        assert_eq!(wrap_index(0, 3, -1), Some(2));
    }

    #[test]
    fn colors_fill_the_lowest_free_slot_and_survive_a_close() {
        // Terminals opened in order take the palette in order.
        assert_eq!(next_color(&[], 5), 0);
        assert_eq!(next_color(&[0], 5), 1);
        assert_eq!(next_color(&[0, 1], 5), 2);
        // Closing the middle terminal frees its slot, and the terminals that
        // stay open keep the colors they had.
        assert_eq!(next_color(&[0, 2], 5), 1);
        // Past the palette the colors have to repeat.
        assert_eq!(next_color(&[0, 1, 2, 3, 4], 5), 0);
        assert_eq!(next_color(&[0, 1, 2, 3, 4, 0], 5), 1);
    }

    #[test]
    fn digit_shortcuts_count_from_one_and_nine_is_last() {
        assert_eq!(nth_index(1, 3), Some(0));
        assert_eq!(nth_index(3, 3), Some(2));
        // 9 is the last tab, not the ninth.
        assert_eq!(nth_index(9, 3), Some(2));
        assert_eq!(nth_index(9, 1), Some(0));
        // A tab that is not open leaves the strip alone.
        assert_eq!(nth_index(4, 3), None);
        assert_eq!(nth_index(1, 0), None);
        assert_eq!(nth_index(0, 3), None);
    }
}
