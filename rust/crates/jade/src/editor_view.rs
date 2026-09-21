//! Tabs + editable buffer-backed editor state (Phase-3b E2).
//!
//! Each [`OpenTab`] now owns a [`jade_buffer::Buffer`] (the canonical text) plus
//! cached per-line highlight spans and decoration analyses that are recomputed as
//! the text changes. `EditorState` keeps the tab strip semantics (dedupe by path,
//! close-index math) from the read-only viewer; the active tab drives
//! `JadeApp::active_file`.
//!
//! ## Highlight invalidation choice
//! On every text change we re-highlight the **whole file** (`rehighlight`) with a
//! per-tab **cached** [`highlight::Highlighter`] — compiling the tree-sitter query
//! is ~20ms and doing it per keystroke was the editor's typing latency (measured
//! 73ms/keystroke on a 1.1k-line file; ~7ms debug / ~1ms release after caching +
//! opt-level-2 deps — see the `perf_probe` test). At the single-source-file scale
//! Jade targets this keeps the code path trivially correct with no
//! incremental-edit bookkeeping. Tree-sitter's incremental `edit` API (reparse
//! only the touched range) is the next lever if large files ever hurt; this is
//! the documented, deliberate choice.
//!
//! ## Decoration debounces (inventory §10)
//! Highlighting + the cheap static size scan run immediately on edit (needed for a
//! correct next frame). The heavier flow analysis (300ms) and tree-sitter symbol
//! outline (800ms) run on per-tab [`Debounce`]s, driven by a wake task in `app.rs`
//! and unit-tested here in isolation.

use std::collections::HashMap;
use std::ops::Range;
use std::path::{Path, PathBuf};

use jade_buffer::{Buffer, EditRecord, LspPosition, Point, Selection, TAB_WIDTH};
use jade_lsp::{CompletionItem, Diagnostic, DiagnosticSeverity, Position as LspRange};
use lsp_types::CompletionTextEdit;

use crate::decorations::flow::{self, FlowAnalysis};
use crate::decorations::size_annotations;
use crate::highlight::{self, Span, TokenPalette};
use crate::structure::{self, Symbol};

/// Static size-annotation rescan delay (inventory §10: memory rescan 1000ms;
/// the 50ms coalesce lives in the app's wake-loop poll granularity).
pub const SIZES_DEBOUNCE_MS: u64 = 1000;
/// Flow-decoration recompute delay (inventory §10: 300ms).
pub const FLOW_DEBOUNCE_MS: u64 = 300;
/// Structure-outline recompute delay (inventory §10: 800ms).
pub const STRUCT_DEBOUNCE_MS: u64 = 800;
/// Sync-detection delay (§4.13): the edit must settle before the cross-file
/// detectors compare the buffer against its snapshot.
pub const SYNC_DEBOUNCE_MS: u64 = 600;

/// One open, editable file.
///
/// `buffer` is the source of truth; `highlights`/`sizes` track it eagerly,
/// `flow`/`symbols` track it on the debounces. `diagnostics` are the last
/// clangd `publishDiagnostics` for this path.
pub struct OpenTab {
    pub path: PathBuf,
    pub name: String,
    /// The editable text buffer (cursors, undo, dirty tracking live here).
    pub buffer: Buffer,
    /// Per-line colored spans (byte ranges relative to the line start). Empty for
    /// non-highlightable files. Recomputed on every edit (`rehighlight`).
    pub highlights: Vec<Vec<Span>>,
    /// Static size annotations by 1-based line (§4.5 system 1).
    pub sizes: HashMap<usize, String>,
    /// Execution-flow analysis (§4.8), recomputed on [`Self::flow_debounce`].
    pub flow: FlowAnalysis,
    /// Tree-sitter symbol outline (§5.5), recomputed on [`Self::struct_debounce`].
    pub symbols: Vec<Symbol>,
    /// Last diagnostics clangd published for this file.
    pub diagnostics: Vec<Diagnostic>,
    /// IME composition range (byte range in the buffer), rendered with an
    /// underline. `None` when not composing.
    pub marked: Option<Range<usize>>,
    /// Foldable regions: fold-start row → matching close row (both 0-based),
    /// recomputed on every edit alongside the highlights. A region hides rows
    /// `start+1 ..= end-1` when folded.
    pub fold_map: HashMap<usize, usize>,
    /// Rows the user has folded. Stale entries (no longer fold starts after an
    /// edit) are ignored by [`visible_rows`](Self::visible_rows).
    pub folds: std::collections::BTreeSet<usize>,
    /// True once `didOpen` was sent for this tab (so we don't double-open).
    pub lsp_opened: bool,
    /// Debounce for the static size-annotation rescan (§10: 1000ms).
    pub sizes_debounce: Debounce,
    /// Debounce for the flow recompute (§10: 300ms).
    pub flow_debounce: Debounce,
    /// Debounce for the symbol-outline recompute (§10: 800ms).
    pub struct_debounce: Debounce,
    /// Debounce for the cross-file sync detectors (§4.13: 600ms).
    pub sync_debounce: Debounce,
    /// The buffer text at the last sync-quiescent point. The detectors compare
    /// the current text against this snapshot, then replace it.
    pub sync_snapshot: String,
    palette: TokenPalette,
    /// Compiled highlighter reused across edits. Building one compiles the
    /// tree-sitter query from source (~20ms) — doing that per keystroke was
    /// the editor's typing latency. `None` for non-highlightable files.
    highlighter: Option<highlight::Highlighter>,
    /// Longest line's width in display columns, cached so the code viewer can
    /// size rows for horizontal scrolling without an O(file) scan every frame.
    /// Recomputed on the same per-edit text pass as [`rehighlight`](Self::rehighlight).
    pub max_cols: usize,
    /// Remembered vertical page position (top display row) for this tab, so
    /// switching away and back — or between tabs — restores where you were
    /// reading. Updated when the tab is switched away from; applied on return.
    pub scroll_top: usize,
    /// True while this is the single preview ("temp") tab. A preview open
    /// replaces this tab. The first edit clears the flag and makes the tab
    /// permanent.
    pub preview: bool,
}

/// The longest line's width in **display** columns — the horizontal-scroll
/// extent. A hard tab counts to its next tab stop (see [`DisplayLine`]); wide
/// glyphs count as one column, matching the viewer's `CHAR_W` monospace
/// approximation.
fn max_line_cols(text: &str) -> usize {
    text.split('\n').map(line_display_cols).max().unwrap_or(0)
}

impl OpenTab {
    /// Read + build an editable tab from a file (utf-8 lossy so any file opens).
    pub fn from_file(path: &Path, palette: TokenPalette) -> std::io::Result<OpenTab> {
        let raw = std::fs::read(path)?;
        let text = String::from_utf8_lossy(&raw).into_owned();
        // The tab keeps the path as given; `index_of` compares canonical
        // forms, so two opens through different spellings share one tab.
        Ok(Self::from_text(path, &text, palette))
    }

    /// Build a tab from already-loaded text (fs path + tests + smokes).
    pub fn from_text(path: &Path, text: &str, palette: TokenPalette) -> OpenTab {
        let buffer = Buffer::from_text(text);
        let highlighter = highlight::Highlighter::for_path(path, palette);
        let highlights = match &highlighter {
            Some(h) => h.highlight(text),
            None => highlight::highlight_file(path, text, palette),
        };
        // Only the C++ family runs the static analyzers; Verilog highlights
        // without flow/size/structure decorations.
        let (sizes, flow, symbols) = if highlight::is_cpp_family(path) {
            (
                size_annotations::collect_size_annotations(text),
                flow::analyze(text),
                structure::parse_symbols(text),
            )
        } else {
            (HashMap::new(), FlowAnalysis::default(), Vec::new())
        };
        OpenTab {
            path: path.to_path_buf(),
            name: path
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_else(|| path.display().to_string()),
            buffer,
            highlights,
            sizes,
            flow,
            symbols,
            diagnostics: Vec::new(),
            marked: None,
            fold_map: fold_regions(text),
            folds: std::collections::BTreeSet::new(),
            lsp_opened: false,
            sizes_debounce: Debounce::new(SIZES_DEBOUNCE_MS),
            flow_debounce: Debounce::new(FLOW_DEBOUNCE_MS),
            struct_debounce: Debounce::new(STRUCT_DEBOUNCE_MS),
            sync_debounce: Debounce::new(SYNC_DEBOUNCE_MS),
            sync_snapshot: text.to_string(),
            palette,
            highlighter,
            max_cols: max_line_cols(text),
            scroll_top: 0,
            preview: false,
        }
    }

    /// The text of line `row` (trailing `\n` stripped), as an owned String.
    pub fn line(&self, row: usize) -> String {
        self.buffer.line(row).into_owned()
    }

    /// Number of lines (matches `text.split('\n').count()`).
    pub fn line_count(&self) -> usize {
        self.buffer.line_count()
    }

    /// Total highlighted spans across all lines (headless smoke metric).
    pub fn span_count(&self) -> usize {
        self.highlights.iter().map(|l| l.len()).sum()
    }

    /// Whether this file's extension is highlighted.
    pub fn is_code(&self) -> bool {
        highlight::is_highlightable(&self.path)
    }

