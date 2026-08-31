//! `jade-build` — the headless build/run/asm engine, a Rust port of
//! `src/main/build-runner.ts` (924 lines). No GUI dependencies: results are
//! returned as values and progress is streamed over tokio mpsc channels.
//!
//! Pipeline (see `docs/jade-feature-inventory.md` §8.1):
//! - [`BuildEngine::compile`] — CMake root discovery / `CMakeLists.txt`
//!   auto-generation, cached configure, build, File-API executable discovery.
//! - [`BuildEngine::run`] — telemetry-socket + dylib injection, stdout/stderr
//!   `__JADE_*` protocol handling (§8.2), ASan/gcov parsing, batched memory
//!   events (perf-findings.md #1).
//! - [`BuildEngine::generate_asm`] — `clang++ -S -masm=intel` with `.loc`
//!   source mapping and `c++filt` demangling.
//!
//! Buffer-alias symbolication (`atos`) is provided to `jade-telemetry` via the
//! [`jade_telemetry::Symbolicator`] hook; see [`AtosSymbolicator`] and the
//! `symbolicate` module for the seam.

mod asm;
mod cmake;
mod compile;
mod dylib;
// Visualize (§4.15): the private Manim venv and the sandboxed render.
pub mod manim;
mod parse;
mod run;
mod symbolicate;
mod types;
mod util;
pub mod venv;

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use tokio::sync::oneshot;

pub use compile::OutputSink;
pub use dylib::{INTERPOSE_DYLIB, PROBE_DYLIB};
// Instrumentation-line parsers, re-exported so the Debug (LLDB) path can
// apply the same `__JADE_*` handling to console output that `run` applies
// to a directly-spawned child's stdio.
pub use parse::{parse_alloc_free, parse_heap_summary, parse_scalar, parse_timing};
pub use run::RunHandle;
pub use symbolicate::AtosSymbolicator;
pub use types::*;

// Re-export the telemetry symbolication trait so callers can name the seam.
pub use jade_telemetry::{Symbolicator, TelemetryServer};

/// Static configuration for a [`BuildEngine`]: native-source locations and an
/// optional telemetry server the engine wires runs into.
#[derive(Clone)]
pub struct EngineConfig {
    /// `include/` dir with `idetools.h`, passed as `JADE_INCLUDE_DIR` / `-I`.
    pub jade_include_dir: PathBuf,
    /// `include/jade_interpose.c` (malloc interposer source).
    pub interpose_c: PathBuf,
    /// `probe/jade_probe.mm` (Metal telemetry probe source).
    pub probe_mm: PathBuf,
    /// Telemetry server whose socket is advertised to runs and whose buffer
    /// aliasing this engine drives via [`AtosSymbolicator`]. `None` in headless
    /// tests that don't need telemetry.
    pub telemetry: Option<Arc<TelemetryServer>>,
}

impl EngineConfig {
    /// Build a config from the three native-source roots, no telemetry.
    pub fn new(jade_include_dir: PathBuf, interpose_c: PathBuf, probe_mm: PathBuf) -> Self {
        EngineConfig {
            jade_include_dir,
            interpose_c,
            probe_mm,
            telemetry: None,
        }
    }

    /// Derive a config from a repository root: `<root>/include`,
    /// `<root>/include/jade_interpose.c`, `<root>/probe/jade_probe.mm`.
    pub fn from_repo_root(root: &Path) -> Self {
        EngineConfig::new(
            root.join("include"),
            root.join("include/jade_interpose.c"),
            root.join("probe/jade_probe.mm"),
        )
    }

    /// Attach a telemetry server (fluent).
    pub fn with_telemetry(mut self, telemetry: Arc<TelemetryServer>) -> Self {
        self.telemetry = Some(telemetry);
        self
    }
}

/// The build engine. Holds the per-`buildDir` configure cache and the
/// single-concurrent-run guard; cheap to keep around for a session.
pub struct BuildEngine {
    config: EngineConfig,
    /// `buildDir → \x1f-joined configure-args key` (build-runner.ts:15).
    configured_builds: Mutex<HashMap<PathBuf, String>>,
    /// Signal channel to the currently-running program's loop (for kill-previous
    /// / [`BuildEngine::stop`]).
    run_stop: Mutex<Option<oneshot::Sender<()>>>,
}

impl BuildEngine {
    pub fn new(config: EngineConfig) -> Self {
        BuildEngine {
            config,
            configured_builds: Mutex::new(HashMap::new()),
            run_stop: Mutex::new(None),
        }
    }

    /// Configure (cached) + build the project owning `req.file`, streaming cmake
    /// output and engine notices to `out`. Port of build-runner.ts `compile()`.
    pub async fn compile(&self, req: &CompileRequest, out: &OutputSink) -> BuildResult {
        compile::compile(self, req, out).await
    }

    /// Launch `cfg.executable`, killing any previous run. Returns a
    /// [`RunHandle`] with the event stream and the eventual [`RunResult`].
    /// Port of build-runner.ts `run()`.
    pub fn run(&self, cfg: RunConfig) -> RunHandle {
        run::spawn_run(self, cfg)
    }

    /// Kill the currently-running program, if any (build-runner.ts `BUILD_STOP`).
    pub fn stop(&self) {
        if let Some(tx) = self.run_stop.lock().unwrap().take() {
            let _ = tx.send(());
        }
    }

    /// Emit filtered, source-mapped, demangled assembly for `file`. Port of
    /// build-runner.ts `generateAssembly()`.
    pub async fn generate_asm(&self, file: &Path, extra_flags: &[String]) -> AsmResult {
        asm::generate_asm(file, &self.config.jade_include_dir, extra_flags).await
    }

    /// Compile-on-demand the Metal probe dylib; `false` = feature unavailable.
    pub fn ensure_probe_dylib(&self) -> bool {
        dylib::ensure_probe_dylib(&self.config.probe_mm)
    }

    /// Compile-on-demand the malloc interposer dylib; `false` = feature unavailable.
    pub fn ensure_interpose_dylib(&self) -> bool {
        dylib::ensure_interpose_dylib(&self.config.interpose_c)
    }

    /// The configured telemetry server, if any.
    pub fn telemetry(&self) -> Option<&Arc<TelemetryServer>> {
        self.config.telemetry.as_ref()
    }
}
