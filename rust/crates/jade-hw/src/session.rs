//! The hardware session: rebuild-on-save state machine, hot swap, input
//! replay, and the translation from raw protocol lines to logical UI events.
//!
//! The pure state machine ([`SessionSm`]) is separate from the async driver
//! ([`run_session`]) so the swap rules are testable without a toolchain.

use std::path::{Path, PathBuf};

use jade_build::{BuildError, Severity};
use tokio::sync::{mpsc, oneshot};

use crate::board::{dk_dev_10m50a, BoardDef};
use crate::compile::{self, HwBuildResult};
use crate::mapping;
use crate::ports;
use crate::protocol::{
    dip_level, dip_mask, duty_to_lit, led_mask_to_lit, pb_level, HwCommand, HwEvent, HwRunState,
    SimCommand, SimLine, SimRunState,
};
use crate::qsf::{parse_qsf, QsfInfo};
use crate::sim::{self, SimEvent, SimHandle};

// ── Pure state machine ──────────────────────────────────────────────────────

/// Session phase. `gen` numbers order the builds; stale results are dropped.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Phase {
    Idle,
    Compiling { gen: u64 },
    Running { gen: u64 },
    /// The old sim keeps running until the new binary is ready.
    CompilingWhileRunning { running: u64, next: u64 },
}

/// One side effect the driver must perform.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SmAction {
    /// Kill the in-flight verilator child.
    AbortCompile,
    StartCompile { gen: u64 },
    /// Gracefully end the current sim.
    QuitOldSim,
    /// Spawn the sim of this generation and replay the input state.
    SpawnSim { gen: u64 },
}

/// The rebuild-on-save state machine (plan A5).
#[derive(Debug)]
pub struct SessionSm {
    pub phase: Phase,
    next_gen: u64,
}

impl Default for SessionSm {
    fn default() -> Self {
        SessionSm {
            phase: Phase::Idle,
            next_gen: 1,
        }
    }
}

impl SessionSm {
    fn fresh_gen(&mut self) -> u64 {
        let g = self.next_gen;
        self.next_gen += 1;
        g
    }

    /// A save happened (editor hook or fs watcher).
    pub fn on_sources_changed(&mut self) -> Vec<SmAction> {
        match self.phase {
            Phase::Idle => {
                let g = self.fresh_gen();
                self.phase = Phase::Compiling { gen: g };
                vec![SmAction::StartCompile { gen: g }]
            }
            Phase::Compiling { .. } => {
                let g = self.fresh_gen();
                self.phase = Phase::Compiling { gen: g };
                vec![SmAction::AbortCompile, SmAction::StartCompile { gen: g }]
            }
            Phase::Running { gen } => {
                let g = self.fresh_gen();
                self.phase = Phase::CompilingWhileRunning {
                    running: gen,
                    next: g,
                };
                vec![SmAction::StartCompile { gen: g }]
            }
            Phase::CompilingWhileRunning { running, .. } => {
                let g = self.fresh_gen();
                self.phase = Phase::CompilingWhileRunning {
                    running,
                    next: g,
                };
                vec![SmAction::AbortCompile, SmAction::StartCompile { gen: g }]
            }
        }
    }

    /// A compile task finished. Stale generations are dropped.
    pub fn on_compile_finished(&mut self, gen: u64, ok: bool) -> Vec<SmAction> {
        match self.phase {
            Phase::Compiling { gen: g } if g == gen => {
                if ok {
                    self.phase = Phase::Running { gen };
                    vec![SmAction::SpawnSim { gen }]
                } else {
                    self.phase = Phase::Idle;
                    vec![]
                }
            }
            Phase::CompilingWhileRunning { running, next } if next == gen => {
                if ok {
                    self.phase = Phase::Running { gen };
                    vec![SmAction::QuitOldSim, SmAction::SpawnSim { gen }]
                } else {
                    // Keep the old sim running; only show the diagnostics.
                    self.phase = Phase::Running { gen: running };
                    vec![]
                }
            }
            _ => vec![],
        }
    }

