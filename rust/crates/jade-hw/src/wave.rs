//! Waveform dump loading for the wave panel.
//!
//! A testbench run writes an FST or VCD dump. This module reads the dump with
//! `wellen` (the parser behind Surfer) and converts it to a compact,
//! render-ready model: one [`WaveSignal`] per variable, with its full change
//! list in file time units. The viewer never touches `wellen` types.

use std::path::{Path, PathBuf};

/// One sampled value of a signal.
#[derive(Debug, Clone, PartialEq)]
pub enum WaveValue {
    /// A bit vector of only `0`/`1` bits, at most 64 wide. The common case.
    Bits(u64),
    /// A bit vector with `x`/`z` bits, or wider than 64 bits: the raw bit
    /// string, MSB first.
    Text(Box<str>),
    Real(f64),
    Str(Box<str>),
}

impl WaveValue {
    /// `true` when any bit is not `0` or `1`.
    pub fn is_unknown(&self) -> bool {
        match self {
            WaveValue::Text(s) => s.bytes().any(|b| !matches!(b, b'0' | b'1')),
            _ => false,
        }
    }

    /// The level of a one-bit signal; `None` for `x`/`z`.
    pub fn bit(&self) -> Option<bool> {
        match self {
            WaveValue::Bits(v) => Some(*v & 1 == 1),
            WaveValue::Text(s) => match s.as_bytes().last() {
                Some(b'0') => Some(false),
                Some(b'1') => Some(true),
                _ => None,
            },
            WaveValue::Real(r) => Some(*r != 0.0),
            WaveValue::Str(_) => None,
        }
    }

    /// The display text: hex for a clean vector, the raw bits when any bit
    /// is `x`/`z` (so the unknown bits stay visible), the number for a real.
    pub fn label(&self, width: u32) -> String {
        match self {
            WaveValue::Bits(v) => {
                let digits = ((width + 3) / 4).max(1) as usize;
                format!("{v:0digits$x}")
            }
            WaveValue::Text(s) => {
                if s.bytes().all(|b| matches!(b, b'0' | b'1')) {
                    // Wider than 64 bits: hex in 4-bit groups from the LSB.
                    let mut out = String::new();
                    let bytes = s.as_bytes();
                    let mut end = bytes.len();
                    let mut digits = Vec::new();
                    while end > 0 {
                        let start = end.saturating_sub(4);
                        let mut n = 0u8;
                        for &b in &bytes[start..end] {
                            n = (n << 1) | (b - b'0');
                        }
                        digits.push(std::char::from_digit(n as u32, 16).unwrap_or('?'));
                        end = start;
                    }
                    digits.reverse();
                    out.extend(digits);
                    out
                } else if s.bytes().all(|b| b == b'x' || b == b'X') {
                    "x".to_string()
                } else if s.bytes().all(|b| b == b'z' || b == b'Z') {
                    "z".to_string()
                } else {
                    s.to_string()
                }
            }
            WaveValue::Real(r) => format!("{r}"),
            WaveValue::Str(s) => s.to_string(),
        }
    }
}

/// One change of a signal: from `time` on, the signal holds `value`.
#[derive(Debug, Clone, PartialEq)]
pub struct WaveChange {
    pub time: u64,
    pub value: WaveValue,
}

/// One variable of the dump, with its full change list.
#[derive(Debug, Clone, PartialEq)]
pub struct WaveSignal {
    /// The short name, with the `[msb:lsb]` range when the variable has one.
    pub name: String,
    /// The dotted scope path, `tb_top.dut`.
    pub scope: String,
    pub width: u32,
    pub changes: Vec<WaveChange>,
}

impl WaveSignal {
    /// The dotted full path, `tb_top.dut.clk`.
    pub fn full_name(&self) -> String {
        if self.scope.is_empty() {
            self.name.clone()
        } else {
            format!("{}.{}", self.scope, self.name)
        }
    }

    /// The value in force at `t`: the last change at or before `t`.
    pub fn value_at(&self, t: u64) -> Option<&WaveValue> {
        let idx = self.changes.partition_point(|c| c.time <= t);
        idx.checked_sub(1).map(|i| &self.changes[i].value)
    }
}

/// One scope of the design hierarchy, in pre-order.
#[derive(Debug, Clone, PartialEq)]
pub struct WaveScope {
    pub name: String,
    /// The dotted full path.
    pub path: String,
    pub depth: usize,
    /// Indices into [`WaveFile::signals`] of the variables declared here.
    pub signals: Vec<usize>,
}

