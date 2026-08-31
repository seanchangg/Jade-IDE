//! Parser for Quartus `.qsf` settings files.
//!
//! The parser reads only the assignments that the simulator needs: the top
//! entity, the Verilog source list, and the pin locations. It keeps the line
//! number of each assignment so diagnostics can point into the `.qsf` file.

use std::path::{Path, PathBuf};

/// One `set_location_assignment PIN_x -to <signal>` line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PinAssignment {
    /// The pin name without the `PIN_` prefix, for example `T20`.
    pub pin: String,
    /// The full signal text, for example `led[0]` or `clk`.
    pub signal: String,
    /// The port name part, for example `led`.
    pub port: String,
    /// The bit index when the signal has one, for example `0` in `led[0]`.
    pub bit: Option<u32>,
    /// 1-based line number in the `.qsf` file.
    pub line: u32,
}

/// The assignments that the simulator reads from a `.qsf` file.
#[derive(Debug, Clone, Default)]
pub struct QsfInfo {
    pub path: PathBuf,
    /// `TOP_LEVEL_ENTITY` value and its line number.
    pub top: Option<(String, u32)>,
    /// `VERILOG_FILE` values in file order, with line numbers.
    pub verilog_files: Vec<(String, u32)>,
    pub pins: Vec<PinAssignment>,
}

/// Parse the text of a `.qsf` file. Unknown lines are ignored.
pub fn parse_qsf(path: &Path, text: &str) -> QsfInfo {
    let mut info = QsfInfo {
        path: path.to_path_buf(),
        ..QsfInfo::default()
    };
    for (idx, raw) in text.lines().enumerate() {
        let line_no = (idx + 1) as u32;
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let tokens = split_tcl(line);
        if tokens.is_empty() {
            continue;
        }
        match tokens[0].as_str() {
            "set_global_assignment" => {
                let Some(name) = flag_value(&tokens, "-name") else {
                    continue;
                };
                // The value follows the `-name <NAME>` pair.
                let value = tokens
                    .iter()
                    .position(|t| t == &name)
                    .and_then(|i| tokens.get(i + 1))
                    .cloned();
                let Some(value) = value else { continue };
                match name.as_str() {
                    "TOP_LEVEL_ENTITY" => info.top = Some((value, line_no)),
                    "VERILOG_FILE" | "SYSTEMVERILOG_FILE" => {
                        info.verilog_files.push((value, line_no))
                    }
                    _ => {}
                }
            }
            "set_location_assignment" => {
                let Some(pin_tok) = tokens.get(1) else { continue };
                let Some(signal) = flag_value(&tokens, "-to") else {
                    continue;
                };
                let pin = pin_tok.strip_prefix("PIN_").unwrap_or(pin_tok).to_string();
                let (port, bit) = split_signal(&signal);
                info.pins.push(PinAssignment {
                    pin,
                    signal,
                    port,
                    bit,
                    line: line_no,
                });
            }
            _ => {}
        }
    }
    info
}

/// Split `led[3]` into `("led", Some(3))` and `clk` into `("clk", None)`.
fn split_signal(signal: &str) -> (String, Option<u32>) {
    if let Some(open) = signal.find('[') {
        if let Some(close) = signal.rfind(']') {
            if let Ok(bit) = signal[open + 1..close].trim().parse::<u32>() {
                return (signal[..open].to_string(), Some(bit));
            }
        }
    }
    (signal.to_string(), None)
}

/// Return the token after `flag`.
fn flag_value(tokens: &[String], flag: &str) -> Option<String> {
    tokens
        .iter()
        .position(|t| t == flag)
        .and_then(|i| tokens.get(i + 1))
        .cloned()
}

/// Split a Tcl-like line into tokens. Double quotes group words. Quotes do
/// not survive into the token.
fn split_tcl(line: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut in_quotes = false;
    for c in line.chars() {
        match c {
            '"' => in_quotes = !in_quotes,
            c if c.is_whitespace() && !in_quotes => {
                if !cur.is_empty() {
                    out.push(std::mem::take(&mut cur));
                }
            }
            c => cur.push(c),
        }
    }
    if !cur.is_empty() {
        out.push(cur);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const BLINK_QSF: &str = r#"# MAX 10 FPGA Development Kit (DK-DEV-10M50-A), PCB 100-0321401 Rev C.
# Pins come from the GHRD ghrd_10m50daf484c6ges.qsf.
set_global_assignment -name FAMILY "MAX 10"
set_global_assignment -name DEVICE 10M50DAF484C6GES
set_global_assignment -name TOP_LEVEL_ENTITY blink
set_global_assignment -name VERILOG_FILE blink.v
set_global_assignment -name SDC_FILE blink.sdc
set_global_assignment -name PROJECT_OUTPUT_DIRECTORY output_files
set_global_assignment -name INTERNAL_FLASH_UPDATE_MODE "SINGLE IMAGE"
set_global_assignment -name NUM_PARALLEL_PROCESSORS 4

# CLK_50_MAX10, 50 MHz.
set_location_assignment PIN_M9 -to clk
set_instance_assignment -name IO_STANDARD "2.5 V" -to clk

# USER_LED[4:0]. The LED is on when the pin is low.
set_location_assignment PIN_T20 -to led[0]
set_location_assignment PIN_U22 -to led[1]
set_location_assignment PIN_U21 -to led[2]
set_location_assignment PIN_AA21 -to led[3]
set_location_assignment PIN_AA22 -to led[4]
set_instance_assignment -name IO_STANDARD "1.5 V" -to led[0]

# USER_PB[0], for a reset if you want one later.
set_location_assignment PIN_L22 -to button
set_instance_assignment -name IO_STANDARD "1.5 V" -to button

# Keep unused pins safe.
set_global_assignment -name RESERVE_ALL_UNUSED_PINS_WEAK_PULLUP "AS INPUT TRI-STATED"

set_global_assignment -name LAST_QUARTUS_VERSION "25.1std.0 Lite Edition"
"#;

    #[test]
    fn parses_blink_qsf() {
        let info = parse_qsf(Path::new("blink.qsf"), BLINK_QSF);
        assert_eq!(info.top, Some(("blink".to_string(), 5)));
        assert_eq!(info.verilog_files, vec![("blink.v".to_string(), 6)]);
        assert_eq!(info.pins.len(), 7);

        let clk = &info.pins[0];
        assert_eq!(clk.pin, "M9");
        assert_eq!(clk.port, "clk");
        assert_eq!(clk.bit, None);
        assert_eq!(clk.line, 13);

        let led4 = info.pins.iter().find(|p| p.signal == "led[4]").unwrap();
        assert_eq!(led4.pin, "AA22");
        assert_eq!(led4.port, "led");
        assert_eq!(led4.bit, Some(4));

        let button = info.pins.iter().find(|p| p.port == "button").unwrap();
        assert_eq!(button.pin, "L22");
        assert_eq!(button.bit, None);
    }

    #[test]
    fn ignores_noise_and_quoted_values() {
        let text = "\nset_global_assignment -name FAMILY \"MAX 10\"\nnot_an_assignment foo bar\nset_location_assignment\n";
        let info = parse_qsf(Path::new("x.qsf"), text);
        assert!(info.top.is_none());
        assert!(info.pins.is_empty());
        assert!(info.verilog_files.is_empty());
    }
}
