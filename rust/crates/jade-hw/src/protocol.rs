//! The `__JADE_HW|` line protocol between the simulator child and the IDE,
//! plus the public event/command types that the UI consumes.
//!
//! The wire protocol carries raw electrical levels. This module also holds
//! the normalization to logical values: an LED is lit when the pin is LOW, a
//! push button reads LOW when you push it, and a DIP switch reads LOW in the
//! ON position.

use jade_build::BuildError;

pub const PROTOCOL_VERSION: u32 = 1;

/// One parsed simulator → IDE line. Raw electrical levels.
#[derive(Debug, Clone, PartialEq)]
pub enum SimLine {
    Hello { version: u32, top: String },
    /// The fixed wrapper port list, verbatim.
    Ports(String),
    /// Raw pin levels for `led[4:0]`, sent on change.
    Led { t_ns: u64, mask: u8 },
    /// Per-LED high-time 0..255 over the last 33 ms window.
    Duty { t_ns: u64, duty: [u8; 5] },
    /// Achieved simulation rate, sent at 1 Hz. `slow_x1000` = 1000 at real time.
    Rate { t_ns: u64, cycles_per_sec: f64, slow_x1000: u64 },
    /// A clock edge, sent only when the commanded rate is 10 Hz or less.
    Clk { t_ns: u64, level: bool },
    State(SimRunState),
    Bye(String),
}

/// Simulator run state as reported over the wire.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SimRunState {
    Running,
    Paused,
    Stepped,
}

/// Parse one stdout line from the simulator. Malformed and unknown lines
/// return `None`; the caller swallows them.
pub fn parse_hw_line(line: &str) -> Option<SimLine> {
    let rest = line.trim().strip_prefix("__JADE_HW|")?;
    let parts: Vec<&str> = rest.split('|').collect();
    match parts.first()? {
        &"HELLO" if parts.len() >= 3 => Some(SimLine::Hello {
            version: parts[1].parse().ok()?,
            top: parts[2].to_string(),
        }),
        &"PORTS" if parts.len() >= 2 => Some(SimLine::Ports(parts[1].to_string())),
        &"LED" if parts.len() >= 3 => Some(SimLine::Led {
            t_ns: parts[1].parse().ok()?,
            mask: parts[2].parse().ok()?,
        }),
        &"DUTY" if parts.len() >= 3 => {
            let t_ns = parts[1].parse().ok()?;
            let vals: Vec<u8> = parts[2]
                .split(',')
                .map(|v| v.parse::<u8>())
                .collect::<Result<_, _>>()
                .ok()?;
            if vals.len() != 5 {
                return None;
            }
            let mut duty = [0u8; 5];
            duty.copy_from_slice(&vals);
            Some(SimLine::Duty { t_ns, duty })
        }
        &"RATE" if parts.len() >= 4 => Some(SimLine::Rate {
            t_ns: parts[1].parse().ok()?,
            cycles_per_sec: parts[2].parse().ok()?,
            slow_x1000: parts[3].parse().ok()?,
        }),
        &"CLK" if parts.len() >= 3 => Some(SimLine::Clk {
            t_ns: parts[1].parse().ok()?,
            level: match parts[2] {
                "0" => false,
                "1" => true,
                _ => return None,
            },
        }),
        &"STATE" if parts.len() >= 2 => Some(SimLine::State(match parts[1] {
            "running" => SimRunState::Running,
            "paused" => SimRunState::Paused,
            "stepped" => SimRunState::Stepped,
            _ => return None,
        })),
        &"BYE" if parts.len() >= 2 => Some(SimLine::Bye(parts[1].to_string())),
        _ => None,
    }
}

/// One IDE → simulator stdin command. Raw electrical levels.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum SimCommand {
    /// Set the electrical level of `pb[i]`.
    SetPb { index: u8, level: bool },
    /// Set the electrical level of `dipsw[i]`.
    SetDip { index: u8, level: bool },
    /// Set all five DIP levels at once; used for replay after a hot swap.
    SetAllDip { mask: u8 },
    Pause,
    Resume,
    Step { cycles: u64 },
    /// 0 = real-time 50 MHz, 1..n = that many cycles per second, -1 = maximum.
    Rate { hz: i64 },
    Quit,
}

