//! The [`Buffer`]: rope-backed text plus cursors, undo, and dirty tracking.
//!
//! Movement ops live in `movement.rs` and typing/editing helpers in `typing.rs`
//! (both `impl Buffer` blocks). This file holds the text core: constructors,
//! coordinate conversions, the transaction engine that all mutations funnel
//! through, undo/redo, and dirty tracking.

use std::borrow::Cow;
use std::ops::Range;

use ropey::Rope;

use crate::edit::{
    Clock, EditKind, EditOp, EditRecord, LspChange, SystemClock, UndoGroup, UndoStack, COALESCE_MS,
};
use crate::point::{LspPosition, Point};
use crate::selection::{CursorSet, Selection};

/// A single planned edit inside a transaction: replace byte range `[start, end)`
/// (in the pre-transaction document) with `text`. Ranges within one batch must
/// be non-overlapping.
pub(crate) struct PlannedEdit {
    pub start: usize,
    pub end: usize,
    pub text: String,
}

/// Full text via `Display`, so `buffer.to_string()` yields the whole buffer.
impl std::fmt::Display for Buffer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.rope)
    }
}

/// A rope-backed text buffer with an editor's worth of state: cursors, grouped
/// undo/redo, LSP-shaped edit records, and dirty tracking.
///
/// # Coordinate story
/// The canonical position is a **byte offset** into the UTF-8 text; cursors and
/// edit ranges use it. Two named-coordinate views are derived on demand:
/// [`Point`] (row + char column, for rendering) and [`LspPosition`] (line +
/// UTF-16 code-unit column, for clangd). Files are `\n`-split; `line(i)` strips a
/// single trailing `\n`, matching the old viewer's `text.split('\n')`.
pub struct Buffer {
    rope: Rope,
    pub(crate) cursors: CursorSet,
    pub(crate) undo: UndoStack,
    version: u64,
    saved_version: u64,
    clock: Box<dyn Clock>,
}

impl Buffer {
    // ---- construction ---------------------------------------------------

    /// Build a buffer from initial text, with a caret at offset 0 and the
    /// wall-clock time source.
    pub fn from_text(text: &str) -> Self {
        Buffer {
            rope: Rope::from_str(text),
            cursors: CursorSet::single(0),
            undo: UndoStack::default(),
            version: 0,
            saved_version: 0,
            clock: Box::new(SystemClock::default()),
        }
    }

    /// Like [`Buffer::from_text`] but with an injected [`Clock`] so undo
    /// coalescing is deterministic under test.
    pub fn with_clock(text: &str, clock: Box<dyn Clock>) -> Self {
        Buffer {
            rope: Rope::from_str(text),
            cursors: CursorSet::single(0),
            undo: UndoStack::default(),
            version: 0,
            saved_version: 0,
            clock,
        }
    }

    /// Crate-internal access to the backing rope (used by `movement.rs`).
    pub(crate) fn rope_ref(&self) -> &Rope {
        &self.rope
    }

    // ---- size / lines ---------------------------------------------------

    /// Total bytes in the buffer.
    pub fn len_bytes(&self) -> usize {
        self.rope.len_bytes()
    }

    /// Total Unicode scalar values (chars) in the buffer.
    pub fn len_chars(&self) -> usize {
        self.rope.len_chars()
    }

    /// Number of lines. Matches `text.split('\n').count()`: `""` → 1,
    /// `"a\n"` → 2 (the trailing empty line counts).
    pub fn line_count(&self) -> usize {
        self.rope.len_lines()
    }

