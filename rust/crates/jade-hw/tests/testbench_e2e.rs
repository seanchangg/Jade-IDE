//! End-to-end: a real Icarus Verilog run of a small testbench, then the
//! dump it writes loads into the wave model. Skips when iverilog is absent.

use std::path::PathBuf;

use jade_hw::testbench::{detect_iverilog, run_testbench};
use jade_hw::wave::{WaveFile, WaveValue};

fn workspace() -> PathBuf {
    let dir = std::env::temp_dir().join(format!("jade_tb_e2e_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("counter.v"),
        "module counter(input clk, input rst, output reg [3:0] q);\n\
         always @(posedge clk) if (rst) q <= 0; else q <= q + 1;\n\
         endmodule\n",
    )
    .unwrap();
    std::fs::write(
        dir.join("tb_counter.v"),
        "`timescale 1ns/1ps\n\
         module tb_counter;\n\
         reg clk = 0; reg rst = 1; wire [3:0] q;\n\
         counter dut(.clk(clk), .rst(rst), .q(q));\n\
         always #5 clk = ~clk;\n\
         initial begin\n\
           $dumpfile(\"tb_counter.fst\"); $dumpvars(0, tb_counter);\n\
           #12 rst = 0;\n\
           #100 $display(\"q=%d\", q); $finish;\n\
         end\n\
         endmodule\n",
    )
    .unwrap();
    dir
}

#[tokio::test]
async fn iverilog_run_writes_a_dump_the_wave_model_loads() {
    if detect_iverilog().is_err() {
        eprintln!("iverilog not installed; skipping");
        return;
    }
    let root = workspace();
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let outcome = run_testbench(root.clone(), root.join("tb_counter.v"), tx).await;
    let mut lines = Vec::new();
    while let Ok(l) = rx.try_recv() {
        lines.push(l);
    }
    assert!(outcome.ok, "run failed: {:?}\n{}", outcome.failure, lines.join("\n"));
    assert!(
        lines.iter().any(|l| l.contains("q=")),
        "the $display line streams to the output: {lines:?}"
    );
    let dump = outcome.dump.expect("dump path");
    assert_eq!(dump, root.join("sim").join("tb_counter.fst"));

    let wave = WaveFile::load(&dump).expect("fst loads");
    assert_eq!(wave.time_exp, -12, "1ns/1ps testbench dumps in picoseconds");
    let q = wave.find("tb_counter.q[3:0]").expect("q is in the dump");
    let sig = &wave.signals[q];
    assert_eq!(sig.width, 4);
    // Reset held for 12 ns, then the counter climbs one per 10 ns clock.
    assert_eq!(sig.value_at(10_000), Some(&WaveValue::Bits(0)));
    let late = sig.value_at(wave.end_time).and_then(|v| v.bit()).is_some();
    assert!(late, "q has a known value at the end");
    assert!(sig.changes.len() >= 8, "the counter changed many times: {}", sig.changes.len());
    // The testbench module is the default row set: clk, rst, q.
    let rows = wave.default_rows();
    let names: Vec<String> = rows.iter().map(|&i| wave.signals[i].name.clone()).collect();
    assert!(names.contains(&"clk".to_string()) && names.contains(&"q[3:0]".to_string()), "{names:?}");
    let _ = std::fs::remove_dir_all(&root);
}