/// A loaded dump.
#[derive(Debug, Clone, PartialEq)]
pub struct WaveFile {
    pub path: PathBuf,
    /// One file time unit is `time_factor × 10^time_exp` seconds.
    pub time_factor: u32,
    pub time_exp: i8,
    /// The last time in the dump, in file time units.
    pub end_time: u64,
    pub scopes: Vec<WaveScope>,
    pub signals: Vec<WaveSignal>,
}

impl WaveFile {
    /// Read a VCD or FST dump. The format is detected from the content.
    pub fn load(path: &Path) -> Result<WaveFile, String> {
        if !path.is_file() {
            return Err(format!("no dump at {}", path.display()));
        }
        let mut wave = wellen::simple::read(path)
            .map_err(|e| format!("cannot read {}: {e}", path.display()))?;
        let (time_factor, time_exp) = match wave.hierarchy().timescale() {
            Some(ts) => (ts.factor.max(1), ts.unit.to_exponent().unwrap_or(-9)),
            None => (1, -9),
        };

        // Walk the hierarchy in pre-order and collect every variable.
        let mut scopes = Vec::new();
        let mut vars: Vec<(usize, wellen::VarRef)> = Vec::new();
        {
            let h = wave.hierarchy();
            // Top-level variables, when a dump has any, live in a root scope.
            let root_vars: Vec<wellen::VarRef> = h.vars().collect();
            if !root_vars.is_empty() {
                scopes.push(WaveScope {
                    name: String::new(),
                    path: String::new(),
                    depth: 0,
                    signals: Vec::new(),
                });
                for v in root_vars {
                    vars.push((0, v));
                }
            }
            let mut stack: Vec<(wellen::ScopeRef, usize)> =
                h.scopes().map(|s| (s, 0)).collect::<Vec<_>>();
            stack.reverse();
            while let Some((sref, depth)) = stack.pop() {
                let scope = &h[sref];
                let idx = scopes.len();
                scopes.push(WaveScope {
                    name: scope.name(h).to_string(),
                    path: scope.full_name(h),
                    depth,
                    signals: Vec::new(),
                });
                for v in scope.vars(h) {
                    vars.push((idx, v));
                }
                let children: Vec<_> = scope.scopes(h).collect();
                for c in children.into_iter().rev() {
                    stack.push((c, depth + 1));
                }
            }
        }

        let mut refs: Vec<wellen::SignalRef> = Vec::new();
        {
            let h = wave.hierarchy();
            for (_, v) in &vars {
                refs.push(h[*v].signal_ref());
            }
        }
        refs.sort();
        refs.dedup();
        wave.load_signals_multi_threaded(&refs);

        let times = wave.time_table();
        let end_time = times.last().copied().unwrap_or(0);
        let mut signals = Vec::with_capacity(vars.len());
        let h = wave.hierarchy();
        for (scope_idx, vref) in &vars {
            let var = &h[*vref];
            let width = var.length(h).unwrap_or(1);
            let mut name = var.name(h).to_string();
            if let Some(ix) = var.index() {
                if ix.msb() != ix.lsb() || width > 1 {
                    name.push_str(&format!("[{}:{}]", ix.msb(), ix.lsb()));
                }
            }
            let mut changes = Vec::new();
            if let Some(sig) = wave.get_signal(var.signal_ref()) {
                for (tidx, value) in sig.iter_changes() {
                    let time = times.get(tidx as usize).copied().unwrap_or(0);
                    let value = convert(value);
                    // A repeated value is not a change; keep the list tight.
                    if changes
                        .last()
                        .is_some_and(|c: &WaveChange| c.value == value)
                    {
                        continue;
                    }
                    changes.push(WaveChange { time, value });
                }
            }
            let sig_idx = signals.len();
            scopes[*scope_idx].signals.push(sig_idx);
            signals.push(WaveSignal {
                name,
                scope: scopes[*scope_idx].path.clone(),
                width,
                changes,
            });
        }

        Ok(WaveFile {
            path: path.to_path_buf(),
            time_factor,
            time_exp,
            end_time,
            scopes,
            signals,
        })
    }

    /// File time units → seconds.
    pub fn seconds(&self, t: f64) -> f64 {
        t * self.time_factor as f64 * 10f64.powi(self.time_exp as i32)
    }

    /// A short time label, `120 ns` or `1.5 µs`, for a time in file units.
    pub fn time_label(&self, t: f64) -> String {
        format_seconds(self.seconds(t))
    }

    /// The signal indices of the first scope with any signals: for a
    /// testbench dump, the testbench module itself.
    pub fn default_rows(&self) -> Vec<usize> {
        self.scopes
            .iter()
            .find(|s| !s.signals.is_empty())
            .map(|s| s.signals.clone())
            .unwrap_or_default()
    }