    /// The current sim child ended on its own.
    pub fn on_sim_exited(&mut self) -> Vec<SmAction> {
        match self.phase {
            Phase::Running { .. } => {
                self.phase = Phase::Idle;
                vec![]
            }
            Phase::CompilingWhileRunning { next, .. } => {
                self.phase = Phase::Compiling { gen: next };
                vec![]
            }
            _ => vec![],
        }
    }
}

/// Logical input state, replayed into a fresh sim after a hot swap.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InputState {
    /// `true` = the switch is in the ON position.
    pub dip: [bool; 5],
    /// `true` = the button is held down.
    pub pb: [bool; 4],
    pub paused: bool,
    pub slow_mo: bool,
}

impl Default for InputState {
    fn default() -> Self {
        InputState {
            dip: [false; 5],
            pb: [false; 4],
            paused: false,
            slow_mo: false,
        }
    }
}

impl InputState {
    /// The command list that restores this state in a fresh sim.
    pub fn replay_commands(&self) -> Vec<SimCommand> {
        let mut out = vec![SimCommand::SetAllDip {
            mask: dip_mask(&self.dip),
        }];
        for (i, &pressed) in self.pb.iter().enumerate() {
            if pressed {
                out.push(SimCommand::SetPb {
                    index: i as u8,
                    level: pb_level(pressed),
                });
            }
        }
        if self.slow_mo {
            out.push(SimCommand::Rate { hz: 1 });
        }
        if self.paused {
            out.push(SimCommand::Pause);
        }
        out
    }
}

// ── Async driver ────────────────────────────────────────────────────────────

/// Everything one build produced, sent back to the driver loop.
struct CompileOutcome {
    gen: u64,
    result: HwBuildResult,
}

