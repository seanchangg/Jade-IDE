//! Shared context building for the two selection-driven AI features
//! (inventory §4.14 Explain, §4.15 Visualize), plus the Explain card's own
//! state machine.
//!
//! Everything here is pure: no gpui, no tokio, no I/O. The card's element tree
//! lives in [`crate::panels::explain_card`] and the orchestration in
//! [`crate::app`]. That split is what makes the prompt layout, the context
//! window, and every state transition testable without a window — the same
//! shape [`crate::ghost`] uses for inline completion.

use std::ops::Range;
use std::path::{Path, PathBuf};

use jade_ai::{ChatDelta, ChatError, ChatRequest, Effort, StopReason};

// ── Budgets ──────────────────────────────────────────────────────────────────
//
// Sized so a request lands near 3.5k input tokens. The caps elide rather than
// truncate: a fragment that silently loses its last line produces a confident
// explanation of the wrong code, which is worse than an honest gap.

/// Longest selection sent whole. Past this, the middle is elided.
pub const MAX_SELECTION_CHARS: usize = 6_000;
/// Lines of surrounding context on each side of the selection.
///
/// Sized against the served model's window, not against a guess: llama-server
/// reports 32768 tokens for the Qwen2.5-Coder chat tier, and the preset asks
/// for a single slot so a request gets all of it (`jade_ai::presets`). At
/// roughly 3.5 characters per token of source, the whole budget below is about
/// 5k tokens — comfortable there, and negligible against Claude's window.
///
/// Wider is not automatically better. A small model loses detail in the middle
/// of a long context, and prefill is not free, so this buys the lines most
/// likely to carry the reason the code is written the way it is.
pub const CONTEXT_LINES: usize = 30;
/// Cap on that surrounding context, trimmed before the selection is.
pub const MAX_CONTEXT_CHARS: usize = 4_000;
/// Refuse outright past this — the selection is a file, not a fragment.
pub const HARD_SELECTION_LIMIT: usize = 60_000;

/// The marker left where content was removed, so the model can see that
/// something is missing instead of inferring a shorter program.
fn elision(lines: usize) -> String {
    format!("\n… {lines} lines elided …\n")
}

// ── Input ────────────────────────────────────────────────────────────────────

/// One selection, plus the means to read the lines around it.
///
/// `line` is a closure rather than a buffer reference so this module stays free
/// of `jade-buffer` and the tests can supply a `Vec<&str>`.
pub struct Selected<'a> {
    pub path: &'a Path,
    pub language: &'a str,
    /// 0-based buffer rows, inclusive.
    pub start_row: usize,
    pub end_row: usize,
    pub text: &'a str,
    pub line: &'a dyn Fn(usize) -> Option<String>,
    /// Declarations of the symbols the fragment uses, from the language
    /// server. Empty when clangd is not up, which is a degradation and not a
    /// failure — the explanation is just less able to say *why*.
    pub symbols: &'a [SymbolDoc],
    /// The file's declaration outline, from [`file_skeleton`]. Empty when the
    /// language has no parser.
    pub skeleton: &'a str,
}

/// Map a file extension to the language name given to the model.
///
/// Deliberately separate from [`crate::highlight`]'s extension test, which
/// answers "does the C++ grammar apply". Those are different questions: a
/// `.metal` file highlights as C++ but must be described as Metal.
pub fn language_name(path: &Path) -> &'static str {
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| e.to_ascii_lowercase());
    match ext.as_deref() {
        Some("c" | "h") => "c",
        Some("cpp" | "cc" | "cxx" | "c++" | "hpp" | "hxx" | "hh" | "inl") => "cpp",
        Some("m") => "objective-c",
        Some("mm") => "objective-c++",
        Some("metal") => "metal",
        Some("cu" | "cuh") => "cuda",
        Some("py") => "python",
        Some("rs") => "rust",
        Some("ts" | "tsx") => "typescript",
        Some("js" | "jsx") => "javascript",
        Some("sh" | "bash" | "zsh") => "shell",
        Some("json") => "json",
        Some("toml") => "toml",
        Some("md") => "markdown",
        _ => "text",
    }
}

/// Cap `text`, keeping the head and the tail and eliding the middle.
///
/// Head and tail both matter in code: the head carries the signature and the
/// tail carries the return. Dropping either changes what the fragment means.
pub fn elide_middle(text: &str, max_chars: usize) -> String {
    if text.len() <= max_chars {
        return text.to_string();
    }
    let keep = max_chars / 2;
    let head_end = floor_char_boundary(text, keep);
    let tail_start = ceil_char_boundary(text, text.len() - keep);
    let removed = text[head_end..tail_start].lines().count();
    let mut out = String::with_capacity(max_chars + 40);
    out.push_str(&text[..head_end]);
    out.push_str(&elision(removed));
    out.push_str(&text[tail_start..]);
    out
}

/// `str::floor_char_boundary` is still unstable, and slicing a multi-byte
/// character in half panics.
fn floor_char_boundary(s: &str, mut i: usize) -> usize {
    if i >= s.len() {
        return s.len();
    }
    while i > 0 && !s.is_char_boundary(i) {
        i -= 1;
    }
    i
}

fn ceil_char_boundary(s: &str, mut i: usize) -> usize {
    while i < s.len() && !s.is_char_boundary(i) {
        i += 1;
    }
    i.min(s.len())
}

/// Render the numbered context window on one side of the selection.
fn context_block(sel: &Selected<'_>, from: usize, to: usize) -> String {
    let mut out = String::new();
    for row in from..=to {
        match (sel.line)(row) {
            Some(l) => {
                // 1-based numbers, so the model can cite what the user sees.
                out.push_str(&format!("{:>5}  {}\n", row + 1, l.trim_end()));
            }
            None => break,
        }
    }
    out
}

