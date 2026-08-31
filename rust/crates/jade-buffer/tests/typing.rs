//! Typing helpers: bracket auto-close, type-over, auto-surround, quote rules,
//! Enter auto-indent, the hard-tab Tab key, and the grapheme-aware deletes.

use jade_buffer::{Buffer, Selection};

#[test]
fn typing_plain_char_inserts_and_advances() {
    let mut b = Buffer::from_text("");
    b.type_char('x');
    assert_eq!(b.to_string(), "x");
    assert_eq!(b.selection().caret(), 1);
}

#[test]
fn auto_close_bracket_inserts_pair_caret_between() {
    let mut b = Buffer::from_text("");
    b.type_char('(');
    assert_eq!(b.to_string(), "()");
    assert_eq!(b.selection().caret(), 1); // between the pair
}

#[test]
fn auto_close_all_bracket_kinds() {
    for (open, closed) in [('(', ")"), ('[', "]"), ('{', "}")] {
        let mut b = Buffer::from_text("");
        b.type_char(open);
        assert_eq!(b.to_string(), format!("{open}{closed}"));
        assert_eq!(b.selection().caret(), 1);
    }
}

#[test]
fn type_over_closing_bracket_is_noop_edit() {
    let mut b = Buffer::from_text("");
    b.type_char('('); // "()", caret 1
    let rec = b.type_char(')'); // type over the ')'
    assert_eq!(b.to_string(), "()"); // no new text
    assert_eq!(b.selection().caret(), 2);
    assert!(rec.is_noop());
    assert_eq!(rec.version, b.version()); // version not bumped
}

#[test]
fn type_close_when_not_matching_inserts() {
    let mut b = Buffer::from_text("x");
    b.set_caret(1);
    b.type_char(')'); // nothing to type over → literal insert
    assert_eq!(b.to_string(), "x)");
}

#[test]
fn quote_auto_closes_in_empty_context() {
    let mut b = Buffer::from_text("");
    b.type_char('"');
    assert_eq!(b.to_string(), "\"\"");
    assert_eq!(b.selection().caret(), 1);
}

#[test]
fn quote_does_not_double_after_word_char() {
    // Apostrophe in `don't` must not auto-close.
    let mut b = Buffer::from_text("don");
    b.set_caret(3);
    b.type_char('\'');
    assert_eq!(b.to_string(), "don'");
    assert_eq!(b.selection().caret(), 4);
}

#[test]
fn quote_type_over() {
    let mut b = Buffer::from_text("");
    b.type_char('"'); // "\"\"" caret 1
    b.type_char('"'); // type over
    assert_eq!(b.to_string(), "\"\"");
    assert_eq!(b.selection().caret(), 2);
}

#[test]
fn auto_surround_selection_with_bracket() {
    let mut b = Buffer::from_text("abc");
    b.set_selection(Selection::new(0, 3));
    b.type_char('(');
    assert_eq!(b.to_string(), "(abc)");
    // Selection preserved around the inner text.
    let sel = b.selection();
    assert_eq!((sel.anchor, sel.head), (1, 4));
}

#[test]
fn auto_surround_selection_with_quote() {
    let mut b = Buffer::from_text("abc");
    b.set_selection(Selection::new(0, 3));
    b.type_char('"');
    assert_eq!(b.to_string(), "\"abc\"");
    let sel = b.selection();
    assert_eq!((sel.anchor, sel.head), (1, 4));
}

#[test]
fn typing_over_selection_with_plain_char_replaces() {
    let mut b = Buffer::from_text("abc");
    b.set_selection(Selection::new(0, 3));
    b.type_char('z');
    assert_eq!(b.to_string(), "z");
    assert_eq!(b.selection().caret(), 1);
}

#[test]
fn enter_copies_previous_line_indentation() {
    let mut b = Buffer::from_text("    foo");
    b.set_caret(7);
    b.insert_newline();
    assert_eq!(b.to_string(), "    foo\n    ");
    assert_eq!(b.line(1), "    ");
    assert_eq!(b.selection().caret(), 12);
}

#[test]
fn enter_with_no_indent() {
    let mut b = Buffer::from_text("foo");
    b.set_caret(3);
    b.insert_newline();
    assert_eq!(b.to_string(), "foo\n");
}

#[test]
fn enter_splits_line_and_indents() {
    let mut b = Buffer::from_text("  ab");
    b.set_caret(3); // between a and b
    b.insert_newline();
    assert_eq!(b.to_string(), "  a\n  b");
}

#[test]
fn tab_inserts_a_hard_tab() {
    let mut b = Buffer::from_text("");
    b.insert_tab();
    assert_eq!(b.to_string(), "\t");
    assert_eq!(b.selection().caret(), 1);
}

#[test]
fn tab_inserts_one_char_from_mid_column() {
    let mut b = Buffer::from_text("ab");
    b.set_caret(2);
    b.insert_tab();
    assert_eq!(b.to_string(), "ab\t");
    assert_eq!(b.selection().caret(), 3);
}

#[test]
fn tab_replaces_a_selection() {
    let mut b = Buffer::from_text("abcd");
    b.set_selection(Selection::new(1, 3));
    b.insert_tab();
    assert_eq!(b.to_string(), "a\td");
    assert_eq!(b.selection().caret(), 2);
}

#[test]
fn backspace_removes_a_whole_indent_step() {
    let mut b = Buffer::from_text("");
    b.insert_tab();
    b.insert_tab();
    b.delete_backward();
    assert_eq!(b.to_string(), "\t"); // one press, one indent step
}

#[test]
fn delete_backward_removes_grapheme() {
    let mut b = Buffer::from_text("a🎉");
    b.set_caret(5); // after emoji
    b.delete_backward();
    assert_eq!(b.to_string(), "a"); // whole emoji gone
    assert_eq!(b.selection().caret(), 1);
}