/// Run the hardware session until `stop` fires or the command channel
/// closes. Events flow to `events`; the caller wraps them for the app.
pub async fn run_session(
    project_root: PathBuf,
    mut commands: mpsc::UnboundedReceiver<HwCommand>,
    events: mpsc::UnboundedSender<HwEvent>,
    mut stop: oneshot::Receiver<()>,
) {
    let board = dk_dev_10m50a();
    let mut sm = SessionSm::default();
    let mut input = InputState::default();
    let mut sim: Option<SimHandle> = None;
    let mut compile_abort: Option<oneshot::Sender<()>> = None;
    let mut binary: Option<PathBuf> = None;
    let mut last_lit_mask: u8 = 0;
    let mut flashing = false;
    // The file the schematic follows. `None` = the board's top module. The
    // module name is resolved from the file at each use, never cached: the
    // file can be empty or broken at tab-switch time, and a later save must
    // still land the schematic on the module the save introduced.
    let mut schematic_path: Option<PathBuf> = None;
    // `true` while the schematic shows the gate-level (Yosys) view; every
    // target change and successful build then refreshes the gate graph too.
    let mut schematic_gates = false;

    let (done_tx, mut done_rx) = mpsc::unbounded_channel::<CompileOutcome>();
    let (flash_tx, mut flash_rx) = mpsc::unbounded_channel::<bool>();

    // First build. A missing verilator is reported and re-checked on save.
    let mut actions = sm.on_sources_changed();
    perform_actions(
        &mut actions, &project_root, &board, &events, &done_tx, &mut sim, &mut compile_abort,
        &mut binary, &input, schematic_path.as_deref().and_then(compile::module_for_file),
    );

    loop {
        tokio::select! {
            _ = &mut stop => {
                if let Some(abort) = compile_abort.take() { let _ = abort.send(()); }
                if let Some(s) = sim.take() { sim::shutdown_sim(s); }
                return;
            }
            cmd = commands.recv() => {
                let Some(cmd) = cmd else {
                    if let Some(abort) = compile_abort.take() { let _ = abort.send(()); }
                    if let Some(s) = sim.take() { sim::shutdown_sim(s); }
                    return;
                };
                match cmd {
                    HwCommand::SetPb(i, pressed) if i < 4 => {
                        input.pb[i] = pressed;
                        if let Some(s) = &sim {
                            s.send(SimCommand::SetPb { index: i as u8, level: pb_level(pressed) });
                        }
                    }
                    HwCommand::SetDip(i, on) if i < 5 => {
                        input.dip[i] = on;
                        if let Some(s) = &sim {
                            s.send(SimCommand::SetDip { index: i as u8, level: dip_level(on) });
                        }
                    }
                    HwCommand::Pause => {
                        input.paused = true;
                        if let Some(s) = &sim { s.send(SimCommand::Pause); }
                    }
                    HwCommand::Resume => {
                        input.paused = false;
                        if let Some(s) = &sim { s.send(SimCommand::Resume); }
                    }
                    HwCommand::Step(n) => {
                        input.paused = true;
                        if let Some(s) = &sim { s.send(SimCommand::Step { cycles: n }); }
                    }
                    HwCommand::SetSlowMo(on) => {
                        input.slow_mo = on;
                        if let Some(s) = &sim {
                            s.send(SimCommand::Rate { hz: if on { 1 } else { 0 } });
                        }
                    }
                    HwCommand::Recompile => {
                        let mut actions = sm.on_sources_changed();
                        perform_actions(
                            &mut actions, &project_root, &board, &events, &done_tx, &mut sim,
                            &mut compile_abort, &mut binary, &input,
                            schematic_path.as_deref().and_then(compile::module_for_file),
                        );
                    }
                    HwCommand::SchematicFor(path) => {
                        // A tab switch never rebuilds. It only re-dumps the
                        // graph, which is one verilator parse.
                        if path != schematic_path {
                            schematic_path = path;
                            let top = schematic_path.as_deref().and_then(compile::module_for_file);
                            let root = project_root.clone();
                            let events2 = events.clone();
                            let gates = schematic_gates;
                            tokio::spawn(async move {
                                emit_schematic(&root, top.as_deref(), &events2).await;
                                if gates {
                                    emit_gates(&root, top.as_deref(), &events2).await;
                                }
                            });
                        }
                    }
                    HwCommand::SchematicGates(on) => {
                        schematic_gates = on;
                        if on {
                            let top = schematic_path.as_deref().and_then(compile::module_for_file);
                            let root = project_root.clone();
                            let events = events.clone();
                            tokio::spawn(async move {
                                emit_gates(&root, top.as_deref(), &events).await;
                            });
                        }
                    }
                    HwCommand::Flash => {
                        // Never concurrent with a compile or another flash.
                        let compiling = !matches!(sm.phase, Phase::Idle | Phase::Running { .. });
                        if flashing || compiling {
                            let _ = events.send(HwEvent::CompileOutput(
                                "[jade] flash skipped: a build is in progress".into(),
                            ));
                        } else {
                            flashing = true;
                            if let Some(s) = &sim { s.send(SimCommand::Pause); }
                            spawn_flash(&project_root, events.clone(), flash_tx.clone());
                        }
                    }
                    _ => {}
                }
            }
            Some(outcome) = done_rx.recv() => {
                let CompileOutcome { gen, result } = outcome;
                let current = matches!(sm.phase, Phase::Compiling { gen: g } if g == gen)
                    || matches!(sm.phase, Phase::CompilingWhileRunning { next, .. } if next == gen);
                if result.canceled || !current {
                    continue;
                }
                let _ = events.send(HwEvent::Diagnostics(result.errors.clone()));
                let _ = events.send(HwEvent::CompileDone { ok: result.success });
                if let Some(bin) = &result.binary {
                    binary = Some(bin.clone());
                }
                let mut actions = sm.on_compile_finished(gen, result.success);
                perform_actions(
                    &mut actions, &project_root, &board, &events, &done_tx, &mut sim,
                    &mut compile_abort, &mut binary, &input,
                    schematic_path.as_deref().and_then(compile::module_for_file),
                );
                // A successful build refreshes the gate view when it is on.
                if result.success && schematic_gates {
                    let top = schematic_path.as_deref().and_then(compile::module_for_file);
                    let root = project_root.clone();
                    let events = events.clone();
                    tokio::spawn(async move {
                        emit_gates(&root, top.as_deref(), &events).await;
                    });
                }
            }
            Some(ok) = flash_rx.recv() => {
                flashing = false;
                let _ = events.send(HwEvent::FlashDone { ok });
                if !input.paused {
                    if let Some(s) = &sim { s.send(SimCommand::Resume); }
                }
            }
            ev = recv_sim(&mut sim) => {
                match ev {
                    SimEvent::Line(line) => {
                        handle_sim_line(line, &board, &events, &mut input, &mut last_lit_mask);
                    }
                    SimEvent::Stderr(l) => { let _ = events.send(HwEvent::CompileOutput(l)); }
                    SimEvent::Exited(code) => {
                        sim = None;
                        if code != 0 {
                            let _ = events.send(HwEvent::SimExited { reason: "signal".into() });
                        }
                        let mut actions = sm.on_sim_exited();
                        perform_actions(
                            &mut actions, &project_root, &board, &events, &done_tx, &mut sim,
                            &mut compile_abort, &mut binary, &input,
                            schematic_path.as_deref().and_then(compile::module_for_file),
                        );
                    }
                }
            }
        }
    }
}

