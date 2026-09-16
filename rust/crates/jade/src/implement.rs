//! Implement-from-header stubs ("smart fill").
//!
//! When a source file includes a header, the completion popup offers one item
//! for each function the header declares but the source does not yet define.
//! Selecting the item replaces the partial line with a full definition stub:
//!
//! ```text
//! int Foo::bar(int x) const {
//!     |
//! }
//! ```
//!
//! The module is pure: it parses text with the same `tree-sitter-cpp` grammar as
//! [`crate::structure`] and returns owned data. `app.rs` resolves the include
//! paths, caches the parsed headers, and applies the edit.

use std::path::{Path, PathBuf};

use jade_lsp::{CompletionItem, CompletionItemKind};
use tree_sitter::Node;

/// The marker `app.rs` stores in `CompletionItem::data` to recognize a stub item.
pub const DATA_TAG: &str = "jade.implement";

/// One function the header declares. `scope` lists the enclosing namespaces and
/// classes from outer to inner. A class scope carries its template parameter
/// list when the class is a template.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HeaderFn {
    /// The bare name: `add`, `Foo`, `~Foo`, `operator==`.
    pub name: String,
    pub scope: Vec<Scope>,
    /// Text before the name: return type and pointer/reference marks, with the
    /// storage specifiers removed. Empty for constructors and destructors.
    pub head: String,
    /// The parameter list without the parentheses and without default values.
    pub params: String,
    /// Qualifiers after the parameter list: ` const`, ` noexcept`, `-> T`.
    pub tail: String,
    /// The template header of the function itself: `template <typename T>`.
    pub template: Option<String>,
    /// The number of parameters (used to match an existing definition).
    pub arity: usize,
}

/// One enclosing scope of a declaration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Scope {
    pub name: String,
    pub kind: ScopeKind,
    /// `template <typename T>` for a class template, else `None`.
    pub template: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScopeKind {
    Namespace,
    Class,
}

/// A definition that already exists in the source file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Defined {
    /// The declarator name as written: `add`, `Foo::bar`, `ns::Foo::bar`.
    pub qualified: String,
    pub arity: usize,
}

/// A ready-to-insert stub for one header function.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Stub {
    /// The bare function name (the popup filters on this).
    pub name: String,
    /// The signature line as it appears in the popup.
    pub label: String,
    /// The full text to insert. Lines after the first carry no indentation; the
    /// body line ends with [`CARET_MARK`], which marks the caret.
    pub text: String,
}

/// The placeholder in [`Stub::text`] that marks where the caret goes.
pub const CARET_MARK: &str = "\u{0}";

// ── Include scanning ──────────────────────────────────────────────────────────

/// Include directives in `source`. Returns `(path, is_quoted)` per directive.
pub fn includes(source: &str) -> Vec<(String, bool)> {
    let mut out = Vec::new();
    for line in source.lines() {
        let t = line.trim_start();
        let Some(rest) = t.strip_prefix('#') else {
            continue;
        };
        let rest = rest.trim_start();
        let Some(rest) = rest.strip_prefix("include") else {
            continue;
        };
        let rest = rest.trim_start();
        if let Some(r) = rest.strip_prefix('"') {
            if let Some(end) = r.find('"') {
                out.push((r[..end].to_string(), true));
            }
        } else if let Some(r) = rest.strip_prefix('<') {
            if let Some(end) = r.find('>') {
                out.push((r[..end].to_string(), false));
            }
        }
    }
    out
}

/// Resolve an include path to a file on disk. Quoted includes look next to the
/// source first. Both forms then try the workspace root and its `include` and
/// `src` folders. Returns `None` when no candidate exists.
pub fn resolve_include(source: &Path, root: &Path, include: &str, quoted: bool) -> Option<PathBuf> {
    let mut candidates: Vec<PathBuf> = Vec::new();
    if quoted {
        if let Some(dir) = source.parent() {
            candidates.push(dir.join(include));
        }
    }
    candidates.push(root.join(include));
    candidates.push(root.join("include").join(include));
    candidates.push(root.join("src").join(include));
    if !quoted {
        if let Some(dir) = source.parent() {
            candidates.push(dir.join(include));
        }
    }
    candidates.into_iter().find(|p| p.is_file())
}

