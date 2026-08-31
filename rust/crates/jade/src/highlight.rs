//! Read-only tree-sitter syntax highlighting (editor-core Step 1).
//!
//! Uses the plain `tree-sitter` runtime + the `tree-sitter-cpp` grammar directly
//! (no zed `language`/`editor` crates). A focused, self-contained highlight query
//! (`HIGHLIGHT_QUERY`) maps grammar captures to the jade-dark token palette
//! (feature inventory §4.2). `.metal` files are parsed with the same C++ grammar
//! (Metal is C++-derived) plus `#any-of?` predicates that recolor the Metal
//! keyword/type sets, and `[[...]]` attribute nodes render as amber annotations
//! (§4.3).
//!
//! Output is **per-line** colored spans (`Vec<Vec<Span>>`, one inner vec per
//! line, byte ranges relative to that line's start). The renderer fills the gaps
//! between spans with the default text color, so a line is fully described by its
//! spans plus the default. Non–C-family extensions get no spans (plain text,
//! still viewable).

use std::path::Path;

use tree_sitter::{Query, QueryCursor, StreamingIterator};

/// jade-dark token palette (§4.2). Kept independent of [`crate::theme::Theme`]
/// so the highlighter (and its tests) need no GPUI/theme coupling; the four
/// syntax colors that also live in `Theme` use identical hex values.
#[derive(Debug, Clone, Copy)]
pub struct TokenPalette {
    pub keyword: u32,      // #56B389 emerald
    pub types: u32,        // #9BB5CF blue-gray
    pub function: u32,     // #8DB2FF periwinkle
    pub string: u32,       // #D4A76A amber (strings + chars)
    pub number: u32,       // #D4A76A amber family
    pub comment: u32,      // #6B7A72 muted green-gray
    pub variable: u32,     // #E2E8E4
    pub preprocessor: u32, // #7A9484
    pub operator: u32,     // #A0B0A8
    pub delimiter: u32,    // #8A9A90
    pub constant: u32,     // literals (true/false/nullptr) — amber family
    pub annotation: u32,   // Metal [[...]] attributes — amber
}

impl TokenPalette {
    pub fn jade_dark() -> Self {
        TokenPalette {
            keyword: 0x56B389,
            types: 0x9BB5CF,
            function: 0x8DB2FF,
            string: 0xD4A76A,
            number: 0xD4A76A,
            comment: 0x6B7A72,
            variable: 0xE2E8E4,
            preprocessor: 0x7A9484,
            operator: 0xA0B0A8,
            delimiter: 0x8A9A90,
            constant: 0xD4A76A,
            annotation: 0xD4A76A,
        }
    }

    /// jade-light token palette (§4.2). Mirrors `Theme::jade_light()`'s
    /// accents (desaturated green/blue/amber on warm charcoal) so syntax stays
    /// legible on the cream background; variable/operator/delimiter darken to
    /// warm-charcoal grays instead of the dark theme's near-whites.
    pub fn jade_light() -> Self {
        TokenPalette {
            keyword: 0x2E7D5B,      // accent (emerald, desaturated)
            types: 0x4A6A88,        // blue-gray
            function: 0x2F5FD0,     // periwinkle
            string: 0x9A6700,       // amber
            number: 0x9A6700,       // amber
            comment: 0x8A8674,      // muted
            variable: 0x373528,     // text (warm charcoal)
            preprocessor: 0x3E6D5D, // light cyan
            operator: 0x5C5A48,     // muted charcoal
            delimiter: 0x6E6C58,    // lighter muted charcoal
            constant: 0x9A6700,     // amber
            annotation: 0x9A6700,   // amber
        }
    }
}

impl Default for TokenPalette {
    fn default() -> Self {
        Self::jade_dark()
    }
}

/// One colored run inside a single line: `[start, end)` are **byte** offsets
/// relative to the start of the line, `color` is a packed `0xRRGGBB`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Span {
    pub start: usize,
    pub end: usize,
    pub color: u32,
}