/// Build the user message. Deterministic byte-for-byte, because the layout is
/// part of the cached prefix — a reordered field is a cache miss.
pub fn build_user_message(sel: &Selected<'_>) -> String {
    let name = sel
        .path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("(untitled)");

    let selection = elide_middle(sel.text, MAX_SELECTION_CHARS);

    let before_from = sel.start_row.saturating_sub(CONTEXT_LINES);
    let before = if sel.start_row > 0 {
        context_block(sel, before_from, sel.start_row - 1)
    } else {
        String::new()
    };
    let after = context_block(sel, sel.end_row + 1, sel.end_row + CONTEXT_LINES);

    // Trim context before the selection: the fragment is the question, the
    // surroundings are only there to resolve names.
    let mut context = format!("{before}{after}");
    if context.len() > MAX_CONTEXT_CHARS {
        context = elide_middle(&context, MAX_CONTEXT_CHARS);
    }

    let mut out = String::with_capacity(selection.len() + context.len() + 256);
    out.push_str(&format!("file: {name}\n"));
    out.push_str(&format!("language: {}\n", sel.language));
    out.push_str(&format!(
        "selection: lines {}-{}\n",
        sel.start_row + 1,
        sel.end_row + 1
    ));
    out.push_str("\n--- selection ---\n");
    out.push_str(&selection);
    if !selection.ends_with('\n') {
        out.push('\n');
    }
    if !sel.symbols.is_empty() {
        // Before the surrounding lines: these are the declarations the
        // fragment actually refers to, and a small model reads what comes
        // first most reliably.
        out.push_str("\n--- declarations used by the selection ---\n");
        for sym in sel.symbols {
            out.push_str(&format!("{}: {}\n", sym.name, sym.detail));
        }
    }
    if !sel.skeleton.trim().is_empty() {
        out.push_str("\n--- file outline (declarations only, no bodies) ---\n");
        out.push_str(sel.skeleton);
    }
    if !context.trim().is_empty() {
        out.push_str("\n--- surrounding context ---\n");
        out.push_str(&context);
    }
    out
}

// ── Symbol context ───────────────────────────────────────────────────────────
//
// The reason a fragment is written the way it is usually lives in a definition
// it refers to, not in the lines around it. A ±80-line window catches that only
// when the definition happens to be nearby; asking the language server catches
// it wherever it is, for a few hundred tokens.

/// The most symbols to resolve for one explanation.
///
/// Each costs a language-server round trip and a slice of the prompt. Twelve
/// covers every identifier in a normal fragment; past that the fragment is
/// large enough that the extra names are noise.
pub const MAX_SYMBOLS: usize = 12;
/// Identifiers shorter than this carry no information — `i`, `n`, `dx`.
const MIN_IDENT_LEN: usize = 3;

/// An identifier worth asking the language server about, at the position of its
/// first use in the selection.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Candidate {
    pub name: String,
    /// Absolute buffer row.
    pub row: usize,
    /// Character column within that row.
    pub col: usize,
}

/// What the language server said about one symbol.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SymbolDoc {
    pub name: String,
    /// The hover text, already flattened and trimmed to a useful size.
    pub detail: String,
}

/// Words that are never worth resolving: language keywords, and the handful of
/// types so common that a definition adds nothing.
///
/// Deliberately covers several languages at once. A false positive costs one
/// wasted round trip; a false negative costs a keyword in the prompt pretending
/// to be a symbol, which is worse.
const SKIP: &[&str] = &[
    // control flow and declarations
    "auto", "break", "case", "catch", "class", "const", "constexpr", "continue", "default",
    "delete", "do", "else", "enum", "explicit", "export", "extern", "final", "for", "friend",
    "goto", "if", "import", "include", "inline", "let", "mutable", "namespace", "new",
    "noexcept", "nullptr", "operator", "override", "private", "protected", "public", "return",
    "sizeof", "static", "struct", "switch", "template", "this", "throw", "try", "typedef",
    "typename", "union", "using", "virtual", "void", "volatile", "while", "func", "fn", "pub",
    "impl", "match", "mod", "self", "super", "where", "async", "await", "move", "ref", "def",
    "elif", "lambda", "pass", "raise", "yield", "var", "function", "typeof", "instanceof",
    // primitives
    "bool", "char", "double", "float", "int", "long", "short", "signed", "unsigned", "size_t",
    "int8_t", "int16_t", "int32_t", "int64_t", "uint8_t", "uint16_t", "uint32_t", "uint64_t",
    "true", "false", "nil", "null", "None", "True", "False", "str", "string",
];

/// Identifiers in `text` worth resolving, in order of first use.
///
/// `start_row` is the buffer row `text` begins on, so the returned positions
/// address the real buffer rather than the fragment.
///
/// Skips anything inside a string or a line comment: a word in a message is not
/// a symbol, and resolving it wastes a round trip and pollutes the prompt.
pub fn identifier_candidates(text: &str, start_row: usize) -> Vec<Candidate> {
    let mut out: Vec<Candidate> = Vec::new();
    let mut seen: Vec<String> = Vec::new();

    for (i, line) in text.lines().enumerate() {
        let code = strip_literals_and_comments(line);
        let b = code.as_bytes();
        let mut j = 0;
        while j < b.len() {
            if !is_ident_start(b[j]) {
                j += 1;
                continue;
            }
            let start = j;
            while j < b.len() && is_ident_char(b[j]) {
                j += 1;
            }
            let word = &code[start..j];
            // A member access is the interesting half: in `frame.pos`, ask
            // about `pos` as well, but never about a numeric suffix.
            if word.len() >= MIN_IDENT_LEN
                && !word.chars().all(|c| c.is_ascii_digit())
                && !SKIP.contains(&word)
                && !seen.iter().any(|s| s == word)
            {
                seen.push(word.to_string());
                out.push(Candidate {
                    name: word.to_string(),
                    row: start_row + i,
                    col: code[..start].chars().count(),
                });
                if out.len() >= MAX_SYMBOLS {
                    return out;
                }
            }
        }
    }
    out
}

