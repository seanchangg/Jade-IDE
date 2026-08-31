//! A small Markdown parser for streamed model prose.
//!
//! Models write Markdown whether or not the prompt asks them to — inline code
//! spans, bold, and numbered lists all turn up in a two-paragraph explanation.
//! Rendering the source text literally shows the reader `**Loop Over
//! Vertices**` and backticked identifiers, which is worse than plain prose.
//!
//! This is not a CommonMark implementation and does not try to be. It covers
//! what actually appears in a short explanation, and it is built for the awkward
//! part: the text is parsed **while it is still arriving**, so every marker is
//! temporarily unclosed. An unterminated `` ` `` or `**` is treated as though it
//! will close, which keeps a word from flickering between literal-marker and
//! styled as the next token lands.

/// Inline emphasis carried by a run of text.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Style {
    /// A `` `code` `` span: monospace, tinted.
    pub code: bool,
    pub bold: bool,
    pub italic: bool,
    /// A `~~strike~~` span: drawn with a line through it.
    pub strike: bool,
}

/// A run of text sharing one style.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Span {
    pub text: String,
    pub style: Style,
}

impl Span {
    fn new(text: impl Into<String>, style: Style) -> Self {
        Span {
            text: text.into(),
            style,
        }
    }
}

/// A block-level element.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Block {
    Paragraph(Vec<Span>),
    /// A list item, with its marker already stripped. `ordered` picks the glyph.
    Item {
        spans: Vec<Span>,
        ordered: bool,
        number: usize,
    },
    /// A fenced code block. Held as raw text: no inline parsing happens inside.
    Code(String),
    /// A heading, rendered as emphasized text rather than at a larger size —
    /// a card this small has no room for a type hierarchy.
    Heading(Vec<Span>),
}

/// Split `src` into blocks, parsing inline markup in each.
pub fn parse(src: &str) -> Vec<Block> {
    let mut out = Vec::new();
    let mut para: Vec<String> = Vec::new();
    let mut fence: Option<Vec<String>> = None;

    let flush = |para: &mut Vec<String>, out: &mut Vec<Block>| {
        if !para.is_empty() {
            let joined = para.join(" ");
            if !joined.trim().is_empty() {
                out.push(Block::Paragraph(parse_inline(joined.trim())));
            }
            para.clear();
        }
    };

    for raw in src.lines() {
        // Inside a fence, every line is literal until the closing marker.
        if let Some(buf) = &mut fence {
            if raw.trim_start().starts_with("```") {
                out.push(Block::Code(buf.join("\n")));
                fence = None;
            } else {
                buf.push(raw.to_string());
            }
            continue;
        }

        let line = raw.trim();

        if line.starts_with("```") {
            flush(&mut para, &mut out);
            fence = Some(Vec::new());
            continue;
        }
        if line.is_empty() {
            flush(&mut para, &mut out);
            continue;
        }
        if let Some(rest) = heading_body(line) {
            flush(&mut para, &mut out);
            out.push(Block::Heading(parse_inline(rest)));
            continue;
        }
        if let Some((rest, ordered, number)) = list_body(line) {
            flush(&mut para, &mut out);
            out.push(Block::Item {
                spans: parse_inline(rest),
                ordered,
                number,
            });
            continue;
        }
        para.push(line.to_string());
    }

    // A fence still open at the end is text arriving mid-stream, not an error.
    if let Some(buf) = fence {
        out.push(Block::Code(buf.join("\n")));
    }
    flush(&mut para, &mut out);
    out
}

/// `### Title` → `Title`.
fn heading_body(line: &str) -> Option<&str> {
    let hashes = line.len() - line.trim_start_matches('#').len();
    (1..=6).contains(&hashes).then(|| line[hashes..].trim_start())
}

/// `- item`, `* item`, `1. item` → the body, plus whether it was numbered.
fn list_body(line: &str) -> Option<(&str, bool, usize)> {
    for m in ["- ", "* ", "+ "] {
        if let Some(rest) = line.strip_prefix(m) {
            return Some((rest, false, 0));
        }
    }
    // `12. body` — the digits, then a dot, then a space.
    let digits = line.len() - line.trim_start_matches(|c: char| c.is_ascii_digit()).len();
    if digits > 0 {
        let rest = &line[digits..];
        if let Some(body) = rest.strip_prefix(". ").or_else(|| rest.strip_prefix(") ")) {
            let n = line[..digits].parse().unwrap_or(0);
            return Some((body, true, n));
        }
    }
    None
}

