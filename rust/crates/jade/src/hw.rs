//! Hardware-mode state and event handling (§B4).
//!
//! `HwState` is the render model for the hardware surfaces (wave panel,
//! schematic, action bar). The engine lives in the
//! `jade-hw` crate; commands go out through the session's channel and
//! [`jade_hw::HwEvent`]s come back through the unified app pump
//! (`AppEvent::Hw`). All values here are logical: `lit`, `pressed`, `on`.

use std::path::Path;

use jade_hw::{HwCommand, HwEvent, HwRunState};
use jade_lsp::{Diagnostic, DiagnosticSeverity, Position, Range};

use crate::app::{AppMode, JadeApp, ToastKind};
use crate::output::push_output;

/// Ignore a watcher `SourcesChanged` this close after an explicit editor-save
/// recompile, so one save does not build twice (plan C4).
const RECOMPILE_DEDUPE_MS: u64 = 600;

/// Render model for hardware mode.
#[derive(Debug, Clone)]
pub struct HwState {
    /// Per-LED lit fraction 0..1 (PWM brightness).
    pub led_duty: [f32; 5],
    /// Bit i set = LED i lit.
    pub led_mask: u8,
    /// `true` = the button is held down.
    pub pb: [bool; 4],
    /// `true` = the switch is in the ON position.
    pub dip: [bool; 5],
    pub sim_time_ns: u64,
    pub achieved_hz: f64,
    pub run_state: HwRunState,
    pub slow_mo: bool,
    /// `now_ms()` of the last slow-motion clock edge (drives the flash dot).
    pub last_clk_edge_ms: u64,
    /// The edge level that last arrived.
    pub clk_level: bool,
    pub compiling: bool,
    /// A flash (`make load`) is in flight.
    pub flashing: bool,
    pub ports_ready: bool,
    /// The user's top module, once discovered.
    pub top: Option<String>,
    /// Install hint while verilator is missing.
    pub tool_missing: Option<String>,
    /// The design's RTL graph, refreshed on every successful pre-pass.
    pub netlist: Option<jade_hw::netlist::Netlist>,
    /// The synthesized gate-level graph, present after the first Yosys run.
    pub gate_netlist: Option<jade_hw::netlist::Netlist>,
    /// `true` = the schematic shows the gate-level view.
    pub schematic_gates: bool,
    /// A gate synthesis runs and no gate graph exists yet.
    pub synthesizing: bool,
    /// Install hint while yosys is missing (the gate view needs it).
    pub synth_missing: Option<String>,
    /// The source file the schematic follows: the selected editor tab.
    pub schematic_path: Option<std::path::PathBuf>,
    /// What the pointer rests on in the schematic, for highlight + tooltip.
    pub schematic_hover: Option<crate::panels::schematic_view::SchematicHover>,
    /// The wave panel: testbench run, loaded dump, view transform.
    pub wave: crate::wave::WaveState,
    /// `now_ms()` of the last explicit (editor-save) recompile request.
    last_recompile_ms: u64,
}

impl Default for HwState {
    fn default() -> Self {
        HwState {
            led_duty: [0.0; 5],
            led_mask: 0,
            pb: [false; 4],
            dip: [false; 5],
            sim_time_ns: 0,
            achieved_hz: 0.0,
            run_state: HwRunState::Running,
            slow_mo: false,
            last_clk_edge_ms: 0,
            clk_level: false,
            compiling: false,
            flashing: false,
            ports_ready: false,
            top: None,
            tool_missing: None,
            netlist: None,
            gate_netlist: None,
            schematic_gates: false,
            synthesizing: false,
            synth_missing: None,
            schematic_path: None,
            schematic_hover: None,
            wave: crate::wave::WaveState::default(),
            last_recompile_ms: 0,
        }
    }
}

/// True for the file types whose save triggers a hardware recompile.
pub fn is_hdl(path: &Path) -> bool {
    matches!(
        path.extension().and_then(|e| e.to_str()),
        Some("v" | "sv" | "svh" | "vh" | "qsf" | "sdc")
    )
}

/// True for a Verilog source, the file kind the schematic can draw. Narrower
/// than [`is_hdl`], which also covers the Quartus settings files.
pub fn is_verilog(path: &Path) -> bool {
    matches!(
        path.extension().and_then(|e| e.to_str()),
        Some("v" | "sv")
    )
}

