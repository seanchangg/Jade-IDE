//! The `InlineCompletionBackend` — lifecycle management (endpoint resolution,
//! managed `llama-server` supervision) and the single-flight `/infill` client.
//! Port of the class in `src/main/inline-completion.ts`.
//!
//! Concurrency model (the Rust analogue of JS's single-threaded event loop):
//! - `lifecycle` (an async mutex) serializes `start`/`stop`/`set_model` and the
//!   managed-startup poll loop, standing in for the fact that the TS methods
//!   never ran concurrently.
//! - `stopped` (an atomic) lets `stop()` signal a cancellation that an
//!   in-progress `start()` health-probe loop observes between probes, exactly as
//!   the TS `this.stopped` flag did (TS :59, :234-235).
//! - `/infill` runs off the lifecycle lock and reads the current endpoint from
//!   the published status, so a slow managed health-probe never blocks typing.

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::io::AsyncReadExt;
use tokio::process::{Child, Command};
use tokio::sync::{watch, Mutex as AsyncMutex};
use tokio::task::AbortHandle;

use crate::{AiModelId, AiState, AiStatus, InfillRequest, InfillResult, ModelSource};

// ── Constants (TS :15-29) ──
const DEFAULT_ENDPOINT: &str = "http://127.0.0.1:8012";
const MANAGED_PORT: u16 = 8630;
const HEALTH_TIMEOUT_MS: u64 = 800;
const STARTUP_DEADLINE_MS: u64 = 15 * 60_000; // generous: first run downloads the model
const REQUEST_TIMEOUT_MS: u64 = 4000;
const POLL_INTERVAL_MS: u64 = 1500;
/// GUI apps on macOS don't inherit the shell PATH, so probe common install dirs
/// (TS :28-29).
const EXTRA_BIN_DIRS: [&str; 3] = ["/opt/homebrew/bin", "/usr/local/bin", "/opt/local/bin"];

fn managed_endpoint() -> String {
    format!("http://127.0.0.1:{MANAGED_PORT}")
}

/// Lifecycle state guarded by the async mutex (the TS instance fields
/// `proc` / `endpoint` / `modelId`, TS :32-38).
struct Lifecycle {
    /// The managed child, `Some` only when *we* spawned it (TS `this.proc`).
    proc: Option<Child>,
    /// Bumped on every managed spawn / stop so a stale poll loop from a prior
    /// run exits instead of clobbering the new server's status.
    startup_gen: u64,
    model_id: AiModelId,
    /// The instruct tier the router's chat preset serves. Lives beside the
    /// completion tier because both are written into one preset file.
    chat_tier: crate::presets::ChatTier,
}

struct Shared {
    status_tx: watch::Sender<AiStatus>,
    lifecycle: AsyncMutex<Lifecycle>,
    stopped: AtomicBool,
    /// PID of the managed server, or 0 for none. It duplicates `lifecycle.proc`
    /// on purpose: quit and signal handlers must kill the server *without*
    /// taking the async lock or awaiting anything. See `kill_managed_now`.
    /// Also set when `start()` adopts an orphan left by a previous Jade, so the
    /// orphan dies with this run.
    managed_pid: AtomicU32,
    /// The abort handle of the in-flight `/infill` task and its generation, so a
    /// superseding call can abort it (the `AbortController` port, TS :36, :122).
    inflight: std::sync::Mutex<Option<(u64, AbortHandle)>>,
    inflight_gen: AtomicU64,
    client: reqwest::Client,
    /// Per-request `/infill` timeout (TS `REQUEST_TIMEOUT_MS`, :26). Field rather
    /// than constant only so tests can shorten it without a multi-second wait.
    request_timeout: Duration,
}

/// AI inline-completion backend. Cheap to clone is *not* implied — hold one and
/// share `&self`; internal state is `Arc`-wrapped for the poll/infill tasks.
pub struct InlineCompletionBackend {
    shared: Arc<Shared>,
}

impl InlineCompletionBackend {
    /// Construct in the `disabled` / "Not started" state (TS :34).
    pub fn new() -> Self {
        Self::with_request_timeout(Duration::from_millis(REQUEST_TIMEOUT_MS))
    }