/// The self-contained highlight query. Named nodes are preferred over anonymous
/// tokens for robustness; the `#any-of?` predicates recolor Metal keyword/type
/// sets when the C++ grammar parses a `.metal` file.
pub const HIGHLIGHT_QUERY: &str = r##"
; ── Comments ──
(comment) @comment

; ── Preprocessor ──
(preproc_directive) @preprocessor
[
  "#include" "#define" "#if" "#ifdef" "#ifndef"
  "#else" "#elif" "#elifdef" "#elifndef" "#endif"
] @preprocessor
(preproc_include (system_lib_string) @string)
(preproc_def name: (identifier) @preprocessor.name)
(preproc_function_def name: (identifier) @preprocessor.name)

; ── Strings / chars / numbers ──
(string_literal) @string
(raw_string_literal) @string
(char_literal) @string
(system_lib_string) @string
(number_literal) @number

; ── Constants / literals ──
(true) @constant
(false) @constant
(null) @constant
(this) @variable

; ── Types ──
(primitive_type) @type.builtin
(sized_type_specifier) @type.builtin
(type_identifier) @type
(auto) @type.builtin
((namespace_identifier) @type
 (#match? @type "^[A-Z]"))

; ── Functions ──
(call_expression
  function: (identifier) @function)
(call_expression
  function: (field_expression
    field: (field_identifier) @function))
(call_expression
  function: (qualified_identifier
    name: (identifier) @function))
(function_declarator
  declarator: (identifier) @function)
(function_declarator
  declarator: (field_identifier) @function)
(function_declarator
  declarator: (qualified_identifier
    name: (identifier) @function))
(template_function
  name: (identifier) @function)

; ── Keywords ──
[
  "if" "else" "for" "while" "do" "switch" "case" "default"
  "break" "continue" "return" "goto"
  "struct" "union" "enum" "typedef" "class" "namespace" "template"
  "typename" "using" "public" "private" "protected" "virtual" "friend"
  "explicit" "operator" "new" "delete" "throw" "try" "catch"
  "sizeof" "static" "extern" "register" "inline" "const" "volatile"
  "constexpr" "consteval" "constinit" "noexcept" "final" "override"
  "mutable" "decltype" "static_assert" "concept" "requires"
  "co_await" "co_return" "co_yield"
] @keyword

; ── Operators / punctuation ──
[
  "+" "-" "*" "/" "%" "=" "==" "!=" "<" ">" "<=" ">=" "&&" "||" "!"
  "&" "|" "^" "~" "<<" ">>" "++" "--" "+=" "-=" "*=" "/=" "%="
  "&=" "|=" "^=" "<<=" ">>=" "->" "::" "?"
] @operator

[
  "(" ")" "{" "}" "[" "]" ";" "," "." ":"
] @delimiter

; ── Metal keyword/type sets (C++ grammar parses .metal — recolor by name) ──
((type_identifier) @keyword
 (#any-of? @keyword
   "kernel" "vertex" "fragment" "compute" "device" "constant"
   "threadgroup" "threadgroup_imageblock" "ray_data" "object_data"))
((identifier) @keyword
 (#any-of? @keyword
   "kernel" "vertex" "fragment" "compute" "device" "constant"
   "threadgroup" "threadgroup_imageblock" "ray_data" "object_data"))
((type_identifier) @type
 (#any-of? @type
   "half" "half2" "half3" "half4" "float2" "float3" "float4"
   "float2x2" "float3x3" "float4x4" "int2" "int3" "int4"
   "uint" "uint2" "uint3" "uint4" "texture2d" "texture3d"
   "texturecube" "sampler" "packed_float3" "packed_float4"))

; ── Metal / C++11 attributes: [[ ... ]] → annotation ──
(attribute_declaration) @annotation
"##;

/// The Verilog highlight query (hardware mode, §B9). Same architecture as the
/// C++ one: captures map onto the existing [`TokenPalette`].
pub const VERILOG_HIGHLIGHT_QUERY: &str = r##"
; ── Comments / strings / numbers ──
(comment) @comment
(string_literal) @string
(integral_number) @number
(real_number) @number
(unsigned_number) @number
(time_literal) @number

; ── Compiler directives ──
(include_compiler_directive) @preprocessor
(timescale_compiler_directive) @preprocessor
(default_nettype_compiler_directive) @preprocessor
(text_macro_usage) @preprocessor

; ── Declarations ──
(module_header (simple_identifier) @function)
(module_instantiation (simple_identifier) @type)
(name_of_instance (instance_identifier) @variable)
(port_identifier) @variable
; $display / $finish and friends.
(system_tf_identifier) @function

; ── Keywords ──
[
  "module" "endmodule" "macromodule"
  "input" "output" "inout"
  "reg" "wire" "logic" "integer" "genvar" "real"
  "parameter" "localparam" "defparam"
  "assign" "always" "always_ff" "always_comb" "always_latch" "initial"
  "posedge" "negedge"
  "begin" "end"
  "if" "else" "case" "casex" "casez" "endcase" "default"
  "for" "while" "repeat" "forever"
  "function" "endfunction" "task" "endtask" "return"
  "generate" "endgenerate"
  "signed" "unsigned"
] @keyword

; ── Operators / punctuation ──
[
  "+" "-" "*" "/" "%" "=" "==" "!=" "===" "!==" "<" ">" "<=" ">="
  "&&" "||" "!" "&" "|" "^" "~" "<<" ">>" "<<<" ">>>" "?" "@" "#"
] @operator

[
  "(" ")" "{" "}" "[" "]" ";" "," "." ":"
] @delimiter
"##;

/// Which tree-sitter grammar highlights a file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Grammar {
    /// C/C++/Objective-C, plus Metal and CUDA through the same grammar.
    Cpp,
    /// Plain Verilog (hardware mode). SystemVerilog files parse best-effort.
    Verilog,
}

/// The grammar for a path, or `None` for plain text.
pub fn grammar_for(path: &Path) -> Option<Grammar> {
    match ext_lower(path).as_deref() {
        Some(
            "cpp" | "cc" | "cxx" | "c++" | "c" | "h" | "hpp" | "hxx" | "hh" | "inl"
            | "metal" | "cu" | "cuh" | "mm" | "m",
        ) => Some(Grammar::Cpp),
        Some("v" | "sv" | "svh" | "vh") => Some(Grammar::Verilog),
        _ => None,
    }
}

/// The auto-indent dialect for a path (Enter indent, closer re-align). Covers
/// more extensions than [`grammar_for`]: brace languages without a grammar
/// (Rust, JS/TS, Java …) still get brace indentation.
pub fn indent_lang_for(path: &Path) -> jade_buffer::IndentLang {
    use jade_buffer::IndentLang;
    match grammar_for(path) {
        Some(Grammar::Cpp) => IndentLang::CFamily,
        Some(Grammar::Verilog) => IndentLang::Verilog,
        None => match ext_lower(path).as_deref() {
            Some("py") => IndentLang::Python,
            Some(
                "rs" | "js" | "jsx" | "ts" | "tsx" | "java" | "cs" | "go" | "json" | "swift"
                | "kt",
            ) => IndentLang::CFamily,
            _ => IndentLang::None,
        },
    }
}

/// Which extensions are highlighted. Everything else falls back to plain text.
pub fn is_highlightable(path: &Path) -> bool {
    grammar_for(path).is_some()
}

/// True for files the C++ analysis pipeline applies to (flow arrows, size
/// annotations, structure outline). Verilog files highlight but must not run
/// the C++ analyzers.
pub fn is_cpp_family(path: &Path) -> bool {
    grammar_for(path) == Some(Grammar::Cpp)
}

fn ext_lower(path: &Path) -> Option<String> {
    path.extension()
        .and_then(|e| e.to_str())
        .map(|e| e.to_ascii_lowercase())
}

/// A reusable highlighter: owns the compiled [`Query`] and a per-capture color
/// table. Cheap to construct once and reuse across files.
pub struct Highlighter {
    language: tree_sitter::Language,
    query: Query,
    /// `capture_colors[i]` is the palette color for capture index `i`, if the
    /// capture name maps to a token color (some captures like `@variable` on
    /// `this` still map; unmapped capture names would be `None`).
    capture_colors: Vec<Option<u32>>,
}

impl Highlighter {
    /// Build the C++ highlighter with the given palette. Returns an error only if
    /// the (static) query fails to compile against the grammar.
    pub fn new_cpp(palette: TokenPalette) -> Result<Self, tree_sitter::QueryError> {
        Self::new(tree_sitter_cpp::LANGUAGE.into(), HIGHLIGHT_QUERY, palette)
    }

    /// Build the Verilog highlighter (hardware mode).
    pub fn new_verilog(palette: TokenPalette) -> Result<Self, tree_sitter::QueryError> {
        Self::new(
            tree_sitter_verilog::LANGUAGE.into(),
            VERILOG_HIGHLIGHT_QUERY,
            palette,
        )
    }

    /// Build the highlighter that matches `path`, or `None` for plain text or
    /// a query-compile failure.
    pub fn for_path(path: &Path, palette: TokenPalette) -> Option<Self> {
        match grammar_for(path)? {
            Grammar::Cpp => Self::new_cpp(palette).ok(),
            Grammar::Verilog => Self::new_verilog(palette).ok(),
        }
    }

    fn new(
        language: tree_sitter::Language,
        query_src: &str,
        palette: TokenPalette,
    ) -> Result<Self, tree_sitter::QueryError> {
        let query = Query::new(&language, query_src)?;
        let capture_colors = query
            .capture_names()
            .iter()
            .map(|name| color_for_capture(name, &palette))
            .collect();
        Ok(Highlighter {
            language,
            query,
            capture_colors,
        })
    }

    /// Highlight `text`, returning one span vector per line (split on `\n`).
    /// Byte ranges in each span are relative to the start of that line and never
    /// cross a line boundary (multi-line tokens like block comments and raw
    /// strings are split at newlines).
    pub fn highlight(&self, text: &str) -> Vec<Vec<Span>> {
        let line_starts = line_starts(text);
        let line_count = line_starts.len();

        // Per-byte color, `u32::MAX` = "no highlight". Paint larger spans first
        // so nested/more-specific captures override their enclosing ranges.
        let mut bytes = vec![u32::MAX; text.len()];

        let mut parser = tree_sitter::Parser::new();
        if parser.set_language(&self.language).is_err() {
            return vec![Vec::new(); line_count];
        }
        let Some(tree) = parser.parse(text, None) else {
            return vec![Vec::new(); line_count];
        };

        // Collect (range, len, color); the text-predicate filter (`#match?`,
        // `#any-of?`) is applied automatically by `matches` via the provider.
        let src = text.as_bytes();
        let mut cursor = QueryCursor::new();
        let mut captured: Vec<(usize, usize, u32)> = Vec::new();
        let mut it = cursor.matches(&self.query, tree.root_node(), src);
        while let Some(m) = it.next() {
            for cap in m.captures {
                if let Some(color) = self.capture_colors[cap.index as usize] {
                    let node = cap.node;
                    let (s, e) = (node.start_byte(), node.end_byte());
                    if e > s && e <= bytes.len() {
                        captured.push((s, e, color));
                    }
                }
            }
        }
        // Largest first → smaller (more specific) captures win on overlap.
        captured.sort_by(|a, b| (b.1 - b.0).cmp(&(a.1 - a.0)));
        for (s, e, color) in captured {
            for b in &mut bytes[s..e] {
                *b = color;
            }
        }

        // Fold the per-byte colors into per-line runs.
        let mut out: Vec<Vec<Span>> = Vec::with_capacity(line_count);
        for i in 0..line_count {
            let line_start = line_starts[i];
            let line_end = if i + 1 < line_count {
                // Exclude the trailing '\n'.
                line_starts[i + 1] - 1
            } else {
                text.len()
            };
            out.push(coalesce(&bytes[line_start..line_end]));
        }
        out
    }
}

/// Highlight a file's text if its extension is a C-family/Metal source; otherwise
/// return empty spans (plain text). One-shot convenience wrapper used by the
/// viewer's open path and the smoke hook.
pub fn highlight_file(path: &Path, text: &str, palette: TokenPalette) -> Vec<Vec<Span>> {
    let line_count = line_starts(text).len();
    match Highlighter::for_path(path, palette) {
        Some(h) => h.highlight(text),
        None => vec![Vec::new(); line_count],
    }
}

/// Byte offsets of the start of each line. Always at least one line (offset 0),
/// matching how the viewer splits text into rows.
fn line_starts(text: &str) -> Vec<usize> {
    let mut starts = vec![0usize];
    for (i, b) in text.bytes().enumerate() {
        if b == b'\n' {
            starts.push(i + 1);
        }
    }
    // A trailing '\n' produces a final empty line start == text.len(); that's a
    // real (empty) line and we keep it, matching editor behavior.
    starts
}

/// Coalesce a slice of per-byte colors (`u32::MAX` = none) into line-relative
/// spans of equal color.
fn coalesce(line_bytes: &[u32]) -> Vec<Span> {
    let mut spans = Vec::new();
    let mut i = 0usize;
    while i < line_bytes.len() {
        let color = line_bytes[i];
        if color == u32::MAX {
            i += 1;
            continue;
        }
        let start = i;
        while i < line_bytes.len() && line_bytes[i] == color {
            i += 1;
        }
        spans.push(Span {
            start,
            end: i,
            color,
        });
    }
    spans
}

/// Map a query capture name to a palette color. Capture names use dotted
/// hierarchies (`type.builtin`); we match on the leading segment(s).
fn color_for_capture(name: &str, p: &TokenPalette) -> Option<u32> {
    let base = name.split('.').next().unwrap_or(name);
    Some(match base {
        "keyword" => p.keyword,
        "type" => p.types,
        "function" => p.function,
        "string" => p.string,
        "number" => p.number,
        "comment" => p.comment,
        "variable" => p.variable,
        "preprocessor" => p.preprocessor,
        "operator" => p.operator,
        "delimiter" => p.delimiter,
        "constant" => p.constant,
        "annotation" => p.annotation,
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn hl(text: &str) -> Vec<Vec<Span>> {
        Highlighter::new_cpp(TokenPalette::jade_dark())
            .expect("query compiles")
            .highlight(text)
    }

    /// The static query must compile against the grammar.
    #[test]
    fn query_compiles() {
        assert!(Highlighter::new_cpp(TokenPalette::jade_dark()).is_ok());
    }

    /// Find the color covering the byte range of `needle` on `line`.
    fn color_of<'a>(spans: &[Vec<Span>], text: &str, line: usize, needle: &str) -> Option<u32> {
        let line_text = text.lines().nth(line)?;
        let col = line_text.find(needle)?;
        spans[line]
            .iter()
            .find(|s| s.start <= col && col < s.end)
            .map(|s| s.color)
    }

    #[test]
    fn keyword_string_comment_type_number() {
        let text = "// hi\nint add(int x) {\n    const char* s = \"hi\";\n    return 42;\n}\n";
        let p = TokenPalette::jade_dark();
        let spans = hl(text);

        // Comment on line 0.
        assert_eq!(color_of(&spans, text, 0, "// hi"), Some(p.comment));
        // Primitive type `int` on line 1.
        assert_eq!(color_of(&spans, text, 1, "int"), Some(p.types));
        // Function name `add` on line 1.
        assert_eq!(color_of(&spans, text, 1, "add"), Some(p.function));
        // Keyword `const` on line 2.
        assert_eq!(color_of(&spans, text, 2, "const"), Some(p.keyword));
        // String literal on line 2.
        assert_eq!(color_of(&spans, text, 2, "\"hi\""), Some(p.string));
        // Keyword `return` and number `42` on line 3.
        assert_eq!(color_of(&spans, text, 3, "return"), Some(p.keyword));
        assert_eq!(color_of(&spans, text, 3, "42"), Some(p.number));
    }

    #[test]
    fn preprocessor_and_system_string() {
        let text = "#include <vector>\n#define N 4\n";
        let p = TokenPalette::jade_dark();
        let spans = hl(text);
        // `<vector>` is a system lib string.
        assert_eq!(color_of(&spans, text, 0, "<vector>"), Some(p.string));
        // The `#define` directive token colors as preprocessor.
        assert_eq!(color_of(&spans, text, 1, "#define"), Some(p.preprocessor));
    }

    #[test]
    fn metal_keywords_recolored() {
        // Parsed with the C++ grammar; `kernel`/`device` recolor via #any-of?.
        let text = "kernel void k(device float4* buf) {}\n";
        let p = TokenPalette::jade_dark();
        let h = Highlighter::new_cpp(p).unwrap();
        let spans = h.highlight(text);
        // At least one keyword-colored span exists for the Metal qualifiers.
        let has_keyword = spans[0].iter().any(|s| s.color == p.keyword);
        assert!(has_keyword, "expected a Metal keyword span");
    }

    /// The Verilog query must compile against its grammar.
    #[test]
    fn verilog_query_compiles() {
        assert!(Highlighter::new_verilog(TokenPalette::jade_dark()).is_ok());
    }

    #[test]
    fn verilog_keywords_and_literals() {
        let text = "// count\nmodule blink (input wire clk, output wire [4:0] led);\n    reg [25:0] count = 26'd0;\n    always @(posedge clk) count <= count + 1;\n    assign led = ~count[25:21];\nendmodule\n";
        let p = TokenPalette::jade_dark();
        let h = Highlighter::new_verilog(p).unwrap();
        let spans = h.highlight(text);
        assert_eq!(color_of(&spans, text, 0, "// count"), Some(p.comment));
        assert_eq!(color_of(&spans, text, 1, "module"), Some(p.keyword));
        assert_eq!(color_of(&spans, text, 1, "blink"), Some(p.function));
        assert_eq!(color_of(&spans, text, 1, "wire"), Some(p.keyword));
        assert_eq!(color_of(&spans, text, 2, "26'd0"), Some(p.number));
        assert_eq!(color_of(&spans, text, 3, "posedge"), Some(p.keyword));
        assert_eq!(color_of(&spans, text, 5, "endmodule"), Some(p.keyword));
    }

    #[test]
    fn grammar_dispatch_by_path() {
        assert_eq!(grammar_for(Path::new("a.cpp")), Some(Grammar::Cpp));
        assert_eq!(grammar_for(Path::new("a.metal")), Some(Grammar::Cpp));
        assert_eq!(grammar_for(Path::new("blink.v")), Some(Grammar::Verilog));
        assert_eq!(grammar_for(Path::new("top.sv")), Some(Grammar::Verilog));
        assert_eq!(grammar_for(Path::new("notes.txt")), None);
        assert!(is_cpp_family(Path::new("a.cpp")));
        assert!(!is_cpp_family(Path::new("blink.v")));
        assert!(is_highlightable(Path::new("blink.v")));
    }

    #[test]
    fn plain_text_fallback() {
        let text = "just some text\nno code here\n";
        let spans = highlight_file(&PathBuf::from("notes.txt"), text, TokenPalette::jade_dark());
        assert_eq!(spans.len(), 3);
        assert!(spans.iter().all(|l| l.is_empty()));
    }

    #[test]
    fn spans_stay_within_line() {
        // A block comment spanning multiple lines must split at newlines.
        let text = "/* line one\n   line two */\nint x;\n";
        let spans = hl(text);
        // Each span end must be <= its line length.
        for (i, line) in text.lines().enumerate() {
            for s in &spans[i] {
                assert!(s.end <= line.len(), "span {:?} exceeds line {} len {}", s, i, line.len());
                assert!(s.start < s.end);
            }
        }
    }
}
