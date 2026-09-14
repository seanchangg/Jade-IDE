//! Verilator detection, the build invocation, and diagnostic parsing.
//!
//! Artifacts live in `<project>/.jade/hw/`: the generated wrapper, the fixed
//! harness, the port dump, `obj_dir/`, and the binary `obj_dir/jade_hw_sim`.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use jade_build::{BuildError, Severity};
use regex::Regex;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::sync::{mpsc, oneshot};

use crate::qsf::QsfInfo;

/// The fixed C++ harness, embedded in the binary.
pub const HARNESS_CPP: &str = include_str!("../assets/harness.cpp");

/// The minimum supported Verilator major version.
pub const MIN_VERILATOR_MAJOR: u32 = 5;

/// Result of one build.
#[derive(Debug, Clone)]
pub struct HwBuildResult {
    pub success: bool,
    pub binary: Option<PathBuf>,
    pub errors: Vec<BuildError>,
    pub duration: Duration,
    /// `true` when a newer save killed this build.
    pub canceled: bool,
}

impl HwBuildResult {
    pub fn failed(errors: Vec<BuildError>, started: Instant) -> Self {
        HwBuildResult {
            success: false,
            binary: None,
            errors,
            duration: started.elapsed(),
            canceled: false,
        }
    }
}

/// Find a usable Verilator. GUI apps get a thin `$PATH`, so the probe also
/// checks the Homebrew locations. `Err` carries the version of a too-old
/// install when one exists.
pub fn detect_verilator() -> Result<(PathBuf, String), Option<String>> {
    let mut candidates: Vec<PathBuf> = vec![PathBuf::from("verilator")];
    for dir in ["/opt/homebrew/bin", "/usr/local/bin"] {
        candidates.push(Path::new(dir).join("verilator"));
    }
    let mut old_version: Option<String> = None;
    for cand in candidates {
        let out = std::process::Command::new(&cand)
            .arg("--version")
            .output();
        let Ok(out) = out else { continue };
        if !out.status.success() {
            continue;
        }
        let text = String::from_utf8_lossy(&out.stdout).into_owned();
        // "Verilator 5.050 2026-07-01 ..."
        let version = text
            .split_whitespace()
            .nth(1)
            .unwrap_or("0")
            .to_string();
        let major: u32 = version
            .split('.')
            .next()
            .and_then(|m| m.parse().ok())
            .unwrap_or(0);
        if major >= MIN_VERILATOR_MAJOR {
            return Ok((cand, version));
        }
        old_version = Some(version);
    }
    Err(old_version)
}

/// The user Verilog source list: `VERILOG_FILE` lines from the `.qsf` when
/// present, else every `*.v` in the project root except `tb_*.v`.
pub fn source_list(project_root: &Path, qsf: Option<&QsfInfo>) -> Vec<PathBuf> {
    if let Some(qsf) = qsf {
        if !qsf.verilog_files.is_empty() {
            return qsf
                .verilog_files
                .iter()
                .map(|(f, _)| project_root.join(f))
                .collect();
        }
    }
    let mut out: Vec<PathBuf> = Vec::new();
    if let Ok(entries) = std::fs::read_dir(project_root) {
        for e in entries.flatten() {
            let name = e.file_name().to_string_lossy().into_owned();
            if name.ends_with(".v") && !name.starts_with("tb_") {
                out.push(e.path());
            }
        }
    }
    out.sort();
    out
}

/// Every candidate source for the schematic: the `.qsf` list plus every `*.v`
/// in the project root that is not a `tb_*.v`. The schematic can show a module
/// the board build leaves out, so its file set is wider than [`source_list`].
pub fn schematic_source_list(project_root: &Path, qsf: Option<&QsfInfo>) -> Vec<PathBuf> {
    let mut out = source_list(project_root, None);
    for p in source_list(project_root, qsf) {
        if !out.contains(&p) {
            out.push(p);
        }
    }
    out.sort();
    out.dedup();
    out
}