/// Await the next sim event; pend forever while no sim runs.
async fn recv_sim(sim: &mut Option<SimHandle>) -> SimEvent {
    match sim {
        Some(s) => match s.events.recv().await {
            Some(ev) => ev,
            // A closed channel means the task ended without an exit event.
            None => SimEvent::Exited(-1),
        },
        None => std::future::pending().await,
    }
}

/// Translate one raw protocol line into logical UI events.
fn handle_sim_line(
    line: SimLine,
    board: &BoardDef,
    events: &mpsc::UnboundedSender<HwEvent>,
    input: &mut InputState,
    last_lit_mask: &mut u8,
) {
    match line {
        SimLine::Hello { top, .. } => {
            let _ = events.send(HwEvent::PortsDiscovered { top });
        }
        SimLine::Ports(_) => {}
        SimLine::Led { mask, .. } => {
            let lit = led_mask_to_lit(mask, board.led_count);
            *last_lit_mask = lit;
            let mut duty = [0.0f32; 5];
            for (i, d) in duty.iter_mut().enumerate() {
                *d = if lit & (1 << i) != 0 { 1.0 } else { 0.0 };
            }
            let _ = events.send(HwEvent::LedFrame { bitmask: lit, duty });
        }
        SimLine::Duty { duty, .. } => {
            let lit = duty_to_lit(duty);
            let mut mask = 0u8;
            for (i, d) in lit.iter().enumerate() {
                if *d >= 0.5 {
                    mask |= 1 << i;
                }
            }
            *last_lit_mask = mask;
            let _ = events.send(HwEvent::LedFrame { bitmask: mask, duty: lit });
        }
        SimLine::Rate { t_ns, cycles_per_sec, slow_x1000 } => {
            let _ = events.send(HwEvent::SimRate {
                sim_time_ns: t_ns,
                achieved_hz: cycles_per_sec,
                slowdown: slow_x1000 as f64 / 1000.0,
            });
        }
        SimLine::Clk { level, .. } => {
            let _ = events.send(HwEvent::ClockEdge { level });
        }
        SimLine::State(s) => {
            let state = match s {
                SimRunState::Running => HwRunState::Running,
                SimRunState::Paused => HwRunState::Paused,
                SimRunState::Stepped => HwRunState::Stepped,
            };
            input.paused = state != HwRunState::Running;
            let _ = events.send(HwEvent::RunState(state));
        }
        SimLine::Bye(reason) => {
            let _ = events.send(HwEvent::SimExited { reason });
        }
    }
}

