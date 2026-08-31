//! Typing & editing helpers (`impl Buffer`) that reproduce the Electron editor's
//! feel (§4.1): bracket-pair auto-close with type-over, auto-surround, Enter
//! auto-indent, and a Tab key that inserts a hard tab. Table-driven via
//! [`PAIRS`] so the behavior is auditable and unit-tested.

use crate::buffer::{Buffer, PlannedEdit};
use crate::edit::EditRecord;
use crate::point::Point;
use crate::selection::Selection;

/// Auto-close / surround pairs, `(open, close)`. Quotes are self-closing (open ==
/// close). Metal `/*`→`*/` comment continuation is intentionally out of scope.
pub const PAIRS: &[(char, char)] = &[
    ('(', ')'),
    ('[', ']'),
    ('{', '}'),
    ('"', '"'),
    ('\'', '\''),
];

/// Display columns per tab stop. The code view draws a hard tab to the next
/// multiple of this width.
pub const TAB_WIDTH: usize = 4;

/// If `ch` opens a pair, its closing char.
fn closer_for(ch: char) -> Option<char> {
    PAIRS.iter().find(|(o, _)| *o == ch).map(|(_, c)| *c)
}

/// True if `ch` is a closing char of some pair (or a quote, which is both).
fn is_closer(ch: char) -> bool {
    PAIRS.iter().any(|(_, c)| *c == ch)
}

impl Buffer {
    /// Type a single character with bracket/quote smarts. Handles, per cursor:
    /// auto-surround (non-empty selection + opener), typing-over a selection,
    /// bracket type-over (closing char with the same char to the right → step
    /// over), auto-close (opener → insert the pair, caret between), and plain
    /// insertion. Coalesces into the current undo group.
    pub fn type_char(&mut self, ch: char) -> EditRecord {
        let mut plans: Vec<PlannedEdit> = Vec::new();
        let mut finals: Vec<Selection> = Vec::new();
        // Running byte delta so multi-cursor final positions stay correct.
        let mut delta: i64 = 0;
        let ch_len = ch.len_utf8();

        let sels = self.cursors.selections();
        for sel in sels {
            let shift = |off: usize| (off as i64 + delta) as usize;
            if !sel.is_empty() {
                let (start, end) = (sel.start(), sel.end());
                if let Some(close) = closer_for(ch) {
                    // Auto-surround: wrap the selection, keep it around the inner
                    // text.
                    let inner_len = end - start;
                    plans.push(PlannedEdit {
                        start,
                        end: start,
                        text: ch.to_string(),
                    });
                    plans.push(PlannedEdit {
                        start: end,
                        end,
                        text: close.to_string(),
                    });
                    let a = shift(start) + ch_len;
                    finals.push(Selection::new(a, a + inner_len));
                    delta += ch_len as i64 + close.len_utf8() as i64;
                } else {
                    // Type over the selection.
                    plans.push(PlannedEdit {
                        start,
                        end,
                        text: ch.to_string(),
                    });
                    finals.push(Selection::at(shift(start) + ch_len));
                    delta += ch_len as i64 - (end - start) as i64;
                }
                continue;
            }

            let caret = sel.head;
            let right = self.char_at(caret);
            if is_closer(ch) && right == Some(ch) {
                // Type-over: step across the existing closing char. No text edit.
                finals.push(Selection::at(shift(caret) + ch_len));
            } else if self.should_auto_close(ch, caret) {
                let close = closer_for(ch).unwrap();
                plans.push(PlannedEdit {
                    start: caret,
                    end: caret,
                    text: format!("{ch}{close}"),
                });
                finals.push(Selection::at(shift(caret) + ch_len));
                delta += ch_len as i64 + close.len_utf8() as i64;
            } else {
                plans.push(PlannedEdit {
                    start: caret,
                    end: caret,
                    text: ch.to_string(),
                });
                finals.push(Selection::at(shift(caret) + ch_len));
                delta += ch_len as i64;
            }
        }
        self.transact(plans, finals, true)
    }

    /// Whether typing `ch` at `caret` should auto-close. Brackets always do;
    /// quotes only when not adjacent to a word char on either side (so an
    /// apostrophe in `don't` or a closing string quote isn't doubled).
    fn should_auto_close(&self, ch: char, caret: usize) -> bool {
        let Some(close) = closer_for(ch) else {
            return false;
        };
        if ch != close {
            return true; // a real bracket
        }
        // Quote: suppress when a word char sits immediately left or right.
        let left_word = self.char_before(caret).is_some_and(is_word);
        let right_word = self.char_at(caret).is_some_and(is_word);
        !left_word && !right_word
    }

