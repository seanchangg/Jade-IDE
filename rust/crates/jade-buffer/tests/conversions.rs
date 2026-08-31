//! Coordinate & size conversions: byte ↔ char ↔ Point (char col) ↔ LspPosition
//! (UTF-16 col), against ASCII, `ü`, `🎉`, and mixed / multi-line content.

use jade_buffer::{Buffer, LspPosition, Point};

// "aü🎉b": a=1B/1char/1cu, ü=2B/1char/1cu, 🎉=4B/1char/2cu, b=1B/1char/1cu.
const MIX: &str = "aü🎉b";

#[test]
fn sizes_ascii() {
    let b = Buffer::from_text("hello");
    assert_eq!(b.len_bytes(), 5);
    assert_eq!(b.len_chars(), 5);
    assert_eq!(b.len_utf16(), 5);
    assert_eq!(b.line_count(), 1);
}

#[test]
fn sizes_multibyte() {
    let b = Buffer::from_text(MIX);
    assert_eq!(b.len_bytes(), 8); // 1+2+4+1
    assert_eq!(b.len_chars(), 4);
    assert_eq!(b.len_utf16(), 5); // emoji is 2 code units
}

#[test]
fn line_count_matches_split_newline() {
    // Mirrors `text.split('\n').count()`.
    assert_eq!(Buffer::from_text("").line_count(), 1);
    assert_eq!(Buffer::from_text("a").line_count(), 1);
    assert_eq!(Buffer::from_text("a\nb").line_count(), 2);
    assert_eq!(Buffer::from_text("a\nb\n").line_count(), 3);
}

#[test]
fn line_strips_trailing_newline() {
    let b = Buffer::from_text("foo\nbar\n");
    assert_eq!(b.line(0), "foo");
    assert_eq!(b.line(1), "bar");
    assert_eq!(b.line(2), "");
}

#[test]
fn point_uses_char_columns() {
    let b = Buffer::from_text(MIX);
    // byte offsets: a@0 ü@1 🎉@3 b@7
    assert_eq!(b.offset_to_point(0), Point::new(0, 0));
    assert_eq!(b.offset_to_point(1), Point::new(0, 1));
    assert_eq!(b.offset_to_point(3), Point::new(0, 2));
    assert_eq!(b.offset_to_point(7), Point::new(0, 3)); // char col 3, not 4
}

#[test]
fn lsp_uses_utf16_columns() {
    let b = Buffer::from_text(MIX);
    // b sits after a(1cu)+ü(1cu)+🎉(2cu) = 4 UTF-16 units.
    assert_eq!(b.offset_to_lsp(7), LspPosition::new(0, 4));
    assert_eq!(b.offset_to_lsp(3), LspPosition::new(0, 2)); // 🎉 at cu 2
}

#[test]
fn point_and_lsp_diverge_on_emoji() {
    let b = Buffer::from_text(MIX);
    // Same byte, different column story: char col 3 vs UTF-16 col 4.
    assert_eq!(b.offset_to_point(7).col, 3);
    assert_eq!(b.offset_to_lsp(7).character, 4);
}

#[test]
fn point_roundtrips() {
    let b = Buffer::from_text(MIX);
    for byte in [0usize, 1, 3, 7, 8] {
        let p = b.offset_to_point(byte);
        assert_eq!(b.point_to_offset(p), byte, "byte {byte}");
    }
}

#[test]
fn lsp_roundtrips() {
    let b = Buffer::from_text(MIX);
    for byte in [0usize, 1, 3, 7, 8] {
        let pos = b.offset_to_lsp(byte);
        assert_eq!(b.lsp_to_offset(pos), byte, "byte {byte}");
    }
}

#[test]
fn conversions_across_lines() {
    let b = Buffer::from_text("ab\nüc\n🎉");
    // line 1 "üc": ü@byte3, c@byte5
    assert_eq!(b.offset_to_point(5), Point::new(1, 1));
    assert_eq!(b.offset_to_lsp(5), LspPosition::new(1, 1));
    // line 2 "🎉": a@0 b@1 \n@2 ü@3 c@5 \n@6 🎉@7..11 → line 2 starts at byte 7.
    assert_eq!(b.offset_to_point(7), Point::new(2, 0));
    // end of emoji is char col 1 / utf16 col 2
    assert_eq!(b.offset_to_point(11), Point::new(2, 1));
    assert_eq!(b.offset_to_lsp(11), LspPosition::new(2, 2));
}

#[test]
fn point_column_clamps_past_line_end() {
    let b = Buffer::from_text("ab\nlongline");
    // Column 99 on the short line clamps to line end (byte 2).
    assert_eq!(b.point_to_offset(Point::new(0, 99)), 2);
}

#[test]
fn lsp_column_clamps_past_line_end() {
    let b = Buffer::from_text("ab\nlongline");
    assert_eq!(b.lsp_to_offset(LspPosition::new(0, 99)), 2);
}

#[test]
fn to_string_roundtrips() {
    let text = "line1\n  indented\n🎉 end";
    assert_eq!(Buffer::from_text(text).to_string(), text);
}

// ── text_range ───────────────────────────────────────────────────────────────

#[test]
fn text_range_returns_just_the_slice() {
    let b = Buffer::from_text("hello world");
    assert_eq!(b.text_range(0..5), "hello");
    assert_eq!(b.text_range(6..11), "world");
    assert_eq!(b.text_range(0..0), "");
}

#[test]
fn text_range_spans_lines() {
    let b = Buffer::from_text("one\ntwo\nthree");
    assert_eq!(b.text_range(4..11), "two\nthr");
}

/// A selection that outlived an edit must not panic the caller.
#[test]
fn text_range_clamps_out_of_bounds() {
    let b = Buffer::from_text("abc");
    assert_eq!(b.text_range(0..999), "abc");
    assert_eq!(b.text_range(999..1000), "");
    assert_eq!(b.text_range(2..1), "");
}

/// Slicing a multi-byte character in half would panic inside ropey.
#[test]
fn text_range_snaps_to_char_boundaries() {
    let b = Buffer::from_text("h\u{e9}llo");
    // Bytes 1..3 are exactly the two bytes of 'é'.
    assert_eq!(b.text_range(1..3), "\u{e9}");
    // A start inside the character widens outward rather than panicking.
    assert_eq!(b.text_range(2..3), "\u{e9}");
}
