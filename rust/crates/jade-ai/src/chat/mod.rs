//! The chat layer: streaming instruction-following requests, used by the
//! Explain and Visualize editor features.
//!
//! This is separate from [`crate::backend`] on purpose. That module is a
//! fill-in-the-middle completion client with a 4-second budget and a single
//! server slot; this one is a long-lived streaming conversation with a
//! different provider, a different credential story, and a different failure
//! surface. They share only the crate.
//!
//! Layout:
//!   - [`provider`] — the vocabulary both providers speak.
//!   - [`sse`] — the incremental Server-Sent Events framer.
//!   - [`anthropic`] / [`local`] — body building and event mapping.
//!   - [`key`] — Anthropic credential resolution.
//!   - This file — the HTTP transport and the single-flight façade.

pub mod anthropic;
pub mod key;
pub mod local;
pub mod provider;
pub mod sse;

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, RwLock};
use std::time::Duration;

use tokio::sync::mpsc::UnboundedSender;
use tokio::task::AbortHandle;

pub use key::{ApiKey, KeySource};
pub use provider::{
    ChatDelta, ChatError, ChatModel, ChatProviderId, ChatRequest, Effort, LocalStatus, StopReason,
};
pub use sse::{SseDecoder, SseEvent};

/// Which feature a request belongs to.
///
/// Explain and Visualize are independent: pressing ⌘⇧M must not cancel an
/// in-flight ⌘⇧E. So the backend keeps one single-flight slot per lane rather
/// than one for the whole crate.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Lane {
    Explain,
    Visualize,
}

impl Lane {
    const ALL: [Lane; 2] = [Lane::Explain, Lane::Visualize];

    fn index(self) -> usize {
        match self {
            Lane::Explain => 0,
            Lane::Visualize => 1,
        }
    }
}

/// One lane's in-flight request: its generation, and the handle that cancels it.
#[derive(Default)]
struct Slot {
    inflight: Mutex<Option<(u64, AbortHandle)>>,
    gen: AtomicU64,
}

impl Slot {
    fn abort(&self) {
        if let Some((_, h)) = self.inflight.lock().unwrap().take() {
            h.abort();
        }
    }

    /// Clear the slot only if it still holds `gen` — a later request may have
    /// already replaced it, and clearing then would strand that one.
    fn clear_if(&self, gen: u64) {
        let mut g = self.inflight.lock().unwrap();
        if g.as_ref().map(|(g, _)| *g) == Some(gen) {
            *g = None;
        }
    }
}

/// The chat entry point. One per app.
pub struct ChatBackend {
    client: reqwest::Client,
    model: RwLock<ChatModel>,
    /// The resolved credential, looked up once and reused. `None` means either
    /// "not looked up yet" or "not present"; `checked` disambiguates.
    credential: RwLock<(Option<ApiKey>, KeySource, bool)>,
    /// What the local llama-server is doing, mirrored from the
    /// inline-completion backend's status. Drives the fallback below.
    local: RwLock<LocalStatus>,
    /// The runtime requests are spawned on.
    ///
    /// Explicit rather than ambient: [`ChatBackend::start`] is called from the
    /// GPUI thread, which is NOT inside a tokio runtime, so a bare
    /// `tokio::spawn` there panics with "no reactor running". A stored
    /// `Handle` schedules onto the app's runtime from anywhere.
    runtime: RwLock<Option<tokio::runtime::Handle>>,
    slots: [Slot; 2],
}

impl Default for ChatBackend {
    fn default() -> Self {
        Self::new()
    }
}

impl ChatBackend {
    pub fn new() -> Self {
        ChatBackend {
            // One shared client so connections and the TLS session are reused
            // across requests — the same reason `backend.rs` keeps one.
            client: reqwest::Client::builder()
                .connect_timeout(Duration::from_secs(10))
                .build()
                .unwrap_or_default(),
            model: RwLock::new(ChatModel::default()),
            credential: RwLock::new((None, KeySource::None, false)),
            local: RwLock::new(LocalStatus::Off),
            runtime: RwLock::new(None),
            slots: [Slot::default(), Slot::default()],
        }
    }

    /// Point the backend at the runtime to spawn requests on. Must be called
    /// before the first [`start`](Self::start); without it, `start` falls back
    /// to the ambient runtime and, failing that, reports a transport error
    /// rather than panicking.
    pub fn set_runtime(&self, handle: tokio::runtime::Handle) {
        *self.runtime.write().unwrap() = Some(handle);
    }

