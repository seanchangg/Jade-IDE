//! Manim render supervision for Visualize (inventory §4.15, steps 1–2).
//!
//! This module EXECUTES MODEL-AUTHORED PYTHON, derived from source code that is
//! by definition attacker-influenced — a comment in a downloaded repo can steer
//! the model. The defenses, in order:
//!
//!   1. [`validate_scene_script`] — a syntactic gate before spawn. A SPEED
//!      BUMP, not a boundary: it rejects the obvious escapes cheaply and
//!      produces a good error message, nothing more.
//!   2. `sandbox-exec` — the actual boundary. The child may read the Python
//!      and Manim installs, read-write only inside the per-request work
//!      directory, and has NO network. Deprecated on macOS but functional,
//!      and the only sandbox available without shipping a container.
//!   3. A wall-clock cap ([`RENDER_TIMEOUT`]), killed by pid.
//!   4. The work directory is removed by a `Drop` impl.
//!
//! Process supervision is modeled on [`crate::compile`]'s `run_cmake`:
//! `tokio::process::Command`, both pipes captured, line streaming into an
//! `UnboundedSender`, a `oneshot` stop channel. Teardown is all three of
//! `kill_on_drop(true)`, the explicit stop channel, and a pid parked in an
//! `AtomicU32` for `cx.on_app_quit` — GPUI ends the process without dropping
//! tokio `Child`ren (see jade/src/main.rs), which is how llama-server used to
//! survive the app.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;
use std::time::Duration;

use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::sync::mpsc::UnboundedSender;
use tokio::sync::oneshot;

use crate::venv;

/// Wall-clock cap on one render. Manim on a 6–12s scene at `-qm` finishes in
/// well under a minute; three minutes means a runaway script.
pub const RENDER_TIMEOUT: Duration = Duration::from_secs(180);

/// The scene class name the prompt pins and the render command selects.
pub const SCENE_CLASS: &str = "JadeScene";

/// The quality flag, part of the cache key. `-qm` is 1280×720 at 30fps —
/// deliberate over Manim's 1080p60 default: it halves the repaint rate, halves
/// decode bandwidth, and renders 3-5× faster.
pub const QUALITY: &str = "-qm";

/// How many cached renders to keep per workspace.
pub const CACHE_KEEP: usize = 20;

/// Progress from one render, streamed as it happens.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RenderEvent {
    /// One log line from manim (or from the venv setup).
    Log(String),
    /// The render finished; the mp4 is at this path (inside the cache).
    Done { video: PathBuf },
    Failed { reason: String },
}

pub type RenderSink = UnboundedSender<RenderEvent>;

/// A render in flight: the stop channel and the child's pid for app-quit
/// teardown. Dropping the handle does NOT stop the render (the task owns the
/// child; `kill_on_drop` fires when the task is dropped with it).
pub struct RenderHandle {
    stop: Option<oneshot::Sender<()>>,
    /// The sandboxed child's pid, 0 when none is running. Killed from
    /// `cx.on_app_quit`, which runs after tokio is already being torn down.
    pub pid: Arc<AtomicU32>,
    pub join: tokio::task::JoinHandle<()>,
}

impl RenderHandle {
    /// Ask the render to stop. Idempotent.
    pub fn stop(&mut self) {
        if let Some(tx) = self.stop.take() {
            let _ = tx.send(());
        }
        kill_pid(&self.pid);
    }
}

/// SIGKILL whatever pid is parked in `slot`, if any. Also usable from a quit
/// hook with no runtime.
pub fn kill_pid(slot: &AtomicU32) {
    let pid = slot.swap(0, Ordering::SeqCst);
    if pid != 0 {
        // SAFETY: kill with a positive pid; worst case ESRCH (already gone).
        unsafe {
            libc::kill(pid as libc::pid_t, libc::SIGKILL);
        }
    }
}

/// The pid of THE active render — Visualize is single-flight per app.
/// Mirrors the per-handle slot so app quit, which has no runtime and never
/// drops the tokio `Child`, can still kill the sandboxed process (the same
/// GPUI-quit hole `main.rs` documents for llama-server).
static ACTIVE_PID: AtomicU32 = AtomicU32::new(0);

/// SIGKILL the active render, if one is running. Synchronous; safe from a
/// quit hook or a signal handler task.
pub fn kill_active_render() {
    kill_pid(&ACTIVE_PID);
}

