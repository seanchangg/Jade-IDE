//! Testbench runs with Icarus Verilog.
//!
//! The wave panel tests a design the way the project `Makefile` does:
//! `iverilog` compiles the testbench with every design source, `vvp` runs it
//! with an FST dump, and the panel loads the dump. A testbench is a root
//! `tb_<name>.v` (or `<name>_tb.v`) in the directory of the active file, or
//! in the workspace root; the design sources are every other `.v`/`.sv` in
//! the directory of the testbench. Files that declare a package go first on
//! the command line, and the testbench goes last, because Icarus resolves
//! a package name only after it has parsed the declaration.

use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use jade_build::{BuildError, Severity};
use regex::Regex;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::sync::mpsc;

/// A testbench that runs longer than this is killed: a missing `$finish`
/// must not pin a core for good.
pub const RUN_TIMEOUT: Duration = Duration::from_secs(60);

/// The outcome of one testbench run.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct TbOutcome {
    pub ok: bool,
    pub errors: Vec<BuildError>,
    /// The dump the run wrote, when one was found.
    pub dump: Option<PathBuf>,
    /// A one-line failure summary for the status line.
    pub failure: Option<String>,
}

fn is_verilog(p: &Path) -> bool {
    matches!(p.extension().and_then(|e| e.to_str()), Some("v" | "sv"))
}

/// `true` for `tb_*.v` and `*_tb.v`.
pub fn is_testbench(p: &Path) -> bool {
    if !is_verilog(p) {
        return false;
    }
    let stem = p.file_stem().and_then(|s| s.to_str()).unwrap_or("");
    stem.starts_with("tb_") || stem.ends_with("_tb")
}

fn root_verilog(root: &Path) -> Vec<PathBuf> {
    let mut v: Vec<PathBuf> = std::fs::read_dir(root)
        .ok()
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.is_file() && is_verilog(p))
        .collect();
    v.sort();
    v
}

/// Every testbench in the project root, sorted by name.
pub fn find_testbenches(root: &Path) -> Vec<PathBuf> {
    root_verilog(root).into_iter().filter(|p| is_testbench(p)).collect()
}

/// The design sources for `tb`: every root `.v`/`.sv` that is not a
/// testbench.
pub fn design_sources(root: &Path) -> Vec<PathBuf> {
    root_verilog(root).into_iter().filter(|p| !is_testbench(p)).collect()
}

/// `true` when the file declares a SystemVerilog package.
///
/// Icarus parses its inputs in one pass, in command-line order. A package
/// name is only known to the lexer after its declaration, so a file that
/// imports a package must come after the file that declares it.
fn declares_package(path: &Path) -> bool {
    let Ok(text) = std::fs::read_to_string(path) else { return false };
    let re = Regex::new(r"(?m)^[ \t]*package\s+[A-Za-z_][A-Za-z0-9_$]*")
        .expect("static regex");
    re.is_match(&crate::compile::strip_comments(&text))
}

/// The iverilog command-line order for `tb` with its design `sources`:
/// package files first, then the other design sources, then the testbench.
/// Every group keeps its incoming order.
pub fn compile_order(tb: &Path, sources: &[PathBuf]) -> Vec<PathBuf> {
    let (pkgs, rest): (Vec<PathBuf>, Vec<PathBuf>) =
        sources.iter().cloned().partition(|p| declares_package(p));
    pkgs.into_iter()
        .chain(rest)
        .chain(std::iter::once(tb.to_path_buf()))
        .collect()
}

/// Choose the testbench for the active editor file: the file itself when it
/// is a testbench, else `tb_<stem>.v` / `<stem>_tb.v` in the same directory,
/// else the first testbench in that directory, else the first in the root.
pub fn pick_testbench(root: &Path, active: Option<&Path>) -> Option<PathBuf> {
    if let Some(active) = active {
        if is_testbench(active) && active.is_file() {
            return Some(active.to_path_buf());
        }
        let dir = active.parent().unwrap_or(root);
        let local = find_testbenches(dir);
        let stem = active.file_stem().and_then(|s| s.to_str()).unwrap_or("");
        for cand in [format!("tb_{stem}"), format!("{stem}_tb")] {
            if let Some(hit) = local
                .iter()
                .find(|p| p.file_stem().and_then(|s| s.to_str()) == Some(cand.as_str()))
            {
                return Some(hit.clone());
            }
        }
        if let Some(first) = local.into_iter().next() {
            return Some(first);
        }
    }
    find_testbenches(root).into_iter().next()
}