// ── Header parsing ────────────────────────────────────────────────────────────

fn parse(source: &str) -> Option<tree_sitter::Tree> {
    let language = tree_sitter_cpp::LANGUAGE;
    let mut parser = tree_sitter::Parser::new();
    parser.set_language(&language.into()).ok()?;
    parser.parse(source, None)
}

/// All function declarations (no body) in a header.
pub fn declarations(header: &str) -> Vec<HeaderFn> {
    let Some(tree) = parse(header) else {
        return Vec::new();
    };
    let src = header.as_bytes();
    let mut out = Vec::new();
    collect_declarations(tree.root_node(), src, &mut out);
    out
}

fn collect_declarations(node: Node, src: &[u8], out: &mut Vec<HeaderFn>) {
    match node.kind() {
        "declaration" | "field_declaration" => {
            if let Some(f) = header_fn(node, src) {
                out.push(f);
            }
            return;
        }
        // Bodies never hold a declaration we want to implement elsewhere, and
        // a friend declaration belongs to another class.
        "function_definition" | "compound_statement" | "friend_declaration" => return,
        _ => {}
    }
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        collect_declarations(child, src, out);
    }
}

/// Follow pointer/reference declarators down to the `function_declarator`.
fn function_declarator(node: Node) -> Option<Node> {
    let mut d = node.child_by_field_name("declarator")?;
    loop {
        match d.kind() {
            "function_declarator" => return Some(d),
            "pointer_declarator" | "reference_declarator" => {
                d = d.child_by_field_name("declarator").or_else(|| {
                    let mut c = d.walk();
                    d.named_children(&mut c).last()
                })?;
            }
            _ => return None,
        }
    }
}

fn text<'a>(node: Node, src: &'a [u8]) -> &'a str {
    node.utf8_text(src).unwrap_or("")
}