/// Execute the side effects the state machine asked for.
#[allow(clippy::too_many_arguments)]
fn perform_actions(
    actions: &mut Vec<SmAction>,
    project_root: &Path,
    board: &BoardDef,
    events: &mpsc::UnboundedSender<HwEvent>,
    done_tx: &mpsc::UnboundedSender<CompileOutcome>,
    sim: &mut Option<SimHandle>,
    compile_abort: &mut Option<oneshot::Sender<()>>,
    binary: &mut Option<PathBuf>,
    input: &InputState,
    schematic_top: Option<String>,
) {
    for action in actions.drain(..) {
        match action {
            SmAction::AbortCompile => {
                if let Some(abort) = compile_abort.take() {
                    let _ = abort.send(());
                }
            }
            SmAction::StartCompile { gen } => {
                let (abort_tx, abort_rx) = oneshot::channel();
                *compile_abort = Some(abort_tx);
                let root = project_root.to_path_buf();
                let board = board.clone();
                let events = events.clone();
                let done = done_tx.clone();
                let sch = schematic_top.clone();
                tokio::spawn(async move {
                    let _ = events.send(HwEvent::CompileStarted);
                    let result =
                        build_once(&root, &board, events.clone(), abort_rx, sch.as_deref()).await;
                    let _ = done.send(CompileOutcome { gen, result });
                });
            }
            SmAction::QuitOldSim => {
                if let Some(s) = sim.take() {
                    sim::shutdown_sim(s);
                }
            }
            SmAction::SpawnSim { .. } => {
                let Some(bin) = binary.clone() else { continue };
                match sim::spawn_sim(&bin, project_root) {
                    Ok(handle) => {
                        for cmd in input.replay_commands() {
                            handle.send(cmd);
                        }
                        *sim = Some(handle);
                    }
                    Err(e) => {
                        let _ = events.send(HwEvent::CompileOutput(format!(
                            "[jade] failed to start the simulator: {e}"
                        )));
                    }
                }
            }
        }
    }
}

/// Run `make load` in the project root and stream its output. The session
/// pauses the sim first and resumes it when the flash completes.
fn spawn_flash(
    project_root: &Path,
    events: mpsc::UnboundedSender<HwEvent>,
    done: mpsc::UnboundedSender<bool>,
) {
    use tokio::io::{AsyncBufReadExt, BufReader};
    let root = project_root.to_path_buf();
    tokio::spawn(async move {
        let _ = events.send(HwEvent::CompileOutput("[jade] make load".into()));
        let child = tokio::process::Command::new("make")
            .arg("load")
            .current_dir(&root)
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn();
        let mut child = match child {
            Ok(c) => c,
            Err(e) => {
                let _ = events.send(HwEvent::CompileOutput(format!(
                    "[jade] failed to run make: {e}"
                )));
                let _ = done.send(false);
                return;
            }
        };
        let mut so = BufReader::new(child.stdout.take().expect("piped")).lines();
        let mut se = BufReader::new(child.stderr.take().expect("piped")).lines();
        let (mut so_done, mut se_done) = (false, false);
        while !(so_done && se_done) {
            tokio::select! {
                line = so.next_line(), if !so_done => match line {
                    Ok(Some(l)) => { let _ = events.send(HwEvent::CompileOutput(l)); }
                    _ => so_done = true,
                },
                line = se.next_line(), if !se_done => match line {
                    Ok(Some(l)) => { let _ = events.send(HwEvent::CompileOutput(l)); }
                    _ => se_done = true,
                },
            }
        }
        let ok = child.wait().await.map(|s| s.success()).unwrap_or(false);
        let _ = done.send(ok);
    });
}