    /// The text of line `row` with any single trailing `\n` stripped. Borrows
    /// from the rope when the line is contiguous, else allocates.
    pub fn line(&self, row: usize) -> Cow<'_, str> {
        let slice = self.rope.line(row);
        match slice.as_str() {
            Some(s) => Cow::Borrowed(s.strip_suffix('\n').unwrap_or(s)),
            None => {
                let s = slice.to_string();
                Cow::Owned(s.strip_suffix('\n').map(str::to_string).unwrap_or(s))
            }
        }
    }

    /// The text of the byte range `range`.
    ///
    /// Exists so a caller that wants one fragment does not pay for the whole
    /// buffer: the obvious `buffer.to_string()[range]` copies every byte in the
    /// file to read a few of them, which on a 20k-line source file is megabytes
    /// per call. Ropey slices the rope instead.
    ///
    /// The range is clamped to the buffer and snapped outward to char
    /// boundaries, so a stale offset returns text rather than panicking.
    pub fn text_range(&self, range: std::ops::Range<usize>) -> String {
        let len = self.rope.len_bytes();
        let start = range.start.min(len);
        let end = range.end.clamp(start, len);
        // A byte range that splits a multi-byte character would panic in
        // ropey; widen to the enclosing characters instead.
        let start_char = self.rope.byte_to_char(start);
        let end_char = self.rope.byte_to_char(end);
        self.rope.slice(start_char..end_char).to_string()
    }

    /// Char length of line `row`, excluding a trailing `\n`.
    pub(crate) fn line_char_len(&self, row: usize) -> usize {
        let slice = self.rope.line(row);
        let mut n = slice.len_chars();
        if n > 0 && slice.char(n - 1) == '\n' {
            n -= 1;
        }
        n
    }

    /// Byte offset of the start of line `row`.
    pub(crate) fn line_start_byte(&self, row: usize) -> usize {
        self.rope.line_to_byte(row)
    }

    /// Byte offset just past the last non-newline char of line `row`.
    pub(crate) fn line_end_byte(&self, row: usize) -> usize {
        let start_char = self.rope.line_to_char(row);
        self.rope.char_to_byte(start_char + self.line_char_len(row))
    }

    /// The row containing byte `offset`.
    pub(crate) fn byte_to_row(&self, offset: usize) -> usize {
        self.rope.byte_to_line(offset)
    }

    // ---- coordinate conversions ----------------------------------------

    /// Byte offset → [`Point`] (row + **char** column).
    pub fn offset_to_point(&self, offset: usize) -> Point {
        let ch = self.rope.byte_to_char(offset);
        let row = self.rope.char_to_line(ch);
        let col = ch - self.rope.line_to_char(row);
        Point::new(row, col)
    }

    /// [`Point`] → byte offset. `col` is clamped to the line's char length so an
    /// out-of-range column (e.g. from a sticky goal column) lands at line end.
    pub fn point_to_offset(&self, point: Point) -> usize {
        let row = point.row.min(self.line_count().saturating_sub(1));
        let line_start = self.rope.line_to_char(row);
        let col = point.col.min(self.line_char_len(row));
        self.rope.char_to_byte(line_start + col)
    }

    /// Byte offset → [`LspPosition`] (line + **UTF-16 code-unit** column). This
    /// is the coordinate clangd speaks.
    pub fn offset_to_lsp(&self, offset: usize) -> LspPosition {
        let ch = self.rope.byte_to_char(offset);
        let line = self.rope.char_to_line(ch);
        let line_start = self.rope.line_to_char(line);
        let character = self.rope.char_to_utf16_cu(ch) - self.rope.char_to_utf16_cu(line_start);
        LspPosition::new(line, character)
    }

    /// [`LspPosition`] → byte offset (inverse of [`Buffer::offset_to_lsp`]).
    pub fn lsp_to_offset(&self, pos: LspPosition) -> usize {
        let line = pos.line.min(self.line_count().saturating_sub(1));
        let line_start_char = self.rope.line_to_char(line);
        let line_start_cu = self.rope.char_to_utf16_cu(line_start_char);
        let max_cu = self.rope.char_to_utf16_cu(line_start_char + self.line_char_len(line));
        let target_cu = (line_start_cu + pos.character).min(max_cu);
        let ch = self.rope.utf16_cu_to_char(target_cu);
        self.rope.char_to_byte(ch)
    }

    /// Total UTF-16 code units in the buffer (handy for LSP full-doc math).
    pub fn len_utf16(&self) -> usize {
        self.rope.len_utf16_cu()
    }

    // ---- cursors --------------------------------------------------------

    /// The primary selection.
    pub fn selection(&self) -> Selection {
        self.cursors.primary()
    }

    /// All selections.
    pub fn selections(&self) -> Vec<Selection> {
        self.cursors.selections()
    }

    /// Read-only access to the cursor set.
    pub fn cursors(&self) -> &CursorSet {
        &self.cursors
    }

    /// Replace all cursors with a single collapsed caret at `offset`.
    pub fn set_caret(&mut self, offset: usize) {
        self.cursors.set_single(offset.min(self.len_bytes()));
    }

    /// Replace the primary (and only) selection.
    pub fn set_selection(&mut self, sel: Selection) {
        self.cursors.set_from_selections(&[sel]);
    }

    /// The char under byte `offset`, or `None` at end-of-buffer.
    pub(crate) fn char_at(&self, offset: usize) -> Option<char> {
        if offset >= self.len_bytes() {
            None
        } else {
            Some(self.rope.char(self.rope.byte_to_char(offset)))
        }
    }

    // ---- dirty tracking -------------------------------------------------

    /// The buffer version (bumped once per text-changing mutation, including
    /// undo/redo). Use as the LSP document version.
    pub fn version(&self) -> u64 {
        self.version
    }

    /// True when the current version differs from the last saved version.
    pub fn is_dirty(&self) -> bool {
        self.version != self.saved_version
    }

    /// Mark the current content as saved (clears [`Buffer::is_dirty`]).
    pub fn mark_saved(&mut self) {
        self.saved_version = self.version;
    }

    // ---- undo grouping --------------------------------------------------

    /// Force the next edit to begin a fresh undo group (breaks time-coalescing).
    pub fn group_boundary(&mut self) {
        self.undo.boundary = true;
    }

    // ---- raw text splice (no cursor / undo bookkeeping) -----------------

    /// Replace bytes `[start, end)` with `text`, returning the removed text.
    /// Pure text mutation — callers handle cursors, undo, and version.
    pub(crate) fn raw_splice(&mut self, start: usize, end: usize, text: &str) -> String {
        let sc = self.rope.byte_to_char(start);
        let ec = self.rope.byte_to_char(end);
        let removed = if ec > sc {
            self.rope.slice(sc..ec).to_string()
        } else {
            String::new()
        };
        if ec > sc {
            self.rope.remove(sc..ec);
        }
        if !text.is_empty() {
            self.rope.insert(sc, text);
        }
        removed
    }

    // ---- the transaction engine ----------------------------------------

    /// Apply a set of non-overlapping planned edits atomically as one undo unit,
    /// set the cursors to `final_selections`, record undo (coalescing with the
    /// previous group when `coalesce` is set and within [`COALESCE_MS`]), bump
    /// the version, and return the LSP-shaped [`EditRecord`].
    ///
    /// `plans` are in pre-transaction byte coordinates. LSP changes are computed
    /// against the pre-edit rope and emitted in descending position order for
    /// correct sequential application.
    pub(crate) fn transact(
        &mut self,
        mut plans: Vec<PlannedEdit>,
        final_selections: Vec<Selection>,
        coalesce: bool,
    ) -> EditRecord {
        if plans.is_empty() {
            // Pure cursor move (e.g. bracket type-over): no text change, no
            // version bump, no undo entry.
            self.cursors.set_from_selections(&final_selections);
            return EditRecord {
                changes: Vec::new(),
                version: self.version,
            };
        }

        let before = self.cursors.selections();

        // Sort ascending by start; batch ranges are non-overlapping.
        plans.sort_by_key(|p| p.start);

        // LSP changes from the *pre-edit* rope, then ordered descending so a
        // sequential consumer never invalidates an earlier range.
        let mut changes: Vec<LspChange> = plans
            .iter()
            .map(|p| LspChange {
                start: self.offset_to_lsp(p.start),
                end: self.offset_to_lsp(p.end),
                new_text: p.text.clone(),
            })
            .collect();
        changes.reverse();

        // Apply left-to-right tracking a running byte delta so each op's stored
        // `start` is valid in the cumulative post-previous-op frame.
        let mut ops: Vec<EditOp> = Vec::with_capacity(plans.len());
        let mut running: i64 = 0;
        for p in &plans {
            let s = (p.start as i64 + running) as usize;
            let e = (p.end as i64 + running) as usize;
            let old = self.raw_splice(s, e, &p.text);
            running += p.text.len() as i64 - (p.end - p.start) as i64;
            ops.push(EditOp {
                start: s,
                old,
                new: p.text.clone(),
            });
        }

        self.cursors.set_from_selections(&final_selections);
        self.version += 1;

        // Where this edit leaves the carets, for the next edit's adjacency test.
        // A live selection anchors nothing: the edit after it replaces text, so
        // it starts its own group.
        let carets: Vec<usize> = if final_selections.iter().all(|s| s.is_empty()) {
            let mut c: Vec<usize> = final_selections.iter().map(|s| s.head).collect();
            c.sort_unstable();
            c
        } else {
            Vec::new()
        };

        // Record / coalesce. A keystroke joins the group before it only when it
        // continues that group: the same kind of edit, at the caret the last one
        // left, inside the time window, with no boundary between them.
        let now = self.clock.now_ms();
        let kind = EditKind::of(&plans);
        let can_coalesce = coalesce
            && self.undo.last_kind == Some(kind)
            && kind.continues(&plans, &self.undo.last_carets)
            && !self.undo.boundary
            && !self.undo.done.is_empty()
            && self
                .undo
                .last_time
                .is_some_and(|t| now.saturating_sub(t) <= COALESCE_MS);

        self.undo.undone.clear();
        if can_coalesce {
            let g = self.undo.done.last_mut().unwrap();
            g.ops.extend(ops);
            g.after = final_selections;
            g.time = now;
        } else {
            self.undo.done.push(UndoGroup {
                ops,
                before,
                after: final_selections,
                time: now,
            });
        }
        // A standalone edit (`coalesce == false`: paste, a completion accept, a
        // block indent) closes its group on both sides, so the typing after it
        // cannot merge back into it either.
        self.undo.boundary = !coalesce;
        self.undo.last_time = Some(now);
        self.undo.last_kind = Some(kind);
        self.undo.last_carets = carets;

        EditRecord {
            changes,
            version: self.version,
        }
    }

    // ---- undo / redo ----------------------------------------------------

    /// Undo the most recent group, restoring text and the pre-edit cursors.
    /// Returns `false` when there is nothing to undo.
    pub fn undo(&mut self) -> bool {
        let Some(group) = self.undo.done.pop() else {
            return false;
        };
        // Revert ops in reverse: each op's inserted text still sits at
        // `[start, start + new.len())` because later ops are above it.
        for op in group.ops.iter().rev() {
            self.raw_splice(op.start, op.start + op.new.len(), &op.old);
        }
        self.cursors.set_from_selections(&group.before);
        self.undo.undone.push(group);
        self.version += 1;
        self.undo.boundary = true;
        self.undo.last_time = None;
        self.undo.last_carets.clear();
        true
    }

    /// Redo the most recently undone group. Returns `false` when there is
    /// nothing to redo.
    pub fn redo(&mut self) -> bool {
        let Some(group) = self.undo.undone.pop() else {
            return false;
        };
        for op in group.ops.iter() {
            self.raw_splice(op.start, op.start + op.old.len(), &op.new);
        }
        self.cursors.set_from_selections(&group.after);
        self.undo.done.push(group);
        self.version += 1;
        self.undo.boundary = true;
        self.undo.last_time = None;
        self.undo.last_carets.clear();
        true
    }

    // ---- public edit entry points --------------------------------------

    /// Replace byte range `range` with `new_text` as a standalone undo group
    /// (never coalesces with adjacent edits). The caret collapses to the end of
    /// the inserted text. This is the general-purpose programmatic edit.
    pub fn edit(&mut self, range: Range<usize>, new_text: &str) -> EditRecord {
        let caret = range.start + new_text.len();
        self.transact(
            vec![PlannedEdit {
                start: range.start,
                end: range.end,
                text: new_text.to_string(),
            }],
            vec![Selection::at(caret)],
            false,
        )
    }

    /// Replace `range` with `new_text` on behalf of the keyboard: the same edit
    /// as [`Buffer::edit`], except it joins the typing burst around it. The
    /// platform text-input path uses this, so one undo takes back a word rather
    /// than one letter.
    pub fn edit_typed(&mut self, range: Range<usize>, new_text: &str) -> EditRecord {
        let caret = range.start + new_text.len();
        self.transact(
            vec![PlannedEdit {
                start: range.start,
                end: range.end,
                text: new_text.to_string(),
            }],
            vec![Selection::at(caret)],
            true,
        )
    }

    /// Apply several non-overlapping edits atomically as one undo group. Each
    /// resulting caret collapses to the end of its inserted text (multi-cursor).
    /// Ranges must not overlap.
    pub fn batch_edit(&mut self, edits: Vec<(Range<usize>, String)>) -> EditRecord {
        let mut plans = Vec::with_capacity(edits.len());
        for (range, text) in edits {
            plans.push(PlannedEdit {
                start: range.start,
                end: range.end,
                text,
            });
        }
        // Final carets: end of each inserted segment, accounting for the running
        // delta of earlier (left) edits.
        plans.sort_by_key(|p| p.start);
        let mut finals = Vec::with_capacity(plans.len());
        let mut running: i64 = 0;
        for p in &plans {
            let s = (p.start as i64 + running) as usize;
            finals.push(Selection::at(s + p.text.len()));
            running += p.text.len() as i64 - (p.end - p.start) as i64;
        }
        self.transact(plans, finals, false)
    }
}
