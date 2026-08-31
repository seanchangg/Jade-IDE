//! Language-aware auto-indent: Enter indentation and closer re-alignment
//! (`insert_newline_auto` / `auto_reindent_line`).

use jade_buffer::{Buffer, IndentLang};

/// Type `text` one char at a time through the live-typing path
/// (`edit_typed` + `auto_reindent_line`), as the app does.
fn type_str(b: &mut Buffer, text: &str, lang: IndentLang) {
    for ch in text.chars() {
        let caret = b.selection().caret();
        b.edit_typed(caret..caret, &ch.to_string());
        b.auto_reindent_line(lang);
    }
}

// ---- Enter auto-indent -----------------------------------------------------

#[test]
fn newline_none_copies_indent_only() {
    let mut b = Buffer::from_text("\tfoo {");
    b.set_caret(6);
    b.insert_newline_auto(IndentLang::None);
    assert_eq!(b.to_string(), "\tfoo {\n\t");
}

#[test]
fn newline_after_open_brace_adds_step() {
    let mut b = Buffer::from_text("\tif (x) {");
    b.set_caret(9);
    b.insert_newline_auto(IndentLang::CFamily);
    assert_eq!(b.to_string(), "\tif (x) {\n\t\t");
    assert_eq!(b.selection().caret(), b.len_bytes());
}

#[test]
fn newline_balanced_line_keeps_indent() {
    let mut b = Buffer::from_text("\tint x = f(a);");
    b.set_caret(14);
    b.insert_newline_auto(IndentLang::CFamily);
    assert_eq!(b.to_string(), "\tint x = f(a);\n\t");
}

#[test]
fn newline_between_braces_splits_pair() {
    let mut b = Buffer::from_text("void f() {}");
    b.set_caret(10); // between { and }
    b.insert_newline_auto(IndentLang::CFamily);
    assert_eq!(b.to_string(), "void f() {\n\t\n}");
    // Caret sits at the end of the indented body line.
    assert_eq!(b.selection().caret(), 12);
}

#[test]
fn newline_space_indent_extends_with_spaces() {
    let mut b = Buffer::from_text("    if (x) {");
    b.set_caret(12);
    b.insert_newline_auto(IndentLang::CFamily);
    assert_eq!(b.to_string(), "    if (x) {\n        ");
}

#[test]
fn newline_ignores_brace_in_comment() {
    let mut b = Buffer::from_text("\tx = 1; // open {");
    b.set_caret(17);
    b.insert_newline_auto(IndentLang::CFamily);
    assert_eq!(b.to_string(), "\tx = 1; // open {\n\t");
}

#[test]
fn newline_after_verilog_begin_adds_step() {
    let mut b = Buffer::from_text("\talways @(posedge clk) begin");
    b.set_caret(28);
    b.insert_newline_auto(IndentLang::Verilog);
    assert_eq!(b.to_string(), "\talways @(posedge clk) begin\n\t\t");
}

#[test]
fn newline_after_labeled_begin_adds_step() {
    let mut b = Buffer::from_text("begin : blink");
    b.set_caret(13);
    b.insert_newline_auto(IndentLang::Verilog);
    assert_eq!(b.to_string(), "begin : blink\n\t");
}

#[test]
fn newline_after_module_header_adds_step() {
    let mut b = Buffer::from_text("module top(input clk);");
    b.set_caret(22);
    b.insert_newline_auto(IndentLang::Verilog);
    assert_eq!(b.to_string(), "module top(input clk);\n\t");
}

#[test]
fn newline_after_python_colon_adds_step() {
    let mut b = Buffer::from_text("def f(x):");
    b.set_caret(9);
    b.insert_newline_auto(IndentLang::Python);
    assert_eq!(b.to_string(), "def f(x):\n\t");
}

#[test]
fn newline_mid_line_counts_only_left_text() {
    // Caret before the `{`: the opener moves down, no extra step.
    let mut b = Buffer::from_text("\tif (x) {");
    b.set_caret(8);
    b.insert_newline_auto(IndentLang::CFamily);
    assert_eq!(b.to_string(), "\tif (x) \n\t{");
}