/// One full build: qsf → sources → ports → mapping → wrapper → verilator.
async fn build_once(
    project_root: &Path,
    board: &BoardDef,
    events: mpsc::UnboundedSender<HwEvent>,
    abort: oneshot::Receiver<()>,
    schematic_top: Option<&str>,
) -> HwBuildResult {
    let started = std::time::Instant::now();

    // Tool check, repeated on every save while the tool is missing.
    let verilator = match compile::detect_verilator() {
        Ok((path, _version)) => path,
        Err(version_found) => {
            let _ = events.send(HwEvent::ToolMissing {
                tool: "verilator".into(),
                version_found,
                install_hint: "brew install verilator".into(),
            });
            return HwBuildResult::failed(vec![], started);
        }
    };

    // Project inputs.
    let qsf = find_qsf(project_root);
    let qsf_path = qsf.as_ref().map(|q| q.path.clone());
    let sources = compile::source_list(project_root, qsf.as_ref());
    if sources.is_empty() {
        let e = BuildError {
            file: project_root.join("."),
            line: 1,
            column: 1,
            message: "no Verilog sources found (*.v, tb_* excluded)".into(),
            severity: Severity::Error,
        };
        return HwBuildResult::failed(vec![e], started);
    }
    let top = qsf
        .as_ref()
        .and_then(|q| q.top.as_ref().map(|(t, _)| t.clone()))
        .unwrap_or_else(|| guess_top_from_sources(&sources));

    // Artifact directory.
    let hw_dir = project_root.join(".jade/hw");
    if let Err(e) = tokio::fs::create_dir_all(&hw_dir).await {
        let e = BuildError {
            file: hw_dir.clone(),
            line: 1,
            column: 1,
            message: format!("cannot create {}: {e}", hw_dir.display()),
            severity: Severity::Error,
        };
        return HwBuildResult::failed(vec![e], started);
    }

    // Port discovery through verilator's parser.
    let ports = match ports::discover_ports(&verilator, &top, &sources, &hw_dir, project_root).await
    {
        Ok(p) => p,
        Err(msg) => {
            // The pre-pass fails on the same syntax errors as the build;
            // parse its stderr so the user gets clickable diagnostics.
            let mut errors = compile::parse_verilator_errors(&msg);
            if errors.is_empty() {
                errors.push(BuildError {
                    file: sources[0].clone(),
                    line: 1,
                    column: 1,
                    message: msg,
                    severity: Severity::Error,
                });
            }
            return HwBuildResult::failed(errors, started);
        }
    };

    // The schematic graph rides on the same pre-pass dump (§schematic). When
    // the app selected another module, that module gets its own dump: the
    // build's dump holds only what the board's top module reaches.
    match schematic_top {
        Some(sel) if sel != top => emit_schematic(project_root, Some(sel), &events).await,
        _ => {
            if let Ok(text) = tokio::fs::read_to_string(hw_dir.join("ports.tree.json")).await {
                if let Some(nl) = crate::netlist::parse_netlist_json(&text, &top) {
                    let _ = events.send(HwEvent::Netlist(nl));
                }
            }
        }
    }

    // Board mapping with precise diagnostics.
    let mapping = match mapping::resolve(qsf.as_ref(), &ports, board) {
        Ok(m) => m,
        Err(errors) => return HwBuildResult::failed(errors, started),
    };
    let warnings = mapping.warnings.clone();

    // Write the wrapper and the fixed harness.
    let wrapper_path = hw_dir.join("jade_hw_top.v");
    let harness_path = hw_dir.join("harness.cpp");
    let wrapper_text = crate::gen::generate_wrapper(&mapping, &ports);
    if let Err(e) = tokio::fs::write(&wrapper_path, &wrapper_text).await {
        let e = BuildError {
            file: wrapper_path.clone(),
            line: 1,
            column: 1,
            message: format!("cannot write the wrapper: {e}"),
            severity: Severity::Error,
        };
        return HwBuildResult::failed(vec![e], started);
    }
    // Write the harness only on change, so make can cache the object file.
    let harness_current = tokio::fs::read_to_string(&harness_path)
        .await
        .map(|t| t == compile::HARNESS_CPP)
        .unwrap_or(false);
    if !harness_current {
        if let Err(e) = tokio::fs::write(&harness_path, compile::HARNESS_CPP).await {
            let e = BuildError {
                file: harness_path.clone(),
                line: 1,
                column: 1,
                message: format!("cannot write the harness: {e}"),
                severity: Severity::Error,
            };
            return HwBuildResult::failed(vec![e], started);
        }
    }

    let (line_tx, mut line_rx) = mpsc::unbounded_channel::<String>();
    let fwd_events = events.clone();
    let forwarder = tokio::spawn(async move {
        while let Some(l) = line_rx.recv().await {
            let _ = fwd_events.send(HwEvent::CompileOutput(l));
        }
    });

    let mut result = compile::compile(
        &verilator,
        project_root,
        &hw_dir,
        &top,
        &wrapper_path,
        &sources,
        &harness_path,
        qsf_path.as_deref(),
        line_tx,
        abort,
    )
    .await;
    let _ = forwarder.await;
    // Mapping warnings ride along with the compiler diagnostics.
    result.errors.extend(warnings);
    result
}