    /// Whether the C++ analysis pipeline (flow/sizes/structure) applies.
    pub fn is_cpp_family(&self) -> bool {
        highlight::is_cpp_family(&self.path)
    }

    /// Post-edit bookkeeping: re-highlight eagerly (rendering must be correct on
    /// the very next frame) and arm the three decoration debounces (§10: sizes
    /// 1000ms, flow 300ms, structure 800ms). Called from every edit entry point.
    pub fn on_edited(&mut self, now_ms: u64) {
        // The first edit makes a preview tab permanent.
        self.preview = false;
        self.rehighlight();
        self.sizes_debounce.arm(now_ms);
        self.flow_debounce.arm(now_ms);
        self.struct_debounce.arm(now_ms);
        self.sync_debounce.arm(now_ms);
    }

    /// Full-file re-highlight (see the module-level invalidation note). The
    /// fold map rides along — same eager-per-edit cadence, same text pass.
    pub fn rehighlight(&mut self) {
        let text = self.buffer.to_string();
        self.highlights = match &self.highlighter {
            Some(h) => h.highlight(&text),
            None => highlight::highlight_file(&self.path, &text, self.palette),
        };
        self.fold_map = fold_regions(&text);
        self.max_cols = max_line_cols(&text);
    }

    /// Swap the token palette (theme toggle, §4.2): rebuild the cached
    /// highlighter (capture→color table is baked in) and re-highlight.
    pub fn set_palette(&mut self, palette: TokenPalette) {
        self.palette = palette;
        if self.highlighter.is_some() {
            self.highlighter = highlight::Highlighter::for_path(&self.path, palette);
        }
        self.rehighlight();
    }

    /// Run any decoration recompute whose debounce has elapsed. Returns true if
    /// anything was recomputed (so the caller can `notify`).
    pub fn poll_decorations(&mut self, now_ms: u64) -> bool {
        let mut changed = false;
        if self.sizes_debounce.poll(now_ms) {
            if self.is_cpp_family() {
                self.sizes = size_annotations::collect_size_annotations(&self.buffer.to_string());
            }
            changed = true;
        }
        if self.flow_debounce.poll(now_ms) {
            if self.is_cpp_family() {
                self.flow = flow::analyze(&self.buffer.to_string());
            }
            changed = true;
        }
        if self.struct_debounce.poll(now_ms) {
            if self.is_cpp_family() {
                self.symbols = structure::parse_symbols(&self.buffer.to_string());
            }
            changed = true;
        }
        changed
    }

    /// True while a decoration recompute is still pending.
    pub fn decorations_pending(&self) -> bool {
        self.sizes_debounce.is_pending()
            || self.flow_debounce.is_pending()
            || self.struct_debounce.is_pending()
            || self.sync_debounce.is_pending()
    }

    /// The monotonic LSP document version (buffer version + 1, so `didOpen` is 1).
    pub fn lsp_version(&self) -> i32 {
        (self.buffer.version() + 1) as i32
    }

    /// The primary caret as a render Point (row + char column).
    pub fn caret_point(&self) -> Point {
        self.buffer.offset_to_point(self.buffer.selection().caret())
    }

    /// The buffer rows currently on screen, in order, honoring folds: a folded
    /// region shows its start row and hides `start+1 ..= end-1`. Stale fold
    /// entries (rows that stopped being fold starts) are skipped.
    pub fn visible_rows(&self) -> Vec<usize> {
        let n = self.line_count();
        let mut out = Vec::with_capacity(n);
        let mut row = 0;
        while row < n {
            out.push(row);
            if self.folds.contains(&row) {
                if let Some(&end) = self.fold_map.get(&row) {
                    if end > row + 1 {
                        out.push(end.min(n - 1)); // keep the closing-brace row
                        row = end + 1;
                        continue;
                    }
                }
            }
            row += 1;
        }
        out
    }

    /// Display index of `buffer_row` in [`visible_rows`](Self::visible_rows)
    /// (the nearest visible row at/before it when hidden).
    pub fn display_row(&self, buffer_row: usize) -> usize {
        let vis = self.visible_rows();
        match vis.binary_search(&buffer_row) {
            Ok(i) => i,
            Err(i) => i.saturating_sub(1),
        }
    }

    /// Unfold any folded region hiding `row` (caret auto-unfold: the caret must
    /// never sit in an invisible line).
    pub fn unfold_containing(&mut self, row: usize) -> bool {
        let covering: Vec<usize> = self
            .folds
            .iter()
            .copied()
            .filter(|&start| {
                self.fold_map
                    .get(&start)
                    .is_some_and(|&end| start < row && row < end)
            })
            .collect();
        let changed = !covering.is_empty();
        for start in covering {
            self.folds.remove(&start);
        }
        changed
    }
}

/// A pure millisecond-window debounce (mirrors `workspace_tree::WatchDebounce`,
/// kept editor-local). `arm` (re)starts the window; `poll` fires once the delay
/// has elapsed since the last `arm`. Deterministic: the caller supplies the clock.
#[derive(Debug, Clone)]
pub struct Debounce {
    delay_ms: u64,
    armed_at: Option<u64>,
}

impl Debounce {
    pub fn new(delay_ms: u64) -> Self {
        Debounce {
            delay_ms,
            armed_at: None,
        }
    }

    /// (Re)arm the window at `now_ms`.
    pub fn arm(&mut self, now_ms: u64) {
        self.armed_at = Some(now_ms);
    }

    /// Fire (consuming the armed state) if the delay has elapsed since `arm`.
    pub fn poll(&mut self, now_ms: u64) -> bool {
        if let Some(t) = self.armed_at {
            if now_ms.saturating_sub(t) >= self.delay_ms {
                self.armed_at = None;
                return true;
            }
        }
        false
    }

    pub fn is_pending(&self) -> bool {
        self.armed_at.is_some()
    }
}

/// The tab strip + editor state.
#[derive(Default)]
pub struct EditorState {
    pub tabs: Vec<OpenTab>,
    pub active: Option<usize>,
    palette: TokenPalette,
}

impl EditorState {
    /// Reload every clean tab whose file changed on disk (a benchmark
    /// rewrote its CSV, a formatter touched a source). A dirty tab keeps
    /// its edits. The reloaded tab keeps its scroll position, preview
    /// state, and language-server registration. Returns the reloaded paths.
    pub fn reload_clean_from_disk(&mut self) -> Vec<PathBuf> {
        let mut reloaded = Vec::new();
        for tab in &mut self.tabs {
            if tab.buffer.is_dirty() {
                continue;
            }
            let Ok(raw) = std::fs::read(&tab.path) else { continue };
            let text = String::from_utf8_lossy(&raw).into_owned();
            if text == tab.buffer.to_string() {
                continue;
            }
            let mut fresh = OpenTab::from_text(&tab.path, &text, self.palette);
            fresh.lsp_opened = tab.lsp_opened;
            fresh.preview = tab.preview;
            fresh.scroll_top = tab.scroll_top;
            reloaded.push(tab.path.clone());
            *tab = fresh;
        }
        reloaded
    }

    pub fn new(palette: TokenPalette) -> Self {
        EditorState {
            tabs: Vec::new(),
            active: None,
            palette,
        }
    }

    /// The tab for `path`. Exact match first; then the canonical form, so
    /// `dir/./a.csv`, a symlinked folder, and `/private/tmp` against `/tmp`
    /// all find the tab that was opened under another spelling.
    pub fn index_of(&self, path: &Path) -> Option<usize> {
        if let Some(i) = self.tabs.iter().position(|t| t.path == path) {
            return Some(i);
        }
        let canon = std::fs::canonicalize(path).ok()?;
        self.tabs.iter().position(|t| t.path == canon || std::fs::canonicalize(&t.path).is_ok_and(|c| c == canon))
    }

    /// Open a file: dedupe by path, else read + build an editable tab and append.
    pub fn open(&mut self, path: &Path) -> std::io::Result<usize> {
        if let Some(i) = self.index_of(path) {
            self.active = Some(i);
            return Ok(i);
        }
        let tab = OpenTab::from_file(path, self.palette)?;
        Ok(self.push_tab(tab))
    }

    /// The index of the single preview ("temp") tab, if one is open.
    pub fn preview_index(&self) -> Option<usize> {
        self.tabs.iter().position(|t| t.preview)
    }

    /// Open a file as the preview tab (interactive opens): dedupe by path;
    /// else the new tab replaces the current preview tab in place, or appends
    /// when there is none. The first edit makes the tab permanent
    /// ([`OpenTab::on_edited`]).
    pub fn open_preview(&mut self, path: &Path) -> std::io::Result<usize> {
        if let Some(i) = self.index_of(path) {
            self.active = Some(i);
            return Ok(i);
        }
        let mut tab = OpenTab::from_file(path, self.palette)?;
        tab.preview = true;
        if let Some(i) = self.preview_index() {
            self.tabs[i] = tab;
            self.active = Some(i);
            return Ok(i);
        }
        Ok(self.push_tab(tab))
    }

    /// Append (or dedupe) an already-built tab; it becomes active.
    pub fn push_tab(&mut self, tab: OpenTab) -> usize {
        if let Some(i) = self.index_of(&tab.path) {
            self.active = Some(i);
            return i;
        }
        self.tabs.push(tab);
        let i = self.tabs.len() - 1;
        self.active = Some(i);
        i
    }