/// The module names a Verilog file declares, in file order.
///
/// Verilator owns the real parse. This scan only needs a name to hand to
/// `--top-module`, so it strips the comments and matches a `module <name>`
/// header at the start of a line.
pub fn module_names(text: &str) -> Vec<String> {
    let re = Regex::new(r"(?m)^[ \t]*module\s+([A-Za-z_][A-Za-z0-9_$]*)").expect("static regex");
    re.captures_iter(&strip_comments(text))
        .map(|c| c[1].to_string())
        .collect()
}

/// The module a source file stands for: the one whose name matches the file
/// stem, else the first one the file declares. `None` when it declares none.
pub fn module_for_file(path: &Path) -> Option<String> {
    let text = std::fs::read_to_string(path).ok()?;
    let names = module_names(&text);
    let stem = path.file_stem().map(|s| s.to_string_lossy().into_owned());
    names
        .iter()
        .find(|n| Some(n.as_str()) == stem.as_deref())
        .or_else(|| names.first())
        .cloned()
}

/// Replace every comment with nothing, and keep the line breaks so a
/// line-anchored match still lands on the right line.
pub(crate) fn strip_comments(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    let mut in_line = false;
    let mut in_block = false;
    while let Some(c) = chars.next() {
        if in_line {
            if c == '\n' {
                in_line = false;
                out.push('\n');
            }
            continue;
        }
        if in_block {
            if c == '*' && chars.peek() == Some(&'/') {
                chars.next();
                in_block = false;
            } else if c == '\n' {
                out.push('\n');
            }
            continue;
        }
        if c == '/' {
            match chars.peek() {
                Some('/') => {
                    chars.next();
                    in_line = true;
                    continue;
                }
                Some('*') => {
                    chars.next();
                    in_block = true;
                    continue;
                }
                _ => {}
            }
        }
        out.push(c);
    }
    out
}

/// Run the one-step Verilator build. Streams every output line to `line_tx`.
/// A signal on `abort` kills the child and marks the result canceled.
#[allow(clippy::too_many_arguments)]
pub async fn compile(
    verilator: &Path,
    project_root: &Path,
    hw_dir: &Path,
    top_module: &str,
    wrapper: &Path,
    sources: &[PathBuf],
    harness: &Path,
    qsf_path: Option<&Path>,
    line_tx: mpsc::UnboundedSender<String>,
    mut abort: oneshot::Receiver<()>,
) -> HwBuildResult {
    let started = Instant::now();
    let obj_dir = hw_dir.join("obj_dir");

    let mut cmd = tokio::process::Command::new(verilator);
    cmd.arg("--cc")
        .arg("--exe")
        .arg("--build")
        .arg("-j")
        .arg("0")
        .arg("-O2")
        .arg("--x-assign")
        .arg("fast")
        .arg("--x-initial")
        .arg("fast")
        .arg("--noassert")
        .arg("--threads")
        .arg("1")
        .arg("-Wno-fatal")
        .arg("-CFLAGS")
        .arg(format!("-O2 -DJADE_HW_USER_TOP={top_module}"))
        .arg("--top-module")
        .arg("jade_hw_top")
        .arg("-Mdir")
        .arg(&obj_dir)
        .arg("-o")
        .arg("jade_hw_sim")
        .arg(wrapper)
        .args(sources)
        .arg(harness)
        .current_dir(project_root)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    // Verilator honors OBJCACHE on its own; advertise ccache when installed
    // and the caller has not set it.
    if std::env::var_os("OBJCACHE").is_none() && which_exists("ccache") {
        cmd.env("OBJCACHE", "ccache");
    }

    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => {
            return HwBuildResult::failed(
                vec![BuildError {
                    file: wrapper.to_path_buf(),
                    line: 1,
                    column: 1,
                    message: format!("failed to run verilator: {e}"),
                    severity: Severity::Error,
                }],
                started,
            )
        }
    };

    let mut so = BufReader::new(child.stdout.take().expect("piped")).lines();
    let mut se = BufReader::new(child.stderr.take().expect("piped")).lines();
    let (mut so_done, mut se_done) = (false, false);
    let mut stderr_text = String::new();
    let mut canceled = false;

    loop {
        tokio::select! {
            _ = &mut abort, if !canceled => {
                let _ = child.start_kill();
                canceled = true;
            }
            line = so.next_line(), if !so_done => match line {
                Ok(Some(l)) => { let _ = line_tx.send(l); }
                _ => so_done = true,
            },
            line = se.next_line(), if !se_done => match line {
                Ok(Some(l)) => {
                    stderr_text.push_str(&l);
                    stderr_text.push('\n');
                    let _ = line_tx.send(l);
                }
                _ => se_done = true,
            },
        }
        if so_done && se_done {
            break;
        }
    }
    let status = child.wait().await;
    let ok = !canceled && status.map(|s| s.success()).unwrap_or(false);

    let mut errors = parse_verilator_errors(&stderr_text);
    rewrite_wrapper_errors(&mut errors, wrapper, qsf_path);

    let binary = obj_dir.join("jade_hw_sim");
    HwBuildResult {
        success: ok && binary.is_file(),
        binary: if ok && binary.is_file() { Some(binary) } else { None },
        errors,
        duration: started.elapsed(),
        canceled,
    }
}