/// Choose the `.qsf` in the project root. When more than one exists, the one
/// whose stem matches the directory name wins, else the first by name.
/// Dump the RTL graph of `top` and send it to the schematic view. `None`
/// selects the board's top module.
///
/// The dump is its own verilator pass over every source in the project, not
/// only the `.qsf` file list, so the schematic can show a module the board
/// build leaves out. It writes beside the build's dump, never over it.
///
/// A failure is silent on purpose: a file that does not elaborate yet leaves
/// the last good graph on screen.
async fn emit_schematic(
    project_root: &Path,
    top: Option<&str>,
    events: &mpsc::UnboundedSender<HwEvent>,
) {
    let Ok((verilator, _)) = compile::detect_verilator() else {
        return;
    };
    let qsf = find_qsf(project_root);
    let sources = compile::schematic_source_list(project_root, qsf.as_ref());
    if sources.is_empty() {
        return;
    }
    let top = match top {
        Some(t) => t.to_string(),
        None => qsf
            .as_ref()
            .and_then(|q| q.top.as_ref().map(|(t, _)| t.clone()))
            .unwrap_or_else(|| guess_top_from_sources(&sources)),
    };
    let hw_dir = project_root.join(".jade/hw");
    if tokio::fs::create_dir_all(&hw_dir).await.is_err() {
        return;
    }
    let tree = hw_dir.join("schematic.tree.json");
    let meta = hw_dir.join("schematic.meta.json");
    let Ok(text) =
        ports::run_json_only(&verilator, &top, &sources, &tree, &meta, project_root).await
    else {
        return;
    };
    if let Some(nl) = crate::netlist::parse_netlist_json(&text, &top) {
        let _ = events.send(HwEvent::Netlist(nl));
    }
}

/// Synthesize the gate-level graph of `top` with Yosys and send it to the
/// schematic view. `None` selects the board's top module.
///
/// A missing Yosys reports back, so the view can show the install hint. A
/// synthesis failure is silent, like the RTL dump: a save that does not
/// elaborate keeps the last good graph on screen.
async fn emit_gates(
    project_root: &Path,
    top: Option<&str>,
    events: &mpsc::UnboundedSender<HwEvent>,
) {
    let Some(yosys) = crate::synth::detect_yosys() else {
        let _ = events.send(HwEvent::SchematicSynthMissing {
            install_hint: "brew install yosys".into(),
        });
        return;
    };
    let qsf = find_qsf(project_root);
    let sources = compile::schematic_source_list(project_root, qsf.as_ref());
    if sources.is_empty() {
        return;
    }
    let top = match top {
        Some(t) => t.to_string(),
        None => qsf
            .as_ref()
            .and_then(|q| q.top.as_ref().map(|(t, _)| t.clone()))
            .unwrap_or_else(|| guess_top_from_sources(&sources)),
    };
    let hw_dir = project_root.join(".jade/hw");
    if tokio::fs::create_dir_all(&hw_dir).await.is_err() {
        return;
    }
    let out = hw_dir.join("gates.json");
    let Ok(text) = crate::synth::run_yosys(&yosys, &top, &sources, &out, project_root).await
    else {
        return;
    };
    if let Some(nl) = crate::synth::parse_yosys_json(&text, &top) {
        let _ = events.send(HwEvent::GateNetlist(nl));
    }
}

