//! Anthropic Messages API, streaming.
//!
//! Rust has no official Anthropic SDK, so this speaks the HTTP API directly
//! over the `reqwest` + rustls client this crate already carries.
//!
//! Four things about the current API are easy to get wrong and each one is a
//! 400 or a silent behavior change, so they are pinned by tests below:
//!
//!   - Model IDs are complete as written (`claude-opus-5`). A date suffix 404s.
//!   - Thinking is on by default on these models. `budget_tokens` is REJECTED;
//!     depth is steered with `output_config.effort`. Explicitly disabling
//!     thinking is worse than leaving it on — it can leak `<thinking>` tags
//!     into the visible text — so this client never sends a `thinking` field.
//!   - Assistant prefill was removed; a trailing assistant message is a 400.
//!     Response shape is controlled with `output_config.format` instead.
//!   - `stop_reason: "refusal"` arrives as a normal HTTP 200. It must be
//!     checked before the content is treated as an answer.

use std::time::Duration;

use serde_json::{json, Value};

use super::provider::{ChatDelta, ChatError, ChatModel, ChatProviderId, ChatRequest, StopReason};
use super::sse::SseEvent;

/// Which provider this module is, stamped on every `Started` delta.
const PROVIDER: ChatProviderId = ChatProviderId::Anthropic;

pub const API_URL: &str = "https://api.anthropic.com/v1/messages";
/// The only version this client has been written against.
pub const API_VERSION: &str = "2023-06-01";

/// Build the request body.
///
/// The system prompt is sent as a one-element array with a `cache_control`
/// breakpoint rather than a bare string: it is identical on every call for a
/// given feature, so caching it turns the fixed instructions into a cache read
/// after the first request of a session.
pub fn build_body(req: &ChatRequest, model: ChatModel) -> Value {
    let mut body = json!({
        "model": model.wire().unwrap_or("claude-opus-5"),
        "max_tokens": req.max_tokens,
        "stream": true,
        "system": [{
            "type": "text",
            "text": req.system,
            "cache_control": { "type": "ephemeral" },
        }],
        "messages": [{ "role": "user", "content": req.user }],
    });

    // `effort` lives inside output_config, not at the top level.
    let mut output_config = json!({ "effort": req.effort.wire() });
    if let Some(schema) = &req.json_schema {
        output_config["format"] = json!({
            "type": "json_schema",
            "schema": schema,
        });
    }
    body["output_config"] = output_config;
    body
}

