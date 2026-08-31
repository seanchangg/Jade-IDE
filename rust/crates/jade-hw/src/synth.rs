//! Gate-level netlist synthesis through Yosys.
//!
//! The RTL view (`netlist.rs`) draws the design as written: a `*` stays one
//! multiplier block. This module runs a real synthesis and shows the actual
//! circuit. Yosys lowers the design (`synth`, then `abc -g simple`) to
//! AND/OR/XOR/MUX gates, inverters, and one flip-flop per state bit, and its
//! `write_json` dump becomes the same [`Netlist`] shape the schematic view
//! already draws.
//!
//! Like the RTL view, a flip-flop shows its clock as text, not as a wire, so
//! the sheet stays readable.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use serde_json::Value;

use crate::netlist::{NetEdge, NetNode, Netlist, NodeKind};

/// Find a usable Yosys. GUI apps get a thin `$PATH`, so the probe also
/// checks the Homebrew locations.
pub fn detect_yosys() -> Option<PathBuf> {
    let mut candidates: Vec<PathBuf> = vec![PathBuf::from("yosys")];
    for dir in ["/opt/homebrew/bin", "/usr/local/bin"] {
        candidates.push(Path::new(dir).join("yosys"));
    }
    candidates.into_iter().find(|cand| {
        std::process::Command::new(cand)
            .arg("--version")
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false)
    })
}

/// Run the synthesis and return the `write_json` text.
///
/// `abc -g simple` restricts the gate library to AND, OR, XOR, MUX, and NOT,
/// so the drawing vocabulary stays small. Flip-flops arrive as `$_DFF_*_`
/// cells from `techmap`.
pub async fn run_yosys(
    yosys: &Path,
    top: &str,
    sources: &[PathBuf],
    out_json: &Path,
    cwd: &Path,
) -> Result<String, String> {
    let script = format!("synth -top {top} -flatten -noabc; abc -g simple; opt_clean");
    let out = tokio::process::Command::new(yosys)
        .arg("-q")
        .arg("-p")
        .arg(&script)
        .arg("-o")
        .arg(out_json)
        .args(sources)
        .current_dir(cwd)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .output()
        .await
        .map_err(|e| format!("failed to run yosys: {e}"))?;
    if !out.status.success() {
        return Err(String::from_utf8_lossy(&out.stderr).into_owned());
    }
    tokio::fs::read_to_string(out_json)
        .await
        .map_err(|e| format!("cannot read {}: {e}", out_json.display()))
}

/// The display label for one Yosys gate cell type.
fn gate_label(ty: &str) -> String {
    match ty {
        "$_NOT_" => "~",
        "$_AND_" => "&",
        "$_NAND_" => "~&",
        "$_OR_" => "|",
        "$_NOR_" => "~|",
        "$_XOR_" => "^",
        "$_XNOR_" => "^~",
        "$_ANDNOT_" => "&~",
        "$_ORNOT_" => "|~",
        "$_MUX_" => "MUX",
        "$_BUF_" => "BUF",
        // Anything else keeps its cell name, without the `$_ _` wrapper.
        other => {
            return other
                .trim_start_matches("$_")
                .trim_start_matches('$')
                .trim_end_matches('_')
                .to_string()
        }
    }
    .to_string()
}

/// True for the cell types that hold state and draw as a register.
fn is_ff(ty: &str) -> bool {
    ty.starts_with("$_DFF")
        || ty.starts_with("$_SDFF")
        || ty.starts_with("$_DLATCH")
        || ty.starts_with("$_SR")
}

/// The port that carries the clock (or the latch gate). It draws as text on
/// the register, never as a wire.
fn clock_port(ty: &str) -> &'static str {
    if ty.starts_with("$_DLATCH") {
        "E"
    } else {
        "C"
    }
}

/// Input pin order on a cell. The mux matches the view's `s`/`1`/`0` marks:
/// `$_MUX_` computes `Y = S ? B : A`.
fn pin_key(ty: &str, port: &str) -> (u32, String) {
    let rank = if ty == "$_MUX_" {
        match port {
            "S" => 0,
            "B" => 1,
            "A" => 2,
            _ => 3,
        }
    } else if is_ff(ty) {
        if port == "D" {
            0
        } else {
            1
        }
    } else {
        1
    };
    (rank, port.to_string())
}