/// Make fragment-relative candidate columns absolute buffer columns.
///
/// [`identifier_candidates`] addresses the FRAGMENT: its first line begins at
/// the selection start, not at column 0 of the buffer line. A selection that
/// starts mid-line therefore needs every first-line column shifted right by
/// the selection's start column, or the language server hovers the wrong
/// token. Later lines are full buffer lines and need no shift.
pub fn absolutize_columns(
    mut cands: Vec<Candidate>,
    start_row: usize,
    start_col: usize,
) -> Vec<Candidate> {
    for c in &mut cands {
        if c.row == start_row {
            c.col += start_col;
        }
    }
    cands
}

fn is_ident_start(c: u8) -> bool {
    c.is_ascii_alphabetic() || c == b'_'
}

fn is_ident_char(c: u8) -> bool {
    c.is_ascii_alphanumeric() || c == b'_'
}

/// Blank out string literals and trailing line comments, preserving byte
/// offsets so the columns stay correct.
fn strip_literals_and_comments(line: &str) -> String {
    let mut out = String::with_capacity(line.len());
    let mut quote: Option<char> = None;
    let mut escaped = false;
    for (i, c) in line.char_indices() {
        if let Some(q) = quote {
            out.extend(std::iter::repeat_n(' ', c.len_utf8()));
            if escaped {
                escaped = false;
            } else if c == '\\' {
                escaped = true;
            } else if c == q {
                quote = None;
            }
            continue;
        }
        match c {
            '"' | '\'' => {
                quote = Some(c);
                out.push(' ');
            }
            '/' if line[i..].starts_with("//") => {
                out.extend(std::iter::repeat_n(' ', line.len() - i));
                break;
            }
            '#' if line[..i].trim().is_empty() && !line.trim_start().starts_with("#include") => {
                // A Python or shell comment, or a preprocessor line whose
                // directive is not itself a symbol.
                out.extend(std::iter::repeat_n(' ', line.len() - i));
                break;
            }
            _ => out.push(c),
        }
    }
    out
}

/// Trim a hover string down to what is worth spending prompt on.
///
/// clangd returns the declaration, then often a documentation comment and a
/// size/offset note. The declaration is the part that explains why the calling
/// code is shaped as it is; the rest is usually longer than the fragment.
pub fn condense_hover(raw: &str) -> String {
    let mut out = String::new();
    for line in raw.lines() {
        let line = line.trim();
        // clangd fences the declaration in a code block; keep the contents.
        if line.is_empty() || line.starts_with("```") || line.starts_with("---") {
            continue;
        }
        // Drop clangd's layout notes — true, and never the reason for anything.
        if line.starts_with("// In ") || line.starts_with("size = ") || line.starts_with("offset = ")
        {
            continue;
        }
        if !out.is_empty() {
            out.push(' ');
        }
        out.push_str(line);
        if out.len() >= 200 {
            break;
        }
    }
    let out = out.trim().to_string();
    if out.len() > 200 {
        let cut = (0..=200).rev().find(|i| out.is_char_boundary(*i)).unwrap_or(0);
        format!("{}…", &out[..cut])
    } else {
        out
    }
}

// ── File skeleton ────────────────────────────────────────────────────────────

/// Cap on the declaration outline.
pub const MAX_SKELETON_CHARS: usize = 3_000;

/// The file's declarations, without any method bodies.
///
/// A line window shows what is physically near the selection, which is not the
/// same as what is relevant to it. The outline shows the shape of the whole
/// file for a fraction of the tokens: every function signature, every type with
/// its fields, every global — and nothing of what those functions do.
///
/// `exclude` is the selection's own row range (0-based, inclusive); its
/// declarations are dropped because the reader can already see them.
///
/// Only meaningful for the languages [`crate::structure`] parses, which is the
/// C family. Callers gate on [`has_skeleton`].
pub fn file_skeleton(source: &str, exclude: std::ops::RangeInclusive<usize>) -> String {
    let symbols = crate::structure::parse_symbols(source);
    let lines: Vec<&str> = source.lines().collect();
    let mut out = String::new();
    for sym in &symbols {
        emit_symbol(sym, 0, &lines, &exclude, &mut out);
        if out.len() >= MAX_SKELETON_CHARS {
            out.push_str("…\n");
            break;
        }
    }
    out
}

/// Whether an outline is available for this language.
pub fn has_skeleton(language: &str) -> bool {
    matches!(
        language,
        "c" | "cpp" | "objective-c" | "objective-c++" | "metal" | "cuda"
    )
}

fn emit_symbol(
    sym: &crate::structure::Symbol,
    depth: usize,
    lines: &[&str],
    exclude: &std::ops::RangeInclusive<usize>,
    out: &mut String,
) {
    use crate::structure::SymbolKind;

    // `Symbol::line` is 1-based.
    let row = sym.line.saturating_sub(1);
    let inside_selection = exclude.contains(&row);

    if !inside_selection {
        if let Some(text) = lines.get(row) {
            let text = text.trim_end();
            // Drop a trailing `{`: the body is exactly what this omits.
            let decl = text.trim_end().trim_end_matches('{').trim_end();
            if !decl.is_empty() {
                for _ in 0..depth {
                    out.push_str("  ");
                }
                out.push_str(decl);
                out.push('\n');
            }
        }
    }

    // Recurse only into the kinds that CONTAIN declarations. A function's
    // children are locals and nested lambdas — the body this is meant to omit.
    let container = matches!(
        sym.kind,
        SymbolKind::Namespace | SymbolKind::Class | SymbolKind::Struct | SymbolKind::Enum
    );
    if !container {
        return;
    }
    for child in &sym.children {
        if out.len() >= MAX_SKELETON_CHARS {
            return;
        }
        emit_symbol(child, depth + 1, lines, exclude, out);
    }
}