    pub fn switch(&mut self, index: usize) {
        if index < self.tabs.len() {
            self.active = Some(index);
        }
    }

    /// Swap the token palette for new tabs and re-highlight every open tab
    /// (theme toggle, §4.2).
    pub fn set_palette(&mut self, palette: TokenPalette) {
        self.palette = palette;
        for tab in &mut self.tabs {
            tab.set_palette(palette);
        }
    }

    /// Close a tab, adjusting the active index (editor-manager.ts:208-241).
    pub fn close(&mut self, index: usize) {
        if index >= self.tabs.len() {
            return;
        }
        let was_active = self.active == Some(index);
        self.tabs.remove(index);
        if self.tabs.is_empty() {
            self.active = None;
        } else if was_active {
            self.active = Some(index.min(self.tabs.len() - 1));
        } else if let Some(a) = self.active {
            if index < a {
                self.active = Some(a - 1);
            }
        }
    }

    /// Remove a tab and hand it back (a move to another pane). The active
    /// index adjusts the same way as [`close`](Self::close).
    pub fn detach(&mut self, index: usize) -> Option<OpenTab> {
        if index >= self.tabs.len() {
            return None;
        }
        let was_active = self.active == Some(index);
        let tab = self.tabs.remove(index);
        if self.tabs.is_empty() {
            self.active = None;
        } else if was_active {
            self.active = Some(index.min(self.tabs.len() - 1));
        } else if let Some(a) = self.active {
            if index < a {
                self.active = Some(a - 1);
            }
        }
        Some(tab)
    }

    /// Insert a tab before `before` (else at the end) and make it active.
    /// A tab for the same path already here wins; the given one drops.
    pub fn insert_tab(&mut self, tab: OpenTab, before: Option<usize>) -> usize {
        if let Some(i) = self.index_of(&tab.path) {
            self.active = Some(i);
            return i;
        }
        let at = before.unwrap_or(self.tabs.len()).min(self.tabs.len());
        self.tabs.insert(at, tab);
        self.active = Some(at);
        at
    }

    /// Move the tab at `from` so it sits before the tab that is at `to` now
    /// (`to == len` moves it to the end). The active tab stays the same tab.
    pub fn reorder(&mut self, from: usize, to: usize) {
        if from >= self.tabs.len() {
            return;
        }
        let active_path = self.active_path();
        let to = to.min(self.tabs.len());
        let tab = self.tabs.remove(from);
        let dest = if to > from { to - 1 } else { to };
        self.tabs.insert(dest, tab);
        self.active = active_path.and_then(|p| self.index_of(&p));
    }

    pub fn active_tab(&self) -> Option<&OpenTab> {
        self.active.and_then(|i| self.tabs.get(i))
    }

    pub fn active_tab_mut(&mut self) -> Option<&mut OpenTab> {
        match self.active {
            Some(i) => self.tabs.get_mut(i),
            None => None,
        }
    }

    pub fn active_path(&self) -> Option<PathBuf> {
        self.active_tab().map(|t| t.path.clone())
    }

    /// Find the tab for `path` (diagnostics routing).
    pub fn tab_mut_for(&mut self, path: &Path) -> Option<&mut OpenTab> {
        self.tabs.iter_mut().find(|t| t.path == path)
    }
}

// ── Pure editor logic (unit-tested below) ─────────────────────────────────────

/// A per-byte combined style cell used to flatten overlapping style layers
/// (syntax color · selection · diagnostic underline · IME marked) into the
/// sorted, non-overlapping run list `gpui::StyledText::with_highlights` requires.
#[derive(Clone, Copy, PartialEq, Eq, Default)]
pub struct CellStyle {
    /// Syntax color (`0xRRGGBB`), if any.
    pub color: Option<u32>,
    /// True when this byte is inside the selection (rendered as a bg wash).
    pub selected: bool,
    /// Diagnostic underline color (`0xRRGGBB`), if any.
    pub underline: Option<u32>,
    /// True when this byte is inside the IME marked (composing) range.
    pub marked: bool,
    /// True when this byte is inside a find match (rendered as an amber wash so
    /// every occurrence is visible; the *current* match also sets `selected`).
    pub find_match: bool,
}

/// Flatten the style layers for one line into coalesced `(byte-range, CellStyle)`
/// runs. Inputs are all in **line-relative byte** coordinates and are assumed to
/// fall on char boundaries (they come from tree-sitter nodes / char-aligned
/// offsets), so runs never split a multi-byte char.
pub fn merge_line_styles(
    len: usize,
    syntax: &[Span],
    selection: Option<Range<usize>>,
    underlines: &[(Range<usize>, u32)],
    marked: Option<Range<usize>>,
    find_matches: &[Range<usize>],
) -> Vec<(Range<usize>, CellStyle)> {
    if len == 0 {
        return Vec::new();
    }
    let mut cells = vec![CellStyle::default(); len];
    for s in syntax {
        for cell in &mut cells[s.start.min(len)..s.end.min(len)] {
            cell.color = Some(s.color);
        }
    }
    for r in find_matches {
        for cell in &mut cells[r.start.min(len)..r.end.min(len)] {
            cell.find_match = true;
        }
    }
    if let Some(sel) = selection {
        for cell in &mut cells[sel.start.min(len)..sel.end.min(len)] {
            cell.selected = true;
        }
    }
    for (r, color) in underlines {
        for cell in &mut cells[r.start.min(len)..r.end.min(len)] {
            cell.underline = Some(*color);
        }
    }
    if let Some(m) = marked {
        for cell in &mut cells[m.start.min(len)..m.end.min(len)] {
            cell.marked = true;
        }
    }
    // Coalesce equal adjacent cells into runs.
    let mut runs = Vec::new();
    let mut start = 0;
    for i in 1..=len {
        if i == len || cells[i] != cells[start] {
            let style = cells[start];
            if style != CellStyle::default() {
                runs.push((start..i, style));
            }
            start = i;
        }
    }
    runs
}

/// Brace-based fold regions: for each row whose LAST opened `{` closes on a
/// later row, map start row → close row. Nested blocks fold independently
/// (hierarchical). String/char/comment contents are skipped so braces inside
/// literals don't pair.
pub fn fold_regions(text: &str) -> HashMap<usize, usize> {
    let mut map = HashMap::new();
    let mut stack: Vec<usize> = Vec::new(); // rows of currently-open braces
    let mut row = 0usize;
    let mut chars = text.chars().peekable();
    let (mut in_str, mut in_chr, mut in_line_c, mut in_block_c, mut esc) =
        (false, false, false, false, false);
    while let Some(c) = chars.next() {
        if c == '\n' {
            row += 1;
            in_line_c = false;
            esc = false;
            continue;
        }
        if esc {
            esc = false;
            continue;
        }
        match c {
            '\\' if in_str || in_chr => esc = true,
            '"' if !in_chr && !in_line_c && !in_block_c => in_str = !in_str,
            '\'' if !in_str && !in_line_c && !in_block_c => in_chr = !in_chr,
            '/' if !in_str && !in_chr && !in_block_c && !in_line_c => match chars.peek() {
                Some('/') => in_line_c = true,
                Some('*') => {
                    chars.next();
                    in_block_c = true;
                }
                _ => {}
            },
            '*' if in_block_c => {
                if chars.peek() == Some(&'/') {
                    chars.next();
                    in_block_c = false;
                }
            }
            '{' if !in_str && !in_chr && !in_line_c && !in_block_c => stack.push(row),
            '}' if !in_str && !in_chr && !in_line_c && !in_block_c => {
                if let Some(start) = stack.pop() {
                    // Only multi-line blocks fold; the LAST { on a row wins
                    // (insert overwrites an earlier same-row pair).
                    if row > start + 1 {
                        map.insert(start, row);
                    }
                }
            }
            _ => {}
        }
    }
    map
}

/// Round a line-relative x pixel to a char column, clamped to `max_col`. Correct
/// only for a line without hard tabs; the editor calls [`px_to_char_col`], which
/// also handles tab stops.
pub fn px_to_col(x_rel: f32, char_w: f32, max_col: usize) -> usize {
    if x_rel <= 0.0 || char_w <= 0.0 {
        return 0;
    }
    ((x_rel / char_w).round() as usize).min(max_col)
}

/// The x pixel of **display** column `col` (see [`DisplayLine`]).
pub fn col_to_px(col: usize, char_w: f32) -> f32 {
    col as f32 * char_w
}

/// The display columns `ch` takes when it starts at column `col`: one, except a
/// hard tab (to the next [`TAB_WIDTH`] stop) and a stray CR, which the text
/// system draws as nothing.
fn char_cols(ch: char, col: usize) -> usize {
    match ch {
        '\t' => TAB_WIDTH - col % TAB_WIDTH,
        // The buffer splits lines on '\n' alone, so a CRLF file leaves a CR at
        // the end of every line. It has no glyph, so it takes no column.
        '\r' => 0,
        _ => 1,
    }
}

/// True when `line` draws one column per char, so display columns and char
/// columns are the same thing.
fn is_plain(line: &str) -> bool {
    !line.contains('\t') && !line.contains('\r')
}

/// The width of `line` in display columns.
pub fn line_display_cols(line: &str) -> usize {
    let mut col = 0;
    for ch in line.chars() {
        col += char_cols(ch, col);
    }
    col
}

