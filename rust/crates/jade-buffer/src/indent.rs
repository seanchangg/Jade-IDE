//! Language-aware auto-indent (`impl Buffer`).
//!
//! Two entry points:
//! * [`Buffer::insert_newline_auto`] — Enter copies the current indent and adds
//!   one step after an opener (`{`, `(`, `begin`, `case`, a Python `:` …).
//! * [`Buffer::auto_reindent_line`] — after a typed character, a line that holds
//!   only a closer (`}`, `end`, `endmodule`, `join` …) re-aligns to the line of
//!   the matching opener. In Verilog the line moves again as the word grows:
//!   `end` aligns to its `begin`, and `endmodule` then aligns to `module`.
//!
//! The scanner works on plain text. It strips `//` and `#` line comments, and
//! it counts word tokens and bracket characters. It does not parse strings or
//! block comments; that trade-off matches the electric-indent behavior of
//! other editors and keeps this crate free of a parser dependency.

use crate::buffer::{Buffer, PlannedEdit};
use crate::edit::{EditKind, EditRecord};
use crate::selection::Selection;
use crate::typing::TAB_WIDTH;

/// The indent dialect of the open file. The app derives this from the file
/// extension; the buffer itself stays language-agnostic.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum IndentLang {
    /// No language rules: Enter copies the current leading whitespace.
    #[default]
    None,
    /// Brace languages (C, C++, CUDA, Metal, Rust, JS …): `{ ( [` indent,
    /// a lone `}` re-aligns to the matching `{`.
    CFamily,
    /// Verilog / SystemVerilog: `begin`/`end` word pairs (plus `module`,
    /// `case`, `fork` … families) indent and re-align.
    Verilog,
    /// Python: a line that ends with `:` indents one step.
    Python,
}

/// Verilog opener words that indent the next line one step.
const VERILOG_OPENERS: &[&str] = &[
    "begin", "fork", "case", "casex", "casez", "randcase", "module", "macromodule",
    "function", "task", "generate", "primitive", "specify", "package", "interface",
];

/// Verilog closer words. Each one ends a [`VERILOG_OPENERS`] block.
const VERILOG_CLOSERS: &[&str] = &[
    "end", "join", "join_any", "join_none", "endcase", "endmodule", "endfunction",
    "endtask", "endgenerate", "endprimitive", "endspecify", "endpackage",
    "endinterface",
];

/// The opener/closer word families for one Verilog closer. The closer list
/// holds every word that closes the same family, so nested pairs count
/// correctly during the backward scan.
fn verilog_pair(closer: &str) -> Option<(&'static [&'static str], &'static [&'static str])> {
    match closer {
        "end" => Some((&["begin"], &["end"])),
        "join" | "join_any" | "join_none" => {
            Some((&["fork"], &["join", "join_any", "join_none"]))
        }
        "endcase" => Some((&["case", "casex", "casez", "randcase"], &["endcase"])),
        "endmodule" => Some((&["module", "macromodule"], &["endmodule"])),
        "endfunction" => Some((&["function"], &["endfunction"])),
        "endtask" => Some((&["task"], &["endtask"])),
        "endgenerate" => Some((&["generate"], &["endgenerate"])),
        "endprimitive" => Some((&["primitive"], &["endprimitive"])),
        "endspecify" => Some((&["specify"], &["endspecify"])),
        "endpackage" => Some((&["package"], &["endpackage"])),
        "endinterface" => Some((&["interface"], &["endinterface"])),
        _ => None,
    }
}

/// The part of `line` before the line comment (`//`, or `#` for Python).
fn code_of(line: &str, lang: IndentLang) -> &str {
    let marker = match lang {
        IndentLang::Python => "#",
        _ => "//",
    };
    match line.find(marker) {
        Some(i) => &line[..i],
        None => line,
    }
}

/// True for a character that can be part of a Verilog keyword or identifier.
fn is_word_char(c: char) -> bool {
    c.is_alphanumeric() || c == '_' || c == '$'
}

/// One scan token: a word, or a single bracket character.
#[derive(PartialEq, Debug)]
enum Tok<'a> {
    Word(&'a str),
    Open,
    Close,
}

