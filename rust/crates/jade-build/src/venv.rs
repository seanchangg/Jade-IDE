//! The private Manim virtual environment (inventory §4.15, step 1).
//!
//! Visualize renders model-authored Manim scenes to mp4. Manim is a heavy
//! Python package, and on this machine `python3` is Anaconda — installing into
//! the user's own Python would be hostile. So Jade keeps a private venv at
//! `~/.local/share/jade/manim-venv`, created with `python3 -m venv` and filled
//! with one `pip install manim`. Nothing outside that directory is touched.
//!
//! Discovery mirrors [`find_llama_server`](../../jade-ai/src/backend.rs): an
//! explicit `JADE_PYTHON` wins, then each PATH entry, then the extra install
//! dirs — GUI apps on macOS do not inherit the shell PATH, which is why the
//! extra tier exists.

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use tokio::process::Command;

use crate::compile::OutputSink;

/// GUI apps on macOS do not inherit the shell PATH, so probe common install
/// dirs. Anaconda's default location is included: on the reference machine
/// `python3` lives only there.
const EXTRA_BIN_DIRS: [&str; 4] = [
    "/opt/homebrew/bin",
    "/usr/local/bin",
    "/opt/local/bin",
    "/opt/anaconda3/bin",
];

fn is_executable(p: &Path) -> bool {
    std::fs::metadata(p)
        .map(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
        .unwrap_or(false)
}

/// Locate a `python3` to build the venv with:
/// `JADE_PYTHON` (must exist) → each PATH dir → the extra install dirs.
pub fn find_python3() -> Option<PathBuf> {
    if let Ok(explicit) = std::env::var("JADE_PYTHON") {
        if !explicit.is_empty() && Path::new(&explicit).exists() {
            return Some(PathBuf::from(explicit));
        }
    }
    let path_dirs = std::env::var("PATH").unwrap_or_default();
    let dirs = path_dirs
        .split(':')
        .map(str::to_string)
        .chain(EXTRA_BIN_DIRS.iter().map(|s| s.to_string()));
    for dir in dirs {
        if dir.is_empty() {
            continue;
        }
        let candidate = Path::new(&dir).join("python3");
        if is_executable(&candidate) {
            return Some(candidate);
        }
    }
    None
}

/// Where the private venv lives. `JADE_MANIM_VENV` overrides it, which keeps
/// the tests off the user's real data directory.
pub fn venv_dir() -> PathBuf {
    if let Ok(explicit) = std::env::var("JADE_MANIM_VENV") {
        if !explicit.is_empty() {
            return PathBuf::from(explicit);
        }
    }
    let home = std::env::var_os("HOME").map(PathBuf::from).unwrap_or_default();
    home.join(".local/share/jade/manim-venv")
}

/// The `manim` entry point inside a venv.
pub fn manim_bin(venv: &Path) -> PathBuf {
    venv.join("bin/manim")
}

/// Whether the venv already holds a runnable `manim`.
pub fn is_ready(venv: &Path) -> bool {
    is_executable(&manim_bin(venv))
}

/// Ask the venv's manim for its version, for the failure banners. One short
/// line, e.g. `Manim Community v0.19.0`.
pub async fn probe_version(venv: &Path) -> Option<String> {
    let out = Command::new(manim_bin(venv))
        .arg("--version")
        .env("PYTHONUNBUFFERED", "1")
        .kill_on_drop(true)
        .output()
        .await
        .ok()?;
    let text = String::from_utf8_lossy(&out.stdout);
    let line = text.lines().find(|l| !l.trim().is_empty())?;
    Some(line.trim().to_string())
}

/// Create the venv and install Manim into it, streaming progress to `out`.
///
/// Idempotent: a venv that already answers `manim --version` is reused as-is.
/// A partial venv (crashed install) is completed by re-running pip, which is
/// itself idempotent. Returns the venv path.
pub async fn ensure(venv: &Path, out: &OutputSink) -> Result<PathBuf, String> {
    if is_ready(venv) {
        return Ok(venv.to_path_buf());
    }

    let python = find_python3().ok_or_else(|| {
        "No python3 found. Install Python 3, or point JADE_PYTHON at one.".to_string()
    })?;

    if !venv.join("bin/pip").exists() {
        let _ = out.send(format!("[manim] Creating a private venv at {}\n", venv.display()));
        if let Some(parent) = venv.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        run_step(Command::new(&python).args(["-m", "venv"]).arg(venv), out, "python3 -m venv")
            .await?;
    }

    let _ = out.send("[manim] Installing manim into the private venv (first run only)\n".into());
    run_step(
        Command::new(venv.join("bin/pip")).args(["install", "--quiet", "manim"]),
        out,
        "pip install manim",
    )
    .await?;

    if !is_ready(venv) {
        return Err("pip finished but the venv has no manim entry point".into());
    }
    Ok(venv.to_path_buf())
}

/// Run one setup command to completion, streaming merged output.
async fn run_step(
    cmd: &mut Command,
    out: &OutputSink,
    what: &str,
) -> Result<(), String> {
    use tokio::io::{AsyncBufReadExt, BufReader};

    cmd.stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .env("PYTHONUNBUFFERED", "1")
        .kill_on_drop(true);
    let mut child = cmd.spawn().map_err(|e| format!("Failed to start {what}: {e}"))?;
    let mut so = BufReader::new(child.stdout.take().expect("piped")).lines();
    let mut se = BufReader::new(child.stderr.take().expect("piped")).lines();
    let mut tail = String::new();
    let (mut so_done, mut se_done) = (false, false);
    loop {
        tokio::select! {
            l = so.next_line(), if !so_done => match l {
                Ok(Some(x)) => { let _ = out.send(format!("{x}\n")); }
                _ => so_done = true,
            },
            l = se.next_line(), if !se_done => match l {
                Ok(Some(x)) => { tail = x.clone(); let _ = out.send(format!("{x}\n")); }
                _ => se_done = true,
            },
        }
        if so_done && se_done {
            break;
        }
    }
    let status = child.wait().await.map_err(|e| format!("{what}: {e}"))?;
    if status.success() {
        Ok(())
    } else {
        Err(format!("{what} failed ({status}): {tail}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn manim_bin_is_under_the_venv() {
        assert_eq!(
            manim_bin(Path::new("/x/venv")),
            PathBuf::from("/x/venv/bin/manim")
        );
    }

    #[test]
    fn a_missing_venv_is_not_ready() {
        assert!(!is_ready(Path::new("/nonexistent/venv")));
    }
}