// ── Explain: the prompt ──────────────────────────────────────────────────────

/// Held byte-stable on purpose: it is the cached prefix. Editing it invalidates
/// every user's cache once, which is fine — editing it per request is not.
pub const EXPLAIN_SYSTEM: &str = "\
You explain a fragment of source code to the developer who is reading it, in a \
small card in their editor.

Explain ONLY the lines under \"--- selection ---\". Everything else in the \
message exists to help you read those lines: the declarations tell you what the \
names mean, the file outline tells you where the fragment sits, and the \
surrounding lines give you the immediate neighbourhood. Never explain that \
material for its own sake, and never describe the file as a whole.

Cover three things, in this order, as one flowing explanation.

Start with what the code does. Then say how it does it, and name the mechanism \
rather than the syntax. Then say why it is written this way: give the reason \
the author had, which is the constraint it satisfies, the case it guards \
against, or the alternative it avoids. If the context does not show that \
reason, write that you cannot tell it from this fragment. Never invent a reason.

Write nothing else. Do not greet the reader. Do not announce what you will do. \
Do not offer to say more. Do not repeat the code line by line.

Write in ASD-STE100 Simplified Technical English:
- Use the simple word. Use one word for one meaning, and the same term each time.
- Keep every sentence short. Use a maximum of 25 words in a sentence.
- Give one idea in each sentence.
- Use the active voice. Do not use the passive voice.
- Use the simple tenses: past, present, and future.
- Do not use the -ing form as a noun or as an adjective.
- Keep the articles: a, an, the.
- Do not use contractions. Write \\\"do not\\\", not \\\"don't\\\".
- Do not use a noun cluster of more than three words.
- Do not use slang, jargon, idioms, or humor.
- Limit a paragraph to six sentences.

Write continuous prose. Never number anything. Never write a list. Never label \
a sentence with the part it covers. The reader must not be able to tell that \
you were given a checklist.

Length: 50 to 120 words, in two or three short paragraphs.

Formatting: put identifiers, types, and expressions in backticks. You may use \
**bold** for a term you define. Do not use headings, code fences, or tables. Do \
not use a list unless the fragment really does hold separate steps.

If the surrounding context does not answer a question, say so in one sentence. \
Do not guess.";

pub const EXPLAIN_MAX_TOKENS: u32 = 2048;

/// Build the Explain request.
pub fn explain_request(sel: &Selected<'_>) -> ChatRequest {
    ChatRequest {
        system: EXPLAIN_SYSTEM.to_string(),
        user: build_user_message(sel),
        max_tokens: EXPLAIN_MAX_TOKENS,
        // Prose about a fragment the reader is looking at is a low-effort task,
        // and this card is judged on arriving fast.
        effort: Effort::Low,
        json_schema: None,
        timeout: std::time::Duration::from_secs(90),
    }
}

// ── Explain: the card ────────────────────────────────────────────────────────

/// Where an Explain request has got to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExplainPhase {
    /// Sent, nothing back yet.
    Requesting,
    /// The model is thinking; still no text.
    Thinking,
    /// Text is arriving.
    Streaming,
    Done,
    Failed(ChatError),
}

/// The Explain card's whole state. Owned by `JadeApp`; the popout window reads
/// the same value, so nothing here may be window-specific.
#[derive(Debug, Clone)]
pub struct ExplainCard {
    /// Bumped per request; deltas from an older generation are dropped.
    pub generation: u64,
    pub path: PathBuf,
    pub language: &'static str,
    /// 0-based, inclusive.
    pub start_row: usize,
    pub end_row: usize,
    /// The byte range the request described, so an edit under the card can
    /// mark it stale instead of silently describing code that no longer exists.
    pub sel_range: Range<usize>,
    pub prose: String,
    pub phase: ExplainPhase,
    pub stale: bool,
    pub popped_out: bool,
    /// `now_ms()` when the card was opened. Drives the loader sweep, the
    /// shimmer, and the per-word resolve — all of which need a clock, and none
    /// of which should own one, since the card already repaints per delta.
    pub started_ms: u64,
    /// `now_ms()` of the first text delta, so the streaming animation measures
    /// from when prose began rather than from when the request went out.
    pub first_text_ms: Option<u64>,
}

impl ExplainCard {
    pub fn new(generation: u64, sel: &Selected<'_>, sel_range: Range<usize>) -> Self {
        ExplainCard {
            generation,
            path: sel.path.to_path_buf(),
            language: language_name(sel.path),
            start_row: sel.start_row,
            end_row: sel.end_row,
            sel_range,
            prose: String::new(),
            phase: ExplainPhase::Requesting,
            stale: false,
            popped_out: false,
            started_ms: 0,
            first_text_ms: None,
        }
    }

    /// Stamp the clock the animations run off. Separate from `new` so the
    /// reducer stays pure and the tests need no clock.
    pub fn started_at(mut self, now_ms: u64) -> Self {
        self.started_ms = now_ms;
        self
    }

    /// Milliseconds of prose animation elapsed, given the current clock.
    pub fn stream_elapsed(&self, now_ms: u64) -> u64 {
        self.first_text_ms
            .map(|t0| now_ms.saturating_sub(t0))
            .unwrap_or(0)
    }

    /// Milliseconds since the card opened — the loader and shimmer clock.
    pub fn elapsed(&self, now_ms: u64) -> u64 {
        now_ms.saturating_sub(self.started_ms)
    }