/// One line as the code view draws it: **hard tabs expanded to spaces** at
/// [`TAB_WIDTH`] stops (and a stray CR counted as zero columns), plus the maps
/// between raw and display coordinates.
///
/// # Why the expansion exists
/// The renderer hands the line to CoreText, which lays a raw U+0009 out on its
/// own default 28px tab stops — a width the editor's char grid does not know
/// about. So on any tab-indented line the glyphs sat at 28px multiples while the
/// caret, drawn at `char column × char advance` (~7.83px), sat somewhere else:
/// the caret was offset from the text in every file that indents with tabs.
/// Expanding to spaces before shaping puts both on the same 4-column grid Jade's
/// own Tab key inserts, so the caret lands on the glyph it points at.
///
/// A line of plain chars keeps its raw text and both maps stay empty (the display
/// column *is* the char column) — the common case allocates nothing extra.
///
/// Wide glyphs (CJK/emoji) still count as one column — the documented
/// `CHAR_W` approximation, unchanged here.
pub struct DisplayLine {
    /// The text to draw (raw line, or the tab-expanded copy).
    pub text: String,
    /// Total display columns.
    pub cols: usize,
    /// Chars in the raw line.
    chars: usize,
    /// Display column at each raw char boundary (`chars + 1` entries). Empty when
    /// the line needs no expansion.
    col_at: Vec<usize>,
    /// Display-text byte offset for each raw byte offset (`raw.len() + 1`
    /// entries). Empty when the line needs no expansion.
    byte_at: Vec<usize>,
}

impl DisplayLine {
    /// Build the display form of `raw` (taken by value: a line that needs no
    /// expansion is kept as-is).
    pub fn new(raw: String) -> DisplayLine {
        let chars = raw.chars().count();
        if is_plain(&raw) {
            return DisplayLine {
                text: raw,
                cols: chars,
                chars,
                col_at: Vec::new(),
                byte_at: Vec::new(),
            };
        }
        let mut text = String::with_capacity(raw.len() + TAB_WIDTH);
        let mut col_at = Vec::with_capacity(chars + 1);
        let mut byte_at = Vec::with_capacity(raw.len() + 1);
        let mut col = 0;
        for ch in raw.chars() {
            col_at.push(col);
            // Every byte of a multi-byte char maps to the char's start, so a
            // range whose bound lands mid-char still maps inside the line.
            for _ in 0..ch.len_utf8() {
                byte_at.push(text.len());
            }
            let n = char_cols(ch, col);
            if ch == '\t' {
                for _ in 0..n {
                    text.push(' ');
                }
            } else {
                text.push(ch);
            }
            col += n;
        }
        col_at.push(col);
        byte_at.push(text.len());
        DisplayLine {
            text,
            cols: col,
            chars,
            col_at,
            byte_at,
        }
    }

    /// The display column of char column `col` (clamped to the line's end) — the
    /// column the caret bar draws at.
    pub fn display_col(&self, col: usize) -> usize {
        let col = col.min(self.chars);
        if self.col_at.is_empty() {
            col
        } else {
            self.col_at[col]
        }
    }

    /// The char column whose display column is nearest `x_cols` (an x pixel
    /// divided by the char advance) — the inverse used for click → caret.
    pub fn char_col_at_x(&self, x_cols: f32) -> usize {
        if x_cols <= 0.0 {
            return 0;
        }
        if self.col_at.is_empty() {
            return (x_cols.round() as usize).min(self.chars);
        }
        // col_at ascends, so the first boundary at-or-past x_cols and the one
        // before it are the only candidates.
        let after = self
            .col_at
            .iter()
            .position(|&c| c as f32 >= x_cols)
            .unwrap_or(self.chars);
        if after == 0 {
            return 0;
        }
        let before = after - 1;
        if x_cols - self.col_at[before] as f32 <= self.col_at[after] as f32 - x_cols {
            before
        } else {
            after
        }
    }

    /// A raw-line byte offset → the same position in [`text`](Self::text).
    pub fn map_byte(&self, byte: usize) -> usize {
        if self.byte_at.is_empty() {
            byte.min(self.text.len())
        } else {
            self.byte_at[byte.min(self.byte_at.len() - 1)]
        }
    }

    /// A raw-line byte range → the same span of [`text`](Self::text), for the
    /// syntax / selection / diagnostic highlight runs.
    pub fn map_range(&self, r: Range<usize>) -> Range<usize> {
        self.map_byte(r.start)..self.map_byte(r.end)
    }
}

/// Round a line-relative x pixel to a **char** column on `line`. Hard tabs make
/// this differ from [`px_to_col`]: x maps to the nearest char boundary measured
/// in display columns, so a click inside a tab's white space lands on one of its
/// two edges.
pub fn px_to_char_col(line: &str, x_rel: f32, char_w: f32) -> usize {
    if x_rel <= 0.0 || char_w <= 0.0 {
        return 0;
    }
    DisplayLine::new(line.to_string()).char_col_at_x(x_rel / char_w)
}

/// The char-column range of the word/identifier at char column `col` on `line`
/// (double-click selection). A word is `[A-Za-z0-9_]`; clicking on a non-word
/// char selects just that char.
pub fn word_range_in_line(line: &str, col: usize) -> Range<usize> {
    let chars: Vec<char> = line.chars().collect();
    let n = chars.len();
    if n == 0 {
        return 0..0;
    }
    let is_word = |c: char| c.is_alphanumeric() || c == '_';
    // Clicking at end-of-line or on a boundary: look at the char under the caret,
    // falling back to the char to the left.
    let idx = col.min(n.saturating_sub(1));
    if !is_word(chars[idx]) {
        // Non-word: select the single char (or nothing at line end).
        return if col >= n { n..n } else { col..col + 1 };
    }
    let mut start = idx;
    while start > 0 && is_word(chars[start - 1]) {
        start -= 1;
    }
    let mut end = idx + 1;
    while end < n && is_word(chars[end]) {
        end += 1;
    }
    start..end
}

/// The prefix identifier immediately left of `caret` on its line: returns the
/// buffer byte range of that identifier (empty range at the caret when there is
/// no identifier char to the left). Used to know what a completion replaces and
/// to filter the list by what's typed.
pub fn ident_range_before(buffer: &Buffer, caret: usize) -> Range<usize> {
    let point = buffer.offset_to_point(caret);
    let line = buffer.line(point.row);
    let chars: Vec<char> = line.chars().collect();
    let is_word = |c: char| c.is_alphanumeric() || c == '_';
    let mut start_col = point.col;
    while start_col > 0 && start_col <= chars.len() && is_word(chars[start_col - 1]) {
        start_col -= 1;
    }
    let start = buffer.point_to_offset(Point::new(point.row, start_col));
    start..caret
}

/// The severity → underline color mapping (§4.2): error red, warning amber, info
/// blue, hint muted-blue.
pub fn severity_color(sev: Option<DiagnosticSeverity>, red: u32, amber: u32, blue: u32) -> u32 {
    match sev {
        Some(DiagnosticSeverity::ERROR) => red,
        Some(DiagnosticSeverity::WARNING) => amber,
        _ => blue,
    }
}

/// The utf16-column span a diagnostic covers **on a single row**, or `None` when
/// the row is outside the diagnostic's line range. For a multi-line diagnostic
/// the first row runs `start.col..line_end`, interior rows the whole line
/// (`0..line_end`), and the last row `0..end.col`. `line_len_u16` is the row's
/// length in utf16 code units.
pub fn diag_span_on_row(
    start: (usize, usize),
    end: (usize, usize),
    row: usize,
    line_len_u16: usize,
) -> Option<(usize, usize)> {
    let (sl, sc) = start;
    let (el, ec) = end;
    if row < sl || row > el {
        return None;
    }
    let s = if row == sl { sc } else { 0 };
    let e = if row == el { ec } else { line_len_u16 };
    let e = e.min(line_len_u16).max(s);
    // A zero-width diagnostic (start == end on the same row) still underlines one
    // column so it's visible.
    if s == e {
        Some((s, (s + 1).min(line_len_u16.max(s + 1))))
    } else {
        Some((s, e))
    }
}

/// Compute the diagnostic underline runs for a row, in **line-relative byte**
/// coordinates, colored by severity. Uses the buffer for utf16→byte conversion.
pub fn diagnostic_underlines_for_row(
    diagnostics: &[Diagnostic],
    row: usize,
    buffer: &Buffer,
    red: u32,
    amber: u32,
    blue: u32,
) -> Vec<(Range<usize>, u32)> {
    let line_start = buffer.point_to_offset(Point::new(row, 0));
    let line_len_u16 = line_utf16_len(buffer, row);
    let mut out = Vec::new();
    for d in diagnostics {
        let start = (
            d.range.start.line as usize,
            d.range.start.character as usize,
        );
        let end = (d.range.end.line as usize, d.range.end.character as usize);
        if let Some((sc, ec)) = diag_span_on_row(start, end, row, line_len_u16) {
            let sb = buffer.lsp_to_offset(LspPosition::new(row, sc)) - line_start;
            let eb = buffer.lsp_to_offset(LspPosition::new(row, ec)) - line_start;
            if eb > sb {
                out.push((sb..eb, severity_color(d.severity, red, amber, blue)));
            }
        }
    }
    out
}