/// Collapse whitespace runs to one space and trim.
fn squash(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

const DROPPED_SPECIFIERS: &[&str] = &["static", "virtual", "explicit", "extern", "friend"];

fn header_fn(decl: Node, src: &[u8]) -> Option<HeaderFn> {
    let fdecl = function_declarator(decl)?;
    let name_node = fdecl.child_by_field_name("declarator")?;
    let name = match name_node.kind() {
        "identifier" | "field_identifier" | "destructor_name" | "operator_name" => {
            squash(text(name_node, src))
        }
        _ => return None,
    };
    let params_node = fdecl.child_by_field_name("parameters")?;

    // Skip what has no out-of-line definition: friends, pure virtuals, and
    // defaulted or deleted members.
    let decl_text = text(decl, src);
    let squashed = squash(decl_text);
    if squashed.starts_with("friend ")
        || squashed.starts_with("typedef ")
        || squashed.ends_with("= 0;")
        || squashed.ends_with("= default;")
        || squashed.ends_with("= delete;")
    {
        return None;
    }
    // A declaration with several declarators (`int a(), b();`) is rare; take
    // only the shape where the declarator is a direct child.
    if decl.child_by_field_name("declarator").is_none() {
        return None;
    }

    // Head: from the declaration start to the name, minus dropped specifiers.
    let head_raw = &decl_text[..name_node.start_byte() - decl.start_byte()];
    let mut head_words: Vec<&str> = head_raw.split_whitespace().collect();
    while let Some(first) = head_words.first() {
        if DROPPED_SPECIFIERS.contains(first) {
            head_words.remove(0);
        } else {
            break;
        }
    }
    let mut head = head_words.join(" ");
    // Keep `int*` / `int&` tight to the type.
    head = head.replace(" *", "*").replace(" &", "&");
    if !head.is_empty() {
        head.push(' ');
    }

    // Parameters without default values.
    let mut params = Vec::new();
    let mut arity = 0;
    let mut cursor = params_node.walk();
    for p in params_node.named_children(&mut cursor) {
        match p.kind() {
            "parameter_declaration" | "variadic_parameter_declaration" => {
                params.push(squash(text(p, src)));
                arity += 1;
            }
            "optional_parameter_declaration" => {
                let mut c = p.walk();
                let eq = p.children(&mut c).find(|n| n.kind() == "=");
                let end = eq.map(|n| n.start_byte()).unwrap_or(p.end_byte());
                params.push(squash(&text(p, src)[..end - p.start_byte()]));
                arity += 1;
            }
            "comment" => {}
            _ => {
                params.push(squash(text(p, src)));
                arity += 1;
            }
        }
    }
    let params = params.join(", ");

    // Tail: qualifiers after the parameter list, minus `override` / `final`.
    let tail_raw = &text(fdecl, src)[params_node.end_byte() - fdecl.start_byte()..];
    let tail_words: Vec<&str> = tail_raw
        .split_whitespace()
        .filter(|w| *w != "override" && *w != "final")
        .collect();
    let tail = if tail_words.is_empty() {
        String::new()
    } else {
        format!(" {}", tail_words.join(" "))
    };

    // Function template header.
    let template = decl
        .parent()
        .filter(|p| p.kind() == "template_declaration")
        .and_then(|p| p.child_by_field_name("parameters"))
        .map(|p| format!("template {}", squash(text(p, src))));

    let scope = scopes_of(decl, src);
    Some(HeaderFn {
        name,
        scope,
        head,
        params,
        tail,
        template,
        arity,
    })
}

/// The enclosing namespaces and classes of `node`, outer to inner.
fn scopes_of(node: Node, src: &[u8]) -> Vec<Scope> {
    let mut scopes = Vec::new();
    let mut cur = node.parent();
    while let Some(n) = cur {
        match n.kind() {
            "namespace_definition" => {
                if let Some(name) = n.child_by_field_name("name") {
                    scopes.push(Scope {
                        name: squash(text(name, src)),
                        kind: ScopeKind::Namespace,
                        template: None,
                    });
                }
            }
            "class_specifier" | "struct_specifier" | "union_specifier" => {
                if let Some(name) = n.child_by_field_name("name") {
                    let template = n
                        .parent()
                        .filter(|p| p.kind() == "template_declaration")
                        .and_then(|p| p.child_by_field_name("parameters"))
                        .map(|p| squash(text(p, src)));
                    scopes.push(Scope {
                        name: squash(text(name, src)),
                        kind: ScopeKind::Class,
                        template,
                    });
                }
            }
            _ => {}
        }
        cur = n.parent();
    }
    scopes.reverse();
    scopes
}

// ── Source parsing ────────────────────────────────────────────────────────────

/// The functions the source already defines (with a body).
pub fn definitions(source: &str) -> Vec<Defined> {
    let Some(tree) = parse(source) else {
        return Vec::new();
    };
    let src = source.as_bytes();
    let mut out = Vec::new();
    collect_definitions(tree.root_node(), src, &mut out);
    out
}

fn collect_definitions(node: Node, src: &[u8], out: &mut Vec<Defined>) {
    if node.kind() == "function_definition" {
        if let Some(fdecl) = function_declarator(node) {
            if let (Some(name), Some(params)) = (
                fdecl.child_by_field_name("declarator"),
                fdecl.child_by_field_name("parameters"),
            ) {
                let mut c = params.walk();
                let arity = params
                    .named_children(&mut c)
                    .filter(|p| p.kind() != "comment")
                    .count();
                out.push(Defined {
                    qualified: squash(text(name, src)).replace(" ", ""),
                    arity,
                });
            }
        }
    }
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        collect_definitions(child, src, out);
    }
}

/// True when the caret sits at file or namespace scope, where a function
/// definition can start. False inside a function body, a class body, or a
/// parameter list.
pub fn at_definition_scope(source: &str, offset: usize) -> bool {
    let Some(tree) = parse(source) else {
        return false;
    };
    let offset = offset.min(source.len());
    let Some(mut node) = tree
        .root_node()
        .descendant_for_byte_range(offset, offset)
    else {
        return true;
    };
    loop {
        match node.kind() {
            "compound_statement"
            | "field_declaration_list"
            | "parameter_list"
            | "enumerator_list"
            | "initializer_list"
            | "function_definition" => return false,
            _ => {}
        }
        match node.parent() {
            Some(p) => node = p,
            None => return true,
        }
    }
}

