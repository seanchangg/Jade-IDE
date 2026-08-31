//! The portable no-op player. Keeps the headless suites and non-macOS builds
//! compiling; the card renders its Ready state with a "playback needs macOS"
//! line instead of frames.

use std::path::Path;

use super::Transport;

pub struct Player {
    transport: Transport,
}

impl Player {
    pub fn open(_path: &Path) -> Result<Player, String> {
        Ok(Player {
            transport: Transport { playing: false, duration: 0.0, position: 0.0 },
        })
    }

    pub fn play(&mut self) {}
    pub fn pause(&mut self) {}
    pub fn toggle(&mut self) {}

    pub fn seek(&mut self, _seconds: f64) {}

    /// No frames on this platform; `false` means nothing new to paint.
    pub fn pump(&mut self) -> bool {
        false
    }

    pub fn transport(&self) -> Transport {
        self.transport
    }

    /// Whether playback degraded (wrong pixel format). Never on the stub.
    pub fn degraded(&self) -> Option<&str> {
        None
    }
}