    fn handle(&self) -> Option<tokio::runtime::Handle> {
        self.runtime
            .read()
            .unwrap()
            .clone()
            .or_else(|| tokio::runtime::Handle::try_current().ok())
    }

    pub fn model(&self) -> ChatModel {
        *self.model.read().unwrap()
    }

    pub fn set_model(&self, m: ChatModel) {
        *self.model.write().unwrap() = m;
    }

    /// Mirror the local server's state. Called from the app's `AiStatus`
    /// handler, so the two backends stay in step without this one having to
    /// know how the server is supervised.
    pub fn set_local_status(&self, status: LocalStatus) {
        *self.local.write().unwrap() = status;
    }

    pub fn local_status(&self) -> LocalStatus {
        self.local.read().unwrap().clone()
    }

    /// A dedicated chat endpoint, overriding the managed server.
    ///
    /// The escape hatch for exactly the case this gate exists for: keep the
    /// small fill-in-the-middle model on the ghost-text server, and point chat
    /// at a second llama-server running an instruct model.
    pub fn endpoint_override() -> Option<String> {
        std::env::var("JADE_CHAT_ENDPOINT")
            .ok()
            .filter(|e| !e.trim().is_empty())
    }

    /// Which provider a request would use right now, and why.
    ///
    /// Exposed so the UI can show the answer BEFORE a request is sent — a card
    /// that says "using the local model" up front is far better than one that
    /// silently produces worse prose than the user expected.
    pub fn effective_provider(&self) -> Result<ChatProviderId, ChatError> {
        match self.model().provider() {
            ChatProviderId::Anthropic => {
                if self.credential().0.is_some() {
                    return Ok(ChatProviderId::Anthropic);
                }
                // No key is not a dead end: fall back to the local server the
                // app already supervises, rather than refusing to answer.
                match self.local_status() {
                    LocalStatus::Ready { .. } => Ok(ChatProviderId::LlamaServer),
                    LocalStatus::Starting => Err(ChatError::LocalStarting),
                    LocalStatus::Off => Err(ChatError::NoCredential),
                }
            }
            ChatProviderId::LlamaServer => match self.local_status() {
                LocalStatus::Ready { .. } => Ok(ChatProviderId::LlamaServer),
                LocalStatus::Starting => Err(ChatError::LocalStarting),
                // The local tier was chosen explicitly, so do NOT silently
                // reach for the network instead — that would be a surprising
                // amount of egress from a setting that says "local".
                LocalStatus::Off => Err(ChatError::Transport(
                    "the local model server is not running".into(),
                )),
            },
        }
    }

    /// The credential and where it came from, resolving on first use.
    pub fn credential(&self) -> (Option<ApiKey>, KeySource) {
        {
            let c = self.credential.read().unwrap();
            if c.2 {
                return (c.0.clone(), c.1);
            }
        }
        let (k, src) = key::resolve();
        let mut c = self.credential.write().unwrap();
        *c = (k.clone(), src, true);
        (k, src)
    }

    /// Forget the cached credential, so the next request re-reads the
    /// environment and the keychain. Called after the user stores a key.
    pub fn refresh_credential(&self) {
        *self.credential.write().unwrap() = (None, KeySource::None, false);
    }

    /// Seed the credential directly, bypassing the environment and the
    /// keychain. Test seam: the interaction suite needs `effective_provider`
    /// to resolve so it can drive the streaming path, and must never read the
    /// developer's real key to do it.
    #[doc(hidden)]
    pub fn set_credential_for_test(&self, key: Option<ApiKey>) {
        let src = if key.is_some() {
            KeySource::Env
        } else {
            KeySource::None
        };
        *self.credential.write().unwrap() = (key, src, true);
    }

    /// Cancel a lane's in-flight request, if any.
    pub fn cancel(&self, lane: Lane) {
        self.slots[lane.index()].abort();
    }

    /// Cancel everything — app quit, or the last card closing.
    pub fn cancel_all(&self) {
        for l in Lane::ALL {
            self.cancel(l);
        }
    }