/// Parse inline markup into styled runs.
///
/// Handles `` `code` ``, `**bold**`, `__bold__`, `*italic*`, `_italic_`, and
/// leaves everything else alone. Code wins over emphasis: `` `a*b*c` `` is a
/// literal code span, which is what a reader of source expects.
pub fn parse_inline(src: &str) -> Vec<Span> {
    let mut out: Vec<Span> = Vec::new();
    let mut buf = String::new();
    let mut style = Style::default();
    let b = src.as_bytes();
    let mut i = 0;

    let push = |buf: &mut String, style: Style, out: &mut Vec<Span>| {
        if !buf.is_empty() {
            // Merge with the previous run when the style matches, so a
            // sentence does not fragment into dozens of elements.
            match out.last_mut() {
                Some(prev) if prev.style == style => prev.text.push_str(buf),
                _ => out.push(Span::new(buf.clone(), style)),
            }
            buf.clear();
        }
    };

    while i < b.len() {
        // A code span runs to the next backtick, or to the end of what has
        // arrived so far. Nothing inside it is markup.
        if b[i] == b'`' {
            push(&mut buf, style, &mut out);
            let start = i + 1;
            let end = src[start..].find('`').map(|j| start + j);
            let (text, next) = match end {
                Some(e) => (&src[start..e], e + 1),
                None => (&src[start..], b.len()),
            };
            if !text.is_empty() {
                let mut s = style;
                s.code = true;
                out.push(Span::new(text, s));
            }
            i = next;
            continue;
        }

        // `**` / `__` toggle bold; `~~` toggles strike; a single `*` / `_`
        // toggles italic.
        let two = i + 1 < b.len() && b[i + 1] == b[i];
        if (b[i] == b'*' || b[i] == b'_') && two {
            push(&mut buf, style, &mut out);
            style.bold = !style.bold;
            i += 2;
            continue;
        }
        if b[i] == b'~' && two {
            push(&mut buf, style, &mut out);
            style.strike = !style.strike;
            i += 2;
            continue;
        }
        if b[i] == b'*' || (b[i] == b'_' && is_word_boundary(src, i)) {
            push(&mut buf, style, &mut out);
            style.italic = !style.italic;
            i += 1;
            continue;
        }

        // Not a marker: copy one whole character, so multi-byte text survives.
        let ch_len = char_len(b[i]);
        buf.push_str(&src[i..(i + ch_len).min(src.len())]);
        i += ch_len;
    }
    push(&mut buf, style, &mut out);
    out
}

/// UTF-8 length from a lead byte.
fn char_len(lead: u8) -> usize {
    match lead {
        0x00..=0x7F => 1,
        0xC0..=0xDF => 2,
        0xE0..=0xEF => 3,
        _ => 4,
    }
}

/// Whether an `_` at `i` sits at a word boundary.
///
/// Without this, `snake_case_name` would be read as emphasis and lose its
/// underscores — which in a card explaining source code is a correctness bug,
/// not a styling one.
fn is_word_boundary(src: &str, i: usize) -> bool {
    let before = src[..i].chars().next_back();
    let after = src[i + 1..].chars().next();
    let word = |c: Option<char>| c.is_some_and(|c| c.is_alphanumeric() || c == '_');
    !(word(before) && word(after))
}

