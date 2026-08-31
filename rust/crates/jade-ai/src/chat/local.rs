//! The local llama-server chat path.
//!
//! [`crate::backend::InlineCompletionBackend`] already resolves, spawns, and
//! supervises a `llama-server`; this module borrows its endpoint and speaks the
//! OpenAI-shaped `/v1/chat/completions` that llama.cpp also serves.
//!
//! This keeps Jade's offline identity intact (inventory §8.7: any server
//! honoring the contract can replace the managed one). It is honestly limited:
//! the managed tiers are FIM-tuned coder models, so they write thin
//! explanations, and they cannot be trusted to emit a runnable Manim scene —
//! [`super::ChatModel::can_write_scenes`] is false for the local tier and the
//! Visualize feature hides itself accordingly.

use serde_json::{json, Value};

use super::provider::{ChatDelta, ChatError, ChatProviderId, ChatRequest, StopReason};
use super::sse::SseEvent;

/// Which provider this module is, stamped on every `Started` delta.
const PROVIDER: ChatProviderId = ChatProviderId::LlamaServer;

/// The sentinel llama.cpp sends instead of a typed terminal event.
const DONE: &str = "[DONE]";

/// ChatML turn markers. llama.cpp's generic fallback template emits these, but
/// a base model's EOS is usually `<|endoftext|>`, so nothing stops generation
/// at the end of a turn and the model emits `<|im_end|><|im_start|>assistant`
/// on a loop. Sending them as explicit stop strings ends the turn even when the
/// tokenizer's EOS disagrees with the template.
const CHATML_STOPS: [&str; 3] = ["<|im_end|>", "<|im_start|>", "<|endoftext|>"];

/// Build the request body. llama-server ignores `model`, but the field is
/// required by the OpenAI schema it validates against.
pub fn build_body(req: &ChatRequest) -> Value {
    json!({
        // Router mode serves two presets; name the instruct one. A request that
        // reached the completion preset would stream ChatML control tokens.
        "model": crate::presets::CHAT_MODEL,
        "stream": true,
        "max_tokens": req.max_tokens,
        // Low but not zero: explanation prose reads badly at temp 0, and the
        // smaller tiers loop on repeated tokens without a little slack.
        "temperature": 0.3,
        "stop": CHATML_STOPS,
        "messages": [
            { "role": "system", "content": req.system },
            { "role": "user", "content": req.user },
        ],
    })
}

/// Strip `<|...|>` control tokens from a text delta.
///
/// Belt to [`CHATML_STOPS`]' braces: a stop string only ends generation once
/// the whole sequence has been emitted, and a delta can still carry a partial
/// or an unmatched one. Nothing legitimate in prose looks like `<|name|>`.
pub fn strip_control_tokens(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(i) = rest.find("<|") {
        out.push_str(&rest[..i]);
        match rest[i..].find("|>") {
            Some(j) => rest = &rest[i + j + 2..],
            // An unterminated marker: drop the tail rather than emit `<|im_`.
            None => return out,
        }
    }
    out.push_str(rest);
    out
}

/// Whether a finished local response is degenerate — nothing but control
/// tokens, whitespace, or a handful of characters.
///
/// A completion model fed a chat template produces exactly this, and showing it
/// is worse than reporting a failure.
pub fn is_degenerate(body: &str) -> bool {
    strip_control_tokens(body).trim().len() < 8
}

/// Read the served model's capabilities from `GET /v1/models`.
///
/// llama-server derives this from the loaded GGUF: a base or fill-in-the-middle
/// model reports `["completion"]` only, an instruct model also reports `"chat"`.
/// This is the authoritative signal for whether chat will work — the presence of
/// a `chat_template` is not, because llama.cpp synthesizes a generic one for
/// every model.
pub fn parse_capabilities(models_json: &str) -> Option<(String, Vec<String>)> {
    let v: Value = serde_json::from_str(models_json).ok()?;
    let m = v.get("models")?.get(0)?;
    let name = m
        .get("name")
        .or_else(|| m.get("model"))
        .and_then(Value::as_str)
        .unwrap_or("the local model");
    // Report the file stem; the full path is noise in a card.
    let short = name.rsplit('/').next().unwrap_or(name).to_string();
    let caps = m
        .get("capabilities")?
        .as_array()?
        .iter()
        .filter_map(Value::as_str)
        .map(str::to_string)
        .collect();
    Some((short, caps))
}

/// True when the capability list says the model can hold a conversation.
pub fn is_chat_capable(caps: &[String]) -> bool {
    caps.iter().any(|c| c == "chat")
}

/// Translate the OpenAI-shaped stream into [`ChatDelta`]s.
///
/// Stateless apart from the terminal bookkeeping: unlike Anthropic, the stop
/// reason rides on the same chunk as the last content.
#[derive(Debug, Default)]
pub struct EventMapper {
    started: bool,
    finish_reason: Option<String>,
}

impl EventMapper {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn map(&mut self, ev: &SseEvent) -> Vec<ChatDelta> {
        if ev.data.trim() == DONE {
            let stop = match self.finish_reason.as_deref() {
                Some("length") => StopReason::MaxTokens,
                Some("stop") | None => StopReason::EndTurn,
                Some(other) => StopReason::Other(other.to_string()),
            };
            return vec![ChatDelta::Done { stop_reason: stop }];
        }