fn push_node(nodes: &mut Vec<NetNode>, kind: NodeKind, label: String, width: u32) -> usize {
    let id = nodes.len();
    nodes.push(NetNode {
        id,
        kind,
        label,
        width,
        clock: None,
        layer: 0,
    });
    id
}

/// The source node for one connection bit: a driver from `driver`, or a
/// fresh constant node for a `"0"` / `"1"` / `"x"` bit.
fn source_for(
    bit: &Value,
    driver: &HashMap<u64, (usize, Option<String>)>,
    nodes: &mut Vec<NetNode>,
) -> (usize, Option<String>) {
    match bit {
        Value::Number(n) => match n.as_u64().and_then(|b| driver.get(&b)) {
            Some((id, label)) => (*id, label.clone()),
            None => (push_node(nodes, NodeKind::Const, "x".into(), 1), None),
        },
        Value::String(s) => (push_node(nodes, NodeKind::Const, s.clone(), 1), None),
        _ => (push_node(nodes, NodeKind::Const, "x".into(), 1), None),
    }
}

/// Parse a Yosys `write_json` dump into the schematic graph.
///
/// The module named `top` wins; a module marked with the `top` attribute is
/// the fallback (Yosys can rename during synthesis). Returns `None` when no
/// module or nothing to draw exists.
pub fn parse_yosys_json(json_text: &str, top: &str) -> Option<Netlist> {
    let root: Value = serde_json::from_str(json_text).ok()?;
    let modules = root.get("modules")?.as_object()?;
    let (mod_name, module) = modules
        .get_key_value(top)
        .or_else(|| {
            modules.iter().find(|(_, m)| {
                m.get("attributes")
                    .and_then(|a| a.get("top"))
                    .is_some()
            })
        })
        .or_else(|| {
            if modules.len() == 1 {
                modules.iter().next()
            } else {
                None
            }
        })?;

    // Net names, per bit. A visible name beats a Yosys-generated one.
    let mut names: HashMap<u64, (String, bool)> = HashMap::new();
    if let Some(nets) = module.get("netnames").and_then(Value::as_object) {
        for (name, nn) in nets {
            let visible = nn.get("hide_name").and_then(Value::as_u64) == Some(0);
            let Some(bits) = nn.get("bits").and_then(Value::as_array) else {
                continue;
            };
            for (i, b) in bits.iter().enumerate() {
                let Some(b) = b.as_u64() else { continue };
                let label = if bits.len() > 1 {
                    format!("{name}[{i}]")
                } else {
                    name.clone()
                };
                match names.get(&b) {
                    Some((_, true)) => {}
                    Some((_, false)) if !visible => {}
                    _ => {
                        names.insert(b, (label, visible));
                    }
                }
            }
        }
    }
    let mut nodes: Vec<NetNode> = Vec::new();
    let mut edges: Vec<NetEdge> = Vec::new();
    // Bit number → (driving node, slice label for the edge).
    let mut driver: HashMap<u64, (usize, Option<String>)> = HashMap::new();

    // Ports first, so inputs and outputs stack in declaration order.
    let mut outputs: Vec<(usize, Vec<Value>)> = Vec::new();
    if let Some(ports) = module.get("ports").and_then(Value::as_object) {
        for (name, p) in ports {
            let dir = p.get("direction").and_then(Value::as_str).unwrap_or("");
            let bits = p
                .get("bits")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default();
            let width = bits.len().max(1) as u32;
            // A port name is a visible net name for its bits (a dump does
            // not always repeat the ports under `netnames`).
            for (i, b) in bits.iter().enumerate() {
                let Some(b) = b.as_u64() else { continue };
                if matches!(names.get(&b), Some((_, true))) {
                    continue;
                }
                let label = if bits.len() > 1 {
                    format!("{name}[{i}]")
                } else {
                    name.clone()
                };
                names.insert(b, (label, true));
            }
            match dir {
                "input" => {
                    let id = push_node(&mut nodes, NodeKind::Input, name.clone(), width);
                    for (i, b) in bits.iter().enumerate() {
                        if let Some(b) = b.as_u64() {
                            let label = (bits.len() > 1).then(|| format!("[{i}]"));
                            driver.insert(b, (id, label));
                        }
                    }
                }
                "output" => {
                    let id = push_node(&mut nodes, NodeKind::Output, name.clone(), width);
                    outputs.push((id, bits));
                }
                _ => {}
            }
        }
    }

    let name_of = |bit: u64| names.get(&bit).map(|(n, _)| n.clone());

    // Cells: first pass creates the nodes and registers every output bit as
    // a driver, so wire-up never depends on cell order.
    let cells = module.get("cells").and_then(Value::as_object);
    let mut cell_nodes: Vec<(usize, &str, &Value)> = Vec::new();
    if let Some(cells) = cells {
        for (_cname, cell) in cells {
            let ty = cell.get("type").and_then(Value::as_str).unwrap_or("?");
            let conns = cell.get("connections").and_then(Value::as_object);
            let conn_bit = |port: &str| -> Option<u64> {
                conns?
                    .get(port)
                    .and_then(Value::as_array)
                    .and_then(|a| a.first())
                    .and_then(Value::as_u64)
            };
            let id = if is_ff(ty) {
                let label = conn_bit("Q")
                    .and_then(name_of)
                    .unwrap_or_else(|| "ff".into());
                let id = push_node(&mut nodes, NodeKind::Reg, label, 1);
                nodes[id].clock = conn_bit(clock_port(ty)).and_then(name_of);
                id
            } else {
                push_node(&mut nodes, NodeKind::Op, gate_label(ty), 1)
            };
            if let (Some(conns), Some(dirs)) = (
                conns,
                cell.get("port_directions").and_then(Value::as_object),
            ) {
                for (pname, pdir) in dirs {
                    if pdir.as_str() != Some("output") {
                        continue;
                    }
                    let Some(bits) = conns.get(pname).and_then(Value::as_array) else {
                        continue;
                    };
                    for b in bits {
                        if let Some(b) = b.as_u64() {
                            driver.insert(b, (id, None));
                        }
                    }
                }
            }
            cell_nodes.push((id, ty, cell));
        }
    }

    // Second pass: one edge per input bit. The clock pin stays off the sheet.
    for (id, ty, cell) in &cell_nodes {
        let Some(conns) = cell.get("connections").and_then(Value::as_object) else {
            continue;
        };
        let Some(dirs) = cell.get("port_directions").and_then(Value::as_object) else {
            continue;
        };
        let mut inputs: Vec<&String> = dirs
            .iter()
            .filter(|(_, d)| d.as_str() == Some("input"))
            .map(|(p, _)| p)
            .filter(|p| !(is_ff(ty) && p.as_str() == clock_port(ty)))
            .collect();
        inputs.sort_by_key(|p| pin_key(ty, p.as_str()));
        for (pin, port) in inputs.iter().enumerate() {
            let Some(bits) = conns.get(*port).and_then(Value::as_array) else {
                continue;
            };
            for b in bits {
                let (src, label) = source_for(b, &driver, &mut nodes);
                edges.push(NetEdge {
                    from: src,
                    to: *id,
                    to_pin: pin as u32,
                    label,
                    width: 1,
                });
            }
        }
    }

    // Output ports collect their bits, one pin per bit.
    for (out_id, bits) in outputs {
        let multi = bits.len() > 1;
        for (i, b) in bits.iter().enumerate() {
            let (src, label) = source_for(b, &driver, &mut nodes);
            let label = label.or_else(|| multi.then(|| format!("[{i}]")));
            edges.push(NetEdge {
                from: src,
                to: out_id,
                to_pin: i as u32,
                label,
                width: 1,
            });
        }
    }

    if edges.is_empty() {
        return None;
    }
    let mut nl = Netlist {
        top: mod_name.clone(),
        nodes,
        edges,
    };
    crate::netlist::assign_layers(&mut nl);
    Some(nl)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn gates_json() -> String {
        // The shape yosys `write_json` emits, hand-built: two-bit input `a`,
        // an AND, a DFF, a MUX with a constant leg, and a two-bit output.
        r#"{
          "modules": {
            "blink": {
              "attributes": {"top": "00000000000000000000000000000001"},
              "ports": {
                "clk": {"direction": "input", "bits": [2]},
                "a":   {"direction": "input", "bits": [3, 4]},
                "y":   {"direction": "output", "bits": [8, 6]}
              },
              "cells": {
                "and1": {
                  "type": "$_AND_",
                  "port_directions": {"A": "input", "B": "input", "Y": "output"},
                  "connections": {"A": [3], "B": [4], "Y": [5]}
                },
                "ff1": {
                  "type": "$_DFF_P_",
                  "port_directions": {"C": "input", "D": "input", "Q": "output"},
                  "connections": {"C": [2], "D": [5], "Q": [6]}
                },
                "mux1": {
                  "type": "$_MUX_",
                  "port_directions": {"A": "input", "B": "input", "S": "input", "Y": "output"},
                  "connections": {"A": [5], "B": ["1"], "S": [6], "Y": [8]}
                }
              },
              "netnames": {
                "state": {"hide_name": 0, "bits": [6]},
                "$abc$1$new_n5": {"hide_name": 1, "bits": [5]}
              }
            }
          }
        }"#
        .to_string()
    }

    #[test]
    fn gate_graph_has_ports_gates_and_the_ff() {
        let nl = parse_yosys_json(&gates_json(), "blink").expect("netlist");
        let find = |label: &str| nl.nodes.iter().find(|n| n.label == label).unwrap();

        let a = find("a");
        assert_eq!(a.kind, NodeKind::Input);
        assert_eq!(a.width, 2);
        let y = find("y");
        assert_eq!(y.kind, NodeKind::Output);
        assert_eq!(y.width, 2);

        // The AND takes a[0] and a[1] as slice-labeled edges.
        let and = find("&");
        assert_eq!(and.kind, NodeKind::Op);
        let and_labels: Vec<_> = nl
            .edges
            .iter()
            .filter(|e| e.to == and.id)
            .filter_map(|e| e.label.clone())
            .collect();
        assert!(and_labels.contains(&"[0]".to_string()), "{and_labels:?}");
        assert!(and_labels.contains(&"[1]".to_string()), "{and_labels:?}");

        // The flip-flop takes the visible net name and the clock as text.
        let ff = find("state");
        assert_eq!(ff.kind, NodeKind::Reg);
        assert_eq!(ff.clock.as_deref(), Some("clk"));
        // No edge arrives from clk: the clock is not a wire.
        let clk = find("clk");
        assert!(!nl.edges.iter().any(|e| e.from == clk.id));
        // The AND drives the D pin.
        assert!(nl.edges.iter().any(|e| e.from == and.id && e.to == ff.id));

        // The mux: select from the ff (pin 0), constant 1 (pin 1), AND (pin 2).
        let mux = find("MUX");
        let pin_of = |from: usize| {
            nl.edges
                .iter()
                .find(|e| e.to == mux.id && e.from == from)
                .map(|e| e.to_pin)
        };
        assert_eq!(pin_of(ff.id), Some(0));
        assert_eq!(pin_of(and.id), Some(2));
        let one = nl.nodes.iter().find(|n| n.label == "1").unwrap();
        assert_eq!(pin_of(one.id), Some(1));

        // The output collects mux → y[0], ff → y[1].
        let y_edges: Vec<_> = nl.edges.iter().filter(|e| e.to == y.id).collect();
        assert_eq!(y_edges.len(), 2);
        assert!(y_edges
            .iter()
            .any(|e| e.from == mux.id && e.to_pin == 0 && e.label.as_deref() == Some("[0]")));
        assert!(y_edges
            .iter()
            .any(|e| e.from == ff.id && e.to_pin == 1 && e.label.as_deref() == Some("[1]")));
    }

    #[test]
    fn layers_put_inputs_and_ffs_left_and_outputs_right() {
        let nl = parse_yosys_json(&gates_json(), "blink").expect("netlist");
        let by_label = |label: &str| nl.nodes.iter().find(|n| n.label == label).unwrap();
        assert_eq!(by_label("clk").layer, 0);
        assert_eq!(by_label("state").layer, 0);
        assert!(by_label("&").layer >= 1);
        assert_eq!(by_label("y").layer, nl.columns() - 1);
    }

    #[test]
    fn top_attribute_is_the_fallback_module() {
        // The asked-for name is absent; the module with the top attribute wins.
        let nl = parse_yosys_json(&gates_json(), "missing").expect("netlist");
        assert_eq!(nl.top, "blink");
    }

    #[test]
    fn unknown_cells_keep_a_trimmed_name() {
        assert_eq!(gate_label("$_AOI3_"), "AOI3");
        assert_eq!(gate_label("$alu"), "alu");
        assert_eq!(gate_label("$_XNOR_"), "^~");
    }

    #[test]
    fn empty_or_bad_input_is_none() {
        assert!(parse_yosys_json("{}", "blink").is_none());
        assert!(parse_yosys_json("not json", "blink").is_none());
        let empty = r#"{"modules": {"blink": {"ports": {}, "cells": {}}}}"#;
        assert!(parse_yosys_json(empty, "blink").is_none());
    }
}
