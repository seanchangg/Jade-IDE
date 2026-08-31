//! `jade-buffer` — the text-buffer core for Jade's own editor.
//!
//! This crate is a pure-logic (no GUI, no async) text model: a rope-backed
//! [`Buffer`] with cursors, grouped undo/redo, LSP-shaped incremental edit
//! records, and dirty tracking. It replaces the read-only `OpenTab`/`editor_view`
//! viewer as the *text source* behind the editor element.
//!
//! # Rope choice: `ropey` (not zed's `rope`)
//! We use [`ropey`](https://docs.rs/ropey) rather than pulling zed's `rope` crate
//! in as a git dependency. Rationale:
//!
//! * **Standalone.** `ropey` is a normal crates.io dependency. Zed's `rope`
//!   drags `sum_tree` and other internals out of the zed checkout, and prior
//!   research flagged the adjacent `text` crate as CRDT/collab-coupled — coupling
//!   we explicitly want to avoid for a single-user editor.
//! * **Native UTF-16.** LSP positions for clangd are UTF-16 code-unit columns.
//!   `ropey` ships `len_utf16_cu` / `char_to_utf16_cu` / `utf16_cu_to_char`, so
//!   [`Buffer::offset_to_lsp`] is exact and O(log n) instead of a hand-rolled
//!   scan.
//! * **All the conversions we need.** byte ↔ char ↔ line (Point) come for free,
//!   and rope edits stay efficient on multi-MB files.
//!
//! Grapheme-aware cursor motion and delete use the `unicode-segmentation` crate.
//!
//! # Coordinate story
//! The **byte offset** into the UTF-8 text is canonical: cursors ([`Selection`]),
//! edit ranges, and undo ops all speak bytes. Two *named-coordinate* views are
//! derived on demand and never stored:
//!
//! | View | Type | Column unit | Used for |
//! |------|------|-------------|----------|
//! | Render/caret | [`Point`] | chars (scalar values) | drawing the grid |
//! | LSP wire | [`LspPosition`] | UTF-16 code units | clangd `didChange`, hover |
//!
//! Files are `\n`-split (no CRLF handling today, matching the old viewer);
//! [`Buffer::line`] strips a single trailing `\n` so line text matches
//! `text.split('\n')`. Conversions are tested against ASCII, `ü` (2-byte UTF-8,
//! 1 UTF-16 unit), `🎉` (4-byte UTF-8, 2 UTF-16 units), and mixed content.
//!
//! # What the editor element calls
//! * **Per keystroke:** one of [`Buffer::type_char`], [`Buffer::insert_newline`]
//!   (or [`Buffer::insert_newline_auto`] with the tab's [`IndentLang`], plus
//!   [`Buffer::auto_reindent_line`] after a typed character),
//!   [`Buffer::insert_tab`], [`Buffer::delete_backward`],
//!   [`Buffer::delete_forward`], [`Buffer::delete_word_back`], a movement op
//!   (`move_left`/`move_word_right`/… with an `extend` flag), or
//!   [`Buffer::undo`]/[`Buffer::redo`]. Each text-changing call returns an
//!   [`EditRecord`] to forward as an LSP `didChange`.
//! * **Per frame:** [`Buffer::line`] for visible rows, [`Buffer::line_count`],
//!   [`Buffer::selections`] / [`Buffer::selection`], [`Buffer::offset_to_point`]
//!   to place carets, and [`Buffer::is_dirty`] for the tab's dirty dot.
//! * **On save:** write `buffer.to_string()`, then [`Buffer::mark_saved`].

mod buffer;
mod edit;
mod indent;
mod movement;
mod point;
mod selection;
mod typing;

pub use buffer::Buffer;
pub use edit::{
    Clock, EditRecord, LspChange, ManualClock, ManualClockHandle, SystemClock, COALESCE_MS,
};
pub use indent::IndentLang;
pub use point::{LspPosition, Point};
pub use selection::{CursorSet, Selection};
pub use typing::{PAIRS, TAB_WIDTH};
