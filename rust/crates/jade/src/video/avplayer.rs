//! AVFoundation playback: `AVPlayer` + `AVPlayerItemVideoOutput` asked for
//! 420f directly, handed to `gpui::surface()` with no conversion.
//!
//! Two hazards shape this file (§4.15 risks 1 and 3):
//!
//! **GPUI panics on a wrong pixel format — it does not degrade.** Its metal
//! renderer `assert_eq!`s the surface's pixel format against
//! `kCVPixelFormatType_420YpCbCr8BiPlanarFullRange` and `unwrap()`s the plane
//! textures. So the output is created with the full attribute dictionary
//! (format + Metal compatibility + IOSurface backing, the same trio
//! `wg3d/metal.rs` builds), and [`Player::pump`] guards the format anyway,
//! degrading to a text banner instead of taking the IDE down.
//!
//! **The CVPixelBuffer bridge is one function, and refcounting it wrong is an
//! intermittent crash under load.** `copyPixelBufferForItemTime:` follows the
//! COPY rule (+1), and the objc2 `CFRetained` owns that +1. The bridge to the
//! `core-video` crate type gpui takes must therefore RETAIN again
//! (`wrap_under_get_rule`), so both owners can drop independently. Never
//! `wrap_under_create_rule` there — `wg3d/metal.rs` documents the sibling
//! trap for `CVMetalTextureGetTexture`.

use std::path::Path;

use core_video::pixel_buffer::kCVPixelFormatType_420YpCbCr8BiPlanarFullRange;
use objc2::rc::Retained;
use objc2::runtime::AnyObject;
use objc2::{AnyThread, MainThreadMarker};
use objc2_av_foundation::{AVPlayer, AVPlayerItem, AVPlayerItemStatus, AVPlayerItemVideoOutput};
use objc2_core_media::CMTime;
use objc2_foundation::{NSDictionary, NSNumber, NSString, NSURL};

use super::Transport;

/// The exact format gpui's shader (and its assert) expects: `420f`.
const WANTED_FORMAT: u32 = kCVPixelFormatType_420YpCbCr8BiPlanarFullRange;

/// One rendered mp4, decoding on demand.
pub struct Player {
    player: Retained<AVPlayer>,
    item: Retained<AVPlayerItem>,
    output: Retained<AVPlayerItemVideoOutput>,
    /// The frame currently on screen.
    frame: Option<core_video::pixel_buffer::CVPixelBuffer>,
    /// The previous frame, kept exactly one extra pump so the compositor is
    /// never looking at a buffer we just dropped.
    prev_frame: Option<core_video::pixel_buffer::CVPixelBuffer>,
    /// `Some(reason)` once a frame arrived in a format gpui would assert on.
    /// Playback then presents no surface rather than crashing the IDE.
    degraded: Option<&'static str>,
    /// The user's intent, distinct from `rate()`: AVPlayer sets rate to 0
    /// itself at the end of the clip.
    want_playing: bool,
}

// SAFETY: AVPlayer, AVPlayerItem and AVPlayerItemVideoOutput are documented
// thread-safe to message; Jade only touches the player from the GPUI main
// thread anyway — this exists because JadeApp itself must be Send for gpui.
unsafe impl Send for Player {}

impl Player {
    /// Open `path` paused at its first frame.
    pub fn open(path: &Path) -> Result<Player, String> {
        let Some(path_str) = path.to_str() else {
            return Err("the video path is not valid UTF-8".into());
        };
        // SAFETY of the marker: objc2 declares the AVPlayer family main-thread
        // only, but Apple's own contract is only "do not message one player
        // from two threads at once". Jade creates and drives the player from
        // the GPUI main thread; the unit tests drive one from a single test
        // thread. Neither shares a player across threads.
        let mtm = unsafe { MainThreadMarker::new_unchecked() };
        unsafe {
            let url = NSURL::fileURLWithPath(&NSString::from_str(path_str));
            let item = AVPlayerItem::playerItemWithURL(&url, mtm);

            // All three attributes, or gpui's metal renderer may panic on the
            // buffer: the 420f format its shader expects, Metal compatibility,
            // and IOSurface backing (an empty dictionary opts in).
            let format_key: &NSString =
                cf_as_ns(objc2_core_video::kCVPixelBufferPixelFormatTypeKey);
            let metal_key: &NSString =
                cf_as_ns(objc2_core_video::kCVPixelBufferMetalCompatibilityKey);
            let iosurface_key: &NSString =
                cf_as_ns(objc2_core_video::kCVPixelBufferIOSurfacePropertiesKey);
            let format_v = NSNumber::new_u32(WANTED_FORMAT);
            let metal_v = NSNumber::new_bool(true);
            let empty_v: Retained<NSDictionary> = NSDictionary::new();
            let values: [&AnyObject; 3] = [&format_v, &metal_v, &empty_v];
            let attrs: Retained<NSDictionary<NSString, AnyObject>> = NSDictionary::from_slices(
                &[format_key, metal_key, iosurface_key],
                &values,
            );

            let output = AVPlayerItemVideoOutput::initWithPixelBufferAttributes(
                AVPlayerItemVideoOutput::alloc(),
                Some(&attrs),
            );
            item.addOutput(&output);

            let player = AVPlayer::playerWithPlayerItem(Some(&item), mtm);
            // Hold the last frame at the end of the clip rather than going
            // black; the transport offers replay.
            player.setActionAtItemEnd(objc2_av_foundation::AVPlayerActionAtItemEnd::Pause);

            Ok(Player {
                player,
                item,
                output,
                frame: None,
                prev_frame: None,
                degraded: None,
                want_playing: false,
            })
        }
    }