/// Tokenize `code` into words and bracket characters, in text order.
fn tokens(code: &str) -> Vec<Tok<'_>> {
    let mut out = Vec::new();
    let mut word_start: Option<usize> = None;
    for (i, c) in code.char_indices() {
        if is_word_char(c) {
            word_start.get_or_insert(i);
            continue;
        }
        if let Some(s) = word_start.take() {
            out.push(Tok::Word(&code[s..i]));
        }
        match c {
            '{' | '(' | '[' => out.push(Tok::Open),
            '}' | ')' | ']' => out.push(Tok::Close),
            _ => {}
        }
    }
    if let Some(s) = word_start.take() {
        out.push(Tok::Word(&code[s..]));
    }
    out
}

/// The net block delta of `code`: openers add one, closers remove one. A
/// positive result means the next line indents one step.
fn line_delta(code: &str, lang: IndentLang) -> i32 {
    match lang {
        IndentLang::None => 0,
        IndentLang::Python => {
            if code.trim_end().ends_with(':') {
                1
            } else {
                0
            }
        }
        IndentLang::CFamily => tokens(code)
            .iter()
            .map(|t| match t {
                Tok::Open => 1,
                Tok::Close => -1,
                Tok::Word(_) => 0,
            })
            .sum(),
        IndentLang::Verilog => tokens(code)
            .iter()
            .map(|t| match t {
                Tok::Open => 1,
                Tok::Close => -1,
                Tok::Word(w) if VERILOG_OPENERS.contains(w) => 1,
                Tok::Word(w) if VERILOG_CLOSERS.contains(w) => -1,
                Tok::Word(_) => 0,
            })
            .sum(),
    }
}

/// True when `rest` (the text after the caret) starts with a closer, so Enter
/// between a pair splits into three lines with the closer on its own line.
fn starts_with_closer(rest: &str, lang: IndentLang) -> bool {
    let rest = rest.trim_start();
    if matches!(rest.chars().next(), Some('}' | ')' | ']')) {
        return true;
    }
    if lang == IndentLang::Verilog {
        let word: String = rest.chars().take_while(|c| is_word_char(*c)).collect();
        return VERILOG_CLOSERS.contains(&word.as_str());
    }
    false
}

/// One indent step in the style of `base` (the indent it extends): spaces when
/// `base` is a non-empty space run, else a hard tab.
fn indent_unit(base: &str) -> String {
    if !base.is_empty() && base.chars().all(|c| c == ' ') {
        " ".repeat(TAB_WIDTH)
    } else {
        "\t".to_string()
    }
}

/// Leading whitespace of `line`, as a byte length.
fn leading_ws_len(line: &str) -> usize {
    line.len() - line.trim_start_matches([' ', '\t']).len()
}

impl Buffer {
    /// Insert a newline at each caret with language-aware indentation:
    /// * Copy the current line's leading whitespace (as [`Buffer::insert_newline`]).
    /// * Add one indent step when the text before the caret opens a block.
    /// * When the caret sits between an opener and its closer, insert two lines
    ///   and keep the closer on its own re-aligned line.
    ///
    /// With [`IndentLang::None`] the behavior equals [`Buffer::insert_newline`].
    pub fn insert_newline_auto(&mut self, lang: IndentLang) -> EditRecord {
        let mut plans = Vec::new();
        let mut finals = Vec::new();
        let mut delta: i64 = 0;
        for sel in self.cursors.selections() {
            let (start, end) = (sel.start(), sel.end());
            let row = self.byte_to_row(start);
            let line = self.line(row).to_string();
            let line_start = self.line_start_byte(row);
            let before = &line[..start - line_start];
            let after_row = self.byte_to_row(end);
            let after_line = self.line(after_row).to_string();
            let rest = &after_line[end - self.line_start_byte(after_row)..];

            // Cap the base indent at the caret, so Enter inside the leading
            // whitespace does not copy whitespace from the right of the caret.
            let base = &before[..leading_ws_len(before)];
            let opens = line_delta(code_of(before, lang), lang) > 0;

            let (text, caret_in_text) = if opens && starts_with_closer(rest, lang) {
                // Split the pair: body line one step deeper, closer re-aligned.
                let body = format!("\n{base}{}", indent_unit(base));
                let closer_line = format!("\n{base}");
                let caret = body.len();
                (format!("{body}{closer_line}"), caret)
            } else if opens {
                let t = format!("\n{base}{}", indent_unit(base));
                let caret = t.len();
                (t, caret)
            } else {
                let t = format!("\n{base}");
                let caret = t.len();
                (t, caret)
            };

            let tlen = text.len();
            plans.push(PlannedEdit {
                start,
                end,
                text,
            });
            let caret = (start as i64 + delta) as usize + caret_in_text;
            finals.push(Selection::at(caret));
            delta += tlen as i64 - (end - start) as i64;
        }
        // A standalone group, as `insert_newline`: one undo removes the line.
        self.transact(plans, finals, false)
    }

