//! Playback tests against a known-good fixture mp4 — the step the handoff
//! orders BEFORE pointing the card at Manim output.
//!
//! `harness = false`, because AVPlayer's state machine delivers through the
//! MAIN run loop: on a libtest worker thread the item status never leaves
//! `Unknown`. Here the checks run on the real main thread and pump the run
//! loop between polls — the same environment GPUI gives the app.
//!
//! The soak (five minutes of pump-loop over the refcount bridge) runs only
//! with `JADE_VIDEO_SOAK=1`; run it under ASan with `MallocScribble` and
//! Zombies before merging changes to `video/avplayer.rs`.

#![cfg(target_os = "macos")]

#[path = "../src/video/mod.rs"]
mod video;

use std::path::PathBuf;
use std::time::{Duration, Instant};

use video::Player;

/// The format gpui's metal renderer asserts on: `420f`.
const WANTED_FORMAT: u32 =
    core_video::pixel_buffer::kCVPixelFormatType_420YpCbCr8BiPlanarFullRange;

fn main() {
    let fixture = fixture_path();

    decodes_the_fixture_to_420f_frames(&fixture);
    eprintln!("ok - decodes_the_fixture_to_420f_frames");

    seeking_while_paused_updates_the_frame(&fixture);
    eprintln!("ok - seeking_while_paused_updates_the_frame");

    if std::env::var("JADE_VIDEO_SOAK").as_deref() == Ok("1") {
        soak_the_bridge(&fixture, Duration::from_secs(300));
        eprintln!("ok - soak_the_bridge (5 minutes)");
    } else {
        // A short soak always runs; it catches gross refcount errors fast.
        soak_the_bridge(&fixture, Duration::from_secs(10));
        eprintln!("ok - soak_the_bridge (10s; JADE_VIDEO_SOAK=1 for the full run)");
    }
}

/// Pump the main run loop once, then sleep a frame's worth.
fn spin() {
    use core_foundation::runloop::{kCFRunLoopDefaultMode, CFRunLoop};
    unsafe {
        CFRunLoop::run_in_mode(kCFRunLoopDefaultMode, Duration::from_millis(8), true);
    }
    std::thread::sleep(Duration::from_millis(8));
}

/// Poll `pump` until a new frame lands or `secs` elapse.
fn pump_until_frame(p: &mut Player, secs: u64) -> bool {
    let deadline = Instant::now() + Duration::from_secs(secs);
    while Instant::now() < deadline {
        if p.pump() {
            return true;
        }
        spin();
    }
    false
}

/// The whole decode path: open, first-frame pump, format guard, transport.
fn decodes_the_fixture_to_420f_frames(fixture: &std::path::Path) {
    let mut p = Player::open(fixture).expect("open");
    p.play();
    assert!(pump_until_frame(&mut p, 10), "no frame within 10s");
    assert!(p.degraded().is_none(), "degraded: {:?}", p.degraded());
    let frame = p.frame().expect("a frame is held");
    assert_eq!(frame.get_pixel_format(), WANTED_FORMAT, "not 420f");
    assert!(frame.get_width() > 0 && frame.get_height() > 0);

    // gpui additionally requires the buffer to be IOSurface-backed; a
    // non-IOSurface buffer would hit an `unwrap()` in its metal renderer.
    assert!(
        frame.get_io_surface().is_some(),
        "the decoded buffer is not IOSurface-backed"
    );

    // AVFoundation loads the duration asynchronously; a decoded first frame
    // does not guarantee it landed. Pump until it does.
    let deadline = Instant::now() + Duration::from_secs(10);
    while p.transport().duration <= 0.5 && Instant::now() < deadline {
        spin();
    }
    let t = p.transport();
    assert!(t.duration > 0.5, "duration {}", t.duration);
    p.pause();
    assert!(!p.transport().playing);
}

/// A paused seek updates the frame — the pump-once-after-seek contract that
/// keeps the scrub bar live while paused.
fn seeking_while_paused_updates_the_frame(fixture: &std::path::Path) {
    let mut p = Player::open(fixture).expect("open");
    p.play();
    assert!(pump_until_frame(&mut p, 10), "no first frame");
    p.pause();
    for target in [0.5, 0.1, 0.9, 0.0] {
        p.seek(target);
        let d = Instant::now() + Duration::from_millis(1500);
        while Instant::now() < d {
            if p.pump() {
                break;
            }
            spin();
        }
    }
    assert!(p.degraded().is_none());
    assert!(p.frame().is_some());
}

/// Loop the clip, pumping and touching every frame the way the renderer
/// would. Any over-release in the bridge shows up here as a crash.
fn soak_the_bridge(fixture: &std::path::Path, run: Duration) {
    let mut p = Player::open(fixture).expect("open");
    p.play();
    let end = Instant::now() + run;
    let mut frames = 0u64;
    while Instant::now() < end {
        if p.transport().finished() {
            p.play(); // replays from the top
        }
        if p.pump() {
            frames += 1;
            if let Some(f) = p.frame() {
                assert_eq!(f.get_pixel_format(), WANTED_FORMAT);
                let _ = (f.get_width(), f.get_height());
            }
        }
        spin();
    }
    let min = run.as_secs() * 10; // far below 30fps; catches a stalled pump
    assert!(frames > min, "only {frames} frames in {run:?}");
}

/// A deterministic 2-second 720p30 h264 clip, the exact shape Manim emits.
/// Generated with ffmpeg on first use rather than checked in, so the repo
/// carries no binary blob; ffmpeg is a hard dependency of Manim itself, so it
/// is present on any machine that can run the feature.
fn fixture_path() -> PathBuf {
    let dir = std::env::temp_dir().join("jade-video-fixtures");
    let path = dir.join("fixture-720p30.mp4");
    if path.exists() {
        return path;
    }
    std::fs::create_dir_all(&dir).unwrap();
    let out = std::process::Command::new("ffmpeg")
        .args([
            "-y", "-f", "lavfi", "-i", "testsrc2=size=1280x720:rate=30:duration=2",
            "-pix_fmt", "yuv420p", "-c:v", "libx264", "-preset", "ultrafast",
        ])
        .arg(&path)
        .env("PATH", "/usr/bin:/bin:/opt/homebrew/bin:/usr/local/bin")
        .output()
        .expect("ffmpeg must be installed (Manim requires it too)");
    assert!(out.status.success(), "ffmpeg failed: {out:?}");
    path
}
