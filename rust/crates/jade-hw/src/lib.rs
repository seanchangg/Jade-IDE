//! `jade-hw` — the hardware-mode simulation engine.
//!
//! The engine compiles a Verilog design with Verilator into a native
//! simulator, runs it as a persistent child, and hot-swaps it on save. The
//! design maps 1:1 to the DK-DEV-10M50-A board: LEDs, push buttons, DIP
//! switches, and the 50 MHz clock.
//!
//! Data flow: the UI sends [`HwCommand`] into the session; the session sends
//! [`HwEvent`] back. The raw `__JADE_HW|` stdio protocol stays internal; the
//! session normalizes electrical levels to logical lit/pressed/on values.

pub mod board;
pub mod compile;
pub mod gen;
pub mod mapping;
pub mod netlist;
pub mod ports;
pub mod protocol;
pub mod qsf;
pub mod session;
pub mod sim;
pub mod synth;
pub mod testbench;
pub mod wave;

use std::path::PathBuf;
use std::sync::Mutex;

use tokio::sync::{mpsc, oneshot};

pub use compile::{detect_verilator, HwBuildResult};
pub use jade_build::{BuildError, Severity};
pub use protocol::{HwCommand, HwEvent, HwRunState};
pub use session::{run_session, InputState, Phase, SessionSm};

/// Handle to a running hardware session.
pub struct HwEngine {
    /// Commands into the session (the UI's `hw_tx`).
    pub commands: mpsc::UnboundedSender<HwCommand>,
    stop: Mutex<Option<oneshot::Sender<()>>>,
    pub task: tokio::task::JoinHandle<()>,
}

impl HwEngine {
    /// End the session: the sim gets a graceful QUIT, the compile is killed.
    pub fn stop(&self) {
        if let Some(tx) = self.stop.lock().unwrap().take() {
            let _ = tx.send(());
        }
    }
}

/// Start a hardware session for `project_root`. Must run inside a tokio
/// runtime. Events arrive on `events`; the caller forwards them to the app.
pub fn start_session(
    project_root: PathBuf,
    events: mpsc::UnboundedSender<HwEvent>,
) -> HwEngine {
    let (cmd_tx, cmd_rx) = mpsc::unbounded_channel();
    let (stop_tx, stop_rx) = oneshot::channel();
    let task = tokio::spawn(session::run_session(project_root, cmd_rx, events, stop_rx));
    HwEngine {
        commands: cmd_tx,
        stop: Mutex::new(Some(stop_tx)),
        task,
    }
}