/// The first file to open in a hardware project: `top.v`, else the first `.v`
/// that declares `module top`, else the first `.v`/`.sv` by name.
pub fn first_hw_file(dir: &Path) -> Option<std::path::PathBuf> {
    let mut hdl: Vec<std::path::PathBuf> = std::fs::read_dir(dir)
        .ok()?
        .flatten()
        .map(|e| e.path())
        .filter(|p| {
            matches!(p.extension().and_then(|e| e.to_str()), Some("v" | "sv"))
        })
        .collect();
    hdl.sort();
    if let Some(top) = hdl.iter().find(|p| p.file_name().is_some_and(|n| n == "top.v")) {
        return Some(top.clone());
    }
    if let Some(with_top) = hdl.iter().find(|p| {
        std::fs::read_to_string(p)
            .map(|t| t.contains("module top"))
            .unwrap_or(false)
    }) {
        return Some(with_top.clone());
    }
    hdl.into_iter().next()
}

impl JadeApp {
    /// Send one command to the hardware session, if one is connected.
    pub fn hw_send(&self, cmd: HwCommand) {
        if let Some(tx) = self.hw_command_tx() {
            let _ = tx.send(cmd);
        }
    }

    /// Point the schematic at the selected editor tab.
    ///
    /// The render loop calls this every frame, so it sends only on a change.
    /// A tab that holds no Verilog returns the schematic to the board's top
    /// module.
    pub fn sync_schematic_target(&mut self) {
        let path = self
            .editor
            .active_tab()
            .map(|t| t.path.clone())
            .filter(|p| is_verilog(p));
        match &self.hw {
            None => return,
            Some(hw) if hw.schematic_path == path => return,
            Some(_) => {}
        }
        if let Some(hw) = &mut self.hw {
            hw.schematic_path = path.clone();
        }
        self.hw_send(HwCommand::SchematicFor(path));
    }

    /// Switch the schematic between the RTL view and the gate-level view.
    /// The gate view synthesizes with Yosys on demand.
    pub fn hw_toggle_schematic_gates(&mut self) {
        let Some(hw) = &mut self.hw else { return };
        hw.schematic_gates = !hw.schematic_gates;
        // Hover indices belong to the other graph.
        hw.schematic_hover = None;
        let on = hw.schematic_gates;
        if on {
            // Re-probe the tool on every turn-on, so a fresh install works
            // without a restart.
            hw.synth_missing = None;
            hw.synthesizing = hw.gate_netlist.is_none();
        }
        self.hw_send(HwCommand::SchematicGates(on));
    }

    /// Run/Pause toggle (⌘R / space on the board).
    pub fn hw_toggle_run(&mut self) {
        let Some(hw) = &mut self.hw else { return };
        if hw.run_state == HwRunState::Running {
            hw.run_state = HwRunState::Paused;
            self.hw_send(HwCommand::Pause);
        } else {
            hw.run_state = HwRunState::Running;
            self.hw_send(HwCommand::Resume);
        }
    }

    /// Step one clock cycle (enabled while paused).
    pub fn hw_step(&mut self) {
        let Some(hw) = &mut self.hw else { return };
        if hw.run_state == HwRunState::Running {
            return;
        }
        self.hw_send(HwCommand::Step(1));
    }

    /// Toggle slow motion (1 Hz teaching clock).
    pub fn hw_toggle_slowmo(&mut self) {
        let Some(hw) = &mut self.hw else { return };
        hw.slow_mo = !hw.slow_mo;
        let on = hw.slow_mo;
        self.hw_send(HwCommand::SetSlowMo(on));
    }

    /// "Flash board": run `make load` against the real board.
    pub fn hw_flash(&mut self) {
        let Some(hw) = &mut self.hw else { return };
        if hw.compiling || hw.flashing {
            return;
        }
        hw.flashing = true;
        self.status_line("[jade] Flashing the board (make load)...");
        self.hw_send(HwCommand::Flash);
    }

    /// Save-hook recompile (§B10): ask the session to rebuild. The fs
    /// watcher's follow-up burst is deduped by timestamp.
    ///
    /// The bottom panel stays as the user left it: a save never opens it
    /// and never changes the active tab. The status line, the toasts, and
    /// the editor pills report the result. The Output tab is one click away.
    pub fn hw_recompile(&mut self) {
        let now = self.now_ms();
        let Some(hw) = &mut self.hw else { return };
        hw.compiling = true;
        hw.last_recompile_ms = now;
        self.status_line("[jade] Rebuilding the simulation...");
        self.hw_send(HwCommand::Recompile);
        // The wave panel follows the same save: re-run the last testbench.
        self.hw_rerun_testbench();
    }