    /// Find a signal by full dotted name.
    pub fn find(&self, full_name: &str) -> Option<usize> {
        self.signals.iter().position(|s| s.full_name() == full_name)
    }
}

/// Format a duration in seconds with the unit that keeps the number short.
pub fn format_seconds(s: f64) -> String {
    let a = s.abs();
    let (v, unit) = if a == 0.0 {
        (0.0, "s")
    } else if a >= 1.0 {
        (s, "s")
    } else if a >= 1e-3 {
        (s * 1e3, "ms")
    } else if a >= 1e-6 {
        (s * 1e6, "µs")
    } else if a >= 1e-9 {
        (s * 1e9, "ns")
    } else if a >= 1e-12 {
        (s * 1e12, "ps")
    } else {
        (s * 1e15, "fs")
    };
    let text = if (v - v.round()).abs() < 1e-6 {
        format!("{}", v.round() as i64)
    } else {
        let t = format!("{v:.3}");
        t.trim_end_matches('0').trim_end_matches('.').to_string()
    };
    format!("{text} {unit}")
}

fn convert(value: wellen::SignalValueRef<'_>) -> WaveValue {
    match value {
        wellen::SignalValueRef::BitVec(bv) => {
            let bits = bv.bit_string();
            if bits.len() <= 64 && bits.bytes().all(|b| matches!(b, b'0' | b'1')) {
                WaveValue::Bits(u64::from_str_radix(&bits, 2).unwrap_or(0))
            } else {
                WaveValue::Text(bits.into_boxed_str())
            }
        }
        wellen::SignalValueRef::Real(r) => WaveValue::Real(r),
        wellen::SignalValueRef::String(s) => WaveValue::Str(s.into()),
        wellen::SignalValueRef::Event => WaveValue::Bits(1),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const VCD: &str = "\
$timescale 1ns $end
$scope module tb $end
$var wire 1 ! clk $end
$var wire 4 \" cnt [3:0] $end
$scope module dut $end
$var wire 1 # led $end
$upscope $end
$upscope $end
$enddefinitions $end
#0
0!
b0000 \"
1#
#5
1!
b0001 \"
#10
0!
#15
1!
bxx01 \"
0#
#20
0!
";

    fn write_vcd() -> PathBuf {
        let dir = std::env::temp_dir().join(format!("jade_wave_{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("t.vcd");
        std::fs::write(&path, VCD).unwrap();
        path
    }

    #[test]
    fn loads_scopes_signals_and_changes() {
        let w = WaveFile::load(&write_vcd()).unwrap();
        assert_eq!(w.end_time, 20);
        assert_eq!(w.time_exp, -9);
        let names: Vec<String> = w.signals.iter().map(|s| s.full_name()).collect();
        assert_eq!(names, vec!["tb.clk", "tb.cnt[3:0]", "tb.dut.led"]);
        assert_eq!(w.scopes.len(), 2);
        assert_eq!(w.scopes[0].signals, vec![0, 1]);
        assert_eq!(w.scopes[1].depth, 1);
        assert_eq!(w.default_rows(), vec![0, 1]);

        let clk = &w.signals[0];
        assert_eq!(clk.width, 1);
        assert_eq!(clk.changes.len(), 5);
        assert_eq!(clk.value_at(7), Some(&WaveValue::Bits(1)));
        assert_eq!(clk.value_at(10), Some(&WaveValue::Bits(0)));

        let cnt = &w.signals[1];
        assert_eq!(cnt.width, 4);
        assert_eq!(cnt.value_at(5).unwrap().label(4), "1");
        let x = cnt.value_at(15).unwrap();
        assert!(x.is_unknown());
        assert_eq!(x.label(4), "xx01");
    }

    #[test]
    fn labels_and_time_format() {
        assert_eq!(WaveValue::Bits(0x3e00).label(16), "3e00");
        assert_eq!(WaveValue::Bits(5).label(3), "5");
        assert_eq!(WaveValue::Text("xxxx".into()).label(4), "x");
        assert_eq!(
            WaveValue::Text("1".repeat(68).into_boxed_str()).label(68),
            "fffffffffffffffff"
        );
        assert_eq!(format_seconds(20e-9), "20 ns");
        assert_eq!(format_seconds(1.5e-6), "1.5 µs");
        assert_eq!(format_seconds(0.0), "0 s");
        assert_eq!(format_seconds(2.5e-12), "2.5 ps");
    }
}
