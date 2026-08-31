//! Incremental Server-Sent Events framer.
//!
//! Both providers stream SSE, and both arrive as arbitrary byte chunks from
//! `reqwest::Response::bytes_stream()` — a chunk boundary can fall anywhere,
//! including the middle of a `data:` line. So the decoder holds a buffer and
//! yields only whole events.
//!
//! The framer follows the WHATWG event-stream rules that the two servers
//! actually use: an event ends at a blank line, a line that starts with `:` is
//! a comment, `data:` lines concatenate with `\n` between them, and one
//! optional space after the colon is stripped. It does NOT implement
//! `retry:` or last-event-id, because neither provider sends them.

/// One decoded event. `event` is the `event:` field (empty when absent), and
/// `data` is every `data:` line joined with `\n`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SseEvent {
    pub event: String,
    pub data: String,
}

/// Feed bytes in, take whole events out.
#[derive(Debug, Default)]
pub struct SseDecoder {
    /// Bytes received but not yet part of a complete event.
    buf: String,
}

impl SseDecoder {
    pub fn new() -> Self {
        Self::default()
    }

    /// Append a chunk and drain every event it completed. A chunk that ends
    /// mid-event contributes nothing and stays buffered.
    pub fn push(&mut self, chunk: &str) -> Vec<SseEvent> {
        self.buf.push_str(chunk);
        let mut out = Vec::new();
        // An event ends at a blank line. Normalize CRLF first so the split
        // below only has to look for "\n\n" (llama.cpp sends LF, but a proxy
        // in front of either endpoint may rewrite to CRLF).
        while let Some(end) = find_event_end(&self.buf) {
            let raw: String = self.buf.drain(..end.terminator_end).collect();
            let block = &raw[..end.block_len];
            if let Some(ev) = parse_block(block) {
                out.push(ev);
            }
        }
        out
    }

    /// Flush whatever is left when the stream closes without a trailing blank
    /// line. Servers usually send one, but a truncated response should not
    /// silently drop its last event.
    pub fn finish(&mut self) -> Option<SseEvent> {
        let rest = std::mem::take(&mut self.buf);
        parse_block(rest.trim_end_matches(['\r', '\n']))
    }
}

/// Where the first event in `s` ends: `block_len` bytes of content, and
/// `terminator_end` bytes to remove from the buffer (content + the blank line).
struct EventEnd {
    block_len: usize,
    terminator_end: usize,
}

fn find_event_end(s: &str) -> Option<EventEnd> {
    // Check "\r\n\r\n" and "\n\n" independently and take whichever comes
    // first — a stream may legitimately mix them across a chunk boundary.
    let lf = s.find("\n\n").map(|i| EventEnd {
        block_len: i,
        terminator_end: i + 2,
    });
    let crlf = s.find("\r\n\r\n").map(|i| EventEnd {
        block_len: i,
        terminator_end: i + 4,
    });
    match (lf, crlf) {
        (Some(a), Some(b)) => Some(if a.block_len <= b.block_len { a } else { b }),
        (Some(a), None) => Some(a),
        (None, Some(b)) => Some(b),
        (None, None) => None,
    }
}