/// The diagnostics whose range covers the character column `col` on `row`.
/// `col` is a char index into the row (what [`px_to_char_col`] returns). The
/// match uses the same per-row span as the underline, so a hover over any part
/// of a squiggle finds its diagnostic. Order follows the diagnostics list.
pub fn diagnostics_at<'a>(
    diagnostics: &'a [Diagnostic],
    buffer: &Buffer,
    row: usize,
    col: usize,
) -> Vec<&'a Diagnostic> {
    let line = buffer.line(row);
    let col_u16: usize = line.chars().take(col).map(|c| c.len_utf16()).sum();
    let line_len_u16: usize = line.chars().map(|c| c.len_utf16()).sum();
    diagnostics
        .iter()
        .filter(|d| {
            let start = (
                d.range.start.line as usize,
                d.range.start.character as usize,
            );
            let end = (d.range.end.line as usize, d.range.end.character as usize);
            match diag_span_on_row(start, end, row, line_len_u16) {
                Some((s, e)) => s <= col_u16 && col_u16 < e,
                None => false,
            }
        })
        .collect()
}

/// Count severities across a diagnostics list → (errors, warnings, infos). Hints
/// fold into infos.
pub fn diagnostic_counts(diagnostics: &[Diagnostic]) -> (usize, usize, usize) {
    let mut e = 0;
    let mut w = 0;
    let mut i = 0;
    for d in diagnostics {
        match d.severity {
            Some(DiagnosticSeverity::ERROR) => e += 1,
            Some(DiagnosticSeverity::WARNING) => w += 1,
            _ => i += 1,
        }
    }
    (e, w, i)
}

/// Filter completion items by the typed `prefix` (case-insensitive prefix match
/// against the item's filter text / label), preserving server order. An empty
/// prefix keeps everything.
pub fn completion_filter(items: &[CompletionItem], prefix: &str) -> Vec<usize> {
    if prefix.is_empty() {
        return (0..items.len()).collect();
    }
    let needle = prefix.to_ascii_lowercase();
    items
        .iter()
        .enumerate()
        .filter(|(_, it)| {
            let hay = it
                .filter_text
                .as_deref()
                .unwrap_or(&it.label)
                .to_ascii_lowercase();
            hay.starts_with(&needle)
        })
        .map(|(i, _)| i)
        .collect()
}

/// Resolve a completion item to the concrete buffer edit to apply: `(byte range
/// to replace, replacement text)`. Prefers the item's `textEdit` (whose range is
/// authoritative, converted from LSP utf16), else replaces the typed identifier
/// (`ident_range`) with `insertText`/`label`.
pub fn completion_edit(
    item: &CompletionItem,
    buffer: &Buffer,
    ident_range: Range<usize>,
) -> (Range<usize>, String) {
    // Snippet items (`snippetSupport: true`) carry LSP snippet syntax in their
    // text — `foo(${1:int a}, ${2:int b})` — which must be expanded, not inserted
    // literally. We keep each placeholder's default text so completing a call
    // fills in its parameters as editable text (the signature hint shows them
    // too); tabstops/variables are dropped.
    let is_snippet = item.insert_text_format == Some(lsp_types::InsertTextFormat::SNIPPET);
    let expand = |text: String| {
        if is_snippet {
            strip_snippet(&text)
        } else {
            text
        }
    };

    if let Some(te) = &item.text_edit {
        let (range, text) = match te {
            CompletionTextEdit::Edit(e) => (&e.range, e.new_text.clone()),
            CompletionTextEdit::InsertAndReplace(e) => (&e.replace, e.new_text.clone()),
        };
        let s = buffer.lsp_to_offset(lsp_pos(range.start));
        let end = buffer.lsp_to_offset(lsp_pos(range.end));
        return (s..end, expand(text));
    }
    let text = item
        .insert_text
        .clone()
        .unwrap_or_else(|| item.label.clone());
    (ident_range, expand(text))
}

/// Expand LSP snippet syntax to plain text, keeping placeholder default text so a
/// completed call shows its parameters (`foo(${1:int a}, ${2:int b})` →
/// `foo(int a, int b)`). Handles `${n:default}` (default kept, nested-aware),
/// `${n|a,b|}` (first choice), bare `$n` / `${n}` / `$var` (dropped), and the
/// `\$ \} \\` escapes.
pub fn strip_snippet(s: &str) -> String {
    let chars: Vec<char> = s.chars().collect();
    let mut i = 0;
    let mut out = String::with_capacity(s.len());
    snippet_text(&chars, &mut i, &mut out, false);
    out
}

/// Parse snippet body into `out`. When `in_placeholder`, stops after the matching
/// unescaped `}` (which it consumes).
fn snippet_text(chars: &[char], i: &mut usize, out: &mut String, in_placeholder: bool) {
    while *i < chars.len() {
        match chars[*i] {
            '\\' => {
                // Escape: emit the next char literally (or a lone trailing '\').
                if *i + 1 < chars.len() {
                    out.push(chars[*i + 1]);
                    *i += 2;
                } else {
                    out.push('\\');
                    *i += 1;
                }
            }
            '}' if in_placeholder => {
                *i += 1;
                return;
            }
            '$' => {
                *i += 1;
                if *i < chars.len() && chars[*i] == '{' {
                    *i += 1;
                    snippet_placeholder(chars, i, out);
                } else {
                    // Bare `$1` / `$var`: drop the identifier/number.
                    while *i < chars.len() && (chars[*i].is_alphanumeric() || chars[*i] == '_') {
                        *i += 1;
                    }
                }
            }
            c => {
                out.push(c);
                *i += 1;
            }
        }
    }
}

/// Parse the body after `${`: a tabstop number or variable, then optional
/// `:default` (kept) or `|choice,…|` (first kept), then the closing `}`.
fn snippet_placeholder(chars: &[char], i: &mut usize, out: &mut String) {
    // Skip the leading tabstop number / variable name.
    while *i < chars.len() && !matches!(chars[*i], ':' | '}' | '|') {
        *i += 1;
    }
    match chars.get(*i) {
        Some(':') => {
            *i += 1;
            snippet_text(chars, i, out, true); // default text (nested-aware)
        }
        Some('|') => {
            *i += 1; // ${1|a,b,c|} → keep the first option
            while *i < chars.len() && !matches!(chars[*i], ',' | '|') {
                out.push(chars[*i]);
                *i += 1;
            }
            while *i < chars.len() && chars[*i] != '}' {
                *i += 1;
            }
            if *i < chars.len() {
                *i += 1; // consume '}'
            }
        }
        _ => {
            if *i < chars.len() {
                *i += 1; // consume '}' of a bare ${1}
            }
        }
    }
}

fn lsp_pos(p: LspRange) -> LspPosition {
    LspPosition::new(p.line as usize, p.character as usize)
}

/// utf16 length of line `row` (excluding the trailing newline).
pub fn line_utf16_len(buffer: &Buffer, row: usize) -> usize {
    buffer.line(row).chars().map(|c| c.len_utf16()).sum()
}

/// Whole-document byte offset → document utf16 offset (EntityInputHandler speaks
/// document-wide utf16). O(rows) using only the buffer's public API.
pub fn byte_to_doc_utf16(buffer: &Buffer, byte: usize) -> usize {
    let p = buffer.offset_to_point(byte);
    let mut u = 0;
    for r in 0..p.row {
        u += line_utf16_len(buffer, r) + 1; // + newline
    }
    let line = buffer.line(p.row);
    u += line
        .chars()
        .take(p.col)
        .map(|c| c.len_utf16())
        .sum::<usize>();
    u
}

/// Document utf16 offset → whole-document byte offset (inverse of the above).
pub fn doc_utf16_to_byte(buffer: &Buffer, target: usize) -> usize {
    let mut u = 0;
    let lines = buffer.line_count();
    for r in 0..lines {
        let ll = line_utf16_len(buffer, r);
        if target <= u + ll {
            let into = target - u;
            let line = buffer.line(r);
            let mut cu = 0;
            let mut col = 0;
            for c in line.chars() {
                if cu >= into {
                    break;
                }
                cu += c.len_utf16();
                col += 1;
            }
            return buffer.point_to_offset(Point::new(r, col));
        }
        u += ll + 1; // newline
    }
    buffer.len_bytes()
}

/// Convert a buffer [`EditRecord`]'s descending utf16 changes into the LSP wire
/// edits `jade_lsp::DidChange::Incremental` expects.
pub fn record_to_lsp_edits(record: &EditRecord) -> Vec<jade_lsp::Utf16RangeEdit> {
    record
        .changes
        .iter()
        .map(|c| jade_lsp::Utf16RangeEdit {
            range: jade_lsp::Range {
                start: jade_lsp::Position::new(c.start.line as u32, c.start.character as u32),
                end: jade_lsp::Position::new(c.end.line as u32, c.end.character as u32),
            },
            text: c.new_text.clone(),
        })
        .collect()
}

