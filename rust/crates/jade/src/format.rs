//! Value-formatting helpers, ported verbatim from the TypeScript renderer so
//! readouts match the Electron app character-for-character (the rules the
//! feature inventory §5.6 / §7.1 call out). Pure functions, unit-tested.

/// Bytes → `B` / `KB` / `MB` (training-view.ts `formatBytes`).
pub fn format_bytes(bytes: f64) -> String {
    if bytes < 1024.0 {
        format!("{}B", bytes.round() as i64)
    } else if bytes < 1024.0 * 1024.0 {
        format!("{:.1}KB", bytes / 1024.0)
    } else if bytes < 1024.0 * 1024.0 * 1024.0 {
        format!("{:.1}MB", bytes / (1024.0 * 1024.0))
    } else {
        format!("{:.2}GB", bytes / (1024.0 * 1024.0 * 1024.0))
    }
}

/// Milliseconds → compact duration (`runtime-panel.ts:459` `formatDuration`).
/// `<1ms` / `123ms` / `1.50s` / `1m 2.3s`. Used by the BENCHMARKS section (§5.4).
pub fn format_duration(ms: f64) -> String {
    if ms < 1.0 {
        "<1ms".to_string()
    } else if ms < 1000.0 {
        format!("{}ms", ms.round() as i64)
    } else if ms < 60000.0 {
        format!("{:.2}s", ms / 1000.0)
    } else {
        format!("{}m {:.1}s", (ms / 60000.0).floor() as i64, (ms % 60000.0) / 1000.0)
    }
}

/// Compact numeric formatting for heatmap range labels (training-view.ts
/// `fmtVal`): exponential outside a "friendly" band, else round to 3 decimals.
pub fn fmt_val(v: f64) -> String {
    if !v.is_finite() {
        return "?".to_string();
    }
    let a = v.abs();
    if a != 0.0 && (a >= 10000.0 || a < 0.001) {
        // JS toExponential(1) — e.g. "1.2e+4"; Rust emits "1.2e4", close enough
        // for an axis hint.
        format!("{:.1e}", v)
    } else {
        // round(v*1000)/1000 then drop trailing zeros like JS String(number)
        let r = (v * 1000.0).round() / 1000.0;
        trim_float(r)
    }
}

/// Sidebar scalar readout: 4-decimal, exponential outside [1e-3, 1e6)
/// (telemetry-panel.ts `formatValue`).
pub fn format_scalar_value(v: f64) -> String {
    let abs = v.abs();
    if abs != 0.0 && (abs < 1e-3 || abs >= 1e6) {
        format!("{:.2e}", v)
    } else {
        format!("{:.4}", v)
    }
}

/// Sidebar timer readout: ms, switching to seconds at ≥1000ms.
pub fn format_timer_value(ms: f64) -> String {
    if ms >= 1000.0 {
        format!("{:.2}s", ms / 1000.0)
    } else {
        format!("{:.1}ms", ms)
    }
}

/// Timing-breakdown average label: like the timer readout but 1-decimal
/// (training-view.ts `renderTimingBreakdown`).
pub fn format_avg_ms(ms: f64) -> String {
    if ms >= 1000.0 {
        format!("{:.1}s", ms / 1000.0)
    } else {
        format!("{:.1}ms", ms)
    }
}

/// Sidebar buffer readout: `R×C @step` (or `R×C`, or `—`).
pub fn format_buffer_value(
    rows: Option<u32>,
    cols: Option<u32>,
    step: Option<i64>,
) -> String {
    match (rows, cols) {
        (Some(r), Some(c)) => match step {
            Some(s) => format!("{}×{} @{}", r, c, s),
            None => format!("{}×{}", r, c),
        },
        _ => "—".to_string(),
    }
}

/// A scalar/buffer whose name marks it as a memory series (training-view.ts
/// `isMemoryName`): routes it to the Memory chart instead of Loss.
pub fn is_memory_name(name: &str) -> bool {
    let n = name.to_lowercase();
    n.contains("memory") || n.contains("heap")
}

/// Drop trailing zeros from a rounded float, JS `String(number)` style.
fn trim_float(v: f64) -> String {
    if v == v.trunc() && v.abs() < 1e15 {
        return format!("{}", v as i64);
    }
    let mut s = format!("{:.3}", v);
    while s.ends_with('0') {
        s.pop();
    }
    if s.ends_with('.') {
        s.pop();
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bytes() {
        assert_eq!(format_bytes(512.0), "512B");
        assert_eq!(format_bytes(2048.0), "2.0KB");
        assert_eq!(format_bytes(5.0 * 1024.0 * 1024.0), "5.0MB");
        assert_eq!(format_bytes(11.5 * 1024.0 * 1024.0 * 1024.0), "11.50GB");
    }

    #[test]
    fn scalar_value_bands() {
        assert_eq!(format_scalar_value(0.5), "0.5000");
        assert_eq!(format_scalar_value(0.0), "0.0000");
        assert_eq!(format_scalar_value(1234.5678), "1234.5678");
        // outside [1e-3, 1e6)
        assert_eq!(format_scalar_value(0.0001), "1.00e-4");
        assert_eq!(format_scalar_value(2_000_000.0), "2.00e6");
    }

    #[test]
    fn timer_value_switches_to_seconds() {
        assert_eq!(format_timer_value(12.34), "12.3ms");
        assert_eq!(format_timer_value(1500.0), "1.50s");
    }

    #[test]
    fn buffer_value() {
        assert_eq!(format_buffer_value(Some(4), Some(8), Some(3)), "4×8 @3");
        assert_eq!(format_buffer_value(Some(4), Some(8), None), "4×8");
        assert_eq!(format_buffer_value(None, None, None), "—");
    }

    #[test]
    fn memory_name_detection() {
        assert!(is_memory_name("Heap Used"));
        assert!(is_memory_name("gpu_memory"));
        assert!(!is_memory_name("loss"));
    }

    #[test]
    fn compact_fmt_val() {
        assert_eq!(fmt_val(1.5), "1.5");
        assert_eq!(fmt_val(2.0), "2");
        assert_eq!(fmt_val(0.0), "0");
        assert!(fmt_val(0.0001).contains('e'));
        assert!(fmt_val(50000.0).contains('e'));
    }
}