    /// The char immediately before `offset`, if any.
    fn char_before(&self, offset: usize) -> Option<char> {
        if offset == 0 {
            None
        } else {
            let b = self.grapheme_before(offset);
            self.char_at(b)
        }
    }

    /// Insert a newline at each caret, copying the current line's leading
    /// whitespace (auto-indent). A non-empty selection is replaced.
    ///
    /// Enter closes the undo group on both sides, so one undo takes back the
    /// line you just typed and leaves the lines above it alone.
    pub fn insert_newline(&mut self) -> EditRecord {
        let mut plans = Vec::new();
        let mut finals = Vec::new();
        let mut delta: i64 = 0;
        for sel in self.cursors.selections() {
            let (start, end) = (sel.start(), sel.end());
            let row = self.byte_to_row(start);
            let indent = self.leading_whitespace(row);
            let text = format!("\n{indent}");
            let tlen = text.len();
            plans.push(PlannedEdit {
                start,
                end,
                text,
            });
            let caret = (start as i64 + delta) as usize + tlen;
            finals.push(Selection::at(caret));
            delta += tlen as i64 - (end - start) as i64;
        }
        // `false`: a standalone group, so one undo takes back the line you just
        // typed and leaves the lines above it alone.
        self.transact(plans, finals, false)
    }

    /// Insert one hard tab (U+0009) at each caret. A non-empty selection is
    /// replaced by the tab.
    ///
    /// The buffer holds the tab as a single char, so Backspace removes a whole
    /// indent step and the arrow keys step over one. The code view draws it to
    /// the next [`TAB_WIDTH`] stop (see `DisplayLine`), so the indent still
    /// looks 4 columns wide.
    pub fn insert_tab(&mut self) -> EditRecord {
        let mut plans = Vec::new();
        let mut finals = Vec::new();
        let mut delta: i64 = 0;
        for sel in self.cursors.selections() {
            let (start, end) = (sel.start(), sel.end());
            plans.push(PlannedEdit {
                start,
                end,
                text: "\t".to_string(),
            });
            let caret = (start as i64 + delta) as usize + 1;
            finals.push(Selection::at(caret));
            delta += 1 - (end - start) as i64;
        }
        self.transact(plans, finals, true)
    }

    /// True when a selection covers more than one row. Tab indents the block in
    /// that case, and inserts one tab in every other case.
    pub fn selection_spans_rows(&self) -> bool {
        self.cursors.selections().iter().any(|sel| {
            !sel.is_empty()
                && self.offset_to_point(sel.start()).row != self.offset_to_point(sel.end()).row
        })
    }

    /// Add one hard tab to the start of every line the cursors touch (Tab on a
    /// multi-row selection). An empty line stays empty, so the edit adds no
    /// trailing whitespace. The selections keep the same text.
    pub fn indent_lines(&mut self) -> EditRecord {
        let mut plans = Vec::new();
        for row in self.selected_rows() {
            if self.line(row).is_empty() {
                continue;
            }
            let start = self.point_to_offset(Point::new(row, 0));
            plans.push(PlannedEdit {
                start,
                end: start,
                text: "\t".to_string(),
            });
        }
        self.reindent(plans)
    }

    /// Remove one indent step from the start of every line the cursors touch
    /// (⇧Tab): a hard tab, else up to [`TAB_WIDTH`] spaces. A line that starts
    /// with no whitespace does not change.
    pub fn outdent_lines(&mut self) -> EditRecord {
        let mut plans = Vec::new();
        for row in self.selected_rows() {
            let n = outdent_len(&self.line(row));
            if n == 0 {
                continue;
            }
            let start = self.point_to_offset(Point::new(row, 0));
            plans.push(PlannedEdit {
                start,
                end: start + n,
                text: String::new(),
            });
        }
        self.reindent(plans)
    }

    /// Every row a cursor sits on or a selection covers, sorted and unique. A
    /// selection that ends at column 0 does not reach into that last row, so
    /// the row drops out (else a full-line drag indents one row too many).
    fn selected_rows(&self) -> Vec<usize> {
        let mut rows = Vec::new();
        for sel in self.cursors.selections() {
            let first = self.offset_to_point(sel.start()).row;
            let end = self.offset_to_point(sel.end());
            let last = if end.row > first && end.col == 0 {
                end.row - 1
            } else {
                end.row
            };
            rows.extend(first..=last);
        }
        rows.sort_unstable();
        rows.dedup();
        rows
    }