// ── The syntactic gate ───────────────────────────────────────────────────────

/// Substrings that reject a script outright. Matched on comment-stripped
/// lines. This list is a SPEED BUMP — the sandbox is the boundary; this exists
/// to fail fast with a readable reason and to keep obviously-hostile scripts
/// from ever spawning a process.
const FORBIDDEN: &[&str] = &[
    "import os",
    "import sys",
    "import subprocess",
    "import socket",
    "import shutil",
    "import pathlib",
    "__import__",
    "open(",
    "eval(",
    "exec(",
    "compile(",
];

/// Check that `script` is shaped like the one thing we run: exactly one
/// `class JadeScene(Scene)`, importing only `from manim import *`.
///
/// This is a line-shape check, not a Python parser. It is deliberately
/// conservative: anything it does not recognize is rejected. The sandbox
/// remains the security boundary; see the module docs.
pub fn validate_scene_script(script: &str) -> Result<(), String> {
    let mut scene_classes = 0usize;
    let mut saw_manim_import = false;

    for (i, raw) in script.lines().enumerate() {
        let line = strip_comment(raw);
        let t = line.trim();
        if t.is_empty() {
            continue;
        }
        let lower = t.to_ascii_lowercase();
        for bad in FORBIDDEN {
            if lower.contains(bad) {
                return Err(format!("line {}: `{}` is not allowed", i + 1, bad.trim_end_matches('(')));
            }
        }
        // `import x` / `from x import y` anywhere, at any indent.
        if t.starts_with("import ") || (t.starts_with("from ") && t.contains(" import ")) {
            if t == "from manim import *" {
                saw_manim_import = true;
                continue;
            }
            return Err(format!(
                "line {}: only `from manim import *` may be imported",
                i + 1
            ));
        }
        // Top-level statements: only the import, the scene class, and inside-
        // class bodies (indented lines) are allowed.
        let indented = raw.starts_with(' ') || raw.starts_with('\t');
        if !indented {
            if t.starts_with("class ") {
                if t.replace(' ', "").starts_with("classJadeScene(Scene)") {
                    scene_classes += 1;
                    continue;
                }
                return Err(format!(
                    "line {}: the only class may be `class JadeScene(Scene)`",
                    i + 1
                ));
            }
            return Err(format!(
                "line {}: unexpected top-level statement `{}`",
                i + 1,
                t.chars().take(40).collect::<String>()
            ));
        }
    }

    if !saw_manim_import {
        return Err("the script must begin with `from manim import *`".into());
    }
    match scene_classes {
        1 => Ok(()),
        0 => Err("the script defines no `class JadeScene(Scene)`".into()),
        n => Err(format!("the script defines {n} scene classes; exactly one is allowed")),
    }
}

/// Drop a `#` comment, respecting string quotes well enough for a gate that
/// errs on the side of rejection (an unterminated quote keeps the whole line).
fn strip_comment(line: &str) -> String {
    let mut out = String::with_capacity(line.len());
    let mut quote: Option<char> = None;
    let mut escaped = false;
    for c in line.chars() {
        if let Some(q) = quote {
            out.push(c);
            if escaped {
                escaped = false;
            } else if c == '\\' {
                escaped = true;
            } else if c == q {
                quote = None;
            }
            continue;
        }
        match c {
            '"' | '\'' => {
                quote = Some(c);
                out.push(c);
            }
            '#' => break,
            _ => out.push(c),
        }
    }
    out
}

// ── The sandbox profile ──────────────────────────────────────────────────────

/// The `sandbox-exec` profile: the boundary the gate above is not.
///
/// Three denies over an allow-default base, in SBPL's later-rule-wins order:
///
///   - **no network**, so nothing read can leave;
///   - **no writes** outside the per-request work directory (plus the system
///     temp spool and `/dev`, which Python and ffmpeg need);
///   - **no reads under `/Users`** — that is where `~/.ssh` lives — with the
///     venv and the work directory allowed back explicitly. The work
///     directory sits inside the workspace, which is usually under home.
///
/// Allow-default rather than deny-default is deliberate: a deny-default
/// profile aborts CPython inside dyld before a line of output (verified on
/// this machine), and enumerating the OS surface Python + ffmpeg + latex need
/// is exactly the brittleness the deprecation warnings are about. The three
/// denies above are the properties the threat model needs; each is probed by
/// a test in `tests/manim_sandbox.rs`.
pub fn sandbox_profile(venv: &Path, work: &Path) -> String {
    format!(
        r#"(version 1)
(allow default)
(deny network*)
(deny file-write*)
(allow file-write*
  (subpath "{work}")
  (subpath "/private/var/folders")
  (subpath "/dev"))
(deny file-read* (subpath "/Users"))
(allow file-read*
  (subpath "{venv}")
  (subpath "{work}"))
"#,
        venv = venv.display(),
        work = work.display(),
    )
}

