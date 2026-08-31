//! RTL netlist extraction from Verilator's `--json-only` AST dump.
//!
//! The schematic view renders this graph: ports on the outside, the
//! effective logic in between — operator blocks (adders, muxes, gates),
//! registers with their clock, named wires, and constants. Verilator has
//! already elaborated the design (an `if` inside `always` arrives as a
//! `COND`), so the walk here is a direct translation, not a synthesis.
//!
//! Bit selects (`count[25:22]`) do not become blocks; they ride on the edge
//! as a label, like an RTL viewer's slice annotation.

use std::collections::HashMap;

use serde_json::Value;

/// What one graph node is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NodeKind {
    Input,
    Output,
    /// A clocked register (`<=` target).
    Reg,
    /// A named intermediate wire.
    Wire,
    /// An operator block (adder, mux, gate, concat …).
    Op,
    Const,
}

/// One node of the schematic graph.
#[derive(Debug, Clone, PartialEq)]
pub struct NetNode {
    pub id: usize,
    pub kind: NodeKind,
    /// Display label: the signal name, the operator glyph, or the literal.
    pub label: String,
    /// Bit width of the node's result.
    pub width: u32,
    /// The clock signal name, for a register.
    pub clock: Option<String>,
    /// Drawing column, 0 = leftmost. Computed by [`assign_layers`].
    pub layer: u32,
}

/// One wire of the schematic graph, from a node's result into an input pin.
#[derive(Debug, Clone, PartialEq)]
pub struct NetEdge {
    pub from: usize,
    pub to: usize,
    /// Input pin index on the target (mux: 0 = select, 1 = then, 2 = else).
    pub to_pin: u32,
    /// A slice annotation like `[25:22]`, when the source is bit-selected.
    pub label: Option<String>,
    pub width: u32,
}

/// The extracted design graph.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Netlist {
    pub top: String,
    pub nodes: Vec<NetNode>,
    pub edges: Vec<NetEdge>,
}

impl Netlist {
    pub fn node(&self, id: usize) -> &NetNode {
        &self.nodes[id]
    }

    /// The number of drawing columns (max layer + 1).
    pub fn columns(&self) -> u32 {
        self.nodes.iter().map(|n| n.layer + 1).max().unwrap_or(0)
    }
}

/// Operator display labels: the operator exactly as Verilog spells it, no
/// typographic substitutes. Anything unknown falls back to its AST name.
fn op_label(t: &str) -> &str {
    match t {
        "ADD" => "+",
        "SUB" | "NEGATE" => "-",
        "MUL" | "MULS" => "*",
        "DIV" | "DIVS" => "/",
        "MODDIV" | "MODDIVS" => "%",
        "AND" | "REDAND" => "&",
        "OR" | "REDOR" => "|",
        "XOR" | "REDXOR" => "^",
        "XNOR" | "REDXNOR" => "^~",
        "NOT" => "~",
        "LOGAND" => "&&",
        "LOGOR" => "||",
        "LOGNOT" => "!",
        "SHIFTL" => "<<",
        "SHIFTR" | "SHIFTRS" => ">>",
        "EQ" | "EQCASE" => "==",
        "NEQ" | "NEQCASE" => "!=",
        "LT" | "LTS" => "<",
        "GT" | "GTS" => ">",
        "LTE" | "LTES" => "<=",
        "GTE" | "GTES" => ">=",
        "COND" => "MUX",
        "CONCAT" => "{…}",
        "REPLICATE" => "{n{…}}",
        other => other,
    }
}

struct Builder<'a> {
    dtypes: &'a HashMap<String, (u32, u32, bool)>,
    nodes: Vec<NetNode>,
    edges: Vec<NetEdge>,
    /// Var name → node id.
    vars: HashMap<String, usize>,
}

impl<'a> Builder<'a> {
    fn width_of(&self, node: &Value) -> u32 {
        node.get("dtypep")
            .and_then(Value::as_str)
            .and_then(|a| self.dtypes.get(a))
            .map(|(msb, lsb, _)| msb - lsb + 1)
            .unwrap_or(1)
    }

