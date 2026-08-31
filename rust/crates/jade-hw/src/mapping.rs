//! Resolve the connection between the user's top module and the board.
//!
//! The primary source is the `.qsf` pin table. When the project has no
//! `.qsf`, a name fallback maps common port names (clk, led, pb, sw) to the
//! board, LSB first. Width and direction conflicts become hard errors with a
//! precise `.qsf` location.

use std::path::PathBuf;

use jade_build::{BuildError, Severity};

use crate::board::{BoardDef, BoardSignal};
use crate::ports::{PortDir, TopPorts};
use crate::qsf::QsfInfo;

/// One bit of a user port, with the Verilog bit index as written.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PortBit {
    pub port: String,
    pub bit: u32,
    /// `true` when the port is a scalar and the index must not be printed.
    pub scalar: bool,
}

/// The resolved board connection for one design.
#[derive(Debug, Clone, Default)]
pub struct BoardMapping {
    pub top: String,
    pub clk: Option<PortBit>,
    pub leds: [Option<PortBit>; 5],
    pub pbs: [Option<PortBit>; 4],
    pub dips: [Option<PortBit>; 5],
    /// Non-fatal findings (unmapped user ports, tied inputs).
    pub warnings: Vec<BuildError>,
}

fn err(file: PathBuf, line: u32, message: String) -> BuildError {
    BuildError {
        file,
        line,
        column: 1,
        message,
        severity: Severity::Error,
    }
}

fn warn(file: PathBuf, line: u32, message: String) -> BuildError {
    BuildError {
        file,
        line,
        column: 1,
        message,
        severity: Severity::Warning,
    }
}

/// Resolve the mapping. Hard errors return `Err`.
pub fn resolve(
    qsf: Option<&QsfInfo>,
    ports: &TopPorts,
    board: &BoardDef,
) -> Result<BoardMapping, Vec<BuildError>> {
    let mut m = BoardMapping {
        top: ports.module.clone(),
        ..BoardMapping::default()
    };
    let mut errors: Vec<BuildError> = Vec::new();

    match qsf {
        Some(qsf) => resolve_from_qsf(qsf, ports, board, &mut m, &mut errors),
        None => resolve_by_name(ports, board, &mut m),
    }

    let diag_file = qsf
        .map(|q| q.path.clone())
        .unwrap_or_else(|| PathBuf::from(format!("{}.v", ports.module)));

    // Every user port bit must land somewhere. Unmapped inputs are tied to
    // 1'b1 (the board keeps unused pins on a weak pull-up); unmapped outputs
    // dangle. Both get a warning.
    for port in &ports.ports {
        let mapped_bits: Vec<u32> = mapped_bits_for(&m, &port.name);
        let total = port.width();
        if port.dir == PortDir::Input && port.name != "clk" {
            let missing = total as usize - mapped_bits.len();
            if missing > 0 && !mapped_bits.is_empty() {
                m.warnings.push(warn(
                    diag_file.clone(),
                    1,
                    format!(
                        "input `{}` has {missing} bit(s) with no board pin; they are tied to 1'b1",
                        port.name
                    ),
                ));
            } else if mapped_bits.is_empty() && m.clk.as_ref().map(|c| &c.port) != Some(&port.name)
            {
                m.warnings.push(warn(
                    diag_file.clone(),
                    1,
                    format!("input `{}` has no board pin; it is tied to 1'b1", port.name),
                ));
            }
        }
        if port.dir == PortDir::Output && mapped_bits.len() < total as usize {
            m.warnings.push(warn(
                diag_file.clone(),
                1,
                format!(
                    "output `{}` has {} bit(s) with no board LED; they are not shown",
                    port.name,
                    total as usize - mapped_bits.len()
                ),
            ));
        }
    }

    if errors.is_empty() {
        Ok(m)
    } else {
        Err(errors)
    }
}

fn mapped_bits_for(m: &BoardMapping, port: &str) -> Vec<u32> {
    let mut out = Vec::new();
    let mut push = |pb: &Option<PortBit>| {
        if let Some(pb) = pb {
            if pb.port == port {
                out.push(pb.bit);
            }
        }
    };
    push(&m.clk);
    for pb in &m.leds {
        push(pb);
    }
    for pb in &m.pbs {
        push(pb);
    }
    for pb in &m.dips {
        push(pb);
    }
    out
}

