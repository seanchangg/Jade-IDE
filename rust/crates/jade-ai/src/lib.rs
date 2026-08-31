//! Jade AI inline-completion backend — Rust port of
//! `src/main/inline-completion.ts` (the Electron main-process class
//! `InlineCompletionBackend`).
//!
//! The backend speaks llama.cpp's HTTP `/infill` (fill-in-the-middle) + `/health`
//! API. It resolves an endpoint in three steps and, failing that, spawns and
//! supervises its own `llama-server` child process (TS :7-13):
//!   1. `JADE_FIM_ENDPOINT` env var (user-managed server, any host)
//!   2. `http://127.0.0.1:8012` (llama.vscode convention — adopt if running)
//!   3. `http://127.0.0.1:8630` (our managed port — adopts a crashed run's orphan)
//!   4. spawn our own `llama-server` with a small FIM model (downloads on first run)
//!
//! See `docs/jade-feature-inventory.md` §8.7 for the preserved contract. This is
//! a "thin client" identity feature: any server honoring `/health` +
//! `/infill {content}` can replace the managed one via `JADE_FIM_ENDPOINT`.
//!
//! No GUI dependencies: status is published over a [`tokio::sync::watch`] channel
//! (the analogue of the TS `onStatus` listener list, TS :113-115) for Phase-3
//! wiring to consume.

mod backend;
pub mod chat;
pub mod presets;

pub use backend::InlineCompletionBackend;
pub use chat::{
    ApiKey, ChatBackend, ChatDelta, ChatError, ChatModel, ChatProviderId, ChatRequest, Effort,
    KeySource, Lane, LocalStatus, StopReason,
};
pub use presets::ChatTier;

use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// Managed-model tiers — all FIM-tuned Qwen2.5-Coder GGUFs served by llama.cpp.
/// Mirrors `AI_MODELS` / `AiModelId` (TS :19-23, shared/types.ts :281).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AiModelId {
    /// Qwen2.5-Coder 0.5B — the JetBrains-FLCC-style tier: the smallest
    /// multi-language FIM model llama.cpp serves well. Measured ~260ms warm
    /// single-line time-to-show vs ~1.4s for the 3B on an Apple-Silicon M-class.
    Fastest,
    /// Qwen2.5-Coder 1.5B — the default tier (`private modelId = 'fast'`, TS :20, :38).
    #[default]
    Fast,
    /// Qwen2.5-Coder 3B (TS :21).
    Balanced,
    /// Qwen2.5-Coder 7B (TS :22).
    Best,
    /// sprite-100m — the from-scratch 100M model trained in `ml/`. Not a
    /// download: it serves a GGUF off disk, so the tier only works on a
    /// machine that has one. See [`sprite_model_path`].
    ///
    /// The alias keeps a saved `~/.config/jade/ai.json` that still says
    /// `"ghost"` working; the model carried that name until 2026-08-09.
    #[serde(alias = "ghost")]
    Sprite,
}

/// Where `llama-server` gets the weights.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ModelSource {
    /// A HuggingFace repo, downloaded on first use (`-hf`).
    Hf(&'static str),
    /// A GGUF already on disk (`-m`).
    Local(PathBuf),
}

/// The GGUF the `Sprite` tier serves. `$JADE_SPRITE_MODEL` overrides it, so a
/// fresh export can be tried without rebuilding the app.
///
/// `sprite-100m-v2-mw` (2026-08-08): the v2 from-scratch run plus a
/// middle-loss-weight anneal. 41.3% Perfect Lines at Q8_0 on metalLLM, against
/// 17% for the model it replaced. Name the file explicitly rather than a moving
/// "final" alias, so the shipped model is readable from the source.
///
/// The runs in `ml/train/runs/` keep their original `ghost-*` names. Those are
/// the record the logs and `HANDOFF.md` refer to, so renaming them would break
/// the trail; only the shipped model took the new name.
pub fn sprite_model_path() -> PathBuf {
    match std::env::var("JADE_SPRITE_MODEL") {
        Ok(p) if !p.is_empty() => PathBuf::from(p),
        _ => PathBuf::from(std::env::var("HOME").unwrap_or_default())
            .join("models/sprite-100m/sprite-100m-v2-mw-q8_0.gguf"),
    }
}