    /// Apply one delta.
    ///
    /// Terminal phases absorb further deltas rather than reopening: an aborted
    /// task can still have queued text behind its failure, and reviving a
    /// failed card would show prose under an error banner.
    pub fn apply(&mut self, delta: ChatDelta) {
        self.apply_at(delta, 0)
    }

    /// [`apply`](Self::apply) with the clock, for the animation stamps.
    pub fn apply_at(&mut self, delta: ChatDelta, now_ms: u64) {
        if matches!(self.phase, ExplainPhase::Done | ExplainPhase::Failed(_)) {
            return;
        }
        match delta {
            ChatDelta::Started(_) => {
                if self.phase == ExplainPhase::Requesting {
                    self.phase = ExplainPhase::Thinking;
                }
            }
            ChatDelta::Thinking => {
                if !matches!(self.phase, ExplainPhase::Streaming) {
                    self.phase = ExplainPhase::Thinking;
                }
            }
            ChatDelta::Text(t) => {
                if self.first_text_ms.is_none() {
                    self.first_text_ms = Some(now_ms);
                }
                self.prose.push_str(&t);
                self.phase = ExplainPhase::Streaming;
            }
            ChatDelta::Done { stop_reason } => {
                // A truncated explanation is still worth reading — unlike a
                // truncated script — so max_tokens finishes rather than fails.
                let _ = stop_reason;
                self.phase = ExplainPhase::Done;
            }
            ChatDelta::Failed(e) => self.phase = ExplainPhase::Failed(e),
        }
    }

    /// Whether the response was cut short at the token cap.
    pub fn truncated(&self, stop: &StopReason) -> bool {
        matches!(stop, StopReason::MaxTokens)
    }

    /// Header label and whether it should read as an error.
    pub fn status(&self) -> (&'static str, bool) {
        match &self.phase {
            ExplainPhase::Requesting => ("Asking…", false),
            ExplainPhase::Thinking => ("Thinking…", false),
            ExplainPhase::Streaming => ("Explaining…", false),
            ExplainPhase::Done => ("Explained", false),
            ExplainPhase::Failed(_) => ("Failed", true),
        }
    }

    pub fn failure(&self) -> Option<&ChatError> {
        match &self.phase {
            ExplainPhase::Failed(e) => Some(e),
            _ => None,
        }
    }

    /// Retry is offered for a transient failure, and for a card that finished
    /// with nothing to show.
    pub fn can_retry(&self) -> bool {
        match &self.phase {
            ExplainPhase::Failed(e) => e.retryable(),
            ExplainPhase::Done => self.prose.trim().is_empty(),
            _ => false,
        }
    }

    /// Whether the card still has work in flight, and so must be torn down
    /// rather than merely dropped.
    pub fn in_flight(&self) -> bool {
        !matches!(self.phase, ExplainPhase::Done | ExplainPhase::Failed(_))
    }