    fn add_node(&mut self, kind: NodeKind, label: String, width: u32) -> usize {
        let id = self.nodes.len();
        self.nodes.push(NetNode {
            id,
            kind,
            label,
            width,
            clock: None,
            layer: 0,
        });
        id
    }

    /// The node for a named variable, created on first sight as a Wire.
    fn var_node(&mut self, name: &str, width: u32) -> usize {
        if let Some(&id) = self.vars.get(name) {
            return id;
        }
        let id = self.add_node(NodeKind::Wire, name.to_string(), width);
        self.vars.insert(name.to_string(), id);
        id
    }

    /// Build the expression tree under `node`; return the source node id and
    /// an optional slice label for the edge into the consumer.
    fn expr(&mut self, node: &Value) -> (usize, Option<String>) {
        let t = node.get("type").and_then(Value::as_str).unwrap_or("");
        let width = self.width_of(node);
        match t {
            "VARREF" => {
                let name = node.get("name").and_then(Value::as_str).unwrap_or("?");
                (self.var_node(name, width), None)
            }
            "CONST" => {
                let name = node.get("name").and_then(Value::as_str).unwrap_or("0");
                (self.add_node(NodeKind::Const, name.to_string(), width), None)
            }
            // A bit select becomes an edge label on its source, not a block.
            "SEL" => {
                let from = first(node, "fromp");
                let lsb = first(node, "lsbp")
                    .and_then(|c| c.get("name").and_then(Value::as_str))
                    .and_then(parse_const);
                let sel_w = node
                    .get("widthConst")
                    .and_then(Value::as_u64)
                    .unwrap_or(width as u64) as u32;
                let Some(from) = from else {
                    return (self.add_node(NodeKind::Const, "?".into(), width), None);
                };
                let (src, inner) = self.expr(from);
                let label = lsb.map(|l| {
                    if sel_w <= 1 {
                        format!("[{l}]")
                    } else {
                        format!("[{}:{}]", l + sel_w - 1, l)
                    }
                });
                (src, label.or(inner))
            }
            // Width adapters are invisible.
            "EXTEND" | "EXTENDS" | "CCAST" => match first(node, "lhsp") {
                Some(c) => self.expr(c),
                None => (self.add_node(NodeKind::Const, "?".into(), width), None),
            },
            "COND" => {
                let op = self.add_node(NodeKind::Op, "MUX".into(), width);
                for (pin, key) in [(0u32, "condp"), (1, "thenp"), (2, "elsep")] {
                    if let Some(c) = first(node, key) {
                        let cw = self.width_of(c);
                        let (src, label) = self.expr(c);
                        self.edges.push(NetEdge { from: src, to: op, to_pin: pin, label, width: cw });
                    }
                }
                (op, None)
            }
            _ => {
                // Generic operator: unary (`lhsp`) or binary (`lhsp`+`rhsp`).
                let op = self.add_node(NodeKind::Op, op_label(t).to_string(), width);
                let mut pin = 0u32;
                for key in ["lhsp", "rhsp", "thsp"] {
                    if let Some(c) = first(node, key) {
                        let cw = self.width_of(c);
                        let (src, label) = self.expr(c);
                        self.edges.push(NetEdge { from: src, to: op, to_pin: pin, label, width: cw });
                        pin += 1;
                    }
                }
                if pin == 0 {
                    // A leaf we do not model; show it as an opaque block.
                    self.nodes[op].kind = NodeKind::Const;
                }
                (op, None)
            }
        }
    }

    /// One assignment: rhs expression flows into the lhs variable node.
    fn assign(&mut self, node: &Value, clocked: Option<&str>) {
        let Some(lhs) = first(node, "lhsp") else { return };
        let Some(rhs) = first(node, "rhsp") else { return };
        // The lhs can itself be a SEL (part assign); use the base variable.
        let mut base = lhs;
        while base.get("type").and_then(Value::as_str) == Some("SEL") {
            match first(base, "fromp") {
                Some(f) => base = f,
                None => return,
            }
        }
        let Some(name) = base.get("name").and_then(Value::as_str) else {
            return;
        };
        let width = self.width_of(base);
        let target = self.var_node(name, width);
        if let Some(clk) = clocked {
            self.nodes[target].kind = NodeKind::Reg;
            self.nodes[target].clock = Some(clk.to_string());
        }
        let rw = self.width_of(rhs);
        let (src, label) = self.expr(rhs);
        self.edges.push(NetEdge { from: src, to: target, to_pin: 0, label, width: rw });
    }