fn find_qsf(project_root: &Path) -> Option<QsfInfo> {
    let mut qsfs: Vec<PathBuf> = std::fs::read_dir(project_root)
        .ok()?
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|x| x == "qsf"))
        .collect();
    qsfs.sort();
    let dir_name = project_root.file_name().map(|s| s.to_string_lossy().into_owned());
    let chosen = qsfs
        .iter()
        .find(|p| {
            p.file_stem().map(|s| s.to_string_lossy().into_owned()) == dir_name
        })
        .or_else(|| qsfs.first())?;
    let text = std::fs::read_to_string(chosen).ok()?;
    Some(parse_qsf(chosen, &text))
}

/// Without a `.qsf`, prefer a file named `top.v`, else the first source's
/// stem. Port discovery validates the choice.
fn guess_top_from_sources(sources: &[PathBuf]) -> String {
    sources
        .iter()
        .find(|p| p.file_stem().is_some_and(|s| s == "top"))
        .or(sources.first())
        .and_then(|p| p.file_stem())
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| "top".into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn save_during_compile_aborts_and_restarts() {
        let mut sm = SessionSm::default();
        let a1 = sm.on_sources_changed();
        assert_eq!(a1, vec![SmAction::StartCompile { gen: 1 }]);
        let a2 = sm.on_sources_changed();
        assert_eq!(
            a2,
            vec![SmAction::AbortCompile, SmAction::StartCompile { gen: 2 }]
        );
        // The aborted generation's result is stale and does nothing.
        assert!(sm.on_compile_finished(1, true).is_empty());
        assert_eq!(sm.phase, Phase::Compiling { gen: 2 });
        // The live generation succeeds and spawns the sim.
        assert_eq!(
            sm.on_compile_finished(2, true),
            vec![SmAction::SpawnSim { gen: 2 }]
        );
        assert_eq!(sm.phase, Phase::Running { gen: 2 });
    }

    #[test]
    fn compile_failure_keeps_the_old_sim() {
        let mut sm = SessionSm::default();
        sm.on_sources_changed();
        sm.on_compile_finished(1, true);
        assert_eq!(sm.phase, Phase::Running { gen: 1 });
        let a = sm.on_sources_changed();
        assert_eq!(a, vec![SmAction::StartCompile { gen: 2 }]);
        assert_eq!(
            sm.phase,
            Phase::CompilingWhileRunning { running: 1, next: 2 }
        );
        // The build fails: the old sim keeps running.
        assert!(sm.on_compile_finished(2, false).is_empty());
        assert_eq!(sm.phase, Phase::Running { gen: 1 });
    }

    #[test]
    fn hot_swap_quits_the_old_sim_then_spawns_the_new() {
        let mut sm = SessionSm::default();
        sm.on_sources_changed();
        sm.on_compile_finished(1, true);
        sm.on_sources_changed();
        let a = sm.on_compile_finished(2, true);
        assert_eq!(
            a,
            vec![SmAction::QuitOldSim, SmAction::SpawnSim { gen: 2 }]
        );
        assert_eq!(sm.phase, Phase::Running { gen: 2 });
    }

    #[test]
    fn sim_exit_during_background_compile_returns_to_compiling() {
        let mut sm = SessionSm::default();
        sm.on_sources_changed();
        sm.on_compile_finished(1, true);
        sm.on_sources_changed();
        sm.on_sim_exited();
        assert_eq!(sm.phase, Phase::Compiling { gen: 2 });
    }

    #[test]
    fn replay_restores_dip_pb_rate_and_pause() {
        let state = InputState {
            dip: [true, false, true, false, false],
            pb: [false, true, false, false],
            paused: true,
            slow_mo: true,
        };
        let cmds = state.replay_commands();
        assert_eq!(
            cmds,
            vec![
                SimCommand::SetAllDip { mask: 0b11010 },
                SimCommand::SetPb { index: 1, level: false },
                SimCommand::Rate { hz: 1 },
                SimCommand::Pause,
            ]
        );
    }

    #[test]
    fn default_replay_is_the_rest_state() {
        let cmds = InputState::default().replay_commands();
        assert_eq!(cmds, vec![SimCommand::SetAllDip { mask: 0b11111 }]);
    }
}