    /// Like [`new`](Self::new) but with a custom `/infill` timeout. Exposed for
    /// tests that need to exercise the timeout path without a multi-second wait;
    /// production code should use [`new`](Self::new).
    #[doc(hidden)]
    pub fn with_request_timeout(request_timeout: Duration) -> Self {
        let (status_tx, _rx) = watch::channel(AiStatus::new(AiState::Disabled, "Not started"));
        let client = reqwest::Client::builder()
            .build()
            .expect("reqwest client builds with rustls");
        InlineCompletionBackend {
            shared: Arc::new(Shared {
                status_tx,
                lifecycle: AsyncMutex::new(Lifecycle {
                    proc: None,
                    startup_gen: 0,
                    model_id: AiModelId::default(),
                    chat_tier: crate::presets::ChatTier::default(),
                }),
                stopped: AtomicBool::new(false),
                managed_pid: AtomicU32::new(0),
                inflight: std::sync::Mutex::new(None),
                inflight_gen: AtomicU64::new(0),
                client,
                request_timeout,
            }),
        }
    }

    /// Subscribe to status changes (analogue of `onStatus`, TS :113-115). The
    /// receiver's initial value is the current status.
    pub fn subscribe(&self) -> watch::Receiver<AiStatus> {
        self.shared.status_tx.subscribe()
    }

    /// The current status (TS `getStatus`, :109-111).
    pub fn status(&self) -> AiStatus {
        self.shared.status_tx.borrow().clone()
    }

    /// Resolve an endpoint or spawn the managed server. Idempotent — a second
    /// call while running or mid-startup is a no-op (TS :47-77).
    pub async fn start(&self) {
        let shared = &self.shared;
        let mut life = shared.lifecycle.lock().await;

        // Idempotent guard (TS :49).
        let state = shared.status_tx.borrow().state;
        if life.proc.is_some() || state == AiState::Ready || state == AiState::Starting {
            return;
        }
        shared.stopped.store(false, Ordering::SeqCst);

        shared.set_status(AiStatus::new(
            AiState::Starting,
            "Looking for a completion server…",
        ));

        // Candidates, in order (TS :52-53). Env var wins; empty/unset filtered.
        let mut candidates: Vec<String> = Vec::new();
        if let Ok(ep) = std::env::var("JADE_FIM_ENDPOINT") {
            if !ep.is_empty() {
                candidates.push(ep);
            }
        }
        candidates.push(DEFAULT_ENDPOINT.to_string());
        candidates.push(managed_endpoint());

        let managed = managed_endpoint();
        for ep in candidates {
            let healthy = shared.is_healthy(&ep).await;
            if shared.stopped.load(Ordering::SeqCst) {
                return; // turned off while we were probing (TS :59)
            }
            if healthy {
                // A healthy server on our own managed port, with a live PID in
                // our PID file, is an orphan from a previous Jade that did not
                // exit cleanly. Adopt it rather than kill it — the weights are
                // already loaded — but take ownership of the PID so quitting
                // this run takes it down. Servers on any other endpoint belong
                // to somebody else and are never killed.
                if ep == managed {
                    if let Some(pid) = read_pid_file().filter(|pid| pid_is_alive(*pid)) {
                        shared.managed_pid.store(pid, Ordering::SeqCst);
                    }
                }
                shared.set_status(AiStatus::ready(format!("Connected to {ep}"), ep));
                return;
            }
        }
        // No managed server answered, so any PID we recorded is dead.
        clear_pid_file();

        // Nothing running — spawn our own (TS :67-76).
        let Some(bin) = find_llama_server() else {
            shared.set_status(AiStatus::new(
                AiState::Disabled,
                "llama-server not found — `brew install llama.cpp`, or set \
                 JADE_FIM_ENDPOINT to a running server",
            ));
            return;
        };

        shared.spawn_managed(&mut life, &bin);
    }

    /// Kill the managed server (freeing GPU/unified memory) and abort any
    /// in-flight request. `start()` brings it back (TS :79-91).
    pub async fn stop(&self) {
        self.stop_with(String::from("Turned off")).await;
    }

