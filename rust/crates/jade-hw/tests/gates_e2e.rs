//! End-to-end gate view test: synthesize a small multiplier with a real
//! Yosys and parse the dump into the schematic graph.
//!
//! The test skips cleanly when yosys is not installed. It checks that the
//! `*` operator becomes an actual gate circuit: AND/XOR gates, one
//! flip-flop per result bit, and no abstract multiply node.

use jade_hw::netlist::NodeKind;
use jade_hw::synth;

const MULT_V: &str = r#"module mult (
    input  wire       clk,
    input  wire [3:0] a,
    input  wire [3:0] b,
    output reg  [7:0] p
);
    always @(posedge clk) begin
        p <= a * b;
    end
endmodule
"#;

#[tokio::test(flavor = "multi_thread")]
async fn yosys_lowers_a_multiply_to_gates() {
    let Some(yosys) = synth::detect_yosys() else {
        eprintln!("SKIP: yosys is not installed");
        return;
    };

    let root = std::env::temp_dir().join(format!("jade_hw_gates_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).unwrap();
    let src = root.join("mult.v");
    std::fs::write(&src, MULT_V).unwrap();
    let out = root.join("gates.json");

    let text = synth::run_yosys(&yosys, "mult", &[src], &out, &root)
        .await
        .expect("yosys run");
    let nl = synth::parse_yosys_json(&text, "mult").expect("netlist");

    // Ports survive with their widths.
    let find = |label: &str| nl.nodes.iter().find(|n| n.label == label).unwrap();
    assert_eq!(find("a").kind, NodeKind::Input);
    assert_eq!(find("a").width, 4);
    assert_eq!(find("p").kind, NodeKind::Output);
    assert_eq!(find("p").width, 8);

    // The multiply is a circuit now: AND and XOR gates, no `*` block.
    assert!(!nl.nodes.iter().any(|n| n.label == "*"));
    let count = |label: &str| nl.nodes.iter().filter(|n| n.label == label).count();
    assert!(count("&") >= 16, "partial products: {} AND gates", count("&"));
    assert!(count("^") >= 8, "adder tree: {} XOR gates", count("^"));

    // One flip-flop per result bit, all clocked by clk, named after `p`.
    let ffs: Vec<_> = nl
        .nodes
        .iter()
        .filter(|n| n.kind == NodeKind::Reg)
        .collect();
    assert_eq!(ffs.len(), 8);
    assert!(ffs.iter().all(|n| n.clock.as_deref() == Some("clk")));
    assert!(ffs.iter().any(|n| n.label == "p[0]"));

    // The clock is text on the register, never a wire.
    let clk = find("clk");
    assert!(!nl.edges.iter().any(|e| e.from == clk.id));

    // Layers hold: sources on the left, the output on the right column.
    assert_eq!(clk.layer, 0);
    assert_eq!(find("p").layer, nl.columns() - 1);

    let _ = std::fs::remove_dir_all(&root);
}