// ── Cache ────────────────────────────────────────────────────────────────────

/// FNV-1a over script + scene + quality. Stable across runs (unlike
/// `DefaultHasher`), which is what lets a reopened workspace reuse a render.
pub fn cache_key(script: &str, scene: &str, quality: &str) -> String {
    let mut h: u64 = 0xcbf29ce484222325;
    for b in script
        .as_bytes()
        .iter()
        .chain(scene.as_bytes())
        .chain(quality.as_bytes())
    {
        h ^= *b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    format!("{h:016x}")
}

/// The workspace cache root: `<workspace>/.jade/manim` (`.jade/` is already
/// gitignored).
pub fn cache_root(workspace: &Path) -> PathBuf {
    workspace.join(".jade/manim")
}

/// Keep the newest `keep` cache entries, removing the rest. Called on startup.
pub fn prune_cache(root: &Path, keep: usize) {
    let Ok(entries) = std::fs::read_dir(root) else { return };
    let mut dirs: Vec<(std::time::SystemTime, PathBuf)> = entries
        .filter_map(|e| e.ok())
        .filter(|e| e.file_type().map(|t| t.is_dir()).unwrap_or(false))
        .map(|e| {
            let t = e
                .metadata()
                .and_then(|m| m.modified())
                .unwrap_or(std::time::SystemTime::UNIX_EPOCH);
            (t, e.path())
        })
        .collect();
    dirs.sort_by_key(|(t, _)| std::cmp::Reverse(*t));
    for (_, dir) in dirs.into_iter().skip(keep) {
        let _ = std::fs::remove_dir_all(dir);
    }
}

// ── Output-path resolution ───────────────────────────────────────────────────

/// Parse Manim's own `File ready at '<path>'` line. The quoted path may be
/// wrapped over several lines by rich's console; this handles the single-line
/// case and the caller falls back to a directory scan for the rest.
pub fn parse_file_ready(line: &str) -> Option<PathBuf> {
    let idx = line.find("File ready at")?;
    let rest = &line[idx + "File ready at".len()..];
    let start = rest.find('\'')? + 1;
    let end = rest[start..].find('\'')? + start;
    let p = &rest[start..end];
    if p.is_empty() {
        None
    } else {
        Some(PathBuf::from(p))
    }
}

/// The newest `*.mp4` under `dir`, recursively. The quality directory name is
/// version-dependent, so nothing here assumes `720p30`.
pub fn newest_mp4(dir: &Path) -> Option<PathBuf> {
    let mut best: Option<(std::time::SystemTime, PathBuf)> = None;
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&d) else { continue };
        for e in entries.filter_map(|e| e.ok()) {
            let p = e.path();
            if p.is_dir() {
                stack.push(p);
            } else if p.extension().and_then(|x| x.to_str()) == Some("mp4") {
                let t = e
                    .metadata()
                    .and_then(|m| m.modified())
                    .unwrap_or(std::time::SystemTime::UNIX_EPOCH);
                if best.as_ref().map(|(bt, _)| t > *bt).unwrap_or(true) {
                    best = Some((t, p));
                }
            }
        }
    }
    best.map(|(_, p)| p)
}

// ── The render ───────────────────────────────────────────────────────────────

/// The per-request work directory, removed on drop — including the drop that
/// happens when the render task is aborted mid-flight.
struct WorkDir(PathBuf);