    async fn stop_with(&self, detail: String) {
        let shared = &self.shared;
        // Set first, without the lock, so an in-progress start()'s probe loop
        // sees it immediately (TS sets `this.stopped = true` synchronously).
        shared.stopped.store(true, Ordering::SeqCst);
        shared.abort_inflight();

        let mut life = shared.lifecycle.lock().await;
        if let Some(mut child) = life.proc.take() {
            let _ = child.start_kill(); // SIGKILL; releases GPU memory (TS :85-87)
        }
        // Kill by PID too: `start_kill` only covers a child we still hold, and
        // an adopted orphan has no `Child` at all.
        kill_pid(shared.managed_pid.swap(0, Ordering::SeqCst));
        clear_pid_file();
        life.startup_gen = life.startup_gen.wrapping_add(1); // retire the poll loop
        shared.set_status(AiStatus::new(AiState::Disabled, detail));
    }

    /// `shutdown()` (TS :157-159) — stop with the shutdown detail.
    pub async fn shutdown(&self) {
        self.stop_with(String::from("Shutting down")).await;
    }

    /// Kill the managed server **now**, synchronously, and report whether a
    /// signal went out.
    ///
    /// Quit paths cannot use [`shutdown`](Self::shutdown): it is async and takes
    /// the lifecycle lock, but GPUI ends the process from `on_app_quit` and a
    /// signal handler has no runtime to await on. `kill_on_drop` does not save
    /// us either — the process exits without dropping the tokio `Child`. That is
    /// why llama-server kept running (and kept the GPU busy) after Jade closed.
    ///
    /// Only ever kills a server this backend spawned or adopted on the managed
    /// port. An external endpoint (`JADE_FIM_ENDPOINT`, or a server on
    /// [`DEFAULT_ENDPOINT`]) is left alone.
    pub fn kill_managed_now(&self) -> bool {
        let shared = &self.shared;
        shared.stopped.store(true, Ordering::SeqCst);
        let killed = kill_pid(shared.managed_pid.swap(0, Ordering::SeqCst));
        clear_pid_file();
        killed
    }

    /// Spawn the managed server directly, skipping endpoint resolution.
    ///
    /// Test seam only. [`start`](Self::start) adopts whatever already answers on
    /// 8012 or the managed port, so on a machine that is running Jade (or a
    /// llama-server for an eval) a spawn-then-kill test would never spawn.
    #[doc(hidden)]
    pub async fn spawn_managed_for_test(&self, bin: &str) {
        let mut life = self.shared.lifecycle.lock().await;
        self.shared.stopped.store(false, Ordering::SeqCst);
        self.shared.spawn_managed(&mut life, bin);
    }

    /// PID of the managed server, or `None`. Test seam.
    #[doc(hidden)]
    pub fn managed_pid(&self) -> Option<u32> {
        match self.shared.managed_pid.load(Ordering::SeqCst) {
            0 => None,
            pid => Some(pid),
        }
    }

    /// Switch the managed model tier, restarting the server only if it's a child
    /// we spawned; adopted/external endpoints pick their own model (TS :96-107).
    pub async fn set_model(&self, id: AiModelId) {
        let shared = &self.shared;
        let restart = {
            let mut life = shared.lifecycle.lock().await;
            if id == life.model_id {
                return;
            }
            life.model_id = id;

            let using_managed = life.proc.is_some();
            let state = shared.status_tx.borrow().state;
            let running = state == AiState::Ready || state == AiState::Starting;
            if !running {
                return; // off/errored — applies on next start (TS :102)
            }
            if !using_managed && state == AiState::Ready {
                return; // external endpoint manages its own model (TS :103)
            }
            true
        };

        if restart {
            self.stop_with(String::from("Switching model…")).await;
            self.start().await;
        }
    }