fn resolve_from_qsf(
    qsf: &QsfInfo,
    ports: &TopPorts,
    board: &BoardDef,
    m: &mut BoardMapping,
    errors: &mut Vec<BuildError>,
) {
    for pin in &qsf.pins {
        let Some(signal) = board.signal_for_pin(&pin.pin) else {
            // A pin that the board table does not model (for example a flash
            // interface pin). The simulator ignores it.
            continue;
        };
        // The port must exist on the top module.
        let Some(port) = ports.port(&pin.port) else {
            errors.push(err(
                qsf.path.clone(),
                pin.line,
                format!(
                    "`{}` is assigned PIN_{} in {} but module {} has no port `{}`",
                    pin.signal,
                    pin.pin,
                    qsf_name(qsf),
                    ports.module,
                    pin.port
                ),
            ));
            continue;
        };
        // Direction check.
        let want_output = matches!(signal, BoardSignal::Led(_));
        let is_output = port.dir == PortDir::Output;
        if want_output != is_output && port.dir != PortDir::Inout {
            let (want, have) = if want_output {
                ("an output", "an input")
            } else {
                ("an input", "an output")
            };
            errors.push(err(
                qsf.path.clone(),
                pin.line,
                format!(
                    "PIN_{} ({}) needs {} but port `{}` of module {} is {}",
                    pin.pin,
                    signal_label(signal),
                    want,
                    pin.port,
                    ports.module,
                    have
                ),
            ));
            continue;
        }
        // Width check.
        let bit = match pin.bit {
            Some(b) => {
                if !port.has_range || b < port.lsb || b > port.msb {
                    let decl = if port.has_range {
                        format!("[{}:{}]", port.msb, port.lsb)
                    } else {
                        "a scalar".to_string()
                    };
                    errors.push(err(
                        qsf.path.clone(),
                        pin.line,
                        format!(
                            "{} is assigned PIN_{} in {} but module {} declares {} as {}",
                            pin.signal,
                            pin.pin,
                            qsf_name(qsf),
                            ports.module,
                            pin.port,
                            decl
                        ),
                    ));
                    continue;
                }
                b
            }
            None => {
                if port.has_range && port.width() > 1 {
                    errors.push(err(
                        qsf.path.clone(),
                        pin.line,
                        format!(
                            "`{}` is assigned PIN_{} without a bit index but module {} declares {} as [{}:{}]",
                            pin.signal, pin.pin, ports.module, pin.port, port.msb, port.lsb
                        ),
                    ));
                    continue;
                }
                port.lsb
            }
        };
        let pb = PortBit {
            port: pin.port.clone(),
            bit,
            scalar: !port.has_range,
        };
        let slot: &mut Option<PortBit> = match signal {
            BoardSignal::Clock50 => &mut m.clk,
            BoardSignal::Led(i) => &mut m.leds[i as usize],
            BoardSignal::Pb(i) => &mut m.pbs[i as usize],
            BoardSignal::DipSw(i) => &mut m.dips[i as usize],
        };
        if slot.is_some() {
            errors.push(err(
                qsf.path.clone(),
                pin.line,
                format!(
                    "PIN_{} ({}) is assigned more than once in {}",
                    pin.pin,
                    signal_label(signal),
                    qsf_name(qsf)
                ),
            ));
            continue;
        }
        *slot = Some(pb);
    }
}

/// Name fallback when the project has no `.qsf`. LSB first.
fn resolve_by_name(ports: &TopPorts, board: &BoardDef, m: &mut BoardMapping) {
    const CLK_NAMES: &[&str] = &["clk", "clock"];
    const LED_NAMES: &[&str] = &["led", "leds"];
    const PB_NAMES: &[&str] = &["button", "buttons", "btn", "pb", "key"];
    const SW_NAMES: &[&str] = &["sw", "dipsw", "switch", "switches", "dip"];

    for port in &ports.ports {
        let lname = port.name.to_ascii_lowercase();
        let bits =
            |count: u8| -> Vec<u32> { (0..count as u32).map(|i| port.lsb + i).collect() };
        match port.dir {
            PortDir::Input => {
                if CLK_NAMES.contains(&lname.as_str()) && m.clk.is_none() {
                    m.clk = Some(PortBit {
                        port: port.name.clone(),
                        bit: port.lsb,
                        scalar: !port.has_range,
                    });
                } else if PB_NAMES.contains(&lname.as_str()) {
                    let n = (port.width() as u8).min(board.pb_count);
                    for (i, bit) in bits(n).into_iter().enumerate() {
                        if m.pbs[i].is_none() {
                            m.pbs[i] = Some(PortBit {
                                port: port.name.clone(),
                                bit,
                                scalar: !port.has_range,
                            });
                        }
                    }
                } else if SW_NAMES.contains(&lname.as_str()) {
                    let n = (port.width() as u8).min(board.dipsw_count);
                    for (i, bit) in bits(n).into_iter().enumerate() {
                        if m.dips[i].is_none() {
                            m.dips[i] = Some(PortBit {
                                port: port.name.clone(),
                                bit,
                                scalar: !port.has_range,
                            });
                        }
                    }
                }
            }
            PortDir::Output => {
                if LED_NAMES.contains(&lname.as_str()) {
                    let n = (port.width() as u8).min(board.led_count);
                    for (i, bit) in bits(n).into_iter().enumerate() {
                        if m.leds[i].is_none() {
                            m.leds[i] = Some(PortBit {
                                port: port.name.clone(),
                                bit,
                                scalar: !port.has_range,
                            });
                        }
                    }
                }
            }
            PortDir::Inout => {}
        }
    }
}

fn qsf_name(qsf: &QsfInfo) -> String {
    qsf.path
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| qsf.path.display().to_string())
}

