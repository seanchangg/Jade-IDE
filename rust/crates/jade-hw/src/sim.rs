//! Spawn and talk to one simulator child over stdio.
//!
//! The child is the Verilator binary `jade_hw_sim`. Commands go in over
//! stdin; protocol lines come back over stdout. Malformed lines are
//! swallowed. Stderr is forwarded verbatim so a crash stays visible.

use std::path::Path;

use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::sync::{mpsc, oneshot};

use crate::protocol::{format_command, parse_hw_line, SimCommand, SimLine};

/// One event from the simulator child.
#[derive(Debug, Clone, PartialEq)]
pub enum SimEvent {
    Line(SimLine),
    Stderr(String),
    /// The child ended with this exit code (-1 for a signal).
    Exited(i32),
}

/// Handle to a running simulator child.
pub struct SimHandle {
    /// Commands to the child's stdin.
    pub input: mpsc::UnboundedSender<SimCommand>,
    /// Parsed events from the child.
    pub events: mpsc::UnboundedReceiver<SimEvent>,
    /// Hard-kill signal. The graceful path is a `Quit` command first.
    pub stop: Option<oneshot::Sender<()>>,
    pub result: tokio::task::JoinHandle<i32>,
}

impl SimHandle {
    /// Send one command; ignore a closed channel.
    pub fn send(&self, cmd: SimCommand) {
        let _ = self.input.send(cmd);
    }
}

/// Spawn the simulator binary with `cwd` as its working directory.
pub fn spawn_sim(binary: &Path, cwd: &Path) -> std::io::Result<SimHandle> {
    let mut child = tokio::process::Command::new(binary)
        .current_dir(cwd)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()?;

    let (input_tx, mut input_rx) = mpsc::unbounded_channel::<SimCommand>();
    let (events_tx, events_rx) = mpsc::unbounded_channel::<SimEvent>();
    let (stop_tx, mut stop_rx) = oneshot::channel::<()>();

    let mut stdin = child.stdin.take().expect("piped");
    let mut so = BufReader::new(child.stdout.take().expect("piped")).lines();
    let mut se = BufReader::new(child.stderr.take().expect("piped")).lines();

    let result = tokio::spawn(async move {
        let (mut so_done, mut se_done) = (false, false);
        let mut killed = false;
        let mut input_open = true;
        loop {
            tokio::select! {
                _ = &mut stop_rx, if !killed => {
                    let _ = child.start_kill();
                    killed = true;
                }
                cmd = input_rx.recv(), if input_open => match cmd {
                    Some(cmd) => {
                        let mut line = format_command(&cmd);
                        line.push('\n');
                        // A write error means the child is gone; the exit
                        // path below reports it.
                        let _ = stdin.write_all(line.as_bytes()).await;
                    }
                    // A closed command channel is not an exit signal: the
                    // reader arms below still drain the child.
                    None => input_open = false,
                },
                line = so.next_line(), if !so_done => match line {
                    Ok(Some(l)) => {
                        if let Some(parsed) = parse_hw_line(&l) {
                            let _ = events_tx.send(SimEvent::Line(parsed));
                        }
                        // Non-protocol stdout is swallowed, like run.rs does
                        // for unknown __JADE_ lines.
                    }
                    _ => so_done = true,
                },
                line = se.next_line(), if !se_done => match line {
                    Ok(Some(l)) => { let _ = events_tx.send(SimEvent::Stderr(l)); }
                    _ => se_done = true,
                },
            }
            if so_done && se_done {
                break;
            }
        }
        let code = child.wait().await.ok().and_then(|s| s.code()).unwrap_or(-1);
        let _ = events_tx.send(SimEvent::Exited(code));
        code
    });

    Ok(SimHandle {
        input: input_tx,
        events: events_rx,
        stop: Some(stop_tx),
        result,
    })
}

/// Gracefully end a simulator: send `QUIT`, give the child 200 ms, then
/// hard-kill. The handle moves into a detached task.
pub fn shutdown_sim(mut sim: SimHandle) {
    sim.send(SimCommand::Quit);
    let stop = sim.stop.take();
    tokio::spawn(async move {
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        if let Some(stop) = stop {
            let _ = stop.send(());
        }
        let _ = sim.result.await;
    });
}