/// The dump file name a testbench asks for: `$dumpfile("tb.fst")`.
pub fn dumpfile_name(tb_source: &str) -> Option<String> {
    let re = Regex::new(r#"\$dumpfile\s*\(\s*"([^"]+)"\s*\)"#).ok()?;
    re.captures(tb_source).map(|c| c[1].to_string())
}

/// Parse `file:line: error: message` lines from iverilog.
pub fn parse_iverilog_errors(text: &str) -> Vec<BuildError> {
    let re = Regex::new(r"^(?P<file>[^:\s][^:]*?):(?P<line>\d+):\s*(?P<sev>error|warning|sorry|internal error):\s*(?P<msg>.*)$")
        .expect("static regex");
    let mut out = Vec::new();
    for line in text.lines() {
        let Some(c) = re.captures(line.trim_end()) else { continue };
        let severity = match &c["sev"] {
            "warning" => Severity::Warning,
            _ => Severity::Error,
        };
        out.push(BuildError {
            file: PathBuf::from(&c["file"]),
            line: c["line"].parse().unwrap_or(0),
            column: 1,
            message: c["msg"].to_string(),
            severity,
        });
    }
    out
}

/// Check that `iverilog` and `vvp` run. `Err` carries the install hint.
pub fn detect_iverilog() -> Result<(), String> {
    for tool in ["iverilog", "vvp"] {
        let ok = std::process::Command::new(tool)
            .arg("-V")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .is_ok();
        if !ok {
            return Err(format!(
                "{tool} not found. Install Icarus Verilog: brew install icarus-verilog"
            ));
        }
    }
    Ok(())
}

/// Open a dump in the external Surfer viewer. `Err` carries the install hint.
pub fn open_in_surfer(dump: &Path) -> Result<(), String> {
    std::process::Command::new("surfer")
        .arg(dump)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .map(|_| ())
        .map_err(|_| "surfer not found. Install it: brew install surfer".to_string())
}

/// The newest `.fst`/`.vcd` under `dir` modified at or after `since`.
pub fn newest_dump(dir: &Path, since: Option<SystemTime>) -> Option<PathBuf> {
    let mut best: Option<(SystemTime, PathBuf)> = None;
    for e in std::fs::read_dir(dir).ok()?.flatten() {
        let p = e.path();
        if !matches!(p.extension().and_then(|x| x.to_str()), Some("fst" | "vcd")) {
            continue;
        }
        let Ok(m) = e.metadata().and_then(|m| m.modified()) else { continue };
        if since.is_some_and(|s| m < s) {
            continue;
        }
        if best.as_ref().is_none_or(|(t, _)| m > *t) {
            best = Some((m, p));
        }
    }
    best.map(|(_, p)| p)
}

/// Forward every line of a child's stdout and stderr to `lines`. Returns
/// the collected stderr for diagnostics parsing.
async fn pump(child: &mut tokio::process::Child, lines: &mpsc::UnboundedSender<String>) -> String {
    let mut so = BufReader::new(child.stdout.take().expect("piped")).lines();
    let mut se = BufReader::new(child.stderr.take().expect("piped")).lines();
    let mut stderr_text = String::new();
    let (mut so_done, mut se_done) = (false, false);
    while !(so_done && se_done) {
        tokio::select! {
            l = so.next_line(), if !so_done => match l {
                Ok(Some(l)) => { let _ = lines.send(format!("  {l}")); }
                _ => so_done = true,
            },
            l = se.next_line(), if !se_done => match l {
                Ok(Some(l)) => {
                    stderr_text.push_str(&l);
                    stderr_text.push('\n');
                    let _ = lines.send(format!("  {l}"));
                }
                _ => se_done = true,
            },
        }
    }
    stderr_text
}

/// Compile and run `tb` from `root`. Output lines stream to `lines`; the
/// dump lands in `<root>/sim/`.
pub async fn run_testbench(
    root: PathBuf,
    tb: PathBuf,
    lines: mpsc::UnboundedSender<String>,
) -> TbOutcome {
    let mut out = TbOutcome::default();
    if let Err(hint) = detect_iverilog() {
        out.failure = Some(hint.clone());
        let _ = lines.send(hint);
        return out;
    }
    let sim = root.join("sim");
    if let Err(e) = std::fs::create_dir_all(&sim) {
        out.failure = Some(format!("cannot create {}: {e}", sim.display()));
        return out;
    }
    let stem = tb.file_stem().and_then(|s| s.to_str()).unwrap_or("tb").to_string();
    let vvp_path = sim.join(format!("{stem}.vvp"));
    let sources = compile_order(&tb, &design_sources(&root));
    let rel = |p: &Path| -> String {
        p.strip_prefix(&root)
            .unwrap_or(p)
            .to_string_lossy()
            .into_owned()
    };

    // ── iverilog ────────────────────────────────────────────────────────────
    let mut cmd = tokio::process::Command::new("iverilog");
    cmd.current_dir(&root)
        .arg("-g2012")
        .arg("-Wall")
        .arg("-o")
        .arg(&vvp_path)
        .args(&sources)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    let shown: Vec<String> = sources.iter().map(|p| rel(p)).collect();
    let _ = lines.send(format!("$ iverilog -g2012 -Wall -o sim/{stem}.vvp {}", shown.join(" ")));
    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => {
            out.failure = Some(format!("failed to run iverilog: {e}"));
            return out;
        }
    };
    let stderr_text = pump(&mut child, &lines).await;
    let status = child.wait().await.ok();
    out.errors = parse_iverilog_errors(&stderr_text);
    if !status.is_some_and(|s| s.success()) {
        if !out.errors.iter().any(|e| e.severity == Severity::Error) {
            out.errors.push(BuildError {
                file: tb.clone(),
                line: 1,
                column: 1,
                message: "iverilog failed".to_string(),
                severity: Severity::Error,
            });
        }
        // The status row shows the first error, not a bare "failed".
        let first = out.errors.iter().find(|e| e.severity == Severity::Error);
        out.failure = Some(match first {
            Some(e) => format!(
                "{}:{}: {}",
                e.file.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default(),
                e.line,
                e.message
            ),
            None => "iverilog failed".to_string(),
        });
        return out;
    }

    // ── vvp ─────────────────────────────────────────────────────────────────
    let started = SystemTime::now();
    let mut cmd = tokio::process::Command::new("vvp");
    cmd.current_dir(&sim)
        .arg("-n")
        .arg(&vvp_path)
        .arg("-fst")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    let _ = lines.send(format!("$ cd sim && vvp -n {stem}.vvp -fst"));
    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => {
            out.failure = Some(format!("failed to run vvp: {e}"));
            return out;
        }
    };
    let run = async {
        let _ = pump(&mut child, &lines).await;
        child.wait().await.ok()
    };
    let status = match tokio::time::timeout(RUN_TIMEOUT, run).await {
        Ok(s) => s,
        Err(_) => {
            let _ = child.start_kill();
            let _ = lines.send(format!(
                "  vvp killed after {} s: does the testbench call $finish?",
                RUN_TIMEOUT.as_secs()
            ));
            out.failure = Some("testbench timed out".to_string());
            None
        }
    };

    // The dump: the name the testbench asked for, else the newest one.
    let named = std::fs::read_to_string(&tb)
        .ok()
        .and_then(|src| dumpfile_name(&src))
        .map(|n| sim.join(n))
        .filter(|p| p.is_file());
    out.dump = named.or_else(|| newest_dump(&sim, Some(started)));
    if out.failure.is_some() {
        return out;
    }
    if !status.is_some_and(|s| s.success()) {
        out.failure = Some("vvp failed".to_string());
        return out;
    }
    if out.dump.is_none() {
        let _ = lines.send(
            "  no dump written: add $dumpfile(\"<name>.fst\"); $dumpvars(0, <tb>); to the testbench"
                .to_string(),
        );
        out.failure = Some("no dump written".to_string());
        return out;
    }
    out.ok = true;
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("jade_tb_{}_{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn discovers_and_picks_testbenches() {
        let dir = tmp("pick");
        for f in ["alpha.v", "tb_alpha.v", "beta_tb.sv", "gamma.v", "notes.md"] {
            std::fs::write(dir.join(f), "").unwrap();
        }
        let tbs = find_testbenches(&dir);
        let names: Vec<_> = tbs.iter().map(|p| p.file_name().unwrap().to_str().unwrap()).collect();
        assert_eq!(names, vec!["beta_tb.sv", "tb_alpha.v"]);
        let srcs: Vec<_> = design_sources(&dir)
            .iter()
            .map(|p| p.file_name().unwrap().to_str().unwrap().to_string())
            .collect();
        assert_eq!(srcs, vec!["alpha.v", "gamma.v"]);

        assert_eq!(pick_testbench(&dir, Some(&dir.join("alpha.v"))), Some(dir.join("tb_alpha.v")));
        assert_eq!(pick_testbench(&dir, Some(&dir.join("beta.v"))), Some(dir.join("beta_tb.sv")));
        assert_eq!(pick_testbench(&dir, Some(&dir.join("tb_alpha.v"))), Some(dir.join("tb_alpha.v")));
        // No match: the first testbench by name.
        assert_eq!(pick_testbench(&dir, Some(&dir.join("gamma.v"))), Some(dir.join("beta_tb.sv")));
        assert_eq!(pick_testbench(&dir, None), Some(dir.join("beta_tb.sv")));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn picks_testbench_next_to_active_file_in_a_sub_directory() {
        let dir = tmp("subdir");
        let sub = dir.join("sys_array");
        std::fs::create_dir_all(&sub).unwrap();
        for f in ["uart.v", "tb_uart.v"] {
            std::fs::write(dir.join(f), "").unwrap();
        }
        for f in ["systolic.v", "tb_systolic.v", "other_tb.v"] {
            std::fs::write(sub.join(f), "").unwrap();
        }
        // The matching testbench in the same directory wins over the root.
        assert_eq!(pick_testbench(&dir, Some(&sub.join("systolic.v"))), Some(sub.join("tb_systolic.v")));
        // No name match: the first testbench in the same directory.
        assert_eq!(pick_testbench(&dir, Some(&sub.join("nomatch.v"))), Some(sub.join("other_tb.v")));
        // A directory with no testbench falls back to the root.
        let empty = dir.join("empty");
        std::fs::create_dir_all(&empty).unwrap();
        std::fs::write(empty.join("x.v"), "").unwrap();
        assert_eq!(pick_testbench(&dir, Some(&empty.join("x.v"))), Some(dir.join("tb_uart.v")));
        // Sources come from the directory of the testbench.
        let srcs: Vec<_> = design_sources(&sub)
            .iter()
            .map(|p| p.file_name().unwrap().to_str().unwrap().to_string())
            .collect();
        assert_eq!(srcs, vec!["systolic.v"]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn orders_packages_first_and_the_testbench_last() {
        let dir = tmp("order");
        std::fs::write(dir.join("systolic.sv"), "module array; endmodule\n").unwrap();
        std::fs::write(
            dir.join("fp16_tb_pkg.sv"),
            "// helpers\npackage fp16_tb_pkg;\nendpackage\n",
        )
        .unwrap();
        // A comment that mentions a package is not a declaration.
        std::fs::write(dir.join("alu.sv"), "// package notes\nmodule alu; endmodule\n").unwrap();
        std::fs::write(dir.join("tb_array.sv"), "module tb_array; endmodule\n").unwrap();

        let tb = dir.join("tb_array.sv");
        let order = compile_order(&tb, &design_sources(&dir));
        assert_eq!(
            order,
            vec![
                dir.join("fp16_tb_pkg.sv"),
                dir.join("alu.sv"),
                dir.join("systolic.sv"),
                tb,
            ]
        );
    }

    #[test]
    fn parses_iverilog_diagnostics() {
        let text = "\
systolic_array.v:7: warning: timescale for fp16mtplr inherited from another file.
tb_systolic.v:1: ...: The inherited timescale is here.
systolic_array.v:67: error: Port expression width 48 does not match expected width 32 or 16.
1 error(s) during elaboration.
";
        let errs = parse_iverilog_errors(text);
        assert_eq!(errs.len(), 2);
        assert_eq!(errs[0].severity, Severity::Warning);
        assert_eq!(errs[0].line, 7);
        assert_eq!(errs[1].severity, Severity::Error);
        assert_eq!(errs[1].file, PathBuf::from("systolic_array.v"));
        assert_eq!(errs[1].line, 67);
        assert!(errs[1].message.starts_with("Port expression width 48"));
    }

    #[test]
    fn reads_the_dumpfile_name() {
        assert_eq!(
            dumpfile_name("initial begin\n  $dumpfile(\"tb_x.fst\"); $dumpvars(0, tb);\nend"),
            Some("tb_x.fst".to_string())
        );
        assert_eq!(dumpfile_name("module m; endmodule"), None);
    }
}