    /// Apply whole-line indent plans and carry the cursors along, so the
    /// selection still covers the same text after the shift. One press is one
    /// undo step (no coalescing).
    fn reindent(&mut self, plans: Vec<PlannedEdit>) -> EditRecord {
        let finals: Vec<Selection> = self
            .cursors
            .selections()
            .iter()
            .map(|sel| {
                Selection::new(
                    map_offset(&plans, sel.anchor),
                    map_offset(&plans, sel.head),
                )
            })
            .collect();
        self.transact(plans, finals, false)
    }

    /// Insert arbitrary text at each caret / over each selection (e.g. paste).
    /// Standalone undo group.
    pub fn insert_text(&mut self, text: &str) -> EditRecord {
        let mut plans = Vec::new();
        let mut finals = Vec::new();
        let mut delta: i64 = 0;
        for sel in self.cursors.selections() {
            let (start, end) = (sel.start(), sel.end());
            plans.push(PlannedEdit {
                start,
                end,
                text: text.to_string(),
            });
            let caret = (start as i64 + delta) as usize + text.len();
            finals.push(Selection::at(caret));
            delta += text.len() as i64 - (end - start) as i64;
        }
        self.transact(plans, finals, false)
    }

    /// Delete backward: a non-empty selection, else one grapheme before the
    /// caret (grapheme-aware — an emoji deletes as one unit).
    pub fn delete_backward(&mut self) -> EditRecord {
        self.delete_with(|b, sel| {
            if sel.is_empty() {
                b.grapheme_before(sel.head)..sel.head
            } else {
                sel.range()
            }
        })
    }

    /// Delete forward: a non-empty selection, else one grapheme after the caret.
    pub fn delete_forward(&mut self) -> EditRecord {
        self.delete_with(|b, sel| {
            if sel.is_empty() {
                sel.head..b.grapheme_after(sel.head)
            } else {
                sel.range()
            }
        })
    }

    /// Delete the word before the caret (⌥⌫): a non-empty selection, else from
    /// the previous word start to the caret.
    pub fn delete_word_back(&mut self) -> EditRecord {
        self.delete_with(|b, sel| {
            if sel.is_empty() {
                b.word_left(sel.head)..sel.head
            } else {
                sel.range()
            }
        })
    }

    /// Shared deletion driver: `range_for` yields the byte range to remove for
    /// each selection. Carets collapse to each removed range's start.
    fn delete_with(
        &mut self,
        range_for: impl Fn(&Buffer, Selection) -> std::ops::Range<usize>,
    ) -> EditRecord {
        let mut plans = Vec::new();
        let mut finals = Vec::new();
        let mut delta: i64 = 0;
        for sel in self.cursors.selections() {
            let range = range_for(self, sel);
            let caret = (range.start as i64 + delta) as usize;
            finals.push(Selection::at(caret));
            if range.start != range.end {
                delta -= (range.end - range.start) as i64;
                plans.push(PlannedEdit {
                    start: range.start,
                    end: range.end,
                    text: String::new(),
                });
            }
        }
        self.transact(plans, finals, true)
    }

    /// Leading run of spaces/tabs on line `row`.
    fn leading_whitespace(&self, row: usize) -> String {
        self.line(row)
            .chars()
            .take_while(|c| *c == ' ' || *c == '\t')
            .collect()
    }
}

/// The bytes one outdent step removes from the start of `line`: a hard tab, or
/// up to [`TAB_WIDTH`] spaces. Zero when the line starts with other text.
fn outdent_len(line: &str) -> usize {
    if line.starts_with('\t') {
        1
    } else {
        line.chars().take(TAB_WIDTH).take_while(|c| *c == ' ').count()
    }
}

/// Where `off` lands after `plans` apply. The plans are whole-line indent edits:
/// sorted by start, non-overlapping, and each one inside the leading whitespace.
fn map_offset(plans: &[PlannedEdit], off: usize) -> usize {
    let mut delta: i64 = 0;
    for p in plans {
        if off >= p.end {
            delta += p.text.len() as i64 - (p.end - p.start) as i64;
        } else if off > p.start {
            // Inside a run that an outdent removes: clamp to the new line start.
            return (p.start as i64 + delta) as usize + p.text.len();
        } else if off == p.start {
            // An insert at the line start pushes the text, and this offset with
            // it, to the right of the new indent.
            delta += p.text.len() as i64;
        }
        // off < p.start: this plan, and every later one, sits after `off`.
    }
    (off as i64 + delta) as usize
}

fn is_word(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}