        let v: Value = match serde_json::from_str(&ev.data) {
            Ok(v) => v,
            Err(e) => {
                return vec![ChatDelta::Failed(ChatError::Protocol(format!(
                    "malformed chunk: {e}"
                )))]
            }
        };

        // llama.cpp reports its own errors in-band as an `error` object.
        if let Some(err) = v.get("error") {
            let msg = err
                .get("message")
                .and_then(Value::as_str)
                .unwrap_or("server error");
            return vec![ChatDelta::Failed(ChatError::Protocol(msg.to_string()))];
        }

        let choice = match v.get("choices").and_then(|c| c.get(0)) {
            Some(c) => c,
            None => return Vec::new(),
        };
        if let Some(fr) = choice.get("finish_reason").and_then(Value::as_str) {
            self.finish_reason = Some(fr.to_string());
        }

        let mut out = Vec::new();
        if !self.started {
            self.started = true;
            out.push(ChatDelta::Started(PROVIDER));
        }
        if let Some(text) = choice
            .get("delta")
            .and_then(|d| d.get("content"))
            .and_then(Value::as_str)
            .filter(|t| !t.is_empty())
        {
            let clean = strip_control_tokens(text);
            if !clean.is_empty() {
                out.push(ChatDelta::Text(clean));
            }
        }
        out
    }

    /// Some builds close the stream after the last chunk without sending
    /// `[DONE]`. Terminate anyway rather than leaving the card spinning.
    pub fn finish(&mut self) -> ChatDelta {
        ChatDelta::Done {
            stop_reason: match self.finish_reason.as_deref() {
                Some("length") => StopReason::MaxTokens,
                _ => StopReason::EndTurn,
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use super::super::provider::Effort;
    use std::time::Duration;

    fn req() -> ChatRequest {
        ChatRequest {
            system: "SYS".into(),
            user: "USR".into(),
            max_tokens: 512,
            effort: Effort::Low,
            json_schema: None,
            timeout: Duration::from_secs(60),
        }
    }

    fn ev(data: &str) -> SseEvent {
        SseEvent {
            event: String::new(),
            data: data.into(),
        }
    }

    #[test]
    fn body_uses_system_and_user_roles() {
        let b = build_body(&req());
        assert_eq!(b["stream"], true);
        assert_eq!(b["messages"][0]["role"], "system");
        assert_eq!(b["messages"][0]["content"], "SYS");
        assert_eq!(b["messages"][1]["role"], "user");
        assert_eq!(b["messages"][1]["content"], "USR");
    }

    #[test]
    fn first_chunk_announces_the_start_once() {
        let mut m = EventMapper::new();
        let c = ev(r#"{"choices":[{"delta":{"content":"a"}}]}"#);
        assert_eq!(
            m.map(&c),
            vec![ChatDelta::Started(PROVIDER), ChatDelta::Text("a".into())]
        );
        assert_eq!(m.map(&c), vec![ChatDelta::Text("a".into())]);
    }

    #[test]
    fn role_only_chunk_yields_no_text() {
        let mut m = EventMapper::new();
        assert_eq!(
            m.map(&ev(r#"{"choices":[{"delta":{"role":"assistant"}}]}"#)),
            vec![ChatDelta::Started(PROVIDER)]
        );
    }

    #[test]
    fn done_sentinel_terminates() {
        let mut m = EventMapper::new();
        m.map(&ev(r#"{"choices":[{"delta":{"content":"a"}}]}"#));
        assert_eq!(
            m.map(&ev(DONE)),
            vec![ChatDelta::Done { stop_reason: StopReason::EndTurn }]
        );
    }

    #[test]
    fn length_finish_reason_maps_to_max_tokens() {
        let mut m = EventMapper::new();
        m.map(&ev(r#"{"choices":[{"delta":{"content":"a"},"finish_reason":"length"}]}"#));
        assert_eq!(
            m.map(&ev(DONE)),
            vec![ChatDelta::Done { stop_reason: StopReason::MaxTokens }]
        );
    }

    #[test]
    fn in_band_error_is_a_failure() {
        let mut m = EventMapper::new();
        match m.map(&ev(r#"{"error":{"message":"context full"}}"#)).as_slice() {
            [ChatDelta::Failed(ChatError::Protocol(msg))] => assert_eq!(msg, "context full"),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn a_stream_that_closes_without_done_still_terminates() {
        let mut m = EventMapper::new();
        m.map(&ev(r#"{"choices":[{"delta":{"content":"a"}}]}"#));
        assert_eq!(
            m.finish(),
            ChatDelta::Done { stop_reason: StopReason::EndTurn }
        );
    }

    #[test]
    fn malformed_chunk_fails_rather_than_panicking() {
        let mut m = EventMapper::new();
        match m.map(&ev("{oops")).as_slice() {
            [ChatDelta::Failed(ChatError::Protocol(_))] => {}
            other => panic!("{other:?}"),
        }
    }
}