impl Drop for WorkDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// Render `script`'s `JadeScene` to an mp4 in the workspace cache.
///
/// Spawns a task; progress and the terminal event arrive on `sink`. The
/// returned handle carries the stop channel and the child pid.
///
/// `cache_root` is per-workspace ([`cache_root`]). A cache hit returns
/// without spawning anything.
pub fn render(
    venv: PathBuf,
    cache_root: PathBuf,
    script: String,
    sink: RenderSink,
) -> RenderHandle {
    let (stop_tx, stop_rx) = oneshot::channel();
    let pid = Arc::new(AtomicU32::new(0));
    let pid2 = pid.clone();
    let join = tokio::spawn(async move {
        let ev = run_render(venv, cache_root, script, &sink, stop_rx, pid2).await;
        let _ = sink.send(ev);
    });
    RenderHandle {
        stop: Some(stop_tx),
        pid,
        join,
    }
}

async fn run_render(
    venv: PathBuf,
    cache_root: PathBuf,
    script: String,
    sink: &RenderSink,
    mut stop_rx: oneshot::Receiver<()>,
    pid_slot: Arc<AtomicU32>,
) -> RenderEvent {
    // Defense in depth: refuse to spawn anything the gate rejects, whatever
    // the caller already checked.
    if let Err(e) = validate_scene_script(&script) {
        return RenderEvent::Failed {
            reason: format!("the script failed validation: {e}"),
        };
    }

    let key = cache_key(&script, SCENE_CLASS, QUALITY);
    let entry = cache_root.join(&key);
    let cached = entry.join("JadeScene.mp4");
    if cached.exists() {
        return RenderEvent::Done { video: cached };
    }

    // Make sure the venv is usable, installing on first use. Slow only once.
    let (setup_tx, mut setup_rx) = tokio::sync::mpsc::unbounded_channel::<String>();
    let sink2 = sink.clone();
    let forward = tokio::spawn(async move {
        while let Some(line) = setup_rx.recv().await {
            let _ = sink2.send(RenderEvent::Log(line.trim_end().to_string()));
        }
    });
    let venv = match venv::ensure(&venv, &setup_tx).await {
        Ok(v) => v,
        Err(e) => {
            drop(setup_tx);
            let _ = forward.await;
            return RenderEvent::Failed {
                reason: format!("Manim setup failed: {e}"),
            };
        }
    };
    drop(setup_tx);
    let _ = forward.await;

    let work = entry.join("work");
    for sub in ["", "tmp", "home"] {
        if let Err(e) = std::fs::create_dir_all(work.join(sub)) {
            return RenderEvent::Failed {
                reason: format!("could not create the work directory: {e}"),
            };
        }
    }
    let work_guard = WorkDir(work.clone());

    let scene_py = work.join("scene.py");
    if let Err(e) = std::fs::write(&scene_py, &script) {
        return RenderEvent::Failed {
            reason: format!("could not write the scene: {e}"),
        };
    }
    let profile = work.join("profile.sb");
    if let Err(e) = std::fs::write(&profile, sandbox_profile(&venv, &work)) {
        return RenderEvent::Failed {
            reason: format!("could not write the sandbox profile: {e}"),
        };
    }

    // The command from the handoff. Verbosity INFO rather than WARNING: the
    // card shows the last log line as progress, and the `File ready at` line
    // this parses is itself logged at INFO.
    let mut cmd = tokio::process::Command::new("/usr/bin/sandbox-exec");
    cmd.arg("-f")
        .arg(&profile)
        .arg(venv::manim_bin(&venv))
        .args(["render", QUALITY, "--format", "mp4", "--media_dir"])
        .arg(&work)
        .args([
            "--disable_caching",
            "--progress_bar",
            "none",
            "--verbosity",
            "INFO",
        ])
        .arg(&scene_py)
        .arg(SCENE_CLASS)
        .current_dir(&work)
        // Python block-buffers stdout when not a tty; the "live log" would
        // otherwise arrive all at once at exit.
        .env("PYTHONUNBUFFERED", "1")
        // Manim can pull matplotlib; a GUI backend would try to open a window
        // from a headless child.
        .env("MPLBACKEND", "Agg")
        // Keep every config/cache write inside the sandbox's writable area.
        .env("HOME", work.join("home"))
        .env("TMPDIR", work.join("tmp"))
        .env("XDG_CACHE_HOME", work.join("home/.cache"))
        // Writes into the venv's site-packages are denied by the profile.
        .env("PYTHONDONTWRITEBYTECODE", "1")
        // ffmpeg (Homebrew) and latex (MacTeX) must be findable without the
        // user's shell PATH.
        .env(
            "PATH",
            format!(
                "{}:/usr/bin:/bin:/opt/homebrew/bin:/usr/local/bin:/Library/TeX/texbin",
                venv.join("bin").display()
            ),
        )
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true);

    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => {
            return RenderEvent::Failed {
                reason: format!("failed to start manim: {e}"),
            }
        }
    };
    let child_pid = child.id().unwrap_or(0);
    pid_slot.store(child_pid, Ordering::SeqCst);
    ACTIVE_PID.store(child_pid, Ordering::SeqCst);

    let mut so = BufReader::new(child.stdout.take().expect("piped")).lines();
    let mut se = BufReader::new(child.stderr.take().expect("piped")).lines();
    let (mut so_done, mut se_done) = (false, false);
    let mut file_ready: Option<PathBuf> = None;
    let mut tail = String::new();
    let deadline = tokio::time::sleep(RENDER_TIMEOUT);
    tokio::pin!(deadline);

    let outcome = loop {
        tokio::select! {
            l = so.next_line(), if !so_done => match l {
                Ok(Some(x)) => {
                    if let Some(p) = parse_file_ready(&x) { file_ready = Some(p); }
                    if !x.trim().is_empty() { tail = x.trim().to_string(); }
                    let _ = sink.send(RenderEvent::Log(x));
                }
                _ => so_done = true,
            },
            l = se.next_line(), if !se_done => match l {
                Ok(Some(x)) => {
                    if let Some(p) = parse_file_ready(&x) { file_ready = Some(p); }
                    if !x.trim().is_empty() { tail = x.trim().to_string(); }
                    let _ = sink.send(RenderEvent::Log(x));
                }
                _ => se_done = true,
            },
            _ = &mut stop_rx => break Outcome::Stopped,
            _ = &mut deadline => break Outcome::TimedOut,
            status = child.wait(), if so_done && se_done => {
                break Outcome::Exited(status.ok().and_then(|s| s.code()).unwrap_or(-1));
            }
        }
    };

    pid_slot.store(0, Ordering::SeqCst);
    // Only clear the global mirror if it still names THIS child.
    let _ = ACTIVE_PID.compare_exchange(child_pid, 0, Ordering::SeqCst, Ordering::SeqCst);
    match outcome {
        Outcome::Stopped => {
            let _ = child.start_kill();
            RenderEvent::Failed {
                reason: "canceled".into(),
            }
        }
        Outcome::TimedOut => {
            let _ = child.start_kill();
            RenderEvent::Failed {
                reason: format!(
                    "the render exceeded {}s and was stopped",
                    RENDER_TIMEOUT.as_secs()
                ),
            }
        }
        Outcome::Exited(code) => {
            // Manim can exit 0 having rendered nothing, so trust the file, not
            // the code. `File ready at` first, newest mp4 as the fallback —
            // the quality directory name is version-dependent.
            let video = file_ready
                .filter(|p| p.exists())
                .or_else(|| newest_mp4(&work.join("videos")));
            match video {
                Some(v) if code == 0 || v.exists() => {
                    // Move the artifact out of the work dir before Drop
                    // removes it.
                    let final_path = entry.join("JadeScene.mp4");
                    if std::fs::rename(&v, &final_path).is_err()
                        && std::fs::copy(&v, &final_path).is_err()
                    {
                        return RenderEvent::Failed {
                            reason: "the render finished but the video could not be moved into the cache".into(),
                        };
                    }
                    drop(work_guard);
                    RenderEvent::Done { video: final_path }
                }
                _ => RenderEvent::Failed {
                    reason: format!("manim exited with code {code}: {tail}"),
                },
            }
        }
    }
}