fn signal_label(s: BoardSignal) -> String {
    match s {
        BoardSignal::Clock50 => "CLK_50_MAX10".to_string(),
        BoardSignal::Led(i) => format!("USER_LED[{i}]"),
        BoardSignal::Pb(i) => format!("USER_PB[{i}]"),
        BoardSignal::DipSw(i) => format!("USER_DIPSW[{i}]"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::board::dk_dev_10m50a;
    use crate::ports::Port;
    use crate::qsf::parse_qsf;
    use std::path::Path;

    fn blink_ports() -> TopPorts {
        TopPorts {
            module: "blink".into(),
            ports: vec![
                Port { name: "clk".into(), dir: PortDir::Input, msb: 0, lsb: 0, has_range: false },
                Port { name: "button".into(), dir: PortDir::Input, msb: 0, lsb: 0, has_range: false },
                Port { name: "led".into(), dir: PortDir::Output, msb: 4, lsb: 0, has_range: true },
            ],
        }
    }

    fn blink_qsf() -> QsfInfo {
        let text = "\
set_global_assignment -name TOP_LEVEL_ENTITY blink
set_global_assignment -name VERILOG_FILE blink.v
set_location_assignment PIN_M9 -to clk
set_location_assignment PIN_T20 -to led[0]
set_location_assignment PIN_U22 -to led[1]
set_location_assignment PIN_U21 -to led[2]
set_location_assignment PIN_AA21 -to led[3]
set_location_assignment PIN_AA22 -to led[4]
set_location_assignment PIN_L22 -to button
";
        parse_qsf(Path::new("blink.qsf"), text)
    }

    #[test]
    fn maps_blink() {
        let m = resolve(Some(&blink_qsf()), &blink_ports(), &dk_dev_10m50a()).unwrap();
        assert_eq!(m.clk, Some(PortBit { port: "clk".into(), bit: 0, scalar: true }));
        for i in 0..5 {
            assert_eq!(
                m.leds[i],
                Some(PortBit { port: "led".into(), bit: i as u32, scalar: false })
            );
        }
        assert_eq!(m.pbs[0], Some(PortBit { port: "button".into(), bit: 0, scalar: true }));
        assert!(m.pbs[1].is_none() && m.dips.iter().all(Option::is_none));
    }

    #[test]
    fn width_mismatch_is_an_error_at_the_qsf_line() {
        let mut ports = blink_ports();
        ports.ports[2].msb = 3; // led is [3:0]; the qsf assigns led[4].
        let errs = resolve(Some(&blink_qsf()), &ports, &dk_dev_10m50a()).unwrap_err();
        assert_eq!(errs.len(), 1);
        let e = &errs[0];
        assert_eq!(e.file, Path::new("blink.qsf"));
        assert_eq!(e.line, 8);
        assert_eq!(e.severity, Severity::Error);
        assert!(e.message.contains("led[4]"), "{}", e.message);
        assert!(e.message.contains("PIN_AA22"), "{}", e.message);
        assert!(e.message.contains("[3:0]"), "{}", e.message);
    }

    #[test]
    fn unknown_port_is_an_error() {
        let mut ports = blink_ports();
        ports.ports.remove(1); // no `button`
        let errs = resolve(Some(&blink_qsf()), &ports, &dk_dev_10m50a()).unwrap_err();
        assert_eq!(errs.len(), 1);
        assert!(errs[0].message.contains("no port `button`"));
        assert_eq!(errs[0].line, 9);
    }

    #[test]
    fn no_qsf_fallback_maps_by_name() {
        let ports = TopPorts {
            module: "top".into(),
            ports: vec![
                Port { name: "clk".into(), dir: PortDir::Input, msb: 0, lsb: 0, has_range: false },
                Port { name: "led".into(), dir: PortDir::Output, msb: 3, lsb: 0, has_range: true },
                Port { name: "sw".into(), dir: PortDir::Input, msb: 4, lsb: 0, has_range: true },
            ],
        };
        let m = resolve(None, &ports, &dk_dev_10m50a()).unwrap();
        assert!(m.clk.is_some());
        assert_eq!(m.leds.iter().filter(|l| l.is_some()).count(), 4);
        assert_eq!(m.leds[0].as_ref().unwrap().bit, 0);
        assert_eq!(m.dips.iter().filter(|l| l.is_some()).count(), 5);
        // led is [3:0], so the board LED 4 stays unmapped and a warning notes
        // the unmapped output bits does not exist (all 4 bits are shown).
        assert!(m.leds[4].is_none());
    }

    #[test]
    fn extra_input_gets_a_tie_warning() {
        let mut ports = blink_ports();
        ports.ports.push(Port {
            name: "enable".into(),
            dir: PortDir::Input,
            msb: 0,
            lsb: 0,
            has_range: false,
        });
        let m = resolve(Some(&blink_qsf()), &ports, &dk_dev_10m50a()).unwrap();
        assert!(m
            .warnings
            .iter()
            .any(|w| w.severity == Severity::Warning && w.message.contains("`enable`")));
    }
}