/// The namespaces the source already opens or imports, so a stub does not
/// qualify with them again.
fn open_namespaces(source: &str) -> Vec<String> {
    let mut out = Vec::new();
    let words: Vec<&str> = source.split(|c: char| !(c.is_alphanumeric() || c == '_' || c == ':')).filter(|w| !w.is_empty()).collect();
    let mut i = 0;
    while i < words.len() {
        if words[i] == "namespace" {
            if let Some(w) = words.get(i + 1) {
                for part in w.split("::") {
                    if !part.is_empty() && part != "std" {
                        out.push(part.to_string());
                    }
                }
            }
        }
        i += 1;
    }
    out
}

// ── Stub building ─────────────────────────────────────────────────────────────

/// The qualified name of a header function: `ns::Foo::bar`.
fn qualified_name(f: &HeaderFn) -> String {
    let mut q: Vec<&str> = f.scope.iter().map(|s| s.name.as_str()).collect();
    q.push(&f.name);
    q.join("::")
}

fn is_defined(f: &HeaderFn, defined: &[Defined]) -> bool {
    let full = qualified_name(f);
    let parts: Vec<&str> = full.split("::").collect();
    defined.iter().any(|d| {
        if d.arity != f.arity {
            return false;
        }
        let dp: Vec<&str> = d.qualified.split("::").collect();
        dp.len() <= parts.len() && parts[parts.len() - dp.len()..] == dp[..]
    })
}

/// Build the stub for one header function, as it should read in `source`.
pub fn stub(f: &HeaderFn, source: &str) -> Stub {
    let open = open_namespaces(source);
    let mut templates: Vec<String> = Vec::new();
    let mut qual: Vec<String> = Vec::new();
    let mut skipping_ns = true;
    for s in &f.scope {
        match s.kind {
            ScopeKind::Namespace => {
                if skipping_ns && open.iter().any(|o| o == &s.name) {
                    continue;
                }
                skipping_ns = false;
                qual.push(s.name.clone());
            }
            ScopeKind::Class => {
                skipping_ns = false;
                match &s.template {
                    Some(t) => {
                        templates.push(format!("template {}", t));
                        qual.push(format!("{}<{}>", s.name, template_args(t)));
                    }
                    None => qual.push(s.name.clone()),
                }
            }
        }
    }
    if let Some(t) = &f.template {
        templates.push(t.clone());
    }
    let mut name = qual.join("::");
    if !name.is_empty() {
        name.push_str("::");
    }
    name.push_str(&f.name);
    let signature = format!("{}{}({}){}", f.head, name, f.params, f.tail);
    let mut text = String::new();
    for t in &templates {
        text.push_str(t);
        text.push('\n');
    }
    text.push_str(&signature);
    text.push_str(" {\n    ");
    text.push_str(CARET_MARK);
    text.push_str("\n}");
    Stub {
        name: f.name.clone(),
        label: signature,
        text,
    }
}

/// `<typename T, int N>` → `T, N`.
fn template_args(params: &str) -> String {
    let inner = params.trim_start_matches('<').trim_end_matches('>');
    inner
        .split(',')
        .filter_map(|p| {
            let p = p.split('=').next().unwrap_or("").trim();
            let p = p.trim_end_matches("...");
            p.split_whitespace().last().map(|s| s.to_string())
        })
        .collect::<Vec<_>>()
        .join(", ")
}

/// The stubs `source` still needs from `header_fns`, in header order.
pub fn missing_stubs(header_fns: &[HeaderFn], source: &str) -> Vec<Stub> {
    let defined = definitions(source);
    header_fns
        .iter()
        .filter(|f| !is_defined(f, &defined))
        .map(|f| stub(f, source))
        .collect()
}

/// Map stubs to completion items. `origin` is the header file name shown as the
/// item detail. The popup filters on the bare function name.
pub fn completion_items(stubs: &[Stub], origin: &str) -> Vec<CompletionItem> {
    stubs
        .iter()
        .map(|s| CompletionItem {
            label: s.label.clone(),
            kind: Some(CompletionItemKind::FUNCTION),
            detail: Some(format!("implement · {}", origin)),
            insert_text: Some(s.text.clone()),
            filter_text: Some(s.name.clone()),
            sort_text: Some(format!("0{}", s.name)),
            data: Some(serde_json::Value::String(DATA_TAG.to_string())),
            ..Default::default()
        })
        .collect()
}