    /// POST `/infill`. Single-flight: a new call aborts the in-flight one so the
    /// server's single slot isn't stuck on a stale suggestion (TS :117-155).
    /// Returns `None` on any error, non-2xx, timeout, or abort.
    pub async fn infill(&self, req: &InfillRequest) -> Option<InfillResult> {
        let shared = &self.shared;

        // Only serve when a resolved endpoint is ready (TS :118).
        let status = shared.status_tx.borrow().clone();
        if status.state != AiState::Ready {
            return None;
        }
        let endpoint = status.endpoint?;

        // Cancel whatever is still running (TS :122-124).
        shared.abort_inflight();

        let gen = shared.inflight_gen.fetch_add(1, Ordering::SeqCst) + 1;
        let body = infill_body(req);
        let url = format!("{endpoint}/infill");
        let client = shared.client.clone();
        let request_timeout = shared.request_timeout;

        // Run the request as a task so a superseding call's `abort()` cancels the
        // reqwest future (dropping it), mirroring `controller.abort()`. The 4s
        // timeout stands in for the TS `setTimeout(() => controller.abort())`.
        let handle = tokio::spawn(async move {
            let resp = client
                .post(&url)
                .json(&body)
                .timeout(request_timeout)
                .send()
                .await
                .ok()?;
            if !resp.status().is_success() {
                return None; // `if (!res.ok) return null` (TS :145)
            }
            let json: serde_json::Value = resp.json().await.ok()?;
            // `typeof json.content === 'string' ? {content} : null` (TS :147)
            json.get("content")
                .and_then(|c| c.as_str())
                .map(|s| InfillResult {
                    content: s.to_string(),
                })
        });

        *shared.inflight.lock().unwrap() = Some((gen, handle.abort_handle()));

        // JoinError => aborted/superseded/panicked => no suggestion (TS :148-150).
        let out = handle.await.unwrap_or(None);

        // `if (this.inflight === controller) this.inflight = null` (TS :153).
        let mut guard = shared.inflight.lock().unwrap();
        if guard.as_ref().map(|(g, _)| *g) == Some(gen) {
            *guard = None;
        }
        out
    }
}

impl Default for InlineCompletionBackend {
    fn default() -> Self {
        Self::new()
    }
}

impl Shared {
    fn set_status(&self, status: AiStatus) {
        // `send_replace` keeps the latest value even with no active receivers,
        // matching the TS field assignment `this.status = s` (TS :162).
        let _ = self.status_tx.send_replace(status);
    }

    fn abort_inflight(&self) {
        if let Some((_, handle)) = self.inflight.lock().unwrap().take() {
            handle.abort();
        }
    }

    /// GET `/health` with an 800ms timeout (TS :166-175).
    async fn is_healthy(&self, endpoint: &str) -> bool {
        match self
            .client
            .get(format!("{endpoint}/health"))
            .timeout(Duration::from_millis(HEALTH_TIMEOUT_MS))
            .send()
            .await
        {
            Ok(resp) => resp.status().is_success(),
            Err(_) => false,
        }
    }

    /// Spawn the managed `llama-server` and start the readiness poll (TS :193-249).
    fn spawn_managed(self: &Arc<Self>, life: &mut Lifecycle, bin: &str) {
        let endpoint = managed_endpoint();
        let model = life.model_id;

        // A local GGUF that is not there can never become ready, and the poll
        // loop would just time out. Say so now, with the path we looked for.
        let (_src_flag, _src_value) = match model.source() {
            ModelSource::Hf(repo) => ("-hf", repo.to_string()),
            ModelSource::Local(path) => {
                if !path.exists() {
                    self.set_status(AiStatus::new(
                        AiState::Error,
                        format!(
                            "{} needs a GGUF at {}. Build one with ml/train/export_gguf.py, \
                             or set JADE_SPRITE_MODEL to point elsewhere.",
                            model.label(),
                            path.display()
                        ),
                    ));
                    return;
                }
                ("-m", path.to_string_lossy().into_owned())
            }
        };

        self.set_status(AiStatus::new(
            AiState::Starting,
            format!(
                "Starting {} + {} (downloads on first use)…",
                model.label(),
                life.chat_tier.label()
            ),
        ));

        // Router mode: ONE server, two presets, each loaded on demand.
        //
        // The completion model cannot also serve chat — asked to follow an
        // instruction it emits ChatML control tokens on a loop — so Jade needs
        // a second, instruct model. Rather than supervise a second process, the
        // server is started in llama.cpp's router mode over a generated preset
        // file; `/infill` and `/v1/chat/completions` then select between them
        // with the request's `model` field. An idle Jade holds no weights,
        // because the router only loads a preset when one is asked for.
        //
        // The per-model flags that used to live here (`-ngl`, `-b`/`-ub`,
        // `--ctx-size`, `--cache-reuse`) moved into the preset file, which is
        // where the router expects them (`crate::presets::render`).
        let preset_path = crate::presets::default_path();
        if let Err(e) = crate::presets::write(&preset_path, model, life.chat_tier) {
            self.set_status(AiStatus::new(
                AiState::Error,
                format!("Could not write the model preset file: {e}"),
            ));
            return;
        }
        let mut cmd = Command::new(bin);
        cmd.args([
            "--models-preset",
            &preset_path.to_string_lossy(),
            "--models-max",
            crate::presets::MODELS_MAX,
            "--host",
            "127.0.0.1",
            "--port",
            &MANAGED_PORT.to_string(),
        ])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true);

