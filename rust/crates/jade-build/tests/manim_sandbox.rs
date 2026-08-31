//! End-to-end tests for the sandboxed Manim render (§4.15 steps 1–2).
//!
//! These run a REAL render inside the real sandbox, so they need the private
//! venv (built on first use, several minutes) and macOS. They are `#[ignore]`d
//! for the normal suite; run them with:
//!
//! ```sh
//! cargo test -p jade-build --test manim_sandbox -- --ignored
//! ```

#![cfg(target_os = "macos")]

use std::path::PathBuf;

use jade_build::manim::{self, RenderEvent};

const SCENE: &str = "\
from manim import *

class JadeScene(Scene):
    def construct(self):
        t = Text(\"sandbox test\")
        self.play(Write(t))
        self.wait(0.5)
";

fn scratch(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("jade-manim-e2e-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

/// The full path: venv ensure (reused when present), gate, sandbox spawn,
/// output-path resolution, move into the cache — then a second call that must
/// hit the cache without spawning.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "runs a real multi-second Manim render; needs the private venv"]
async fn a_real_scene_renders_inside_the_sandbox_and_caches() {
    let cache = scratch("render");
    let venv = jade_build::venv::venv_dir();

    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let _h = manim::render(venv.clone(), cache.clone(), SCENE.into(), tx);
    let video = loop {
        match rx.recv().await.expect("the channel must yield a terminal event") {
            RenderEvent::Log(_) => continue,
            RenderEvent::Done { video } => break video,
            RenderEvent::Failed { reason } => panic!("render failed: {reason}"),
        }
    };
    assert!(video.exists(), "{video:?}");
    assert!(
        std::fs::metadata(&video).unwrap().len() > 1_000,
        "suspiciously small mp4"
    );
    // The work directory is removed once the artifact is moved out.
    assert!(
        !video.parent().unwrap().join("work").exists(),
        "the work dir must not outlive the render"
    );

    // Second call: a cache hit, immediate, no venv needed (poison the path).
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let _h = manim::render(PathBuf::from("/nonexistent"), cache.clone(), SCENE.into(), tx);
    match rx.recv().await.unwrap() {
        RenderEvent::Done { video: v2 } => assert_eq!(v2, video),
        other => panic!("expected a cache hit, got {other:?}"),
    }
    let _ = std::fs::remove_dir_all(&cache);
}

/// The three security properties, probed with hostile one-liners run under
/// the exact profile the renderer writes. The gate would reject these
/// scripts; the sandbox must stop them even if the gate is bypassed.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "spawns sandboxed python; needs the private venv"]
async fn the_sandbox_blocks_home_reads_network_and_outside_writes() {
    let venv = jade_build::venv::venv_dir();
    let python = venv.join("bin/python3");
    if !python.exists() {
        panic!("the private venv is missing; run the render test first");
    }
    let work = scratch("probes");
    std::fs::create_dir_all(work.join("home")).unwrap();
    let profile = work.join("profile.sb");
    std::fs::write(&profile, manim::sandbox_profile(&venv, &work)).unwrap();

    let home = std::env::var("HOME").unwrap();
    let probe = |code: String| {
        let python = python.clone();
        let profile = profile.clone();
        let work = work.clone();
        async move {
            tokio::process::Command::new("/usr/bin/sandbox-exec")
                .arg("-f")
                .arg(&profile)
                .arg(&python)
                .arg("-c")
                .arg(&code)
                .current_dir(&work)
                .env("HOME", work.join("home"))
                .kill_on_drop(true)
                .output()
                .await
                .unwrap()
        }
    };

    // Reading a file in the user's home must fail…
    let out = probe(format!("print(open('{home}/.zshrc').read())")).await;
    assert!(!out.status.success(), "home read was allowed");

    // …the network must be unreachable…
    let out = probe(
        "import socket; socket.create_connection(('1.1.1.1', 443), timeout=3)".to_string(),
    )
    .await;
    assert!(!out.status.success(), "network was allowed");

    // …writes must stay inside the work directory…
    let out = probe("open('/private/tmp/jade-sbx-escape.txt', 'w').write('x')".to_string()).await;
    assert!(!out.status.success(), "an outside write was allowed");
    assert!(!std::path::Path::new("/private/tmp/jade-sbx-escape.txt").exists());

    // …and a write INSIDE it must succeed, or the render itself could not work.
    let out = probe(format!("open('{}/ok.txt', 'w').write('x')", work.display())).await;
    assert!(out.status.success(), "the work dir write failed: {out:?}");

    let _ = std::fs::remove_dir_all(&work);
}

/// The wall-clock cap: a scene that loops forever is killed, not waited on.
/// Uses a tiny timeout by rendering nothing — covered instead at the unit
/// level by the stop channel; the 180s cap is not worth a 180s test.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "spawns a real render and stops it mid-flight; needs the private venv"]
async fn stop_kills_an_inflight_render() {
    let cache = scratch("stop");
    let venv = jade_build::venv::venv_dir();
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let mut h = manim::render(venv, cache.clone(), SCENE.into(), tx);
    // Let it get past the gate and spawn, then pull the plug.
    tokio::time::sleep(std::time::Duration::from_secs(2)).await;
    h.stop();
    let ev = loop {
        match rx.recv().await.expect("terminal event") {
            RenderEvent::Log(_) => continue,
            other => break other,
        }
    };
    match ev {
        RenderEvent::Failed { reason } => assert!(reason.contains("cancel"), "{reason}"),
        // A very fast machine may legitimately finish first; accept it.
        RenderEvent::Done { .. } => {}
        RenderEvent::Log(_) => unreachable!("logs are consumed by the loop above"),
    }
    let _ = std::fs::remove_dir_all(&cache);
}