enum Outcome {
    Stopped,
    TimedOut,
    Exited(i32),
}

#[cfg(test)]
mod tests {
    use super::*;

    const GOOD: &str = "\
from manim import *

class JadeScene(Scene):
    def construct(self):
        t = Text(\"hello\")
        self.play(Write(t))
        self.wait(1)
";

    // ── the gate ─────────────────────────────────────────────────────────

    #[test]
    fn a_wellformed_scene_passes() {
        assert_eq!(validate_scene_script(GOOD), Ok(()));
    }

    #[test]
    fn forbidden_imports_and_calls_are_rejected() {
        for bad in [
            "import os",
            "import sys",
            "import subprocess",
            "import socket",
            "import shutil",
            "import pathlib",
            "x = __import__('os')",
            "f = open('/etc/passwd')",
            "eval('1')",
            "exec('1')",
            "compile('1', 'f', 'eval')",
        ] {
            let script = format!("from manim import *\nclass JadeScene(Scene):\n    def construct(self):\n        {bad}\n");
            assert!(
                validate_scene_script(&script).is_err(),
                "{bad} slipped through"
            );
        }
    }

    /// Indentation must not smuggle an import past the gate.
    #[test]
    fn an_indented_import_is_still_rejected() {
        let s = "from manim import *\nclass JadeScene(Scene):\n    def construct(self):\n        from os import path\n";
        assert!(validate_scene_script(s).is_err());
    }

    #[test]
    fn only_the_manim_star_import_is_allowed() {
        let s = "from manim import Scene\nclass JadeScene(Scene):\n    pass\n";
        assert!(validate_scene_script(s).is_err());
        let s = "import numpy\nfrom manim import *\nclass JadeScene(Scene):\n    pass\n";
        assert!(validate_scene_script(s).is_err());
    }

    #[test]
    fn exactly_one_scene_class() {
        let none = "from manim import *\nx = 1\n";
        assert!(validate_scene_script(none).is_err());
        let two = format!("{GOOD}\nclass JadeScene(Scene):\n    pass\n");
        assert!(validate_scene_script(&two).is_err());
        let other = "from manim import *\nclass Evil(object):\n    pass\n";
        assert!(validate_scene_script(other).is_err());
    }

    #[test]
    fn unexpected_top_level_statements_are_rejected() {
        let s = "from manim import *\nprint('hi')\nclass JadeScene(Scene):\n    pass\n";
        assert!(validate_scene_script(s).is_err());
    }

    /// A `#` comment must not hide a forbidden call, and a forbidden word in a
    /// comment alone must not reject a good script.
    #[test]
    fn comments_are_stripped_before_matching() {
        let hidden = "from manim import *\nclass JadeScene(Scene):\n    def construct(self):\n        eval('x')  # harmless\n";
        assert!(validate_scene_script(hidden).is_err());
        let benign = "from manim import *\n# do not eval( anything\nclass JadeScene(Scene):\n    def construct(self):\n        self.wait(1)\n";
        assert_eq!(validate_scene_script(benign), Ok(()));
    }

    /// `open(` inside a string literal still rejects — err on rejection; the
    /// model can simply not write that string.
    #[test]
    fn missing_import_is_rejected() {
        let s = "class JadeScene(Scene):\n    pass\n";
        assert!(validate_scene_script(s).is_err());
    }

    // ── the profile ──────────────────────────────────────────────────────

    #[test]
    fn the_profile_denies_network_and_scopes_writes_to_the_work_dir() {
        let p = sandbox_profile(Path::new("/v/env"), Path::new("/w/work"));
        assert!(p.contains("(deny network*)"), "{p}");
        assert!(p.contains("(deny file-write*)"), "{p}");
        // The write allow-back runs up to the read deny.
        let start = p.find("(allow file-write*").unwrap();
        let end = p.find("(deny file-read*").unwrap();
        let writes = &p[start..end];
        assert!(writes.contains("/w/work"), "{p}");
        assert!(!writes.contains("/v/env"), "the venv must not be writable: {p}");
    }

    /// The user's home directory must not be readable — that is the exact
    /// exfiltration path this feature worries about. Only the venv and the
    /// work directory are allowed back after the `/Users` deny.
    #[test]
    fn the_profile_denies_home_reads_except_venv_and_work() {
        let p = sandbox_profile(
            Path::new("/Users/u/.local/share/jade/manim-venv"),
            Path::new("/Users/u/proj/.jade/manim/k/work"),
        );
        assert!(p.contains(r#"(deny file-read* (subpath "/Users"))"#), "{p}");
        let readback = p.split("(allow file-read*").nth(1).unwrap();
        let allowed: Vec<&str> = readback
            .lines()
            .filter(|l| l.trim().starts_with("(subpath"))
            .collect();
        assert_eq!(allowed.len(), 2, "{p}");
        assert!(allowed[0].contains("manim-venv") && allowed[1].contains("work"), "{p}");
    }

    // ── cache + output parsing ───────────────────────────────────────────

    #[test]
    fn cache_keys_are_stable_and_input_sensitive() {
        let a = cache_key("s", "JadeScene", "-qm");
        assert_eq!(a, cache_key("s", "JadeScene", "-qm"));
        assert_ne!(a, cache_key("s2", "JadeScene", "-qm"));
        assert_ne!(a, cache_key("s", "JadeScene", "-qh"));
        assert_eq!(a.len(), 16);
    }

    #[test]
    fn file_ready_lines_parse() {
        let line = "INFO     File ready at '/tmp/w/videos/scene/720p30/JadeScene.mp4'";
        assert_eq!(
            parse_file_ready(line),
            Some(PathBuf::from("/tmp/w/videos/scene/720p30/JadeScene.mp4"))
        );
        assert_eq!(parse_file_ready("no such line"), None);
        assert_eq!(parse_file_ready("File ready at ''"), None);
    }

    #[test]
    fn newest_mp4_scans_recursively() {
        let dir = std::env::temp_dir().join(format!("jade-manim-scan-{}", std::process::id()));
        let deep = dir.join("videos/scene/999p99");
        std::fs::create_dir_all(&deep).unwrap();
        std::fs::write(deep.join("Old.mp4"), b"a").unwrap();
        std::fs::write(deep.join("ignore.txt"), b"a").unwrap();
        // Ensure a later mtime on the second file.
        let newer = deep.join("New.mp4");
        std::fs::write(&newer, b"b").unwrap();
        let now = std::time::SystemTime::now() + std::time::Duration::from_secs(5);
        let _ = filetime_set(&newer, now);
        assert_eq!(newest_mp4(&dir), Some(newer));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Set an mtime without pulling the `filetime` crate.
    fn filetime_set(p: &Path, t: std::time::SystemTime) -> std::io::Result<()> {
        let f = std::fs::OpenOptions::new().write(true).open(p)?;
        f.set_modified(t)
    }

    #[test]
    fn prune_keeps_the_newest_entries() {
        let root = std::env::temp_dir().join(format!("jade-manim-prune-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        for i in 0..5 {
            let d = root.join(format!("k{i}"));
            std::fs::create_dir_all(&d).unwrap();
            let f = std::fs::OpenOptions::new()
                .write(true)
                .create(true)
                .truncate(true)
                .open(d.join("stamp"))
                .unwrap();
            drop(f);
            let t = std::time::SystemTime::now() + std::time::Duration::from_secs(i * 10);
            let dh = std::fs::OpenOptions::new().read(true).open(&d).unwrap();
            let _ = dh.set_modified(t);
        }
        prune_cache(&root, 2);
        let left: Vec<String> = std::fs::read_dir(&root)
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(left.len(), 2, "{left:?}");
        assert!(left.contains(&"k3".to_string()) && left.contains(&"k4".to_string()), "{left:?}");
        let _ = std::fs::remove_dir_all(&root);
    }

    /// The render task itself refuses an invalid script — defense in depth
    /// even when a caller forgets the gate.
    #[tokio::test]
    async fn render_refuses_an_invalid_script() {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let mut h = render(
            PathBuf::from("/nonexistent-venv"),
            std::env::temp_dir().join("jade-manim-refuse"),
            "import os\n".into(),
            tx,
        );
        let ev = rx.recv().await.unwrap();
        match ev {
            RenderEvent::Failed { reason } => assert!(reason.contains("validation"), "{reason}"),
            other => panic!("{other:?}"),
        }
        h.stop();
    }

    /// A cache hit returns the artifact without touching the venv or spawning
    /// anything — the venv path here does not even exist.
    #[tokio::test]
    async fn a_cache_hit_skips_the_render() {
        let root = std::env::temp_dir().join(format!("jade-manim-hit-{}", std::process::id()));
        let key = cache_key(GOOD, SCENE_CLASS, QUALITY);
        let entry = root.join(&key);
        std::fs::create_dir_all(&entry).unwrap();
        std::fs::write(entry.join("JadeScene.mp4"), b"mp4").unwrap();
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let _h = render(PathBuf::from("/nonexistent-venv"), root.clone(), GOOD.into(), tx);
        match rx.recv().await.unwrap() {
            RenderEvent::Done { video } => assert_eq!(video, entry.join("JadeScene.mp4")),
            other => panic!("{other:?}"),
        }
        let _ = std::fs::remove_dir_all(&root);
    }
}
