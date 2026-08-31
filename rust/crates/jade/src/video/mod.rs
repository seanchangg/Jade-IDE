//! Video playback for the Visualize card (inventory §4.15, step 3).
//!
//! macOS: AVFoundation decodes the rendered mp4 and hands 420f
//! `CVPixelBuffer`s to gpui's `surface()` element, zero-copy. Everything
//! touching AVFoundation is behind `cfg(target_os = "macos")` with a portable
//! stub, so the headless suites keep running — the same split `wg3d` uses for
//! its Metal renderer.
//!
//! The frame pump is NOT a timer. `JadeApp::ensure_video_frame` calls
//! [`Player::pump`] once per animation frame via
//! `window.request_animation_frame()`, and stops requesting frames the moment
//! the card pauses or leaves the screen — the settle-and-stop contract
//! `wg3d::render::ensure_anim` honors.

#[cfg(target_os = "macos")]
mod avplayer;
#[cfg(target_os = "macos")]
pub use avplayer::Player;

#[cfg(not(target_os = "macos"))]
mod stub;
#[cfg(not(target_os = "macos"))]
pub use stub::Player;

/// Transport state the card renders, identical on both platforms.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Transport {
    pub playing: bool,
    /// Seconds. Zero until the asset reports it.
    pub duration: f64,
    pub position: f64,
}

impl Transport {
    /// Scrub fraction in `0.0..=1.0`, safe against a zero duration.
    pub fn fraction(&self) -> f64 {
        if self.duration <= 0.0 {
            0.0
        } else {
            (self.position / self.duration).clamp(0.0, 1.0)
        }
    }

    /// Whether playback stopped at the end (the replay affordance).
    pub fn finished(&self) -> bool {
        !self.playing && self.duration > 0.0 && self.position >= self.duration - 0.05
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fraction_is_clamped_and_zero_safe() {
        let t = Transport { playing: false, duration: 0.0, position: 3.0 };
        assert_eq!(t.fraction(), 0.0);
        let t = Transport { playing: true, duration: 10.0, position: 2.5 };
        assert_eq!(t.fraction(), 0.25);
        let t = Transport { playing: true, duration: 10.0, position: 99.0 };
        assert_eq!(t.fraction(), 1.0);
    }

    #[test]
    fn finished_needs_a_known_duration() {
        let t = Transport { playing: false, duration: 0.0, position: 0.0 };
        assert!(!t.finished(), "an unloaded asset is not finished");
        let t = Transport { playing: false, duration: 8.0, position: 7.99 };
        assert!(t.finished());
        let t = Transport { playing: true, duration: 8.0, position: 7.99 };
        assert!(!t.finished(), "still playing");
    }
}