// ---- Closer re-alignment ---------------------------------------------------

#[test]
fn typed_close_brace_aligns_to_open() {
    let mut b = Buffer::from_text("if (x) {\n\tfoo();\n\t");
    b.set_caret(b.len_bytes());
    type_str(&mut b, "}", IndentLang::CFamily);
    assert_eq!(b.to_string(), "if (x) {\n\tfoo();\n}");
    assert_eq!(b.selection().caret(), b.len_bytes());
}

#[test]
fn typed_close_brace_respects_nesting() {
    let mut b = Buffer::from_text("if (a) {\n\tif (b) {\n\t\tx();\n\t}\n\t\t");
    b.set_caret(b.len_bytes());
    type_str(&mut b, "}", IndentLang::CFamily);
    assert_eq!(b.to_string(), "if (a) {\n\tif (b) {\n\t\tx();\n\t}\n}");
}

#[test]
fn verilog_end_aligns_to_matching_begin() {
    let text = "always @(posedge clk) begin\n\tif (rst) begin\n\t\tq <= 0;\n\t";
    let mut b = Buffer::from_text(text);
    b.set_caret(b.len_bytes());
    type_str(&mut b, "end", IndentLang::Verilog);
    // The inner `end` aligns to the inner `begin` (one tab).
    assert!(b.to_string().ends_with("\n\tend"));
}

#[test]
fn verilog_end_grows_into_endmodule_and_realigns() {
    let text = "module top;\n\talways @(clk) begin\n\t\tq <= d;\n\tend\n\t";
    let mut b = Buffer::from_text(text);
    b.set_caret(b.len_bytes());
    // Type `end`: aligns to `begin`? No — that begin already closed, so the
    // scan reaches no opener and the line does not move yet. Continue to
    // `endmodule`: the line aligns to `module` (column 0).
    type_str(&mut b, "endmodule", IndentLang::Verilog);
    assert!(b.to_string().ends_with("\nendmodule"));
}

#[test]
fn verilog_end_moves_back_and_forth() {
    // `end` first aligns to the open `begin`; the same word grown into
    // `endcase` re-aligns to the outer `case`.
    let text = "case (op)\n\t2'b00: begin\n\t\ty = a;\n\t";
    let mut b = Buffer::from_text(text);
    b.set_caret(b.len_bytes());
    type_str(&mut b, "end", IndentLang::Verilog);
    assert!(b.to_string().ends_with("\n\tend"), "end aligns to begin");
    let caret = b.selection().caret();
    b.edit_typed(caret..caret, "\n\t");
    type_str(&mut b, "endcase", IndentLang::Verilog);
    assert!(b.to_string().ends_with("\nendcase"), "endcase aligns to case");
}

#[test]
fn verilog_join_aligns_to_fork() {
    let mut b = Buffer::from_text("initial fork\n\ta();\n\t\t");
    b.set_caret(b.len_bytes());
    type_str(&mut b, "join", IndentLang::Verilog);
    assert!(b.to_string().ends_with("\njoin"));
}

#[test]
fn realign_noop_when_line_has_leading_text() {
    let mut b = Buffer::from_text("x = 1; friend");
    b.set_caret(6);
    let before = b.to_string();
    b.set_caret(b.len_bytes());
    let rec = b.auto_reindent_line(IndentLang::CFamily);
    assert!(rec.is_noop());
    assert_eq!(b.to_string(), before);
}

#[test]
fn realign_noop_without_matching_opener() {
    let mut b = Buffer::from_text("\t\tend");
    b.set_caret(b.len_bytes());
    let rec = b.auto_reindent_line(IndentLang::Verilog);
    assert!(rec.is_noop());
    assert_eq!(b.to_string(), "\t\tend");
}

#[test]
fn realign_undo_coalesces_with_typing() {
    let mut b = Buffer::from_text("if (x) {\n\t");
    b.set_caret(b.len_bytes());
    type_str(&mut b, "}", IndentLang::CFamily);
    assert_eq!(b.to_string(), "if (x) {\n}");
    // One undo removes the `}` and the indent shift together.
    assert!(b.undo());
    assert_eq!(b.to_string(), "if (x) {\n\t");
}