    /// Re-align the caret's line after a typed character. When the text before
    /// the caret is only whitespace plus one closer token, the line's leading
    /// whitespace becomes the indent of the matching opener's line.
    ///
    /// Closers: `}` (CFamily and Verilog) and the Verilog closer words. The
    /// call is a no-op for every other line, for multi-cursor states, and when
    /// no opener matches. Coalesces into the typing burst, so one undo removes
    /// the word and the shift together.
    pub fn auto_reindent_line(&mut self, lang: IndentLang) -> EditRecord {
        let noop = EditRecord {
            changes: Vec::new(),
            version: self.version(),
        };
        if !matches!(lang, IndentLang::CFamily | IndentLang::Verilog) {
            return noop;
        }
        let sels = self.cursors.selections();
        let [sel] = sels.as_slice() else {
            return noop;
        };
        if !sel.is_empty() {
            return noop;
        }
        let caret = sel.head;
        let row = self.byte_to_row(caret);
        let line_start = self.line_start_byte(row);
        let line = self.line(row).to_string();
        let head = &line[..caret - line_start];
        let ws_len = leading_ws_len(head);
        let token = &head[ws_len..];

        let target_row = if token == "}" {
            self.match_open_bracket(row, lang)
        } else if lang == IndentLang::Verilog && verilog_pair(token).is_some() {
            self.match_verilog_opener(row, token)
        } else {
            return noop;
        };
        let Some(target_row) = target_row else {
            return noop;
        };

        let target_line = self.line(target_row);
        let target = &target_line[..leading_ws_len(&target_line)];
        if target == &head[..ws_len] {
            return noop;
        }
        let target = target.to_string();
        let tlen = target.len();
        let plans = vec![PlannedEdit {
            start: line_start,
            end: line_start + ws_len,
            text: target,
        }];
        let caret = (caret as i64 + tlen as i64 - ws_len as i64) as usize;

        // The shift belongs to the keystroke that caused it: fold this edit
        // into the newest undo group, then restore the burst bookkeeping so
        // the next typed character still coalesces at the shifted caret.
        let join = !self.undo.boundary
            && self.undo.last_kind == Some(EditKind::Insert)
            && !self.undo.done.is_empty();
        let prev_kind = self.undo.last_kind;
        let record = self.transact(plans, vec![Selection::at(caret)], true);
        if join && self.undo.done.len() >= 2 {
            let g = self.undo.done.pop().unwrap();
            let prev = self.undo.done.last_mut().unwrap();
            prev.ops.extend(g.ops);
            prev.after = g.after;
            prev.time = g.time;
            self.undo.last_kind = prev_kind;
            self.undo.last_carets = vec![caret];
        }
        record
    }

    /// The row of the `{` that matches a `}` at the start of `from_row`. Scans
    /// upward, right to left, and counts nested brace pairs.
    fn match_open_bracket(&self, from_row: usize, lang: IndentLang) -> Option<usize> {
        let mut depth = 1i32;
        for row in (0..from_row).rev() {
            let line = self.line(row);
            let code = code_of(&line, lang);
            for c in code.chars().rev() {
                match c {
                    '}' => depth += 1,
                    '{' => {
                        depth -= 1;
                        if depth == 0 {
                            return Some(row);
                        }
                    }
                    _ => {}
                }
            }
        }
        None
    }

    /// The row of the opener word that matches `closer` at the start of
    /// `from_row` (for example `begin` for `end`, `module` for `endmodule`).
    /// Scans upward, right to left, and counts nested pairs of the same family.
    fn match_verilog_opener(&self, from_row: usize, closer: &str) -> Option<usize> {
        let (openers, closers) = verilog_pair(closer)?;
        let mut depth = 1i32;
        for row in (0..from_row).rev() {
            let line = self.line(row);
            let code = code_of(&line, IndentLang::Verilog);
            for tok in tokens(code).iter().rev() {
                let Tok::Word(w) = tok else { continue };
                if closers.contains(w) {
                    depth += 1;
                } else if openers.contains(w) {
                    depth -= 1;
                    if depth == 0 {
                        return Some(row);
                    }
                }
            }
        }
        None
    }
}