        let mut child = match cmd.spawn() {
            Ok(c) => c,
            Err(err) => {
                // `this.proc.on('error')` (TS :217-220).
                self.set_status(AiStatus::new(
                    AiState::Error,
                    format!("Failed to launch llama-server: {err}"),
                ));
                return;
            }
        };

        // stderr tail capture — last 2000 chars for error reporting (TS :212-215).
        let stderr_tail = Arc::new(std::sync::Mutex::new(String::new()));
        if let Some(mut stderr) = child.stderr.take() {
            let tail = stderr_tail.clone();
            tokio::spawn(async move {
                let mut buf = [0u8; 4096];
                loop {
                    match stderr.read(&mut buf).await {
                        Ok(0) | Err(_) => break,
                        Ok(n) => {
                            let mut t = tail.lock().unwrap();
                            t.push_str(&String::from_utf8_lossy(&buf[..n]));
                            if t.len() > 2000 {
                                let cut = t.len() - 2000;
                                *t = t[cut..].to_string();
                            }
                        }
                    }
                }
            });
        }

        life.startup_gen = life.startup_gen.wrapping_add(1);
        let gen = life.startup_gen;
        // Record the PID before anything can await, so a quit that lands in the
        // next microsecond still finds a server to kill. The file survives a
        // crash, which is how the *next* run finds this server (see `start`).
        let pid = child.id().unwrap_or(0);
        self.managed_pid.store(pid, Ordering::SeqCst);
        write_pid_file(pid);
        life.proc = Some(child);

        // Poll until the model is loaded (server returns 503 while loading),
        // first tick after 1500ms (TS :233-248).
        let shared = self.clone();
        let deadline = Instant::now() + Duration::from_millis(STARTUP_DEADLINE_MS);
        tokio::spawn(async move {
            shared.poll_ready(gen, endpoint, model, deadline, stderr_tail).await;
        });
    }

    async fn poll_ready(
        self: Arc<Self>,
        gen: u64,
        endpoint: String,
        model: AiModelId,
        deadline: Instant,
        stderr_tail: Arc<std::sync::Mutex<String>>,
    ) {
        loop {
            tokio::time::sleep(Duration::from_millis(POLL_INTERVAL_MS)).await;
            if self.stopped.load(Ordering::SeqCst) {
                return;
            }

            let mut life = self.lifecycle.lock().await;
            if life.startup_gen != gen {
                return; // superseded by a restart/stop
            }
            if life.proc.is_none() {
                return;
            }

            // Detect an early exit before health comes up (TS :222-230).
            if let Some(child) = life.proc.as_mut() {
                if let Ok(Some(exit)) = child.try_wait() {
                    life.proc = None;
                    self.managed_pid.store(0, Ordering::SeqCst); // it is gone
                    clear_pid_file();
                    let ready = self.status_tx.borrow().state == AiState::Ready;
                    if !self.stopped.load(Ordering::SeqCst) && !ready {
                        let tail = stderr_tail.lock().unwrap().clone();
                        let last3 = tail
                            .split('\n')
                            .filter(|l| !l.is_empty())
                            .collect::<Vec<_>>();
                        let last3 = last3[last3.len().saturating_sub(3)..].join(" ");
                        let code = exit.code().map(|c| c.to_string()).unwrap_or_else(|| "signal".into());
                        self.set_status(AiStatus::new(
                            AiState::Error,
                            format!("llama-server exited with code {code}: {last3}"),
                        ));
                    }
                    return;
                }
            }

            if self.is_healthy(&endpoint).await {
                self.set_status(AiStatus::ready(
                    format!("Local model ready ({})", model.label()),
                    endpoint.clone(),
                ));
                return;
            }

            if Instant::now() > deadline {
                // `setStatus(error)` then `this.shutdown()` (TS :242-243).
                self.set_status(AiStatus::new(
                    AiState::Error,
                    "Timed out waiting for the completion model to load",
                ));
                self.stopped.store(true, Ordering::SeqCst);
                if let Some(mut child) = life.proc.take() {
                    let _ = child.start_kill();
                }
                kill_pid(self.managed_pid.swap(0, Ordering::SeqCst));
                clear_pid_file();
                life.startup_gen = life.startup_gen.wrapping_add(1);
                self.set_status(AiStatus::new(AiState::Disabled, "Shutting down"));
                return;
            }
        }
    }
}