/// The plain text of a span list, with all markup removed.
pub fn plain(spans: &[Span]) -> String {
    spans.iter().map(|s| s.text.as_str()).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn code() -> Style {
        Style {
            code: true,
            ..Default::default()
        }
    }
    fn bold() -> Style {
        Style {
            bold: true,
            ..Default::default()
        }
    }
    fn italic() -> Style {
        Style {
            italic: true,
            ..Default::default()
        }
    }

    // ---- inline ---------------------------------------------------------

    #[test]
    fn plain_text_is_one_span() {
        let s = parse_inline("just words here");
        assert_eq!(s, vec![Span::new("just words here", Style::default())]);
    }

    #[test]
    fn a_code_span_is_extracted() {
        let s = parse_inline("the `verts` container");
        assert_eq!(
            s,
            vec![
                Span::new("the ", Style::default()),
                Span::new("verts", code()),
                Span::new(" container", Style::default()),
            ]
        );
    }

    #[test]
    fn bold_and_italic() {
        assert_eq!(
            parse_inline("**Loop**"),
            vec![Span::new("Loop", bold())]
        );
        assert_eq!(parse_inline("__Loop__"), vec![Span::new("Loop", bold())]);
        assert_eq!(parse_inline("*soft*"), vec![Span::new("soft", italic())]);
    }

    #[test]
    fn strike_toggles_and_a_single_tilde_is_literal() {
        let s = parse_inline("a ~~gone~~ b");
        assert_eq!(s.len(), 3);
        assert!(s[1].style.strike);
        assert_eq!(s[1].text, "gone");
        assert_eq!(plain(&parse_inline("~1 and ~2")), "~1 and ~2");
    }

    /// Markup inside a code span is literal — a reader of source expects that.
    #[test]
    fn code_wins_over_emphasis() {
        let s = parse_inline("`a*b*c`");
        assert_eq!(s, vec![Span::new("a*b*c", code())]);
    }

    /// The bug that would matter most in a card about code.
    #[test]
    fn snake_case_identifiers_keep_their_underscores() {
        let s = parse_inline("call read_frame_data now");
        assert_eq!(plain(&s), "call read_frame_data now");
        assert!(s.iter().all(|x| !x.style.italic));
    }

    #[test]
    fn multibyte_text_survives() {
        let s = parse_inline("naïve `é` — ok");
        assert_eq!(plain(&s), "naïve é — ok");
    }

    // ---- streaming: markers that have not closed yet ---------------------

    /// Mid-stream a code span is open. Styling the tail keeps a word from
    /// flickering between a literal backtick and styled text as tokens land.
    #[test]
    fn an_unterminated_code_span_styles_the_rest() {
        let s = parse_inline("the `ver");
        assert_eq!(
            s,
            vec![
                Span::new("the ", Style::default()),
                Span::new("ver", code()),
            ]
        );
        // And no stray backtick is ever shown.
        assert!(!plain(&s).contains('`'));
    }

    #[test]
    fn an_unterminated_bold_run_styles_the_rest() {
        let s = parse_inline("a **Loop Ove");
        assert_eq!(
            s,
            vec![
                Span::new("a ", Style::default()),
                Span::new("Loop Ove", bold()),
            ]
        );
        assert!(!plain(&s).contains('*'));
    }

    /// A lone backtick at the very end of what has arrived must not panic or
    /// emit an empty span.
    #[test]
    fn a_trailing_marker_alone_is_harmless() {
        assert_eq!(parse_inline("word `"), vec![Span::new("word ", Style::default())]);
        assert_eq!(parse_inline("`"), vec![]);
        assert_eq!(parse_inline(""), vec![]);
        assert_eq!(parse_inline("**"), vec![]);
    }

    /// Growing the text one character at a time must never panic and must
    /// never show a marker.
    #[test]
    fn parsing_every_prefix_is_safe() {
        let full = "The **loop** walks `verts` and scales `v.pos` by `scale`.";
        for i in 0..=full.len() {
            if !full.is_char_boundary(i) {
                continue;
            }
            let spans = parse_inline(&full[..i]);
            let text = plain(&spans);
            assert!(!text.contains('`'), "backtick shown at prefix {i}: {text:?}");
            assert!(!text.contains("**"), "asterisks shown at prefix {i}: {text:?}");
        }
    }

    // ---- blocks ---------------------------------------------------------

    #[test]
    fn paragraphs_split_on_blank_lines() {
        let b = parse("one line\nsame para\n\nsecond para");
        assert_eq!(b.len(), 2);
        match &b[0] {
            Block::Paragraph(s) => assert_eq!(plain(s), "one line same para"),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn numbered_and_bulleted_items() {
        let b = parse("1. **First**: does a thing\n- second\n* third");
        assert_eq!(b.len(), 3);
        match &b[0] {
            Block::Item {
                spans,
                ordered,
                number,
            } => {
                assert!(ordered);
                assert_eq!(*number, 1);
                assert_eq!(plain(spans), "First: does a thing");
            }
            other => panic!("{other:?}"),
        }
        assert!(matches!(&b[1], Block::Item { ordered: false, .. }));
        assert!(matches!(&b[2], Block::Item { ordered: false, .. }));
    }

    /// A decimal in prose is not a list item.
    #[test]
    fn a_bare_number_is_not_a_list() {
        let b = parse("3.14 is pi");
        assert!(matches!(&b[0], Block::Paragraph(_)));
    }

    #[test]
    fn headings_become_emphasized_text() {
        let b = parse("## What it does\nbody");
        assert!(matches!(&b[0], Block::Heading(_)));
        match &b[0] {
            Block::Heading(s) => assert_eq!(plain(s), "What it does"),
            _ => unreachable!(),
        }
    }

    #[test]
    fn fenced_code_is_kept_verbatim() {
        let b = parse("before\n```cpp\nint x = *p;\n```\nafter");
        assert_eq!(b.len(), 3);
        match &b[1] {
            Block::Code(t) => assert_eq!(t, "int x = *p;"),
            other => panic!("{other:?}"),
        }
    }

    /// A fence still open mid-stream renders what has arrived, not nothing.
    #[test]
    fn an_unclosed_fence_still_renders() {
        let b = parse("```\nint x = 1;");
        match b.last() {
            Some(Block::Code(t)) => assert_eq!(t, "int x = 1;"),
            other => panic!("{other:?}"),
        }
    }

    /// The real shape a model returns, end to end.
    #[test]
    fn a_realistic_model_response() {
        let src = "The fragment scales every vertex.\n\n\
                   1. **Loop Over Vertices**: uses `for (auto& v : verts)` to walk `verts`.\n\
                   2. **Scaling**: multiplies `v.pos` by `scale`.";
        let b = parse(src);
        assert_eq!(b.len(), 3);
        assert!(matches!(b[0], Block::Paragraph(_)));
        for item in &b[1..] {
            match item {
                Block::Item { spans, ordered, .. } => {
                    assert!(ordered);
                    let t = plain(spans);
                    assert!(!t.contains('*') && !t.contains('`'), "{t:?}");
                    assert!(spans.iter().any(|s| s.style.bold));
                    assert!(spans.iter().any(|s| s.style.code));
                }
                other => panic!("{other:?}"),
            }
        }
    }
}