/// The three headers every request needs.
pub fn headers(key: &str) -> [(&'static str, String); 3] {
    [
        ("x-api-key", key.to_string()),
        ("anthropic-version", API_VERSION.to_string()),
        ("content-type", "application/json".to_string()),
    ]
}

/// Classify a non-2xx response. `retry_after` is the raw header value.
pub fn classify_status(status: u16, retry_after: Option<&str>, body: &str) -> ChatError {
    match status {
        401 | 403 => ChatError::Auth,
        429 => ChatError::RateLimited {
            retry_after_ms: parse_retry_after(retry_after),
        },
        // 529 is Anthropic's "overloaded"; the rest of the 5xx family is
        // transient for the same purpose, so both retry.
        500..=599 => ChatError::Overloaded,
        _ => ChatError::Protocol(short_error(status, body)),
    }
}

/// `retry-after` is in seconds, and may be absent or unparseable.
fn parse_retry_after(v: Option<&str>) -> Option<u64> {
    v?.trim().parse::<f64>().ok().map(|s| (s * 1000.0) as u64)
}

/// Pull `error.message` out of an error body, falling back to a truncated
/// dump. The raw body can be long and is not useful in a card.
fn short_error(status: u16, body: &str) -> String {
    let msg = serde_json::from_str::<Value>(body)
        .ok()
        .and_then(|v| {
            v.get("error")
                .and_then(|e| e.get("message"))
                .and_then(|m| m.as_str())
                .map(str::to_string)
        })
        .unwrap_or_else(|| body.chars().take(200).collect());
    if msg.is_empty() {
        format!("HTTP {status}")
    } else {
        format!("HTTP {status}: {msg}")
    }
}

/// Incremental translation of the SSE stream into [`ChatDelta`]s.
///
/// Stateful because the terminal `message_stop` has to report the stop reason,
/// which arrives one event earlier on `message_delta`.
#[derive(Debug, Default)]
pub struct EventMapper {
    stop_reason: Option<String>,
    refusal_category: Option<String>,
    /// Thinking is announced once, however many thinking deltas arrive.
    announced_thinking: bool,
}

impl EventMapper {
    pub fn new() -> Self {
        Self::default()
    }

    /// Map one SSE event to zero or more deltas.
    pub fn map(&mut self, ev: &SseEvent) -> Vec<ChatDelta> {
        // The `event:` line and the payload's own `type` always agree; trust
        // the payload, because a proxy may drop the event line.
        let v: Value = match serde_json::from_str(&ev.data) {
            Ok(v) => v,
            Err(e) => {
                return vec![ChatDelta::Failed(ChatError::Protocol(format!(
                    "malformed event payload: {e}"
                )))]
            }
        };
        let ty = v.get("type").and_then(Value::as_str).unwrap_or(&ev.event);

        match ty {
            "message_start" => vec![ChatDelta::Started(PROVIDER)],

            "content_block_delta" => {
                let d = match v.get("delta") {
                    Some(d) => d,
                    None => return Vec::new(),
                };
                match d.get("type").and_then(Value::as_str) {
                    Some("text_delta") => d
                        .get("text")
                        .and_then(Value::as_str)
                        .filter(|t| !t.is_empty())
                        .map(|t| vec![ChatDelta::Text(t.to_string())])
                        .unwrap_or_default(),
                    Some("thinking_delta") | Some("signature_delta") => {
                        if self.announced_thinking {
                            Vec::new()
                        } else {
                            self.announced_thinking = true;
                            vec![ChatDelta::Thinking]
                        }
                    }
                    // input_json_delta and anything newer: not used here.
                    _ => Vec::new(),
                }
            }

            "message_delta" => {
                if let Some(d) = v.get("delta") {
                    if let Some(sr) = d.get("stop_reason").and_then(Value::as_str) {
                        self.stop_reason = Some(sr.to_string());
                    }
                }
                // `stop_details` is populated ONLY for a refusal and is null
                // for every other stop reason, so it must be guarded.
                self.refusal_category = v
                    .get("stop_details")
                    .and_then(|s| s.get("category"))
                    .and_then(Value::as_str)
                    .map(str::to_string);
                Vec::new()
            }

            "message_stop" => {
                let out = match self.stop_reason.as_deref() {
                    Some("refusal") => ChatDelta::Failed(ChatError::Refusal {
                        category: self.refusal_category.take(),
                    }),
                    Some("max_tokens") => ChatDelta::Done {
                        stop_reason: StopReason::MaxTokens,
                    },
                    Some("end_turn") | None => ChatDelta::Done {
                        stop_reason: StopReason::EndTurn,
                    },
                    Some(other) => ChatDelta::Done {
                        stop_reason: StopReason::Other(other.to_string()),
                    },
                };
                vec![out]
            }

            "error" => {
                let msg = v
                    .get("error")
                    .and_then(|e| e.get("message"))
                    .and_then(Value::as_str)
                    .unwrap_or("stream error");
                let kind = v
                    .get("error")
                    .and_then(|e| e.get("type"))
                    .and_then(Value::as_str)
                    .unwrap_or("");
                let err = if kind == "overloaded_error" {
                    ChatError::Overloaded
                } else {
                    ChatError::Protocol(msg.to_string())
                };
                vec![ChatDelta::Failed(err)]
            }

            // ping, content_block_start, content_block_stop: nothing to emit.
            _ => Vec::new(),
        }
    }
}

/// Default whole-stream deadline. Generous because a Visualize request writes
/// a few hundred lines of Python and thinking time counts against it.
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(120);

#[cfg(test)]
mod tests {
    use super::*;
    use super::super::provider::Effort;

    fn req() -> ChatRequest {
        ChatRequest {
            system: "SYS".into(),
            user: "USR".into(),
            max_tokens: 2048,
            effort: Effort::Low,
            json_schema: None,
            timeout: DEFAULT_TIMEOUT,
        }
    }

    fn ev(event: &str, data: &str) -> SseEvent {
        SseEvent {
            event: event.into(),
            data: data.into(),
        }
    }

    // ---- body shape -----------------------------------------------------

    #[test]
    fn body_has_the_required_fields() {
        let b = build_body(&req(), ChatModel::ClaudeOpus5);
        assert_eq!(b["model"], "claude-opus-5");
        assert_eq!(b["max_tokens"], 2048);
        assert_eq!(b["stream"], true);
        assert_eq!(b["messages"][0]["role"], "user");
        assert_eq!(b["messages"][0]["content"], "USR");
    }

    /// `budget_tokens` and an explicit `thinking` block are both wrong on
    /// these models — one is a 400, the other leaks tags into the answer.
    #[test]
    fn body_never_sends_thinking_or_budget_tokens() {
        let b = build_body(&req(), ChatModel::ClaudeOpus5);
        assert!(b.get("thinking").is_none(), "{b}");
        assert!(!b.to_string().contains("budget_tokens"), "{b}");
    }

    /// Prefill was removed; only a single user message may be sent.
    #[test]
    fn body_has_no_assistant_prefill() {
        let b = build_body(&req(), ChatModel::ClaudeOpus5);
        let msgs = b["messages"].as_array().unwrap();
        assert_eq!(msgs.len(), 1);
        assert!(msgs.iter().all(|m| m["role"] == "user"));
    }

    /// effort belongs inside output_config, not at the top level.
    #[test]
    fn effort_is_nested_in_output_config() {
        let b = build_body(&req(), ChatModel::ClaudeOpus5);
        assert_eq!(b["output_config"]["effort"], "low");
        assert!(b.get("effort").is_none());
    }

    #[test]
    fn system_carries_a_cache_breakpoint() {
        let b = build_body(&req(), ChatModel::ClaudeOpus5);
        assert_eq!(b["system"][0]["text"], "SYS");
        assert_eq!(b["system"][0]["cache_control"]["type"], "ephemeral");
    }

    #[test]
    fn a_schema_becomes_output_config_format() {
        let mut r = req();
        r.json_schema = Some(json!({"type": "object"}));
        let b = build_body(&r, ChatModel::ClaudeOpus5);
        assert_eq!(b["output_config"]["format"]["type"], "json_schema");
        assert_eq!(b["output_config"]["format"]["schema"]["type"], "object");
        // The deprecated top-level spelling must not appear.
        assert!(b.get("output_format").is_none());
    }

    #[test]
    fn no_schema_means_no_format_key() {
        let b = build_body(&req(), ChatModel::ClaudeOpus5);
        assert!(b["output_config"].get("format").is_none());
    }

    // ---- status classification ------------------------------------------

    #[test]
    fn status_classification() {
        assert_eq!(classify_status(401, None, ""), ChatError::Auth);
        assert_eq!(classify_status(403, None, ""), ChatError::Auth);
        assert_eq!(classify_status(529, None, ""), ChatError::Overloaded);
        assert_eq!(classify_status(503, None, ""), ChatError::Overloaded);
        assert_eq!(
            classify_status(429, Some("2"), ""),
            ChatError::RateLimited { retry_after_ms: Some(2000) }
        );
        assert_eq!(
            classify_status(429, None, ""),
            ChatError::RateLimited { retry_after_ms: None }
        );
    }

    #[test]
    fn a_400_surfaces_the_server_message() {
        let body = r#"{"error":{"type":"invalid_request_error","message":"thinking.budget_tokens: unsupported"}}"#;
        match classify_status(400, None, body) {
            ChatError::Protocol(m) => assert!(m.contains("budget_tokens"), "{m}"),
            other => panic!("{other:?}"),
        }
    }

    // ---- event mapping ---------------------------------------------------

    #[test]
    fn a_plain_text_turn() {
        let mut m = EventMapper::new();
        assert_eq!(
            m.map(&ev("message_start", r#"{"type":"message_start"}"#)),
            vec![ChatDelta::Started(PROVIDER)]
        );
        assert!(m
            .map(&ev("content_block_start", r#"{"type":"content_block_start"}"#))
            .is_empty());
        assert_eq!(
            m.map(&ev(
                "content_block_delta",
                r#"{"type":"content_block_delta","delta":{"type":"text_delta","text":"Hel"}}"#
            )),
            vec![ChatDelta::Text("Hel".into())]
        );
        assert!(m
            .map(&ev("message_delta", r#"{"type":"message_delta","delta":{"stop_reason":"end_turn"}}"#))
            .is_empty());
        assert_eq!(
            m.map(&ev("message_stop", r#"{"type":"message_stop"}"#)),
            vec![ChatDelta::Done { stop_reason: StopReason::EndTurn }]
        );
    }

    /// Leading and trailing spaces are the model's, not ours.
    #[test]
    fn text_deltas_are_not_trimmed() {
        let mut m = EventMapper::new();
        let got = m.map(&ev(
            "content_block_delta",
            r#"{"type":"content_block_delta","delta":{"type":"text_delta","text":"  a  "}}"#,
        ));
        assert_eq!(got, vec![ChatDelta::Text("  a  ".into())]);
    }

    #[test]
    fn thinking_is_announced_only_once() {
        let mut m = EventMapper::new();
        let e = ev(
            "content_block_delta",
            r#"{"type":"content_block_delta","delta":{"type":"thinking_delta","thinking":"…"}}"#,
        );
        assert_eq!(m.map(&e), vec![ChatDelta::Thinking]);
        assert!(m.map(&e).is_empty());
        assert!(m.map(&e).is_empty());
    }

    /// A refusal is an HTTP 200 whose stop_reason says otherwise.
    #[test]
    fn a_refusal_becomes_a_failure_not_a_success() {
        let mut m = EventMapper::new();
        m.map(&ev(
            "message_delta",
            r#"{"type":"message_delta","delta":{"stop_reason":"refusal"},"stop_details":{"type":"refusal","category":"cyber"}}"#,
        ));
        assert_eq!(
            m.map(&ev("message_stop", r#"{"type":"message_stop"}"#)),
            vec![ChatDelta::Failed(ChatError::Refusal {
                category: Some("cyber".into())
            })]
        );
    }

    /// stop_details is null on every non-refusal stop, so reading it must not
    /// panic or invent a category.
    #[test]
    fn null_stop_details_is_handled() {
        let mut m = EventMapper::new();
        m.map(&ev(
            "message_delta",
            r#"{"type":"message_delta","delta":{"stop_reason":"end_turn"},"stop_details":null}"#,
        ));
        assert_eq!(
            m.map(&ev("message_stop", r#"{"type":"message_stop"}"#)),
            vec![ChatDelta::Done { stop_reason: StopReason::EndTurn }]
        );
    }

    /// A truncated response must be distinguishable — for Visualize it means
    /// a half-written script, which is a failure rather than a result.
    #[test]
    fn max_tokens_is_reported_distinctly() {
        let mut m = EventMapper::new();
        m.map(&ev(
            "message_delta",
            r#"{"type":"message_delta","delta":{"stop_reason":"max_tokens"}}"#,
        ));
        assert_eq!(
            m.map(&ev("message_stop", r#"{"type":"message_stop"}"#)),
            vec![ChatDelta::Done { stop_reason: StopReason::MaxTokens }]
        );
    }

    #[test]
    fn a_mid_stream_error_event_is_classified() {
        let mut m = EventMapper::new();
        assert_eq!(
            m.map(&ev(
                "error",
                r#"{"type":"error","error":{"type":"overloaded_error","message":"Overloaded"}}"#
            )),
            vec![ChatDelta::Failed(ChatError::Overloaded)]
        );
    }

    #[test]
    fn a_malformed_payload_fails_rather_than_panicking() {
        let mut m = EventMapper::new();
        match m.map(&ev("message_delta", "{not json")).as_slice() {
            [ChatDelta::Failed(ChatError::Protocol(_))] => {}
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn ping_and_unknown_events_are_ignored() {
        let mut m = EventMapper::new();
        assert!(m.map(&ev("ping", r#"{"type":"ping"}"#)).is_empty());
        assert!(m.map(&ev("whats_this", r#"{"type":"whats_this"}"#)).is_empty());
    }

    /// A stream that stops without ever reporting a reason still terminates.
    #[test]
    fn message_stop_without_a_message_delta() {
        let mut m = EventMapper::new();
        assert_eq!(
            m.map(&ev("message_stop", r#"{"type":"message_stop"}"#)),
            vec![ChatDelta::Done { stop_reason: StopReason::EndTurn }]
        );
    }
}