/// Format one command as a stdin line, without the newline.
pub fn format_command(cmd: &SimCommand) -> String {
    match cmd {
        SimCommand::SetPb { index, level } => format!("SET|PB|{index}|{}", *level as u8),
        SimCommand::SetDip { index, level } => format!("SET|DIPSW|{index}|{}", *level as u8),
        SimCommand::SetAllDip { mask } => format!("SETALL|DIPSW|{mask}"),
        SimCommand::Pause => "PAUSE".to_string(),
        SimCommand::Resume => "RESUME".to_string(),
        SimCommand::Step { cycles } => format!("STEP|{cycles}"),
        SimCommand::Rate { hz } => format!("RATE|{hz}"),
        SimCommand::Quit => "QUIT".to_string(),
    }
}

// ── Public UI-facing types ──────────────────────────────────────────────────

/// Logical run state for the UI.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum HwRunState {
    #[default]
    Running,
    Paused,
    Stepped,
}

/// Events from the hardware session to the app. All values are logical:
/// `lit`, `pressed`, and `on` are already normalized from electrical levels.
#[derive(Debug, Clone, PartialEq)]
pub enum HwEvent {
    /// A required tool is absent or too old.
    ToolMissing {
        tool: String,
        version_found: Option<String>,
        install_hint: String,
    },
    /// Port discovery completed; the top module name is known.
    PortsDiscovered { top: String },
    /// The design's RTL graph for the schematic view, fresh on every build.
    Netlist(crate::netlist::Netlist),
    /// The synthesized gate-level graph for the schematic's gate view.
    GateNetlist(crate::netlist::Netlist),
    /// The gate view needs Yosys, and no usable install was found.
    SchematicSynthMissing { install_hint: String },
    /// Logical LED frame: bit i set = LED i lit; duty 0..1 = lit fraction.
    LedFrame { bitmask: u8, duty: [f32; 5] },
    /// Achieved simulation rate. `slowdown` = 1.0 at real time.
    SimRate {
        sim_time_ns: u64,
        achieved_hz: f64,
        slowdown: f64,
    },
    /// A clock edge in slow-motion mode.
    ClockEdge { level: bool },
    RunState(HwRunState),
    /// Source files changed on disk (from the fs watcher).
    SourcesChanged,
    CompileStarted,
    /// One line of compile or flash output for the output pane.
    CompileOutput(String),
    Diagnostics(Vec<BuildError>),
    CompileDone { ok: bool },
    /// The simulator child ended: `eof`, `quit`, or `signal`.
    SimExited { reason: String },
    /// A flash (`make load`) run completed.
    FlashDone { ok: bool },
}

/// Commands from the app to the hardware session. All values are logical.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HwCommand {
    /// `pressed = true` when the pointer holds the button down.
    SetPb(usize, bool),
    /// `on = true` when the switch is in the ON position.
    SetDip(usize, bool),
    Pause,
    Resume,
    Step(u64),
    SetSlowMo(bool),
    Recompile,
    Flash,
    /// Point the schematic at this source file. The session reads the module
    /// the file declares and dumps that module's graph. `None` returns the
    /// schematic to the board's top module.
    SchematicFor(Option<std::path::PathBuf>),
    /// Turn the schematic's gate-level view on or off. `true` starts a
    /// Yosys synthesis of the current schematic target.
    SchematicGates(bool),
}

/// Normalize a raw active-low LED mask to a logical lit mask.
pub fn led_mask_to_lit(raw: u8, led_count: u8) -> u8 {
    !raw & ((1u8 << led_count) - 1)
}

/// Normalize raw high-time duty (0..255) to a logical lit fraction (0..1).
/// The LED is lit while the pin is LOW, so lit = 1 - high fraction.
pub fn duty_to_lit(raw: [u8; 5]) -> [f32; 5] {
    let mut out = [0.0f32; 5];
    for (o, r) in out.iter_mut().zip(raw.iter()) {
        *o = 1.0 - (*r as f32 / 255.0);
    }
    out
}

/// Translate a logical "pressed" state to the electrical push-button level.
pub fn pb_level(pressed: bool) -> bool {
    !pressed
}