fn which_exists(tool: &str) -> bool {
    let path = std::env::var_os("PATH").unwrap_or_default();
    std::env::split_paths(&path)
        .chain([PathBuf::from("/opt/homebrew/bin"), PathBuf::from("/usr/local/bin")])
        .any(|d| d.join(tool).is_file())
}

/// Parse Verilator stderr into diagnostics.
///
/// Grammar: `%Error: file:line:col: message`, `%Warning-ID: file:line:col:
/// message`, plus location-free lines (`%Error: Cannot continue`) and `... `
/// continuation/context lines, which do not create diagnostics.
pub fn parse_verilator_errors(stderr: &str) -> Vec<BuildError> {
    let re = Regex::new(
        r"^%(Error|Warning)(?:-([A-Za-z0-9_]+))?:\s+(?:([^\s:][^:]*):(\d+):(?:(\d+):)?\s*)?(.*)$",
    )
    .expect("static regex");
    let mut out = Vec::new();
    for line in stderr.lines() {
        let Some(caps) = re.captures(line) else { continue };
        let severity = if &caps[1] == "Error" {
            Severity::Error
        } else {
            Severity::Warning
        };
        let Some(file) = caps.get(3) else {
            // A location-free line, for example "%Error: Cannot continue".
            continue;
        };
        let line_no: u32 = caps.get(4).and_then(|m| m.as_str().parse().ok()).unwrap_or(1);
        let column: u32 = caps.get(5).and_then(|m| m.as_str().parse().ok()).unwrap_or(1);
        let mut message = caps.get(6).map(|m| m.as_str()).unwrap_or("").to_string();
        if let Some(id) = caps.get(2) {
            message = format!("[{}] {}", id.as_str(), message);
        }
        out.push(BuildError {
            file: PathBuf::from(file.as_str()),
            line: line_no,
            column,
            message,
            severity,
        });
    }
    out
}

