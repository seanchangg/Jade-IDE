//! Port discovery through Verilator's parser.
//!
//! The engine never parses Verilog in Rust. A pre-pass runs
//! `verilator --json-only` and reads the AST dump: the `VAR` nodes of the top
//! module carry `direction`, and the type table carries the bit range. The
//! plan named the older `--xml-only` flag; Verilator 5.050 removed it, so the
//! pre-pass uses the JSON dump, which holds the same data.

use std::collections::HashMap;
use std::path::Path;

use serde_json::Value;

/// Port direction.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PortDir {
    Input,
    Output,
    Inout,
}

/// One port of the top module.
#[derive(Debug, Clone, PartialEq)]
pub struct Port {
    pub name: String,
    pub dir: PortDir,
    pub msb: u32,
    pub lsb: u32,
    /// `false` for a scalar port declared without a range.
    pub has_range: bool,
}

impl Port {
    pub fn width(&self) -> u32 {
        self.msb - self.lsb + 1
    }
}

/// The discovered ports of the top module.
#[derive(Debug, Clone, PartialEq)]
pub struct TopPorts {
    pub module: String,
    pub ports: Vec<Port>,
}

impl TopPorts {
    pub fn port(&self, name: &str) -> Option<&Port> {
        self.ports.iter().find(|p| p.name == name)
    }
}

/// Parse a `--json-only` AST dump. Return the ports of the module named
/// `top`, or an error message when the dump does not contain it.
pub fn parse_ports_json(json_text: &str, top: &str) -> Result<TopPorts, String> {
    let root: Value =
        serde_json::from_str(json_text).map_err(|e| format!("bad verilator JSON: {e}"))?;

    // The type table: BASICDTYPE addr → (msb, lsb, has_range).
    let mut dtypes: HashMap<String, (u32, u32, bool)> = HashMap::new();
    collect_dtypes(&root, &mut dtypes);

    // The top module by name.
    let mut module: Option<&Value> = None;
    find_module(&root, top, &mut module);
    let module = module.ok_or_else(|| format!("module `{top}` not found by verilator"))?;

    let mut ports = Vec::new();
    if let Some(stmts) = module.get("stmtsp").and_then(Value::as_array) {
        for node in stmts {
            if node.get("type").and_then(Value::as_str) != Some("VAR") {
                continue;
            }
            let dir = match node.get("direction").and_then(Value::as_str) {
                Some("INPUT") => PortDir::Input,
                Some("OUTPUT") => PortDir::Output,
                Some("INOUT") => PortDir::Inout,
                _ => continue,
            };
            let Some(name) = node.get("name").and_then(Value::as_str) else {
                continue;
            };
            let (msb, lsb, has_range) = node
                .get("dtypep")
                .and_then(Value::as_str)
                .and_then(|addr| dtypes.get(addr).copied())
                .unwrap_or((0, 0, false));
            ports.push(Port {
                name: name.to_string(),
                dir,
                msb,
                lsb,
                has_range,
            });
        }
    }
    Ok(TopPorts {
        module: top.to_string(),
        ports,
    })
}

/// Run `verilator --json-only` over `sources` with `top` as the root. The AST
/// dump goes to `tree` and its metadata to `meta`. Returns the dump text.
///
/// `-Wno-fatal` matches the real build, so a lint warning on an unrelated
/// source cannot fail the pre-pass.
pub async fn run_json_only(
    verilator: &Path,
    top: &str,
    sources: &[std::path::PathBuf],
    tree: &Path,
    meta: &Path,
    cwd: &Path,
) -> Result<String, String> {
    let mut cmd = tokio::process::Command::new(verilator);
    cmd.arg("--json-only")
        .arg("-Wno-fatal")
        .arg("--top-module")
        .arg(top)
        .arg("--json-only-output")
        .arg(tree)
        .arg("--json-only-meta-output")
        .arg(meta)
        .args(sources)
        .current_dir(cwd)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    let out = cmd
        .output()
        .await
        .map_err(|e| format!("failed to run verilator: {e}"))?;
    if !out.status.success() {
        return Err(String::from_utf8_lossy(&out.stderr).into_owned());
    }
    tokio::fs::read_to_string(tree)
        .await
        .map_err(|e| format!("cannot read {}: {e}", tree.display()))
}