    /// React to one edit spanning rows `first..=last`, which replaced
    /// `removed_rows + 1` lines with `added_rows + 1`.
    ///
    /// Two distinct jobs, and conflating them is the bug to avoid:
    ///
    ///   - An edit that TOUCHES the explained rows makes the explanation
    ///     possibly wrong, so the card is marked stale.
    ///   - An edit strictly ABOVE that only shifts the rows down or up leaves
    ///     the explanation true, but moves the code. The anchor follows, and
    ///     the card is NOT marked stale — otherwise typing anywhere earlier in
    ///     the file would invalidate every open card.
    ///
    /// Rows rather than byte offsets because that is the unit the edit record
    /// reports, and because it is what the reader sees in the gutter.
    pub fn note_edit_rows(&mut self, first: usize, last: usize, removed: usize, added: usize) {
        // Touching includes abutting: an insert on the line just after the
        // fragment can change what the fragment means (a closing brace, an
        // `else`), and the reader would not see why the card went quiet.
        if first <= self.end_row + 1 && self.start_row <= last + 1 {
            self.stale = true;
            return;
        }
        if last < self.start_row && removed != added {
            let shift = added as isize - removed as isize;
            self.start_row = self.start_row.saturating_add_signed(shift);
            self.end_row = self.end_row.saturating_add_signed(shift);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lines_fn(src: &'static [&'static str]) -> impl Fn(usize) -> Option<String> {
        move |i: usize| src.get(i).map(|s| s.to_string())
    }

    const SRC: &[&str] = &[
        "#include <vector>",
        "",
        "int total(const std::vector<int>& xs) {",
        "    int sum = 0;",
        "    for (auto& x : xs) sum += x;",
        "    return sum;",
        "}",
    ];

    fn sel<'a>(f: &'a dyn Fn(usize) -> Option<String>, text: &'a str) -> Selected<'a> {
        Selected {
            path: Path::new("/w/total.cpp"),
            language: "cpp",
            start_row: 3,
            end_row: 5,
            text,
            line: f,
            symbols: &[],
            skeleton: "",
        }
    }

    // ---- language ------------------------------------------------------

    #[test]
    fn language_names() {
        assert_eq!(language_name(Path::new("a.cpp")), "cpp");
        assert_eq!(language_name(Path::new("a.h")), "c");
        // .metal highlights as C++ but is not C++.
        assert_eq!(language_name(Path::new("a.metal")), "metal");
        assert_eq!(language_name(Path::new("a.mm")), "objective-c++");
        assert_eq!(language_name(Path::new("a.PY")), "python");
        assert_eq!(language_name(Path::new("Makefile")), "text");
    }

    // ---- elision -------------------------------------------------------

    #[test]
    fn short_text_is_untouched() {
        assert_eq!(elide_middle("abc", 10), "abc");
    }

    #[test]
    fn elision_keeps_head_and_tail() {
        let src = format!("HEAD{}TAIL", "x".repeat(500));
        let out = elide_middle(&src, 100);
        assert!(out.starts_with("HEAD"), "{out}");
        assert!(out.ends_with("TAIL"), "{out}");
        assert!(out.contains("elided"), "{out}");
    }

    /// Slicing a multi-byte character in half panics; the cap is in bytes.
    #[test]
    fn elision_respects_char_boundaries() {
        let src = "é".repeat(400); // 800 bytes
        let out = elide_middle(&src, 101); // odd cap → boundary lands mid-char
        assert!(out.contains("elided"));
        assert!(out.chars().all(|c| c == 'é' || !c.is_alphabetic() || c.is_ascii()));
    }

    // ---- message layout -------------------------------------------------

    #[test]
    fn message_has_the_expected_shape() {
        let f = lines_fn(SRC);
        let text = "    int sum = 0;\n    for (auto& x : xs) sum += x;\n    return sum;\n";
        let msg = build_user_message(&sel(&f, text));
        assert!(msg.starts_with("file: total.cpp\nlanguage: cpp\nselection: lines 4-6\n"), "{msg}");
        assert!(msg.contains("--- selection ---"));
        assert!(msg.contains("--- surrounding context ---"));
        assert!(msg.contains("sum += x"));
    }

    /// The numbers must match the gutter the user is looking at.
    #[test]
    fn context_lines_are_one_based() {
        let f = lines_fn(SRC);
        let msg = build_user_message(&sel(&f, "x"));
        assert!(msg.contains("    1  #include <vector>"), "{msg}");
        assert!(msg.contains("    7  }"), "{msg}");
    }

    /// A selection at row 0 must not underflow into the context builder.
    #[test]
    fn selection_at_the_top_of_the_file() {
        let f = lines_fn(SRC);
        let s = Selected {
            path: Path::new("/w/a.cpp"),
            language: "cpp",
            start_row: 0,
            end_row: 0,
            text: "#include <vector>",
            line: &f,
            symbols: &[],
            skeleton: "",
        };
        let msg = build_user_message(&s);
        assert!(msg.contains("selection: lines 1-1"), "{msg}");
    }

    /// And one at the end must stop at EOF rather than emitting blanks.
    #[test]
    fn selection_at_the_end_of_the_file() {
        let f = lines_fn(SRC);
        let s = Selected {
            path: Path::new("/w/a.cpp"),
            language: "cpp",
            start_row: 6,
            end_row: 6,
            text: "}",
            line: &f,
            symbols: &[],
            skeleton: "",
        };
        let msg = build_user_message(&s);
        assert!(msg.contains("selection: lines 7-7"), "{msg}");
        // Nothing after row 6 exists, so no context line past it.
        assert!(!msg.contains("    8  "), "{msg}");
    }

    #[test]
    fn an_oversized_selection_is_elided_not_dropped() {
        let f = lines_fn(SRC);
        let big = "z".repeat(MAX_SELECTION_CHARS * 2);
        let msg = build_user_message(&sel(&f, &big));
        assert!(msg.contains("elided"), "expected an elision marker");
        assert!(msg.len() < MAX_SELECTION_CHARS * 2);
    }

    /// The layout is the cached prefix — same input, same bytes.
    #[test]
    fn message_is_deterministic() {
        let f = lines_fn(SRC);
        let a = build_user_message(&sel(&f, "x"));
        let b = build_user_message(&sel(&f, "x"));
        assert_eq!(a, b);
    }

    #[test]
    fn a_file_with_no_context_still_builds() {
        let none = |_: usize| None;
        let s = Selected {
            path: Path::new("/w/a.cpp"),
            language: "cpp",
            start_row: 0,
            end_row: 0,
            text: "int x;",
            line: &none,
            symbols: &[],
            skeleton: "",
        };
        let msg = build_user_message(&s);
        assert!(msg.contains("int x;"));
        assert!(!msg.contains("--- surrounding context ---"), "{msg}");
    }

    // ---- request --------------------------------------------------------

    #[test]
    fn explain_request_streams_prose() {
        let f = lines_fn(SRC);
        let r = explain_request(&sel(&f, "x"));
        // A schema would turn the whole response into JSON, which is exactly
        // what splitting Explain from Visualize is meant to avoid.
        assert!(r.json_schema.is_none());
        assert_eq!(r.effort, Effort::Low);
        assert_eq!(r.system, EXPLAIN_SYSTEM);
    }

    // ---- card reducer ---------------------------------------------------

    fn card() -> ExplainCard {
        let f = lines_fn(SRC);
        ExplainCard::new(1, &sel(&f, "x"), 40..90)
    }

    #[test]
    fn happy_path_transitions() {
        let mut c = card();
        assert_eq!(c.phase, ExplainPhase::Requesting);
        c.apply(ChatDelta::Started(jade_ai::ChatProviderId::Anthropic));
        assert_eq!(c.phase, ExplainPhase::Thinking);
        c.apply(ChatDelta::Text("Hel".into()));
        assert_eq!(c.phase, ExplainPhase::Streaming);
        c.apply(ChatDelta::Text("lo".into()));
        assert_eq!(c.prose, "Hello");
        c.apply(ChatDelta::Done { stop_reason: StopReason::EndTurn });
        assert_eq!(c.phase, ExplainPhase::Done);
        assert!(!c.in_flight());
    }

    /// A late Thinking must not blank out text that has already arrived.
    #[test]
    fn thinking_after_text_does_not_regress() {
        let mut c = card();
        c.apply(ChatDelta::Text("a".into()));
        c.apply(ChatDelta::Thinking);
        assert_eq!(c.phase, ExplainPhase::Streaming);
    }

    /// An aborted task can queue text behind its terminal delta.
    #[test]
    fn terminal_phases_absorb_later_deltas() {
        let mut c = card();
        c.apply(ChatDelta::Text("a".into()));
        c.apply(ChatDelta::Done { stop_reason: StopReason::EndTurn });
        c.apply(ChatDelta::Text("LATE".into()));
        assert_eq!(c.prose, "a");

        let mut c = card();
        c.apply(ChatDelta::Failed(ChatError::Overloaded));
        c.apply(ChatDelta::Text("LATE".into()));
        assert_eq!(c.prose, "");
        assert_eq!(c.status().0, "Failed");
    }

    #[test]
    fn failure_is_reported_and_may_be_retried() {
        let mut c = card();
        c.apply(ChatDelta::Failed(ChatError::Overloaded));
        assert!(c.can_retry());
        assert!(c.failure().is_some());

        let mut c = card();
        c.apply(ChatDelta::Failed(ChatError::Refusal { category: None }));
        assert!(!c.can_retry(), "a refusal will not fix itself");

        let mut c = card();
        c.apply(ChatDelta::Failed(ChatError::NoCredential));
        assert!(!c.can_retry(), "a missing key is a setup step, not a retry");
    }

    /// A turn that produced nothing is worth retrying even though it succeeded.
    #[test]
    fn an_empty_success_offers_retry() {
        let mut c = card();
        c.apply(ChatDelta::Done { stop_reason: StopReason::EndTurn });
        assert!(c.can_retry());
    }

    #[test]
    fn a_truncated_explanation_still_finishes() {
        let mut c = card();
        c.apply(ChatDelta::Text("half a thought".into()));
        c.apply(ChatDelta::Done { stop_reason: StopReason::MaxTokens });
        assert_eq!(c.phase, ExplainPhase::Done);
        assert!(c.truncated(&StopReason::MaxTokens));
    }

    // ---- staleness -------------------------------------------------------

    // The fixture card covers rows 3..=5.

    #[test]
    fn an_edit_inside_the_fragment_marks_it_stale() {
        let mut c = card();
        c.note_edit_rows(4, 4, 0, 0);
        assert!(c.stale);
    }

    /// An edit on the line just before or after can change what the fragment
    /// means — a closing brace, an `else`.
    #[test]
    fn an_edit_abutting_either_end_marks_it_stale() {
        let mut c = card();
        c.note_edit_rows(2, 2, 0, 0);
        assert!(c.stale);
        let mut c = card();
        c.note_edit_rows(6, 6, 0, 0);
        assert!(c.stale);
    }

    #[test]
    fn an_edit_well_clear_of_it_does_not() {
        let mut c = card();
        c.note_edit_rows(0, 0, 0, 0);
        assert!(!c.stale);
        c.note_edit_rows(20, 20, 0, 0);
        assert!(!c.stale);
    }

    /// Inserting lines above moves the code without changing what it means:
    /// the anchor follows and the card stays valid.
    #[test]
    fn inserting_lines_above_shifts_the_anchor_without_staling() {
        let mut c = card();
        c.note_edit_rows(0, 0, 0, 2);
        assert!(!c.stale, "the fragment did not change");
        assert_eq!((c.start_row, c.end_row), (5, 7));
    }

    #[test]
    fn deleting_lines_above_shifts_the_anchor_back() {
        let mut c = card();
        c.note_edit_rows(0, 1, 1, 0);
        assert!(!c.stale);
        assert_eq!((c.start_row, c.end_row), (2, 4));
    }

    /// A delete above that would push the anchor below zero must clamp, not
    /// underflow.
    #[test]
    fn a_huge_delete_above_clamps_at_zero() {
        let mut c = card();
        c.note_edit_rows(0, 0, 99, 0);
        assert_eq!(c.start_row, 0);
    }

    // ---- symbol candidates ------------------------------------------------

    fn names(c: &[Candidate]) -> Vec<String> {
        c.iter().map(|x| x.name.clone()).collect()
    }

    #[test]
    fn candidates_skip_keywords_and_short_names() {
        let c = identifier_candidates("for (auto& v : verts) sum += v.pos;", 0);
        assert_eq!(names(&c), vec!["verts", "sum", "pos"]);
    }

    /// Positions must address the real buffer, so the language server is asked
    /// about the right place.
    #[test]
    fn candidate_positions_are_absolute() {
        let c = identifier_candidates("alpha\n  beta", 40);
        assert_eq!(c[0], Candidate { name: "alpha".into(), row: 40, col: 0 });
        assert_eq!(c[1], Candidate { name: "beta".into(), row: 41, col: 2 });
    }

    /// A word in a message is not a symbol; resolving it wastes a round trip
    /// and puts a sentence fragment in the prompt.
    #[test]
    fn text_inside_strings_and_comments_is_ignored() {
        let c = identifier_candidates(r#"call(handler); // the handler runs later"#, 0);
        assert_eq!(names(&c), vec!["call", "handler"]);

        let c = identifier_candidates(r#"log("failed to open device");"#, 0);
        assert_eq!(names(&c), vec!["log"]);
    }

    #[test]
    fn an_escaped_quote_does_not_end_the_string() {
        let c = identifier_candidates(r#"log("say \"boo\" now"); after"#, 0);
        assert_eq!(names(&c), vec!["log", "after"]);
    }

    #[test]
    fn each_name_is_asked_about_once() {
        let c = identifier_candidates("frame; frame; frame;", 0);
        assert_eq!(names(&c), vec!["frame"]);
    }

    #[test]
    fn the_candidate_list_is_capped() {
        let src: String = (0..40).map(|i| format!("name{i:02} ")).collect();
        assert_eq!(identifier_candidates(&src, 0).len(), MAX_SYMBOLS);
    }

    /// A selection that starts mid-line must shift its first line's columns
    /// by the start column — and only the first line's.
    #[test]
    fn midline_selections_absolutize_first_line_columns() {
        // Selection text "sum += extra;\nnext(sum);" starting at row 7, col 11.
        let c = identifier_candidates("sum += extra;\nnext(sum);", 7);
        let c = absolutize_columns(c, 7, 11);
        assert_eq!(c[0], Candidate { name: "sum".into(), row: 7, col: 11 });
        assert_eq!(c[1], Candidate { name: "extra".into(), row: 7, col: 18 });
        // The second line is a full buffer line: untouched.
        assert_eq!(c[2], Candidate { name: "next".into(), row: 8, col: 0 });
    }

    /// The columns are character columns, so a tab-indented or non-ASCII line
    /// still resolves at the right place.
    #[test]
    fn columns_count_characters_not_bytes() {
        let c = identifier_candidates("// ué\nrecord_alloc(n);", 0);
        assert_eq!(c[0].name, "record_alloc");
        assert_eq!(c[0].col, 0);
    }

    // ---- hover condensing --------------------------------------------------

    #[test]
    fn condense_keeps_the_declaration_and_drops_the_noise() {
        let raw = "```cpp\nvoid record_alloc(size_t bytes)\n```\n---\n// In namespace probe\nsize = 8 bytes\nRecords one Metal buffer allocation.";
        let got = condense_hover(raw);
        assert!(got.contains("void record_alloc(size_t bytes)"), "{got}");
        assert!(got.contains("Records one Metal buffer allocation."), "{got}");
        assert!(!got.contains("size = 8"), "{got}");
        assert!(!got.contains("```"), "{got}");
    }

    #[test]
    fn condense_bounds_the_length() {
        let got = condense_hover(&"x".repeat(5_000));
        assert!(got.len() <= 205, "{}", got.len());
    }

    #[test]
    fn condense_survives_multibyte_at_the_cut() {
        let got = condense_hover(&"é".repeat(500));
        assert!(got.len() <= 205);
    }

    // ---- the message -------------------------------------------------------

    #[test]
    fn declarations_appear_before_the_surrounding_lines() {
        let f = lines_fn(SRC);
        let syms = vec![SymbolDoc {
            name: "total".into(),
            detail: "int total(const std::vector<int>& xs)".into(),
        }];
        let s = Selected {
            path: Path::new("/w/total.cpp"),
            language: "cpp",
            start_row: 3,
            end_row: 5,
            text: "x",
            line: &f,
            symbols: &syms,
            skeleton: "",
        };
        let msg = build_user_message(&s);
        let d = msg.find("--- declarations used by the selection ---").unwrap();
        let c = msg.find("--- surrounding context ---").unwrap();
        assert!(d < c, "declarations must come first");
        assert!(msg.contains("total: int total(const std::vector<int>& xs)"));
    }

    /// No language server is a degradation, not a failure: the section is
    /// simply absent.
    #[test]
    fn no_symbols_means_no_section() {
        let f = lines_fn(SRC);
        let msg = build_user_message(&sel(&f, "x"));
        assert!(!msg.contains("declarations used by the selection"));
    }

    // ---- file skeleton -----------------------------------------------------

    const CPP: &str = "\
namespace probe {

struct Frame {
  int width;
  int height;
  void reset();
};

static int g_count = 0;

int total(const std::vector<int>& xs) {
  int sum = 0;
  for (auto& x : xs) sum += x;
  return sum;
}

}
";

    #[test]
    fn the_outline_keeps_declarations_and_drops_bodies() {
        let sk = file_skeleton(CPP, 100..=100);
        assert!(sk.contains("struct Frame"), "{sk}");
        assert!(sk.contains("int width"), "fields are attributes: {sk}");
        assert!(sk.contains("int total(const std::vector<int>& xs)"), "{sk}");
        // The body of `total` must not appear — that is the whole point.
        assert!(!sk.contains("sum += x"), "body leaked: {sk}");
        assert!(!sk.contains("return sum"), "body leaked: {sk}");
    }

    /// A trailing brace is the start of the body, not part of the declaration.
    #[test]
    fn declarations_do_not_carry_an_opening_brace() {
        let sk = file_skeleton(CPP, 100..=100);
        for line in sk.lines() {
            assert!(!line.trim_end().ends_with('{'), "{line:?}");
        }
    }

    /// The reader can already see the selection, so it is not repeated.
    #[test]
    fn the_selections_own_declaration_is_excluded() {
        let total_row = CPP.lines().position(|l| l.contains("int total")).unwrap();
        let sk = file_skeleton(CPP, total_row..=total_row + 4);
        assert!(!sk.contains("int total("), "{sk}");
        // Everything else still appears.
        assert!(sk.contains("struct Frame"), "{sk}");
    }

    #[test]
    fn the_outline_is_bounded() {
        let big: String = (0..4000).map(|i| format!("int f{i}(int a);\n")).collect();
        let sk = file_skeleton(&big, 99_999..=99_999);
        assert!(sk.len() <= MAX_SKELETON_CHARS + 64, "{}", sk.len());
    }

    #[test]
    fn skeletons_only_exist_for_parsed_languages() {
        assert!(has_skeleton("cpp"));
        assert!(has_skeleton("objective-c++"));
        assert!(has_skeleton("metal"));
        assert!(!has_skeleton("python"));
        assert!(!has_skeleton("text"));
    }

    #[test]
    fn an_empty_file_yields_an_empty_outline() {
        assert_eq!(file_skeleton("", 0..=0), "");
    }

    #[test]
    fn the_outline_appears_before_the_surrounding_lines() {
        let f = lines_fn(SRC);
        let s = Selected {
            path: Path::new("/w/a.cpp"),
            language: "cpp",
            start_row: 3,
            end_row: 5,
            text: "x",
            line: &f,
            symbols: &[],
            skeleton: "int total(const std::vector<int>& xs)\n",
        };
        let msg = build_user_message(&s);
        let o = msg.find("--- file outline").unwrap();
        let c = msg.find("--- surrounding context ---").unwrap();
        assert!(o < c);
    }
}