    /// Start a request on `lane`, superseding whatever that lane was doing.
    ///
    /// Returns the generation stamped on this request. Every delta the caller
    /// receives should be checked against the generation it last started,
    /// because an aborted task may already have queued deltas in `sink`.
    ///
    /// The work runs as a spawned task so that a superseding `abort()` drops
    /// the in-flight response future and closes the connection — the same
    /// mechanism [`crate::backend::InlineCompletionBackend::infill`] uses.
    pub fn start(&self, lane: Lane, req: ChatRequest, sink: UnboundedSender<ChatDelta>) -> u64 {
        let slot = &self.slots[lane.index()];
        slot.abort();
        let gen = slot.gen.fetch_add(1, Ordering::SeqCst) + 1;

        let model = self.model();
        let client = self.client.clone();

        // Resolve everything the task needs up front: it must not touch `self`,
        // so the backend can be shared without the task borrowing it.
        let plan = self.effective_provider().and_then(|p| match p {
            ChatProviderId::Anthropic => match self.credential().0 {
                Some(key) => Ok(Plan::Anthropic { key, model }),
                // `effective_provider` already proved a key exists; this arm is
                // unreachable in practice, and failing beats an unwrap.
                None => Err(ChatError::NoCredential),
            },
            ChatProviderId::LlamaServer => match self.local_status() {
                LocalStatus::Ready { endpoint, .. } => Ok(Plan::Local { endpoint }),
                _ => Err(ChatError::Transport(
                    "the local model server went away".into(),
                )),
            },
        });

        let plan = match plan {
            Ok(p) => p,
            Err(e) => {
                // Fail on the channel rather than returning an error, so every
                // caller has exactly one path for handling failure.
                let _ = sink.send(ChatDelta::Failed(e));
                return gen;
            }
        };

        let Some(rt) = self.handle() else {
            let _ = sink.send(ChatDelta::Failed(ChatError::Transport(
                "no runtime to run the request on".into(),
            )));
            return gen;
        };
        let handle = rt.spawn(async move {
            run(client, plan, req, sink).await;
        });
        *slot.inflight.lock().unwrap() = Some((gen, handle.abort_handle()));
        gen
    }

    /// Release a lane's slot once its stream has terminated.
    pub fn finished(&self, lane: Lane, gen: u64) {
        self.slots[lane.index()].clear_if(gen);
    }
}

/// Everything a request task needs, resolved before it is spawned.
enum Plan {
    Anthropic { key: ApiKey, model: ChatModel },
    Local { endpoint: String },
}

/// Drive one streaming request to completion, pushing deltas onto `sink`.
///
/// Guarantees exactly one terminal delta. Every early return goes through
/// [`fail`], and the normal path ends with the mapper's own terminal event or
/// a synthesized one — otherwise a card would spin forever on a truncated
/// response.
async fn run(
    client: reqwest::Client,
    plan: Plan,
    req: ChatRequest,
    sink: UnboundedSender<ChatDelta>,
) {
    use futures_util::StreamExt;

    let (url, body, mut builder) = match &plan {
        Plan::Anthropic { key, model } => {
            let mut b = client.post(anthropic::API_URL);
            for (k, v) in anthropic::headers(key.expose()) {
                b = b.header(k, v);
            }
            (
                anthropic::API_URL.to_string(),
                anthropic::build_body(&req, *model),
                b,
            )
        }
        Plan::Local { endpoint } => {
            let url = format!("{}/v1/chat/completions", endpoint.trim_end_matches('/'));
            let b = client.post(&url).header("content-type", "application/json");
            (url, local::build_body(&req), b)
        }
    };
    let _ = url;

    builder = builder.json(&body).timeout(req.timeout);

    let resp = match builder.send().await {
        Ok(r) => r,
        Err(e) => return fail(&sink, transport_error(e)),
    };

    let status = resp.status();
    if !status.is_success() {
        let retry_after = resp
            .headers()
            .get(reqwest::header::RETRY_AFTER)
            .and_then(|v| v.to_str().ok())
            .map(str::to_string);
        let text = resp.text().await.unwrap_or_default();
        return fail(
            &sink,
            anthropic::classify_status(status.as_u16(), retry_after.as_deref(), &text),
        );
    }

    let mut decoder = SseDecoder::new();
    let mut mapper = Mapper::new(&plan);
    let mut stream = resp.bytes_stream();
    // The body is UTF-8, but a chunk boundary can split a multi-byte
    // character, so bytes are held over rather than decoded lossily.
    let mut carry: Vec<u8> = Vec::new();

    while let Some(chunk) = stream.next().await {
        let bytes = match chunk {
            Ok(b) => b,
            Err(e) => return fail(&sink, transport_error(e)),
        };
        carry.extend_from_slice(&bytes);
        let text = match std::str::from_utf8(&carry) {
            Ok(s) => {
                let s = s.to_string();
                carry.clear();
                s
            }
            Err(e) => {
                let good = e.valid_up_to();
                let s = String::from_utf8_lossy(&carry[..good]).into_owned();
                carry.drain(..good);
                s
            }
        };
        for ev in decoder.push(&text) {
            for delta in mapper.map(&ev) {
                let terminal = matches!(delta, ChatDelta::Done { .. } | ChatDelta::Failed(_));
                if sink.send(delta).is_err() {
                    return; // the card is gone; nobody is listening
                }
                if terminal {
                    return;
                }
            }
        }
    }

    // The stream ended without a terminal event.
    if let Some(ev) = decoder.finish() {
        for delta in mapper.map(&ev) {
            let terminal = matches!(delta, ChatDelta::Done { .. } | ChatDelta::Failed(_));
            if sink.send(delta).is_err() {
                return;
            }
            if terminal {
                return;
            }
        }
    }
    let _ = sink.send(mapper.finish());
}