    /// Walk statements, tracking the clock of the enclosing `always @(posedge …)`.
    fn stmts(&mut self, node: &Value, clock: Option<&str>) {
        let t = node.get("type").and_then(Value::as_str).unwrap_or("");
        match t {
            "ALWAYS" => {
                // The clock comes from the sensitivity tree, when there is one.
                let clk = node
                    .get("sentreep")
                    .and_then(Value::as_array)
                    .and_then(|a| a.first())
                    .and_then(|s| find_type(s, "VARREF"))
                    .and_then(|v| v.get("name").and_then(Value::as_str))
                    .map(str::to_string);
                for key in ["stmtsp"] {
                    if let Some(kids) = node.get(key).and_then(Value::as_array) {
                        for k in kids {
                            self.stmts(k, clk.as_deref().or(clock));
                        }
                    }
                }
            }
            "BEGIN" => {
                if let Some(kids) = node.get("stmtsp").and_then(Value::as_array) {
                    for k in kids {
                        self.stmts(k, clock);
                    }
                }
            }
            "ASSIGNW" | "ASSIGN" => self.assign(node, None),
            "ASSIGNDLY" => self.assign(node, clock),
            _ => {}
        }
    }
}

fn first<'v>(node: &'v Value, key: &str) -> Option<&'v Value> {
    node.get(key).and_then(Value::as_array).and_then(|a| a.first())
}

fn find_type<'v>(node: &'v Value, want: &str) -> Option<&'v Value> {
    match node {
        Value::Object(map) => {
            if map.get("type").and_then(Value::as_str) == Some(want) {
                return Some(node);
            }
            for v in map.values() {
                if let Some(f) = find_type(v, want) {
                    return Some(f);
                }
            }
            None
        }
        Value::Array(items) => items.iter().find_map(|v| find_type(v, want)),
        _ => None,
    }
}

/// `5'h16` → 22; `4'h0` → 0; plain decimals pass through.
fn parse_const(name: &str) -> Option<u32> {
    let body = match name.split_once('\'') {
        Some((_w, rest)) => rest,
        None => return name.parse().ok(),
    };
    let body = body.trim_start_matches('s');
    let (radix, digits) = match body.split_at(1) {
        ("h", d) => (16, d),
        ("d", d) => (10, d),
        ("o", d) => (8, d),
        ("b", d) => (2, d),
        _ => (10, body),
    };
    u32::from_str_radix(digits, radix).ok()
}

/// Longest-path layering, drawn the way RTL viewers draw it: inputs AND
/// registers form the left column (a register's Q output drives the logic
/// cloud), the combinational blocks flow rightward, and the outputs share
/// the rightmost column. The register's D input arrives as a short feedback
/// wire. Constants hug their consumer's column so they never pile up on the
/// left edge.
pub(crate) fn assign_layers(nl: &mut Netlist) {
    let n = nl.nodes.len();
    let mut layer = vec![0u32; n];
    // Relaxation over forward edges; edges INTO a register are the feedback
    // path and must not push it rightward, so the graph is a DAG and n
    // passes are enough.
    for _ in 0..n {
        let mut changed = false;
        for e in &nl.edges {
            if nl.nodes[e.to].kind == NodeKind::Reg {
                continue;
            }
            if layer[e.to] < layer[e.from] + 1 {
                layer[e.to] = layer[e.from] + 1;
                changed = true;
            }
        }
        if !changed {
            break;
        }
    }
    // All outputs share the rightmost column.
    let max = layer
        .iter()
        .enumerate()
        .filter(|(i, _)| nl.nodes[*i].kind != NodeKind::Input)
        .map(|(_, l)| *l)
        .max()
        .unwrap_or(0)
        .max(1);
    for (i, node) in nl.nodes.iter_mut().enumerate() {
        node.layer = match node.kind {
            NodeKind::Input | NodeKind::Reg => 0,
            NodeKind::Output => max,
            _ => layer[i],
        };
    }
    // A constant moves next to its (single) consumer.
    let consumer_layer: Vec<Option<u32>> = (0..n)
        .map(|i| {
            nl.edges
                .iter()
                .filter(|e| e.from == i)
                .map(|e| nl.nodes[e.to].layer)
                .min()
        })
        .collect();
    for (i, node) in nl.nodes.iter_mut().enumerate() {
        if node.kind == NodeKind::Const {
            if let Some(c) = consumer_layer[i] {
                node.layer = c.saturating_sub(1);
            }
        }
    }
}