    /// Apply one engine event to the render model (the `AppEvent::Hw` arm).
    pub fn on_hw_event(&mut self, ev: HwEvent) {
        let now = self.now_ms();
        match ev {
            HwEvent::ToolMissing { tool, version_found, install_hint } => {
                let found = match version_found {
                    Some(v) => format!("found {v}, need 5.0 or newer"),
                    None => "not found".to_string(),
                };
                push_output(
                    &mut self.output,
                    &format!("[jade] {tool} is required for hardware mode ({found}). Install it with: {install_hint}"),
                );
                self.output_visible = true;
                self.bottom_view = crate::app::BottomView::Output;
                if let Some(hw) = &mut self.hw {
                    hw.tool_missing = Some(install_hint.clone());
                    hw.compiling = false;
                }
                self.push_toast(ToastKind::Error, format!("{tool} missing · {install_hint}"));
            }
            HwEvent::Netlist(nl) => {
                if let Some(hw) = &mut self.hw {
                    hw.netlist = Some(nl);
                    // The node and edge indices of a hover belong to the old
                    // graph; a fresh graph starts with no hover.
                    if !hw.schematic_gates {
                        hw.schematic_hover = None;
                    }
                }
            }
            HwEvent::GateNetlist(nl) => {
                if let Some(hw) = &mut self.hw {
                    hw.gate_netlist = Some(nl);
                    hw.synthesizing = false;
                    hw.synth_missing = None;
                    if hw.schematic_gates {
                        hw.schematic_hover = None;
                    }
                }
            }
            HwEvent::SchematicSynthMissing { install_hint } => {
                push_output(
                    &mut self.output,
                    &format!("[jade] The gate-level schematic needs yosys (not found). Install it with: {install_hint}"),
                );
                if let Some(hw) = &mut self.hw {
                    hw.synth_missing = Some(install_hint.clone());
                    hw.synthesizing = false;
                }
                self.push_toast(ToastKind::Error, format!("yosys missing · {install_hint}"));
            }
            HwEvent::PortsDiscovered { top } => {
                if let Some(hw) = &mut self.hw {
                    hw.ports_ready = true;
                    hw.tool_missing = None;
                    hw.top = Some(top);
                }
            }
            HwEvent::LedFrame { bitmask, duty } => {
                if let Some(hw) = &mut self.hw {
                    hw.led_mask = bitmask;
                    hw.led_duty = duty;
                }
            }
            HwEvent::SimRate { sim_time_ns, achieved_hz, .. } => {
                if let Some(hw) = &mut self.hw {
                    hw.sim_time_ns = sim_time_ns;
                    hw.achieved_hz = achieved_hz;
                }
            }
            HwEvent::ClockEdge { level } => {
                if let Some(hw) = &mut self.hw {
                    hw.last_clk_edge_ms = now;
                    hw.clk_level = level;
                }
            }
            HwEvent::RunState(state) => {
                if let Some(hw) = &mut self.hw {
                    hw.run_state = state;
                }
            }
            HwEvent::SourcesChanged => {
                // The fs watcher and the editor-save hook can both fire for
                // one save; the timestamp collapses the pair.
                let recent = self
                    .hw
                    .as_ref()
                    .map(|h| now.saturating_sub(h.last_recompile_ms) < RECOMPILE_DEDUPE_MS)
                    .unwrap_or(true);
                if !recent {
                    self.hw_recompile();
                }
            }
            HwEvent::CompileStarted => {
                // The bottom panel stays as the user left it (see
                // `hw_recompile`).
                if let Some(hw) = &mut self.hw {
                    hw.compiling = true;
                }
            }
            HwEvent::CompileOutput(line) => push_output(&mut self.output, &line),
            HwEvent::Diagnostics(errors) => self.on_hw_diagnostics(errors),
            HwEvent::CompileDone { ok } => {
                if let Some(hw) = &mut self.hw {
                    hw.compiling = false;
                }
                if ok {
                    self.push_toast(ToastKind::Success, "Simulation rebuilt");
                    self.status_line("[jade] Simulation rebuilt");
                } else {
                    // The toast, the status line, and the editor pills carry
                    // the failure; the bottom panel stays as the user left it.
                    self.push_toast(ToastKind::Error, "Hardware build failed");
                    self.status_line("[jade] Hardware build failed");
                }
            }
            HwEvent::SimExited { reason } => {
                if reason != "quit" {
                    self.status_line(&format!("[jade] Simulator stopped ({reason})"));
                }
            }
            HwEvent::FlashDone { ok } => {
                if let Some(hw) = &mut self.hw {
                    hw.flashing = false;
                }
                if ok {
                    self.push_toast(ToastKind::Success, "Board flashed");
                } else {
                    self.push_toast(ToastKind::Error, "Flash failed");
                }
            }
        }
    }