/// Errors inside the generated wrapper name the `.qsf` instead, so the user
/// lands in a file that exists in the project.
fn rewrite_wrapper_errors(errors: &mut [BuildError], wrapper: &Path, qsf_path: Option<&Path>) {
    let wrapper_name = wrapper.file_name().map(|s| s.to_string_lossy().into_owned());
    for e in errors.iter_mut() {
        let name = e.file.file_name().map(|s| s.to_string_lossy().into_owned());
        if name.is_some() && name == wrapper_name {
            e.message = format!(
                "in the generated board wrapper (check the pin assignments): {}",
                e.message
            );
            if let Some(qsf) = qsf_path {
                e.file = qsf.to_path_buf();
                e.line = 1;
                e.column = 1;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_canned_verilator_output() {
        let stderr = "\
%Error: blink.v:7:15: syntax error, unexpected IDENTIFIER
    7 |     output wir [4:0] led
      |               ^~~
%Warning-WIDTH: blink.v:13:12: Operator ASSIGNW expects 5 bits on the Assign RHS, but Assign RHS's SEL generates 4 bits.
                             : ... note: In instance 'blink'
%Error: Cannot continue
";
        let errors = parse_verilator_errors(stderr);
        assert_eq!(errors.len(), 2);
        assert_eq!(errors[0].file, PathBuf::from("blink.v"));
        assert_eq!(errors[0].line, 7);
        assert_eq!(errors[0].column, 15);
        assert_eq!(errors[0].severity, Severity::Error);
        assert!(errors[0].message.contains("syntax error"));
        assert_eq!(errors[1].severity, Severity::Warning);
        assert!(errors[1].message.starts_with("[WIDTH]"));
        assert_eq!(errors[1].line, 13);
    }

    #[test]
    fn rewrites_wrapper_errors_to_the_qsf() {
        let mut errors = vec![BuildError {
            file: PathBuf::from(".jade/hw/jade_hw_top.v"),
            line: 14,
            column: 9,
            message: "Pin not found".into(),
            severity: Severity::Error,
        }];
        rewrite_wrapper_errors(
            &mut errors,
            Path::new("/p/.jade/hw/jade_hw_top.v"),
            Some(Path::new("/p/blink.qsf")),
        );
        assert_eq!(errors[0].file, PathBuf::from("/p/blink.qsf"));
        assert_eq!(errors[0].line, 1);
        assert!(errors[0].message.contains("generated board wrapper"));
    }

    #[test]
    fn source_list_prefers_qsf_and_skips_testbenches() {
        let dir = std::env::temp_dir().join(format!("jade_hw_srcs_{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        std::fs::write(dir.join("blink.v"), "").unwrap();
        std::fs::write(dir.join("tb_blink.v"), "").unwrap();
        std::fs::write(dir.join("uart.v"), "").unwrap();

        let globbed = source_list(&dir, None);
        assert_eq!(globbed.len(), 2);
        assert!(globbed.iter().all(|p| !p.ends_with("tb_blink.v")));

        let qsf = crate::qsf::parse_qsf(
            &dir.join("x.qsf"),
            "set_global_assignment -name VERILOG_FILE blink.v\n",
        );
        let listed = source_list(&dir, Some(&qsf));
        assert_eq!(listed, vec![dir.join("blink.v")]);

        // The schematic sees every root source, not only the qsf list, so it
        // can draw a module the board build leaves out.
        let wide = schematic_source_list(&dir, Some(&qsf));
        assert_eq!(wide, vec![dir.join("blink.v"), dir.join("uart.v")]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn module_names_skip_comments() {
        let src = "// module commented\n\
                   /* module blocked\n\
                    */\n\
                   module uart #(parameter N = 1) (input clk);\n\
                   endmodule\n\
                   module uart_rx (input clk);\n\
                   endmodule\n";
        assert_eq!(module_names(src), vec!["uart", "uart_rx"]);
    }

    #[test]
    fn module_for_file_prefers_the_stem() {
        let dir = std::env::temp_dir().join(format!("jade_hw_mods_{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);

        // Two modules in one file: the one named like the file wins, even
        // though it is declared second.
        let two = dir.join("uart.v");
        std::fs::write(&two, "module uart_rx();\nendmodule\nmodule uart();\nendmodule\n")
            .unwrap();
        assert_eq!(module_for_file(&two), Some("uart".to_string()));

        // No name matches the stem: the first declaration wins.
        let odd = dir.join("design.v");
        std::fs::write(&odd, "module alpha();\nendmodule\nmodule beta();\nendmodule\n").unwrap();
        assert_eq!(module_for_file(&odd), Some("alpha".to_string()));

        // An empty file declares nothing.
        let empty = dir.join("systolic.v");
        std::fs::write(&empty, "").unwrap();
        assert_eq!(module_for_file(&empty), None);

        let _ = std::fs::remove_dir_all(&dir);
    }
}