/// Byte range of the primary selection intersected with `[line_start, line_end]`
/// and made line-relative — or `None` when the selection doesn't cover any of
/// this line's text. (An empty selection, or one touching only the line's very
/// end, yields `None` — nothing to wash.)
pub fn selection_on_line(
    sel: Selection,
    line_start: usize,
    line_end: usize,
) -> Option<Range<usize>> {
    let (s, e) = (sel.start(), sel.end());
    if s >= e {
        return None; // empty selection
    }
    if e <= line_start || s >= line_end {
        return None; // no overlap with this line's text
    }
    let cs = s.max(line_start) - line_start;
    let ce = e.min(line_end) - line_start;
    if ce <= cs {
        return None;
    }
    Some(cs..ce)
}

/// Every find match intersecting `[line_start, line_end]`, clipped and made
/// line-relative (byte coords). `matches` are whole-buffer byte ranges. Empty
/// when no match touches this line.
pub fn find_matches_on_line(
    matches: &[Range<usize>],
    line_start: usize,
    line_end: usize,
) -> Vec<Range<usize>> {
    let mut out = Vec::new();
    for m in matches {
        if m.end <= line_start || m.start >= line_end {
            continue;
        }
        let cs = m.start.max(line_start) - line_start;
        let ce = m.end.min(line_end) - line_start;
        if ce > cs {
            out.push(cs..ce);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use jade_lsp::{CompletionItem, Diagnostic, DiagnosticSeverity, Position, Range as LspR};
    use lsp_types::TextEdit;

    fn tab_from(name: &str, text: &str) -> OpenTab {
        OpenTab::from_text(
            &PathBuf::from(format!("/x/{name}")),
            text,
            TokenPalette::jade_dark(),
        )
    }

    #[test]
    fn open_dedupes_and_close_index() {
        let mut e = EditorState::new(TokenPalette::jade_dark());
        e.push_tab(tab_from("a.cpp", "int a;\n"));
        e.push_tab(tab_from("b.cpp", "int b;\n"));
        assert_eq!(e.tabs.len(), 2);
        assert_eq!(e.active, Some(1));
        e.push_tab(tab_from("a.cpp", "int a;\n")); // dedupe → switch
        assert_eq!(e.tabs.len(), 2);
        assert_eq!(e.active, Some(0));
        e.close(0);
        assert_eq!(e.active, Some(0));
        assert_eq!(e.active_tab().unwrap().name, "b.cpp");
    }

    #[test]
    fn preview_tab_replaces_until_edit() {
        let dir = std::env::temp_dir().join("jade-preview-tab-test");
        std::fs::create_dir_all(&dir).unwrap();
        let a = dir.join("a.cpp");
        let b = dir.join("b.cpp");
        let c = dir.join("c.cpp");
        std::fs::write(&a, "int a;\n").unwrap();
        std::fs::write(&b, "int b;\n").unwrap();
        std::fs::write(&c, "int c;\n").unwrap();

        let mut e = EditorState::new(TokenPalette::jade_dark());
        // A tab from push_tab (restore path) is permanent.
        e.push_tab(tab_from("solid.cpp", "int s;\n"));
        // The first preview open appends.
        e.open_preview(&a).unwrap();
        assert_eq!(e.tabs.len(), 2);
        assert_eq!(e.preview_index(), Some(1));
        // The next preview open replaces the preview tab in place.
        e.open_preview(&b).unwrap();
        assert_eq!(e.tabs.len(), 2);
        assert_eq!(e.tabs[1].path, b);
        assert_eq!(e.active, Some(1));
        // An edit makes the preview tab permanent.
        e.tabs[1].buffer.type_char('x');
        e.tabs[1].on_edited(0);
        assert_eq!(e.preview_index(), None);
        // The next preview open appends a new preview tab.
        e.open_preview(&c).unwrap();
        assert_eq!(e.tabs.len(), 3);
        assert_eq!(e.preview_index(), Some(2));
        // A re-open of an already open file only switches.
        e.open_preview(&b).unwrap();
        assert_eq!(e.tabs.len(), 3);
        assert_eq!(e.active, Some(1));
    }

    #[test]
    fn buffer_backed_tab_line_access() {
        let t = tab_from("a.cpp", "int x = 1;\n// c\n");
        assert_eq!(t.line_count(), 3);
        assert_eq!(t.line(0), "int x = 1;");
        assert_eq!(t.line(1), "// c");
        assert!(t.span_count() > 0);
    }

    #[test]
    fn debounce_fires_once_after_delay() {
        let mut d = Debounce::new(300);
        assert!(!d.is_pending());
        d.arm(1000);
        assert!(d.is_pending());
        assert!(!d.poll(1200)); // not yet
        assert!(!d.poll(1299));
        assert!(d.poll(1300)); // fires
        assert!(!d.poll(1400)); // consumed
        assert!(!d.is_pending());
        // Re-arming (typing burst) pushes the deadline out.
        d.arm(2000);
        d.arm(2100);
        assert!(!d.poll(2300));
        assert!(d.poll(2400));
    }

    #[test]
    fn on_edited_rehighlights_and_arms() {
        let mut t = tab_from("a.cpp", "int x;\n");
        // Type into the buffer, then run the eager trackers.
        t.buffer.type_char('y');
        t.on_edited(500);
        assert!(t.decorations_pending());
        // Flow fires at +300ms (=800), symbols at +800ms (=1300), sizes at
        // +1000ms (=1500).
        assert!(!t.poll_decorations(700)); // none due yet
        assert!(t.poll_decorations(800)); // flow due
        assert!(t.decorations_pending()); // struct + sizes + sync still pending
        assert!(t.sync_debounce.poll(1100)); // sync due (the app loop polls it)
        assert!(t.poll_decorations(1300)); // struct due
        assert!(t.poll_decorations(1500)); // sizes due
        assert!(!t.decorations_pending());
    }

    #[test]
    fn max_cols_tracks_longest_line_and_updates_on_edit() {
        // Longest line is the 15-char "int a = 12345;" row (＝ its char count).
        let mut t = tab_from("a.cpp", "int x;\nint a = 12345;\n");
        assert_eq!(t.max_cols, "int a = 12345;".chars().count());

        // Growing a line past the current max updates the cached extent.
        t.buffer.set_caret(0);
        for _ in 0..40 {
            t.buffer.type_char('z');
        }
        t.on_edited(0); // rehighlight recomputes max_cols
        assert!(t.max_cols >= 40, "max_cols grew with the edited line");
    }

    /// A tab-free line: the display form is the raw line and both maps are the
    /// identity.
    #[test]
    fn display_line_without_tabs_is_the_raw_line() {
        let d = DisplayLine::new("int x = 1;".to_string());
        assert_eq!(d.text, "int x = 1;");
        assert_eq!(d.cols, 10);
        assert_eq!(d.display_col(4), 4);
        assert_eq!(d.display_col(99), 10); // clamps to the line end
        assert_eq!(d.map_range(2..5), 2..5);
        assert_eq!(d.char_col_at_x(3.4), 3);
        assert_eq!(d.char_col_at_x(3.6), 4);
    }

    /// A hard tab advances to the next TAB_WIDTH stop, so the caret column of
    /// everything after it shifts — the offset this whole layer exists to fix.
    #[test]
    fn display_line_expands_tabs_to_stops() {
        let d = DisplayLine::new("\tx".to_string());
        assert_eq!(d.text, "    x");
        assert_eq!(d.cols, 5);
        assert_eq!(d.display_col(0), 0); // before the tab
        assert_eq!(d.display_col(1), 4); // the 'x' sits at the first stop
        assert_eq!(d.display_col(2), 5);

        // A tab from column 2 fills only the 2 columns left to the stop.
        let d = DisplayLine::new("ab\tcd".to_string());
        assert_eq!(d.text, "ab  cd");
        assert_eq!(d.display_col(3), 4);
        assert_eq!(d.cols, 6);

        // Two leading tabs → two full stops (8 columns of indent).
        let d = DisplayLine::new("\t\tif (x) {".to_string());
        assert_eq!(d.display_col(2), 8);
    }

    /// A CRLF file leaves a CR at the end of each line. It draws as nothing, so
    /// it must take no column — otherwise the caret at end of line sat one cell
    /// past the last glyph.
    #[test]
    fn display_line_gives_a_stray_cr_no_column() {
        let d = DisplayLine::new("int x;\r".to_string());
        assert_eq!(d.cols, 6);
        assert_eq!(d.display_col(6), 6); // before the CR
        assert_eq!(d.display_col(7), 6); // after it — same x
        assert_eq!(line_display_cols("int x;\r"), 6);
    }

    /// Highlight runs are byte ranges into the raw line; they must land on the
    /// same text in the expanded copy.
    #[test]
    fn display_line_maps_highlight_ranges() {
        let d = DisplayLine::new("\tint\tx".to_string());
        assert_eq!(d.text, "    int x");
        assert_eq!(d.map_range(1..4), 4..7); // "int"
        assert_eq!(d.map_range(5..6), 8..9); // "x"
                                             // A multi-byte char keeps its whole span (bytes map to the char start).
        let d = DisplayLine::new("\tü".to_string());
        assert_eq!(d.map_range(1..3), 4..6);
    }

    /// Clicking inside a tab's white space snaps to the nearer of its two edges.
    #[test]
    fn px_to_char_col_snaps_across_a_tab() {
        // "\tx" draws as 4 spaces + x; char boundaries sit at columns 0, 4, 5.
        // char_w = 8 → x=15px is 1.875 columns: nearer column 0 than column 4.
        assert_eq!(px_to_char_col("\tx", 15.0, 8.0), 0);
        assert_eq!(px_to_char_col("\tx", 18.0, 8.0), 1); // 2.25 cols → the 'x'
        assert_eq!(px_to_char_col("\tx", 999.0, 8.0), 2); // clamps past EOL
                                                          // No tab → same rounding as px_to_col.
        assert_eq!(px_to_char_col("abcd", 11.0, 8.0), 1);
        assert_eq!(px_to_char_col("abcd", 13.0, 8.0), 2);
    }

    /// The h-scroll extent counts a tab to its stop, not as one column.
    #[test]
    fn max_cols_counts_tabs_to_their_stop() {
        let t = tab_from("a.cpp", "\t\tx\n");
        assert_eq!(t.max_cols, 9);
    }

    #[test]
    fn px_to_col_rounds_and_clamps() {
        // char_w = 8: x=11 → col 1 (round 1.375), x=13 → col 2 (round 1.625).
        assert_eq!(px_to_col(0.0, 8.0, 10), 0);
        assert_eq!(px_to_col(11.0, 8.0, 10), 1);
        assert_eq!(px_to_col(13.0, 8.0, 10), 2);
        assert_eq!(px_to_col(999.0, 8.0, 4), 4); // clamp to max_col
        assert_eq!(px_to_col(-5.0, 8.0, 10), 0);
        assert_eq!(col_to_px(3, 8.0), 24.0);
    }

    #[test]
    fn word_range_double_click() {
        let line = "foo_bar.baz(qux)";
        assert_eq!(word_range_in_line(line, 0), 0..7); // foo_bar
        assert_eq!(word_range_in_line(line, 4), 0..7);
        assert_eq!(word_range_in_line(line, 8), 8..11); // baz
        assert_eq!(word_range_in_line(line, 7), 7..8); // '.' → single char
        assert_eq!(word_range_in_line(line, 12), 12..15); // qux
    }

    #[test]
    fn ident_range_before_caret() {
        let buf = Buffer::from_text("  foo.bar");
        // caret at end (after "bar")
        let r = ident_range_before(&buf, buf.len_bytes());
        assert_eq!(&"  foo.bar"[r.clone()], "bar");
        // caret right after "foo"
        let r2 = ident_range_before(&buf, 5);
        assert_eq!(&"  foo.bar"[r2], "foo");
    }

    #[test]
    fn diag_span_single_and_multiline() {
        // Single-line: row 2, cols 4..9.
        assert_eq!(diag_span_on_row((2, 4), (2, 9), 2, 20), Some((4, 9)));
        // Row outside range.
        assert_eq!(diag_span_on_row((2, 4), (2, 9), 1, 20), None);
        assert_eq!(diag_span_on_row((2, 4), (2, 9), 3, 20), None);
        // Multi-line 1:3 .. 3:5 over a 12-col middle line.
        assert_eq!(diag_span_on_row((1, 3), (3, 5), 1, 12), Some((3, 12))); // first: to EOL
        assert_eq!(diag_span_on_row((1, 3), (3, 5), 2, 12), Some((0, 12))); // interior: whole
        assert_eq!(diag_span_on_row((1, 3), (3, 5), 3, 12), Some((0, 5))); // last: to end col
    }

    #[test]
    fn diagnostic_underlines_bytes() {
        let buf = Buffer::from_text("int x = 1;\nlong yy = 2;\n");
        // Diagnostic on row 1, cols 5..7 (the "yy").
        let d = Diagnostic {
            range: LspR {
                start: Position::new(1, 5),
                end: Position::new(1, 7),
            },
            severity: Some(DiagnosticSeverity::ERROR),
            ..Default::default()
        };
        let runs = diagnostic_underlines_for_row(&[d], 1, &buf, 0xff0000, 0xffaa00, 0x0000ff);
        assert_eq!(runs.len(), 1);
        assert_eq!(runs[0].0, 5..7);
        assert_eq!(runs[0].1, 0xff0000);
    }

    #[test]
    fn diagnostics_at_finds_the_squiggle_under_the_pointer() {
        let buf = Buffer::from_text("int x = 1;\nlong yy = 2;\n");
        let mk = |sl, sc, el, ec, msg: &str| Diagnostic {
            range: LspR {
                start: Position::new(sl, sc),
                end: Position::new(el, ec),
            },
            message: msg.to_string(),
            ..Default::default()
        };
        let ds = vec![
            mk(1, 5, 1, 7, "unused variable 'yy'"),
            mk(0, 4, 1, 4, "spans two rows"),
            mk(0, 4, 0, 4, "zero width"),
        ];
        // Inside "yy" and inside the two-row span's tail.
        let hit: Vec<_> = diagnostics_at(&ds, &buf, 1, 5)
            .iter()
            .map(|d| d.message.as_str())
            .collect();
        assert_eq!(hit, vec!["unused variable 'yy'"]);
        let hit: Vec<_> = diagnostics_at(&ds, &buf, 1, 2)
            .iter()
            .map(|d| d.message.as_str())
            .collect();
        assert_eq!(hit, vec!["spans two rows"]);
        // The zero-width diagnostic covers one column, so col 4 hits both.
        let hit: Vec<_> = diagnostics_at(&ds, &buf, 0, 4)
            .iter()
            .map(|d| d.message.as_str())
            .collect();
        assert_eq!(hit, vec!["spans two rows", "zero width"]);
        // Past the end of "yy": nothing.
        assert!(diagnostics_at(&ds, &buf, 1, 7).is_empty());
    }

    #[test]
    fn diagnostic_counts_by_severity() {
        let mk = |s| Diagnostic {
            severity: Some(s),
            ..Default::default()
        };
        let ds = vec![
            mk(DiagnosticSeverity::ERROR),
            mk(DiagnosticSeverity::ERROR),
            mk(DiagnosticSeverity::WARNING),
            mk(DiagnosticSeverity::INFORMATION),
        ];
        assert_eq!(diagnostic_counts(&ds), (2, 1, 1));
    }

    #[test]
    fn completion_filter_prefix() {
        let mk = |l: &str| CompletionItem {
            label: l.to_string(),
            ..Default::default()
        };
        let items = vec![
            mk("push_back"),
            mk("pop_back"),
            mk("push_front"),
            mk("size"),
        ];
        let f = completion_filter(&items, "pu");
        assert_eq!(f, vec![0, 2]); // push_back, push_front
        assert_eq!(completion_filter(&items, "").len(), 4);
        assert_eq!(completion_filter(&items, "zzz").len(), 0);
    }

    #[test]
    fn completion_edit_text_edit_range() {
        // Buffer: "  v.pu" — caret after "pu"; a textEdit replacing cols 4..6 of
        // row 0 with "push_back".
        let buf = Buffer::from_text("  v.pu");
        let item = CompletionItem {
            label: "push_back".into(),
            text_edit: Some(CompletionTextEdit::Edit(TextEdit {
                range: LspR {
                    start: Position::new(0, 4),
                    end: Position::new(0, 6),
                },
                new_text: "push_back".into(),
            })),
            ..Default::default()
        };
        let (range, text) = completion_edit(&item, &buf, 4..6);
        assert_eq!(range, 4..6);
        assert_eq!(text, "push_back");
    }

    #[test]
    fn completion_edit_insert_text_fallback() {
        let buf = Buffer::from_text("v.pu");
        let item = CompletionItem {
            label: "push_back".into(),
            insert_text: Some("push_back".into()),
            ..Default::default()
        };
        let (range, text) = completion_edit(&item, &buf, 2..4);
        assert_eq!(range, 2..4); // replaces the typed "pu"
        assert_eq!(text, "push_back");
    }

    #[test]
    fn strip_snippet_keeps_parameter_defaults() {
        // The exact clangd shape for a two-arg call.
        assert_eq!(
            strip_snippet("foo(${1:int a}, ${2:int b})"),
            "foo(int a, int b)"
        );
        // Bare tabstops and the final `$0` vanish; `()` stays.
        assert_eq!(strip_snippet("bar($1)$0"), "bar()");
        assert_eq!(strip_snippet("baz(${1})"), "baz()");
        // Choice placeholder keeps the first option.
        assert_eq!(strip_snippet("f(${1|a,b,c|})"), "f(a)");
        // Escapes: literal `$`, `}`, `\`.
        assert_eq!(strip_snippet("price = \\$5"), "price = $5");
        // Nested placeholder: the inner default is kept.
        assert_eq!(strip_snippet("${1:outer ${2:inner}}"), "outer inner");
        // Non-snippet text is untouched.
        assert_eq!(strip_snippet("plain_text"), "plain_text");
    }

    #[test]
    fn completion_edit_expands_snippet_only_when_flagged() {
        use lsp_types::InsertTextFormat;
        let buf = Buffer::from_text("Poi");
        // Snippet-format constructor → expanded, params kept.
        let snip = CompletionItem {
            label: "Point".into(),
            insert_text: Some("Point(${1:int x}, ${2:int y})".into()),
            insert_text_format: Some(InsertTextFormat::SNIPPET),
            ..Default::default()
        };
        let (_r, text) = completion_edit(&snip, &buf, 0..3);
        assert_eq!(text, "Point(int x, int y)");

        // Same text WITHOUT the snippet flag is inserted verbatim (never expanded).
        let plain = CompletionItem {
            label: "Point".into(),
            insert_text: Some("Point(${1:int x})".into()),
            ..Default::default()
        };
        let (_r, text) = completion_edit(&plain, &buf, 0..3);
        assert_eq!(text, "Point(${1:int x})");
    }

    #[test]
    fn utf16_doc_conversions_roundtrip() {
        // Mixed BMP + astral: "aü\n🎉b" — ü is 1 utf16 unit, 🎉 is 2.
        let buf = Buffer::from_text("aü\n🎉b");
        // Byte offsets that fall on char boundaries: a=0, ü=1(2 bytes), \n=3,
        // 🎉=4(4 bytes), b=8, end=9.
        for byte in [0usize, 1, 3, 4, 8, 9] {
            let u = byte_to_doc_utf16(&buf, byte);
            assert_eq!(doc_utf16_to_byte(&buf, u), byte, "byte {byte}");
        }
        // "aü" = a(1) ü(1) = 2 utf16, newline = 1 → row1 starts at utf16 3.
        let row1_start = buf.point_to_offset(Point::new(1, 0));
        assert_eq!(byte_to_doc_utf16(&buf, row1_start), 3);
    }

    #[test]
    fn selection_on_line_clips() {
        // Buffer rows: "abcd\nefgh\n". Row 0 bytes 0..4, row 1 bytes 5..9.
        // Selection bytes 2..7 (spans the newline).
        let sel = Selection::new(2, 7);
        assert_eq!(selection_on_line(sel, 0, 4), Some(2..4)); // row 0: cols 2..4
        assert_eq!(selection_on_line(sel, 5, 9), Some(0..2)); // row 1: cols 0..2
                                                              // A row entirely outside.
        assert_eq!(selection_on_line(sel, 10, 14), None);
    }

    #[test]
    fn merge_line_styles_flattens_overlap() {
        // len 6, syntax color on 0..3, selection on 2..5, underline on 4..6.
        let syntax = vec![Span {
            start: 0,
            end: 3,
            color: 0x111111,
        }];
        let runs = merge_line_styles(6, &syntax, Some(2..5), &[(4..6, 0xff0000)], None, &[]);
        // Expected boundaries: 0..2 (color), 2..3 (color+sel), 3..4 (sel),
        // 4..5 (sel+underline), 5..6 (underline).
        assert_eq!(runs.len(), 5);
        assert_eq!(runs[0].0, 0..2);
        assert_eq!(runs[0].1.color, Some(0x111111));
        assert!(!runs[0].1.selected);
        assert_eq!(runs[1].0, 2..3);
        assert!(runs[1].1.selected && runs[1].1.color == Some(0x111111));
        assert_eq!(runs[3].0, 4..5);
        assert!(runs[3].1.selected && runs[3].1.underline == Some(0xff0000));
    }

    #[test]
    fn find_matches_on_line_clips() {
        // Buffer rows: "abcd\nefgh". Row 0 bytes 0..4, row 1 bytes 5..9.
        // Matches at bytes 1..3 (row 0) and 6..8 (row 1).
        let matches = vec![1..3, 6..8];
        assert_eq!(find_matches_on_line(&matches, 0, 4), vec![1..3]);
        assert_eq!(find_matches_on_line(&matches, 5, 9), vec![1..3]); // line-relative
        assert!(find_matches_on_line(&matches, 10, 14).is_empty());
    }

    #[test]
    fn merge_line_styles_marks_find_matches() {
        // A find match on 1..4, no selection: those cells get find_match set.
        let runs = merge_line_styles(6, &[], None, &[], None, &[1..4]);
        assert_eq!(runs.len(), 1);
        assert_eq!(runs[0].0, 1..4);
        assert!(runs[0].1.find_match && !runs[0].1.selected);
    }
}
/// Typing-latency probe (kept `#[ignore]`d; run explicitly when touching the
/// edit hot path): `cargo test -p jade perf_probe -- --ignored --nocapture`.
/// Point `PERF_FILE` at a real source file, else a synthetic 1.1k-line C++
/// file is generated. History: 73ms/keystroke (query recompiled per edit +
/// unoptimized deps) → 7ms debug / ~1ms release after caching the compiled
/// highlighter and opt-level 2 deps. Next lever if large files hurt:
/// tree-sitter incremental parse (`Tree::edit` + old-tree reparse).
#[cfg(test)]
mod perf_probe {
    use std::time::Instant;

    #[test]
    #[ignore]
    fn keystroke_cost_on_real_file() {
        let text = std::env::var("PERF_FILE")
            .ok()
            .and_then(|p| std::fs::read_to_string(p).ok())
            .unwrap_or_else(|| {
                (0..1100)
                    .map(|i| format!("static int value_{i} = {i} * 42; // filler\n"))
                    .collect()
            });
        let path = std::path::PathBuf::from("/x/perf.cpp");
        let mut tab = crate::editor_view::OpenTab::from_text(
            &path,
            &text,
            crate::highlight::TokenPalette::jade_dark(),
        );
        // Simulate 10 keystrokes at a middle line: insert char + eager bookkeeping.
        let mid = tab.buffer.point_to_offset(jade_buffer::Point::new(500, 0));
        tab.buffer.set_caret(mid);
        let t0 = Instant::now();
        for i in 0..10 {
            tab.buffer.type_char('x');
            let t = Instant::now();
            tab.on_edited(i);
            println!("on_edited: {:?}", t.elapsed());
        }
        println!("10 keystrokes total: {:?}", t0.elapsed());
    }
}

#[cfg(test)]
mod fold_tests {
    use super::*;

    const SRC: &str = "void f() {\n    int a;\n    int b;\n}\nint x;\nnamespace n {\nvoid g() {\n    int c;\n}\n}\n";
    // rows: 0 "void f() {", 1..2 body, 3 "}", 4 "int x;",
    //       5 "namespace n {", 6 "void g() {", 7 body, 8 "}", 9 "}"

    #[test]
    fn fold_regions_finds_multiline_blocks() {
        let m = fold_regions(SRC);
        assert_eq!(m.get(&0), Some(&3));
        assert_eq!(m.get(&5), Some(&9)); // namespace (hierarchical parent)
        assert_eq!(m.get(&6), Some(&8)); // nested fn
        assert!(!m.contains_key(&4));
    }

    #[test]
    fn fold_regions_ignores_braces_in_literals_and_comments() {
        let m = fold_regions("int a; // {\nchar c = '{';\n/* { */\nvoid f() {\nint b;\n}\n");
        assert_eq!(m.len(), 1);
        assert_eq!(m.get(&3), Some(&5));
    }

    #[test]
    fn visible_rows_hide_folded_interior_keep_close() {
        let mut t = OpenTab::from_text(&PathBuf::from("/x/f.cpp"), SRC, TokenPalette::jade_dark());
        assert_eq!(t.visible_rows().len(), t.line_count());
        t.folds.insert(0);
        let vis = t.visible_rows();
        // rows 1..=2 hidden; 0 and the closing 3 stay.
        assert!(vis.starts_with(&[0, 3, 4, 5, 6]));
        // display_row maps hidden rows to the fold start.
        assert_eq!(t.display_row(0), 0);
        assert_eq!(t.display_row(2), 0);
        assert_eq!(t.display_row(3), 1);
        // unfold_containing restores them.
        assert!(t.unfold_containing(2));
        assert_eq!(t.visible_rows().len(), t.line_count());
    }

    #[test]
    fn nested_folds_hierarchical() {
        let mut t = OpenTab::from_text(&PathBuf::from("/x/f.cpp"), SRC, TokenPalette::jade_dark());
        t.folds.insert(5); // fold the namespace: hides 6..=8, keeps 9
        let vis = t.visible_rows();
        assert_eq!(vis, vec![0, 1, 2, 3, 4, 5, 9, 10]); // 10 = trailing empty line
                                                        // folding the inner fn alone (namespace open) hides only 7
        t.folds.clear();
        t.folds.insert(6);
        assert_eq!(t.visible_rows(), vec![0, 1, 2, 3, 4, 5, 6, 8, 9, 10]);
    }
}

#[cfg(test)]
mod path_dedupe_tests {
    use super::*;

    /// The same file opened as `dir/a.csv`, `dir/./a.csv`, and through a
    /// symlinked folder is one tab.
    #[test]
    fn one_tab_per_file_across_path_spellings() {
        let dir = std::env::temp_dir().join(format!("jade-dedupe-{}", std::process::id()));
        let real = dir.join("real");
        std::fs::create_dir_all(&real).unwrap();
        let file = real.join("a.csv");
        std::fs::write(&file, "x,y\n1,2\n").unwrap();
        let link = dir.join("link");
        std::os::unix::fs::symlink(&real, &link).unwrap();

        let mut e = EditorState::new(TokenPalette::default());
        e.open(&file).unwrap();
        e.open(&real.join(".").join("a.csv")).unwrap();
        e.open(&link.join("a.csv")).unwrap();
        e.open_preview(&link.join("a.csv")).unwrap();
        assert_eq!(e.tabs.len(), 1, "{:?}", e.tabs.iter().map(|t| t.path.clone()).collect::<Vec<_>>());
        assert!(e.index_of(&link.join("a.csv")).is_some());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