/// Parse one event block. Returns `None` for a block with no `data:` line —
/// a keep-alive comment (`: ping`) or a stray field carries no event.
fn parse_block(block: &str) -> Option<SseEvent> {
    let mut event = String::new();
    let mut data: Option<String> = None;

    for line in block.split('\n') {
        let line = line.strip_suffix('\r').unwrap_or(line);
        if line.is_empty() || line.starts_with(':') {
            continue; // blank or comment
        }
        let (field, value) = match line.split_once(':') {
            // One optional space after the colon is part of the framing.
            Some((f, v)) => (f, v.strip_prefix(' ').unwrap_or(v)),
            // A line with no colon is a field with an empty value.
            None => (line, ""),
        };
        match field {
            "event" => event = value.to_string(),
            "data" => match &mut data {
                Some(d) => {
                    d.push('\n');
                    d.push_str(value);
                }
                None => data = Some(value.to_string()),
            },
            _ => {} // id / retry / unknown — neither provider uses them
        }
    }

    data.map(|data| SseEvent { event, data })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ev(event: &str, data: &str) -> SseEvent {
        SseEvent {
            event: event.to_string(),
            data: data.to_string(),
        }
    }

    #[test]
    fn single_event() {
        let mut d = SseDecoder::new();
        let got = d.push("event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n");
        assert_eq!(got, vec![ev("message_stop", "{\"type\":\"message_stop\"}")]);
    }

    #[test]
    fn two_events_in_one_chunk() {
        let mut d = SseDecoder::new();
        let got = d.push("event: a\ndata: 1\n\nevent: b\ndata: 2\n\n");
        assert_eq!(got, vec![ev("a", "1"), ev("b", "2")]);
    }

    /// The case that actually bites: a chunk boundary inside a `data:` line.
    #[test]
    fn event_split_across_chunks() {
        let mut d = SseDecoder::new();
        assert!(d.push("event: content_block_de").is_empty());
        assert!(d.push("lta\ndata: {\"text\":\"hel").is_empty());
        let got = d.push("lo\"}\n\n");
        assert_eq!(got, vec![ev("content_block_delta", "{\"text\":\"hello\"}")]);
    }

    /// A boundary that lands between the two newlines of the terminator.
    #[test]
    fn split_inside_terminator() {
        let mut d = SseDecoder::new();
        assert!(d.push("data: x\n").is_empty());
        assert_eq!(d.push("\n"), vec![ev("", "x")]);
    }

    #[test]
    fn multi_line_data_joins_with_newline() {
        let mut d = SseDecoder::new();
        assert_eq!(d.push("data: one\ndata: two\n\n"), vec![ev("", "one\ntwo")]);
    }

    #[test]
    fn comments_and_blank_fields_are_skipped() {
        let mut d = SseDecoder::new();
        assert!(d.push(": ping\n\n").is_empty());
        assert_eq!(d.push(": keep-alive\ndata: v\n\n"), vec![ev("", "v")]);
    }

    #[test]
    fn crlf_terminator() {
        let mut d = SseDecoder::new();
        assert_eq!(d.push("event: a\r\ndata: 1\r\n\r\n"), vec![ev("a", "1")]);
    }

    #[test]
    fn no_space_after_colon() {
        let mut d = SseDecoder::new();
        assert_eq!(d.push("data:{\"a\":1}\n\n"), vec![ev("", "{\"a\":1}")]);
    }

    /// A value containing colons must survive: split_once, not split.
    #[test]
    fn value_may_contain_colons() {
        let mut d = SseDecoder::new();
        assert_eq!(
            d.push("data: {\"url\":\"https://x.test\"}\n\n"),
            vec![ev("", "{\"url\":\"https://x.test\"}")]
        );
    }

    #[test]
    fn done_sentinel_is_an_ordinary_event() {
        let mut d = SseDecoder::new();
        assert_eq!(d.push("data: [DONE]\n\n"), vec![ev("", "[DONE]")]);
    }

    #[test]
    fn finish_flushes_an_unterminated_tail() {
        let mut d = SseDecoder::new();
        assert!(d.push("event: a\ndata: 1").is_empty());
        assert_eq!(d.finish(), Some(ev("a", "1")));
        assert_eq!(d.finish(), None);
    }

    /// One byte at a time — the pathological chunking a proxy can produce.
    #[test]
    fn byte_at_a_time() {
        let mut d = SseDecoder::new();
        let src = "event: content_block_delta\ndata: {\"i\":0}\n\n";
        let mut got = Vec::new();
        for ch in src.chars() {
            got.extend(d.push(&ch.to_string()));
        }
        assert_eq!(got, vec![ev("content_block_delta", "{\"i\":0}")]);
    }

    /// A 200KB payload delivered in 4KB slices — one event, reassembled.
    #[test]
    fn large_event_survives_many_chunks() {
        let mut d = SseDecoder::new();
        let payload = "x".repeat(200_000);
        let line = format!("data: {payload}\n\n");
        let mut got = Vec::new();
        for chunk in line.as_bytes().chunks(4096) {
            got.extend(d.push(std::str::from_utf8(chunk).unwrap()));
        }
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].data, payload);
    }
}