impl AiModelId {
    /// Where the weights come from (TS :20-22 knew only `-hf`).
    pub fn source(self) -> ModelSource {
        match self {
            AiModelId::Fastest => ModelSource::Hf("ggml-org/Qwen2.5-Coder-0.5B-Q8_0-GGUF"),
            AiModelId::Fast => ModelSource::Hf("ggml-org/Qwen2.5-Coder-1.5B-Q8_0-GGUF"),
            AiModelId::Balanced => ModelSource::Hf("ggml-org/Qwen2.5-Coder-3B-Q8_0-GGUF"),
            AiModelId::Best => ModelSource::Hf("ggml-org/Qwen2.5-Coder-7B-Q8_0-GGUF"),
            AiModelId::Sprite => ModelSource::Local(sprite_model_path()),
        }
    }

    /// `-b` / `-ub` for this model. llama-server derives how much prefix
    /// `/infill` keeps from the batch size — `n_prefix_take = 3*(n_batch/4)`
    /// — so this is a quality knob, not only a throughput one.
    ///
    /// sprite-100m trains on prefixes up to 1280 tokens; 1024 would feed it
    /// only 768. Measured on metalLLM, 400 cases, two independent case
    /// samples: 17.0%/13.2% Perfect Lines at `-b 1024` against 21.2%/21.8%
    /// at 1792. CAUTION — that curve is NOT monotonic (1408 scores below
    /// 1024, and 1920 collapses to ~9%), so 1792 is the best measured point,
    /// not an understood optimum. See `ml/HANDOFF.md`.
    pub fn batch(self) -> u32 {
        match self {
            AiModelId::Sprite => 1792,
            _ => 1024,
        }
    }

    /// Human-readable label used in status detail strings (TS :20-22).
    pub fn label(self) -> &'static str {
        match self {
            AiModelId::Fastest => "Qwen2.5-Coder 0.5B",
            AiModelId::Fast => "Qwen2.5-Coder 1.5B",
            AiModelId::Balanced => "Qwen2.5-Coder 3B",
            AiModelId::Best => "Qwen2.5-Coder 7B",
            AiModelId::Sprite => "sprite-100m",
        }
    }
}

/// The four completion states (shared/types.ts :278 `AiCompletionState`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AiState {
    Disabled,
    Starting,
    Ready,
    Error,
}

/// Status snapshot broadcast on the watch channel — port of the `AiStatus`
/// interface (shared/types.ts :283-287). `endpoint` is populated only in the
/// `Ready` state.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AiStatus {
    pub state: AiState,
    pub detail: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub endpoint: Option<String>,
}

impl AiStatus {
    fn new(state: AiState, detail: impl Into<String>) -> Self {
        AiStatus {
            state,
            detail: detail.into(),
            endpoint: None,
        }
    }

    fn ready(detail: impl Into<String>, endpoint: impl Into<String>) -> Self {
        AiStatus {
            state: AiState::Ready,
            detail: detail.into(),
            endpoint: Some(endpoint.into()),
        }
    }
}

/// A fill-in-the-middle request — port of `InfillRequest` (shared/types.ts
/// :289-296). `single_line` maps to the TS `singleLine` (default `false`).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct InfillRequest {
    pub prefix: String,
    pub suffix: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub filename: Option<String>,
    /// Single-line mode: the server stops at the first newline — lower latency,
    /// CLion-style one-line ghost text (shared/types.ts :293-295).
    #[serde(default)]
    pub single_line: bool,
}

/// The one field we read back from a `/infill` response — port of
/// `InfillResult` (shared/types.ts :298-300).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InfillResult {
    pub content: String,
}