/// Parse a `--json-only` dump into the schematic graph for module `top`.
/// Returns `None` when the module is absent or holds nothing to draw.
pub fn parse_netlist_json(json_text: &str, top: &str) -> Option<Netlist> {
    let root: Value = serde_json::from_str(json_text).ok()?;
    let mut dtypes = HashMap::new();
    crate::ports::collect_dtypes(&root, &mut dtypes);

    let mut module: Option<&Value> = None;
    find_module(&root, top, &mut module);
    let module = module?;

    let mut b = Builder {
        dtypes: &dtypes,
        nodes: Vec::new(),
        edges: Vec::new(),
        vars: HashMap::new(),
    };

    // Ports first, in declaration order, so inputs stack top-down.
    let stmts = module.get("stmtsp").and_then(Value::as_array)?;
    for s in stmts {
        if s.get("type").and_then(Value::as_str) != Some("VAR") {
            continue;
        }
        let dir = s.get("direction").and_then(Value::as_str);
        let name = s.get("name").and_then(Value::as_str).unwrap_or("?");
        let width = b.width_of(s);
        match dir {
            Some("INPUT") => {
                let id = b.add_node(NodeKind::Input, name.to_string(), width);
                b.vars.insert(name.to_string(), id);
            }
            Some("OUTPUT") => {
                let id = b.add_node(NodeKind::Output, name.to_string(), width);
                b.vars.insert(name.to_string(), id);
            }
            _ => {}
        }
    }
    for s in stmts {
        b.stmts(s, None);
    }

    let mut nl = Netlist {
        top: top.to_string(),
        nodes: b.nodes,
        edges: b.edges,
    };
    if nl.edges.is_empty() {
        return None;
    }
    assign_layers(&mut nl);
    Some(nl)
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

    fn blink_json() -> String {
        // Generate nothing here: use a hand-built AST in the same shape the
        // dumps have, covering ASSIGNW, ASSIGNDLY, SEL, NOT, ADD, CONCAT.
        r#"{
          "type":"NETLIST",
          "typeTablep":[{"type":"TYPETABLE","typesp":[
            {"type":"BASICDTYPE","addr":"(1)","keyword":"logic"},
            {"type":"BASICDTYPE","addr":"(5)","keyword":"logic","range":"4:0"},
            {"type":"BASICDTYPE","addr":"(26)","keyword":"logic","range":"25:0"},
            {"type":"BASICDTYPE","addr":"(4)","keyword":"logic","range":"3:0"}
          ]}],
          "modulesp":[{"type":"MODULE","name":"blink","stmtsp":[
            {"type":"VAR","name":"clk","dtypep":"(1)","direction":"INPUT"},
            {"type":"VAR","name":"button","dtypep":"(1)","direction":"INPUT"},
            {"type":"VAR","name":"led","dtypep":"(5)","direction":"OUTPUT"},
            {"type":"VAR","name":"count","dtypep":"(26)","direction":"NONE"},
            {"type":"ALWAYS","sentreep":[{"type":"SENTREE","sensesp":[{"type":"SENITEM","edgeType":"POS","sensp":[{"type":"VARREF","name":"clk","dtypep":"(1)"}]}]}],
             "stmtsp":[{"type":"ASSIGNDLY","dtypep":"(26)",
               "rhsp":[{"type":"ADD","dtypep":"(26)",
                 "lhsp":[{"type":"VARREF","name":"count","dtypep":"(26)"}],
                 "rhsp":[{"type":"CONST","name":"26'h1","dtypep":"(26)"}]}],
               "lhsp":[{"type":"VARREF","name":"count","dtypep":"(26)"}]}]},
            {"type":"ALWAYS","stmtsp":[{"type":"ASSIGNW","dtypep":"(5)",
               "rhsp":[{"type":"NOT","dtypep":"(5)",
                 "lhsp":[{"type":"CONCAT","dtypep":"(5)",
                   "lhsp":[{"type":"SEL","dtypep":"(4)","widthConst":4,
                     "fromp":[{"type":"VARREF","name":"count","dtypep":"(26)"}],
                     "lsbp":[{"type":"CONST","name":"5'h16","dtypep":"(1)"}]}],
                   "rhsp":[{"type":"SEL","dtypep":"(1)","widthConst":1,
                     "fromp":[{"type":"VARREF","name":"count","dtypep":"(26)"}],
                     "lsbp":[{"type":"CONST","name":"5'h19","dtypep":"(1)"}]}]}]}],
               "lhsp":[{"type":"VARREF","name":"led","dtypep":"(5)"}]}]}
          ]}]
        }"#
        .to_string()
    }

    #[test]
    fn blink_graph_has_ports_reg_and_ops() {
        let nl = parse_netlist_json(&blink_json(), "blink").expect("netlist");
        let find = |label: &str| nl.nodes.iter().find(|n| n.label == label);

        let clk = find("clk").unwrap();
        assert_eq!(clk.kind, NodeKind::Input);
        let led = find("led").unwrap();
        assert_eq!(led.kind, NodeKind::Output);
        assert_eq!(led.width, 5);

        // `count` is a register clocked by clk.
        let count = find("count").unwrap();
        assert_eq!(count.kind, NodeKind::Reg);
        assert_eq!(count.clock.as_deref(), Some("clk"));
        assert_eq!(count.width, 26);

        // The adder feeds the register; the NOT feeds the output.
        let add = nl.nodes.iter().find(|n| n.label == "+").unwrap();
        assert!(nl
            .edges
            .iter()
            .any(|e| e.from == add.id && e.to == count.id));
        let not = nl.nodes.iter().find(|n| n.label == "~").unwrap();
        assert!(nl.edges.iter().any(|e| e.from == not.id && e.to == led.id));

        // The bit selects ride as edge labels into the concat.
        let concat = nl.nodes.iter().find(|n| n.label == "{…}").unwrap();
        let slice_labels: Vec<_> = nl
            .edges
            .iter()
            .filter(|e| e.to == concat.id)
            .filter_map(|e| e.label.clone())
            .collect();
        assert!(slice_labels.contains(&"[25:22]".to_string()), "{slice_labels:?}");
        assert!(slice_labels.contains(&"[25]".to_string()), "{slice_labels:?}");
    }

    #[test]
    fn layers_flow_left_to_right_with_register_feedback() {
        let nl = parse_netlist_json(&blink_json(), "blink").expect("netlist");
        let by_label = |label: &str| nl.nodes.iter().find(|n| n.label == label).unwrap();
        let clk = by_label("clk");
        let add = by_label("+");
        let count = by_label("count");
        let led = by_label("led");
        let one = by_label("26'h1");
        // Inputs and registers form the source column.
        assert_eq!(clk.layer, 0);
        assert_eq!(count.layer, 0);
        // The logic flows rightward; the adder's D edge back into count is
        // the feedback path.
        assert!(add.layer >= 1);
        // A constant hugs its consumer's column.
        assert_eq!(one.layer, add.layer - 1);
        // The output owns the rightmost column.
        assert_eq!(led.layer, nl.columns() - 1);
    }

    #[test]
    fn const_radix_parses() {
        assert_eq!(parse_const("5'h16"), Some(22));
        assert_eq!(parse_const("4'h0"), Some(0));
        assert_eq!(parse_const("32'sh1"), Some(1));
        assert_eq!(parse_const("8'b1010"), Some(10));
        assert_eq!(parse_const("7"), Some(7));
    }

    #[test]
    fn empty_or_missing_module_is_none() {
        assert!(parse_netlist_json("{}", "blink").is_none());
        assert!(parse_netlist_json("not json", "blink").is_none());
    }
}