#[test]
fn delete_backward_removes_selection() {
    let mut b = Buffer::from_text("hello");
    b.set_selection(Selection::new(1, 4));
    b.delete_backward();
    assert_eq!(b.to_string(), "ho");
    assert_eq!(b.selection().caret(), 1);
}

#[test]
fn delete_backward_at_start_is_noop() {
    let mut b = Buffer::from_text("abc");
    b.set_caret(0);
    b.delete_backward();
    assert_eq!(b.to_string(), "abc");
}

#[test]
fn delete_forward_removes_grapheme() {
    let mut b = Buffer::from_text("🎉a");
    b.set_caret(0);
    b.delete_forward();
    assert_eq!(b.to_string(), "a");
    assert_eq!(b.selection().caret(), 0);
}

#[test]
fn delete_word_back_removes_previous_word() {
    let mut b = Buffer::from_text("foo bar");
    b.set_caret(7);
    b.delete_word_back();
    assert_eq!(b.to_string(), "foo ");
    assert_eq!(b.selection().caret(), 4);
}

#[test]
fn delete_word_back_stops_at_punctuation() {
    let mut b = Buffer::from_text("foo.bar");
    b.set_caret(7);
    b.delete_word_back(); // removes "bar"
    assert_eq!(b.to_string(), "foo.");
    b.delete_word_back(); // removes "."
    assert_eq!(b.to_string(), "foo");
}

#[test]
fn typed_text_is_undoable_as_a_unit() {
    let mut b = Buffer::from_text("");
    b.type_char('(');
    assert_eq!(b.to_string(), "()");
    b.undo();
    assert_eq!(b.to_string(), "");
}

#[test]
fn insert_newline_then_delete_backward_roundtrips() {
    let mut b = Buffer::from_text("ab");
    b.set_caret(1);
    b.insert_newline();
    assert_eq!(b.to_string(), "a\nb");
    b.delete_backward();
    assert_eq!(b.to_string(), "ab");
}

// ── Block indent / outdent (Tab and ⇧Tab over a selection) ──────────────────

/// Select from row 0 to row 1, so both rows shift.
fn select_rows(b: &mut Buffer, from: usize, to: usize) {
    b.set_selection(Selection::new(from, to));
}

#[test]
fn tab_over_a_multi_row_selection_indents_each_line() {
    let mut b = Buffer::from_text("a\nb\nc");
    select_rows(&mut b, 0, 3); // "a\nb"
    assert!(b.selection_spans_rows());
    b.indent_lines();
    assert_eq!(b.to_string(), "\ta\n\tb\nc"); // row 2 is outside the selection
}

#[test]
fn indent_keeps_the_selection_on_the_same_text() {
    let mut b = Buffer::from_text("a\nb");
    select_rows(&mut b, 0, 3);
    b.indent_lines();
    let sel = b.selection();
    assert_eq!(&b.to_string()[sel.start()..sel.end()], "a\n\tb");
}

#[test]
fn indent_skips_an_empty_line() {
    let mut b = Buffer::from_text("a\n\nb");
    select_rows(&mut b, 0, 4);
    b.indent_lines();
    assert_eq!(b.to_string(), "\ta\n\n\tb"); // no trailing whitespace on row 1
}

#[test]
fn a_selection_ending_at_column_zero_leaves_that_row_alone() {
    let mut b = Buffer::from_text("a\nb\nc");
    select_rows(&mut b, 0, 2); // "a\n" — the caret sits at the start of row 1
    b.indent_lines();
    assert_eq!(b.to_string(), "\ta\nb\nc");
}

#[test]
fn outdent_removes_a_hard_tab() {
    let mut b = Buffer::from_text("\ta\n\tb");
    select_rows(&mut b, 0, 5);
    b.outdent_lines();
    assert_eq!(b.to_string(), "a\nb");
}

#[test]
fn outdent_removes_up_to_four_spaces() {
    let mut b = Buffer::from_text("      a\n  b\nc");
    select_rows(&mut b, 0, 12);
    b.outdent_lines();
    assert_eq!(b.to_string(), "  a\nb\nc"); // 6→2 spaces, 2→0, and c is untouched
}

#[test]
fn outdent_works_with_no_selection() {
    let mut b = Buffer::from_text("\t\tx");
    b.set_caret(3); // caret after the two tabs, no selection
    b.outdent_lines();
    assert_eq!(b.to_string(), "\tx");
    assert_eq!(b.selection().caret(), 2); // the caret rode along with the text
}

#[test]
fn outdent_clamps_a_caret_that_sits_inside_the_removed_run() {
    let mut b = Buffer::from_text("    x");
    b.set_caret(2); // inside the four spaces
    b.outdent_lines();
    assert_eq!(b.to_string(), "x");
    assert_eq!(b.selection().caret(), 0);
}

#[test]
fn outdent_on_a_line_with_no_indent_changes_nothing() {
    let mut b = Buffer::from_text("x\ny");
    select_rows(&mut b, 0, 3);
    b.outdent_lines();
    assert_eq!(b.to_string(), "x\ny");
}

#[test]
fn indent_then_outdent_round_trips() {
    let mut b = Buffer::from_text("a\n  b\n\nc");
    select_rows(&mut b, 0, 8); // the whole buffer
    b.indent_lines();
    b.outdent_lines();
    assert_eq!(b.to_string(), "a\n  b\n\nc");
}

#[test]
fn a_single_row_selection_does_not_span_rows() {
    let mut b = Buffer::from_text("abc");
    select_rows(&mut b, 0, 2);
    assert!(!b.selection_spans_rows()); // Tab inserts one tab over it instead
}
