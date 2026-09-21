//! Headless interaction tests: real clicks + keystrokes through GPUI's full
//! window dispatch (layout, hitboxes, focus, key routing) via test-support —
//! the same harness Zed's editor tests use. These exist because the pure-logic
//! smokes (`--smoke edit`) can pass while the event plumbing in a live window
//! is broken; this is the layer that proves a click actually reaches
//! `editor_mouse_down` and an arrow key actually reaches `editor_key`.

use std::path::PathBuf;
use std::sync::Arc;

use gpui::{point, px, Modifiers, TestAppContext};

use crate::app::{AppDeps, JadeApp};
use jade_ai::InlineCompletionBackend;
use jade_build::{BuildEngine, EngineConfig};
use jade_sysmon::SystemMonitor;
use jade_telemetry::TelemetryServer;
use jade_term::TermManager;

/// A throwaway workspace with one known C++ file. Rows (0-based):
/// 0: `int main() {`
/// 1: `    int alpha = 1;`
/// 2: `    int beta = 2;`
/// 3: `    return alpha + beta;`
/// 4: `}`
fn test_workspace() -> (PathBuf, PathBuf) {
    let dir = std::env::temp_dir().join(format!(
        "jade-itest-{}-{}",
        std::process::id(),
        std::thread::current().name().unwrap_or("t").replace("::", "-")
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let file = dir.join("main.cpp");
    std::fs::write(
        &file,
        "int main() {\n    int alpha = 1;\n    int beta = 2;\n    return alpha + beta;\n}\n",
    )
    .unwrap();
    (dir, file)
}

/// Assemble real AppDeps against the throwaway workspace. The tokio runtime is
/// leaked so engine handles stay valid for the life of the test process.
fn test_deps(workspace: PathBuf) -> (AppDeps, tokio::sync::mpsc::UnboundedReceiver<crate::app::AppEvent>) {
    // A current-thread runtime that is never driven: tasks (telemetry accept
    // loop, engine futures) queue but never poll, so no foreign thread runs
    // during the test — gpui's determinism checker requires that. The editor
    // interaction under test is all synchronous on the gpui thread.
    let runtime = Box::leak(Box::new(
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap(),
    ));
    // Unix sockets cap at ~104 bytes of path; keep it short and unique.
    static SOCK_N: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
    let socket = PathBuf::from(format!(
        "/tmp/jade-it-{}-{}.sock",
        std::process::id(),
        SOCK_N.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    ));
    let _ = std::fs::remove_file(&socket);
    let _guard = runtime.enter(); // TelemetryServer::start needs a reactor
    let (server, _tel_rx) = TelemetryServer::start(socket).expect("bind telemetry socket");
    let server = Arc::new(server);
    let engine = Arc::new(BuildEngine::new(
        EngineConfig::from_repo_root(&workspace).with_telemetry(server.clone()),
    ));
    let (app_tx, app_rx) = tokio::sync::mpsc::unbounded_channel();
    let deps = AppDeps {
        server,
        engine,
        ai: Arc::new(InlineCompletionBackend::new()),
        // No credential is resolved until a request is made, so a headless app
        // can hold a chat backend without touching the keychain or the network.
        chat: Arc::new(jade_ai::ChatBackend::new()),
        sysmon: Arc::new(SystemMonitor::new()), // constructed, not started
        term: Arc::new(TermManager::new()),
        runtime: runtime.handle().clone(),
        app_tx,
        active_file: None,
        repo_root: workspace.clone(),
        // Prefs save into the throwaway workspace, NEVER the developer's real
        // ~/.config/jade/telemetry.json (checkbox/bundle tests call save()).
        prefs_path: Some(workspace.join(".jade").join("telemetry-test.json")),
        workspace_root: workspace,
        // The test workspace is "open" (it has a real tree); the fs-watch is a
        // no-op so no notify thread runs under the determinism checker.
        workspace_opened: true,
        fs_watch: Arc::new(|_root| None),
        demo: false,
        mode: crate::app::AppMode::Software,
        hw_tx: None,
        hw_watch: Arc::new(|_root| None),
    };
    (deps, app_rx)
}

/// Same as [`test_deps`], but hardware mode with a test command channel: the
/// app sends [`jade_hw::HwCommand`]s there instead of spawning an engine.
fn test_deps_hw(
    workspace: PathBuf,
) -> (
    AppDeps,
    tokio::sync::mpsc::UnboundedReceiver<crate::app::AppEvent>,
    tokio::sync::mpsc::UnboundedReceiver<jade_hw::HwCommand>,
) {
    let (mut deps, app_rx) = test_deps(workspace);
    let (hw_tx, hw_rx) = tokio::sync::mpsc::unbounded_channel();
    deps.mode = crate::app::AppMode::Hardware;
    deps.hw_tx = Some(hw_tx);
    (deps, app_rx, hw_rx)
}

/// A throwaway hardware workspace: one Verilog blink plus its `.qsf`.
fn test_hw_workspace() -> (PathBuf, PathBuf) {
    let dir = std::env::temp_dir().join(format!(
        "jade-hwtest-{}-{}",
        std::process::id(),
        std::thread::current().name().unwrap_or("t").replace("::", "-")
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let file = dir.join("blink.v");
    std::fs::write(
        &file,
        "module blink (\n    input  wire clk,\n    output wire [4:0] led\n);\n    reg [25:0] count = 0;\n    always @(posedge clk) count <= count + 1;\n    assign led = ~count[25:21];\nendmodule\n",
    )
    .unwrap();
    std::fs::write(
        dir.join("blink.qsf"),
        "set_global_assignment -name TOP_LEVEL_ENTITY blink\nset_global_assignment -name VERILOG_FILE blink.v\nset_location_assignment PIN_M9 -to clk\nset_location_assignment PIN_T20 -to led[0]\n",
    )
    .unwrap();
    (dir, file)
}

#[gpui::test]
async fn click_focuses_sets_caret_and_arrows_navigate(cx: &mut TestAppContext) {
    let (dir, file) = test_workspace();
    let (deps, app_rx) = test_deps(dir);

    let (app, cx) = cx.add_window_view(|_window, cx| JadeApp::new(cx, deps, app_rx));
    app.update_in(cx, |app, _window, cx| {
        app.open_file(file.clone());
        cx.notify();
    });
    cx.run_until_parked();

    // Opening a file must focus the editor and paint a caret with NO click —
    // this was the original "I can't find my cursor / arrows are dead" bug:
    // nothing granted the editor focus until the user happened to click code.
    app.update_in(cx, |app, window, _cx| {
        let focused = app
            .editor_focus
            .as_ref()
            .map(|f| f.is_focused(window))
            .unwrap_or(false);
        assert!(focused, "opening a file must focus the editor");
    });
    assert!(
        cx.debug_bounds("editor-caret").is_some(),
        "caret bar must be painted right after open"
    );

    // Row 1 is "    int alpha = 1;". Click near column 6 (inside "int").
    let cell = cx
        .debug_bounds("code-cell-1")
        .expect("code cell for row 1 was painted");
    let x = cell.origin.x + px(6.0 * crate::panels::code_view::CHAR_W + 1.0);
    let y = cell.origin.y + px(crate::panels::code_view::LINE_H / 2.0);
    cx.simulate_click(point(x, y), Modifiers::default());

    // The click must focus the editor and place the caret at row 1.
    app.update_in(cx, |app, window, _cx| {
        let focused = app
            .editor_focus
            .as_ref()
            .map(|f| f.is_focused(window))
            .unwrap_or(false);
        assert!(focused, "editor focus handle not focused after click");
        let caret = app.editor.active_tab().unwrap().caret_point();
        assert_eq!(caret.row, 1, "caret row after click");
        assert_eq!(caret.col, 6, "caret col after click");
    });

    // The caret bar must actually be painted now.
    cx.run_until_parked();
    assert!(
        cx.debug_bounds("editor-caret").is_some(),
        "caret bar not painted after click+focus"
    );

    // Arrows: down then left must move the caret through the buffer.
    cx.simulate_keystrokes("down");
    app.update_in(cx, |app, _w, _cx| {
        let caret = app.editor.active_tab().unwrap().caret_point();
        assert_eq!((caret.row, caret.col), (2, 6), "caret after down");
    });
    cx.simulate_keystrokes("left left");
    app.update_in(cx, |app, _w, _cx| {
        let caret = app.editor.active_tab().unwrap().caret_point();
        assert_eq!((caret.row, caret.col), (2, 4), "caret after left left");
    });
}

/// The action-bar diagnostic pills (§ error/warning/note counts): clicking one
/// opens the list for that severity, and clicking a row jumps the editor to the
/// line the diagnostic flags. This is the whole point of the pills — before
/// this, a click did nothing at all.
#[gpui::test]
async fn diagnostic_pill_opens_a_list_and_a_row_jumps_to_the_line(cx: &mut TestAppContext) {
    use crate::app::DiagKind;
    use jade_lsp::{Diagnostic, DiagnosticSeverity, Position, Range};

    let (dir, file) = test_workspace();
    let (deps, app_rx) = test_deps(dir);
    let (app, cx) = cx.add_window_view(|_window, cx| JadeApp::new(cx, deps, app_rx));
    app.update_in(cx, |app, _window, cx| {
        app.open_file(file.clone());
        cx.notify();
    });
    cx.run_until_parked();

    // Stand in for clangd: publish one warning and two errors, out of order and
    // with the errors on rows 3 and 1 of the fixture.
    app.update_in(cx, |app, _window, cx| {
        let mk = |sev, line, ch| Diagnostic {
            severity: Some(sev),
            range: Range::new(Position::new(line, ch), Position::new(line, ch + 3)),
            message: format!("boom at {line}:{ch}"),
            ..Default::default()
        };
        let tab = app.editor.active_tab_mut().expect("a tab is open");
        tab.diagnostics = vec![
            mk(DiagnosticSeverity::ERROR, 3, 11),
            mk(DiagnosticSeverity::WARNING, 2, 4),
            mk(DiagnosticSeverity::ERROR, 1, 8),
        ];
        assert_eq!(app.active_diag_counts(), (2, 1, 0), "pill counts");
        cx.notify();
    });
    cx.run_until_parked();

    // Click the error pill.
    let pill = cx
        .debug_bounds("pill-err")
        .expect("the error pill was painted");
    cx.simulate_click(pill.center(), Modifiers::default());
    cx.run_until_parked();
    app.update_in(cx, |app, _window, _cx| {
        assert_eq!(app.diag_popup, Some(DiagKind::Error), "pill opens its list");
        // Errors only, read top-down: row 1 before row 3.
        let rows = app.diag_list(DiagKind::Error);
        assert_eq!(rows, vec![2, 0], "errors listed in source order");
    });

    // The first row is the row-1 error; clicking it must move the caret there.
    let row = cx
        .debug_bounds("diag-row-0")
        .expect("the first diagnostic row was painted");
    cx.simulate_click(row.center(), Modifiers::default());
    cx.run_until_parked();
    app.update_in(cx, |app, _window, _cx| {
        let caret = app.editor.active_tab().unwrap().caret_point();
        assert_eq!((caret.row, caret.col), (1, 8), "caret jumped to the error");
        assert!(app.diag_popup.is_none(), "jumping closes the list");
    });

    // A pill with nothing to show still opens — the popup is what says so.
    let info = cx
        .debug_bounds("pill-info")
        .expect("the note pill was painted");
    cx.simulate_click(info.center(), Modifiers::default());
    cx.run_until_parked();
    app.update_in(cx, |app, _window, _cx| {
        assert_eq!(app.diag_popup, Some(DiagKind::Info));
        assert!(app.diag_list(DiagKind::Info).is_empty());
    });
    // Clicking the same pill again closes it.
    cx.simulate_click(info.center(), Modifiers::default());
    cx.run_until_parked();
    app.update_in(cx, |app, _window, _cx| {
        assert!(app.diag_popup.is_none(), "the same pill toggles shut");
    });
}

/// Run-store lifecycle, headless: a telemetry-producing run persists to
/// `.jade/runs.db` at exit, overlays load/unload its series, empty runs are
/// skipped, and delete removes both the row and any live overlay. Drives the
/// same `pending_run` → `persist_finished_run` seam `action_run`/`on_run_done`
/// use, minus the subprocess.
#[test]
fn run_lifecycle_persists_and_overlays() {
    use crate::run_store::{PendingRun, KIND_RUN};

    let (dir, _file) = test_workspace();
    let (deps, _rx) = test_deps(dir.clone());
    let mut app = JadeApp::assemble(deps);
    assert!(app.run_store.is_some(), "store opens in a fresh workspace");
    assert!(app.stored_runs.is_empty());

    // A run with telemetry → stored.
    app.pending_run = Some(PendingRun::begin("a.out".to_string(), KIND_RUN, &dir));
    app.training.push_scalar("loss", 0, 1.0);
    app.training.push_scalar("loss", 1, 0.4);
    app.persist_finished_run(Some(1500), Some(0));
    assert_eq!(app.stored_runs.len(), 1);
    let meta = &app.stored_runs[0];
    assert_eq!((meta.label.as_str(), meta.duration_ms), ("a.out", Some(1500)));
    let id = meta.id;

    // Overlay round-trip loads the stored series.
    app.toggle_run_overlay(id);
    assert_eq!(app.run_overlays.len(), 1);
    assert_eq!(
        app.run_overlays[0].1.scalars.get("loss").map(|v| v.len()),
        Some(2)
    );
    app.toggle_run_overlay(id);
    assert!(app.run_overlays.is_empty());

    // A telemetry-less run is not stored (training was ghost-cleared at start).
    app.training.clear();
    app.pending_run = Some(PendingRun::begin("a.out".to_string(), KIND_RUN, &dir));
    app.persist_finished_run(Some(5), Some(0));
    assert_eq!(app.stored_runs.len(), 1, "empty run skipped");

    // Delete drops the row and the live overlay.
    app.toggle_run_overlay(id);
    app.delete_stored_run(id);
    assert!(app.stored_runs.is_empty());
    assert!(app.run_overlays.is_empty());

    // The DB actually lives in the workspace's .jade dir.
    assert!(dir.join(".jade").join("runs.db").exists());
    let _ = std::fs::remove_dir_all(&dir);
}

/// Timer bundles, headless: staged timers become one synthetic summed series
/// per loop cycle; members stop charting individually (but stay tracked);
/// dissolve restores raw behavior; defs persist to workspace.json.
#[test]
fn timer_bundle_aggregates_and_persists() {
    use crate::app::AppEvent;
    use jade_telemetry::{Event, Timing};

    let (dir, _file) = test_workspace();
    let (deps, _rx) = test_deps(dir.clone());
    let mut app = JadeApp::assemble(deps);

    let timing = |name: &str, ms: f64, step: i64| {
        AppEvent::Telemetry(Event::Timing(Timing {
            name: name.to_string(),
            ms,
            step,
        }))
    };

    // Two forward kernels stream; bundle them as "Forward".
    app.apply_app_event(timing("embedForward", 1.0, 0));
    app.apply_app_event(timing("linearForward", 2.0, 1));
    app.create_timer_group("Forward", vec!["embedForward".into(), "linearForward".into()]);
    assert!(app.registry.is_enabled(jade_telemetry::Kind::Timer, "Forward"));

    // A full cycle → one summed "Forward" point; members don't chart raw.
    let before = app.training.current.timings.len();
    app.apply_app_event(timing("embedForward", 1.5, 2));
    app.apply_app_event(timing("linearForward", 2.5, 3));
    let new: Vec<_> = app.training.current.timings[before..].iter().collect();
    assert_eq!(new.len(), 1, "members feed the bundle, not the raw chart");
    assert_eq!(new[0].name, "Forward");
    assert_eq!(new[0].ms, 4.0);

    // Cycle wrap (member repeats) flushes a partial window.
    app.apply_app_event(timing("embedForward", 1.0, 4)); // starts next window
    app.apply_app_event(timing("embedForward", 1.2, 5)); // wrap → flush 1.0
    let last = app.training.current.timings.last().unwrap();
    assert_eq!((last.name.as_str(), last.ms), ("Forward", 1.0));

    // Defs persisted per-workspace.
    let ui = crate::workspace_state::load(&dir);
    assert_eq!(ui.timer_groups.len(), 1);
    assert_eq!(ui.timer_groups[0].name, "Forward");

    // Dissolve: members chart raw again, def gone from disk.
    app.dissolve_timer_group("Forward");
    let before = app.training.current.timings.len();
    app.apply_app_event(timing("embedForward", 3.0, 6));
    let last = app.training.current.timings.last().unwrap();
    assert_eq!(app.training.current.timings.len(), before + 1);
    assert_eq!(last.name, "embedForward");
    assert!(crate::workspace_state::load(&dir).timer_groups.is_empty());

    // No staging → printable keys edit the buffer filter; Esc clears it
    // (and only an already-empty filter lets Esc fall through to close).
    assert!(app.pre_run_key("d", Some("d"), true));
    assert!(app.pre_run_key("_", Some("_"), true));
    assert_eq!(app.buffer_search, "d_");
    assert!(app.pre_run_key("backspace", None, false));
    assert_eq!(app.buffer_search, "d");
    assert!(app.pre_run_key("escape", None, false), "esc clears the filter");
    assert!(app.buffer_search.is_empty());
    assert!(!app.pre_run_key("escape", None, false), "second esc falls through");

    // Staging keystrokes: type a name, enter creates from staged members.
    app.toggle_group_staging("embedForward");
    app.toggle_group_staging("linearForward");
    assert!(app.pre_run_key("f", Some("F"), true));
    assert!(app.pre_run_key("enter", None, false), "needs name — consumed");
    assert!(app.timer_groups.get("F").is_some(), "bundle created via keys");
    assert!(app.group_staging.is_empty());

    let _ = std::fs::remove_dir_all(&dir);
}

/// Persisted selections seed the registry at startup: the sidebar / pre-run
/// panel show them immediately (no run needed), a seeded list does NOT
/// auto-launch a discovery scan (that ran the user's program while they were
/// still selecting), and the probe's first real decl confirms the item so
/// tracking re-arms.
#[test]
fn prefs_seed_registry_at_startup() {
    use crate::app::AppEvent;
    use jade_telemetry::{Event, Kind};

    let (dir, _file) = test_workspace();
    let decl = |name: &str| {
        AppEvent::Telemetry(Event::Decl {
            kind: Kind::Timer,
            name: name.to_string(),
            meta: None,
            renamed_from: None,
        })
    };

    // Session A: probe declares a timer; the user checks it (pref saved).
    {
        let (deps, _rx) = test_deps(dir.clone());
        let mut app = JadeApp::assemble(deps);
        app.apply_app_event(decl("attnForward"));
        app.toggle_enabled(Kind::Timer, "attnForward");
        assert!(app.registry.is_enabled(Kind::Timer, "attnForward"));
    }

    // Session B: the selection is visible before any run or discovery…
    let (deps, _rx) = test_deps(dir.clone());
    let mut app = JadeApp::assemble(deps);
    let item = app.registry.get(Kind::Timer, "attnForward").expect("seeded from pref");
    assert!(item.enabled, "seeded item shows checked");
    assert!(item.seeded);

    // …and Run shows that seeded list as-is instead of auto-launching the
    // program to rescan it — the user is mid-selection; renames are covered
    // by the panel's explicit Rescan button (same `start_discovery` path).
    app.last_build = Some(jade_build::BuildResult {
        success: true,
        executable: Some(std::path::PathBuf::from("/bin/sleep")),
        errors: Vec::new(),
        duration: std::time::Duration::from_millis(1),
        project_root: Some(dir.clone()),
    });
    app.open_pre_run(crate::app::PreRunLaunch::Run);
    assert!(!app.discovery_active, "seeded list must not auto-launch a scan");
    app.start_discovery();
    assert!(app.discovery_active, "explicit Rescan still scans");

    // The probe's real decl confirms the seeded item (and re-arms `track`
    // through the seeded→confirmed `pref_enabled` transition).
    app.apply_app_event(decl("attnForward"));
    let item = app.registry.get(Kind::Timer, "attnForward").unwrap();
    assert!(!item.seeded, "real decl confirms the seeded item");
    assert!(item.enabled);

    let _ = std::fs::remove_dir_all(&dir);
}

/// Stale seeded rows — names the probe no longer declares — are pruned (and
/// their prefs cleared) once a scan/run reports real inventory, so the
/// panels don't fill with dead duplicates after every rename or refactor.
/// Bundle synthetic timers are exempt: they confirm via aggregation.
#[test]
fn stale_seeded_rows_prune_after_run() {
    use crate::app::AppEvent;
    use jade_telemetry::{Event, Kind};
    use std::collections::HashMap;

    let (dir, _file) = test_workspace();
    let decl = |kind: Kind, name: &str| {
        AppEvent::Telemetry(Event::Decl {
            kind,
            name: name.to_string(),
            meta: None,
            renamed_from: None,
        })
    };
    let run_done = || {
        AppEvent::RunDone(jade_build::RunResult {
            exit_code: 0,
            duration: std::time::Duration::from_millis(5),
            executed_lines: HashMap::new(),
            sanitizer_output: None,
            interpose_active: false,
            instrumentation_summary: None,
        })
    };

    // Session A: real decls, checked by the user; plus a bundle.
    {
        let (deps, _rx) = test_deps(dir.clone());
        let mut app = JadeApp::assemble(deps);
        app.apply_app_event(decl(Kind::Timer, "oldKernel"));
        app.apply_app_event(decl(Kind::Buffer, "oldBuffer"));
        app.toggle_enabled(Kind::Timer, "oldKernel");
        app.toggle_enabled(Kind::Buffer, "oldBuffer");
        app.create_timer_group("Fwd", vec!["a".into(), "b".into()]);
    }

    // Session B: everything seeds back, then the probe reports NEW names only.
    let (deps, _rx) = test_deps(dir.clone());
    let mut app = JadeApp::assemble(deps);
    assert!(app.registry.get(Kind::Timer, "oldKernel").is_some());
    assert!(app.registry.get(Kind::Buffer, "oldBuffer").is_some());
    app.apply_app_event(decl(Kind::Timer, "newKernel"));
    app.apply_app_event(run_done());

    assert!(
        app.registry.get(Kind::Timer, "oldKernel").is_none(),
        "stale seeded timer pruned after the run"
    );
    assert!(
        app.registry.get(Kind::Buffer, "oldBuffer").is_none(),
        "stale seeded buffer pruned after the run"
    );
    assert!(
        app.registry.get(Kind::Timer, "Fwd").is_some(),
        "bundle synthetic timer survives pruning"
    );
    assert!(app.registry.get(Kind::Timer, "newKernel").is_some());

    // Prefs cleared → session C doesn't re-seed the stale names.
    let (deps, _rx) = test_deps(dir.clone());
    let app = JadeApp::assemble(deps);
    assert!(app.registry.get(Kind::Timer, "oldKernel").is_none());
    assert!(app.registry.get(Kind::Buffer, "oldBuffer").is_none());

    let _ = std::fs::remove_dir_all(&dir);
}

/// A stale '+'-joined timer selection (an old command-buffer timer name the
/// per-encoder probe never declares again) migrates to a real bundle over its
/// member kernels once every member has been declared — the summed series
/// returns under the familiar name with zero re-checking.
#[test]
fn stale_joined_timer_pref_migrates_to_bundle() {
    use crate::app::AppEvent;
    use jade_telemetry::{Event, Kind, Timing};

    let (dir, _file) = test_workspace();
    let decl = |name: &str| {
        AppEvent::Telemetry(Event::Decl {
            kind: Kind::Timer,
            name: name.to_string(),
            meta: None,
            renamed_from: None,
        })
    };

    // Session A (old probe): the joined command-buffer timer is checked.
    {
        let (deps, _rx) = test_deps(dir.clone());
        let mut app = JadeApp::assemble(deps);
        app.apply_app_event(decl("qkv+flashFwd"));
        app.toggle_enabled(Kind::Timer, "qkv+flashFwd");
    }

    // Session B (per-encoder probe): only the member kernels are declared.
    let (deps, _rx) = test_deps(dir.clone());
    let mut app = JadeApp::assemble(deps);
    assert!(app.registry.get(Kind::Timer, "qkv+flashFwd").unwrap().seeded);

    app.apply_app_event(decl("qkv"));
    assert!(
        app.timer_groups.get("qkv+flashFwd").is_none(),
        "no migration until every member is live"
    );
    app.apply_app_event(decl("flashFwd"));
    let def = app.timer_groups.get("qkv+flashFwd").expect("migrated to a bundle");
    assert_eq!(def.members, vec!["qkv".to_string(), "flashFwd".to_string()]);
    assert!(app.registry.is_enabled(Kind::Timer, "qkv+flashFwd"));

    // The bundle aggregates member samples under the familiar name.
    let timing = |name: &str, ms: f64, step: i64| {
        AppEvent::Telemetry(Event::Timing(Timing {
            name: name.to_string(),
            ms,
            step,
        }))
    };
    app.apply_app_event(timing("qkv", 2.0, 0));
    app.apply_app_event(timing("flashFwd", 3.0, 1));
    let last = app.training.current.timings.last().expect("bundle point");
    assert_eq!((last.name.as_str(), last.ms), ("qkv+flashFwd", 5.0));

    let _ = std::fs::remove_dir_all(&dir);
}

/// A persisted bundle must survive a restart: the defs reload, the synthetic
/// timer re-enters the registry with its saved enabled-pref (so the decl
/// handshake re-arms member tracking), and member samples aggregate again —
/// WITHOUT re-creating the bundle. Regression: bundles only worked in the
/// session that created them; every later launch loaded the defs but never
/// declared the synthetic timer, so `timer_in_enabled_group` saw nothing.
#[test]
fn timer_bundle_survives_restart() {
    use crate::app::AppEvent;
    use jade_telemetry::{Event, Timing};

    let (dir, _file) = test_workspace();
    let timing = |name: &str, ms: f64, step: i64| {
        AppEvent::Telemetry(Event::Timing(Timing {
            name: name.to_string(),
            ms,
            step,
        }))
    };

    // Session A: bundle two kernels, then "quit" (drop the app).
    {
        let (deps, _rx) = test_deps(dir.clone());
        let mut app = JadeApp::assemble(deps);
        app.apply_app_event(timing("embedForward", 1.0, 0));
        app.apply_app_event(timing("linearForward", 2.0, 1));
        app.create_timer_group("Forward", vec!["embedForward".into(), "linearForward".into()]);
    }

    // Session B: fresh assemble from the same workspace.
    let (deps, _rx) = test_deps(dir.clone());
    let mut app = JadeApp::assemble(deps);
    assert!(
        app.timer_groups.get("Forward").is_some(),
        "bundle def reloads from workspace.json"
    );
    assert!(
        app.registry.is_enabled(jade_telemetry::Kind::Timer, "Forward"),
        "synthetic timer re-declared with its enabled pref"
    );

    // Member samples flow again → the bundle's summed series resumes.
    app.apply_app_event(timing("embedForward", 1.5, 0));
    app.apply_app_event(timing("linearForward", 2.5, 1));
    let last = app.training.current.timings.last().expect("bundle point");
    assert_eq!((last.name.as_str(), last.ms), ("Forward", 4.0));

    // A probe decl for a timer NAMED like a bundle def dissolves the def (it
    // shadows a real timer — the probe joins command-buffer kernel names with
    // '+', so such collisions happen) while the enabled pref survives, keeping
    // the raw series charting without a re-check.
    app.apply_app_event(AppEvent::Telemetry(Event::Decl {
        kind: jade_telemetry::Kind::Timer,
        name: "Forward".to_string(),
        meta: None,
        renamed_from: None,
    }));
    assert!(
        app.timer_groups.get("Forward").is_none(),
        "shadowing bundle def dissolved by the real probe timer"
    );
    assert!(
        app.registry.is_enabled(jade_telemetry::Kind::Timer, "Forward"),
        "enabled pref survives the dissolve"
    );
    assert!(
        crate::workspace_state::load(&dir).timer_groups.is_empty(),
        "dissolve persisted"
    );

    // The re-declared synthetic bundle timers are a usable seeded list: a
    // fresh session opening the pre-run panel shows them and does NOT
    // auto-launch a discovery run (the user's program must not start while
    // they're still selecting; the header's Rescan refreshes names).
    let (deps, _rx2) = test_deps(dir.clone());
    let mut fresh = JadeApp::assemble(deps);
    fresh.last_build = Some(jade_build::BuildResult {
        success: true,
        executable: Some(std::path::PathBuf::from("/bin/sleep")),
        errors: Vec::new(),
        duration: std::time::Duration::from_millis(1),
        project_root: Some(dir.clone()),
    });
    fresh.open_pre_run(crate::app::PreRunLaunch::Run);
    assert!(
        !fresh.discovery_active,
        "seeded bundle list must not auto-launch a scan"
    );
    assert!(
        !fresh
            .registry
            .items_of_kind(jade_telemetry::Kind::Timer)
            .is_empty(),
        "panel has the bundle timers to select from"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

/// Per-pipeline kernel charts: with the runtime sidebar open, each ENABLED
/// timer renders its own mini-chart (independent scales), no bundling needed.
#[gpui::test]
async fn kernel_charts_render_per_enabled_timer(cx: &mut TestAppContext) {
    use crate::app::AppEvent;
    use jade_telemetry::{Event, Kind, Timing};

    let (dir, _file) = test_workspace();
    let (deps, app_rx) = test_deps(dir);

    let (app, cx) = cx.add_window_view(|_window, cx| JadeApp::new(cx, deps, app_rx));
    let timing = |name: &str, ms: f64, step: i64| {
        AppEvent::Telemetry(Event::Timing(Timing {
            name: name.to_string(),
            ms,
            step,
        }))
    };

    app.update_in(cx, |app, _w, cx| {
        app.runtime_visible = true;
        // Two pipelines stream: a slow one and a 100× faster one.
        for i in 0..4 {
            app.apply_app_event(timing("attnForward", 5.0, i * 2));
            app.apply_app_event(timing("embedForward", 0.05, i * 2 + 1));
        }
        // The user checks both timers (pre-run panel / sidebar checkbox path).
        app.registry.set_enabled(Kind::Timer, "attnForward", true);
        app.registry.set_enabled(Kind::Timer, "embedForward", true);
        cx.notify();
    });
    cx.run_until_parked();

    let attn = cx.debug_bounds("kernel-chart-attnForward");
    let embed = cx.debug_bounds("kernel-chart-embedForward");
    assert!(attn.is_some(), "attnForward gets its own chart");
    assert!(embed.is_some(), "embedForward gets its own chart");
    assert_ne!(attn, embed, "charts are separate elements");

    // Unchecking a timer removes ONLY its chart.
    app.update_in(cx, |app, _w, cx| {
        app.registry.set_enabled(Kind::Timer, "embedForward", false);
        cx.notify();
    });
    cx.run_until_parked();
    assert!(cx.debug_bounds("kernel-chart-attnForward").is_some());
    assert!(
        cx.debug_bounds("kernel-chart-embedForward").is_none(),
        "unchecked timer's chart is gone"
    );
}

/// Like [`test_deps`] but with a real multi-thread runtime, for headless tests
/// that need engine tasks (spawn/kill child processes) to actually execute.
/// Only usable in tests WITHOUT a gpui context — foreign threads would trip
/// gpui's determinism checker.
fn test_deps_driven(
    workspace: PathBuf,
) -> (AppDeps, tokio::sync::mpsc::UnboundedReceiver<crate::app::AppEvent>) {
    let runtime = Box::leak(Box::new(
        tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .unwrap(),
    ));
    static SOCK_N: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
    let socket = PathBuf::from(format!(
        "/tmp/jade-itd-{}-{}.sock",
        std::process::id(),
        SOCK_N.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    ));
    let _ = std::fs::remove_file(&socket);
    let _guard = runtime.enter();
    let (server, _tel_rx) = TelemetryServer::start(socket).expect("bind telemetry socket");
    let server = Arc::new(server);
    let engine = Arc::new(BuildEngine::new(
        EngineConfig::from_repo_root(&workspace).with_telemetry(server.clone()),
    ));
    let (app_tx, app_rx) = tokio::sync::mpsc::unbounded_channel();
    let deps = AppDeps {
        server,
        engine,
        ai: Arc::new(InlineCompletionBackend::new()),
        // No credential is resolved until a request is made, so a headless app
        // can hold a chat backend without touching the keychain or the network.
        chat: Arc::new(jade_ai::ChatBackend::new()),
        sysmon: Arc::new(SystemMonitor::new()),
        term: Arc::new(TermManager::new()),
        runtime: runtime.handle().clone(),
        app_tx,
        active_file: None,
        repo_root: workspace.clone(),
        prefs_path: Some(workspace.join(".jade").join("telemetry-test.json")),
        workspace_root: workspace,
        workspace_opened: true,
        fs_watch: Arc::new(|_root| None),
        demo: false,
        mode: crate::app::AppMode::Software,
        hw_tx: None,
        hw_watch: Arc::new(|_root| None),
    };
    (deps, app_rx)
}

/// Pre-run tracking panel, headless: "Run with tracking…" opens the panel
/// (not the program), an empty registry triggers a discovery run that
/// auto-finishes, and confirming launches the real run. The plain Run button
/// launches at once with no panel. Uses `/bin/sleep` (exits instantly with a
/// usage error) as the "program" so no build is needed.
#[test]
fn pre_run_panel_discovers_then_launches() {
    use crate::app::{AppEvent, PreRunLaunch};
    use std::time::{Duration, Instant};

    let (dir, _file) = test_workspace();
    let (deps, mut rx) = test_deps_driven(dir.clone());
    let mut app = JadeApp::assemble(deps);

    // Drive the app until `done` holds, applying events like the real pump.
    let mut drain = |app: &mut JadeApp,
                     rx: &mut tokio::sync::mpsc::UnboundedReceiver<AppEvent>,
                     done: &dyn Fn(&JadeApp) -> bool| {
        let deadline = Instant::now() + Duration::from_secs(10);
        while !done(app) && Instant::now() < deadline {
            match rx.try_recv() {
                Ok(ev) => app.apply_app_event(ev),
                Err(_) => std::thread::sleep(Duration::from_millis(20)),
            }
        }
        assert!(done(app), "drain timed out");
    };

    // Without a build, neither Run entry does anything.
    app.action_run();
    assert!(!app.running, "no build → no run");
    app.action_run_tracked();
    assert!(app.pre_run.is_none(), "no build → no panel");

    // Fake a successful build of a real (instant-exit) binary.
    app.last_build = Some(jade_build::BuildResult {
        success: true,
        executable: Some(std::path::PathBuf::from("/bin/sleep")),
        errors: Vec::new(),
        duration: Duration::from_millis(1),
        project_root: Some(dir.clone()),
    });

    // The plain Run button launches now, with no panel and no scan.
    app.action_run();
    assert!(app.pre_run.is_none(), "direct Run opens no panel");
    assert!(!app.discovery_active, "direct Run starts no scan");
    assert!(app.running, "direct Run launched");
    drain(&mut app, &mut rx, &|a| !a.running);

    // "Run with tracking…" opens the panel; the empty registry starts a
    // discovery run.
    app.action_run_tracked();
    assert!(app.pre_run.is_some(), "panel open");
    assert!(app.discovery_active, "discovery started (registry empty)");
    assert!(!app.running, "discovery is not a real run");

    // The child exits on its own; discovery cleanup runs on its RunDone.
    drain(&mut app, &mut rx, &|a| !a.discovery_active);
    assert_eq!(app.pre_run.map(|p| p.discovering), Some(false));
    assert!(app.stored_runs.is_empty(), "discovery never hits the run store");

    // Confirm → the real run launches and finishes.
    assert_eq!(app.pre_run.map(|p| p.launch), Some(PreRunLaunch::Run));
    app.confirm_pre_run();
    assert!(app.pre_run.is_none(), "panel closed on confirm");
    assert!(app.running, "real run launched");
    drain(&mut app, &mut rx, &|a| !a.running);
    assert!(app.stored_runs.is_empty(), "no telemetry → still nothing stored");

    // Cancel path: reopen and Esc out without launching.
    app.action_run_tracked();
    assert!(app.pre_run.is_some());
    app.cancel_pre_run();
    assert!(app.pre_run.is_none());
    assert!(!app.running);

    let _ = std::fs::remove_dir_all(&dir);
}

/// `open_project` state transition (§2), headless via `assemble` — no GPUI
/// context needed since `open_project` is `cx`-free. Opening folder B repoints
/// the tree, restores B's persisted active tab, and follows `active_file`.
#[test]
fn open_project_transitions_state() {
    let (dir_a, _file_a) = test_workspace();
    let (deps, _rx) = test_deps(dir_a.clone());
    let mut app = JadeApp::assemble(deps);

    // Folder B: two source files + a persisted ui blob restoring util.cpp as the
    // active tab plus a breakpoint, exactly like a real workspace.json.
    let dir_b = std::env::temp_dir().join(format!("jade-openproj-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir_b);
    std::fs::create_dir_all(&dir_b).unwrap();
    std::fs::write(dir_b.join("alpha.cpp"), "int main(){return 0;}\n").unwrap();
    std::fs::write(dir_b.join("util.cpp"), "int u(){return 1;}\n").unwrap();
    let util = dir_b.join("util.cpp").display().to_string();
    let ui = crate::workspace_state::WorkspaceUi {
        open_tabs: vec![crate::workspace_state::TabState {
            path: util.clone(),
            is_dirty: false,
        }],
        active_tab_index: Some(0),
        breakpoints: std::collections::HashMap::from([(util.clone(), vec![2u32])]),
        ..Default::default()
    };
    crate::workspace_state::save(&dir_b, &ui);

    app.open_project(dir_b.clone());

    assert!(app.workspace_opened);
    assert_eq!(app.tree.as_ref().unwrap().root, dir_b, "tree repointed to B");
    assert_eq!(
        app.active_file.as_deref(),
        Some(dir_b.join("util.cpp").as_path()),
        "restored active tab drives active_file"
    );
    assert!(
        app.breakpoints.to_map().contains_key(&util),
        "B's persisted breakpoints restored"
    );

    let _ = std::fs::remove_dir_all(&dir_a);
    let _ = std::fs::remove_dir_all(&dir_b);
}

/// `open_project` on a folder with no persisted tabs falls back to the first
/// source file alphabetically (mirrors `--project`).
#[test]
fn open_project_falls_back_to_first_source_file() {
    let (dir_a, _file_a) = test_workspace();
    let (deps, _rx) = test_deps(dir_a.clone());
    let mut app = JadeApp::assemble(deps);

    let dir_b = std::env::temp_dir().join(format!("jade-openproj2-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir_b);
    std::fs::create_dir_all(&dir_b).unwrap();
    std::fs::write(dir_b.join("zeta.cpp"), "int z(){return 0;}\n").unwrap();
    std::fs::write(dir_b.join("beta.cpp"), "int b(){return 0;}\n").unwrap();

    app.open_project(dir_b.clone());
    assert_eq!(
        app.active_file.as_deref(),
        Some(dir_b.join("beta.cpp").as_path()),
        "first source file alphabetically opens"
    );
    let _ = std::fs::remove_dir_all(&dir_a);
    let _ = std::fs::remove_dir_all(&dir_b);
}

/// Switching to another project and back keeps unsaved edits: the outgoing
/// project's live editor (dirty buffers included) is stashed in memory, so the
/// return trip restores it instead of reloading pristine files from disk.
#[test]
fn unsaved_edits_survive_project_switch() {
    let (dir_a, file_a) = test_workspace();
    let (deps, _rx) = test_deps(dir_a.clone());
    let mut app = JadeApp::assemble(deps);

    // Open main.cpp in A and make an unsaved edit.
    app.open_file(file_a.clone());
    app.editor.active_tab_mut().unwrap().buffer.type_char('Z');
    let edited = app.editor.active_tab().unwrap().buffer.to_string();
    assert_ne!(
        edited,
        std::fs::read_to_string(&file_a).unwrap(),
        "buffer diverged from disk"
    );
    assert!(app.editor.active_tab().unwrap().buffer.is_dirty());

    // A second project with its own file.
    let dir_b = std::env::temp_dir().join(format!("jade-switchedit-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir_b);
    std::fs::create_dir_all(&dir_b).unwrap();
    std::fs::write(dir_b.join("b.cpp"), "int b(){return 0;}\n").unwrap();

    // Switch away, then back — without ever saving.
    app.open_project(dir_b.clone());
    assert!(
        app.editor.active_tab().map(|t| t.path.as_path()) != Some(file_a.as_path()),
        "switched away from A's file"
    );
    app.open_project(dir_a.clone());

    let tab = app.editor.active_tab().expect("A's tab restored");
    assert_eq!(tab.path, file_a, "same file active back in A");
    assert_eq!(
        tab.buffer.to_string(),
        edited,
        "unsaved edit preserved, not reloaded from disk"
    );
    assert!(tab.buffer.is_dirty(), "still dirty after the round-trip");

    let _ = std::fs::remove_dir_all(&dir_a);
    let _ = std::fs::remove_dir_all(&dir_b);
}

/// Page position is preserved across a project switch: scroll deep into A's
/// file, switch to B and back, and the viewport returns to the same top row
/// (the scroll position rides along with the stashed editor).
#[gpui::test]
async fn page_position_survives_project_switch(cx: &mut TestAppContext) {
    // Project A with a long file so there's somewhere to scroll.
    let dir_a = std::env::temp_dir().join(format!("jade-scrollswitch-a-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir_a);
    std::fs::create_dir_all(&dir_a).unwrap();
    let file_a = dir_a.join("main.cpp");
    let body: String = (0..300).map(|i| format!("static int line_{i} = {i};\n")).collect();
    std::fs::write(&file_a, body).unwrap();

    let (deps, app_rx) = test_deps(dir_a.clone());
    let (app, cx) = cx.add_window_view(|_window, cx| JadeApp::new(cx, deps, app_rx));
    app.update_in(cx, |app, _w, cx| {
        app.open_file(file_a.clone());
        cx.notify();
    });
    cx.run_until_parked();

    // Scroll deep into the file, then read where the viewport landed.
    app.update_in(cx, |app, _w, cx| {
        app.code_scroll.scroll_to_item(150, gpui::ScrollStrategy::Top);
        cx.notify();
    });
    cx.run_until_parked();
    let saved_top = app.update_in(cx, |app, _w, _cx| app.editor_scroll_top());
    assert!(saved_top > 100, "scrolled deep (top={saved_top})");

    // Project B (short file — its own scroll resets to the top).
    let dir_b = std::env::temp_dir().join(format!("jade-scrollswitch-b-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir_b);
    std::fs::create_dir_all(&dir_b).unwrap();
    std::fs::write(dir_b.join("b.cpp"), "int b(){return 0;}\n").unwrap();

    // Switch away, then back.
    app.update_in(cx, |app, _w, cx| {
        app.open_project(dir_b.clone());
        cx.notify();
    });
    cx.run_until_parked();
    app.update_in(cx, |app, _w, cx| {
        app.open_project(dir_a.clone());
        cx.notify();
    });
    cx.run_until_parked();

    let restored_top = app.update_in(cx, |app, _w, _cx| app.editor_scroll_top());
    assert!(
        (restored_top as i64 - saved_top as i64).abs() <= 1,
        "page position restored: saved={saved_top} restored={restored_top}"
    );

    let _ = std::fs::remove_dir_all(&dir_a);
    let _ = std::fs::remove_dir_all(&dir_b);
}

/// Each tab keeps its own page position: scroll deep in tab A, switch to tab B
/// (which shows at the top), switch back to A and it's where you left it.
#[gpui::test]
async fn page_position_is_per_tab(cx: &mut TestAppContext) {
    let dir = std::env::temp_dir().join(format!("jade-tabscroll-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let long: String = (0..300).map(|i| format!("int a_{i} = {i};\n")).collect();
    let file_a = dir.join("a.cpp");
    let file_b = dir.join("b.cpp");
    std::fs::write(&file_a, &long).unwrap();
    std::fs::write(&file_b, &long).unwrap();
    let (deps, app_rx) = test_deps(dir.clone());

    let (app, cx) = cx.add_window_view(|_window, cx| JadeApp::new(cx, deps, app_rx));
    // Open both files as tabs (B ends up active).
    app.update_in(cx, |app, _w, cx| {
        app.open_file(file_a.clone());
        // Promote A so the open of B does not replace the preview tab.
        app.editor.active_tab_mut().unwrap().preview = false;
        app.open_file(file_b.clone());
        cx.notify();
    });
    cx.run_until_parked();

    // Switch to tab A and scroll it deep.
    let (a_idx, b_idx) = app.update_in(cx, |app, _w, _cx| {
        (
            app.editor.index_of(&file_a).unwrap(),
            app.editor.index_of(&file_b).unwrap(),
        )
    });
    app.update_in(cx, |app, _w, cx| {
        app.switch_tab(a_idx);
        cx.notify();
    });
    cx.run_until_parked();
    app.update_in(cx, |app, _w, cx| {
        app.code_scroll.scroll_to_item(150, gpui::ScrollStrategy::Top);
        cx.notify();
    });
    cx.run_until_parked();
    let a_top = app.update_in(cx, |app, _w, _cx| app.editor_scroll_top());
    assert!(a_top > 100, "tab A scrolled deep (top={a_top})");

    // Switch to B — it must show at the top (its own untouched position).
    app.update_in(cx, |app, _w, cx| {
        app.switch_tab(b_idx);
        cx.notify();
    });
    cx.run_until_parked();
    let b_top = app.update_in(cx, |app, _w, _cx| app.editor_scroll_top());
    assert!(b_top <= 1, "tab B keeps its own top position (top={b_top})");

    // Back to A — restored to where we left it.
    app.update_in(cx, |app, _w, cx| {
        app.switch_tab(a_idx);
        cx.notify();
    });
    cx.run_until_parked();
    let a_again = app.update_in(cx, |app, _w, _cx| app.editor_scroll_top());
    assert!(
        (a_again as i64 - a_top as i64).abs() <= 1,
        "tab A page position restored: was={a_top} now={a_again}"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

/// `close_project` (§2): closing a background project just drops it; closing the
/// active one switches to a neighbor; the last project can't be closed. `cx`-free.
#[test]
fn close_project_switches_and_removes() {
    let (dir_a, _file_a) = test_workspace();
    let (deps, _rx) = test_deps(dir_a.clone());
    let mut app = JadeApp::assemble(deps);

    let mk = |tag: &str| {
        let d = std::env::temp_dir().join(format!("jade-closeproj-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        std::fs::write(d.join("main.cpp"), "int main(){return 0;}\n").unwrap();
        d
    };
    let dir_b = mk("b");
    let dir_c = mk("c");

    // assemble seeds [A]; open B then C → [A, B, C] with C active.
    app.open_project(dir_b.clone());
    app.open_project(dir_c.clone());
    assert_eq!(app.open_projects, vec![dir_a.clone(), dir_b.clone(), dir_c.clone()]);
    assert_eq!(app.tree.as_ref().unwrap().root, dir_c, "last-opened project is active");

    // Close a background project (B): removed; C stays active.
    app.close_project(&dir_b);
    assert_eq!(app.open_projects, vec![dir_a.clone(), dir_c.clone()]);
    assert_eq!(app.tree.as_ref().unwrap().root, dir_c, "closing a background project keeps the active root");

    // Close the active project (C): switches to the remaining neighbor (A).
    app.close_project(&dir_c);
    assert_eq!(app.open_projects, vec![dir_a.clone()]);
    assert_eq!(app.tree.as_ref().unwrap().root, dir_a, "closing the active project switches to a neighbor");

    // The only remaining project can't be closed — nowhere to switch to.
    app.close_project(&dir_a);
    assert_eq!(app.open_projects, vec![dir_a.clone()], "the last project is never closed");
    assert_eq!(app.tree.as_ref().unwrap().root, dir_a);

    for d in [dir_a, dir_b, dir_c] {
        let _ = std::fs::remove_dir_all(&d);
    }
}

/// AI settings menu state (sparkle popover): toggling opens/closes it, and the
/// model selector updates the shown tier (backend `set_model` is fire-and-forget
/// on the runtime; here we assert the UI-visible selection).
#[test]
fn ai_menu_toggle_and_model_selection() {
    use jade_ai::AiModelId;
    let (dir, _file) = test_workspace();
    let (deps, _rx) = test_deps(dir.clone());
    let mut app = JadeApp::assemble(deps);

    // Redirect prefs to a temp file so the test never writes the user's real
    // ~/.config/jade/ai.json.
    let prefs_path = dir.join("ai.json");
    app.ai_prefs = crate::ai_prefs::AiPrefs::load_from(&prefs_path);

    assert!(!app.ai_menu_open, "menu starts closed");
    app.toggle_ai_menu();
    assert!(app.ai_menu_open, "sparkle opens the menu");

    app.set_ai_model(AiModelId::Balanced);
    assert_eq!(app.ai_model, AiModelId::Balanced, "selecting a tier updates it");
    // …and the choice was persisted to the redirected prefs file.
    assert_eq!(
        crate::ai_prefs::AiPrefs::load_from(&prefs_path).model,
        AiModelId::Balanced,
        "tier persists across a reload"
    );

    app.close_ai_menu();
    assert!(!app.ai_menu_open, "outside click / choice closes the menu");
    let _ = std::fs::remove_dir_all(&dir);
}

/// The Run split button's chevron opens the run menu; Esc, an outside click,
/// or a choice closes it.
#[test]
fn run_menu_toggles_and_closes() {
    let (dir, _file) = test_workspace();
    let (deps, _rx) = test_deps(dir.clone());
    let mut app = JadeApp::assemble(deps);

    assert!(!app.run_menu_open, "menu starts closed");
    app.toggle_run_menu();
    assert!(app.run_menu_open, "chevron opens the menu");
    app.toggle_run_menu();
    assert!(!app.run_menu_open, "chevron again closes it");

    app.toggle_run_menu();
    app.close_run_menu();
    assert!(!app.run_menu_open, "Esc / backdrop / a choice closes the menu");
    let _ = std::fs::remove_dir_all(&dir);
}

/// Clicking the sparkle starts the managed AI backend when completion is enabled
/// but the server is idle — so the user never has to toggle the switch off and on
/// to wake it up. `cx`-free (the spawned `start()` queues on the never-driven
/// test runtime; we assert the decision, not the process).
#[test]
fn sparkle_starts_idle_ai_backend() {
    use jade_ai::AiState;
    let (dir, _file) = test_workspace();
    let (deps, _rx) = test_deps(dir.clone());
    let mut app = JadeApp::assemble(deps);

    // Fresh launch: completion defaults on, but the backend is Disabled — this
    // was the bug (ghost text stayed dead until you toggled off/on).
    assert!(app.ai_completion_enabled, "completion on by default");
    assert_eq!(app.ai_status.state, AiState::Disabled, "backend idle at launch");

    // Opening the sparkle menu kicks off the backend.
    app.toggle_ai_menu();
    assert!(app.ai_menu_open);
    // A second ensure while still idle would re-issue (idempotent at the backend),
    // but once the backend reports Ready it must NOT restart.
    app.ai_status.state = AiState::Ready;
    assert!(!app.ensure_ai_started(), "no restart once the backend is running");

    // Error is treated as idle → the sparkle retries it.
    app.ai_status.state = AiState::Error;
    assert!(app.ensure_ai_started(), "sparkle retries a failed backend");

    // With completion disabled, the sparkle never force-starts the server.
    app.ai_status.state = AiState::Disabled;
    app.ai_completion_enabled = false;
    assert!(!app.ensure_ai_started(), "disabled completion stays off");

    let _ = std::fs::remove_dir_all(&dir);
}

/// The AI menu renders through GPUI's real layout without panicking (its checkbox
/// / radio glyphs, model rows, and status footer all lay out).
#[gpui::test]
async fn ai_menu_renders(cx: &mut TestAppContext) {
    let (dir, _file) = test_workspace();
    let (deps, app_rx) = test_deps(dir.clone());
    let (app, cx) = cx.add_window_view(|_window, cx| JadeApp::new(cx, deps, app_rx));
    app.update_in(cx, |app, _window, cx| {
        app.toggle_ai_menu();
        cx.notify();
    });
    cx.run_until_parked();
    app.update_in(cx, |app, _window, _cx| {
        assert!(app.ai_menu_open, "menu open after toggle → it painted a frame");
    });
    let _ = std::fs::remove_dir_all(&dir);
}

/// The signature hint is cleared by `dismiss_popups` (the path Escape and an
/// editor click both funnel through). The response-parsing + generation-supersede
/// logic is covered by `jade_lsp::active_signature_hint` and mirrors hover.
#[test]
fn signature_hint_dismisses() {
    let (dir, _file) = test_workspace();
    let (deps, _rx) = test_deps(dir.clone());
    let mut app = JadeApp::assemble(deps);

    app.signature = Some(crate::app::SignatureState {
        label: "Point(int x, int y)".into(),
        active_param: Some(13..18), // "int y"
        anchor: (0, 6),
    });
    // Sanity: the active-param slice is a valid byte range into the label.
    let s = app.signature.as_ref().unwrap();
    assert_eq!(&s.label[s.active_param.clone().unwrap()], "int y");

    app.dismiss_popups();
    assert!(app.signature.is_none(), "dismiss_popups clears the hint");
    let _ = std::fs::remove_dir_all(&dir);
}

/// The signature hint renders through GPUI's real layout (the before/active/after
/// label split) without panicking on the active-parameter byte slice.
#[gpui::test]
async fn signature_hint_renders(cx: &mut TestAppContext) {
    let (dir, file) = test_workspace();
    let (deps, app_rx) = test_deps(dir.clone());
    let (app, cx) = cx.add_window_view(|_window, cx| JadeApp::new(cx, deps, app_rx));
    app.update_in(cx, |app, _window, cx| {
        app.open_file(file.clone());
        app.signature = Some(crate::app::SignatureState {
            label: "Point(int x, int y)".into(),
            active_param: Some(13..18),
            anchor: (2, 6), // row 2 has room above it in the viewport
        });
        cx.notify();
    });
    cx.run_until_parked();
    app.update_in(cx, |app, _window, _cx| {
        assert!(app.signature.is_some(), "signature hint painted a frame");
    });
    let hint = cx
        .debug_bounds("signature-hint")
        .expect("the signature hint popup must actually paint when `signature` is set");
    // The hint must NOT overlap the caret's own line (row 2) — that was the bug.
    let caret_row = cx.debug_bounds("code-cell-2").expect("row 2 painted");
    let hint_bottom = hint.origin.y + hint.size.height;
    let row_top = caret_row.origin.y;
    let row_bottom = caret_row.origin.y + caret_row.size.height;
    assert!(
        hint_bottom <= row_top || hint.origin.y >= row_bottom,
        "signature hint overlaps the caret line: hint=[{:?},{:?}] row=[{:?},{:?}]",
        hint.origin.y, hint_bottom, row_top, row_bottom
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// Regression for the abort on "delete the `#include` lines under a hover".
/// The hover popup keeps a buffer row. A delete that shortens the buffer must
/// hide the popup, and a stale row that still reaches the render must not
/// read a line past the end of the buffer.
#[gpui::test]
async fn hover_survives_line_deletion(cx: &mut TestAppContext) {
    let (dir, _cpp) = test_workspace();
    let file = dir.join("hdr.h");
    std::fs::write(&file, "#include <a>\n#include <b>\n#include <c>\n").unwrap();
    let (deps, app_rx) = test_deps(dir.clone());

    let (app, cx) = cx.add_window_view(|_window, cx| JadeApp::new(cx, deps, app_rx));
    app.update_in(cx, |app, _window, cx| {
        app.open_file(file.clone());
        cx.notify();
    });
    cx.run_until_parked();

    // Focus the editor with a real click on the last include line.
    let cell = cx
        .debug_bounds("code-cell-2")
        .expect("code cell for row 2 was painted");
    cx.simulate_click(
        point(
            cell.origin.x + px(1.0),
            cell.origin.y + px(crate::panels::code_view::LINE_H / 2.0),
        ),
        Modifiers::default(),
    );

    // A squiggle tooltip sits on row 2, then the user selects all three
    // include lines and presses Backspace.
    app.update_in(cx, |app, _w, cx| {
        app.hover = Some(crate::app::HoverState {
            text: String::new(),
            row: 2,
            col: 3,
            diagnostics: vec![(None, "included header is not used".into())],
        });
        select_bytes(app, 0..39);
        cx.notify();
    });
    cx.run_until_parked();
    cx.simulate_keystrokes("backspace");
    cx.run_until_parked();

    app.update_in(cx, |app, _w, _cx| {
        let tab = app.editor.active_tab().unwrap();
        assert_eq!(tab.buffer.to_string(), "", "the three lines are gone");
        assert!(app.hover.is_none(), "an edit hides the hover popup");
    });

    // A late LSP reply can still land a row past the end. The render must
    // clamp instead of abort.
    app.update_in(cx, |app, _w, cx| {
        app.hover = Some(crate::app::HoverState {
            text: "int a".into(),
            row: 2,
            col: 3,
            diagnostics: Vec::new(),
        });
        app.signature = Some(crate::app::SignatureState {
            label: "f(int x)".into(),
            active_param: Some(2..7),
            anchor: (2, 3),
        });
        cx.notify();
    });
    cx.run_until_parked();
    app.update_in(cx, |app, _w, _cx| {
        assert!(app.hover.is_some(), "the frame painted with a stale hover row");
    });
    let _ = std::fs::remove_dir_all(&dir);
}

/// After the code list scrolls horizontally, a click at the same physical pixel
/// must map to a column shifted by the scroll amount (the click→column mapping
/// folds in `editor_h_scroll`). Regression for "everything misaligns when I
/// scroll sideways".
#[gpui::test]
async fn horizontal_scroll_click_maps_to_shifted_column(cx: &mut TestAppContext) {
    let dir = std::env::temp_dir().join(format!("jade-hscroll-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let file = dir.join("main.cpp");
    // One very long line so there's room to scroll sideways and click deep.
    std::fs::write(&file, format!("int x = {};\n", "a".repeat(300))).unwrap();
    let (deps, app_rx) = test_deps(dir.clone());

    let (app, cx) = cx.add_window_view(|_window, cx| JadeApp::new(cx, deps, app_rx));
    app.update_in(cx, |app, _w, cx| {
        app.open_file(file.clone());
        cx.notify();
    });
    cx.run_until_parked();

    let cell = cx.debug_bounds("code-cell-0").expect("row 0 painted");
    let cw = app.update_in(cx, |app, _w, _cx| app.char_w());
    // A fixed physical click position ~20 columns into the (unscrolled) row.
    let click = gpui::point(
        cell.origin.x + px(20.0 * cw + 1.0),
        cell.origin.y + px(crate::panels::code_view::LINE_H / 2.0),
    );
    cx.simulate_click(click, Modifiers::default());
    let col0 = app.update_in(cx, |app, _w, _cx| app.editor.active_tab().unwrap().caret_point().col);

    // Scroll right by ~30 columns, then click the SAME physical pixel.
    let shift_cols = 30.0;
    app.update_in(cx, |app, _w, cx| {
        app.code_scroll
            .0
            .borrow()
            .base_handle
            .set_offset(gpui::point(px(-shift_cols * cw), px(0.)));
        cx.notify();
    });
    cx.run_until_parked();
    cx.simulate_click(click, Modifiers::default());
    let col1 = app.update_in(cx, |app, _w, _cx| app.editor.active_tab().unwrap().caret_point().col);

    assert!(
        (col1 as i64 - col0 as i64 - shift_cols as i64).abs() <= 1,
        "same click after scrolling {shift_cols} cols right should land ~{shift_cols} cols further: col0={col0} col1={col1}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// Typing `(` (and `,`) through the real IME pipeline triggers a signature-help
/// request; typing an ordinary identifier char does not. This proves the
/// keystroke → `on_text_inserted` → `schedule_signature_help` routing that the
/// end-to-end `--smoke sighelp` exercises against real clangd.
#[gpui::test]
async fn typing_open_paren_triggers_signature_help(cx: &mut TestAppContext) {
    let (dir, file) = test_workspace();
    let (deps, app_rx) = test_deps(dir.clone());
    let (app, cx) = cx.add_window_view(|_window, cx| JadeApp::new(cx, deps, app_rx));
    app.update_in(cx, |app, _window, cx| {
        app.open_file(file.clone()); // focuses the editor
        cx.notify();
    });
    cx.run_until_parked();

    let before = app.read_with(cx, |app, _| app.sig_help_requests);
    cx.simulate_input("(");
    let after_paren = app.read_with(cx, |app, _| app.sig_help_requests);
    assert_eq!(after_paren, before + 1, "typing `(` requests signature help");

    cx.simulate_input("x"); // an identifier char must NOT trigger it
    let after_ident = app.read_with(cx, |app, _| app.sig_help_requests);
    assert_eq!(after_ident, after_paren, "identifier chars don't trigger it");

    cx.simulate_input(","); // a new argument re-triggers
    let after_comma = app.read_with(cx, |app, _| app.sig_help_requests);
    assert_eq!(after_comma, after_paren + 1, "typing `,` re-requests");
    let _ = std::fs::remove_dir_all(&dir);
}

/// Welcome mode (§2): assembled with `workspace_opened=false`, the tree is NOT
/// scanned (no fallback repo-root scan) and there is no active file.
#[test]
fn no_workspace_leaves_tree_unscanned() {
    let (dir, _file) = test_workspace();
    let (mut deps, _rx) = test_deps(dir.clone());
    deps.workspace_opened = false;
    deps.active_file = None;
    let app = JadeApp::assemble(deps);
    assert!(!app.workspace_opened);
    assert!(app.tree.is_none(), "welcome mode must not scan a tree");
    assert!(app.active_file.is_none());
    let _ = std::fs::remove_dir_all(&dir);
}

#[gpui::test]
async fn typing_inserts_through_input_pipeline(cx: &mut TestAppContext) {
    let (dir, file) = test_workspace();
    let (deps, app_rx) = test_deps(dir);

    let (app, cx) = cx.add_window_view(|_window, cx| JadeApp::new(cx, deps, app_rx));
    app.update_in(cx, |app, _window, cx| {
        app.open_file(file.clone());
        cx.notify();
    });
    cx.run_until_parked();

    // Click at the very start of row 2 ("    int beta = 2;").
    let cell = cx
        .debug_bounds("code-cell-2")
        .expect("code cell for row 2 was painted");
    cx.simulate_click(
        point(
            cell.origin.x + px(1.0),
            cell.origin.y + px(crate::panels::code_view::LINE_H / 2.0),
        ),
        Modifiers::default(),
    );

    // Type through the platform input pipeline (IME replace_text_in_range).
    cx.simulate_input("zz");
    app.update_in(cx, |app, _w, _cx| {
        let tab = app.editor.active_tab().unwrap();
        assert_eq!(
            tab.line(2),
            "zz    int beta = 2;",
            "typed text did not reach the buffer"
        );
        assert!(tab.buffer.is_dirty());
    });

    // Backspace through the key pipeline.
    cx.simulate_keystrokes("backspace");
    app.update_in(cx, |app, _w, _cx| {
        assert_eq!(app.editor.active_tab().unwrap().line(2), "z    int beta = 2;");
    });
}

/// Auto-indent end to end in a Verilog file: Enter after `begin` adds one
/// step, and a typed `end` re-aligns to the matching `begin`.
#[gpui::test]
async fn verilog_auto_indent_through_input_pipeline(cx: &mut TestAppContext) {
    let (dir, _cpp) = test_workspace();
    let file = dir.join("top.v");
    std::fs::write(&file, "always @(posedge clk) begin\n\tq <= d;\n").unwrap();
    let (deps, app_rx) = test_deps(dir);

    let (app, cx) = cx.add_window_view(|_window, cx| JadeApp::new(cx, deps, app_rx));
    app.update_in(cx, |app, _window, cx| {
        app.open_file(file.clone());
        let tab = app.editor.active_tab_mut().unwrap();
        let end = tab.buffer.len_bytes();
        tab.buffer.set_caret(end);
        cx.notify();
    });
    cx.run_until_parked();

    // The caret sits after "\tq <= d;\n" (column 0 of row 2). Enter first: the
    // balanced line above keeps its indent.
    cx.simulate_input("\tif (rst) begin");
    cx.simulate_keystrokes("enter");
    app.update_in(cx, |app, _w, _cx| {
        let tab = app.editor.active_tab().unwrap();
        assert_eq!(tab.line(2), "\tif (rst) begin");
        // Enter after `begin` adds one indent step.
        assert_eq!(tab.line(3), "\t\t");
    });

    // Type `end`: the line re-aligns from two tabs to the `if`'s one tab.
    cx.simulate_input("end");
    app.update_in(cx, |app, _w, _cx| {
        let tab = app.editor.active_tab().unwrap();
        assert_eq!(tab.line(3), "\tend", "`end` aligns to its `begin`");
    });
}

/// Regression: a plain click (down + the tiny pressed-button move macOS sends
/// before up) on a VISIBLE row must never scroll the list. This was the
/// "editor constantly jumps around when I click" bug.
#[gpui::test]
async fn click_on_visible_row_does_not_scroll(cx: &mut TestAppContext) {
    let dir = std::env::temp_dir().join(format!("jade-noscroll-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let file = dir.join("main.cpp");
    let body: String = (0..300)
        .map(|i| format!("static int line_{i} = {i};\n"))
        .collect();
    std::fs::write(&file, body).unwrap();
    let (deps, app_rx) = test_deps(dir);

    let (app, cx) = cx.add_window_view(|_window, cx| JadeApp::new(cx, deps, app_rx));
    app.update_in(cx, |app, _window, cx| {
        app.open_file(file.clone());
        cx.notify();
    });
    cx.run_until_parked();

    let (top0, rows) = app.update_in(cx, |app, _w, _cx| {
        (
            app.editor_scroll_top(),
            app.editor_rows.load(std::sync::atomic::Ordering::Relaxed),
        )
    });
    println!("top0={top0} rows={rows}");

    // Click a row in the middle of the viewport, with the drag-move a real
    // click generates.
    let probe = top0 + (rows as usize) / 2;
    let cell = cx
        .debug_bounds(match probe {
            _ => Box::leak(format!("code-cell-{probe}").into_boxed_str()) as &'static str,
        })
        .expect("probe row visible");
    let pos = gpui::point(
        cell.origin.x + px(30.),
        cell.origin.y + px(crate::panels::code_view::LINE_H / 2.0),
    );
    cx.simulate_mouse_down(pos, gpui::MouseButton::Left, Modifiers::default());
    cx.simulate_mouse_move(pos, gpui::MouseButton::Left, Modifiers::default());
    cx.simulate_mouse_up(pos, gpui::MouseButton::Left, Modifiers::default());
    cx.run_until_parked();

    let top1 = app.update_in(cx, |app, _w, _cx| app.editor_scroll_top());
    assert_eq!(
        top1, top0,
        "clicking a visible row must not scroll (top {top0} -> {top1})"
    );
}

/// Same as above but with the list scrolled deep into the file — exercises
/// `editor_scroll_top()` (top_item) accuracy, including the PAD_TOP offset.
#[gpui::test]
async fn click_on_visible_row_when_scrolled_does_not_scroll(cx: &mut TestAppContext) {
    let dir = std::env::temp_dir().join(format!("jade-noscroll2-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let file = dir.join("main.cpp");
    let body: String = (0..300)
        .map(|i| format!("static int line_{i} = {i};\n"))
        .collect();
    std::fs::write(&file, body).unwrap();
    let (deps, app_rx) = test_deps(dir);

    let (app, cx) = cx.add_window_view(|_window, cx| JadeApp::new(cx, deps, app_rx));
    app.update_in(cx, |app, _window, cx| {
        app.open_file(file.clone());
        cx.notify();
    });
    cx.run_until_parked();

    // Scroll deep, then re-read where the viewport actually is.
    app.update_in(cx, |app, _w, cx| {
        app.code_scroll
            .scroll_to_item(150, gpui::ScrollStrategy::Top);
        cx.notify();
    });
    cx.run_until_parked();
    let (top0, rows) = app.update_in(cx, |app, _w, _cx| {
        (
            app.editor_scroll_top(),
            app.editor_rows.load(std::sync::atomic::Ordering::Relaxed),
        )
    });
    println!("scrolled: top0={top0} rows={rows}");

    for offset in [1usize, (rows as usize) / 2, rows as usize - 2] {
        let probe = top0 + offset;
        let sel: &'static str = Box::leak(format!("code-cell-{probe}").into_boxed_str());
        let Some(cell) = cx.debug_bounds(sel) else {
            println!("row {probe} not painted; skipping");
            continue;
        };
        let pos = gpui::point(
            cell.origin.x + px(30.),
            cell.origin.y + px(crate::panels::code_view::LINE_H / 2.0),
        );
        cx.simulate_mouse_down(pos, gpui::MouseButton::Left, Modifiers::default());
        cx.simulate_mouse_move(pos, gpui::MouseButton::Left, Modifiers::default());
        cx.simulate_mouse_up(pos, gpui::MouseButton::Left, Modifiers::default());
        cx.run_until_parked();
        let top1 = app.update_in(cx, |app, _w, _cx| app.editor_scroll_top());
        assert_eq!(
            top1, top0,
            "clicking visible row {probe} (offset {offset}) scrolled {top0} -> {top1}"
        );
    }
}

/// Clicking to the RIGHT of a line's text (inside the row, past the last char)
/// must put the caret at that line's end; typing with the caret scrolled out of
/// view must follow-scroll so the caret is visible again.
#[gpui::test]
async fn click_past_eol_and_type_follow_scroll(cx: &mut TestAppContext) {
    let dir = std::env::temp_dir().join(format!("jade-eol-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let file = dir.join("main.cpp");
    let body: String = (0..300)
        .map(|i| format!("static int line_{i} = {i};\n"))
        .collect();
    std::fs::write(&file, body).unwrap();
    let (deps, app_rx) = test_deps(dir);

    let (app, cx) = cx.add_window_view(|_window, cx| JadeApp::new(cx, deps, app_rx));
    app.update_in(cx, |app, _window, cx| {
        app.open_file(file.clone());
        cx.notify();
    });
    cx.run_until_parked();

    // Click far to the right of row 3's short text — caret must land at its EOL.
    let cell = cx.debug_bounds("code-cell-3").expect("row 3 painted");
    let pos = gpui::point(
        cell.origin.x + cell.size.width - px(20.),
        cell.origin.y + px(crate::panels::code_view::LINE_H / 2.0),
    );
    cx.simulate_click(pos, Modifiers::default());
    let eol = "static int line_3 = 3;".chars().count();
    app.update_in(cx, |app, _w, _cx| {
        let caret = app.editor.active_tab().unwrap().caret_point();
        assert_eq!(
            (caret.row, caret.col),
            (3, eol),
            "click right of code must set caret at that line's end"
        );
    });

    // Scroll the caret out of view (deep), then type: viewport must follow.
    app.update_in(cx, |app, _w, cx| {
        app.code_scroll
            .scroll_to_item(200, gpui::ScrollStrategy::Top);
        cx.notify();
    });
    cx.run_until_parked();
    let top_before = app.update_in(cx, |app, _w, _cx| app.editor_scroll_top());
    assert!(top_before > 100, "precondition: scrolled deep ({top_before})");

    cx.simulate_input("z");
    cx.run_until_parked();
    let (top_after, rows, caret_row) = app.update_in(cx, |app, _w, _cx| {
        (
            app.editor_scroll_top(),
            app.editor_rows.load(std::sync::atomic::Ordering::Relaxed) as usize,
            app.editor.active_tab().unwrap().caret_point().row,
        )
    });
    assert_eq!(caret_row, 3);
    assert!(
        top_after <= caret_row && caret_row < top_after + rows,
        "typing out of view must scroll the caret back into view (top {top_after}, rows {rows})"
    );
}

/// ⌘⌫ deletes the whole current line; ⌘← is line-aware Home: first press goes
/// to the text edge (first non-whitespace), second to column 0.
#[gpui::test]
async fn cmd_backspace_and_smart_home(cx: &mut TestAppContext) {
    let (dir, file) = test_workspace();
    let (deps, app_rx) = test_deps(dir);

    let (app, cx) = cx.add_window_view(|_window, cx| JadeApp::new(cx, deps, app_rx));
    app.update_in(cx, |app, _window, cx| {
        app.open_file(file.clone());
        cx.notify();
    });
    cx.run_until_parked();

    // Caret into row 1 ("    int alpha = 1;") at col 8.
    let cell = cx.debug_bounds("code-cell-1").expect("row 1 painted");
    cx.simulate_click(
        gpui::point(
            cell.origin.x + px(8.0 * crate::panels::code_view::CHAR_W + 1.0),
            cell.origin.y + px(crate::panels::code_view::LINE_H / 2.0),
        ),
        Modifiers::default(),
    );

    // Smart home: ⌘← → col 4 (text edge), again → col 0, again → back to 4.
    cx.simulate_keystrokes("cmd-left");
    app.update_in(cx, |app, _w, _cx| {
        assert_eq!(app.editor.active_tab().unwrap().caret_point().col, 4);
    });
    cx.simulate_keystrokes("cmd-left");
    app.update_in(cx, |app, _w, _cx| {
        assert_eq!(app.editor.active_tab().unwrap().caret_point().col, 0);
    });
    cx.simulate_keystrokes("cmd-left");
    app.update_in(cx, |app, _w, _cx| {
        assert_eq!(app.editor.active_tab().unwrap().caret_point().col, 4);
    });

    // ⌘⌫ deletes the whole line: row 1 becomes the old row 2.
    cx.simulate_keystrokes("cmd-backspace");
    app.update_in(cx, |app, _w, _cx| {
        let tab = app.editor.active_tab().unwrap();
        assert_eq!(tab.line(1), "    int beta = 2;", "line 1 deleted wholesale");
        assert!(tab.buffer.is_dirty());
    });
}

/// ⌘F opens the find bar, typing routes into its captured buffer, the match
/// count + selection track the query, Enter walks matches, and replace-all
/// rewrites every hit through the real edit pipeline.
#[gpui::test]
async fn find_bar_opens_types_navigates_and_replaces(cx: &mut TestAppContext) {
    let (dir, file) = test_workspace();
    let (deps, app_rx) = test_deps(dir);

    let (app, cx) = cx.add_window_view(|_window, cx| JadeApp::new(cx, deps, app_rx));
    app.update_in(cx, |app, _window, cx| {
        app.open_file(file.clone());
        cx.notify();
    });
    cx.run_until_parked();

    // ⌘F opens the bar and hands its captured buffer keyboard focus.
    cx.simulate_keystrokes("cmd-f");
    cx.run_until_parked();
    app.update_in(cx, |app, window, _cx| {
        assert!(app.find.is_some(), "cmd-f must open the find bar");
        let focused = app
            .find_focus
            .as_ref()
            .map(|f| f.is_focused(window))
            .unwrap_or(false);
        assert!(focused, "find bar must own keyboard focus while open");
    });

    // "alpha" occurs on row 1 ("int alpha = 1;") and row 3 ("return alpha + beta;").
    cx.simulate_keystrokes("a l p h a");
    app.update_in(cx, |app, _w, _cx| {
        let st = app.find.as_ref().unwrap();
        assert_eq!(st.query, "alpha");
        assert_eq!(st.count(), 2, "two matches for alpha");
        // The current match is selected in the buffer (row 1's alpha).
        let sel = app.editor.active_tab().unwrap().buffer.selection();
        let start = app.editor.active_tab().unwrap().buffer.offset_to_point(sel.start());
        assert_eq!(start.row, 1, "first match selected on row 1");
    });

    // Enter walks to the next match (row 3), Enter again wraps back to row 1.
    cx.simulate_keystrokes("enter");
    app.update_in(cx, |app, _w, _cx| {
        let sel = app.editor.active_tab().unwrap().buffer.selection();
        let start = app.editor.active_tab().unwrap().buffer.offset_to_point(sel.start());
        assert_eq!(start.row, 3, "enter advances to the row-3 match");
    });
    cx.simulate_keystrokes("enter");
    app.update_in(cx, |app, _w, _cx| {
        let sel = app.editor.active_tab().unwrap().buffer.selection();
        let start = app.editor.active_tab().unwrap().buffer.offset_to_point(sel.start());
        assert_eq!(start.row, 1, "enter wraps back to the row-1 match");
    });

    // The replace row is shown by default (the dropdown). Click into the replace
    // field to focus it, then type — proving the fields are clickable.
    let rf = cx
        .debug_bounds("find-replace-field")
        .expect("replace field painted");
    cx.simulate_click(
        point(rf.origin.x + px(10.0), rf.origin.y + px(11.0)),
        Modifiers::default(),
    );
    cx.run_until_parked();
    app.update_in(cx, |app, _w, _cx| {
        assert_eq!(
            app.find.as_ref().unwrap().field,
            crate::find::FindField::Replace,
            "clicking the replace field makes it active"
        );
    });
    cx.simulate_keystrokes("g a m m a");
    app.update_in(cx, |app, _w, _cx| {
        assert_eq!(app.find.as_ref().unwrap().replace, "gamma");
    });

    // Enter while the replace field is focused replaces the current match (row 1)
    // and advances — the other occurrence is untouched.
    cx.simulate_keystrokes("enter");
    app.update_in(cx, |app, _w, _cx| {
        let tab = app.editor.active_tab().unwrap();
        assert_eq!(tab.line(1), "    int gamma = 1;", "row 1 replaced via Enter");
        assert_eq!(tab.line(3), "    return alpha + beta;", "row 3 not yet replaced");
        assert_eq!(app.find.as_ref().unwrap().count(), 1, "one alpha match left");
    });
    // Enter again replaces the remaining match (row 3).
    cx.simulate_keystrokes("enter");
    app.update_in(cx, |app, _w, _cx| {
        let tab = app.editor.active_tab().unwrap();
        assert_eq!(tab.line(3), "    return gamma + beta;", "row 3 replaced via Enter");
        assert_eq!(app.find.as_ref().unwrap().count(), 0, "no alpha matches remain");
    });

    // Esc closes the bar and returns focus to the editor.
    cx.simulate_keystrokes("escape");
    cx.run_until_parked();
    app.update_in(cx, |app, window, _cx| {
        assert!(app.find.is_none(), "escape closes the find bar");
        let focused = app
            .editor_focus
            .as_ref()
            .map(|f| f.is_focused(window))
            .unwrap_or(false);
        assert!(focused, "editor regains focus after the bar closes");
    });
}

/// The two reported find-bar breakages, through real GPUI dispatch: (1) editing
/// the buffer while the bar is open must rescan the matches so the wash ranges
/// don't go stale (the "typed a space/tab and the highlights slid off" bug),
/// and (2) clicking mid-text in the query field must put the caret between the
/// clicked chars so typing inserts there (previously append-only).
#[gpui::test]
async fn find_bar_tracks_edits_and_clicks_place_the_caret(cx: &mut TestAppContext) {
    let (dir, file) = test_workspace();
    let (deps, app_rx) = test_deps(dir);

    let (app, cx) = cx.add_window_view(|_window, cx| JadeApp::new(cx, deps, app_rx));
    app.update_in(cx, |app, _window, cx| {
        app.open_file(file.clone());
        cx.notify();
    });
    cx.run_until_parked();

    cx.simulate_keystrokes("cmd-f");
    cx.run_until_parked();
    cx.simulate_keystrokes("a l p h a");
    cx.run_until_parked();

    let before = app.update_in(cx, |app, _w, _cx| {
        let st = app.find.as_ref().unwrap();
        assert_eq!(st.count(), 2, "two alpha matches");
        st.matches.clone()
    });

    // Click into the editor at row 0 col 0 and type: the insertion lands before
    // both matches, so their byte ranges shift and the bar must follow.
    let row = cx.debug_bounds("code-cell-0").expect("row 0 painted");
    cx.simulate_click(
        point(row.origin.x + px(2.0), row.origin.y + px(2.0)),
        Modifiers::default(),
    );
    cx.run_until_parked();
    cx.simulate_keystrokes("x");
    cx.run_until_parked();
    app.update_in(cx, |app, _w, _cx| {
        let st = app.find.as_ref().unwrap();
        assert_eq!(st.count(), 2, "matches rescanned after the buffer edit");
        let shifted: Vec<_> = before.iter().map(|r| r.start + 1..r.end + 1).collect();
        assert_eq!(st.matches, shifted, "ranges follow the inserted byte");
    });

    // Click between the 2nd and 3rd chars of the query ("al|pha") using the
    // geometry the field's canvas captured (same data the real handler reads).
    let qf = cx.debug_bounds("find-query-field").expect("query field painted");
    let (left, cw) = app.update_in(cx, |app, _w, _cx| {
        use std::sync::atomic::Ordering;
        (
            f32::from_bits(app.find_field_left[0].load(Ordering::Relaxed)),
            f32::from_bits(app.find_char_w.load(Ordering::Relaxed)),
        )
    });
    assert!(cw > 0.0, "field canvas measured the char advance");
    cx.simulate_click(
        point(px(left + 2.2 * cw), qf.origin.y + px(11.0)),
        Modifiers::default(),
    );
    cx.run_until_parked();
    app.update_in(cx, |app, _w, _cx| {
        assert_eq!(
            app.find.as_ref().unwrap().cursor,
            2,
            "click lands the caret between 'al' and 'pha'"
        );
    });

    // Typing now inserts at the caret (and rescans: "alzpha" matches nothing).
    cx.simulate_keystrokes("z");
    cx.run_until_parked();
    app.update_in(cx, |app, _w, _cx| {
        let st = app.find.as_ref().unwrap();
        assert_eq!(st.query, "alzpha", "char inserted mid-text at the caret");
        assert_eq!(st.count(), 0);
    });

    // Arrow keys move the caret and backspace deletes before it: from "alz|pha"
    // a → then ← round-trip leaves the caret after the 'z', and backspace
    // removes exactly it — mid-text, not the trailing 'a'.
    cx.simulate_keystrokes("right left backspace");
    cx.run_until_parked();
    app.update_in(cx, |app, _w, _cx| {
        let st = app.find.as_ref().unwrap();
        assert_eq!(st.query, "alpha", "backspace deleted the mid-text char");
        assert_eq!(st.count(), 2, "query restored, matches rescanned");
    });
}

/// End-to-end ghost text on an auto-indented line (§4.11): press Enter at the
/// end of an indented row (auto-indent copies the 4-space lead), feed the
/// `/infill` response the backend would send, and require a painted ghost run —
/// then Tab must insert the suggestion at the caret. This replays the reported
/// "no ghost text after Enter/indent" scenario through real GPUI dispatch.
#[gpui::test]
async fn ghost_text_paints_on_indented_line_and_tab_accepts(cx: &mut TestAppContext) {
    use crate::app::AppEvent;
    use jade_ai::AiState;

    let (dir, file) = test_workspace();
    let (deps, app_rx) = test_deps(dir.clone());
    let (app, cx) = cx.add_window_view(|_window, cx| JadeApp::new(cx, deps, app_rx));
    app.update_in(cx, |app, _window, cx| {
        app.open_file(file.clone());
        // The backend is Ready before the edit — otherwise schedule_ghost bails.
        app.ai_status.state = AiState::Ready;
        cx.notify();
    });
    cx.run_until_parked();

    // Caret to the end of row 1 ("    int alpha = 1;"), then Enter: auto-indent
    // makes row 2 == "    " with the caret at col 4 — the reported scenario.
    cx.simulate_keystrokes("down");
    cx.simulate_keystrokes("cmd-right");
    cx.simulate_keystrokes("enter");
    cx.run_until_parked();

    let (prefix, suffix) = app.update_in(cx, |app, _w, _cx| {
        let tab = app.editor.active_tab().unwrap();
        assert_eq!(tab.line(2), "    ", "Enter auto-indented the new row");
        assert_eq!(tab.caret_point().col, 4, "caret sits after the indent");
        assert!(app.ghost.is_none(), "no ghost yet — request still in flight");
        let full = tab.buffer.to_string();
        let caret = tab.buffer.selection().caret();
        (full[..caret].to_string(), full[caret..].to_string())
    });

    // Deliver the `/infill` response the debounced task would have sent
    // (generation 1: the Enter was the only edit since launch).
    app.update_in(cx, |app, _w, cx| {
        app.apply_app_event(AppEvent::Ghost {
            generation: 1,
            content: Some("int gamma = alpha + beta;".into()),
            prefix,
            suffix,
            line_suffix: String::new(),
            anchor: (2, 4),
            max_lines: 6,
        });
        cx.notify();
    });
    cx.run_until_parked();

    app.update_in(cx, |app, _w, _cx| {
        assert_eq!(
            app.ghost.as_ref().map(|g| g.text.as_str()),
            Some("int gamma = alpha + beta;"),
            "ghost state set from the infill response"
        );
    });
    assert!(
        cx.debug_bounds("ghost-run").is_some(),
        "ghost run must actually paint on the indented caret row"
    );

    // Tab accepts: the suggestion lands after the indent.
    cx.simulate_keystrokes("tab");
    app.update_in(cx, |app, _w, _cx| {
        let tab = app.editor.active_tab().unwrap();
        assert_eq!(
            tab.line(2),
            "    int gamma = alpha + beta;",
            "Tab inserted the ghost at the caret"
        );
        assert!(app.ghost.is_none(), "accepting consumes the ghost");
    });
    let _ = std::fs::remove_dir_all(&dir);
}

/// Word-by-word partial accept (⌥→, ported from JetBrains FLCC): each press
/// inserts just the next word of the ghost and the remainder stays ghosted —
/// served straight from the typed-through cache, no new model round-trip.
#[gpui::test]
async fn ghost_word_accept_keeps_remainder(cx: &mut TestAppContext) {
    use crate::app::AppEvent;
    use jade_ai::AiState;

    let (dir, file) = test_workspace();
    let (deps, app_rx) = test_deps(dir.clone());
    let (app, cx) = cx.add_window_view(|_window, cx| JadeApp::new(cx, deps, app_rx));
    app.update_in(cx, |app, _window, cx| {
        app.open_file(file.clone());
        app.ai_status.state = AiState::Ready;
        cx.notify();
    });
    cx.run_until_parked();

    cx.simulate_keystrokes("down");
    cx.simulate_keystrokes("cmd-right");
    cx.simulate_keystrokes("enter");
    cx.run_until_parked();

    let (prefix, suffix) = app.update_in(cx, |app, _w, _cx| {
        let tab = app.editor.active_tab().unwrap();
        let full = tab.buffer.to_string();
        let caret = tab.buffer.selection().caret();
        (full[..caret].to_string(), full[caret..].to_string())
    });
    app.update_in(cx, |app, _w, cx| {
        app.apply_app_event(AppEvent::Ghost {
            generation: 1,
            content: Some("int gamma = 3;".into()),
            prefix,
            suffix,
            line_suffix: String::new(),
            anchor: (2, 4),
            max_lines: 6,
        });
        cx.notify();
    });
    cx.run_until_parked();

    // ⌥→ takes "int"; the rest stays ghosted without a new request.
    cx.simulate_keystrokes("alt-right");
    app.update_in(cx, |app, _w, _cx| {
        let tab = app.editor.active_tab().unwrap();
        assert_eq!(tab.line(2), "    int", "first word inserted");
        assert_eq!(
            app.ghost.as_ref().map(|g| g.text.as_str()),
            Some(" gamma = 3;"),
            "remainder re-served from the typed-through cache"
        );
    });

    // Again: the leading space rides with the next word.
    cx.simulate_keystrokes("alt-right");
    app.update_in(cx, |app, _w, _cx| {
        let tab = app.editor.active_tab().unwrap();
        assert_eq!(tab.line(2), "    int gamma", "second word inserted");
        assert_eq!(
            app.ghost.as_ref().map(|g| g.text.as_str()),
            Some(" = 3;"),
            "remainder still ghosted"
        );
    });

    // Tab lands the rest.
    cx.simulate_keystrokes("tab");
    app.update_in(cx, |app, _w, _cx| {
        let tab = app.editor.active_tab().unwrap();
        assert_eq!(tab.line(2), "    int gamma = 3;", "Tab accepts the tail");
        assert!(app.ghost.is_none());
    });
    let _ = std::fs::remove_dir_all(&dir);
}

/// ⌘⇧C hands the active file off to CLion (status feedback; the launcher spawn
/// queues on the never-driven test runtime), while plain ⌘C stays copy — the
/// two share the "c" chord arm and must not shadow each other.
#[gpui::test]
async fn cmd_shift_c_opens_in_clion_plain_cmd_c_copies(cx: &mut TestAppContext) {
    let (dir, file) = test_workspace();
    let (deps, app_rx) = test_deps(dir.clone());
    let (app, cx) = cx.add_window_view(|_window, cx| JadeApp::new(cx, deps, app_rx));
    app.update_in(cx, |app, _w, cx| {
        app.open_file(file.clone());
        cx.notify();
    });
    cx.run_until_parked();

    // Select the word under the caret so ⌘C has something to copy.
    cx.simulate_keystrokes("down");
    cx.simulate_keystrokes("cmd-shift-c");
    cx.run_until_parked();
    app.update_in(cx, |app, _w, _cx| {
        let last = app.output.last().map(|s| s.as_str()).unwrap_or("");
        assert!(
            last.contains("Opening in CLion") && last.contains("main.cpp:2"),
            "⌘⇧C reports the hand-off target, got {last:?}"
        );
    });

    // ⌘A then ⌘C: still the plain copy path (no CLion status spam).
    cx.simulate_keystrokes("cmd-a");
    cx.simulate_keystrokes("cmd-c");
    cx.run_until_parked();
    app.update_in(cx, |app, _w, _cx| {
        let clion_lines = app
            .output
            .iter()
            .filter(|l| l.contains("Opening in CLion"))
            .count();
        assert_eq!(clion_lines, 1, "plain ⌘C must not trigger the hand-off");
    });
    let _ = std::fs::remove_dir_all(&dir);
}

/// Regression (2026-07-18): a metalLLM run whose loss went non-finite fed NaN
/// samples into the training chart; `chart_canvas` passed them straight to
/// lyon's `line_to`, whose NaN assert aborts the whole process. Rendering the
/// runtime sidebar with NaN/inf samples in an otherwise-finite series must
/// paint the finite points and survive.
#[gpui::test]
async fn non_finite_scalars_render_without_crashing(cx: &mut TestAppContext) {
    use jade_telemetry::Kind;

    let (dir, _file) = test_workspace();
    let (deps, app_rx) = test_deps(dir.clone());

    let (app, cx) = cx.add_window_view(|_window, cx| JadeApp::new(cx, deps, app_rx));
    app.update_in(cx, |app, _window, cx| {
        app.runtime_visible = true;
        // NaN mixed into finite samples leaves scalar_stats finite (NaN loses
        // every comparison), so no range check upstream can catch this — the
        // exact shape that crashed. (An inf sample would poison the stored max
        // and get the whole series skipped upstream, never reaching the chart.)
        for (step, v) in [(0, 1.0), (1, f64::NAN), (2, 0.5), (3, 0.25)] {
            app.registry.note_scalar("loss", step, v, &app.prefs.clone());
            app.training.push_scalar("loss", step, v);
        }
        app.registry.set_enabled(Kind::Scalar, "loss", true);
        cx.notify();
    });
    // The paint pass runs here; before the fix this aborted with SIGABRT.
    cx.run_until_parked();

    let _ = std::fs::remove_dir_all(&dir);
}

/// Cross-file sync (§4.13), end to end: (1) a kernel rename in a .metal file
/// updates the host's string literal on disk; (2) a hyperparameter value
/// change propagates to the shader's declaration; (3) a one-token line edit
/// applies to the other occurrences in the same file.
#[gpui::test]
async fn sync_suggestions_detect_and_apply(cx: &mut TestAppContext) {
    let (dir, _file) = test_workspace();
    let host = dir.join("host.cpp");
    std::fs::write(
        &host,
        "#define N_EMBED_CFG 384\n\
         void build() {\n\
             p1 = makePipeline(library, \"embedForward\");\n\
             p2 = makePipeline(library, \"embedBackward\");\n\
         }\n",
    )
    .unwrap();
    let shader = dir.join("embed.metal");
    std::fs::write(
        &shader,
        "#define N_EMBED_CFG 384\n\
         kernel void embedForward(device float* x) {}\n\
         kernel void embedBackward(device float* x) {}\n",
    )
    .unwrap();
    let (deps, app_rx) = test_deps(dir.clone());
    let (app, cx) = cx.add_window_view(|_window, cx| JadeApp::new(cx, deps, app_rx));

    // 1. Kernel rename: embedForward → embedFwd in the shader.
    app.update_in(cx, |app, _window, cx| {
        app.open_file(shader.clone());
        let idx = app.editor.active.unwrap();
        let tab = app.editor.active_tab_mut().unwrap();
        let text = tab.buffer.to_string();
        let at = text.find("embedForward").unwrap();
        tab.buffer.edit(at..at + "embedForward".len(), "embedFwd");
        // A direct buffer edit skips after_edit; promote the preview tab as it would.
        tab.preview = false;
        assert!(app.sync_detect(idx), "kernel rename must produce a suggestion");
        match app.sync_suggestion.as_ref() {
            Some(crate::sync::SyncSuggestion::RenameKernel { old, new, refs, files }) => {
                assert_eq!((old.as_str(), new.as_str()), ("embedForward", "embedFwd"));
                assert_eq!((*refs, *files), (1, 1));
            }
            other => panic!("expected RenameKernel, got {other:?}"),
        }
        app.sync_apply(cx);
    });
    let host_text = std::fs::read_to_string(&host).unwrap();
    assert!(host_text.contains("\"embedFwd\""), "host literal updated on disk");
    assert!(!host_text.contains("\"embedForward\""), "old literal gone");
    assert!(host_text.contains("\"embedBackward\""), "other kernel untouched");

    // 2. Hyperparameter: 384 → 512 in the host propagates to the shader.
    app.update_in(cx, |app, _window, cx| {
        app.open_file(host.clone());
        let idx = app.editor.active.unwrap();
        let tab = app.editor.active_tab_mut().unwrap();
        let text = tab.buffer.to_string();
        let at = text.find("384").unwrap();
        tab.buffer.edit(at..at + 3, "512");
        // A direct buffer edit skips after_edit; promote the preview tab as it would.
        tab.preview = false;
        assert!(app.sync_detect(idx), "value change must produce a suggestion");
        match app.sync_suggestion.as_ref() {
            Some(crate::sync::SyncSuggestion::Hyperparam { name, to, sites, files }) => {
                assert_eq!(name, "N_EMBED_CFG");
                assert_eq!(to, "512");
                assert_eq!((*sites, *files), (1, 1));
            }
            other => panic!("expected Hyperparam, got {other:?}"),
        }
        app.sync_apply(cx);
    });
    // The shader is open in a tab, so the propagation lands in its buffer.
    app.update_in(cx, |app, _window, _cx| {
        let tab = app.editor.tabs.iter().find(|t| t.path == shader).unwrap();
        assert!(
            tab.buffer.to_string().contains("N_EMBED_CFG 512"),
            "shader declaration updated in its open buffer"
        );
        assert!(tab.buffer.is_dirty(), "shader tab left dirty for review");
    });

    // 3. Similar lines: p.M → p.T on one line offers the other occurrences.
    let tensors = dir.join("tensors.cpp");
    std::fs::write(
        &tensors,
        "void alloc() {\n\
             Tensor q({p.M, p.K});\n\
             Tensor k({p.M, p.K});\n\
             Tensor v({p.M, p.N});\n\
         }\n",
    )
    .unwrap();
    app.update_in(cx, |app, _window, cx| {
        app.open_file(tensors.clone());
        let idx = app.editor.active.unwrap();
        let tab = app.editor.active_tab_mut().unwrap();
        let text = tab.buffer.to_string();
        let at = text.find("p.M").unwrap();
        tab.buffer.edit(at..at + 3, "p.T");
        assert!(app.sync_detect(idx), "token edit must produce a suggestion");
        match app.sync_suggestion.as_ref() {
            Some(crate::sync::SyncSuggestion::SimilarLines { from, to, count }) => {
                assert_eq!((from.as_str(), to.as_str()), ("p.M", "p.T"));
                assert_eq!(*count, 2, "two other occurrences remain");
            }
            other => panic!("expected SimilarLines, got {other:?}"),
        }
        app.sync_apply(cx);
        let after = app.editor.active_tab().unwrap().buffer.to_string();
        assert!(!after.contains("p.M"), "all occurrences replaced");
        assert_eq!(after.matches("p.T").count(), 3);
        // One undo restores the batch, leaving only the user's own edit.
        app.editor.active_tab_mut().unwrap().buffer.undo();
        let undone = app.editor.active_tab().unwrap().buffer.to_string();
        assert_eq!(undone.matches("p.T").count(), 1, "batch is one undo group");
        cx.notify();
    });

    let _ = std::fs::remove_dir_all(&dir);
}

/// Cross-file sync (§4.13), incremental rename: the detector must chain across
/// debounce windows (the host references the ORIGINAL name, not the last
/// snapshot's intermediate), and the apply must also rename derived host
/// identifiers such as `residualAddPipeline`.
#[gpui::test]
async fn sync_rename_chains_and_renames_derived_idents(cx: &mut TestAppContext) {
    let (dir, _file) = test_workspace();
    let host = dir.join("model.h");
    std::fs::write(
        &host,
        "struct Model {\n\
             MTL::ComputePipelineState* residualAddPipeline;\n\
             void build() {\n\
                 residualAddPipeline = makePipeline(library, \"residualAdd\");\n\
             }\n\
         };\n",
    )
    .unwrap();
    let shader = dir.join("elementwise.metal");
    std::fs::write(&shader, "kernel void residualAdd(device float* x) {}\n").unwrap();
    let (deps, app_rx) = test_deps(dir.clone());
    let (app, cx) = cx.add_window_view(|_window, cx| JadeApp::new(cx, deps, app_rx));

    app.update_in(cx, |app, _window, cx| {
        app.open_file(shader.clone());
        let idx = app.editor.active.unwrap();

        // First debounce window: residualAdd → residualAddV.
        let tab = app.editor.active_tab_mut().unwrap();
        let text = tab.buffer.to_string();
        let at = text.find("residualAdd(").unwrap();
        tab.buffer.edit(at + "residualAdd".len()..at + "residualAdd".len(), "V");
        assert!(app.sync_detect(idx), "first step must suggest");

        // Second window: residualAddV → residualAddV2. The host has no
        // "residualAddV" reference — the suggestion must chain to the
        // original name instead of going stale.
        let tab = app.editor.active_tab_mut().unwrap();
        let text = tab.buffer.to_string();
        let at = text.find("residualAddV(").unwrap();
        tab.buffer
            .edit(at + "residualAddV".len()..at + "residualAddV".len(), "2");
        assert!(app.sync_detect(idx), "second step must re-suggest");
        match app.sync_suggestion.as_ref() {
            Some(crate::sync::SyncSuggestion::RenameKernel { old, new, refs, files }) => {
                assert_eq!(old, "residualAdd", "anchored on the original name");
                assert_eq!(new, "residualAddV2");
                // The string literal + two residualAddPipeline identifiers.
                assert_eq!((*refs, *files), (3, 1));
            }
            other => panic!("expected chained RenameKernel, got {other:?}"),
        }
        app.sync_apply(cx);
    });
    let host_text = std::fs::read_to_string(&host).unwrap();
    assert!(host_text.contains("\"residualAddV2\""), "literal renamed");
    assert_eq!(
        host_text.matches("residualAddV2Pipeline").count(),
        2,
        "derived identifiers renamed at both sites"
    );
    assert!(!host_text.contains("\"residualAdd\""), "old literal gone");

    // A revert within the next window clears the pending suggestion.
    app.update_in(cx, |app, _window, cx| {
        let idx = app.editor.active.unwrap();
        let tab = app.editor.active_tab_mut().unwrap();
        let text = tab.buffer.to_string();
        let at = text.find("residualAddV2").unwrap();
        tab.buffer.edit(at..at + "residualAddV2".len(), "residualAddV3");
        assert!(app.sync_detect(idx), "rename after apply suggests again");
        let tab = app.editor.active_tab_mut().unwrap();
        let text = tab.buffer.to_string();
        let at = text.find("residualAddV3").unwrap();
        tab.buffer.edit(at..at + "residualAddV3".len(), "residualAddV2");
        assert!(app.sync_detect(idx), "revert must clear the banner");
        assert!(app.sync_suggestion.is_none(), "no suggestion after revert");
        cx.notify();
    });

    let _ = std::fs::remove_dir_all(&dir);
}

/// Cross-file sync scope (§4.13): with a directory selected in the file tree,
/// the rename scan covers ONLY that directory. References outside it are
/// neither counted nor rewritten. With no directory selected, the scan covers
/// the whole workspace.
#[gpui::test]
async fn sync_scope_follows_tree_selection(cx: &mut TestAppContext) {
    let (dir, _file) = test_workspace();
    let level = dir.join("level_1");
    std::fs::create_dir_all(&level).unwrap();
    let host_in = level.join("host.cpp");
    std::fs::write(
        &host_in,
        "p = makePipeline(library, \"residualAdd\"); residualAddPipeline = p;\n",
    )
    .unwrap();
    let host_out = dir.join("other.cpp");
    std::fs::write(&host_out, "q = makePipeline(library, \"residualAdd\");\n").unwrap();
    let shader = level.join("elementwise.metal");
    std::fs::write(&shader, "kernel void residualAdd(device float* x) {}\n").unwrap();
    let (deps, app_rx) = test_deps(dir.clone());
    let (app, cx) = cx.add_window_view(|_window, cx| JadeApp::new(cx, deps, app_rx));

    app.update_in(cx, |app, _window, cx| {
        // Select the level directory in the tree, like before opening a
        // terminal there.
        app.select_tree_path(level.clone());
        app.open_file(shader.clone());
        let idx = app.editor.active.unwrap();
        let tab = app.editor.active_tab_mut().unwrap();
        let text = tab.buffer.to_string();
        let at = text.find("residualAdd").unwrap();
        tab.buffer.edit(at..at + "residualAdd".len(), "residualAddTiled");
        assert!(app.sync_detect(idx));
        match app.sync_suggestion.as_ref() {
            Some(crate::sync::SyncSuggestion::RenameKernel { refs, files, .. }) => {
                // Literal + derived identifier in level_1/host.cpp only —
                // other.cpp is outside the selected directory.
                assert_eq!((*refs, *files), (2, 1), "scan scoped to the selection");
            }
            other => panic!("expected RenameKernel, got {other:?}"),
        }
        assert!(app.sync_scope_is_narrow(), "banner shows the scope");
        app.sync_apply(cx);
    });
    let in_text = std::fs::read_to_string(&host_in).unwrap();
    assert!(in_text.contains("\"residualAddTiled\""));
    assert!(in_text.contains("residualAddTiledPipeline"));
    let out_text = std::fs::read_to_string(&host_out).unwrap();
    assert!(
        out_text.contains("\"residualAdd\""),
        "file outside the selected directory untouched"
    );

    // Clearing the selection widens the scope back to the workspace root.
    app.update_in(cx, |app, _window, cx| {
        app.tree_selection = None;
        let idx = app.editor.active.unwrap();
        let tab = app.editor.active_tab_mut().unwrap();
        let text = tab.buffer.to_string();
        let at = text.find("residualAddTiled").unwrap();
        tab.buffer
            .edit(at..at + "residualAddTiled".len(), "residualAddWide");
        assert!(app.sync_detect(idx));
        match app.sync_suggestion.as_ref() {
            Some(crate::sync::SyncSuggestion::RenameKernel { old, refs, files, .. }) => {
                // level_1/host.cpp now holds residualAddTiled (2 refs); the
                // untouched other.cpp still holds residualAdd, which no longer
                // matches — so the workspace-wide scan finds the 2 in-level refs.
                assert_eq!(old, "residualAddTiled");
                assert_eq!((*refs, *files), (2, 1));
            }
            other => panic!("expected RenameKernel, got {other:?}"),
        }
        assert!(!app.sync_scope_is_narrow(), "workspace-wide scope");
        cx.notify();
    });

    let _ = std::fs::remove_dir_all(&dir);
}

/// A tab-indented file: the caret bar must land on the glyph the caret column
/// points at, and a click on that glyph must come back as the same column.
///
/// The regression: the renderer handed CoreText a raw `\t`, which lays it out on
/// its own 28px stops, while the caret drew at `char column × 7.83px`. Every
/// tab-indented line drew the caret away from its text. Tabs now expand to
/// spaces at 4-column stops, and x ↔ column mapping goes through the display
/// column.
#[gpui::test]
async fn tab_indented_line_places_the_caret_on_the_glyph(cx: &mut TestAppContext) {
    let dir = std::env::temp_dir().join(format!("jade-tabs-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let file = dir.join("tabs.cpp");
    // Row 1 is "\tint alpha = 1;" — it draws as "    int alpha = 1;", so char
    // column 4 ('t' of "int") sits at display column 7.
    std::fs::write(&file, "int main() {\n\tint alpha = 1;\n}\n").unwrap();
    let (deps, app_rx) = test_deps(dir.clone());

    let (app, cx) = cx.add_window_view(|_window, cx| JadeApp::new(cx, deps, app_rx));
    app.update_in(cx, |app, _window, cx| {
        app.open_file(file.clone());
        cx.notify();
    });
    cx.run_until_parked();

    let cw = app.update_in(cx, |app, _w, _cx| app.char_w());
    let cell = cx
        .debug_bounds("code-cell-1")
        .expect("code cell for row 1 was painted");
    // Click at display column 7 → char column 4.
    let x = cell.origin.x + px(7.0 * cw);
    let y = cell.origin.y + px(crate::panels::code_view::LINE_H / 2.0);
    cx.simulate_click(point(x, y), Modifiers::default());
    app.update_in(cx, |app, _window, _cx| {
        let caret = app.editor.active_tab().unwrap().caret_point();
        assert_eq!((caret.row, caret.col), (1, 4), "click on a tab-indented row");
    });
    cx.run_until_parked();

    // The painted caret bar sits at the display column, not the char column
    // (which would draw it 3 columns to the left, inside the indent). Both
    // bounds come from the same post-click frame.
    let cell = cx.debug_bounds("code-cell-1").expect("code cell repainted");
    let caret = cx
        .debug_bounds("editor-caret")
        .expect("caret bar was painted");
    let offset = f32::from(caret.origin.x - cell.origin.x);
    assert!(
        (offset - 7.0 * cw).abs() < 1.0,
        "caret bar at display column 7 ({} px), got {offset} px (the char column \
         would be {} px)",
        7.0 * cw,
        4.0 * cw
    );

    let _ = std::fs::remove_dir_all(&dir);
}


// ── Explain card (§4.14) ─────────────────────────────────────────────────────


/// Give the app's chat backend a credential so `explain_trigger` takes the
/// real request path instead of parking the card on "no model available".
///
/// The request itself never reaches the network: `test_deps` builds a
/// current-thread runtime that is never driven, so the spawned task queues and
/// is never polled. The test then injects the deltas it wants to assert on.
fn stub_chat_credential(app: &JadeApp) {
    app.chat
        .set_credential_for_test(Some(jade_ai::ApiKey::new("sk-ant-test-not-a-real-key")));
}

/// Select a range in the open buffer by byte offsets.
fn select_bytes(app: &mut JadeApp, range: std::ops::Range<usize>) {
    let tab = app.editor.active_tab_mut().expect("a tab is open");
    tab.buffer
        .set_selection(jade_buffer::Selection::new(range.start, range.end));
}

/// ⌘⇧E over a selection opens a card, and it is really painted.
#[gpui::test]
async fn explain_opens_a_card_over_the_selection(cx: &mut TestAppContext) {
    let (dir, file) = test_workspace();
    let (deps, app_rx) = test_deps(dir);
    let (app, cx) = cx.add_window_view(|_window, cx| JadeApp::new(cx, deps, app_rx));
    app.update_in(cx, |app, _w, cx| {
        app.open_file(file);
        cx.notify();
    });
    cx.run_until_parked();

    // "    int alpha = 1;" is row 1: bytes 13..31 of the fixture.
    app.update_in(cx, |app, _w, cx| {
        select_bytes(app, 17..31);
        cx.notify();
    });
    cx.simulate_keystrokes("cmd-shift-e");
    cx.run_until_parked();

    app.update_in(cx, |app, _w, _cx| {
        let c = app.explain.as_ref().expect("a card was opened");
        assert_eq!(c.start_row, 1, "anchored on the selected row");
        assert_eq!(c.language, "cpp");
        assert_eq!(c.generation, 1);
    });
    assert!(
        cx.debug_bounds("explain-card").is_some(),
        "the card must actually paint, not just exist in state"
    );
}

/// With nothing selected there is nothing to explain, and no request is spent.
#[gpui::test]
async fn explain_with_no_selection_opens_nothing(cx: &mut TestAppContext) {
    let (dir, file) = test_workspace();
    let (deps, app_rx) = test_deps(dir);
    let (app, cx) = cx.add_window_view(|_window, cx| JadeApp::new(cx, deps, app_rx));
    app.update_in(cx, |app, _w, cx| {
        app.open_file(file);
        cx.notify();
    });
    cx.run_until_parked();

    cx.simulate_keystrokes("cmd-shift-e");
    cx.run_until_parked();
    app.update_in(cx, |app, _w, _cx| {
        assert!(app.explain.is_none(), "no selection → no card");
        assert!(!app.toasts.is_empty(), "the user is told why");
    });
    assert!(cx.debug_bounds("explain-card").is_none());
}

/// Streamed deltas accumulate into the card's prose.
#[gpui::test]
async fn explain_streams_prose_into_the_card(cx: &mut TestAppContext) {
    use crate::app::AppEvent;
    let (dir, file) = test_workspace();
    let (deps, app_rx) = test_deps(dir);
    let (app, cx) = cx.add_window_view(|_window, cx| JadeApp::new(cx, deps, app_rx));
    app.update_in(cx, |app, _w, cx| {
        app.open_file(file);
        stub_chat_credential(app);
        select_bytes(app, 17..31);
        app.explain_trigger(cx);
    });
    cx.run_until_parked();

    app.update_in(cx, |app, _w, cx| {
        app.apply_app_event(AppEvent::Explain {
            generation: 1,
            delta: jade_ai::ChatDelta::Started(jade_ai::ChatProviderId::Anthropic),
        });
        app.apply_app_event(AppEvent::Explain {
            generation: 1,
            delta: jade_ai::ChatDelta::Text("It declares ".into()),
        });
        app.apply_app_event(AppEvent::Explain {
            generation: 1,
            delta: jade_ai::ChatDelta::Text("alpha.".into()),
        });
        cx.notify();
        let c = app.explain.as_ref().unwrap();
        assert_eq!(c.prose, "It declares alpha.");
        assert_eq!(c.status().0, "Explaining…");
    });
}

/// A superseded request's deltas must never land in the new card. Aborting the
/// task is not enough — deltas can already be queued on the pump.
#[gpui::test]
async fn explain_drops_deltas_from_a_superseded_request(cx: &mut TestAppContext) {
    use crate::app::AppEvent;
    let (dir, file) = test_workspace();
    let (deps, app_rx) = test_deps(dir);
    let (app, cx) = cx.add_window_view(|_window, cx| JadeApp::new(cx, deps, app_rx));
    app.update_in(cx, |app, _w, cx| {
        app.open_file(file);
        stub_chat_credential(app);
        select_bytes(app, 17..31);
        app.explain_trigger(cx);
        // A different selection supersedes.
        select_bytes(app, 36..49);
        app.explain_trigger(cx);
    });
    cx.run_until_parked();

    app.update_in(cx, |app, _w, _cx| {
        assert_eq!(app.explain.as_ref().unwrap().generation, 2);
        app.apply_app_event(AppEvent::Explain {
            generation: 1,
            delta: jade_ai::ChatDelta::Text("STALE".into()),
        });
        app.apply_app_event(AppEvent::Explain {
            generation: 2,
            delta: jade_ai::ChatDelta::Text("fresh".into()),
        });
        assert_eq!(app.explain.as_ref().unwrap().prose, "fresh");
    });
}

/// Re-triggering on the SAME selection must not spend a second request.
#[gpui::test]
async fn explain_on_the_same_selection_is_idempotent(cx: &mut TestAppContext) {
    let (dir, file) = test_workspace();
    let (deps, app_rx) = test_deps(dir);
    let (app, cx) = cx.add_window_view(|_window, cx| JadeApp::new(cx, deps, app_rx));
    app.update_in(cx, |app, _w, cx| {
        app.open_file(file);
        select_bytes(app, 17..31);
        app.explain_trigger(cx);
        let gen = app.explain.as_ref().unwrap().generation;
        app.explain_trigger(cx);
        assert_eq!(
            app.explain.as_ref().unwrap().generation,
            gen,
            "holding the chord must not re-request"
        );
    });
}

/// Esc closes the card — but only after any completion popup has had its turn.
#[gpui::test]
async fn escape_closes_the_card(cx: &mut TestAppContext) {
    let (dir, file) = test_workspace();
    let (deps, app_rx) = test_deps(dir);
    let (app, cx) = cx.add_window_view(|_window, cx| JadeApp::new(cx, deps, app_rx));
    app.update_in(cx, |app, _w, cx| {
        app.open_file(file);
        select_bytes(app, 17..31);
        app.explain_trigger(cx);
    });
    cx.run_until_parked();
    assert!(cx.debug_bounds("explain-card").is_some());

    cx.simulate_keystrokes("escape");
    cx.run_until_parked();
    app.update_in(cx, |app, _w, _cx| {
        assert!(app.explain.is_none(), "escape closes the card");
    });
    assert!(cx.debug_bounds("explain-card").is_none());
}

/// The existing ⌘⇧A and ⌘C bindings must survive the new arms.
#[gpui::test]
async fn explain_binding_does_not_shadow_the_existing_chords(cx: &mut TestAppContext) {
    let (dir, file) = test_workspace();
    let (deps, app_rx) = test_deps(dir);
    let (app, cx) = cx.add_window_view(|_window, cx| JadeApp::new(cx, deps, app_rx));
    app.update_in(cx, |app, _w, cx| {
        app.open_file(file);
        cx.notify();
    });
    cx.run_until_parked();

    let before = app.update_in(cx, |app, _w, _cx| app.asm_visible);
    cx.simulate_keystrokes("cmd-shift-a");
    cx.run_until_parked();
    app.update_in(cx, |app, _w, _cx| {
        assert_ne!(app.asm_visible, before, "cmd-shift-a still toggles ASM");
        assert!(app.explain.is_none(), "cmd-shift-a must not explain");
    });

    // ⌘E alone is the flow toggle, not Explain.
    cx.simulate_keystrokes("cmd-e");
    cx.run_until_parked();
    app.update_in(cx, |app, _w, _cx| {
        assert!(app.explain.is_none(), "cmd-e must not explain");
    });
}

/// An edit under the card marks it stale rather than silently letting it
/// describe code that is no longer there.
#[gpui::test]
async fn editing_under_the_card_marks_it_stale(cx: &mut TestAppContext) {
    let (dir, file) = test_workspace();
    let (deps, app_rx) = test_deps(dir);
    let (app, cx) = cx.add_window_view(|_window, cx| JadeApp::new(cx, deps, app_rx));
    app.update_in(cx, |app, _w, cx| {
        app.open_file(file);
        select_bytes(app, 17..31);
        app.explain_trigger(cx);
        assert!(!app.explain.as_ref().unwrap().stale);
    });
    cx.run_until_parked();

    // Type inside the explained fragment.
    app.update_in(cx, |app, _w, cx| {
        select_bytes(app, 20..20);
        cx.notify();
    });
    cx.simulate_keystrokes("x");
    cx.run_until_parked();
    app.update_in(cx, |app, _w, _cx| {
        assert!(
            app.explain.as_ref().unwrap().stale,
            "an edit inside the fragment must mark the card stale"
        );
    });
}

/// With no API key, Explain must not dead-end: it brings the local model server
/// up and parks the card on a transient, retryable "starting" state.
#[gpui::test]
async fn explain_without_a_key_falls_back_to_the_local_model(cx: &mut TestAppContext) {
    let (dir, file) = test_workspace();
    let (deps, app_rx) = test_deps(dir);
    let (app, cx) = cx.add_window_view(|_window, cx| JadeApp::new(cx, deps, app_rx));
    app.update_in(cx, |app, _w, cx| {
        app.open_file(file);
        // No key, and no local server yet — the state a fresh machine is in.
        app.chat.set_credential_for_test(None);
        app.chat.set_local_status(jade_ai::LocalStatus::Off);
        select_bytes(app, 17..31);
        app.explain_trigger(cx);
    });
    cx.run_until_parked();

    app.update_in(cx, |app, _w, _cx| {
        let c = app.explain.as_ref().expect("a card still opens");
        assert_eq!(
            c.failure(),
            Some(&jade_ai::ChatError::LocalStarting),
            "no key must start the local model, not give up"
        );
        assert!(c.can_retry(), "a starting server is a wait, not a dead end");
    });
    assert!(cx.debug_bounds("explain-card").is_some());
}

/// Only a machine with neither a key nor any local model says so.
#[gpui::test]
async fn explain_reports_setup_only_when_nothing_can_serve(cx: &mut TestAppContext) {
    let (dir, file) = test_workspace();
    let (deps, app_rx) = test_deps(dir);
    let (app, cx) = cx.add_window_view(|_window, cx| JadeApp::new(cx, deps, app_rx));
    app.update_in(cx, |app, _w, _cx| {
        app.open_file(file);
        app.chat.set_credential_for_test(None);
        app.chat.set_local_status(jade_ai::LocalStatus::Off);
        // `effective_provider` is what the card's empty state reads.
        assert_eq!(
            app.chat.effective_provider(),
            Err(jade_ai::ChatError::NoCredential)
        );
        app.chat
            .set_local_status(jade_ai::LocalStatus::Ready { endpoint: "http://127.0.0.1:8630".into() });
        assert_eq!(
            app.chat.effective_provider(),
            Ok(jade_ai::ChatProviderId::LlamaServer),
            "a running local server is enough on its own"
        );
    });
}

/// A card parked on "starting" resumes by itself once the server is up.
#[gpui::test]
async fn a_parked_card_resumes_when_the_local_model_is_ready(cx: &mut TestAppContext) {
    use crate::app::AppEvent;
    let (dir, file) = test_workspace();
    let (deps, app_rx) = test_deps(dir);
    let (app, cx) = cx.add_window_view(|_window, cx| JadeApp::new(cx, deps, app_rx));
    app.update_in(cx, |app, _w, cx| {
        app.open_file(file);
        app.chat.set_credential_for_test(None);
        app.chat.set_local_status(jade_ai::LocalStatus::Off);
        select_bytes(app, 17..31);
        app.explain_trigger(cx);
        assert_eq!(
            app.explain.as_ref().unwrap().failure(),
            Some(&jade_ai::ChatError::LocalStarting)
        );
    });
    cx.run_until_parked();

    // The server comes up.
    app.update_in(cx, |app, _w, cx| {
        app.apply_app_event(AppEvent::Ai(jade_ai::AiStatus {
            state: jade_ai::AiState::Ready,
            detail: "ready".into(),
            endpoint: Some("http://127.0.0.1:8630".into()),
        }));
        app.after_events(cx);
        let c = app.explain.as_ref().expect("the card is still open");
        assert!(
            c.failure().is_none(),
            "the parked card must re-issue itself, not wait for a manual retry"
        );
        assert!(c.generation > 1, "it was actually re-requested");
    });
}

// ── Visualize card (§4.15) ───────────────────────────────────────────────────

/// Point the app's AI prefs at a throwaway file, with Visualize consent in a
/// known state. The default `AiPrefs::load()` reads the developer's real
/// `~/.config/jade/ai.json`, and a consent test must neither read nor write it.
fn stub_ai_prefs(app: &mut JadeApp, dir: &std::path::Path, visualize_enabled: bool) {
    let path = dir.join(".jade").join("ai-test.json");
    let _ = std::fs::create_dir_all(path.parent().unwrap());
    let mut prefs = crate::ai_prefs::AiPrefs::load_from(&path);
    prefs.visualize_enabled = visualize_enabled;
    app.ai_prefs = prefs;
}

/// The first ⌘⇧M shows the consent card: no request goes out and no generated
/// Python can run until the user opts in.
#[gpui::test]
async fn visualize_asks_for_consent_first(cx: &mut TestAppContext) {
    let (dir, file) = test_workspace();
    let (deps, app_rx) = test_deps(dir.clone());
    let (app, cx) = cx.add_window_view(|_window, cx| JadeApp::new(cx, deps, app_rx));
    app.update_in(cx, |app, _w, cx| {
        app.open_file(file);
        stub_ai_prefs(app, &dir, false);
        stub_chat_credential(app);
        select_bytes(app, 17..31);
        cx.notify();
    });
    cx.run_until_parked();

    cx.simulate_keystrokes("cmd-shift-m");
    cx.run_until_parked();

    app.update_in(cx, |app, _w, _cx| {
        let c = app.visualize.as_ref().expect("a consent card was opened");
        assert!(c.awaiting_consent(), "phase: {:?}", c.phase);
        assert!(!c.in_flight(), "consent must not spend a request");
    });
    assert!(
        cx.debug_bounds("visualize-card").is_some(),
        "the consent card must actually paint"
    );
}

/// Accepting consent persists the choice (to the test path) and starts the
/// request the card was opened for.
#[gpui::test]
async fn visualize_consent_accept_starts_the_request(cx: &mut TestAppContext) {
    let (dir, file) = test_workspace();
    let (deps, app_rx) = test_deps(dir.clone());
    let (app, cx) = cx.add_window_view(|_window, cx| JadeApp::new(cx, deps, app_rx));
    app.update_in(cx, |app, _w, cx| {
        app.open_file(file);
        stub_ai_prefs(app, &dir, false);
        stub_chat_credential(app);
        select_bytes(app, 17..31);
        app.visualize_trigger(cx);
    });
    cx.run_until_parked();

    app.update_in(cx, |app, _w, cx| {
        assert!(app.visualize.as_ref().unwrap().awaiting_consent());
        app.visualize_consent_accept(cx);
        assert!(app.ai_prefs.visualize_enabled);
        let c = app.visualize.as_ref().expect("the card survives consent");
        assert!(!c.awaiting_consent(), "the request started: {:?}", c.phase);
        assert!(c.generation > 1, "a fresh generation for the real request");
    });
    // The choice landed in the throwaway prefs file, not the real one.
    let saved = std::fs::read_to_string(dir.join(".jade").join("ai-test.json")).unwrap();
    assert!(saved.contains("\"visualize_enabled\": true"), "{saved}");
}

/// With consent already given, ⌘⇧M goes straight to the request and paints
/// the working card.
#[gpui::test]
async fn visualize_opens_a_card_when_enabled(cx: &mut TestAppContext) {
    let (dir, file) = test_workspace();
    let (deps, app_rx) = test_deps(dir.clone());
    let (app, cx) = cx.add_window_view(|_window, cx| JadeApp::new(cx, deps, app_rx));
    app.update_in(cx, |app, _w, cx| {
        app.open_file(file);
        stub_ai_prefs(app, &dir, true);
        stub_chat_credential(app);
        select_bytes(app, 17..31);
        cx.notify();
    });
    cx.run_until_parked();

    cx.simulate_keystrokes("cmd-shift-m");
    cx.run_until_parked();

    app.update_in(cx, |app, _w, _cx| {
        let c = app.visualize.as_ref().expect("a card was opened");
        assert_eq!(c.start_row, 1, "anchored on the selected row");
        assert_eq!(c.language, "cpp");
        assert!(!c.awaiting_consent());
        // Explain is untouched — the lanes are independent.
        assert!(app.explain.is_none());
    });
    assert!(cx.debug_bounds("visualize-card").is_some());
}

/// The local gate: a selection with no executable code costs nothing — no
/// card, no request, a toast that says why.
#[gpui::test]
async fn visualize_gate_rejects_inappropriate_selections(cx: &mut TestAppContext) {
    let (dir, file) = test_workspace();
    let (deps, app_rx) = test_deps(dir.clone());
    let (app, cx) = cx.add_window_view(|_window, cx| JadeApp::new(cx, deps, app_rx));
    app.update_in(cx, |app, _w, cx| {
        app.open_file(file);
        stub_ai_prefs(app, &dir, true);
        stub_chat_credential(app);
        // Row 4 is the bare `}` — nothing to animate.
        select_bytes(app, 75..76);
        cx.notify();
    });
    cx.run_until_parked();

    cx.simulate_keystrokes("cmd-shift-m");
    cx.run_until_parked();

    app.update_in(cx, |app, _w, _cx| {
        assert!(app.visualize.is_none(), "gated selection → no card");
        assert!(!app.toasts.is_empty(), "the user is told why");
    });
    assert!(cx.debug_bounds("visualize-card").is_none());
}

/// The local model tier cannot write scenes; ⌘⇧M says so instead of failing
/// later with something confusing.
#[gpui::test]
async fn visualize_refuses_the_local_tier(cx: &mut TestAppContext) {
    let (dir, file) = test_workspace();
    let (deps, app_rx) = test_deps(dir.clone());
    let (app, cx) = cx.add_window_view(|_window, cx| JadeApp::new(cx, deps, app_rx));
    app.update_in(cx, |app, _w, cx| {
        app.open_file(file);
        stub_ai_prefs(app, &dir, true);
        app.chat.set_model(jade_ai::ChatModel::Local);
        select_bytes(app, 17..31);
        app.visualize_trigger(cx);
    });
    cx.run_until_parked();
    app.update_in(cx, |app, _w, _cx| {
        assert!(app.visualize.is_none());
        assert!(!app.toasts.is_empty());
    });
}

/// The full pipeline, request → verdict → render events → Ready, driven by
/// injected events exactly as the pump delivers them.
#[gpui::test]
async fn visualize_pipeline_reaches_ready(cx: &mut TestAppContext) {
    use crate::app::AppEvent;
    let (dir, file) = test_workspace();
    let (deps, app_rx) = test_deps(dir.clone());
    let (app, cx) = cx.add_window_view(|_window, cx| JadeApp::new(cx, deps, app_rx));
    app.update_in(cx, |app, _w, cx| {
        app.open_file(file);
        stub_ai_prefs(app, &dir, true);
        stub_chat_credential(app);
        select_bytes(app, 17..31);
        app.visualize_trigger(cx);
    });
    cx.run_until_parked();

    let plan = serde_json::json!({
        "suitable": true,
        "reason": "Shows the addition.",
        "title": "Adding alpha and beta",
        "script": "from manim import *\n\nclass JadeScene(Scene):\n    def construct(self):\n        self.play(Write(Text(\"a+b\")))\n        self.wait(1)\n",
        "duration_s": 7.0
    })
    .to_string();

    // A real mp4 for the Ready hand-off, so the player has a file to open.
    let video = dir.join("clip.mp4");
    std::fs::write(&video, b"not really an mp4; open() only needs a path").unwrap();

    app.update_in(cx, |app, _w, cx| {
        app.apply_app_event(AppEvent::Visualize {
            generation: 1,
            delta: jade_ai::ChatDelta::Text(plan),
        });
        app.apply_app_event(AppEvent::Visualize {
            generation: 1,
            delta: jade_ai::ChatDelta::Done { stop_reason: jade_ai::StopReason::EndTurn },
        });
        let c = app.visualize.as_ref().unwrap();
        assert_eq!(c.status().0, "Rendering…", "{:?}", c.phase);

        app.apply_app_event(AppEvent::VisualizeRender {
            generation: 1,
            ev: jade_build::manim::RenderEvent::Log("INFO Animation 0: Write(Text)".into()),
        });
        app.apply_app_event(AppEvent::VisualizeRender {
            generation: 1,
            ev: jade_build::manim::RenderEvent::Done { video: video.clone() },
        });
        cx.notify();
        let c = app.visualize.as_ref().unwrap();
        assert_eq!(c.status().0, "Ready", "{:?}", c.phase);
        assert!(app.visualize_player.is_some(), "a player was opened");
    });
    assert!(cx.debug_bounds("visualize-card").is_some());
}

/// The model-side gate: an unsuitable verdict ends benignly as "Skipped".
#[gpui::test]
async fn visualize_unsuitable_verdict_shows_skipped(cx: &mut TestAppContext) {
    use crate::app::AppEvent;
    let (dir, file) = test_workspace();
    let (deps, app_rx) = test_deps(dir.clone());
    let (app, cx) = cx.add_window_view(|_window, cx| JadeApp::new(cx, deps, app_rx));
    app.update_in(cx, |app, _w, cx| {
        app.open_file(file);
        stub_ai_prefs(app, &dir, true);
        stub_chat_credential(app);
        select_bytes(app, 17..31);
        app.visualize_trigger(cx);
    });
    cx.run_until_parked();

    let verdict = serde_json::json!({
        "suitable": false,
        "reason": "The line only declares a constant.",
        "title": "", "script": "", "duration_s": 0
    })
    .to_string();
    app.update_in(cx, |app, _w, cx| {
        app.apply_app_event(AppEvent::Visualize {
            generation: 1,
            delta: jade_ai::ChatDelta::Text(verdict),
        });
        app.apply_app_event(AppEvent::Visualize {
            generation: 1,
            delta: jade_ai::ChatDelta::Done { stop_reason: jade_ai::StopReason::EndTurn },
        });
        cx.notify();
        let c = app.visualize.as_ref().unwrap();
        assert_eq!(c.status(), ("Skipped", false));
        // No render task was started for a declined verdict — the phase
        // stayed terminal without touching manim.
    });
    assert!(cx.debug_bounds("visualize-card").is_some(), "the reason is shown");
}

/// Esc closes the card from the keyboard, and a queued stale delta cannot
/// revive it.
#[gpui::test]
async fn escape_closes_the_visualize_card(cx: &mut TestAppContext) {
    use crate::app::AppEvent;
    let (dir, file) = test_workspace();
    let (deps, app_rx) = test_deps(dir.clone());
    let (app, cx) = cx.add_window_view(|_window, cx| JadeApp::new(cx, deps, app_rx));
    app.update_in(cx, |app, _w, cx| {
        app.open_file(file);
        stub_ai_prefs(app, &dir, true);
        stub_chat_credential(app);
        select_bytes(app, 17..31);
        cx.notify();
    });
    cx.run_until_parked();
    cx.simulate_keystrokes("cmd-shift-m");
    cx.run_until_parked();
    app.update_in(cx, |app, _w, _cx| {
        assert!(app.visualize.is_some());
    });

    cx.simulate_keystrokes("escape");
    cx.run_until_parked();
    app.update_in(cx, |app, _w, _cx| {
        assert!(app.visualize.is_none(), "Esc closes the card");
        // A straggler from the canceled generation is dropped, not revived.
        app.apply_app_event(AppEvent::Visualize {
            generation: 1,
            delta: jade_ai::ChatDelta::Text("late".into()),
        });
        assert!(app.visualize.is_none());
    });
    assert!(cx.debug_bounds("visualize-card").is_none());
}

/// Explain and Visualize are independent: both cards can be open at once and
/// closing one leaves the other.
#[gpui::test]
async fn explain_and_visualize_are_independent(cx: &mut TestAppContext) {
    let (dir, file) = test_workspace();
    let (deps, app_rx) = test_deps(dir.clone());
    let (app, cx) = cx.add_window_view(|_window, cx| JadeApp::new(cx, deps, app_rx));
    app.update_in(cx, |app, _w, cx| {
        app.open_file(file);
        stub_ai_prefs(app, &dir, true);
        stub_chat_credential(app);
        select_bytes(app, 17..31);
        cx.notify();
    });
    cx.run_until_parked();

    cx.simulate_keystrokes("cmd-shift-e");
    cx.run_until_parked();
    cx.simulate_keystrokes("cmd-shift-m");
    cx.run_until_parked();

    app.update_in(cx, |app, _w, _cx| {
        assert!(app.explain.is_some(), "⌘⇧M must not cancel ⌘⇧E");
        assert!(app.visualize.is_some());
    });
    assert!(cx.debug_bounds("explain-card").is_some());
    assert!(cx.debug_bounds("visualize-card").is_some());

    // Esc closes Visualize first, Explain second.
    cx.simulate_keystrokes("escape");
    cx.run_until_parked();
    app.update_in(cx, |app, _w, _cx| {
        assert!(app.visualize.is_none());
        assert!(app.explain.is_some());
    });
    cx.simulate_keystrokes("escape");
    cx.run_until_parked();
    app.update_in(cx, |app, _w, _cx| {
        assert!(app.explain.is_none());
    });
}

/// Enter accepts consent from the keyboard — ⌘⇧M, Enter, done.
#[gpui::test]
async fn visualize_consent_enter_accepts(cx: &mut TestAppContext) {
    let (dir, file) = test_workspace();
    let (deps, app_rx) = test_deps(dir.clone());
    let (app, cx) = cx.add_window_view(|_window, cx| JadeApp::new(cx, deps, app_rx));
    app.update_in(cx, |app, _w, cx| {
        app.open_file(file);
        stub_ai_prefs(app, &dir, false);
        stub_chat_credential(app);
        select_bytes(app, 17..31);
        cx.notify();
    });
    cx.run_until_parked();
    cx.simulate_keystrokes("cmd-shift-m");
    cx.run_until_parked();
    cx.simulate_keystrokes("enter");
    cx.run_until_parked();
    app.update_in(cx, |app, _w, _cx| {
        assert!(app.ai_prefs.visualize_enabled, "Enter recorded consent");
        let c = app.visualize.as_ref().expect("the card went on to request");
        assert!(!c.awaiting_consent(), "{:?}", c.phase);
        // Enter must NOT also type a newline into the buffer.
        let text = app.editor.active_tab().unwrap().buffer.to_string();
        assert_eq!(text.lines().count(), 5, "the buffer is unchanged");
    });
}

/// No key and no explicit chat endpoint: the card fails as a setup step
/// instead of burning a request on a model that cannot write scenes.
#[gpui::test]
async fn visualize_without_credential_is_a_setup_step(cx: &mut TestAppContext) {
    let (dir, file) = test_workspace();
    let (deps, app_rx) = test_deps(dir.clone());
    let (app, cx) = cx.add_window_view(|_window, cx| JadeApp::new(cx, deps, app_rx));
    app.update_in(cx, |app, _w, cx| {
        app.open_file(file);
        stub_ai_prefs(app, &dir, true);
        // Force "resolved, absent" so the keychain is never touched.
        app.chat.set_credential_for_test(None);
        select_bytes(app, 17..31);
        app.visualize_trigger(cx);
    });
    cx.run_until_parked();
    app.update_in(cx, |app, _w, _cx| {
        let c = app.visualize.as_ref().expect("a card reports the setup step");
        match c.failure() {
            Some(crate::visualize::VisualizeError::Chat(e)) => {
                assert_eq!(e.headline(), "No model available");
            }
            other => panic!("{other:?}"),
        }
        assert!(!c.can_retry(), "a missing key is a setup step, not a retry");
    });
    assert!(cx.debug_bounds("visualize-card").is_some());
}

/// A consent card parked behind another tab must not swallow Enter or record
/// consent from a buffer the user cannot see (review finding).
#[gpui::test]
async fn hidden_consent_card_leaves_enter_to_the_editor(cx: &mut TestAppContext) {
    let (dir, file) = test_workspace();
    let other = dir.join("other.cpp");
    std::fs::write(&other, "int x = 1;\n").unwrap();
    let (deps, app_rx) = test_deps(dir.clone());
    let (app, cx) = cx.add_window_view(|_window, cx| JadeApp::new(cx, deps, app_rx));
    app.update_in(cx, |app, _w, cx| {
        app.open_file(file);
        stub_ai_prefs(app, &dir, false);
        stub_chat_credential(app);
        select_bytes(app, 17..31);
        app.visualize_trigger(cx);
        assert!(app.visualize.as_ref().unwrap().awaiting_consent());
        // Another file comes to the front; the card is hidden, not closed.
        app.open_file(other.clone());
        cx.notify();
    });
    cx.run_until_parked();

    cx.simulate_keystrokes("enter");
    cx.run_until_parked();

    app.update_in(cx, |app, _w, _cx| {
        assert!(!app.ai_prefs.visualize_enabled, "consent must not be recorded blind");
        assert!(app.visualize.as_ref().unwrap().awaiting_consent());
        // Enter went to the editor as a newline in the VISIBLE buffer.
        let text = app.editor.active_tab().unwrap().buffer.to_string();
        assert!(text.starts_with('\n'), "the newline landed in the editor: {text:?}");
    });
}

/// A retry while another tab is in front reads the tab that OWNS the card —
/// and is a no-op once that tab is closed (review finding).
#[gpui::test]
async fn retry_reads_the_owning_tab_not_the_active_one(cx: &mut TestAppContext) {
    use crate::app::AppEvent;
    let (dir, file) = test_workspace();
    let other = dir.join("other.cpp");
    std::fs::write(&other, "int x = 1;\n").unwrap();
    let (deps, app_rx) = test_deps(dir.clone());
    let (app, cx) = cx.add_window_view(|_window, cx| JadeApp::new(cx, deps, app_rx));
    app.update_in(cx, |app, _w, cx| {
        app.open_file(file.clone());
        // Promote the tab so the later open of `other` does not replace it.
        app.editor.active_tab_mut().unwrap().preview = false;
        stub_ai_prefs(app, &dir, true);
        stub_chat_credential(app);
        select_bytes(app, 17..31);
        app.visualize_trigger(cx);
        app.apply_app_event(AppEvent::Visualize {
            generation: 1,
            delta: jade_ai::ChatDelta::Failed(jade_ai::ChatError::Overloaded),
        });
        // Another file in front; the retry must still describe `main.cpp`.
        app.open_file(other.clone());
        app.visualize_retry(cx);
        let c = app.visualize.as_ref().unwrap();
        assert_eq!(c.generation, 2, "the retry was issued");
        assert_eq!(c.path, file, "the card still describes its own file");

        // Close the owning tab: a further retry has nothing to read and
        // leaves the card alone rather than reading the wrong buffer.
        app.apply_app_event(AppEvent::Visualize {
            generation: 2,
            delta: jade_ai::ChatDelta::Failed(jade_ai::ChatError::Overloaded),
        });
        let i = app.editor.tabs.iter().position(|t| t.path == file).unwrap();
        app.close_tab(i);
        app.visualize_retry(cx);
        assert_eq!(app.visualize.as_ref().unwrap().generation, 2, "no blind retry");
    });
}

/// A card that failed before any text has nothing to animate; the ticker must
/// stop instead of repainting a static banner at 30fps (review finding).
#[gpui::test]
async fn a_failed_explain_card_does_not_spin_the_ticker(cx: &mut TestAppContext) {
    let (dir, file) = test_workspace();
    let (deps, app_rx) = test_deps(dir);
    let (app, cx) = cx.add_window_view(|_window, cx| JadeApp::new(cx, deps, app_rx));
    app.update_in(cx, |app, _w, cx| {
        app.open_file(file);
        // Resolved-absent credential and no local server: the card parks on
        // a failure immediately, with no text ever arriving.
        app.chat.set_credential_for_test(None);
        select_bytes(app, 17..31);
        app.explain_trigger(cx);
        let c = app.explain.as_ref().unwrap();
        assert!(c.failure().is_some(), "{:?}", c.phase);
        assert!(
            !app.explain_working(),
            "a text-less terminal card must not keep the ticker alive"
        );
    });
}

/// Tab inserts one hard tab, Tab over a multi-row selection indents the block,
/// and ⇧Tab takes one indent step back off every line the selection covers.
#[gpui::test]
async fn tab_inserts_a_hard_tab_and_shift_tab_outdents(cx: &mut TestAppContext) {
    use jade_buffer::{Point, Selection};

    let (dir, file) = test_workspace();
    let (deps, app_rx) = test_deps(dir.clone());
    let (app, cx) = cx.add_window_view(|_window, cx| JadeApp::new(cx, deps, app_rx));
    app.update_in(cx, |app, _window, cx| {
        app.open_file(file.clone());
        cx.notify();
    });
    cx.run_until_parked();

    // A plain Tab at the start of row 0 inserts one char, not four spaces.
    cx.simulate_keystrokes("tab");
    app.update_in(cx, |app, _w, _cx| {
        let tab = app.editor.active_tab().unwrap();
        assert_eq!(tab.line(0), "\tint main() {", "Tab inserted a hard tab");
        assert_eq!(tab.caret_point().col, 1, "the tab is one column of caret travel");
    });

    // One Backspace takes the whole indent step back.
    cx.simulate_keystrokes("backspace");
    app.update_in(cx, |app, _w, _cx| {
        assert_eq!(app.editor.active_tab().unwrap().line(0), "int main() {");
    });

    // Select rows 1..2 ("    int alpha = 1;" and "    int beta = 2;"), then Tab.
    app.update_in(cx, |app, _w, cx| {
        let tab = app.editor.active_tab_mut().unwrap();
        let start = tab.buffer.point_to_offset(Point::new(1, 0));
        let end = tab.buffer.point_to_offset(Point::new(2, 4));
        tab.buffer.set_selection(Selection::new(start, end));
        cx.notify();
    });
    cx.simulate_keystrokes("tab");
    app.update_in(cx, |app, _w, _cx| {
        let tab = app.editor.active_tab().unwrap();
        assert_eq!(tab.line(1), "\t    int alpha = 1;", "row 1 gained an indent");
        assert_eq!(tab.line(2), "\t    int beta = 2;", "row 2 gained an indent");
        assert_eq!(tab.line(3), "    return alpha + beta;", "row 3 is outside");
    });

    // ⇧Tab undoes it: the leading tab goes first.
    cx.simulate_keystrokes("shift-tab");
    app.update_in(cx, |app, _w, _cx| {
        let tab = app.editor.active_tab().unwrap();
        assert_eq!(tab.line(1), "    int alpha = 1;");
        assert_eq!(tab.line(2), "    int beta = 2;");
    });

    // A second ⇧Tab eats the four spaces the file shipped with.
    cx.simulate_keystrokes("shift-tab");
    app.update_in(cx, |app, _w, _cx| {
        let tab = app.editor.active_tab().unwrap();
        assert_eq!(tab.line(1), "int alpha = 1;", "spaces outdent too");
        assert_eq!(tab.line(2), "int beta = 2;");
    });
}

/// Undo works in chunks, not per character: a typing burst is one group, Enter
/// closes it, and a caret jump starts a new one.
#[gpui::test]
async fn undo_takes_back_a_typing_burst_not_one_letter(cx: &mut TestAppContext) {
    let (dir, file) = test_workspace();
    let (deps, app_rx) = test_deps(dir.clone());
    let (app, cx) = cx.add_window_view(|_window, cx| JadeApp::new(cx, deps, app_rx));
    app.update_in(cx, |app, _window, cx| {
        app.open_file(file.clone());
        cx.notify();
    });
    cx.run_until_parked();

    // Type through the platform input pipeline, the path the keyboard uses.
    cx.simulate_input("hello");
    app.update_in(cx, |app, _w, _cx| {
        assert_eq!(
            app.editor.active_tab().unwrap().line(0),
            "helloint main() {"
        );
    });

    // One ⌘Z takes back all five letters.
    cx.simulate_keystrokes("cmd-z");
    app.update_in(cx, |app, _w, _cx| {
        assert_eq!(app.editor.active_tab().unwrap().line(0), "int main() {");
    });

    // One ⌘⇧Z puts them all back.
    cx.simulate_keystrokes("cmd-shift-z");
    app.update_in(cx, |app, _w, _cx| {
        assert_eq!(
            app.editor.active_tab().unwrap().line(0),
            "helloint main() {"
        );
    });

    // Enter closes the group: the burst after it undoes on its own.
    cx.simulate_keystrokes("enter");
    cx.simulate_input("world");
    app.update_in(cx, |app, _w, _cx| {
        let tab = app.editor.active_tab().unwrap();
        assert_eq!(tab.line(0), "hello");
        assert_eq!(tab.line(1), "worldint main() {");
    });
    cx.simulate_keystrokes("cmd-z");
    app.update_in(cx, |app, _w, _cx| {
        let tab = app.editor.active_tab().unwrap();
        assert_eq!(tab.line(1), "int main() {", "only the second burst came off");
        assert_eq!(tab.line(0), "hello", "the first burst stayed");
    });
}

// ── Hardware mode (§B11) ─────────────────────────────────────────────────────

/// Welcome keys pick a mode: `2` marks Hardware as pending and opens the
/// folder prompt. No tree scan happens — no workspace is open yet.
#[gpui::test]
async fn welcome_keys_choose_mode_without_scanning_a_tree(cx: &mut TestAppContext) {
    use crate::app::AppMode;

    let (dir, _file) = test_workspace();
    let (mut deps, app_rx) = test_deps(dir);
    deps.workspace_opened = false;
    let (app, cx) = cx.add_window_view(|_window, cx| JadeApp::new(cx, deps, app_rx));
    cx.run_until_parked();

    cx.simulate_keystrokes("2");
    app.update_in(cx, |app, _w, _cx| {
        assert_eq!(app.pending_mode, Some(AppMode::Hardware));
        assert!(app.tree.is_none(), "choosing a mode must not scan a tree");
        assert!(!app.workspace_opened);
    });

    // `1` overrides the pending pick with Software.
    cx.simulate_keystrokes("1");
    app.update_in(cx, |app, _w, _cx| {
        assert_eq!(app.pending_mode, Some(AppMode::Software));
        assert!(app.tree.is_none());
    });
}

/// Hardware mode swaps the runtime sidebar for the wave panel and drops the
/// C++ build/run/debug cluster from the action bar.
#[gpui::test]
async fn hardware_mode_swaps_wave_panel_for_runtime_sidebar(cx: &mut TestAppContext) {
    let (dir, file) = test_hw_workspace();
    let (deps, app_rx, _hw_rx) = test_deps_hw(dir);
    let (app, cx) = cx.add_window_view(|_window, cx| JadeApp::new(cx, deps, app_rx));
    app.update_in(cx, |app, _w, cx| {
        app.open_file(file.clone());
        cx.notify();
    });
    cx.run_until_parked();

    assert!(cx.debug_bounds("wave-panel").is_some(), "wave panel must render");
    assert!(cx.debug_bounds("wave-run-tb").is_some(), "Run testbench must render");
    assert!(cx.debug_bounds("hw-run").is_some(), "the sim Run/Pause must render");
    assert!(cx.debug_bounds("runtime-sidebar").is_none(), "no runtime sidebar");
    assert!(cx.debug_bounds("btn-build").is_none(), "no Build button");
    assert!(cx.debug_bounds("btn-run").is_none(), "no Run button");
    assert!(cx.debug_bounds("btn-debug").is_none(), "no Debug button");
}

/// LED frames update the render model's mask and per-LED duty.
#[gpui::test]
async fn led_frame_updates_duty(cx: &mut TestAppContext) {
    use crate::app::AppEvent;
    use jade_hw::HwEvent;

    let (dir, file) = test_hw_workspace();
    let (deps, app_rx, _hw_rx) = test_deps_hw(dir);
    let (app, cx) = cx.add_window_view(|_window, cx| JadeApp::new(cx, deps, app_rx));
    app.update_in(cx, |app, _w, cx| {
        app.open_file(file.clone());
        app.apply_app_event(AppEvent::Hw(HwEvent::LedFrame {
            bitmask: 0b00101,
            duty: [1.0, 0.0, 0.55, 0.0, 0.0],
        }));
        cx.notify();
    });
    app.update_in(cx, |app, _w, _cx| {
        let hw = app.hw.as_ref().unwrap();
        assert_eq!(hw.led_mask, 0b00101);
        assert_eq!(hw.led_duty[0], 1.0);
        assert_eq!(hw.led_duty[2], 0.55);
        assert_eq!(hw.led_duty[1], 0.0);
    });
}

/// A loaded dump lists the testbench's signals as rows; a click on the
/// canvas places the cursor, and the value column reads at it.
#[gpui::test]
async fn wave_panel_lists_rows_and_a_click_sets_the_cursor(cx: &mut TestAppContext) {
    use crate::app::AppEvent;
    use crate::wave::WaveEvent;
    use std::sync::Arc;

    let (dir, file) = test_hw_workspace();
    let vcd = dir.join("t.vcd");
    std::fs::write(
        &vcd,
        "$timescale 1ns $end\n$scope module tb $end\n$var wire 1 ! clk $end\n$var wire 4 \" cnt [3:0] $end\n$upscope $end\n$enddefinitions $end\n#0\n0!\nb0000 \"\n#10\n1!\nb0101 \"\n#20\n0!\n",
    )
    .unwrap();
    let (deps, app_rx, _hw_rx) = test_deps_hw(dir);
    let (app, cx) = cx.add_window_view(|_window, cx| JadeApp::new(cx, deps, app_rx));
    app.update_in(cx, |app, _w, cx| {
        app.open_file(file.clone());
        let loaded = jade_hw::wave::WaveFile::load(&vcd).expect("vcd loads");
        let generation = app.hw.as_ref().unwrap().wave.generation;
        app.apply_app_event(AppEvent::Wave(WaveEvent::Loaded {
            generation,
            result: Ok(Arc::new(loaded)),
        }));
        cx.notify();
    });
    cx.run_until_parked();

    app.update_in(cx, |app, _w, _cx| {
        let wave = &app.hw.as_ref().unwrap().wave;
        assert_eq!(wave.rows, vec![0, 1], "the testbench scope's signals are the rows");
        assert!(wave.cursor.is_none());
        assert_eq!(wave.read_time(), 20, "with no cursor the values read at the end");
    });
    assert!(cx.debug_bounds("wave-row-0").is_some(), "clk row renders");
    assert!(cx.debug_bounds("wave-row-1").is_some(), "cnt row renders");

    let canvas = cx.debug_bounds("wave-canvas").expect("trace canvas painted");
    cx.simulate_mouse_down(canvas.center(), gpui::MouseButton::Left, Modifiers::default());
    cx.simulate_mouse_up(canvas.center(), gpui::MouseButton::Left, Modifiers::default());
    cx.run_until_parked();
    app.update_in(cx, |app, _w, _cx| {
        let wave = &app.hw.as_ref().unwrap().wave;
        let c = wave.cursor.expect("a click places the cursor");
        assert!(c <= 20, "the cursor stays inside the dump: {c}");
        assert!(wave.drag.is_none(), "mouse up ends the pan");
        // The row that reads `cnt` at the cursor shows the value in force.
        let file = wave.file.as_ref().unwrap();
        let cnt = file.signals[1].value_at(c).map(|v| v.label(4));
        assert!(matches!(cnt.as_deref(), Some("0") | Some("5")), "cnt reads at the cursor: {cnt:?}");
    });
}

/// A re-run with a different signal set starts the rows from the new
/// testbench scope, instead of keeping only the names both dumps share.
#[gpui::test]
async fn wave_rows_reset_when_the_signal_set_changes(cx: &mut TestAppContext) {
    use crate::app::AppEvent;
    use crate::wave::WaveEvent;
    use std::sync::Arc;

    let (dir, file) = test_hw_workspace();
    let old = dir.join("old.vcd");
    std::fs::write(
        &old,
        "$timescale 1ns $end\n$scope module tb $end\n$var wire 1 ! clk $end\n$var wire 4 \" top $end\n$upscope $end\n$enddefinitions $end\n#0\n0!\nb0 \"\n#10\n1!\n",
    )
    .unwrap();
    let new = dir.join("new.vcd");
    std::fs::write(
        &new,
        "$timescale 1ns $end\n$scope module tb $end\n$var wire 1 ! clk $end\n$var wire 4 \" a $end\n$var wire 4 # out $end\n$upscope $end\n$enddefinitions $end\n#0\n0!\nb0 \"\nb0 #\n#10\n1!\n",
    )
    .unwrap();
    let (deps, app_rx, _hw_rx) = test_deps_hw(dir);
    let (app, cx) = cx.add_window_view(|_window, cx| JadeApp::new(cx, deps, app_rx));
    app.update_in(cx, |app, _w, cx| {
        app.open_file(file.clone());
        for path in [&old, &new] {
            let loaded = jade_hw::wave::WaveFile::load(path).expect("vcd loads");
            let generation = app.hw.as_ref().unwrap().wave.generation;
            app.apply_app_event(AppEvent::Wave(WaveEvent::Loaded {
                generation,
                result: Ok(Arc::new(loaded)),
            }));
        }
        cx.notify();
    });
    app.update_in(cx, |app, _w, _cx| {
        let wave = &app.hw.as_ref().unwrap().wave;
        let names: Vec<String> = wave
            .rows
            .iter()
            .map(|&i| wave.file.as_ref().unwrap().signals[i].name.clone())
            .collect();
        assert_eq!(names, vec!["clk", "a", "out"], "rows restart from the new testbench scope");
    });
}

/// A Verilog tab opens with tree-sitter highlighting and none of the C++
/// analysis products (flow, structure, sizes) — and no clangd.
#[gpui::test]
async fn verilog_tab_opens_highlighted_without_lsp(cx: &mut TestAppContext) {
    let (dir, file) = test_hw_workspace();
    let (deps, app_rx, _hw_rx) = test_deps_hw(dir);
    let (app, cx) = cx.add_window_view(|_window, cx| JadeApp::new(cx, deps, app_rx));
    app.update_in(cx, |app, _w, cx| {
        app.open_file(file.clone());
        cx.notify();
    });
    app.update_in(cx, |app, _w, _cx| {
        let tab = app.editor.active_tab().expect("blink.v is open");
        assert!(tab.span_count() > 0, "Verilog highlight spans exist");
        assert!(tab.flow.segments.is_empty(), "no C++ flow analysis");
        assert!(tab.symbols.is_empty(), "no C++ structure symbols");
        assert!(tab.sizes.is_empty(), "no C++ size annotations");
    });
}

/// Saving an HDL file in hardware mode sends a Recompile to the engine and
/// marks the panel as compiling.
#[gpui::test]
async fn save_in_hardware_mode_triggers_recompile_command(cx: &mut TestAppContext) {
    use jade_hw::HwCommand;

    let (dir, file) = test_hw_workspace();
    let (deps, app_rx, mut hw_rx) = test_deps_hw(dir);
    let (app, cx) = cx.add_window_view(|_window, cx| JadeApp::new(cx, deps, app_rx));
    app.update_in(cx, |app, _w, cx| {
        app.open_file(file.clone());
        cx.notify();
    });
    cx.run_until_parked();
    while hw_rx.try_recv().is_ok() {}

    app.update_in(cx, |app, _w, _cx| {
        app.editor_save();
        assert!(app.hw.as_ref().unwrap().compiling, "save marks the panel compiling");
    });
    let mut cmds = Vec::new();
    while let Ok(c) = hw_rx.try_recv() {
        cmds.push(c);
    }
    assert!(
        cmds.contains(&HwCommand::Recompile),
        "the save reached the engine as a Recompile: {cmds:?}"
    );
}

/// The schematic follows the selected editor tab: opening a second Verilog
/// file sends its path to the engine, and the same tab sends nothing twice.
#[gpui::test]
async fn schematic_follows_the_selected_tab(cx: &mut TestAppContext) {
    use jade_hw::HwCommand;

    let (dir, file) = test_hw_workspace();
    let other = dir.join("uart.v");
    std::fs::write(&other, "module uart (input wire clk);\nendmodule\n").unwrap();

    let (deps, app_rx, mut hw_rx) = test_deps_hw(dir);
    let (app, cx) = cx.add_window_view(|_window, cx| JadeApp::new(cx, deps, app_rx));
    app.update_in(cx, |app, _w, cx| {
        app.open_file(file.clone());
        cx.notify();
    });
    cx.run_until_parked();
    let mut cmds = Vec::new();
    while let Ok(c) = hw_rx.try_recv() {
        cmds.push(c);
    }
    assert!(
        cmds.contains(&HwCommand::SchematicFor(Some(file.clone()))),
        "the first tab reached the engine: {cmds:?}"
    );

    // The same tab stays selected: the render loop must stay quiet.
    app.update_in(cx, |_app, _w, cx| cx.notify());
    cx.run_until_parked();
    assert!(
        hw_rx.try_recv().is_err(),
        "an unchanged selection sends no command"
    );

    app.update_in(cx, |app, _w, cx| {
        app.open_file(other.clone());
        cx.notify();
    });
    cx.run_until_parked();
    let mut cmds = Vec::new();
    while let Ok(c) = hw_rx.try_recv() {
        cmds.push(c);
    }
    assert!(
        cmds.contains(&HwCommand::SchematicFor(Some(other.clone()))),
        "the new tab reached the engine: {cmds:?}"
    );
}

/// The chosen mode persists in the workspace `ui` blob and comes back on the
/// next open of the same folder.
#[gpui::test]
async fn mode_persists_across_reopen(cx: &mut TestAppContext) {
    use crate::app::AppMode;

    let (dir, file) = test_hw_workspace();
    let (deps, app_rx, _hw_rx) = test_deps_hw(dir.clone());
    let (app, cx) = cx.add_window_view(|_window, cx| JadeApp::new(cx, deps, app_rx));
    app.update_in(cx, |app, _w, cx| {
        app.open_file(file.clone());
        app.save_ui_state();
        cx.notify();
    });

    let ui = crate::workspace_state::load(&dir);
    assert_eq!(ui.mode.as_deref(), Some("hardware"), "mode persisted");

    // A fresh app over the same folder resolves back into hardware mode
    // WITHOUT the --hardware flag or a pending welcome pick.
    let (dir2, _f2) = test_workspace();
    let (mut deps2, app_rx2) = test_deps(dir2);
    let (hw_tx2, _hw_rx2) = tokio::sync::mpsc::unbounded_channel();
    deps2.hw_tx = Some(hw_tx2);
    let (app2, cx) = cx.add_window_view(|_window, cx| JadeApp::new(cx, deps2, app_rx2));
    app2.update_in(cx, |app, _w, cx| {
        assert_eq!(app.mode, AppMode::Software, "the C++ folder stays software");
        app.open_project(dir.clone());
        assert_eq!(app.mode, AppMode::Hardware, "the persisted mode comes back");
        assert!(app.hw.is_some());
        cx.notify();
    });
}

/// The Schematic tab (hardware mode): a Netlist event renders the RTL graph
/// in the bottom panel; software mode never shows the tab.
#[gpui::test]
async fn schematic_tab_renders_the_netlist(cx: &mut TestAppContext) {
    use crate::app::{AppEvent, BottomView};
    use jade_hw::netlist::{NetEdge, NetNode, Netlist, NodeKind};
    use jade_hw::HwEvent;

    let (dir, file) = test_hw_workspace();
    let (deps, app_rx, _hw_rx) = test_deps_hw(dir);
    let (app, cx) = cx.add_window_view(|_window, cx| JadeApp::new(cx, deps, app_rx));
    app.update_in(cx, |app, _w, cx| {
        app.open_file(file.clone());
        // A tiny graph: clk → reg count → NOT → led.
        let nl = Netlist {
            top: "blink".into(),
            nodes: vec![
                NetNode { id: 0, kind: NodeKind::Input, label: "clk".into(), width: 1, clock: None, layer: 0 },
                NetNode { id: 1, kind: NodeKind::Reg, label: "count".into(), width: 26, clock: Some("clk".into()), layer: 1 },
                NetNode { id: 2, kind: NodeKind::Op, label: "~".into(), width: 5, clock: None, layer: 1 },
                NetNode { id: 3, kind: NodeKind::Output, label: "led".into(), width: 5, clock: None, layer: 2 },
            ],
            edges: vec![
                NetEdge { from: 1, to: 2, to_pin: 0, label: Some("[25:21]".into()), width: 5 },
                NetEdge { from: 2, to: 3, to_pin: 0, label: None, width: 5 },
            ],
        };
        app.apply_app_event(AppEvent::Hw(HwEvent::Netlist(nl)));
        app.bottom_view = BottomView::Schematic;
        cx.notify();
    });
    cx.run_until_parked();

    assert!(
        cx.debug_bounds("schematic-body").is_some(),
        "the schematic body renders in hardware mode"
    );
    app.update_in(cx, |app, _w, _cx| {
        assert!(app.hw.as_ref().unwrap().netlist.is_some());
    });
}

/// The schematic's gate view: the eyebrow toggle sends `SchematicGates` to
/// the engine, a `GateNetlist` event renders in gate mode, and a missing
/// yosys stores the install hint.
#[gpui::test]
async fn schematic_gate_view_toggles_and_renders(cx: &mut TestAppContext) {
    use crate::app::{AppEvent, BottomView};
    use jade_hw::netlist::{NetEdge, NetNode, Netlist, NodeKind};
    use jade_hw::{HwCommand, HwEvent};

    let (dir, file) = test_hw_workspace();
    let (deps, app_rx, mut hw_rx) = test_deps_hw(dir);
    let (app, cx) = cx.add_window_view(|_window, cx| JadeApp::new(cx, deps, app_rx));
    app.update_in(cx, |app, _w, cx| {
        app.open_file(file.clone());
        app.hw_toggle_schematic_gates();
        let hw = app.hw.as_ref().unwrap();
        assert!(hw.schematic_gates);
        assert!(hw.synthesizing, "no gate graph yet: the wait state shows");

        // The synthesized graph arrives: a AND-into-register circuit.
        let nl = Netlist {
            top: "blink".into(),
            nodes: vec![
                NetNode { id: 0, kind: NodeKind::Input, label: "a".into(), width: 2, clock: None, layer: 0 },
                NetNode { id: 1, kind: NodeKind::Op, label: "&".into(), width: 1, clock: None, layer: 1 },
                NetNode { id: 2, kind: NodeKind::Reg, label: "q".into(), width: 1, clock: Some("clk".into()), layer: 0 },
                NetNode { id: 3, kind: NodeKind::Output, label: "y".into(), width: 1, clock: None, layer: 2 },
            ],
            edges: vec![
                NetEdge { from: 0, to: 1, to_pin: 0, label: Some("[0]".into()), width: 1 },
                NetEdge { from: 0, to: 1, to_pin: 1, label: Some("[1]".into()), width: 1 },
                NetEdge { from: 1, to: 2, to_pin: 0, label: None, width: 1 },
                NetEdge { from: 2, to: 3, to_pin: 0, label: None, width: 1 },
            ],
        };
        app.apply_app_event(AppEvent::Hw(HwEvent::GateNetlist(nl)));
        let hw = app.hw.as_ref().unwrap();
        assert!(hw.gate_netlist.is_some());
        assert!(!hw.synthesizing);
        app.bottom_view = BottomView::Schematic;
        cx.notify();
    });
    cx.run_until_parked();

    assert!(
        cx.debug_bounds("schematic-body").is_some(),
        "the gate graph renders in the schematic body"
    );
    let mut cmds = Vec::new();
    while let Ok(c) = hw_rx.try_recv() {
        cmds.push(c);
    }
    assert!(
        cmds.contains(&HwCommand::SchematicGates(true)),
        "the toggle reached the engine: {cmds:?}"
    );

    // A missing yosys reports the install hint and ends the wait state.
    app.update_in(cx, |app, _w, cx| {
        app.apply_app_event(AppEvent::Hw(HwEvent::SchematicSynthMissing {
            install_hint: "brew install yosys".into(),
        }));
        let hw = app.hw.as_ref().unwrap();
        assert_eq!(hw.synth_missing.as_deref(), Some("brew install yosys"));
        assert!(!hw.synthesizing);
        // The toggle back returns to the RTL view.
        app.hw_toggle_schematic_gates();
        assert!(!app.hw.as_ref().unwrap().schematic_gates);
        cx.notify();
    });
}

/// Markdown preview panel: it opens by itself on a `.md` tab, a block click
/// enters preview-edit mode (raw source + buffer caret), typed text lands in
/// the buffer through the normal editor path, Escape returns to the rendered
/// view, and ⌘⇧D hides the panel.
#[gpui::test]
async fn markdown_preview_edits_in_both_views(cx: &mut TestAppContext) {
    let (dir, _file) = test_workspace();
    let md = dir.join("README.md");
    std::fs::write(&md, "# Title\n\nHello world paragraph.\n\n- item one\n").unwrap();
    let (deps, app_rx) = test_deps(dir);
    let (app, cx) = cx.add_window_view(|_window, cx| JadeApp::new(cx, deps, app_rx));
    app.update_in(cx, |app, _w, cx| {
        app.open_file(md.clone());
        cx.notify();
    });
    cx.run_until_parked();

    // The preview opens by itself on a Markdown tab.
    assert!(cx.debug_bounds("markdown-panel").is_some(), "panel opens on the .md tab");
    assert!(cx.debug_bounds("md-block-0").is_some(), "heading block rendered");

    // A click on the heading block enters preview-edit mode: the caret moves
    // to the block's first row and the block shows raw source.
    let b = cx.debug_bounds("md-block-0").unwrap();
    cx.simulate_click(b.center(), Modifiers::default());
    cx.run_until_parked();
    app.update_in(cx, |app, _w, _cx| {
        assert!(app.md_edit, "preview click enters edit mode");
        assert_eq!(app.editor.active_tab().unwrap().caret_point().row, 0);
    });
    assert!(cx.debug_bounds("md-raw-0").is_some(), "clicked block shows raw source");

    // Typing goes through the editor pipeline into the shared buffer.
    cx.simulate_keystrokes("cmd-right");
    cx.simulate_input("!!");
    app.update_in(cx, |app, _w, _cx| {
        let tab = app.editor.active_tab().unwrap();
        assert_eq!(tab.line(0), "# Title!!", "typed text reached the buffer");
        assert!(tab.buffer.is_dirty());
    });
    cx.run_until_parked();

    // Escape leaves preview-edit mode; the raw block renders again as prose.
    cx.simulate_keystrokes("escape");
    cx.run_until_parked();
    app.update_in(cx, |app, _w, _cx| {
        assert!(!app.md_edit, "escape leaves edit mode");
    });
    assert!(cx.debug_bounds("md-raw-0").is_none());
    assert!(cx.debug_bounds("md-block-0").is_some());

    // An edit made in the CODE view reshapes the preview: a new heading at the
    // end of the file becomes a new block.
    app.update_in(cx, |app, _w, cx| {
        let tab = app.editor.active_tab_mut().unwrap();
        let end = tab.buffer.len_bytes();
        tab.buffer.set_caret(end);
        cx.notify();
    });
    cx.simulate_input("\n## Second\n");
    cx.run_until_parked();
    app.update_in(cx, |app, _w, _cx| {
        let tab = app.editor.active_tab().unwrap();
        let row = tab.line_count() - 2;
        assert_eq!(tab.line(row), "## Second");
    });

    // ⌘⇧D hides the panel again.
    cx.simulate_keystrokes("cmd-shift-d");
    cx.run_until_parked();
    assert!(cx.debug_bounds("markdown-panel").is_none());
}

/// The panel opens only for Markdown tabs, and it takes the right slot from
/// the runtime sidebar while a `.md` tab is in front.
#[gpui::test]
async fn markdown_preview_gates_on_the_active_tab(cx: &mut TestAppContext) {
    let (dir, cpp) = test_workspace();
    let md = dir.join("notes.md");
    std::fs::write(&md, "plain paragraph\n").unwrap();
    let (deps, app_rx) = test_deps(dir);
    let (app, cx) = cx.add_window_view(|_window, cx| JadeApp::new(cx, deps, app_rx));
    app.update_in(cx, |app, _w, cx| {
        app.open_file(cpp.clone());
        // Show the runtime sidebar (off by default), so the test proves the
        // preview takes its slot.
        app.runtime_visible = true;
        cx.notify();
    });
    cx.run_until_parked();
    assert!(
        cx.debug_bounds("markdown-panel").is_none(),
        "no panel for a C++ tab"
    );
    assert!(
        cx.debug_bounds("runtime-sidebar").is_some(),
        "runtime sidebar in place on a C++ tab"
    );

    app.update_in(cx, |app, _w, cx| {
        app.open_file(md.clone());
        cx.notify();
    });
    cx.run_until_parked();
    assert!(
        cx.debug_bounds("markdown-panel").is_some(),
        "panel appears on the .md tab"
    );
    assert!(
        cx.debug_bounds("runtime-sidebar").is_none(),
        "the preview takes the runtime sidebar's slot"
    );
}

/// The preview is universal: in hardware mode a Markdown tab replaces the
/// wave panel, and a Verilog tab brings the wave panel back.
#[gpui::test]
async fn markdown_preview_replaces_the_wave_panel_in_hardware_mode(cx: &mut TestAppContext) {
    let (dir, file) = test_hw_workspace();
    let md = dir.join("README.md");
    std::fs::write(&md, "# Board notes\n").unwrap();
    let (deps, app_rx, _hw_rx) = test_deps_hw(dir);
    let (app, cx) = cx.add_window_view(|_window, cx| JadeApp::new(cx, deps, app_rx));
    app.update_in(cx, |app, _w, cx| {
        app.open_file(md.clone());
        cx.notify();
    });
    cx.run_until_parked();
    assert!(
        cx.debug_bounds("markdown-panel").is_some(),
        "preview renders in hardware mode"
    );
    assert!(
        cx.debug_bounds("wave-panel").is_none(),
        "the preview takes the wave panel's slot"
    );

    app.update_in(cx, |app, _w, cx| {
        app.open_file(file.clone());
        cx.notify();
    });
    cx.run_until_parked();
    assert!(cx.debug_bounds("markdown-panel").is_none());
    assert!(
        cx.debug_bounds("wave-panel").is_some(),
        "the wave panel returns on a Verilog tab"
    );
}

// ── Split panes (window management) ──────────────────────────────────────────

/// Split, open a file in the new pane, move it back by drag, and close the
/// pane: the layout and the live editor follow every step, and a file lives
/// in one pane at most.
#[test]
fn split_panes_move_and_close() {
    let (dir, main) = test_workspace();
    let readme = dir.join("README.md");
    std::fs::write(&readme, "# Title\n\ntext\n").unwrap();
    let (deps, _rx) = test_deps(dir.clone());
    let mut app = JadeApp::assemble(deps);
    app.open_file(main.clone());
    assert_eq!(app.pane_count(), 1);
    assert!(!app.split_mode());

    // ⌘\: a new empty pane to the right takes the keyboard.
    app.split_pane();
    assert_eq!(app.pane_count(), 2);
    assert_eq!(app.focused_pane(), 1);
    assert!(app.editor.tabs.is_empty());
    assert_eq!(app.active_file, None);
    assert_eq!(app.pane_active_tab(0).unwrap().path, main);

    // The next open lands in the focused (right) pane; a Markdown tab there
    // shows formatted by default, and the docked preview stays away.
    app.open_file(readme.clone());
    assert_eq!(app.editor.active_path(), Some(readme.clone()));
    assert!(app.pane_shows_md(1));
    assert!(!app.md_panel_active());
    app.set_pane_md(1, false);
    assert!(!app.pane_shows_md(1));

    // An open of a file another pane holds focuses that pane, no second buffer.
    app.open_file(main.clone());
    assert_eq!(app.focused_pane(), 0);
    assert_eq!(app.editor.active_path(), Some(main.clone()));
    assert_eq!(app.pane_active_tab(1).unwrap().path, readme);

    // Drag README from pane 1 onto pane 0's strip: pane 1 empties and closes.
    app.move_tab(1, 0, 0, None);
    assert_eq!(app.pane_count(), 1);
    assert_eq!(app.editor.tabs.len(), 2);
    assert_eq!(app.editor.active_path(), Some(readme.clone()));

    // Drag main.cpp to the right edge: a new pane with it takes the keyboard.
    app.split_with_tab(0, 0, 0);
    assert_eq!(app.pane_count(), 2);
    assert_eq!(app.focused_pane(), 1);
    assert_eq!(app.editor.active_path(), Some(main.clone()));
    assert_eq!(app.pane_active_tab(0).unwrap().path, readme);

    // Persist + restore the two-pane layout in a fresh app.
    app.save_ui_state();
    let (deps2, _rx2) = test_deps(dir.clone());
    let app2 = JadeApp::assemble(deps2);
    assert_eq!(app2.pane_count(), 2);
    assert_eq!(app2.focused_pane(), 1);
    assert_eq!(app2.editor.active_path(), Some(main.clone()));
    assert_eq!(app2.pane_active_tab(0).unwrap().path, readme);

    // Close the focused pane: the neighbor takes the keyboard.
    app.close_pane(1);
    assert_eq!(app.pane_count(), 1);
    assert_eq!(app.editor.active_path(), Some(readme));
    let _ = std::fs::remove_dir_all(&dir);
}

/// A formatted Markdown pane keeps its scroll offset across a focus swap:
/// the offset moves into the slot's own handle when the pane goes to the
/// background, and back into the live handle when the pane gets the
/// keyboard again. The caret sync does not fire on the swap.
#[test]
fn md_pane_keeps_scroll_across_focus() {
    let (dir, main) = test_workspace();
    let readme = dir.join("README.md");
    std::fs::write(&readme, "# Title\n\ntext\n\nmore\n").unwrap();
    let (deps, _rx) = test_deps(dir.clone());
    let mut app = JadeApp::assemble(deps);
    app.open_file(main.clone());
    app.split_pane();
    app.open_file(readme.clone());
    assert_eq!(app.focused_pane(), 1);
    assert!(app.pane_shows_md(1));

    // A wheel scroll in the focused formatted view.
    app.md_scroll.set_offset(gpui::point(px(0.), px(-240.)));

    // Click off into pane 0: the offset lands in pane 1's own handle.
    app.focus_pane(0);
    assert_eq!(app.focused_pane(), 0);
    assert_eq!(f32::from(app.panes[1].md_scroll.offset().y), -240.);

    // A wheel scroll while pane 1 is in the background.
    app.panes[1]
        .md_scroll
        .set_offset(gpui::point(px(0.), px(-120.)));

    // Click back: the live handle takes the background offset, and the
    // caret sync is marked done for the tab so the render does not jump.
    app.focus_pane(1);
    assert_eq!(app.focused_pane(), 1);
    assert_eq!(f32::from(app.md_scroll.offset().y), -120.);
    assert_eq!(app.md_synced, Some((readme, 0)));
    let _ = std::fs::remove_dir_all(&dir);
}

/// A code pane keeps its exact scroll offset across a focus swap, in both
/// directions, and each active tab remembers its page row.
#[test]
fn code_pane_keeps_scroll_across_focus() {
    use crate::panels::code_view::{LINE_H, PAD_TOP};
    let (dir, main) = test_workspace();
    let other = dir.join("other.cpp");
    std::fs::write(&other, "int a;\n".repeat(200)).unwrap();
    let (deps, _rx) = test_deps(dir.clone());
    let mut app = JadeApp::assemble(deps);
    app.open_file(main.clone());
    app.split_pane();
    app.open_file(other.clone());
    assert_eq!(app.focused_pane(), 1);

    // A wheel scroll in the focused code list: 12 rows and a bit, plus some
    // horizontal travel.
    let y = -(12.0 * LINE_H + PAD_TOP + 5.0);
    app.code_scroll
        .0
        .borrow()
        .base_handle
        .set_offset(point(px(-30.), px(y)));

    // Click into pane 0: pane 1's own list takes the exact offset, and pane
    // 0 (never scrolled) shows from the top.
    app.focus_pane(0);
    assert_eq!(app.focused_pane(), 0);
    let parked = app.panes[1].scroll.0.borrow().base_handle.offset();
    assert_eq!((f32::from(parked.x), f32::from(parked.y)), (-30., y));
    assert_eq!(app.panes[1].active_tab().unwrap().scroll_top, 13);
    let live = app.code_scroll.0.borrow().base_handle.offset();
    assert_eq!((f32::from(live.x), f32::from(live.y)), (0., 0.));

    // A wheel scroll while pane 1 is in the background, then click back.
    app.panes[1]
        .scroll
        .0
        .borrow()
        .base_handle
        .set_offset(point(px(0.), px(-100.)));
    app.focus_pane(1);
    assert_eq!(app.focused_pane(), 1);
    let live = app.code_scroll.0.borrow().base_handle.offset();
    assert_eq!(f32::from(live.y), -100.);
    assert_eq!(app.editor.active_tab().unwrap().scroll_top, 5);
    let _ = std::fs::remove_dir_all(&dir);
}

/// A tab moved into another pane shows at its remembered page row, and the
/// pane it left keeps the page of the tab that stays active there.
#[test]
fn moved_tab_keeps_its_page() {
    use crate::panels::code_view::LINE_H;
    let (dir, main) = test_workspace();
    let a = dir.join("a.cpp");
    let b = dir.join("b.cpp");
    std::fs::write(&a, "int a;\n".repeat(200)).unwrap();
    std::fs::write(&b, "int b;\n".repeat(200)).unwrap();
    let (deps, _rx) = test_deps(dir.clone());
    let mut app = JadeApp::assemble(deps);
    app.open_file(main.clone());
    app.editor.active_tab_mut().unwrap().preview = false; // keep it open
    app.open_file(a.clone());
    assert_eq!(app.editor.tabs.len(), 2);
    let set_live = |app: &JadeApp, rows: f32| {
        app.code_scroll
            .0
            .borrow()
            .base_handle
            .set_offset(point(px(0.), px(-(rows * LINE_H))));
    };
    let live_y = |app: &JadeApp| f32::from(app.code_scroll.0.borrow().base_handle.offset().y);

    // `a` at row 30, then back to main.cpp at row 7.
    set_live(&app, 30.);
    app.switch_tab(0);
    assert_eq!(app.editor.tabs[1].scroll_top, 30);
    assert_eq!(live_y(&app), 0.);
    set_live(&app, 7.);

    // A second pane with `b`, then drag `a` onto its strip.
    app.split_pane();
    app.open_file(b.clone());
    app.move_tab(0, 1, 1, None);
    assert_eq!(app.focused_pane(), 1);
    assert_eq!(app.editor.active_path(), Some(a));
    assert_eq!(live_y(&app), -(30. * LINE_H));
    let parked = app.panes[0].scroll.0.borrow().base_handle.offset();
    assert_eq!(f32::from(parked.y), -(7. * LINE_H));
    assert_eq!(app.pane_active_tab(0).unwrap().path, main);
    let _ = std::fs::remove_dir_all(&dir);
}

/// A drag from the right pane into the left pane's split zone: the new
/// pane lands between the two, and the emptied right pane closes.
#[test]
fn split_with_tab_from_the_right_closes_the_empty_pane() {
    let (dir, main) = test_workspace();
    let a = dir.join("a.cpp");
    std::fs::write(&a, "int a;\n").unwrap();
    let (deps, _rx) = test_deps(dir.clone());
    let mut app = JadeApp::assemble(deps);
    app.open_file(main.clone());
    app.split_pane();
    app.open_file(a.clone());
    assert_eq!(app.focused_pane(), 1);

    app.split_with_tab(1, 0, 0);
    assert_eq!(app.pane_count(), 2);
    assert_eq!(app.focused_pane(), 1);
    assert_eq!(app.editor.active_path(), Some(a));
    assert_eq!(app.pane_active_tab(0).unwrap().path, main);
    let _ = std::fs::remove_dir_all(&dir);
}

/// Reorder inside one pane keeps the active tab the same tab.
#[test]
fn reorder_tabs_keeps_active() {
    let (dir, main) = test_workspace();
    let other = dir.join("util.cpp");
    std::fs::write(&other, "int u();\n").unwrap();
    let (deps, _rx) = test_deps(dir.clone());
    let mut app = JadeApp::assemble(deps);
    let _ = app.editor.open(&main);
    let _ = app.editor.open(&other);
    app.editor.switch(0);
    app.move_tab(0, 0, 0, Some(2)); // main.cpp to the end
    assert_eq!(app.editor.tabs[1].path, main);
    assert_eq!(app.editor.active, Some(1));
    let _ = std::fs::remove_dir_all(&dir);
}

/// A divider drag moves width share between two neighbors, stops at the
/// minimum pane width, and a double-click equalizes.
#[test]
fn pane_divider_moves_share() {
    let (dir, main) = test_workspace();
    let (deps, _rx) = test_deps(dir.clone());
    let mut app = JadeApp::assemble(deps);
    app.open_file(main);
    app.split_pane();
    assert_eq!(app.pane_count(), 2);
    // Row 1000px, shares 1:1 → 500px each. +100px on the divider → 600/400.
    app.resize_panes(0, 100.0, 1.0, 1.0, 1000.0);
    assert!((app.panes[0].weight - 1.2).abs() < 1e-4);
    assert!((app.panes[1].weight - 0.8).abs() < 1e-4);
    // The right pane cannot go under 160px: share 0.32 of 2.0.
    app.resize_panes(0, 900.0, 1.0, 1.0, 1000.0);
    assert!((app.panes[1].weight - 0.32).abs() < 1e-4);
    assert!((app.panes[0].weight - 1.68).abs() < 1e-4);
    app.equalize_panes();
    assert_eq!(app.panes[0].weight, 1.0);
    assert_eq!(app.panes[1].weight, 1.0);
    let _ = std::fs::remove_dir_all(&dir);
}

/// Implement-from-header stubs: with `math.h` included, typing `ad` at file
/// scope offers `int add(int a, int b)`; Enter fills the definition stub and
/// puts the caret on the body line.
#[gpui::test]
async fn header_stub_completion_fills_a_definition(cx: &mut TestAppContext) {
    let (dir, _main) = test_workspace();
    std::fs::write(
        dir.join("math.h"),
        "int add(int a, int b);\nint sub(int a, int b);\n",
    )
    .unwrap();
    let file = dir.join("impl.cpp");
    std::fs::write(
        &file,
        "#include \"math.h\"\n\nint sub(int a, int b) { return a - b; }\n\n",
    )
    .unwrap();
    let (deps, app_rx) = test_deps(dir);

    let (app, cx) = cx.add_window_view(|_window, cx| JadeApp::new(cx, deps, app_rx));
    app.update_in(cx, |app, _window, cx| {
        app.open_file(file.clone());
        cx.notify();
    });
    cx.run_until_parked();

    // Click the empty row 3 (file scope) and type a return type + name.
    let cell = cx
        .debug_bounds("code-cell-3")
        .expect("code cell for row 3 was painted");
    cx.simulate_click(
        point(
            cell.origin.x + px(1.0),
            cell.origin.y + px(crate::panels::code_view::LINE_H / 2.0),
        ),
        Modifiers::default(),
    );
    cx.simulate_input("int ad");
    app.update_in(cx, |app, _w, _cx| {
        let c = app.completion.as_ref().expect("popup opened for the header stub");
        let item = c.current().expect("a stub is selected");
        assert_eq!(item.label, "int add(int a, int b)");
        assert_eq!(item.detail.as_deref(), Some("implement · math.h"));
        // `sub` is already defined in this file, so it is not offered.
        assert!(
            !c.items.iter().any(|it| it.label.contains("sub(")),
            "defined function was offered again"
        );
    });

    cx.simulate_keystrokes("enter");
    app.update_in(cx, |app, _w, _cx| {
        assert!(app.completion.is_none(), "popup closed on accept");
        let tab = app.editor.active_tab().unwrap();
        assert_eq!(tab.line(3), "int add(int a, int b) {");
        assert_eq!(tab.line(4), "    ");
        assert_eq!(tab.line(5), "}");
        let caret = tab.caret_point();
        assert_eq!((caret.row, caret.col), (4, 4), "caret sits on the body line");
    });
}

/// Shared setup for the stub-and-ghost seam tests: a workspace with `math.h`
/// and an `impl.cpp` that includes it, the AI backend marked Ready, and the
/// caret on the empty row 3 with `int ad` typed so the stub popup is open.
async fn stub_popup_with_ghost_ready(
    cx: &mut TestAppContext,
) -> (PathBuf, gpui::Entity<JadeApp>, &mut gpui::VisualTestContext) {
    use jade_ai::AiState;

    let (dir, _main) = test_workspace();
    std::fs::write(dir.join("math.h"), "int add(int a, int b);\n").unwrap();
    let file = dir.join("impl.cpp");
    std::fs::write(&file, "#include \"math.h\"\n\n\n").unwrap();
    let (deps, app_rx) = test_deps(dir.clone());

    let (app, cx) = cx.add_window_view(|_window, cx| JadeApp::new(cx, deps, app_rx));
    app.update_in(cx, |app, _window, cx| {
        app.open_file(file.clone());
        app.ai_status.state = AiState::Ready;
        cx.notify();
    });
    cx.run_until_parked();

    let cell = cx
        .debug_bounds("code-cell-2")
        .expect("code cell for row 2 was painted");
    cx.simulate_click(
        point(
            cell.origin.x + px(1.0),
            cell.origin.y + px(crate::panels::code_view::LINE_H / 2.0),
        ),
        Modifiers::default(),
    );
    cx.simulate_input("int ad");
    app.update_in(cx, |app, _w, _cx| {
        let c = app.completion.as_ref().expect("stub popup is open");
        assert_eq!(c.current().unwrap().label, "int add(int a, int b)");
        assert!(app.ghost.is_none(), "ghost request is still in flight");
    });
    (dir, app, cx)
}

/// Deliver the `/infill` response for the current ghost generation, as the
/// debounced task would, with `content` anchored at the caret.
fn deliver_ghost(app: &gpui::Entity<JadeApp>, cx: &mut gpui::VisualTestContext, content: &str) {
    use crate::app::AppEvent;
    let (generation, prefix, suffix, anchor) = app.update_in(cx, |app, _w, _cx| {
        let tab = app.editor.active_tab().unwrap();
        let full = tab.buffer.to_string();
        let caret = tab.buffer.selection().caret();
        let p = tab.caret_point();
        (
            app.ghost_generation(),
            full[..caret].to_string(),
            full[caret..].to_string(),
            (p.row, p.col),
        )
    });
    let content = content.to_string();
    app.update_in(cx, |app, _w, cx| {
        app.apply_app_event(AppEvent::Ghost {
            generation,
            content: Some(content),
            prefix,
            suffix,
            line_suffix: String::new(),
            anchor,
            max_lines: 6,
        });
        cx.notify();
    });
    cx.run_until_parked();
}

/// Seam 1: Enter takes the header stub, then the ghost picks up on the empty
/// body line and Tab fills the body. The two hand off without a stale popup.
#[gpui::test]
async fn header_stub_hands_off_to_ghost_body(cx: &mut TestAppContext) {
    let (dir, app, cx) = stub_popup_with_ghost_ready(cx).await;

    cx.simulate_keystrokes("enter");
    app.update_in(cx, |app, _w, _cx| {
        assert!(app.completion.is_none(), "popup closed on stub accept");
        assert!(app.ghost.is_none(), "old ghost cleared; body request in flight");
        let tab = app.editor.active_tab().unwrap();
        assert_eq!(tab.line(2), "int add(int a, int b) {");
        assert_eq!((tab.caret_point().row, tab.caret_point().col), (3, 4));
    });

    deliver_ghost(&app, cx, "return a + b;");
    app.update_in(cx, |app, _w, _cx| {
        let g = app.ghost.as_ref().expect("ghost proposes the body");
        assert_eq!(g.text, "return a + b;");
        assert_eq!(g.anchor, (3, 4), "ghost sits on the stub's body line");
        assert!(app.completion.is_none(), "no popup reopened by the ghost");
    });
    assert!(cx.debug_bounds("ghost-run").is_some(), "ghost paints on the body line");

    cx.simulate_keystrokes("tab");
    app.update_in(cx, |app, _w, _cx| {
        let tab = app.editor.active_tab().unwrap();
        assert_eq!(tab.line(2), "int add(int a, int b) {");
        assert_eq!(tab.line(3), "    return a + b;");
        assert_eq!(tab.line(4), "}");
        assert!(app.ghost.is_none());
        assert!(app.completion.is_none());
    });
    let _ = std::fs::remove_dir_all(&dir);
}

/// Seam 2: with the stub popup open and a ghost up at the same caret, Tab
/// takes the ghost and closes the popup, so a later Enter cannot apply a stale
/// stub on top of the accepted text.
#[gpui::test]
async fn ghost_tab_closes_the_stub_popup(cx: &mut TestAppContext) {
    let (dir, app, cx) = stub_popup_with_ghost_ready(cx).await;

    deliver_ghost(&app, cx, "d(int a, int b) {");
    app.update_in(cx, |app, _w, _cx| {
        assert!(app.ghost.is_some(), "ghost shows next to the open popup");
        assert!(app.completion.is_some(), "popup stays open beside the ghost");
    });

    cx.simulate_keystrokes("tab");
    app.update_in(cx, |app, _w, _cx| {
        let tab = app.editor.active_tab().unwrap();
        assert_eq!(tab.line(2), "int add(int a, int b) {", "Tab took the ghost");
        assert!(app.completion.is_none(), "popup closed with the ghost accept");
    });

    // A second Enter now only inserts a newline: nothing stale is applied.
    cx.simulate_keystrokes("enter");
    app.update_in(cx, |app, _w, _cx| {
        let tab = app.editor.active_tab().unwrap();
        assert_eq!(tab.line(2), "int add(int a, int b) {");
        assert!(
            !tab.buffer.to_string().contains("int add(int a, int b) {int add"),
            "no stub landed on top of the ghost text"
        );
    });
    let _ = std::fs::remove_dir_all(&dir);
}

/// Seam 3: Esc clears the ghost first and the popup second, and Enter with a
/// ghost up still takes the popup's stub (Enter is the popup's key, Tab the
/// ghost's).
#[gpui::test]
async fn enter_prefers_stub_while_ghost_is_up(cx: &mut TestAppContext) {
    let (dir, app, cx) = stub_popup_with_ghost_ready(cx).await;
    deliver_ghost(&app, cx, "d(int a, int b) { return a + b; }");

    cx.simulate_keystrokes("enter");
    app.update_in(cx, |app, _w, _cx| {
        let tab = app.editor.active_tab().unwrap();
        assert_eq!(tab.line(2), "int add(int a, int b) {");
        assert_eq!(tab.line(3), "    ");
        assert_eq!(tab.line(4), "}");
        assert!(app.completion.is_none());
        assert!(app.ghost.is_none(), "the pre-accept ghost never survives the stub");
    });
    let _ = std::fs::remove_dir_all(&dir);
}

/// Clicking a `.trace` bundle opens the RUNTIME sidebar with a TRACE section
/// that waits for the export; the export result lands only for the newest
/// generation; the × clears it.
#[gpui::test]
async fn trace_bundle_opens_runtime_trace_section(cx: &mut TestAppContext) {
    use crate::app::AppEvent;
    use crate::trace::{TraceAnalysis, TraceKind};
    use std::sync::Arc;

    let (dir, _file) = test_workspace();
    let bundle = dir.join("ob.trace");
    std::fs::create_dir_all(&bundle).unwrap();
    let (deps, app_rx) = test_deps(dir);
    let (app, cx) = cx.add_window_view(|_window, cx| JadeApp::new(cx, deps, app_rx));

    app.update_in(cx, |app, _w, cx| {
        assert!(!app.runtime_visible);
        app.open_file(bundle.clone()); // the tree and Quick Open both land here
        cx.notify();
    });
    cx.run_until_parked();

    app.update_in(cx, |app, _w, cx| {
        assert!(app.runtime_visible, "the sidebar opens so the section is visible");
        let state = app.trace.as_ref().expect("a TRACE section is pending");
        assert_eq!(state.path, bundle);
        assert!(state.result.is_none(), "still exporting");
        assert!(app.editor.active_tab().is_none(), "a bundle never becomes a buffer");

        let stale = app.trace_gen - 1;
        app.apply_app_event(AppEvent::Trace {
            generation: stale,
            result: Err("old".into()),
        });
        assert!(app.trace.as_ref().unwrap().result.is_none(), "stale generations drop");

        let analysis = TraceAnalysis {
            path: bundle.clone(),
            template: Some("CPU Counters".into()),
            instruments: vec![],
            kind: TraceKind::Other,
        };
        app.apply_app_event(AppEvent::Trace {
            generation: app.trace_gen,
            result: Ok(Arc::new(analysis)),
        });
        let got = app.trace.as_ref().unwrap().result.as_ref().unwrap().as_ref().unwrap();
        assert_eq!(got.describe(), "CPU Counters");

        app.clear_trace();
        assert!(app.trace.is_none());
        cx.notify();
    });
    cx.run_until_parked();
}

/// The COUNTERS picker: typing a mnemonic and Enter adds it, × removes it,
/// the set persists in the workspace `ui` blob, and the copy command names
/// the workspace trace path.
#[gpui::test]
async fn counters_picker_adds_removes_and_persists(cx: &mut TestAppContext) {
    use gpui::Keystroke;

    let (dir, _file) = test_workspace();
    let (deps, app_rx) = test_deps(dir.clone());
    let (app, cx) = cx.add_window_view(|_window, cx| JadeApp::new(cx, deps, app_rx));

    app.update_in(cx, |app, _w, cx| {
        assert_eq!(app.counters.events.len(), 8, "the defaults fill the set");
        app.counters_remove("INST_BRANCH");
        app.counters_remove("BRANCH_MISPRED_NONSPEC");
        assert_eq!(app.counters.events.len(), 6);

        app.counters.editing = true;
        for ch in "inst_branch".chars() {
            let mut ks = Keystroke::parse(&ch.to_string()).unwrap();
            ks.key_char = Some(ch.to_string());
            assert!(app.counters_key(&ks));
        }
        assert_eq!(app.counters.query, "inst_branch");
        assert!(app.counters_key(&Keystroke::parse("enter").unwrap()));
        assert!(app.counters.events.iter().any(|e| e == "INST_BRANCH"), "{:?}", app.counters.events);
        assert!(app.counters.query.is_empty());

        let cmd = app.counters_command();
        assert!(cmd.starts_with("xcrun xctrace record --template "));
        assert!(cmd.contains("run.trace"), "no build yet: {cmd}");
        assert!(cmd.ends_with("-- <program>"));
        cx.notify();
    });
    cx.run_until_parked();

    let ui = crate::workspace_state::load(&dir);
    assert_eq!(ui.counter_events.len(), 7);
    assert!(ui.counter_events.contains(&"INST_BRANCH".to_string()));
    let _ = std::fs::remove_dir_all(&dir);
}

/// A CSV tab: ⌘⇧D swaps the text for the chart, the render-time prepare
/// parses once per buffer version and rebuilds the chart only when the
/// configuration changes, and the chips cycle the columns.
#[gpui::test]
async fn csv_tab_toggles_chart_and_cycles_columns(cx: &mut TestAppContext) {
    use crate::panels::csv_view::ChartKind;

    let (dir, _file) = test_workspace();
    let csv = dir.join("latency.csv");
    std::fs::write(
        &csv,
        "name,lower_ns,upper_ns,count\napply warm,40,43,685\napply warm,80,87,2651\napply cold,960,1023,400\n",
    )
    .unwrap();
    let (deps, app_rx) = test_deps(dir.clone());
    let (app, cx) = cx.add_window_view(|_window, cx| JadeApp::new(cx, deps, app_rx));

    app.update_in(cx, |app, _w, cx| {
        app.open_file(csv.clone());
        assert!(app.csv_chart_active(), "a CSV tab opens as its chart");
        app.toggle_md_preview(cx);
        assert!(!app.csv_chart_active(), "⌘⇧D shows the text");
        app.toggle_md_preview(cx);
        assert!(app.csv_chart_active(), "and back to the chart");
        cx.notify();
    });
    cx.run_until_parked();

    app.update_in(cx, |app, _w, cx| {
        let (_, v1, table) = app.csv.table.clone().expect("parsed at render");
        assert_eq!(table.headers.len(), 4);
        let (_, _, cfg, chart) = app.csv.chart.clone().expect("chart built at render");
        assert_eq!((cfg.x, cfg.y, cfg.group), (1, 3, Some(0)));
        assert_eq!(chart.series.len(), 2);

        // Same version, same config: prepare keeps both caches.
        let before = std::sync::Arc::as_ptr(&chart);
        app.csv_prepare();
        assert_eq!(std::sync::Arc::as_ptr(&app.csv.chart.as_ref().unwrap().3), before);
        assert_eq!(app.csv.table.as_ref().unwrap().1, v1);

        // A control change rebuilds the chart from the cached table.
        app.csv_cycle_group();
        app.csv_prepare();
        let (_, _, cfg2, chart2) = app.csv.chart.clone().unwrap();
        assert_eq!(cfg2.group, Some(1));
        assert_ne!(std::sync::Arc::as_ptr(&chart2), before);
        app.csv_toggle_kind();
        app.csv_toggle_log();
        app.csv_prepare();
        let cfg3 = app.csv.chart.as_ref().unwrap().2.clone();
        assert_eq!(cfg3.kind, ChartKind::Line);
        assert!(cfg3.log_x, "40 to 1023 is under two decades, so log starts off and toggles on");

        // Group cycles past the last column back to none.
        app.csv_cycle_group();
        app.csv_cycle_group();
        app.csv_cycle_group();
        app.csv_prepare();
        assert_eq!(app.csv.chart.as_ref().unwrap().2.group, None);

        app.toggle_md_preview(cx);
        assert!(!app.csv_chart_active(), "toggle back to text");
        cx.notify();
    });
    cx.run_until_parked();
    let _ = std::fs::remove_dir_all(&dir);
}
