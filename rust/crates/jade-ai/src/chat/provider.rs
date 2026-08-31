//! The chat vocabulary shared by both providers.
//!
//! This is the layer the Explain and Visualize features talk to. It is
//! deliberately small: one request shape, one delta stream, one error enum.
//! Anything provider-specific (SSE event names, JSON body layout, credential
//! resolution) stays inside [`super::anthropic`] and [`super::local`].

use std::time::Duration;

use serde::{Deserialize, Serialize};

/// How hard the model should think. Maps to Anthropic's `output_config.effort`
/// and is ignored by llama-server, which has no equivalent knob.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Effort {
    /// Explain: prose about a code fragment, wanted in about a second.
    #[default]
    Low,
    /// Visualize: writing a Manim scene is a design task, not a lookup.
    Medium,
    High,
}

impl Effort {
    pub fn wire(self) -> &'static str {
        match self {
            Effort::Low => "low",
            Effort::Medium => "medium",
            Effort::High => "high",
        }
    }
}

/// One chat turn. `system` is held stable across calls on purpose — it carries
/// the prompt-cache breakpoint, so any per-request variation belongs in `user`.
#[derive(Debug, Clone)]
pub struct ChatRequest {
    pub system: String,
    pub user: String,
    pub max_tokens: u32,
    pub effort: Effort,
    /// `Some` forces a schema-validated JSON response (`output_config.format`).
    /// Visualize uses this; Explain leaves it `None` and streams prose.
    pub json_schema: Option<serde_json::Value>,
    /// Deadline for the whole stream, not for one chunk.
    pub timeout: Duration,
}

/// Why a stream stopped. Only the variants both features act on differently.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StopReason {
    EndTurn,
    /// The response was cut off. For Visualize this means a truncated script,
    /// so it is a failure, not a success.
    MaxTokens,
    Other(String),
}

/// Streamed output. Exactly one terminal delta (`Done` or `Failed`) is always
/// sent, so a consumer never has to time out waiting for the end.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ChatDelta {
    /// The request was accepted and the response has begun. Carries the
    /// provider that actually served it, which is not always the one the model
    /// setting names — an Anthropic model with no key falls back to the local
    /// server, and the card has to be able to say so.
    Started(ChatProviderId),
    /// The model is thinking; no visible text yet. Sent at most once.
    Thinking,
    /// A text delta. Append verbatim — never trim, the model controls spacing.
    Text(String),
    Done { stop_reason: StopReason },
    Failed(ChatError),
}

/// What the local llama-server is doing, as far as the chat layer knows.
///
/// Three states rather than `Option<endpoint>` because "coming up" and "not
/// there" call for completely different words in the card: one is a progress
/// message, the other is a setup instruction.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum LocalStatus {
    /// Not running, and nothing is bringing it up.
    #[default]
    Off,
    /// Spawning, or downloading weights on first run.
    Starting,
    /// The router is up at `endpoint`. Which model serves which request is
    /// chosen per request by its `model` field, so nothing further is needed
    /// here — see [`crate::presets`].
    Ready { endpoint: String },
}

/// Everything that can go wrong, classified so the card can say something
/// useful rather than printing a status code.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ChatError {
    /// No API key AND no local server. Not an error the user should see as a
    /// failure — it is a setup step, and the card renders it as one.
    NoCredential,
    /// No API key, but the local server is coming up. Purely transient: the
    /// same request will work in a few seconds, so the card says so and offers
    /// a retry rather than reporting a failure.
    LocalStarting,
    /// 401 / 403 — a key exists but the server rejected it.
    Auth,
    /// 429. `retry_after_ms` comes from the `retry-after` header when present.
    RateLimited { retry_after_ms: Option<u64> },
    /// 529 or 5xx — transient on the server side, worth a retry.
    Overloaded,
    /// `stop_reason: "refusal"`. The category is an open set, so it is carried
    /// as a string rather than an enum.
    Refusal { category: Option<String> },
    Timeout,
    /// Connection-level failure: DNS, TLS, a dropped socket.
    Transport(String),
    /// The server answered, but not in a shape we can read.
    Protocol(String),
    /// Superseded by a newer request, or the card was closed.
    Canceled,
}

impl ChatError {
    /// Whether the card should offer a Retry button. A refusal or a missing
    /// credential will not fix itself, so retrying only wastes a request.
    pub fn retryable(&self) -> bool {
        matches!(
            self,
            ChatError::LocalStarting
                | ChatError::RateLimited { .. }
                | ChatError::Overloaded
                | ChatError::Timeout
                | ChatError::Transport(_)
                | ChatError::Protocol(_)
        )
    }