    pub fn play(&mut self) {
        // Replay from the top when the clip already finished.
        if self.transport().finished() {
            self.seek(0.0);
        }
        self.want_playing = true;
        unsafe { self.player.play() };
    }

    pub fn pause(&mut self) {
        self.want_playing = false;
        unsafe { self.player.pause() };
    }

    pub fn toggle(&mut self) {
        if self.transport().playing {
            self.pause()
        } else {
            self.play()
        }
    }

    /// Seek with zero tolerance — Manim's x264 output has sparse keyframes
    /// and a tolerant seek feels notchy — then pump once so the frame updates
    /// while paused.
    pub fn seek(&mut self, seconds: f64) {
        let zero = CMTime {
            value: 0,
            timescale: 1,
            flags: objc2_core_media::CMTimeFlags::Valid,
            epoch: 0,
        };
        let target = CMTime {
            value: (seconds.max(0.0) * 600.0) as i64,
            timescale: 600,
            flags: objc2_core_media::CMTimeFlags::Valid,
            epoch: 0,
        };
        unsafe {
            self.player
                .seekToTime_toleranceBefore_toleranceAfter(target, zero, zero);
        }
        self.pump();
    }

    /// One `hasNewPixelBufferForItemTime:` poll. Returns whether a new frame
    /// was taken. Called once per animation frame while playing, and once
    /// after a seek.
    pub fn pump(&mut self) -> bool {
        if self.degraded.is_some() {
            return false;
        }
        unsafe {
            // The presentation clock: the current host (mach absolute) time,
            // mapped into the item's timeline by the output itself.
            // `core_video::host_time` is already a dependency; no new crate.
            let host_now = core_video::host_time::get_current_host_time();
            let item_time = self.output.itemTimeForMachAbsoluteTime(host_now as i64);
            if !self.output.hasNewPixelBufferForItemTime(item_time) {
                return false;
            }
            let Some(buf) = self
                .output
                .copyPixelBufferForItemTime_itemTimeForDisplay(item_time, std::ptr::null_mut())
            else {
                return false;
            };
            let pb = bridge(&buf);
            // gpui's renderer asserts on the format; we degrade instead.
            if pb.get_pixel_format() != WANTED_FORMAT {
                self.degraded = Some("the decoder produced an unexpected pixel format");
                self.frame = None;
                self.prev_frame = None;
                return true;
            }
            // Keep the outgoing frame one extra pump; the compositor may
            // still reference it this frame.
            self.prev_frame = self.frame.replace(pb);
            true
        }
    }

    /// The frame to hand `gpui::surface()`, if any.
    pub fn frame(&self) -> Option<core_video::pixel_buffer::CVPixelBuffer> {
        self.frame.clone()
    }

    pub fn transport(&self) -> Transport {
        unsafe {
            let rate = self.player.rate();
            let duration = if self.item.status() == AVPlayerItemStatus::ReadyToPlay {
                cm_seconds(self.item.duration())
            } else {
                0.0
            };
            Transport {
                playing: rate > 0.0,
                duration,
                position: cm_seconds(self.player.currentTime()).clamp(0.0, duration.max(0.0)),
            }
        }
    }

    /// `Some(reason)` when playback degraded rather than crash the renderer.
    pub fn degraded(&self) -> Option<&str> {
        self.degraded
    }
}

impl Drop for Player {
    fn drop(&mut self) {
        unsafe { self.player.pause() };
    }
}

/// CMTime → seconds, 0.0 for invalid or indefinite times.
fn cm_seconds(t: CMTime) -> f64 {
    if !t.flags.contains(objc2_core_media::CMTimeFlags::Valid) || t.timescale == 0 {
        return 0.0;
    }
    t.value as f64 / t.timescale as f64
}

/// The one bridge from objc2's retained `CVBuffer` (what
/// `copyPixelBufferForItemTime:` yields, a +1 the `Retained` owns) to the
/// `core-video` crate type gpui's `surface()` takes.
///
/// `wrap_under_get_rule` RETAINS, so the objc2 side keeps its own +1 and both
/// may drop independently. Never `wrap_under_create_rule` here — that would
/// steal the +1 and over-release when both drop.
fn bridge(
    buf: &Retained<objc2_core_video::CVBuffer>,
) -> core_video::pixel_buffer::CVPixelBuffer {
    use core_foundation::base::TCFType;
    let raw = Retained::as_ptr(buf) as core_video::pixel_buffer::CVPixelBufferRef;
    unsafe { core_video::pixel_buffer::CVPixelBuffer::wrap_under_get_rule(raw) }
}

/// Toll-free bridge a CoreFoundation string constant to the `NSString` the
/// Foundation dictionary API wants.
fn cf_as_ns(cf: &objc2_core_foundation::CFString) -> &NSString {
    // SAFETY: CFString and NSString are toll-free bridged; the reference
    // stays borrowed.
    unsafe { &*(cf as *const objc2_core_foundation::CFString as *const NSString) }
}