/// One mapper over both providers, so [`run`] has a single code path.
enum Mapper {
    Anthropic(anthropic::EventMapper),
    Local(local::EventMapper),
}

impl Mapper {
    fn new(plan: &Plan) -> Self {
        match plan {
            Plan::Anthropic { .. } => Mapper::Anthropic(anthropic::EventMapper::new()),
            Plan::Local { .. } => Mapper::Local(local::EventMapper::new()),
        }
    }

    fn map(&mut self, ev: &SseEvent) -> Vec<ChatDelta> {
        match self {
            Mapper::Anthropic(m) => m.map(ev),
            Mapper::Local(m) => m.map(ev),
        }
    }

    /// The terminal delta to synthesize when the connection closed early.
    fn finish(&mut self) -> ChatDelta {
        match self {
            // Anthropic always sends `message_stop`; not seeing one means the
            // connection dropped mid-answer, which is a failure, not an end.
            Mapper::Anthropic(_) => ChatDelta::Failed(ChatError::Transport(
                "the connection closed before the response finished".into(),
            )),
            Mapper::Local(m) => m.finish(),
        }
    }
}

fn fail(sink: &UnboundedSender<ChatDelta>, e: ChatError) {
    let _ = sink.send(ChatDelta::Failed(e));
}

fn transport_error(e: reqwest::Error) -> ChatError {
    if e.is_timeout() {
        ChatError::Timeout
    } else {
        ChatError::Transport(e.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lanes_have_independent_slots() {
        let b = ChatBackend::new();
        // Distinct indices is what keeps ⌘⇧M from canceling ⌘⇧E.
        assert_ne!(Lane::Explain.index(), Lane::Visualize.index());
        b.cancel(Lane::Explain); // no panic on an empty slot
        b.cancel_all();
    }

    #[test]
    fn clear_if_only_clears_its_own_generation() {
        let slot = Slot::default();
        let rt = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();
        let h = rt.spawn(async {});
        *slot.inflight.lock().unwrap() = Some((7, h.abort_handle()));
        slot.clear_if(6); // a stale finisher must not strand generation 7
        assert!(slot.inflight.lock().unwrap().is_some());
        slot.clear_if(7);
        assert!(slot.inflight.lock().unwrap().is_none());
    }

    #[test]
    fn missing_credential_fails_on_the_channel_not_by_returning() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let _g = rt.enter();
        let b = ChatBackend::new();
        // Force the "already resolved, absent" state without touching the env.
        *b.credential.write().unwrap() = (None, KeySource::None, true);
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let gen = b.start(
            Lane::Explain,
            ChatRequest {
                system: "s".into(),
                user: "u".into(),
                max_tokens: 16,
                effort: Effort::Low,
                json_schema: None,
                timeout: Duration::from_secs(1),
            },
            tx,
        );
        assert_eq!(gen, 1);
        assert_eq!(
            rx.try_recv().unwrap(),
            ChatDelta::Failed(ChatError::NoCredential)
        );
    }

    #[test]
    fn generations_increase_per_lane() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let _g = rt.enter();
        let b = ChatBackend::new();
        *b.credential.write().unwrap() = (None, KeySource::None, true);
        let req = || ChatRequest {
            system: "s".into(),
            user: "u".into(),
            max_tokens: 16,
            effort: Effort::Low,
            json_schema: None,
            timeout: Duration::from_secs(1),
        };
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        assert_eq!(b.start(Lane::Explain, req(), tx.clone()), 1);
        assert_eq!(b.start(Lane::Explain, req(), tx.clone()), 2);
        // The other lane counts on its own.
        assert_eq!(b.start(Lane::Visualize, req(), tx), 1);
    }

    // ── provider fallback ─────────────────────────────────────────────────

    fn no_key(b: &ChatBackend) {
        *b.credential.write().unwrap() = (None, KeySource::None, true);
    }
    fn with_key(b: &ChatBackend) {
        *b.credential.write().unwrap() =
            (Some(ApiKey::new("sk-ant-test")), KeySource::Env, true);
    }

    /// The whole point: no API key must not mean no answer.
    #[test]
    fn no_key_falls_back_to_a_ready_local_server() {
        let b = ChatBackend::new();
        no_key(&b);
        b.set_local_status(LocalStatus::Ready { endpoint: "http://127.0.0.1:8630".into() });
        assert_eq!(b.effective_provider(), Ok(ChatProviderId::LlamaServer));
    }

    /// A server still coming up is a wait, not a failure — and it is retryable.
    #[test]
    fn no_key_with_a_starting_server_reports_starting() {
        let b = ChatBackend::new();
        no_key(&b);
        b.set_local_status(LocalStatus::Starting);
        assert_eq!(b.effective_provider(), Err(ChatError::LocalStarting));
        assert!(ChatError::LocalStarting.retryable());
    }

    /// Only when there is genuinely nothing is it a setup problem.
    #[test]
    fn no_key_and_no_server_is_the_setup_message() {
        let b = ChatBackend::new();
        no_key(&b);
        b.set_local_status(LocalStatus::Off);
        assert_eq!(b.effective_provider(), Err(ChatError::NoCredential));
        let d = ChatError::NoCredential.detail().unwrap();
        assert!(d.contains("ANTHROPIC_API_KEY"), "{d}");
        assert!(d.contains("local model"), "mentions both options: {d}");
    }

    /// A key wins even when a local server is available — the user picked the
    /// Anthropic tier, and it is the better model.
    #[test]
    fn a_key_is_preferred_over_the_local_server() {
        let b = ChatBackend::new();
        with_key(&b);
        b.set_local_status(LocalStatus::Ready { endpoint: "http://127.0.0.1:8630".into() });
        assert_eq!(b.effective_provider(), Ok(ChatProviderId::Anthropic));
    }

    /// Choosing the local tier explicitly must NOT quietly send code to the
    /// network just because a key happens to be exported.
    #[test]
    fn the_local_tier_never_falls_forward_to_the_network() {
        let b = ChatBackend::new();
        with_key(&b);
        b.set_model(ChatModel::Local);
        b.set_local_status(LocalStatus::Off);
        match b.effective_provider() {
            Err(ChatError::Transport(_)) => {}
            other => panic!("expected a local transport error, got {other:?}"),
        }
    }

    /// The fallback must actually be taken by `start`, not only reported.
    #[test]
    fn start_uses_the_fallback_rather_than_failing() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let _g = rt.enter();
        let b = ChatBackend::new();
        no_key(&b);
        b.set_local_status(LocalStatus::Ready { endpoint: "http://127.0.0.1:1".into() });
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        b.start(
            Lane::Explain,
            ChatRequest {
                system: "s".into(),
                user: "u".into(),
                max_tokens: 16,
                effort: Effort::Low,
                json_schema: None,
                timeout: Duration::from_millis(50),
            },
            tx,
        );
        // A real request is attempted (and fails to connect to port 1) rather
        // than being refused up front with NoCredential.
        assert!(
            !matches!(rx.try_recv(), Ok(ChatDelta::Failed(ChatError::NoCredential))),
            "the fallback was not taken"
        );
    }
}