/// True when `item` is a stub item this module produced.
pub fn is_stub_item(item: &CompletionItem) -> bool {
    matches!(&item.data, Some(serde_json::Value::String(s)) if s == DATA_TAG)
}

/// Where an accepted stub replaces text on its line. When everything before the
/// typed identifier looks like a return type (`int`, `const Foo*`, `std::`), the
/// replacement starts at the line's first non-blank character. Otherwise only
/// the identifier is replaced. Columns are char columns.
pub fn replace_start_col(line: &str, ident_start_col: usize) -> usize {
    let before: Vec<char> = line.chars().take(ident_start_col).collect();
    let first = before.iter().position(|c| !c.is_whitespace());
    let Some(first) = first else {
        return ident_start_col;
    };
    let typeish = before[first..]
        .iter()
        .all(|c| c.is_alphanumeric() || c.is_whitespace() || matches!(c, '_' | '*' | '&' | ':' | '<' | '>' | ','));
    if typeish {
        first
    } else {
        ident_start_col
    }
}

/// Indent every line after the first with `indent` and turn the caret marker
/// into the caret offset. Returns `(text, caret_offset_within_text)`.
pub fn indent_stub(text: &str, indent: &str) -> (String, usize) {
    let mut out = String::with_capacity(text.len() + indent.len() * 4);
    for (i, line) in text.split('\n').enumerate() {
        if i > 0 {
            out.push('\n');
            if !line.is_empty() {
                out.push_str(indent);
            }
        }
        out.push_str(line);
    }
    let caret = out.find(CARET_MARK).unwrap_or(out.len());
    let out = out.replacen(CARET_MARK, "", 1);
    (out, caret)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn stubs_for(header: &str, source: &str) -> Vec<String> {
        missing_stubs(&declarations(header), source)
            .into_iter()
            .map(|s| s.label)
            .collect()
    }

    #[test]
    fn includes_finds_quoted_and_angled() {
        let src = "#include \"math.h\"\n#include <vector>\n  #  include   \"a/b.hpp\"\n";
        assert_eq!(
            includes(src),
            vec![
                ("math.h".to_string(), true),
                ("vector".to_string(), false),
                ("a/b.hpp".to_string(), true)
            ]
        );
    }

    #[test]
    fn free_function_declaration() {
        let h = "int add(int a, int b);\nvoid hello();\n";
        let fns = declarations(h);
        assert_eq!(fns.len(), 2);
        assert_eq!(fns[0].name, "add");
        assert_eq!(fns[0].head, "int ");
        assert_eq!(fns[0].params, "int a, int b");
        assert_eq!(fns[0].arity, 2);
        assert_eq!(stubs_for(h, ""), vec!["int add(int a, int b)", "void hello()"]);
    }

    #[test]
    fn stub_text_has_body_and_caret() {
        let h = "int add(int a, int b);";
        let s = &missing_stubs(&declarations(h), "")[0];
        assert_eq!(s.text, "int add(int a, int b) {\n    \u{0}\n}");
        let (t, caret) = indent_stub(&s.text, "  ");
        assert_eq!(t, "int add(int a, int b) {\n      \n  }");
        assert_eq!(&t[..caret], "int add(int a, int b) {\n      ");
    }

    #[test]
    fn class_methods_are_qualified_and_specifiers_dropped() {
        let h = r#"
class Foo {
public:
    Foo(int x = 3);
    virtual ~Foo();
    static int count();
    int get() const;
    virtual void run() override;
    virtual void pure() = 0;
    Foo(const Foo&) = default;
    const char* name(std::string s, double d = 1.0) noexcept;
    friend void peek(Foo&);
    int value;
};
"#;
        assert_eq!(
            stubs_for(h, ""),
            vec![
                "Foo::Foo(int x)",
                "Foo::~Foo()",
                "int Foo::count()",
                "int Foo::get() const",
                "void Foo::run()",
                "const char* Foo::name(std::string s, double d) noexcept",
            ]
        );
    }

    #[test]
    fn namespaces_qualify_unless_open_in_source() {
        let h = "namespace geo {\nnamespace inner {\ndouble area(double r);\n}\nstruct P { int x() const; };\n}\n";
        assert_eq!(
            stubs_for(h, ""),
            vec!["double geo::inner::area(double r)", "int geo::P::x() const"]
        );
        let src = "#include \"g.h\"\nnamespace geo {\n}\n";
        assert_eq!(
            stubs_for(h, src),
            vec!["double inner::area(double r)", "int P::x() const"]
        );
        let src2 = "using namespace geo;\n";
        assert_eq!(stubs_for(h, src2)[0], "double inner::area(double r)");
    }

    #[test]
    fn already_defined_functions_are_skipped() {
        let h = "int add(int a, int b);\nint add(int a);\nclass Foo { void go(); };\n";
        let src = "int add(int a, int b) { return a + b; }\nvoid Foo::go() {}\n";
        assert_eq!(stubs_for(h, src), vec!["int add(int a)"]);
    }

    #[test]
    fn templates_carry_their_headers() {
        let h = "template <typename T>\nclass Box {\npublic:\n    T get() const;\n};\ntemplate <class U> U twice(U v);\n";
        let s = missing_stubs(&declarations(h), "");
        assert_eq!(s[0].label, "T Box<T>::get() const");
        assert!(s[0].text.starts_with("template <typename T>\nT Box<T>::get() const {"));
        assert_eq!(s[1].label, "U twice(U v)");
        assert!(s[1].text.starts_with("template <class U>\nU twice(U v) {"));
    }

    #[test]
    fn inline_definitions_in_header_are_not_offered() {
        let h = "inline int sq(int x) { return x * x; }\nint cube(int x);\n";
        assert_eq!(stubs_for(h, ""), vec!["int cube(int x)"]);
    }

    #[test]
    fn c_header_with_extern_c() {
        let h = "#ifdef __cplusplus\nextern \"C\" {\n#endif\nint parse(const char* s, size_t n);\n#ifdef __cplusplus\n}\n#endif\n";
        assert_eq!(stubs_for(h, ""), vec!["int parse(const char* s, size_t n)"]);
    }

    #[test]
    fn definition_scope_detection() {
        let src = "#include \"a.h\"\n\nint main() {\n    ad\n}\n\nad\nclass X {\n  ad\n};\nnamespace n {\nad\n}\n";
        let at = |needle: &str, nth: usize| {
            src.match_indices(needle).nth(nth).unwrap().0 + needle.len()
        };
        assert!(!at_definition_scope(src, at("    ad", 0)), "inside main");
        assert!(at_definition_scope(src, at("\nad", 0)), "file scope");
        assert!(!at_definition_scope(src, at("  ad", 1)), "class body");
        assert!(at_definition_scope(src, at("\nad", 1)), "namespace scope");
    }

    #[test]
    fn replace_start_col_covers_a_typed_return_type() {
        assert_eq!(replace_start_col("int ad", 4), 0);
        assert_eq!(replace_start_col("  const Foo* ad", 13), 2);
        assert_eq!(replace_start_col("ad", 0), 0);
        assert_eq!(replace_start_col("x = ad", 4), 4);
    }

    #[test]
    fn completion_items_are_tagged() {
        let s = missing_stubs(&declarations("int f();"), "");
        let items = completion_items(&s, "m.h");
        assert!(is_stub_item(&items[0]));
        assert_eq!(items[0].filter_text.as_deref(), Some("f"));
        assert_eq!(items[0].detail.as_deref(), Some("implement · m.h"));
    }

    #[test]
    fn resolve_include_prefers_source_dir() {
        let dir = std::env::temp_dir().join(format!("jade-impl-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("src")).unwrap();
        std::fs::create_dir_all(dir.join("include")).unwrap();
        std::fs::write(dir.join("src/a.h"), "").unwrap();
        std::fs::write(dir.join("include/b.h"), "").unwrap();
        let src = dir.join("src/main.cpp");
        assert_eq!(resolve_include(&src, &dir, "a.h", true), Some(dir.join("src/a.h")));
        assert_eq!(resolve_include(&src, &dir, "b.h", true), Some(dir.join("include/b.h")));
        assert_eq!(resolve_include(&src, &dir, "b.h", false), Some(dir.join("include/b.h")));
        assert_eq!(resolve_include(&src, &dir, "vector", false), None);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
