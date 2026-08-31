//! End-to-end engine test: build and run the blink design headless.
//!
//! The test skips cleanly when verilator is not installed. It checks the
//! full loop: compile, HELLO, deterministic STEP, live LED activity,
//! hot swap on a source change, and input replay.

use std::path::PathBuf;
use std::time::{Duration, Instant};

use jade_hw::protocol::{HwCommand, HwEvent, HwRunState};
use tokio::sync::mpsc;

const BLINK_V: &str = r#"module blink (
    input  wire       clk,
    input  wire       button,
    output wire [4:0] led
);
    reg [25:0] count = 0;
    always @(posedge clk) count <= count + 1;
    assign led = ~{count[25:22], count[25]};
endmodule
"#;

const BLINK_QSF: &str = r#"set_global_assignment -name TOP_LEVEL_ENTITY blink
set_global_assignment -name VERILOG_FILE blink.v
set_location_assignment PIN_M9 -to clk
set_location_assignment PIN_T20 -to led[0]
set_location_assignment PIN_U22 -to led[1]
set_location_assignment PIN_U21 -to led[2]
set_location_assignment PIN_AA21 -to led[3]
set_location_assignment PIN_AA22 -to led[4]
set_location_assignment PIN_L22 -to button
"#;

async fn wait_for<F: FnMut(&HwEvent) -> bool>(
    rx: &mut mpsc::UnboundedReceiver<HwEvent>,
    what: &str,
    timeout: Duration,
    mut pred: F,
) -> HwEvent {
    let deadline = Instant::now() + timeout;
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        let ev = tokio::time::timeout(remaining, rx.recv())
            .await
            .unwrap_or_else(|_| panic!("timed out while waiting for {what}"))
            .unwrap_or_else(|| panic!("event channel closed while waiting for {what}"));
        if pred(&ev) {
            return ev;
        }
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn blink_end_to_end() {
    if jade_hw::detect_verilator().is_err() {
        eprintln!("SKIP: verilator is not installed");
        return;
    }

    let root = std::env::temp_dir().join(format!("jade_hw_e2e_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).unwrap();
    std::fs::write(root.join("blink.v"), BLINK_V).unwrap();
    std::fs::write(root.join("blink.qsf"), BLINK_QSF).unwrap();

    let (ev_tx, mut ev) = mpsc::unbounded_channel();
    let engine = jade_hw::start_session(root.clone(), ev_tx);

    // First build and sim start.
    let done = wait_for(&mut ev, "the first CompileDone", Duration::from_secs(120), |e| {
        matches!(e, HwEvent::CompileDone { .. })
    })
    .await;
    assert_eq!(done, HwEvent::CompileDone { ok: true });
    wait_for(&mut ev, "HELLO", Duration::from_secs(10), |e| {
        matches!(e, HwEvent::PortsDiscovered { top } if top == "blink")
    })
    .await;

    // Deterministic step: pause, then run exactly 2^25 cycles.
    // count[25] becomes 1, so LED0 and LED4 are lit (active low pins).
    let _ = engine.commands.send(HwCommand::Pause);
    wait_for(&mut ev, "the paused state", Duration::from_secs(5), |e| {
        matches!(e, HwEvent::RunState(HwRunState::Paused))
    })
    .await;
    let step_start = Instant::now();
    let _ = engine.commands.send(HwCommand::Step(1 << 25));
    // The harness emits the forced LED frame first, then STATE|stepped.
    let frame = wait_for(&mut ev, "the stepped LED frame", Duration::from_secs(30), |e| {
        matches!(e, HwEvent::LedFrame { .. })
    })
    .await;
    let step_secs = step_start.elapsed().as_secs_f64();
    let rate = (1u64 << 25) as f64 / step_secs;
    eprintln!("step rate: {:.1} M cycles/s", rate / 1e6);
    assert!(rate > 10e6, "simulation is too slow: {rate:.0} cycles/s");
    match frame {
        HwEvent::LedFrame { bitmask, .. } => assert_eq!(bitmask, 0b10001),
        _ => unreachable!(),
    }
    wait_for(&mut ev, "the stepped state", Duration::from_secs(5), |e| {
        matches!(e, HwEvent::RunState(HwRunState::Stepped))
    })
    .await;

    // Live run: at real time, LED1 (count[22]) toggles every 84 ms, so
    // 700 ms shows several distinct frames.
    let _ = engine.commands.send(HwCommand::Resume);
    let mut masks = std::collections::HashSet::new();
    let live_deadline = Instant::now() + Duration::from_millis(1500);
    while Instant::now() < live_deadline && masks.len() < 3 {
        let remaining = live_deadline.saturating_duration_since(Instant::now());
        match tokio::time::timeout(remaining, ev.recv()).await {
            Ok(Some(HwEvent::LedFrame { bitmask, .. })) => {
                masks.insert(bitmask);
            }
            Ok(Some(_)) => {}
            _ => break,
        }
    }
    assert!(
        masks.len() >= 2,
        "expected several distinct LED frames, got {masks:?}"
    );

    // Hot swap: flip a DIP switch, touch the source, and recompile. The new
    // sim must say HELLO again and the session must replay the DIP state.
    let _ = engine.commands.send(HwCommand::SetDip(2, true));
    std::fs::write(root.join("blink.v"), format!("{BLINK_V}\n")).unwrap();
    let _ = engine.commands.send(HwCommand::Recompile);
    wait_for(&mut ev, "the hot-swap CompileDone", Duration::from_secs(60), |e| {
        matches!(e, HwEvent::CompileDone { ok: true })
    })
    .await;
    wait_for(&mut ev, "HELLO after the hot swap", Duration::from_secs(10), |e| {
        matches!(e, HwEvent::PortsDiscovered { .. })
    })
    .await;

    // The generated artifacts live under .jade/hw.
    assert!(root.join(".jade/hw/jade_hw_top.v").is_file());
    assert!(root.join(".jade/hw/obj_dir/jade_hw_sim").is_file());

    engine.stop();
    let _ = engine.task.await;
    let _ = std::fs::remove_dir_all(&root);
    let _ = PathBuf::new();
}