/// Translate a logical "on" state to the electrical DIP-switch level.
pub fn dip_level(on: bool) -> bool {
    !on
}

/// Build the electrical DIP mask from logical on/off states.
pub fn dip_mask(on: &[bool; 5]) -> u8 {
    let mut mask = 0u8;
    for (i, &o) in on.iter().enumerate() {
        if dip_level(o) {
            mask |= 1 << i;
        }
    }
    mask
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_all_sim_lines() {
        assert_eq!(
            parse_hw_line("__JADE_HW|HELLO|1|blink"),
            Some(SimLine::Hello { version: 1, top: "blink".into() })
        );
        assert_eq!(
            parse_hw_line("__JADE_HW|PORTS|clk:in:1,pb:in:4,dipsw:in:5,led:out:5"),
            Some(SimLine::Ports("clk:in:1,pb:in:4,dipsw:in:5,led:out:5".into()))
        );
        assert_eq!(
            parse_hw_line("__JADE_HW|LED|1340|21"),
            Some(SimLine::Led { t_ns: 1340, mask: 21 })
        );
        assert_eq!(
            parse_hw_line("__JADE_HW|DUTY|1340|0,64,128,192,255"),
            Some(SimLine::Duty { t_ns: 1340, duty: [0, 64, 128, 192, 255] })
        );
        assert_eq!(
            parse_hw_line("__JADE_HW|RATE|1000|50000000|1000"),
            Some(SimLine::Rate { t_ns: 1000, cycles_per_sec: 50_000_000.0, slow_x1000: 1000 })
        );
        assert_eq!(
            parse_hw_line("__JADE_HW|CLK|20|1"),
            Some(SimLine::Clk { t_ns: 20, level: true })
        );
        assert_eq!(
            parse_hw_line("__JADE_HW|STATE|paused"),
            Some(SimLine::State(SimRunState::Paused))
        );
        assert_eq!(parse_hw_line("__JADE_HW|BYE|quit"), Some(SimLine::Bye("quit".into())));
    }

    #[test]
    fn malformed_lines_are_swallowed() {
        for line in [
            "__JADE_HW|",
            "__JADE_HW|LED|abc|3",
            "__JADE_HW|DUTY|10|1,2,3",
            "__JADE_HW|STATE|zooming",
            "__JADE_HW|MYSTERY|1",
            "not a protocol line",
            "__JADE_ALLOC|0x1|8",
        ] {
            assert_eq!(parse_hw_line(line), None, "line: {line}");
        }
    }

    #[test]
    fn formats_commands() {
        assert_eq!(format_command(&SimCommand::SetPb { index: 2, level: false }), "SET|PB|2|0");
        assert_eq!(format_command(&SimCommand::SetDip { index: 4, level: true }), "SET|DIPSW|4|1");
        assert_eq!(format_command(&SimCommand::SetAllDip { mask: 0b10110 }), "SETALL|DIPSW|22");
        assert_eq!(format_command(&SimCommand::Pause), "PAUSE");
        assert_eq!(format_command(&SimCommand::Resume), "RESUME");
        assert_eq!(format_command(&SimCommand::Step { cycles: 67108864 }), "STEP|67108864");
        assert_eq!(format_command(&SimCommand::Rate { hz: -1 }), "RATE|-1");
        assert_eq!(format_command(&SimCommand::Quit), "QUIT");
    }

    #[test]
    fn normalizes_active_low() {
        // Raw 0b11110: led[0] pin low → LED 0 lit only.
        assert_eq!(led_mask_to_lit(0b11110, 5), 0b00001);
        // Raw duty 255 = pin high all window = LED dark.
        let lit = duty_to_lit([255, 0, 128, 255, 0]);
        assert!(lit[0] < 0.001 && lit[1] > 0.999);
        assert!((lit[2] - 0.498).abs() < 0.01);
        // Pressed button drives the pin low; DIP ON drives the pin low.
        assert!(!pb_level(true) && pb_level(false));
        assert!(!dip_level(true) && dip_level(false));
        // ON switches (0 and 2) drive their pins low; the rest stay high.
        assert_eq!(dip_mask(&[true, false, true, false, false]), 0b11010);
    }
}