    /// One line for the card's banner headline.
    pub fn headline(&self) -> &'static str {
        match self {
            ChatError::NoCredential => "No model available",
            ChatError::LocalStarting => "Starting the local model",
            ChatError::Auth => "The API key was rejected",
            ChatError::RateLimited { .. } => "Rate limited",
            ChatError::Overloaded => "The service is busy",
            ChatError::Refusal { .. } => "The model declined this request",
            ChatError::Timeout => "The request timed out",
            ChatError::Transport(_) => "Could not reach the service",
            ChatError::Protocol(_) => "Unexpected response",
            ChatError::Canceled => "Canceled",
        }
    }

    /// The detail line, when there is one worth showing.
    pub fn detail(&self) -> Option<String> {
        match self {
            ChatError::NoCredential => Some(
                "Set ANTHROPIC_API_KEY for Claude, or turn on AI in the sparkle menu \
                 to run a local model."
                    .to_string(),
            ),
            ChatError::LocalStarting => Some(
                "The local model server is coming up. First run downloads the weights, \
                 which takes a few minutes."
                    .to_string(),
            ),
            ChatError::RateLimited {
                retry_after_ms: Some(ms),
            } => Some(format!("Try again in {} seconds.", ms.div_ceil(1000))),
            ChatError::Refusal {
                category: Some(cat),
            } => Some(format!("Category: {cat}.")),
            ChatError::Transport(e) | ChatError::Protocol(e) => Some(e.clone()),
            _ => None,
        }
    }
}

/// Which provider is serving chat. Persisted in `~/.config/jade/ai.json`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ChatProviderId {
    #[default]
    Anthropic,
    /// The llama-server this crate already supervises, over its OpenAI-shaped
    /// `/v1/chat/completions`. Offline, but a small FIM-tuned model writes weak
    /// explanations and cannot produce a valid Manim scene.
    LlamaServer,
}

/// The model to serve chat with.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ChatModel {
    #[default]
    ClaudeOpus5,
    ClaudeSonnet5,
    ClaudeHaiku45,
    /// Whatever the local llama-server has loaded.
    Local,
}

impl ChatModel {
    /// The exact `model` string to put on the wire. These are complete as-is:
    /// appending a date suffix is a 404.
    pub fn wire(self) -> Option<&'static str> {
        match self {
            ChatModel::ClaudeOpus5 => Some("claude-opus-5"),
            ChatModel::ClaudeSonnet5 => Some("claude-sonnet-5"),
            ChatModel::ClaudeHaiku45 => Some("claude-haiku-4-5"),
            ChatModel::Local => None,
        }
    }

    pub fn provider(self) -> ChatProviderId {
        match self {
            ChatModel::Local => ChatProviderId::LlamaServer,
            _ => ChatProviderId::Anthropic,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            ChatModel::ClaudeOpus5 => "Claude Opus 5",
            ChatModel::ClaudeSonnet5 => "Claude Sonnet 5",
            ChatModel::ClaudeHaiku45 => "Claude Haiku 4.5",
            ChatModel::Local => "Local (llama-server)",
        }
    }

    /// Whether this model can be trusted to write a runnable Manim scene.
    /// The local tier is FIM-tuned and cannot, so Visualize hides itself.
    pub fn can_write_scenes(self) -> bool {
        !matches!(self, ChatModel::Local)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A date suffix on any of these is a 404. Pin them with a test so a
    /// well-meaning edit cannot quietly add one.
    #[test]
    fn model_ids_are_undated() {
        assert_eq!(ChatModel::ClaudeOpus5.wire(), Some("claude-opus-5"));
        assert_eq!(ChatModel::ClaudeSonnet5.wire(), Some("claude-sonnet-5"));
        assert_eq!(ChatModel::ClaudeHaiku45.wire(), Some("claude-haiku-4-5"));
        assert_eq!(ChatModel::Local.wire(), None);
        for m in [
            ChatModel::ClaudeOpus5,
            ChatModel::ClaudeSonnet5,
            ChatModel::ClaudeHaiku45,
        ] {
            let id = m.wire().unwrap();
            assert!(
                !id.chars().rev().take(8).all(|c| c.is_ascii_digit()),
                "{id} looks date-suffixed"
            );
        }
    }

    #[test]
    fn only_transient_errors_offer_retry() {
        assert!(ChatError::Overloaded.retryable());
        assert!(ChatError::Timeout.retryable());
        assert!(ChatError::RateLimited { retry_after_ms: None }.retryable());
        assert!(!ChatError::NoCredential.retryable());
        assert!(!ChatError::Auth.retryable());
        assert!(!ChatError::Refusal { category: None }.retryable());
        assert!(!ChatError::Canceled.retryable());
    }

    #[test]
    fn rate_limit_detail_rounds_up_to_whole_seconds() {
        let e = ChatError::RateLimited {
            retry_after_ms: Some(1500),
        };
        assert_eq!(e.detail().as_deref(), Some("Try again in 2 seconds."));
    }

    #[test]
    fn local_tier_cannot_write_scenes() {
        assert!(!ChatModel::Local.can_write_scenes());
        assert!(ChatModel::ClaudeOpus5.can_write_scenes());
    }
}