    /// Route hardware diagnostics like `on_build_done` routes C++ ones: into
    /// the OUTPUT pane (clickable `path:line:col:` lines) and into the open
    /// tabs' diagnostics vecs so the action-bar pills work unmodified.
    pub(crate) fn on_hw_diagnostics(&mut self, errors: Vec<jade_build::BuildError>) {
        for e in &errors {
            let tag = match e.severity {
                jade_build::Severity::Error => "error",
                jade_build::Severity::Warning => "warning",
                jade_build::Severity::Note => "note",
            };
            push_output(
                &mut self.output,
                &format!(
                    "  {}:{}:{}: {tag}: {}",
                    e.file.display(),
                    e.line,
                    e.column,
                    e.message
                ),
            );
        }
        // Every open tab gets the diagnostics that name its file; tabs with
        // none are cleared, so a fixed file loses its pills.
        for tab in &mut self.editor.tabs {
            if !is_hdl(&tab.path) {
                continue;
            }
            tab.diagnostics = errors
                .iter()
                .filter(|e| paths_match(&e.file, &tab.path))
                .map(|e| Diagnostic {
                    severity: Some(match e.severity {
                        jade_build::Severity::Error => DiagnosticSeverity::ERROR,
                        jade_build::Severity::Warning => DiagnosticSeverity::WARNING,
                        jade_build::Severity::Note => DiagnosticSeverity::INFORMATION,
                    }),
                    range: Range::new(
                        Position::new(e.line.saturating_sub(1), e.column.saturating_sub(1)),
                        Position::new(e.line.saturating_sub(1), e.column),
                    ),
                    message: e.message.clone(),
                    ..Default::default()
                })
                .collect();
        }
    }

    /// Enter hardware mode for the current workspace: create the render state,
    /// restore DIP positions, connect the engine session, and start the
    /// source watch. Idempotent.
    pub fn start_hw_mode(&mut self) {
        if self.hw.is_some() {
            return;
        }
        self.mode = AppMode::Hardware;
        let ui = crate::workspace_state::load(&self.workspace_root);
        let mut state = HwState::default();
        for (i, on) in ui.dip_switches.iter().take(5).enumerate() {
            state.dip[i] = *on;
        }
        if let Some(w) = ui.board_width {
            self.wave_width = (w as f32).clamp(crate::wave::MIN_W, crate::wave::MAX_W);
        }
        self.hw = Some(state);
        self.sidebar_tab = crate::app::SidebarTab::Files;

        // Connect the engine unless a test channel is injected.
        self.hw_connect_engine();
        // Replay restored DIP positions into the session.
        let dips = self.hw.as_ref().map(|h| h.dip).unwrap_or_default();
        for (i, on) in dips.iter().enumerate() {
            if *on {
                self.hw_send(HwCommand::SetDip(i, true));
            }
        }
        self.hw_start_watch();
        // Open on the last dump, so the panel is not an empty sheet.
        self.hw_load_existing_wave();
    }

    /// Leave hardware mode: stop the session, drop the watch and the state.
    pub fn stop_hw_mode(&mut self) {
        self.mode = AppMode::Software;
        self.hw = None;
        self.hw_stop_engine();
        self.hw_drop_watch();
    }
}

/// Compare a diagnostic path to a tab path: exact match, or the same file
/// name when the diagnostic is relative.
fn paths_match(diag: &Path, tab: &Path) -> bool {
    if diag == tab {
        return true;
    }
    if diag.is_relative() {
        return diag.file_name().is_some() && tab.file_name() == diag.file_name();
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn first_hw_file_prefers_top_module() {
        let dir = std::env::temp_dir().join(format!("jade_hw_first_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("alpha.v"), "module alpha; endmodule\n").unwrap();
        std::fs::write(dir.join("zeta.v"), "module top; endmodule\n").unwrap();
        // A file that declares `module top` wins over alphabetical order.
        assert_eq!(first_hw_file(&dir), Some(dir.join("zeta.v")));
        // An actual top.v wins over everything.
        std::fs::write(dir.join("top.v"), "module blink; endmodule\n").unwrap();
        assert_eq!(first_hw_file(&dir), Some(dir.join("top.v")));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn is_hdl_covers_sources_and_constraints() {
        assert!(is_hdl(Path::new("blink.v")));
        assert!(is_hdl(Path::new("top.sv")));
        assert!(is_hdl(Path::new("blink.qsf")));
        assert!(is_hdl(Path::new("blink.sdc")));
        assert!(!is_hdl(Path::new("main.cpp")));
        assert!(!is_hdl(Path::new("README.md")));
    }
}