/// Build the `/infill` request body exactly as the TS did (TS :132-142).
fn infill_body(req: &InfillRequest) -> serde_json::Value {
    serde_json::json!({
        // Router mode serves two presets; name the one that can do this.
        "model": crate::presets::FIM_MODEL,
        "input_prefix": req.prefix,
        "input_suffix": req.suffix,
        "n_predict": if req.single_line { 64 } else { 96 },
        // Multiline stops at a blank line: the client cuts there anyway
        // (`ghost::post_process`), so generating past it only burns latency —
        // benchmarked, the 96-token cap was routinely hit generating text that
        // was then thrown away (JetBrains FLCC likewise prefers
        // newline-terminated hypotheses over longer ones, arXiv 2405.08704).
        "stop": if req.single_line { vec!["\n"] } else { vec!["\n\n"] },
        "top_k": 40,
        "top_p": 0.99,
        "temperature": 0.1,
        "cache_prompt": true,
        "t_max_predict_ms": 1500,
    })
}

/// Locate the `llama-server` binary (TS :177-191):
/// `JADE_LLAMA_SERVER` (must exist) → each PATH dir → the extra install dirs,
/// taking the first that exists and is executable (X_OK).
fn find_llama_server() -> Option<String> {
    if let Ok(explicit) = std::env::var("JADE_LLAMA_SERVER") {
        if !explicit.is_empty() && Path::new(&explicit).exists() {
            return Some(explicit);
        }
    }

    let path_dirs = std::env::var("PATH").unwrap_or_default();
    let dirs = path_dirs
        .split(':')
        .map(|s| s.to_string())
        .chain(EXTRA_BIN_DIRS.iter().map(|s| s.to_string()));

    for dir in dirs {
        if dir.is_empty() {
            continue;
        }
        let candidate = Path::new(&dir).join("llama-server");
        if is_executable(&candidate) {
            return candidate.to_str().map(|s| s.to_string());
        }
    }
    None
}

/// SIGKILL `pid`, the same signal `stop()` has always sent. A PID of 0 means
/// "no managed server" and is ignored. Returns whether the signal went out.
fn kill_pid(pid: u32) -> bool {
    if pid == 0 {
        return false;
    }
    // SAFETY: `kill` with a positive PID and a valid signal number. The worst
    // case is ESRCH (the process is already gone), which we report as `false`.
    unsafe { libc::kill(pid as libc::pid_t, libc::SIGKILL) == 0 }
}

/// Is `pid` still a live process? `signal 0` runs the permission and existence
/// checks without delivering anything.
fn pid_is_alive(pid: u32) -> bool {
    if pid == 0 {
        return false;
    }
    // SAFETY: as `kill_pid`; signal 0 delivers nothing.
    unsafe { libc::kill(pid as libc::pid_t, 0) == 0 }
}

/// Where the managed server's PID is recorded, so the next run can find a
/// server this one left behind. `JADE_LLAMA_PIDFILE` overrides it, which keeps
/// tests off the real `~/.config/jade`.
fn pid_file() -> Option<PathBuf> {
    if let Ok(explicit) = std::env::var("JADE_LLAMA_PIDFILE") {
        if !explicit.is_empty() {
            return Some(PathBuf::from(explicit));
        }
    }
    let home = std::env::var_os("HOME")?;
    Some(Path::new(&home).join(".config/jade/llama-server.pid"))
}

fn write_pid_file(pid: u32) {
    let Some(path) = pid_file() else { return };
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let _ = std::fs::write(path, pid.to_string());
}

fn read_pid_file() -> Option<u32> {
    let text = std::fs::read_to_string(pid_file()?).ok()?;
    text.trim().parse().ok()
}

fn clear_pid_file() {
    if let Some(path) = pid_file() {
        let _ = std::fs::remove_file(path);
    }
}

/// `fs.accessSync(candidate, X_OK)` equivalent (TS :185-188).
fn is_executable(path: &Path) -> bool {
    match std::fs::metadata(path) {
        Ok(meta) => meta.is_file() && (meta.permissions().mode() & 0o111 != 0),
        Err(_) => false,
    }
}