/// Run the pre-pass: `verilator --json-only` over `sources`, dump into
/// `out_dir`, and parse the result.
pub async fn discover_ports(
    verilator: &Path,
    top: &str,
    sources: &[std::path::PathBuf],
    out_dir: &Path,
    cwd: &Path,
) -> Result<TopPorts, String> {
    let tree = out_dir.join("ports.tree.json");
    let meta = out_dir.join("ports.meta.json");
    let text = run_json_only(verilator, top, sources, &tree, &meta, cwd).await?;
    parse_ports_json(&text, top)
}

pub(crate) fn collect_dtypes(node: &Value, out: &mut HashMap<String, (u32, u32, bool)>) {
    match node {
        Value::Object(map) => {
            if map.get("type").and_then(Value::as_str) == Some("BASICDTYPE") {
                if let Some(addr) = map.get("addr").and_then(Value::as_str) {
                    let range = map
                        .get("range")
                        .and_then(Value::as_str)
                        .and_then(parse_range);
                    let (msb, lsb, has_range) = match range {
                        Some((m, l)) => (m, l, true),
                        None => (0, 0, false),
                    };
                    out.insert(addr.to_string(), (msb, lsb, has_range));
                }
            }
            for v in map.values() {
                collect_dtypes(v, out);
            }
        }
        Value::Array(items) => {
            for v in items {
                collect_dtypes(v, out);
            }
        }
        _ => {}
    }
}

fn parse_range(s: &str) -> Option<(u32, u32)> {
    let (msb, lsb) = s.split_once(':')?;
    Some((msb.trim().parse().ok()?, lsb.trim().parse().ok()?))
}

fn find_module<'a>(node: &'a Value, top: &str, out: &mut Option<&'a Value>) {
    if out.is_some() {
        return;
    }
    match node {
        Value::Object(map) => {
            if map.get("type").and_then(Value::as_str) == Some("MODULE")
                && map.get("name").and_then(Value::as_str) == Some(top)
            {
                *out = Some(node);
                return;
            }
            for v in map.values() {
                find_module(v, top, out);
            }
        }
        Value::Array(items) => {
            for v in items {
                find_module(v, top, out);
            }
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A trimmed `--json-only` dump for blink.v (Verilator 5.050 shape).
    const BLINK_JSON: &str = r#"{
      "type":"NETLIST","name":"$root",
      "typeTablep":[
        {"type":"TYPETABLE","typesp":[
          {"type":"BASICDTYPE","name":"logic","addr":"(G)","keyword":"logic","generic":true},
          {"type":"BASICDTYPE","name":"logic","addr":"(J)","keyword":"logic","range":"4:0","generic":true},
          {"type":"BASICDTYPE","name":"logic","addr":"(L)","keyword":"logic","range":"25:0","generic":true}
        ]}
      ],
      "modulesp":[
        {"type":"MODULE","name":"blink","level":1,"stmtsp":[
          {"type":"VAR","name":"clk","dtypep":"(G)","isPrimaryIO":true,"direction":"INPUT","varType":"WIRE"},
          {"type":"VAR","name":"button","dtypep":"(G)","isPrimaryIO":true,"direction":"INPUT","varType":"WIRE"},
          {"type":"VAR","name":"led","dtypep":"(J)","isPrimaryIO":true,"direction":"OUTPUT","varType":"WIRE"},
          {"type":"VAR","name":"count","dtypep":"(L)","direction":"NONE","varType":"VAR"}
        ]},
        {"type":"MODULE","name":"@CONST-POOL@"}
      ]
    }"#;

    #[test]
    fn parses_blink_ports() {
        let tp = parse_ports_json(BLINK_JSON, "blink").unwrap();
        assert_eq!(tp.module, "blink");
        assert_eq!(tp.ports.len(), 3);
        let clk = tp.port("clk").unwrap();
        assert_eq!(clk.dir, PortDir::Input);
        assert_eq!(clk.width(), 1);
        assert!(!clk.has_range);
        let led = tp.port("led").unwrap();
        assert_eq!(led.dir, PortDir::Output);
        assert_eq!((led.msb, led.lsb), (4, 0));
        assert_eq!(led.width(), 5);
        // Internal signals do not become ports.
        assert!(tp.port("count").is_none());
    }

    #[test]
    fn missing_module_is_an_error() {
        assert!(parse_ports_json(BLINK_JSON, "nope").is_err());
        assert!(parse_ports_json("not json", "blink").is_err());
    }
}
