//! Edit records, the undo model, and the injectable clock.
//!
//! Every public mutation returns an [`EditRecord`] carrying LSP-shaped
//! incremental changes ([`LspChange`]) so the caller can forward a
//! `textDocument/didChange` without re-diffing. Internally each mutation is
//! stored as an [`UndoGroup`] of [`EditOp`]s plus the cursor state before/after,
//! so undo/redo restores both text and carets.

use crate::buffer::PlannedEdit;
use crate::point::LspPosition;
use crate::selection::Selection;

/// Coalescing window: edits landing within this many milliseconds of the
/// previous one merge into a single undo group. Monaco-ish "typing burst"
/// grouping.
///
/// The window is one of four gates. An edit also has to continue the group: the
/// same [`EditKind`], at the caret the last edit left behind, with no
/// [`crate::Buffer::group_boundary`] between them. Those gates catch the cases a
/// timer alone gets wrong (the user clicks elsewhere and types on), so the
/// window itself can be generous.
pub const COALESCE_MS: u64 = 500;

/// A monotonic millisecond time source, injectable so undo coalescing is
/// deterministic under test. The real editor uses [`SystemClock`]; tests use a
/// [`ManualClock`] they advance by hand.
pub trait Clock {
    fn now_ms(&self) -> u64;
}

/// Wall-clock source: milliseconds since the clock was created. Only *deltas*
/// matter for coalescing, so a relative origin is fine.
pub struct SystemClock {
    start: std::time::Instant,
}

impl Default for SystemClock {
    fn default() -> Self {
        SystemClock {
            start: std::time::Instant::now(),
        }
    }
}

impl Clock for SystemClock {
    fn now_ms(&self) -> u64 {
        self.start.elapsed().as_millis() as u64
    }
}

/// A hand-advanced clock for deterministic tests. Clone the handle you get from
/// [`ManualClock::handle`] before moving the clock into a `Buffer`, then call
/// [`ManualClockHandle::set`] / [`ManualClockHandle::advance`].
#[derive(Clone, Default)]
pub struct ManualClock {
    now: std::rc::Rc<std::cell::Cell<u64>>,
}

/// A shared handle to a [`ManualClock`]'s current time.
#[derive(Clone)]
pub struct ManualClockHandle {
    now: std::rc::Rc<std::cell::Cell<u64>>,
}

impl ManualClock {
    pub fn new() -> Self {
        Self::default()
    }

    /// A handle that can move this clock even after it is boxed into a `Buffer`.
    pub fn handle(&self) -> ManualClockHandle {
        ManualClockHandle {
            now: self.now.clone(),
        }
    }
}

impl ManualClockHandle {
    pub fn set(&self, ms: u64) {
        self.now.set(ms);
    }

    pub fn advance(&self, ms: u64) {
        self.now.set(self.now.get() + ms);
    }

    pub fn now(&self) -> u64 {
        self.now.get()
    }
}

impl Clock for ManualClock {
    fn now_ms(&self) -> u64 {
        self.now.get()
    }
}

/// One LSP incremental change: replace the `[start, end)` UTF-16 range with
/// `new_text`. Positions are in the **pre-edit** document. Within an
/// [`EditRecord`] the changes are ordered for correct sequential application
/// (descending position — see [`EditRecord::changes`]).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LspChange {
    pub start: LspPosition,
    pub end: LspPosition,
    pub new_text: String,
}

/// The result of one public mutation. Feed [`EditRecord::changes`] straight into
/// a `textDocument/didChange` with [`EditRecord::version`] as the document
/// version.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EditRecord {
    /// LSP incremental changes, ordered so a consumer applying them **in order**
    /// against the evolving document stays correct: descending by position, so
    /// editing a later range never invalidates an earlier range's offsets.
    pub changes: Vec<LspChange>,
    /// The buffer version after this edit (monotonic; use as the LSP doc
    /// version). Pure cursor moves and no-op type-overs do not bump it, so such
    /// records have an empty `changes` and a version equal to the prior one.
    pub version: u64,
}

impl EditRecord {
    /// No text changed (a pure caret move, e.g. bracket type-over).
    pub fn is_noop(&self) -> bool {
        self.changes.is_empty()
    }
}

/// One reversible text op within an undo group. `start` is the byte offset in
/// the buffer state produced by all *earlier* ops in the same group (so forward
/// replay applies ops in order, and undo replays inverses in reverse).
#[derive(Clone, Debug)]
pub(crate) struct EditOp {
    pub start: usize,
    pub old: String,
    pub new: String,
}

/// A coalesced unit of undo: the ops that make up the change plus the cursor
/// selections before and after, so undo/redo restores carets too.
#[derive(Clone, Debug)]
pub(crate) struct UndoGroup {
    pub ops: Vec<EditOp>,
    pub before: Vec<Selection>,
    pub after: Vec<Selection>,
    pub time: u64,
}

/// The undo/redo stacks plus coalescing bookkeeping.
#[derive(Default)]
pub(crate) struct UndoStack {
    pub done: Vec<UndoGroup>,
    pub undone: Vec<UndoGroup>,
    /// Set by `group_boundary()`; forces the next edit to start a fresh group.
    pub boundary: bool,
    pub last_time: Option<u64>,
    /// What the newest group did. A run merges only with its own kind.
    pub last_kind: Option<EditKind>,
    /// Where the newest group left each caret, sorted. Empty when it left a
    /// live selection, which always starts a fresh group.
    pub last_carets: Vec<usize>,
}

/// What an edit does, for undo grouping: only a run of one kind merges into one
/// group, so a Backspace never joins the letters it deletes.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum EditKind {
    /// Every cursor inserted text and removed none (typing).
    Insert,
    /// Every cursor removed text and inserted none (Backspace, Delete).
    Delete,
    /// Anything else: a replace, a paste over a selection, a mixed batch.
    Other,
}

impl EditKind {
    /// Classify one transaction's plans (sorted by start, non-overlapping).
    pub fn of(plans: &[PlannedEdit]) -> EditKind {
        if plans.is_empty() {
            EditKind::Other
        } else if plans.iter().all(|p| p.start == p.end && !p.text.is_empty()) {
            EditKind::Insert
        } else if plans.iter().all(|p| p.start < p.end && p.text.is_empty()) {
            EditKind::Delete
        } else {
            EditKind::Other
        }
    }

    /// True when `plans` picks up exactly where `carets` left off, cursor for
    /// cursor. A caret that moved between the two edits (a click, an arrow key,
    /// a jump to another line) breaks the match, so the next edit opens a new
    /// group.
    pub fn continues(self, plans: &[PlannedEdit], carets: &[usize]) -> bool {
        if plans.len() != carets.len() {
            return false;
        }
        match self {
            // Typing goes on at the caret the last keystroke left.
            EditKind::Insert => plans.iter().zip(carets).all(|(p, c)| p.start == *c),
            // Backspace takes the text before the caret, Delete the text after
            // it. Both continue a delete run.
            EditKind::Delete => plans
                .iter()
                .zip(carets)
                .all(|(p, c)| p.start == *c || p.end == *c),
            EditKind::Other => false,
        }
    }
}
